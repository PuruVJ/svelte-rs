//! Server-side typed transform.
//!
//! Greenfield rewrite: every output is `svelte_js_ast::Program`, no
//! `serde_json::Value` anywhere. Coverage grows fixture-by-fixture from the
//! simplest static templates outward. Shapes that aren't yet handled return
//! `None` from the entry points; `svelte_compiler::compile` surfaces that as
//! a `typed_server_unsupported` diagnostic.

#![forbid(unsafe_code)]

mod script;
mod typed_fast;

pub use typed_fast::try_typed_server;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Second-tier typed entry point. Currently handles:
/// - "instance script (imports + optionally rune-erasable statements) + simple template"
/// - "single <Component bind:this={x}/>"
/// - "<svelte:element this={tag}>"
pub fn try_typed_server_component(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    // Defer entirely-static fragments to typed_fast (it's cheaper).
    if root.instance.is_none() && is_pure_static_fragment(&root.fragment) {
        return None;
    }

    // Process the instance script: rewrite runes, split imports vs rest.
    let mut script_imports: Vec<Statement> = Vec::new();
    let mut script_rest: Vec<Statement> = Vec::new();
    let mut uses_props = false;
    let mut needs_component_wrap = false;
    let mut consts: std::collections::HashMap<String, Expression> =
        std::collections::HashMap::new();
    let mut derived_bindings: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut async_info: Option<script::AsyncInfo> = None;
    if let Some(s) = root.instance.as_ref() {
        let mut content = s.content.clone();
        let info = script::rewrite_program_for_server(&mut content);
        uses_props = info.uses_props;
        needs_component_wrap = info.needs_component_wrap();
        derived_bindings = info.derived_bindings;
        if let Some(name) = &info.single_id_props {
            script::rewrite_props_destructure(&mut content, name);
        }
        consts = script::collect_script_constants(&content, &info.rune_bindings);
        let (imports, rest) = partition_imports(&content.body)?;
        script_imports = imports;
        if let Some(ai) = script::transform_async_script_server(&rest) {
            async_info = Some(ai);
        } else {
            script_rest = rest;
        }
    }

    // Build the function body: rune-rewritten script statements first, then template.
    let mut func_body: Vec<Statement> = if let Some(ai) = &async_info {
        ai.setup_stmts.clone()
    } else {
        script_rest
    };
    // Apply template-only transforms: substitute script constants AND
    // call-wrap every Identifier that refers to a $derived binding.
    let mut fragment = root.fragment.clone();
    // Always run: even with no consts, the fold pass folds pure Math.*
    // calls into number literals.
    substitute_consts_in_fragment(&mut fragment, &consts);
    let derived = &derived_bindings;
    if !derived.is_empty() {
        call_derived_in_fragment(&mut fragment, derived);
    }
    let template_body = if let Some(ai) = &async_info {
        lower_fragment_server_async(&fragment, &ai.async_bindings, ai.last_group_idx)?
    } else {
        lower_fragment_server(&fragment)?
    };
    func_body.extend(template_body);

    // When script triggers component-context: wrap the whole body in
    // `$$renderer.component(($$renderer) => { ... });`.
    if needs_component_wrap {
        let inner = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$renderer")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: func_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        func_body = vec![t::stmt(Expression::Call(Box::new(CallExpression {
            callee: t::member_id(t::id("$$renderer"), "component"),
            arguments: vec![Argument::Expression(inner)],
            optional: false,
            span: Span::ZERO,
        })))];
    }

    // Build the parameter list. Runes-mode uses_props adds $$props.
    let mut params = vec![t::pat_id("$$renderer")];
    if uses_props {
        params.push(t::pat_id("$$props"));
    }

    // Detect async in the template (each-block/if-block test, or top-level
    // await expression tags). If found, emit the `flags/async` import even
    // when the script itself doesn't have top-level await.
    let template_has_async = fragment_has_async(&root.fragment);

    let mut top: Vec<Statement> = Vec::with_capacity(3 + script_imports.len());
    if async_info.is_some() || template_has_async {
        top.push(t::import_side_effect("svelte/internal/flags/async"));
    }
    top.push(t::import_namespace("$", "svelte/internal/server"));
    top.extend(script_imports);
    top.push(t::export_default_function(component_name, params, func_body));
    Some(t::program(top))
}

/// Lower a fragment under top-level-await semantics. Expressions that
/// reference any binding in `async_bindings` get wrapped with
/// `$$renderer.async([$$promises[idx]], ($$renderer) => $$renderer.push(
/// () => $.escape(EXPR)));`. Surrounding text/static-elements split into
/// their own push statements.
fn lower_fragment_server_async(
    f: &svelte_ast::fragment::Fragment,
    async_bindings: &std::collections::HashSet<String>,
    last_group_idx: usize,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut buf = TemplateBuf::new();
    let nodes = trim_boundary_whitespace(&f.nodes);
    let nodes = trim_boundary_text(nodes);

    // If the first non-whitespace top-level node is an async-tainted
    // ExpressionTag, prepend `<!---->` marker.
    let needs_anchor = matches!(
        nodes.first(),
        Some(FragmentChild::ExpressionTag(t)) if expr_refs_any(&t.expression, async_bindings)
    );
    if needs_anchor {
        buf.push_str("<!---->");
    }

    for n in nodes.iter() {
        match n {
            FragmentChild::RegularElement(el) => {
                // Detect: element whose only non-ws child is an
                // async-tainted ExpressionTag.
                let body_non_ws: Vec<&FragmentChild> = el
                    .fragment
                    .nodes
                    .iter()
                    .filter(|c| match c {
                        FragmentChild::Text(t) => !t.data.trim().is_empty(),
                        _ => true,
                    })
                    .collect();
                let split_async = body_non_ws.len() == 1
                    && matches!(
                        body_non_ws[0],
                        FragmentChild::ExpressionTag(t) if expr_refs_any(&t.expression, async_bindings)
                    );
                if split_async {
                    // Open tag → buf
                    buf.push_str("<");
                    buf.push_str(&el.name);
                    for attr in &el.attributes {
                        append_element_attribute_server(attr, &mut buf)?;
                    }
                    buf.push_str(">");
                    if let Some(stmt) = buf.flush() {
                        out.push(stmt);
                    }
                    // Async-wrap the expression
                    let et = match body_non_ws[0] {
                        FragmentChild::ExpressionTag(t) => t,
                        _ => unreachable!(),
                    };
                    out.push(emit_async_wrap(&et.expression, last_group_idx));
                    // Close tag → buf
                    buf.push_str("</");
                    buf.push_str(&el.name);
                    buf.push_str(">");
                    continue;
                }
                // Fallback: treat as a static-only element.
                if append_node_to_template(n, &mut buf).is_none() {
                    return None;
                }
            }
            FragmentChild::ExpressionTag(t) => {
                if expr_refs_any(&t.expression, async_bindings) {
                    // Flush any pending buffer, then emit async wrap.
                    if let Some(stmt) = buf.flush() {
                        out.push(stmt);
                    }
                    out.push(emit_async_wrap(&t.expression, last_group_idx));
                } else if append_node_to_template(n, &mut buf).is_none() {
                    return None;
                }
            }
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {
                if append_node_to_template(n, &mut buf).is_none() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }
    Some(out)
}

fn fragment_has_async(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(node_has_async)
}

fn node_has_async(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::ExpressionTag(t) => expr_has_await_top(&t.expression),
        FragmentChild::HtmlTag(t) => expr_has_await_top(&t.expression),
        FragmentChild::RegularElement(el) => {
            el.attributes.iter().any(attr_has_async) || fragment_has_async(&el.fragment)
        }
        FragmentChild::Component(c) => {
            c.attributes.iter().any(attr_has_async) || fragment_has_async(&c.fragment)
        }
        FragmentChild::SvelteElement(el) => {
            expr_has_await_top(&el.tag)
                || el.attributes.iter().any(attr_has_async)
                || fragment_has_async(&el.fragment)
        }
        FragmentChild::EachBlock(eb) => {
            expr_has_await_top(&eb.expression)
                || fragment_has_async(&eb.body)
                || eb.fallback.as_ref().map_or(false, fragment_has_async)
        }
        FragmentChild::IfBlock(ib) => {
            expr_has_await_top(&ib.test)
                || fragment_has_async(&ib.consequent)
                || ib.alternate.as_ref().map_or(false, fragment_has_async)
        }
        FragmentChild::AwaitBlock(ab) => {
            expr_has_await_top(&ab.expression)
                || ab.pending.as_ref().map_or(false, fragment_has_async)
                || ab.then.as_ref().map_or(false, fragment_has_async)
                || ab.catch_.as_ref().map_or(false, fragment_has_async)
        }
        FragmentChild::KeyBlock(kb) => {
            expr_has_await_top(&kb.expression) || fragment_has_async(&kb.fragment)
        }
        _ => false,
    }
}

fn attr_has_async(a: &svelte_ast::attributes::ElementAttribute) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    match a {
        ElementAttribute::Attribute(a) => match &a.value {
            AttributeValue::Single(tag) => expr_has_await_top(&tag.expression),
            AttributeValue::Many(parts) => parts.iter().any(|p| {
                matches!(p, AttributeValuePart::ExpressionTag(t) if expr_has_await_top(&t.expression))
            }),
            _ => false,
        },
        ElementAttribute::SpreadAttribute(s) => expr_has_await_top(&s.expression),
        ElementAttribute::BindDirective(b) => expr_has_await_top(&b.expression),
        _ => false,
    }
}

fn expr_has_await_top(e: &Expression) -> bool {
    match e {
        Expression::Await(_) => true,
        Expression::Function(f) if f.r#async => false,
        Expression::Arrow(a) if a.r#async => false,
        Expression::Call(c) => {
            expr_has_await_top(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_has_await_top(e),
                    Argument::Spread(s) => expr_has_await_top(&s.argument),
                })
        }
        Expression::Binary(b) => expr_has_await_top(&b.left) || expr_has_await_top(&b.right),
        Expression::Logical(l) => expr_has_await_top(&l.left) || expr_has_await_top(&l.right),
        Expression::Unary(u) => expr_has_await_top(&u.argument),
        Expression::Member(m) => expr_has_await_top(&m.object),
        Expression::Conditional(c) => {
            expr_has_await_top(&c.test)
                || expr_has_await_top(&c.consequent)
                || expr_has_await_top(&c.alternate)
        }
        Expression::Paren(p) => expr_has_await_top(&p.expression),
        _ => false,
    }
}

fn expr_refs_any(e: &Expression, names: &std::collections::HashSet<String>) -> bool {
    match e {
        Expression::Identifier(i) => names.contains(&i.name),
        Expression::Member(m) => expr_refs_any(&m.object, names),
        Expression::Call(c) => {
            expr_refs_any(&c.callee, names)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_refs_any(e, names),
                    Argument::Spread(s) => expr_refs_any(&s.argument, names),
                })
        }
        Expression::Binary(b) => expr_refs_any(&b.left, names) || expr_refs_any(&b.right, names),
        Expression::Logical(l) => expr_refs_any(&l.left, names) || expr_refs_any(&l.right, names),
        Expression::Unary(u) => expr_refs_any(&u.argument, names),
        Expression::Conditional(c) => {
            expr_refs_any(&c.test, names)
                || expr_refs_any(&c.consequent, names)
                || expr_refs_any(&c.alternate, names)
        }
        Expression::Paren(p) => expr_refs_any(&p.expression, names),
        Expression::Template(t) => t.expressions.iter().any(|e| expr_refs_any(e, names)),
        _ => false,
    }
}

/// `$$renderer.async([$$promises[idx]], ($$renderer) => $$renderer.push(
/// () => $.escape(EXPR)));`
fn emit_async_wrap(expr: &Expression, group_idx: usize) -> Statement {
    let promises_slot = Expression::Member(Box::new(MemberExpression {
        object: t::id("$$promises"),
        property: MemberProperty::Expression(t::lit_number(group_idx as f64)),
        computed: true,
        optional: false,
        span: Span::ZERO,
    }));
    let blockers = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(promises_slot)],
        span: Span::ZERO,
    }));
    let escape_call = t::call(t::member_id(t::id("$"), "escape"), vec![expr.clone()]);
    let push_thunk = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(escape_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let inner_push = t::call(
        t::member_id(t::id("$$renderer"), "push"),
        vec![push_thunk],
    );
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Expression(inner_push),
        r#async: false,
        span: Span::ZERO,
    }));
    t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "async"),
        vec![blockers, arrow],
    ))
}

/// Lower an entire root-level fragment to a sequence of server statements.
fn lower_fragment_server(f: &svelte_ast::fragment::Fragment) -> Option<Vec<Statement>> {
    lower_fragment_with_marker(f, false)
}

/// Lower a fragment with an optional leading `<!---->` marker (prepended
/// to the first push if the first non-whitespace child needs it).
fn lower_fragment_with_marker(
    f: &svelte_ast::fragment::Fragment,
    needs_marker: bool,
) -> Option<Vec<Statement>> {
    let mut out = Vec::new();
    let mut buf = TemplateBuf::new();
    if needs_marker {
        buf.push_str("<!---->");
    }
    let nodes = trim_boundary_whitespace(&f.nodes);
    let nodes = trim_boundary_text(nodes);
    let mut emitted_static_push = false;
    let mut last_was_component = false;
    for n in nodes.iter() {
        // RegularElement with <option> children: write open tag + interleave
        // option calls + close tag inline (keeps the existing buf flowing).
        if let FragmentChild::RegularElement(el) = n {
            if has_option_child(el) {
                emit_select_inline(el, &mut buf, &mut out)?;
                last_was_component = false;
                continue;
            }
        }
        if append_node_to_template(n, &mut buf).is_none() {
            if let Some(stmt) = buf.flush() {
                emitted_static_push = true;
                out.push(stmt);
            }
            match n {
                FragmentChild::Component(c) => {
                    out.push(lower_component_server(c)?);
                    last_was_component = true;
                }
                FragmentChild::SvelteElement(el) => {
                    out.push(lower_svelte_element_server(el)?);
                    last_was_component = false;
                }
                FragmentChild::EachBlock(eb) => {
                    out.extend(lower_each_block_server(eb)?);
                    last_was_component = false;
                }
                FragmentChild::AwaitBlock(ab) => {
                    out.extend(lower_await_block_server(ab)?);
                    // BLOCK_CLOSE `<!--]-->` fuses with the next text push.
                    buf.push_str("<!--]-->");
                    last_was_component = false;
                }
                FragmentChild::IfBlock(ib) => {
                    out.extend(lower_if_block_server(ib)?);
                    // BLOCK_CLOSE `<!--]-->` fuses with the next text push.
                    buf.push_str("<!--]-->");
                    last_was_component = false;
                }
                _ => return None,
            }
        } else {
            last_was_component = false;
        }
    }
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    } else if last_was_component && emitted_static_push {
        // Mid-fragment Component followed by no more static content needs
        // a closing `<!---->` marker to anchor the end of the hydration scope.
        out.push(push_template("<!---->"));
    }
    Some(out)
}

/// Inline emit `<select>` with `<option>` children — writes the open tag
/// into `buf`, interleaves option calls into `out` (flushing buf each
/// time), and finishes by writing the close tag into `buf`.
fn emit_select_inline(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
) -> Option<()> {
    buf.push_str("<");
    buf.push_str(&el.name);
    for attr in &el.attributes {
        append_element_attribute_server(attr, buf)?;
    }
    buf.push_str(">");
    let children = trim_boundary_whitespace(&el.fragment.nodes);
    for c in children {
        match c {
            FragmentChild::RegularElement(child) if child.name == "option" => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.push(lower_option_server(child)?);
            }
            other => {
                if append_node_to_template(other, buf).is_none() {
                    return None;
                }
            }
        }
    }
    buf.push_str("</");
    buf.push_str(&el.name);
    buf.push_str(">");
    Some(())
}

/// Returns true unless the first non-trivial child of the fragment is a
/// RegularElement (which provides its own anchor). Used to decide whether
/// to prepend `<!---->` to a body push.
fn body_needs_marker(f: &svelte_ast::fragment::Fragment) -> bool {
    let first = f
        .nodes
        .iter()
        .find(|n| !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()));
    match first {
        None => false,
        Some(FragmentChild::RegularElement(_)) => false,
        // Comments are dropped — look past them when deciding.
        Some(FragmentChild::Comment(_)) => true,
        _ => true,
    }
}

/// `{#each EXPR as PATTERN[, INDEX]}body{/each}` →
///
/// ```text
/// $$renderer.push(`<!--[-->`);
/// const each_array = $.ensure_array_like(EXPR);
/// for (let INDEX = 0, $$length = each_array.length; INDEX < $$length; INDEX++) {
///     let PATTERN = each_array[INDEX];
///     ...body...
/// }
/// $$renderer.push(`<!--]-->`);
/// ```
fn lower_each_block_server(eb: &svelte_ast::blocks::EachBlock) -> Option<Vec<Statement>> {
    // Index name — explicit when given, `$$index` otherwise. No-context form
    // (no `as`) still uses the explicit index if present.
    let index_name = eb.index.clone().unwrap_or_else(|| "$$index".to_string());

    // for-loop init: `let INDEX = 0, $$length = each_array.length`
    let init = Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Let,
        declarations: vec![
            VariableDeclarator {
                id: t::pat_id(&index_name),
                init: Some(t::lit_number(0.0)),
                span: Span::ZERO,
            },
            VariableDeclarator {
                id: t::pat_id("$$length"),
                init: Some(Expression::Member(Box::new(MemberExpression {
                    object: t::id("each_array"),
                    property: MemberProperty::Identifier(Identifier {
                        name: "length".to_string(),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))),
                span: Span::ZERO,
            },
        ],
        span: Span::ZERO,
    }));

    let test = Expression::Binary(Box::new(BinaryExpression {
        left: t::id(&index_name),
        operator: BinaryOperator::Lt,
        right: t::id("$$length"),
        span: Span::ZERO,
    }));
    let update = Expression::Update(Box::new(UpdateExpression {
        operator: UpdateOperator::Increment,
        argument: t::id(&index_name),
        prefix: false,
        span: Span::ZERO,
    }));

    // Inside the loop: `let PATTERN = each_array[INDEX];` then body.
    let mut body_stmts: Vec<Statement> = Vec::new();
    if let Some(ctx) = &eb.context {
        body_stmts.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: ctx.clone(),
                init: Some(Expression::Member(Box::new(MemberExpression {
                    object: t::id("each_array"),
                    property: MemberProperty::Expression(t::id(&index_name)),
                    computed: true,
                    optional: false,
                    span: Span::ZERO,
                }))),
                span: Span::ZERO,
            }],
            span: Span::ZERO,
        })));
    }

    let expr_is_async = expr_has_await_top(&eb.expression);
    if expr_is_async {
        // Body uses async-block lowering: ExpressionTags with await become
        // separate `$$renderer.push(async () => ...)` statements. Prepend
        // a `<!---->` per-iteration anchor when the body's first child
        // would normally need one.
        if body_needs_marker(&eb.body) {
            body_stmts.push(push_template("<!---->"));
        }
        body_stmts.extend(lower_fragment_for_async_block(&eb.body)?);
    } else {
        let needs_marker = body_needs_marker(&eb.body);
        body_stmts.extend(lower_fragment_with_marker(&eb.body, needs_marker)?);
    }

    let each_array_init = if expr_is_async {
        wrap_async_test(&eb.expression)
    } else {
        eb.expression.clone()
    };

    let each_array_decl = Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Const,
        declarations: vec![VariableDeclarator {
            id: t::pat_id("each_array"),
            init: Some(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "ensure_array_like"),
                arguments: vec![Argument::Expression(each_array_init)],
                optional: false,
                span: Span::ZERO,
            }))),
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    }));

    let for_stmt = Statement::For(Box::new(ForStatement {
        init: Some(ForInit::Declaration(Box::new(match init {
            Statement::Variable(v) => *v,
            _ => unreachable!(),
        }))),
        test: Some(test),
        update: Some(update),
        body: Statement::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        span: Span::ZERO,
    }));

    if expr_is_async {
        // Build the inside-child_block body.
        let mut inside_body: Vec<Statement> = vec![each_array_decl];
        if let Some(fallback) = &eb.fallback {
            // With a fallback: wrap the for-loop in
            // `if (each_array.length !== 0) { ... } else { ... fallback ... }`
            let length_member = Expression::Member(Box::new(MemberExpression {
                object: t::id("each_array"),
                property: MemberProperty::Identifier(Identifier {
                    name: "length".to_string(),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }));
            let test_neq_zero = Expression::Binary(Box::new(BinaryExpression {
                left: length_member,
                operator: BinaryOperator::StrictNotEq,
                right: t::lit_number(0.0),
                span: Span::ZERO,
            }));
            let then_branch: Vec<Statement> = vec![push_string("<!--[-->"), for_stmt];
            // Else branch: `<!--[!-->` marker + fallback body
            let mut else_branch: Vec<Statement> = vec![push_string("<!--[!-->")];
            if body_needs_marker(fallback) {
                else_branch.push(push_template("<!---->"));
            }
            else_branch.extend(lower_fragment_for_async_block(fallback)?);
            inside_body.push(Statement::If(Box::new(IfStatement {
                test: test_neq_zero,
                consequent: Statement::Block(Box::new(BlockStatement {
                    body: then_branch,
                    span: Span::ZERO,
                })),
                alternate: Some(Statement::Block(Box::new(BlockStatement {
                    body: else_branch,
                    span: Span::ZERO,
                }))),
                span: Span::ZERO,
            })));
        } else {
            inside_body.push(for_stmt);
        }
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$renderer")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: inside_body,
                span: Span::ZERO,
            })),
            r#async: true,
            span: Span::ZERO,
        }));
        // When there's no fallback, the outer `<!--[-->` push stays outside
        // (matches non-fallback expected). With a fallback, the marker is
        // emitted from inside the conditional.
        let mut out: Vec<Statement> = Vec::new();
        if eb.fallback.is_none() {
            out.push(push_template("<!--[-->"));
        }
        out.push(t::stmt(t::call(
            t::member_id(t::id("$$renderer"), "child_block"),
            vec![arrow],
        )));
        out.push(push_template("<!--]-->"));
        Some(out)
    } else {
        Some(vec![
            push_template("<!--[-->"),
            each_array_decl,
            for_stmt,
            push_template("<!--]-->"),
        ])
    }
}

/// `{#if TEST}consequent{:else if X}...{:else}alternate{/if}` →
///
/// ```text
/// if (TEST) {
///     $$renderer.push('<!--[0-->');
///     ...consequent body...
/// } else {
///     $$renderer.push('<!--[-1-->');
///     ...alternate body...
/// }
/// ```
///
/// Caller appends a trailing `<!--]-->` marker that fuses with following content.
/// Ports `packages/svelte/src/compiler/phases/3-transform/server/visitors/IfBlock.js`.
fn lower_if_block_server(
    ib: &svelte_ast::blocks::IfBlock,
) -> Option<Vec<Statement>> {
    let test_is_async = expr_has_await_top(&ib.test);

    let mut consequent_body: Vec<Statement> = Vec::new();
    consequent_body.push(if test_is_async {
        push_string("<!--[0-->")
    } else {
        push_template("<!--[0-->")
    });
    if test_is_async {
        consequent_body.extend(lower_fragment_for_async_block(&ib.consequent)?);
    } else {
        consequent_body.extend(lower_fragment_with_marker(
            &ib.consequent,
            body_needs_marker(&ib.consequent),
        )?);
    }

    let mut alternate_body: Vec<Statement> = Vec::new();
    alternate_body.push(if test_is_async {
        push_string("<!--[-1-->")
    } else {
        push_template("<!--[-1-->")
    });
    if let Some(alt) = &ib.alternate {
        if test_is_async {
            alternate_body.extend(lower_fragment_for_async_block(alt)?);
        } else {
            alternate_body.extend(lower_fragment_with_marker(alt, body_needs_marker(alt))?);
        }
    }

    let test = if test_is_async {
        wrap_async_test(&ib.test)
    } else {
        ib.test.clone()
    };

    let if_stmt = Statement::If(Box::new(IfStatement {
        test,
        consequent: Statement::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        alternate: Some(Statement::Block(Box::new(BlockStatement {
            body: alternate_body,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }));

    if test_is_async {
        // Wrap in `$$renderer.child_block(async ($$renderer) => { if(...) {...} else {...} })`
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$renderer")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: vec![if_stmt],
                span: Span::ZERO,
            })),
            r#async: true,
            span: Span::ZERO,
        }));
        Some(vec![t::stmt(t::call(
            t::member_id(t::id("$$renderer"), "child_block"),
            vec![arrow],
        ))])
    } else {
        Some(vec![if_stmt])
    }
}

/// `EXPR` → `(await $.save(EXPR))()`. Used when an async block's test
/// needs blocker tracking.
fn wrap_async_test(test: &Expression) -> Expression {
    // Strip the outer await if present so we wrap the underlying value.
    let inner = match test {
        Expression::Await(a) => a.argument.clone(),
        e => e.clone(),
    };
    let save_call = t::call(t::member_id(t::id("$"), "save"), vec![inner]);
    let awaited = Expression::Paren(Box::new(ParenthesizedExpression {
        expression: Expression::Await(Box::new(AwaitExpression {
            argument: save_call,
            span: Span::ZERO,
        })),
        span: Span::ZERO,
    }));
    t::call(awaited, Vec::new())
}

/// Lower a fragment inside an async block body. ExpressionTags with await
/// in their expression become separate `$$renderer.push(async () =>
/// $.escape(await EXPR))` statements; other content uses normal lowering.
fn lower_fragment_for_async_block(
    f: &svelte_ast::fragment::Fragment,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut buf = TemplateBuf::new();
    let nodes = trim_boundary_whitespace(&f.nodes);
    let nodes = trim_boundary_text(nodes);
    for n in nodes.iter() {
        if let FragmentChild::ExpressionTag(t) = n {
            if expr_has_await_top(&t.expression) {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                // `$$renderer.push(async () => $.escape(await EXPR))`
                let escape_call = t::call(
                    t::member_id(t::id("$"), "escape"),
                    vec![t.expression.clone()],
                );
                let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(escape_call),
                    r#async: true,
                    span: Span::ZERO,
                }));
                out.push(t::stmt(t::call(
                    t::member_id(t::id("$$renderer"), "push"),
                    vec![arrow],
                )));
                continue;
            }
        }
        if append_node_to_template(n, &mut buf).is_none() {
            return None;
        }
    }
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }
    Some(out)
}

/// `{#await EXPR [as PAT][:then PAT][:catch PAT]}...{/await}` →
/// `$.await($$renderer, EXPR, pending_arrow, then_arrow, catch_arrow?);`
/// Returns the call statement; caller appends a trailing `<!--]-->` marker.
fn lower_await_block_server(
    ab: &svelte_ast::blocks::AwaitBlock,
) -> Option<Vec<Statement>> {
    // pending arrow: `() => { ...pending body... }`
    let pending = build_block_arrow(None, ab.pending.as_ref())?;
    // then arrow: `(value) => { ...then body... }`
    let then = build_block_arrow(ab.value.as_ref(), ab.then.as_ref())?;

    let mut args = vec![
        Argument::Expression(t::id("$$renderer")),
        Argument::Expression(ab.expression.clone()),
        Argument::Expression(pending),
        Argument::Expression(then),
    ];
    if ab.error.is_some() || ab.catch_.is_some() {
        let catch_arrow = build_block_arrow(ab.error.as_ref(), ab.catch_.as_ref())?;
        args.push(Argument::Expression(catch_arrow));
    }

    Some(vec![t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "await"),
        arguments: args,
        optional: false,
        span: Span::ZERO,
    })))])
}

fn build_block_arrow(
    param: Option<&svelte_js_ast::Pattern>,
    body: Option<&svelte_ast::fragment::Fragment>,
) -> Option<Expression> {
    let params = match param {
        Some(p) => vec![p.clone()],
        None => Vec::new(),
    };
    let body_stmts = match body {
        Some(f) => lower_fragment_with_marker(f, body_needs_marker(f))?,
        None => Vec::new(),
    };
    Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
        params,
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    })))
}

/// True if the element has any `<option>` direct child. `<select>` /
/// `<datalist>` etc. with rich-content options need special interleaved
/// `$$renderer.option(...)` lowering.
fn has_option_child(el: &svelte_ast::elements::RegularElement) -> bool {
    el.fragment.nodes.iter().any(|n| {
        matches!(n, FragmentChild::RegularElement(child) if child.name == "option")
    })
}

/// `<select>` with `<option>` children: emit the open tag, each option as a
/// `$$renderer.option(props, body_fn)` call, then the close tag.
fn lower_select_element_server(
    el: &svelte_ast::elements::RegularElement,
) -> Option<Vec<Statement>> {
    let mut out = Vec::new();
    // Open tag — serialize into a small TemplateBuf, flush.
    let mut buf = TemplateBuf::new();
    buf.push_str("<");
    buf.push_str(&el.name);
    for attr in &el.attributes {
        append_element_attribute_server(attr, &mut buf)?;
    }
    buf.push_str(">");
    let children = trim_boundary_whitespace(&el.fragment.nodes);
    for c in children {
        match c {
            FragmentChild::RegularElement(child) if child.name == "option" => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.push(lower_option_server(child)?);
            }
            other => {
                if append_node_to_template(other, &mut buf).is_none() {
                    return None;
                }
            }
        }
    }
    buf.push_str("</");
    buf.push_str(&el.name);
    buf.push_str(">");
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }
    Some(out)
}

/// `<option value="X">content</option>` → `$$renderer.option({ value: 'X' }, ($$renderer) => { ...body... });`.
fn lower_option_server(el: &svelte_ast::elements::RegularElement) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &el.attributes {
        if let ElementAttribute::Attribute(a) = attr {
            if is_event_handler_name(&a.name) {
                continue;
            }
            props.push(attribute_to_object_member(a)?);
        }
    }
    // <option> body doesn't need a hydration anchor — renderer.option handles positioning.
    let body_stmts = lower_fragment_with_marker(&el.fragment, false)?;
    let body_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$$renderer"), "option"),
        arguments: vec![
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
            Argument::Expression(body_arrow),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
}

/// `$$renderer.push(\`STR\`);`
fn push_template(s: &str) -> Statement {
    t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$$renderer"), "push"),
        arguments: vec![Argument::Expression(t::template_raw(
            vec![s.to_string()],
            Vec::new(),
        ))],
        optional: false,
        span: Span::ZERO,
    })))
}

/// `$$renderer.push('STR');` — single-quoted string literal form used inside
/// async child_block bodies for the `<!--[0-->`/`<!--[-1-->` markers.
fn push_string(s: &str) -> Statement {
    t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$$renderer"), "push"),
        arguments: vec![Argument::Expression(Expression::Literal(Box::new(
            Literal::String(StringLiteral {
                value: s.to_string(),
                raw: Some(format!("'{}'", s.replace('\'', "\\'"))),
                span: Span::ZERO,
            }),
        )))],
        optional: false,
        span: Span::ZERO,
    })))
}

/// Append a fragment child to the template literal buffer. Returns `None`
/// if the node can't be expressed inline (Component, block, etc.).
fn append_node_to_template(n: &FragmentChild, buf: &mut TemplateBuf) -> Option<()> {
    match n {
        FragmentChild::Text(t) => {
            buf.push_str(&escape_text(&collapse_ws(&t.data)));
            Some(())
        }
        FragmentChild::ExpressionTag(tag) => {
            // Constant-fold literal expressions (no `$.escape` wrap, just inline).
            if let Some(s) = literal_expr_to_string(&tag.expression) {
                buf.push_str(&escape_text(&s));
            } else {
                buf.push_expr(Expression::Call(Box::new(CallExpression {
                    callee: t::member_id(t::id("$"), "escape"),
                    arguments: vec![Argument::Expression(tag.expression.clone())],
                    optional: false,
                    span: Span::ZERO,
                })));
            }
            Some(())
        }
        FragmentChild::HtmlTag(tag) => {
            // `{@html EXPR}` → `${$.html(EXPR)}` (no escaping).
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "html"),
                arguments: vec![Argument::Expression(tag.expression.clone())],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
        FragmentChild::RegularElement(el) if has_option_child(el) => {
            // `<select>` with `<option>` children uses interleaved
            // `$$renderer.option(...)` calls — signal "not inline" so the
            // caller can lower it as a separate statement.
            None
        }
        FragmentChild::RegularElement(el) => {
            buf.push_str("<");
            buf.push_str(&el.name);
            for attr in &el.attributes {
                append_element_attribute_server(attr, buf)?;
            }
            if is_void(&el.name) {
                // Self-closing void element form: `<br/>`.
                buf.push_str("/>");
                Some(())
            } else {
                buf.push_str(">");
                let children = trim_boundary_whitespace(&el.fragment.nodes);
                let children = trim_boundary_text(children);
                for c in children.iter() {
                    append_node_to_template(c, buf)?;
                }
                buf.push_str("</");
                buf.push_str(&el.name);
                buf.push_str(">");
                Some(())
            }
        }
        FragmentChild::Comment(_) => Some(()), // HTML comments dropped server-side
        _ => None,
    }
}

/// After `trim_boundary_whitespace`, trim leading whitespace inside the
/// FIRST surviving Text node and trailing whitespace inside the LAST.
/// Returns owned Vec since the first/last text may need to be cloned.
fn trim_boundary_text(nodes: &[FragmentChild]) -> Vec<FragmentChild> {
    let mut out: Vec<FragmentChild> = nodes.to_vec();
    if let Some(FragmentChild::Text(t)) = out.first_mut() {
        let trimmed = t.data.trim_start().to_string();
        if trimmed != t.data {
            t.data = trimmed.clone();
            t.raw = trimmed;
        }
    }
    if let Some(FragmentChild::Text(t)) = out.last_mut() {
        let trimmed = t.data.trim_end().to_string();
        if trimmed != t.data {
            t.data = trimmed.clone();
            t.raw = trimmed;
        }
    }
    out
}

/// Skip leading + trailing whitespace-only Text nodes (and Comments,
/// which are dropped server-side anyway) from a slice of fragment children.
fn trim_boundary_whitespace(nodes: &[FragmentChild]) -> &[FragmentChild] {
    let is_boundary_skip = |n: &FragmentChild| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        FragmentChild::Comment(_) => true,
        _ => false,
    };
    let mut start = 0;
    let mut end = nodes.len();
    while start < end && is_boundary_skip(&nodes[start]) {
        start += 1;
    }
    while end > start && is_boundary_skip(&nodes[end - 1]) {
        end -= 1;
    }
    &nodes[start..end]
}

/// Collapse all consecutive whitespace runs in `s` to a single space.
/// Matches upstream's `regex_starts_with_whitespaces` / collapse-whitespace
/// behavior at preserveWhitespace=false.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Server-side: serialize one element attribute or directive into the
/// template buffer. Static text → ` name="value"`. Dynamic single-expression
/// → `${$.attr('name', expr)}` interpolation. Event-handlers and most
/// directives are dropped. `bind:X={expr}` becomes an `$.attr('X', expr)`
/// interpolation (server emits the current value as an attribute).
fn append_element_attribute_server(
    attr: &ElementAttribute,
    buf: &mut TemplateBuf,
) -> Option<()> {
    match attr {
        ElementAttribute::Attribute(a) => {
            // Drop event-handler attributes (`onclick`, `onfoo`, ...).
            if is_event_handler_name(&a.name) {
                return Some(());
            }
            append_value_attribute(&a.name, &a.value, buf)
        }
        ElementAttribute::BindDirective(b) => {
            // Server treats `bind:X={expr}` as `X={expr}` for attribute output.
            // `bind:this` is dropped (refs are runtime-only).
            if b.name == "this" {
                return Some(());
            }
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "attr"),
                arguments: vec![
                    Argument::Expression(string_lit(&b.name)),
                    Argument::Expression(b.expression.clone()),
                ],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
        // Other directives (on:/use:/transition:/animate:/let:/class:/style:/attach) drop server-side.
        _ => Some(()),
    }
}

/// Emit one `name=value`-flavored attribute. Static → literal in template;
/// dynamic → `${$.attr('name', expr)}` interpolation.
fn append_value_attribute(
    name: &str,
    value: &AttributeValue,
    buf: &mut TemplateBuf,
) -> Option<()> {
    match value {
        AttributeValue::Empty => {
            // HTML5 emits `name=""` for bare attribute presence (matches upstream).
            buf.push_str(" ");
            buf.push_str(name);
            buf.push_str("=\"\"");
            Some(())
        }
        AttributeValue::Single(tag) => {
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "attr"),
                arguments: vec![
                    Argument::Expression(string_lit(name)),
                    Argument::Expression(tag.expression.clone()),
                ],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
        AttributeValue::Many(parts) => {
            if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                // Static text value.
                buf.push_str(" ");
                buf.push_str(name);
                buf.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        buf.push_str(&escape_attribute_text(&t.data));
                    }
                }
                buf.push_str("\"");
                Some(())
            } else {
                // TODO: concat parts via template literal + $.attr.
                None
            }
        }
    }
}

fn string_lit(s: &str) -> Expression {
    Expression::Literal(Box::new(Literal::String(StringLiteral {
        value: s.to_string(),
        raw: Some(format!("'{s}'")),
        span: Span::ZERO,
    })))
}

/// Returns true for DOM event-handler attribute names like `onclick`,
/// `onmouseenter`. Lowercase-only; `on-foo` (SVG style) is preserved.
fn is_event_handler_name(name: &str) -> bool {
    if !name.starts_with("on") || name.len() < 3 {
        return false;
    }
    let third = name.as_bytes()[2];
    third.is_ascii_lowercase()
}

fn escape_attribute_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

/// Constant-fold an ExpressionTag's inner expression to a plain string if it's
/// a primitive Literal. Returns None for non-literals.
fn literal_expr_to_string(e: &Expression) -> Option<String> {
    match e {
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::String(s) => Some(s.value.clone()),
            Literal::Number(n) => Some(format_number(n.value)),
            Literal::Boolean(b) => Some(b.value.to_string()),
            // `{null}` and `{undefined}` render as empty string in templates.
            Literal::Null(_) => Some(String::new()),
            _ => None,
        },
        // `{undefined}` is parsed as Identifier { name: "undefined" } not Null.
        Expression::Identifier(i) if i.name == "undefined" => Some(String::new()),
        _ => None,
    }
}

fn format_number(n: f64) -> String {
    if n == n.trunc() && n.is_finite() && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

/// Buffer for accumulating `$$renderer.push(\`...\`)` template content.
struct TemplateBuf {
    /// Static string chunks separated by expression placeholders. Always
    /// has `exprs.len() + 1` entries when non-empty.
    parts: Vec<String>,
    exprs: Vec<Expression>,
}

impl TemplateBuf {
    fn new() -> Self {
        Self { parts: vec![String::new()], exprs: Vec::new() }
    }

    fn push_str(&mut self, s: &str) {
        self.parts.last_mut().unwrap().push_str(s);
    }

    fn push_expr(&mut self, e: Expression) {
        self.exprs.push(e);
        self.parts.push(String::new());
    }

    fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.parts.iter().all(|s| s.is_empty())
    }

    /// True if the buffer carries nothing but whitespace static text (no
    /// expressions). Used to skip emitting a `push(\`   \`)` for a fragment
    /// whose only content was whitespace between non-template-fits nodes.
    fn is_whitespace_only(&self) -> bool {
        self.exprs.is_empty() && self.parts.iter().all(|s| s.chars().all(|c| c.is_whitespace()))
    }

    /// Flush to a `$$renderer.push(\`...\`)` statement (or None when empty
    /// or whitespace-only). Boundary whitespace is handled at the fragment
    /// level by `trim_boundary_whitespace`, not here.
    fn flush(&mut self) -> Option<Statement> {
        if self.is_empty() || self.is_whitespace_only() {
            self.parts.clear();
            self.parts.push(String::new());
            self.exprs.clear();
            return None;
        }
        let parts = std::mem::take(&mut self.parts);
        let exprs = std::mem::take(&mut self.exprs);
        self.parts.push(String::new());
        Some(t::stmt(Expression::Call(Box::new(CallExpression {
            callee: t::member_id(t::id("$$renderer"), "push"),
            arguments: vec![Argument::Expression(t::template_raw(parts, exprs))],
            optional: false,
            span: Span::ZERO,
        }))))
    }
}

/// `<svelte:element this={tag}>` → `$.element($$renderer, tag);` (server form,
/// drops attribute content for now).
fn lower_svelte_element_server(
    el: &svelte_ast::elements::SvelteElement,
) -> Option<Statement> {
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "element"),
        arguments: vec![
            Argument::Expression(t::id("$$renderer")),
            Argument::Expression(el.tag.clone()),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
}

/// `<Foo a={x} b="y" {...rest}>BODY</Foo>` → `Foo($$renderer, { a: x, b: 'y', ...rest, children: ..., $$slots: { default: true } });`.
/// Directives (`bind:this`, `on:click`, etc.) are dropped server-side.
fn lower_component_server(c: &svelte_ast::elements::Component) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => {
                if let Some(prop) = attribute_to_object_member(a) {
                    props.push(prop);
                } else {
                    return None;
                }
            }
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            _ => {}
        }
    }

    // If the component has body content, lower it into a `children:
    // ($$renderer) => { ... }` slot + add `$$slots: { default: true }`.
    let has_body = !c.fragment.nodes.iter().all(|n| {
        matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty())
    });
    if has_body {
        let body_stmts =
            lower_fragment_with_marker(&c.fragment, body_needs_marker(&c.fragment))?;
        let children_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$renderer")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: body_stmts,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: "children".to_string(),
                span: Span::ZERO,
            }),
            value: children_arrow,
            kind: PropertyKind::Init,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
        // $$slots: { default: true }
        props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: "$$slots".to_string(),
                span: Span::ZERO,
            }),
            value: Expression::Object(Box::new(ObjectExpression {
                properties: vec![ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: "default".to_string(),
                        span: Span::ZERO,
                    }),
                    value: Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                        value: true,
                        span: Span::ZERO,
                    }))),
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                }))],
                span: Span::ZERO,
            })),
            kind: PropertyKind::Init,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
    }

    let args = vec![
        Argument::Expression(t::id("$$renderer")),
        Argument::Expression(Expression::Object(Box::new(ObjectExpression {
            properties: props,
            span: Span::ZERO,
        }))),
    ];
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: args,
        optional: false,
        span: Span::ZERO,
    }))))
}

/// `name={expr}` → `{ name: expr }`. `name="literal"` → `{ name: 'literal' }`.
fn attribute_to_object_member(a: &Attribute) -> Option<ObjectMember> {
    let value: Expression = match &a.value {
        AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        AttributeValue::Single(tag) => tag.expression.clone(),
        AttributeValue::Many(parts) => {
            // For now, only support single-part Text or single-part Expression.
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: t.data.clone(),
                            raw: Some(format!("'{}'", t.raw.replace('\'', "\\'"))),
                            span: Span::ZERO,
                        })))
                    }
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                }
            } else {
                // TODO: concatenated parts → template literal.
                return None;
            }
        }
    };
    Some(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: a.name.clone(),
            span: Span::ZERO,
        }),
        value,
        kind: PropertyKind::Init,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    })))
}

#[allow(dead_code)]
fn single_non_ws_node(f: &svelte_ast::fragment::Fragment) -> Option<&FragmentChild> {
    let non_ws: Vec<&FragmentChild> = f
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if non_ws.len() == 1 {
        Some(non_ws[0])
    } else {
        None
    }
}

fn is_pure_static_fragment(f: &svelte_ast::fragment::Fragment) -> bool {
    fn is_static(n: &FragmentChild) -> bool {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => true,
            FragmentChild::RegularElement(el) => {
                el.attributes.is_empty() && el.fragment.nodes.iter().all(is_static)
            }
            _ => false,
        }
    }
    f.nodes.iter().all(is_static)
}

/// Walk every Expression in a fragment and call-wrap Identifier refs to
/// derived bindings.
fn call_derived_in_fragment(
    f: &mut svelte_ast::fragment::Fragment,
    derived: &std::collections::HashSet<String>,
) {
    for n in &mut f.nodes {
        call_derived_in_node(n, derived);
    }
}

fn call_derived_in_node(
    n: &mut FragmentChild,
    derived: &std::collections::HashSet<String>,
) {
    match n {
        FragmentChild::ExpressionTag(t) => script::call_derived_refs(&mut t.expression, derived),
        FragmentChild::HtmlTag(t) => script::call_derived_refs(&mut t.expression, derived),
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                call_derived_in_attr(attr, derived);
            }
            call_derived_in_fragment(&mut el.fragment, derived);
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                call_derived_in_attr(attr, derived);
            }
            call_derived_in_fragment(&mut c.fragment, derived);
        }
        FragmentChild::SvelteElement(el) => {
            script::call_derived_refs(&mut el.tag, derived);
            for attr in &mut el.attributes {
                call_derived_in_attr(attr, derived);
            }
            call_derived_in_fragment(&mut el.fragment, derived);
        }
        FragmentChild::EachBlock(eb) => {
            script::call_derived_refs(&mut eb.expression, derived);
            if let Some(k) = &mut eb.key {
                script::call_derived_refs(k, derived);
            }
            call_derived_in_fragment(&mut eb.body, derived);
            if let Some(f) = &mut eb.fallback {
                call_derived_in_fragment(f, derived);
            }
        }
        FragmentChild::IfBlock(ib) => {
            script::call_derived_refs(&mut ib.test, derived);
            call_derived_in_fragment(&mut ib.consequent, derived);
            if let Some(a) = &mut ib.alternate {
                call_derived_in_fragment(a, derived);
            }
        }
        FragmentChild::AwaitBlock(ab) => {
            script::call_derived_refs(&mut ab.expression, derived);
            if let Some(p) = &mut ab.pending {
                call_derived_in_fragment(p, derived);
            }
            if let Some(t) = &mut ab.then {
                call_derived_in_fragment(t, derived);
            }
            if let Some(c) = &mut ab.catch_ {
                call_derived_in_fragment(c, derived);
            }
        }
        FragmentChild::KeyBlock(kb) => {
            script::call_derived_refs(&mut kb.expression, derived);
            call_derived_in_fragment(&mut kb.fragment, derived);
        }
        _ => {}
    }
}

fn call_derived_in_attr(
    attr: &mut ElementAttribute,
    derived: &std::collections::HashSet<String>,
) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => {
                script::call_derived_refs(&mut tag.expression, derived);
            }
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        script::call_derived_refs(&mut t.expression, derived);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => {
            script::call_derived_refs(&mut s.expression, derived);
        }
        ElementAttribute::BindDirective(b) => {
            script::call_derived_refs(&mut b.expression, derived);
        }
        _ => {}
    }
}

/// Walk every Expression in a fragment and apply substitute_and_fold.
fn substitute_consts_in_fragment(
    f: &mut svelte_ast::fragment::Fragment,
    consts: &std::collections::HashMap<String, Expression>,
) {
    for n in &mut f.nodes {
        substitute_consts_in_node(n, consts);
    }
}

fn substitute_consts_in_node(
    n: &mut FragmentChild,
    consts: &std::collections::HashMap<String, Expression>,
) {
    match n {
        FragmentChild::ExpressionTag(t) => script::substitute_and_fold(&mut t.expression, consts),
        FragmentChild::HtmlTag(t) => script::substitute_and_fold(&mut t.expression, consts),
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                substitute_consts_in_attr(attr, consts);
            }
            substitute_consts_in_fragment(&mut el.fragment, consts);
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                substitute_consts_in_attr(attr, consts);
            }
            substitute_consts_in_fragment(&mut c.fragment, consts);
        }
        FragmentChild::SvelteElement(el) => {
            script::substitute_and_fold(&mut el.tag, consts);
            for attr in &mut el.attributes {
                substitute_consts_in_attr(attr, consts);
            }
            substitute_consts_in_fragment(&mut el.fragment, consts);
        }
        FragmentChild::EachBlock(eb) => {
            script::substitute_and_fold(&mut eb.expression, consts);
            if let Some(k) = &mut eb.key {
                script::substitute_and_fold(k, consts);
            }
            substitute_consts_in_fragment(&mut eb.body, consts);
            if let Some(f) = &mut eb.fallback {
                substitute_consts_in_fragment(f, consts);
            }
        }
        FragmentChild::IfBlock(ib) => {
            script::substitute_and_fold(&mut ib.test, consts);
            substitute_consts_in_fragment(&mut ib.consequent, consts);
            if let Some(a) = &mut ib.alternate {
                substitute_consts_in_fragment(a, consts);
            }
        }
        FragmentChild::AwaitBlock(ab) => {
            script::substitute_and_fold(&mut ab.expression, consts);
            if let Some(p) = &mut ab.pending {
                substitute_consts_in_fragment(p, consts);
            }
            if let Some(t) = &mut ab.then {
                substitute_consts_in_fragment(t, consts);
            }
            if let Some(c) = &mut ab.catch_ {
                substitute_consts_in_fragment(c, consts);
            }
        }
        FragmentChild::KeyBlock(kb) => {
            script::substitute_and_fold(&mut kb.expression, consts);
            substitute_consts_in_fragment(&mut kb.fragment, consts);
        }
        _ => {}
    }
}

fn substitute_consts_in_attr(
    attr: &mut ElementAttribute,
    consts: &std::collections::HashMap<String, Expression>,
) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => {
                script::substitute_and_fold(&mut tag.expression, consts);
            }
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        script::substitute_and_fold(&mut t.expression, consts);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => {
            script::substitute_and_fold(&mut s.expression, consts);
        }
        ElementAttribute::BindDirective(b) => {
            script::substitute_and_fold(&mut b.expression, consts);
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn fragment_is_empty(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().all(|n| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        _ => false,
    })
}

fn partition_imports(body: &[Statement]) -> Option<(Vec<Statement>, Vec<Statement>)> {
    let mut imports = Vec::new();
    let mut rest = Vec::new();
    let mut saw_non_import = false;
    for s in body {
        match s {
            Statement::Import(_) => {
                if saw_non_import {
                    return None;
                }
                imports.push(s.clone());
            }
            _ => {
                saw_non_import = true;
                rest.push(s.clone());
            }
        }
    }
    Some((imports, rest))
}
