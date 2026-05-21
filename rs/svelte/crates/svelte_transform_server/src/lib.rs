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

pub use typed_fast::{try_typed_server, try_typed_server_with};

// Re-export the filename-aware entry too (declared in this file).
pub use self::try_typed_server_component_with_filename as try_typed_server_component_with_filename_pub;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Backwards-compatible entry — defaults `experimental_async = false`.
pub fn try_typed_server_component(root: &Root, component_name: &str) -> Option<Program> {
    try_typed_server_component_with_filename(root, component_name, false, None)
}

/// Compatibility shim — defaults filename to None, preserve_comments to false.
pub fn try_typed_server_component_with(
    root: &Root,
    component_name: &str,
    experimental_async: bool,
) -> Option<Program> {
    try_typed_server_component_with_opts(
        root, component_name, experimental_async, None, false,
    )
}

/// Compatibility shim — preserve_comments defaults to false.
pub fn try_typed_server_component_with_filename(
    root: &Root,
    component_name: &str,
    experimental_async: bool,
    filename: Option<&str>,
) -> Option<Program> {
    try_typed_server_component_with_opts(
        root, component_name, experimental_async, filename, false,
    )
}

/// Second-tier typed entry point. Currently handles:
/// - "instance script (imports + optionally rune-erasable statements) + simple template"
/// - "single <Component bind:this={x}/>"
/// - "<svelte:element this={tag}>"
pub fn try_typed_server_component_with_opts(
    root: &Root,
    component_name: &str,
    experimental_async: bool,
    filename: Option<&str>,
    preserve_comments: bool,
) -> Option<Program> {
    PRESERVE_COMMENTS.with(|c| c.set(preserve_comments));
    BODY_VAR_COUNTER.with(|c| c.set(0));
    // `<script module>` content is hoisted above the export default
    // function. Statements are pulled in source order; imports flow to
    // `script_imports` so they're emitted with the regular instance imports.
    let mut module_imports: Vec<Statement> = Vec::new();
    let mut module_rest: Vec<Statement> = Vec::new();
    if let Some(m) = root.module.as_ref() {
        for s in m.content.body.iter() {
            match s {
                Statement::Import(_) => module_imports.push(s.clone()),
                _ => module_rest.push(s.clone()),
            }
        }
    }
    // When the source has a `<style>` block, append `svelte-{hash}` to every
    // class attribute and (if css injection is enabled) also emit
    // `const $$css = { hash, code }` + `$$renderer.global.css.add($$css);`.
    // The hash matches upstream's default `cssHash` (hash of filename, or hash
    // of css source if filename is unknown).
    let css_hash: Option<String> = root.css.as_ref().map(|_css| {
        let basis = filename.unwrap_or("(unknown)");
        format!("svelte-{}", svelte_filename_hash(basis))
    });
    // Set the thread-local so element-attribute emission can read the hash.
    CSS_HASH.with(|c| {
        *c.borrow_mut() = css_hash.clone();
    });

    // Defer entirely-static fragments to typed_fast (it's cheaper).
    if root.instance.is_none() && is_pure_static_fragment(&root.fragment) {
        return None;
    }

    // Reset the per-component each-array counter for `<select>` lowering.
    SELECT_EACH_COUNTER.with(|c| c.set(0));
    // Thread filename into the lowering pass for `$.head(HASH, ...)`.
    HEAD_FILENAME.with(|c| {
        *c.borrow_mut() = filename.map(|s| s.to_string());
    });

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
    // Inspect the template + script for any "non-safe identifier" callee
    // (IIFE, member-method on a complex expression, NewExpression). Upstream
    // sets `needs_context = true` in those cases (CallExpression.js line 31,
    // MemberExpression.js line 23), and the server emits `$$renderer.component`
    // wrap when `needs_context` is true.
    if fragment_has_unsafe_call(&root.fragment) {
        needs_component_wrap = true;
    }
    // Script-side unsafe-call check: any `new Expression`, IIFE, method
    // call on a complex object, or non-safe MemberExpression in the
    // script body also triggers needs_context. Mirrors upstream's analyze
    // visitors walking script statements.
    if let Some(s) = root.instance.as_ref() {
        if script_body_has_unsafe(&s.content.body) {
            needs_component_wrap = true;
        }
        // Imports → "unsafe" callee: any `import { X } from '...'; X(...)`
        // in script body or template position triggers needs_context too.
        let import_names = collect_import_names(&s.content.body);
        if !import_names.is_empty() {
            if script_body_has_unsafe_with_imports(&s.content.body, &import_names) {
                needs_component_wrap = true;
            }
            if fragment_has_unsafe_callee(&root.fragment, &import_names) {
                needs_component_wrap = true;
            }
        }
    }
    let mut legacy_export_props: Vec<String> = Vec::new();
    if let Some(s) = root.instance.as_ref() {
        let mut content = s.content.clone();
        let info = script::rewrite_program_for_server(&mut content);
        uses_props = info.uses_props;
        needs_component_wrap |= info.needs_component_wrap();
        derived_bindings = info.derived_bindings;
        legacy_export_props = info.legacy_export_props;
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

    // Upstream's MemberExpression analyzer sets `needs_context = true` for
    // any non-safe MemberExpression in template position. A MemberExpression
    // is "non-safe" when its root identifier resolves to a prop / bindable
    // prop / rest prop / import. We track legacy `export let X` names; any
    // template-position `X.foo` reference triggers the component wrap.
    if !legacy_export_props.is_empty() {
        let prop_names: std::collections::HashSet<String> =
            legacy_export_props.iter().cloned().collect();
        if fragment_has_unsafe_prop_member(&root.fragment, &prop_names) {
            needs_component_wrap = true;
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

    // Extract top-level SnippetBlocks to hoist as separate `function` decls
    // outside the export. Removed from fragment so they don't flow through
    // the template lowering.
    let snippet_decls = extract_and_lower_snippets(&mut fragment)?;

    // Detect Component-with-bind:value at top level — triggers the
    // do-while `$$settled` wrap pattern.
    let needs_bind_wrap = fragment_has_component_bind(&fragment);

    let template_body = if let Some(ai) = &async_info {
        lower_fragment_server_async(
            &fragment,
            &ai.async_bindings,
            ai.last_group_idx,
            &ai.blocker_bindings,
        )?
    } else {
        lower_fragment_server(&fragment)?
    };

    if needs_bind_wrap {
        // Script bindings stay at the outer function-body level; only the
        // template-rendering statements move into `$$render_inner`.
        let outer_settled = wrap_for_bind_settled(template_body);
        func_body.extend(outer_settled);
    } else {
        func_body.extend(template_body);
    }

    // `$$restProps` plumbing — when the template spreads $$restProps, we
    // need to prepend `const $$sanitized_props = $.sanitize_props($$props);
    // const $$restProps = $.rest_props($$sanitized_props, ['name', ...]);`
    // The names list is the legacy export prop names.
    if fragment_uses_rest_props(&fragment) && !legacy_export_props.is_empty() {
        let name_array = Expression::Array(Box::new(ArrayExpression {
            elements: legacy_export_props
                .iter()
                .map(|n| ArrayElement::Expression(string_lit(n)))
                .collect(),
            span: Span::ZERO,
        }));
        let sanitize_decl = t::const_decl(
            "$$sanitized_props",
            t::call(
                t::member_id(t::id("$"), "sanitize_props"),
                vec![t::id("$$props")],
            ),
        );
        let restprops_decl = t::const_decl(
            "$$restProps",
            t::call(
                t::member_id(t::id("$"), "rest_props"),
                vec![t::id("$$sanitized_props"), name_array],
            ),
        );
        // Prepend to the function body.
        let mut prefix = vec![sanitize_decl, restprops_decl];
        prefix.extend(std::mem::take(&mut func_body));
        func_body = prefix;
        uses_props = true;
    }

    // Legacy `export let X` writes back through `$.bind_props` at the end
    // so the parent component sees mutations made inside the child.
    if !legacy_export_props.is_empty() {
        let mut props: Vec<ObjectMember> = Vec::with_capacity(legacy_export_props.len());
        for name in &legacy_export_props {
            props.push(ObjectMember::Property(Box::new(Property {
                key: PropertyKey::Identifier(Identifier {
                    name: name.clone(),
                    span: Span::ZERO,
                }),
                value: t::id(name),
                kind: PropertyKind::Init,
                computed: false,
                shorthand: true,
                method: false,
                span: Span::ZERO,
            })));
        }
        let obj = Expression::Object(Box::new(ObjectExpression {
            properties: props,
            span: Span::ZERO,
        }));
        func_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "bind_props"),
            vec![t::id("$$props"), obj],
        )));
        uses_props = true;
    }

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

    // Build the parameter list. Runes-mode uses_props adds $$props. When the
    // body needs the `$$renderer.component(...)` wrap, upstream always passes
    // `$$props` through to the outer function (so the wrap can forward props
    // into the inner closure even when the script doesn't read them directly).
    let mut params = vec![t::pat_id("$$renderer")];
    if uses_props || needs_component_wrap {
        params.push(t::pat_id("$$props"));
    }

    // Detect async in the template (each-block/if-block test, or top-level
    // await expression tags). If found, emit the `flags/async` import even
    // when the script itself doesn't have top-level await.
    let template_has_async = fragment_has_async(&root.fragment);

    let mut top: Vec<Statement> = Vec::with_capacity(3 + script_imports.len() + module_imports.len() + module_rest.len());
    if experimental_async || async_info.is_some() || template_has_async {
        top.push(t::import_side_effect("svelte/internal/flags/async"));
    }
    top.push(t::import_namespace("$", "svelte/internal/server"));
    top.extend(module_imports);
    top.extend(script_imports);
    // Non-import `<script module>` statements (e.g. `const X = ...`) emit
    // after the imports, before the default export.
    top.extend(module_rest);
    // Hoisted snippet function declarations come before the default export.
    top.extend(snippet_decls);
    top.push(t::export_default_function(component_name, params, func_body));
    Some(t::program(top))
}

/// Extract top-level `{#snippet NAME(...)}` blocks from the fragment, lower
/// each as a `function NAME($$renderer) { body }` declaration, and remove
/// the SnippetBlock children from the fragment.
fn extract_and_lower_snippets(
    fragment: &mut svelte_ast::fragment::Fragment,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut remaining: Vec<FragmentChild> = Vec::with_capacity(fragment.nodes.len());
    for n in std::mem::take(&mut fragment.nodes) {
        if let FragmentChild::SnippetBlock(sb) = &n {
            let name = sb.expression.name.clone();
            // Snippet body needs a leading `<!---->` anchor UNLESS the first
            // non-whitespace child is an `<option>` (which emits a self-anchored
            // `$$renderer.option(...)` call) or a `<select>` etc.
            let first = sb.body.nodes.iter().find(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            });
            let needs_marker = match first {
                Some(FragmentChild::RegularElement(el))
                    if el.name == "option" || el.name == "select" =>
                {
                    false
                }
                _ => body_needs_marker(&sb.body),
            };
            let body_stmts: Vec<Statement> =
                lower_fragment_with_marker(&sb.body, needs_marker)?;
            let mut params = vec![t::pat_id("$$renderer")];
            for p in &sb.parameters {
                params.push(p.clone());
            }
            out.push(t::function_decl(&name, params, body_stmts));
            continue;
        }
        remaining.push(n);
    }
    fragment.nodes = remaining;
    Some(out)
}

/// Does the fragment contain a top-level `<Component bind:X={...} />` where
/// X is not `this`? (bind:this just captures the component instance and
/// doesn't need the $$settled re-render dance.)
fn fragment_has_component_bind(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| {
        if let FragmentChild::Component(c) = n {
            c.attributes.iter().any(|a| {
                if let svelte_ast::attributes::ElementAttribute::BindDirective(b) = a {
                    b.name != "this"
                } else {
                    false
                }
            })
        } else {
            false
        }
    })
}

/// Wrap the function body for `bind:` on Components: declare \$\$settled +
/// \$\$inner_renderer + \$\$render_inner, then loop do/while + subsume.
fn wrap_for_bind_settled(inner: Vec<Statement>) -> Vec<Statement> {
    let mut out: Vec<Statement> = Vec::new();
    // `let $$settled = true;`
    out.push(Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Let,
        declarations: vec![VariableDeclarator {
            id: t::pat_id("$$settled"),
            init: Some(Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            })))),
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    })));
    // `let $$inner_renderer;`
    out.push(Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Let,
        declarations: vec![VariableDeclarator {
            id: t::pat_id("$$inner_renderer"),
            init: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    })));
    // `function $$render_inner($$renderer) { ...inner... }`
    out.push(t::function_decl(
        "$$render_inner",
        vec![t::pat_id("$$renderer")],
        inner,
    ));
    // `do { $$settled = true; $$inner_renderer = $$renderer.copy();
    //      $$render_inner($$inner_renderer); } while (!$$settled);`
    let mut do_body: Vec<Statement> = Vec::new();
    do_body.push(t::stmt(Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(t::id("$$settled")),
        operator: AssignmentOperator::Assign,
        right: Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }))));
    do_body.push(t::stmt(Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(t::id("$$inner_renderer")),
        operator: AssignmentOperator::Assign,
        right: t::call(t::member_id(t::id("$$renderer"), "copy"), Vec::new()),
        span: Span::ZERO,
    }))));
    do_body.push(t::stmt(t::call(
        t::id("$$render_inner"),
        vec![t::id("$$inner_renderer")],
    )));
    let not_settled = Expression::Unary(Box::new(UnaryExpression {
        operator: UnaryOperator::Not,
        argument: t::id("$$settled"),
        prefix: true,
        span: Span::ZERO,
    }));
    out.push(Statement::DoWhile(Box::new(DoWhileStatement {
        body: Statement::Block(Box::new(BlockStatement {
            body: do_body,
            span: Span::ZERO,
        })),
        test: not_settled,
        span: Span::ZERO,
    })));
    // `$$renderer.subsume($$inner_renderer);`
    out.push(t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "subsume"),
        vec![t::id("$$inner_renderer")],
    )));
    out
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
    blocker_bindings: &std::collections::HashMap<String, usize>,
) -> Option<Vec<Statement>> {
    lower_fragment_server_async_with(
        f,
        async_bindings,
        last_group_idx,
        "$$promises",
        blocker_bindings,
    )
}

/// Returns true if any CallExpression in the fragment has a non-safe-identifier
/// callee (an arrow/function IIFE, a complex MemberExpression base, etc).
/// Mirrors upstream's `is_safe_identifier` check inside CallExpression visitor.
fn fragment_has_unsafe_call(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(node_has_unsafe_call)
}

/// Returns true if any spread attribute in the fragment references
/// `$$restProps`. Used to trigger the sanitize_props / rest_props
/// declarations.
fn fragment_uses_rest_props(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(node_uses_rest_props)
}

fn node_uses_rest_props(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::RegularElement(el) => {
            el.attributes.iter().any(|a| match a {
                ElementAttribute::SpreadAttribute(s) => {
                    matches!(&s.expression, Expression::Identifier(i) if i.name == "$$restProps")
                }
                _ => false,
            }) || fragment_uses_rest_props(&el.fragment)
        }
        FragmentChild::Component(c) => {
            c.attributes.iter().any(|a| match a {
                ElementAttribute::SpreadAttribute(s) => {
                    matches!(&s.expression, Expression::Identifier(i) if i.name == "$$restProps")
                }
                _ => false,
            }) || fragment_uses_rest_props(&c.fragment)
        }
        FragmentChild::EachBlock(eb) => {
            fragment_uses_rest_props(&eb.body)
                || eb.fallback.as_ref().map_or(false, fragment_uses_rest_props)
        }
        FragmentChild::IfBlock(ib) => {
            fragment_uses_rest_props(&ib.consequent)
                || ib.alternate.as_ref().map_or(false, fragment_uses_rest_props)
        }
        _ => false,
    }
}

/// Script-side "needs_context" trigger: any NewExpression / unsafe call
/// inside the instance script body. Mirrors upstream's analyze visitors
/// walking script statements (NewExpression always triggers; CallExpression
/// with non-safe callee triggers).
fn script_body_has_unsafe(body: &[Statement]) -> bool {
    body.iter().any(stmt_has_unsafe)
}

/// Collect the imported binding names from `import` statements at the top
/// of a script body. Used to mark CallExpressions whose callee is an
/// imported identifier as "unsafe" (triggers needs_context).
fn collect_import_names(body: &[Statement]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for s in body {
        if let Statement::Import(imp) = s {
            for spec in &imp.specifiers {
                match spec {
                    ImportSpecifierKind::Default(d) => { out.insert(d.local.name.clone()); }
                    ImportSpecifierKind::Namespace(n) => { out.insert(n.local.name.clone()); }
                    ImportSpecifierKind::Named(n) => { out.insert(n.local.name.clone()); }
                }
            }
        }
    }
    out
}

/// Walk script body looking for `CallExpression` whose callee identifier is
/// in `imports`. Mirrors `!is_safe_identifier(callee, scope)` when binding
/// kind is `'import'`.
fn script_body_has_unsafe_with_imports(
    body: &[Statement],
    imports: &std::collections::HashSet<String>,
) -> bool {
    body.iter().any(|s| stmt_has_unsafe_call_with_imports(s, imports))
}

fn stmt_has_unsafe_call_with_imports(
    s: &Statement,
    imports: &std::collections::HashSet<String>,
) -> bool {
    match s {
        Statement::Variable(v) => v.declarations.iter().any(|d| {
            d.init.as_ref().map_or(false, |e| expr_calls_import(e, imports))
        }),
        Statement::Expression(e) => expr_calls_import(&e.expression, imports),
        Statement::Return(r) => r.argument.as_ref().map_or(false, |e| expr_calls_import(e, imports)),
        Statement::If(i) => {
            expr_calls_import(&i.test, imports)
                || stmt_has_unsafe_call_with_imports(&i.consequent, imports)
                || i.alternate.as_ref().map_or(false, |a| stmt_has_unsafe_call_with_imports(a, imports))
        }
        Statement::Block(b) => b.body.iter().any(|s| stmt_has_unsafe_call_with_imports(s, imports)),
        Statement::Function(f) => f.body.body.iter().any(|s| stmt_has_unsafe_call_with_imports(s, imports)),
        _ => false,
    }
}

fn expr_calls_import(
    e: &Expression,
    imports: &std::collections::HashSet<String>,
) -> bool {
    match e {
        Expression::Call(c) => {
            // Callee is a plain Identifier that's imported → unsafe.
            if let Expression::Identifier(id) = &c.callee {
                if imports.contains(&id.name) {
                    return true;
                }
            }
            expr_calls_import(&c.callee, imports)
                || c.arguments.iter().any(|a| match a {
                    svelte_js_ast::Argument::Expression(e) => expr_calls_import(e, imports),
                    svelte_js_ast::Argument::Spread(s) => expr_calls_import(&s.argument, imports),
                })
        }
        Expression::Binary(b) => expr_calls_import(&b.left, imports) || expr_calls_import(&b.right, imports),
        Expression::Logical(l) => expr_calls_import(&l.left, imports) || expr_calls_import(&l.right, imports),
        Expression::Unary(u) => expr_calls_import(&u.argument, imports),
        Expression::Conditional(c) => {
            expr_calls_import(&c.test, imports)
                || expr_calls_import(&c.consequent, imports)
                || expr_calls_import(&c.alternate, imports)
        }
        Expression::Paren(p) => expr_calls_import(&p.expression, imports),
        Expression::Member(m) => expr_calls_import(&m.object, imports),
        Expression::Await(a) => expr_calls_import(&a.argument, imports),
        _ => false,
    }
}

/// Walk a fragment looking for any template-position CallExpression whose
/// callee is an imported identifier.
fn fragment_has_unsafe_callee(
    f: &svelte_ast::fragment::Fragment,
    imports: &std::collections::HashSet<String>,
) -> bool {
    f.nodes.iter().any(|n| node_has_unsafe_callee(n, imports))
}

fn node_has_unsafe_callee(
    n: &FragmentChild,
    imports: &std::collections::HashSet<String>,
) -> bool {
    match n {
        FragmentChild::ExpressionTag(t) => expr_calls_import(&t.expression, imports),
        FragmentChild::HtmlTag(t) => expr_calls_import(&t.expression, imports),
        FragmentChild::ConstTag(ct) => ct.declaration.declarations.iter().any(|d| {
            d.init.as_ref().map_or(false, |e| expr_calls_import(e, imports))
        }),
        FragmentChild::RegularElement(el) => {
            el.attributes.iter().any(|a| attr_has_unsafe_callee(a, imports))
                || fragment_has_unsafe_callee(&el.fragment, imports)
        }
        FragmentChild::Component(c) => {
            c.attributes.iter().any(|a| attr_has_unsafe_callee(a, imports))
                || fragment_has_unsafe_callee(&c.fragment, imports)
        }
        FragmentChild::IfBlock(ib) => {
            expr_calls_import(&ib.test, imports)
                || fragment_has_unsafe_callee(&ib.consequent, imports)
                || ib.alternate.as_ref().map_or(false, |f| fragment_has_unsafe_callee(f, imports))
        }
        FragmentChild::EachBlock(eb) => {
            expr_calls_import(&eb.expression, imports)
                || fragment_has_unsafe_callee(&eb.body, imports)
                || eb.fallback.as_ref().map_or(false, |f| fragment_has_unsafe_callee(f, imports))
        }
        _ => false,
    }
}

fn attr_has_unsafe_callee(
    a: &ElementAttribute,
    imports: &std::collections::HashSet<String>,
) -> bool {
    match a {
        ElementAttribute::Attribute(attr) => match &attr.value {
            AttributeValue::Single(t) => expr_calls_import(&t.expression, imports),
            AttributeValue::Many(parts) => parts.iter().any(|p| match p {
                AttributeValuePart::ExpressionTag(t) => expr_calls_import(&t.expression, imports),
                _ => false,
            }),
            _ => false,
        },
        ElementAttribute::SpreadAttribute(s) => expr_calls_import(&s.expression, imports),
        _ => false,
    }
}

fn stmt_has_unsafe(s: &Statement) -> bool {
    match s {
        Statement::Variable(v) => v.declarations.iter().any(|d| {
            d.init.as_ref().map_or(false, expr_has_unsafe_call)
        }),
        Statement::Expression(e) => expr_has_unsafe_call(&e.expression),
        Statement::Return(r) => r.argument.as_ref().map_or(false, expr_has_unsafe_call),
        Statement::If(i) => {
            expr_has_unsafe_call(&i.test)
                || stmt_has_unsafe(&i.consequent)
                || i.alternate.as_ref().map_or(false, |a| stmt_has_unsafe(a))
        }
        Statement::Block(b) => b.body.iter().any(stmt_has_unsafe),
        Statement::Function(f) => f.body.body.iter().any(stmt_has_unsafe),
        Statement::Throw(t) => expr_has_unsafe_call(&t.argument),
        Statement::Try(t) => {
            t.block.body.iter().any(stmt_has_unsafe)
                || t.handler.as_ref().map_or(false, |h| h.body.body.iter().any(stmt_has_unsafe))
                || t.finalizer.as_ref().map_or(false, |f| f.body.iter().any(stmt_has_unsafe))
        }
        Statement::For(f) => {
            f.test.as_ref().map_or(false, expr_has_unsafe_call)
                || f.update.as_ref().map_or(false, expr_has_unsafe_call)
                || stmt_has_unsafe(&f.body)
        }
        Statement::ForIn(f) => expr_has_unsafe_call(&f.right) || stmt_has_unsafe(&f.body),
        Statement::ForOf(f) => expr_has_unsafe_call(&f.right) || stmt_has_unsafe(&f.body),
        Statement::While(w) => expr_has_unsafe_call(&w.test) || stmt_has_unsafe(&w.body),
        Statement::DoWhile(w) => expr_has_unsafe_call(&w.test) || stmt_has_unsafe(&w.body),
        Statement::Switch(sw) => {
            expr_has_unsafe_call(&sw.discriminant)
                || sw.cases.iter().any(|c| {
                    c.test.as_ref().map_or(false, expr_has_unsafe_call)
                        || c.consequent.iter().any(stmt_has_unsafe)
                })
        }
        Statement::ExportNamed(e) => {
            e.declaration.as_ref().map_or(false, |d| stmt_has_unsafe(d))
        }
        Statement::ExportDefault(e) => match &e.declaration {
            ExportDefault::Function(f) => f.body.body.iter().any(stmt_has_unsafe),
            ExportDefault::Expression(ex) => expr_has_unsafe_call(ex),
            _ => false,
        },
        _ => false,
    }
}

/// Mirrors upstream MemberExpression visitor: returns true if any
/// template-position expression contains a MemberExpression whose root
/// Identifier matches one of the given names (which we treat as
/// prop-kind bindings).
fn fragment_has_unsafe_prop_member(
    f: &svelte_ast::fragment::Fragment,
    props: &std::collections::HashSet<String>,
) -> bool {
    f.nodes.iter().any(|n| node_has_unsafe_prop_member(n, props))
}

fn node_has_unsafe_prop_member(
    n: &FragmentChild,
    props: &std::collections::HashSet<String>,
) -> bool {
    match n {
        FragmentChild::ExpressionTag(t) => expr_root_member_in(&t.expression, props),
        FragmentChild::HtmlTag(t) => expr_root_member_in(&t.expression, props),
        FragmentChild::ConstTag(ct) => ct
            .declaration
            .declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, |e| expr_root_member_in(e, props))),
        FragmentChild::RegularElement(el) => {
            el.attributes.iter().any(|a| attr_has_unsafe_prop_member(a, props))
                || fragment_has_unsafe_prop_member(&el.fragment, props)
        }
        FragmentChild::Component(c) => {
            c.attributes.iter().any(|a| attr_has_unsafe_prop_member(a, props))
                || fragment_has_unsafe_prop_member(&c.fragment, props)
        }
        FragmentChild::SvelteElement(el) => {
            expr_root_member_in(&el.tag, props)
                || el.attributes.iter().any(|a| attr_has_unsafe_prop_member(a, props))
                || fragment_has_unsafe_prop_member(&el.fragment, props)
        }
        FragmentChild::EachBlock(eb) => {
            expr_root_member_in(&eb.expression, props)
                || fragment_has_unsafe_prop_member(&eb.body, props)
                || eb.fallback.as_ref().map_or(false, |f| fragment_has_unsafe_prop_member(f, props))
        }
        FragmentChild::IfBlock(ib) => {
            expr_root_member_in(&ib.test, props)
                || fragment_has_unsafe_prop_member(&ib.consequent, props)
                || ib.alternate.as_ref().map_or(false, |f| fragment_has_unsafe_prop_member(f, props))
        }
        FragmentChild::AwaitBlock(ab) => {
            expr_root_member_in(&ab.expression, props)
                || ab.pending.as_ref().map_or(false, |f| fragment_has_unsafe_prop_member(f, props))
                || ab.then.as_ref().map_or(false, |f| fragment_has_unsafe_prop_member(f, props))
                || ab.catch_.as_ref().map_or(false, |f| fragment_has_unsafe_prop_member(f, props))
        }
        FragmentChild::KeyBlock(kb) => {
            expr_root_member_in(&kb.expression, props)
                || fragment_has_unsafe_prop_member(&kb.fragment, props)
        }
        _ => false,
    }
}

fn attr_has_unsafe_prop_member(
    a: &ElementAttribute,
    props: &std::collections::HashSet<String>,
) -> bool {
    match a {
        ElementAttribute::Attribute(attr) => match &attr.value {
            AttributeValue::Single(tag) => expr_root_member_in(&tag.expression, props),
            AttributeValue::Many(parts) => parts.iter().any(|p| match p {
                AttributeValuePart::ExpressionTag(e) => expr_root_member_in(&e.expression, props),
                _ => false,
            }),
            _ => false,
        },
        ElementAttribute::SpreadAttribute(s) => expr_root_member_in(&s.expression, props),
        ElementAttribute::BindDirective(b) => expr_root_member_in(&b.expression, props),
        ElementAttribute::ClassDirective(c) => expr_root_member_in(&c.expression, props),
        ElementAttribute::StyleDirective(s) => match &s.value {
            AttributeValue::Single(tag) => expr_root_member_in(&tag.expression, props),
            AttributeValue::Many(parts) => parts.iter().any(|p| match p {
                AttributeValuePart::ExpressionTag(e) => expr_root_member_in(&e.expression, props),
                _ => false,
            }),
            _ => false,
        },
        _ => false,
    }
}

/// True if `expr` contains a MemberExpression whose root Identifier is in
/// `names`. A MemberExpression's "root" is the deepest `.object` chain;
/// `is_safe_identifier` upstream walks `.object` until non-Member, then
/// checks the Identifier's binding kind.
fn expr_root_member_in(
    expr: &Expression,
    names: &std::collections::HashSet<String>,
) -> bool {
    match expr {
        Expression::Member(m) => {
            // Walk to the root.
            let mut cur: &Expression = &m.object;
            loop {
                match cur {
                    Expression::Member(inner) => cur = &inner.object,
                    Expression::Identifier(id) => return names.contains(&id.name),
                    _ => return false,
                }
            }
        }
        Expression::Call(c) => {
            expr_root_member_in(&c.callee, names)
                || c.arguments.iter().any(|a| match a {
                    svelte_js_ast::Argument::Expression(e) => expr_root_member_in(e, names),
                    svelte_js_ast::Argument::Spread(s) => expr_root_member_in(&s.argument, names),
                })
        }
        Expression::Binary(b) => {
            expr_root_member_in(&b.left, names) || expr_root_member_in(&b.right, names)
        }
        Expression::Logical(l) => {
            expr_root_member_in(&l.left, names) || expr_root_member_in(&l.right, names)
        }
        Expression::Unary(u) => expr_root_member_in(&u.argument, names),
        Expression::Conditional(c) => {
            expr_root_member_in(&c.test, names)
                || expr_root_member_in(&c.consequent, names)
                || expr_root_member_in(&c.alternate, names)
        }
        Expression::Paren(p) => expr_root_member_in(&p.expression, names),
        Expression::Template(t) => t.expressions.iter().any(|e| expr_root_member_in(e, names)),
        Expression::Array(a) => a.elements.iter().any(|el| match el {
            ArrayElement::Expression(e) => expr_root_member_in(e, names),
            ArrayElement::Spread(s) => expr_root_member_in(&s.argument, names),
            ArrayElement::Elision => false,
        }),
        Expression::Object(o) => o.properties.iter().any(|p| match p {
            ObjectMember::Property(p) => expr_root_member_in(&p.value, names),
            ObjectMember::Spread(s) => expr_root_member_in(&s.argument, names),
        }),
        Expression::Await(a) => expr_root_member_in(&a.argument, names),
        Expression::Spread(s) => expr_root_member_in(&s.argument, names),
        Expression::Sequence(s) => s.expressions.iter().any(|e| expr_root_member_in(e, names)),
        _ => false,
    }
}

fn node_has_unsafe_call(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::ExpressionTag(t) => expr_has_unsafe_call(&t.expression),
        FragmentChild::HtmlTag(t) => expr_has_unsafe_call(&t.expression),
        FragmentChild::ConstTag(ct) => ct
            .declaration
            .declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, expr_has_unsafe_call)),
        FragmentChild::RegularElement(el) => {
            el.attributes.iter().any(attr_has_unsafe_call)
                || fragment_has_unsafe_call(&el.fragment)
        }
        FragmentChild::Component(c) => {
            c.attributes.iter().any(attr_has_unsafe_call)
                || fragment_has_unsafe_call(&c.fragment)
        }
        FragmentChild::SvelteElement(el) => {
            expr_has_unsafe_call(&el.tag)
                || el.attributes.iter().any(attr_has_unsafe_call)
                || fragment_has_unsafe_call(&el.fragment)
        }
        FragmentChild::EachBlock(eb) => {
            expr_has_unsafe_call(&eb.expression)
                || fragment_has_unsafe_call(&eb.body)
                || eb.fallback.as_ref().map_or(false, fragment_has_unsafe_call)
        }
        FragmentChild::IfBlock(ib) => {
            expr_has_unsafe_call(&ib.test)
                || fragment_has_unsafe_call(&ib.consequent)
                || ib.alternate.as_ref().map_or(false, fragment_has_unsafe_call)
        }
        FragmentChild::AwaitBlock(ab) => {
            expr_has_unsafe_call(&ab.expression)
                || ab.pending.as_ref().map_or(false, fragment_has_unsafe_call)
                || ab.then.as_ref().map_or(false, fragment_has_unsafe_call)
                || ab.catch_.as_ref().map_or(false, fragment_has_unsafe_call)
        }
        FragmentChild::KeyBlock(kb) => {
            expr_has_unsafe_call(&kb.expression) || fragment_has_unsafe_call(&kb.fragment)
        }
        _ => false,
    }
}

fn attr_has_unsafe_call(a: &ElementAttribute) -> bool {
    match a {
        ElementAttribute::Attribute(attr) => match &attr.value {
            AttributeValue::Many(parts) => parts.iter().any(|p| match p {
                AttributeValuePart::ExpressionTag(e) => expr_has_unsafe_call(&e.expression),
                _ => false,
            }),
            _ => false,
        },
        ElementAttribute::SpreadAttribute(s) => expr_has_unsafe_call(&s.expression),
        _ => false,
    }
}

fn expr_has_unsafe_call(e: &Expression) -> bool {
    match e {
        Expression::Call(c) => {
            if !is_safe_callee(&c.callee) {
                return true;
            }
            expr_has_unsafe_call(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    svelte_js_ast::Argument::Expression(e) => expr_has_unsafe_call(e),
                    svelte_js_ast::Argument::Spread(s) => expr_has_unsafe_call(&s.argument),
                })
        }
        Expression::New(_) => true,
        Expression::Member(m) => expr_has_unsafe_call(&m.object),
        Expression::Binary(b) => {
            expr_has_unsafe_call(&b.left) || expr_has_unsafe_call(&b.right)
        }
        Expression::Logical(l) => {
            expr_has_unsafe_call(&l.left) || expr_has_unsafe_call(&l.right)
        }
        Expression::Unary(u) => expr_has_unsafe_call(&u.argument),
        Expression::Update(u) => expr_has_unsafe_call(&u.argument),
        Expression::Assignment(a) => {
            (match &a.left {
                AssignmentTarget::Expression(e) => expr_has_unsafe_call(e),
                _ => false,
            }) || expr_has_unsafe_call(&a.right)
        }
        Expression::Conditional(c) => {
            expr_has_unsafe_call(&c.test)
                || expr_has_unsafe_call(&c.consequent)
                || expr_has_unsafe_call(&c.alternate)
        }
        Expression::Paren(p) => expr_has_unsafe_call(&p.expression),
        Expression::Sequence(s) => s.expressions.iter().any(expr_has_unsafe_call),
        Expression::Spread(s) => expr_has_unsafe_call(&s.argument),
        Expression::Await(a) => expr_has_unsafe_call(&a.argument),
        Expression::Array(a) => a.elements.iter().any(|el| match el {
            ArrayElement::Expression(e) => expr_has_unsafe_call(e),
            _ => false,
        }),
        Expression::Template(t) => t.expressions.iter().any(expr_has_unsafe_call),
        // Don't descend into function bodies — only eager evaluation.
        _ => false,
    }
}

/// A callee is "safe" if, after walking through MemberExpressions, we end at
/// a plain Identifier. Arrow/function IIFEs, parenthesized expressions over
/// non-identifiers, and other complex callees count as unsafe.
fn is_safe_callee(e: &Expression) -> bool {
    let mut node = e;
    loop {
        match node {
            Expression::Member(m) => node = &m.object,
            Expression::Paren(p) => node = &p.expression,
            Expression::Identifier(_) => return true,
            _ => return false,
        }
    }
}

/// Builds the final IfStatement once consequent + alternate are known.
fn finalize_if(
    test_is_async: bool,
    raw_test: &Expression,
    consequent_body: Vec<Statement>,
    alternate: Option<Statement>,
) -> Statement {
    let test = if test_is_async {
        wrap_async_test(raw_test)
    } else {
        raw_test.clone()
    };
    Statement::If(Box::new(IfStatement {
        test,
        consequent: Statement::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        alternate,
        span: Span::ZERO,
    }))
}

/// Context threaded through if-chain building inside an async fragment.
/// Lets `build_if_chain_server_ex` decide whether to flatten an elseif into
/// the chain or BREAK OUT into a `child_block` / `async_block`.
struct AsyncCtx<'a> {
    promises_var: &'a str,
    blocker_bindings: &'a std::collections::HashMap<String, usize>,
    /// The set of blockers from the parent if/elseif chain so far. When an
    /// elseif introduces blockers not in this set, the chain breaks.
    parent_blockers: std::collections::BTreeSet<usize>,
    /// Sibling-unique counter for `promises`/`promises_1`/... local vars when
    /// a consequent fragment uses `lower_fragment_with_const_await_with`. Each
    /// time the consequent grabs a name it bumps this counter.
    const_await_counter: std::cell::Cell<usize>,
}

/// Wraps the given block-lowering output in
/// `$$renderer.async_block([$$promises[idx0], ...], (async)? ($$renderer) => { ... });`,
/// optionally marking the arrow async if the test contains await.
fn wrap_async_block(
    inner: Vec<Statement>,
    promises_var: &str,
    blocker_indices: &[usize],
    arrow_is_async: bool,
) -> Statement {
    let blockers = Expression::Array(Box::new(ArrayExpression {
        elements: blocker_indices
            .iter()
            .map(|i| {
                ArrayElement::Expression(Expression::Member(Box::new(MemberExpression {
                    object: t::id(promises_var),
                    property: MemberProperty::Expression(t::lit_number(*i as f64)),
                    computed: true,
                    optional: false,
                    span: Span::ZERO,
                })))
            })
            .collect(),
        span: Span::ZERO,
    }));
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: inner,
            span: Span::ZERO,
        })),
        r#async: arrow_is_async,
        span: Span::ZERO,
    }));
    t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "async_block"),
        vec![blockers, arrow],
    ))
}

/// Wraps the given inner statements in `$$renderer.child_block(async ($$renderer) => { ... });`.
fn wrap_child_block(inner: Vec<Statement>) -> Statement {
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: inner,
            span: Span::ZERO,
        })),
        r#async: true,
        span: Span::ZERO,
    }));
    t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "child_block"),
        vec![arrow],
    ))
}

/// Collects the unique sorted set of blocker indices for any binding referenced
/// (eagerly, not inside nested functions) in `e`. The result is used to build
/// the `[$$promises[i], ...]` array for `async_block`.
fn collect_block_indices_in_expr(
    e: &Expression,
    blocker_bindings: &std::collections::HashMap<String, usize>,
    out: &mut std::collections::BTreeSet<usize>,
) {
    match e {
        Expression::Identifier(id) => {
            if let Some(idx) = blocker_bindings.get(&id.name) {
                out.insert(*idx);
            }
        }
        Expression::Call(c) => {
            collect_block_indices_in_expr(&c.callee, blocker_bindings, out);
            for a in &c.arguments {
                match a {
                    svelte_js_ast::Argument::Expression(e) => {
                        collect_block_indices_in_expr(e, blocker_bindings, out)
                    }
                    svelte_js_ast::Argument::Spread(s) => {
                        collect_block_indices_in_expr(&s.argument, blocker_bindings, out)
                    }
                }
            }
        }
        Expression::Member(m) => {
            collect_block_indices_in_expr(&m.object, blocker_bindings, out);
            if m.computed {
                if let MemberProperty::Expression(e) = &m.property {
                    collect_block_indices_in_expr(e, blocker_bindings, out);
                }
            }
        }
        Expression::Binary(b) => {
            collect_block_indices_in_expr(&b.left, blocker_bindings, out);
            collect_block_indices_in_expr(&b.right, blocker_bindings, out);
        }
        Expression::Logical(l) => {
            collect_block_indices_in_expr(&l.left, blocker_bindings, out);
            collect_block_indices_in_expr(&l.right, blocker_bindings, out);
        }
        Expression::Unary(u) => collect_block_indices_in_expr(&u.argument, blocker_bindings, out),
        Expression::Update(u) => collect_block_indices_in_expr(&u.argument, blocker_bindings, out),
        Expression::Assignment(a) => {
            if let AssignmentTarget::Expression(e) = &a.left {
                collect_block_indices_in_expr(e, blocker_bindings, out);
            }
            collect_block_indices_in_expr(&a.right, blocker_bindings, out);
        }
        Expression::Conditional(c) => {
            collect_block_indices_in_expr(&c.test, blocker_bindings, out);
            collect_block_indices_in_expr(&c.consequent, blocker_bindings, out);
            collect_block_indices_in_expr(&c.alternate, blocker_bindings, out);
        }
        Expression::Paren(p) => collect_block_indices_in_expr(&p.expression, blocker_bindings, out),
        Expression::Sequence(s) => {
            for e in &s.expressions {
                collect_block_indices_in_expr(e, blocker_bindings, out);
            }
        }
        Expression::Spread(s) => collect_block_indices_in_expr(&s.argument, blocker_bindings, out),
        Expression::Await(a) => collect_block_indices_in_expr(&a.argument, blocker_bindings, out),
        Expression::Array(a) => {
            for el in &a.elements {
                if let ArrayElement::Expression(e) = el {
                    collect_block_indices_in_expr(e, blocker_bindings, out);
                }
            }
        }
        Expression::Template(tpl) => {
            for e in &tpl.expressions {
                collect_block_indices_in_expr(e, blocker_bindings, out);
            }
        }
        Expression::New(n) => {
            collect_block_indices_in_expr(&n.callee, blocker_bindings, out);
            for a in &n.arguments {
                match a {
                    svelte_js_ast::Argument::Expression(e) => {
                        collect_block_indices_in_expr(e, blocker_bindings, out)
                    }
                    svelte_js_ast::Argument::Spread(s) => {
                        collect_block_indices_in_expr(&s.argument, blocker_bindings, out)
                    }
                }
            }
        }
        // Stop at function boundaries (template expressions are eager).
        Expression::Function(_) | Expression::Arrow(_) => {}
        _ => {}
    }
}

/// Collects all blocker indices from an IfBlock test plus any nested
/// flattened elseif tests (those without their own await/break-out).
fn collect_if_block_indices(
    ib: &svelte_ast::blocks::IfBlock,
    blocker_bindings: &std::collections::HashMap<String, usize>,
    out: &mut std::collections::BTreeSet<usize>,
) {
    collect_block_indices_in_expr(&ib.test, blocker_bindings, out);
    // Walk down the flattened else-if chain.
    if let Some(alt) = &ib.alternate {
        let non_ws: Vec<&FragmentChild> = alt
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::IfBlock(inner) = non_ws[0] {
                if inner.elseif && !expr_has_await_top(&inner.test) {
                    collect_if_block_indices(inner, blocker_bindings, out);
                }
            }
        }
    }
}

fn lower_fragment_server_async_with(
    f: &svelte_ast::fragment::Fragment,
    async_bindings: &std::collections::HashSet<String>,
    last_group_idx: usize,
    promises_var: &str,
    blocker_bindings: &std::collections::HashMap<String, usize>,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut buf = TemplateBuf::new();
    let nodes = trim_boundary_whitespace(&f.nodes);
    let nodes = trim_boundary_text(nodes);
    // Sibling-shared counter for `promises`/`promises_1`/... names when
    // sibling if-blocks each emit their own `lower_fragment_with_const_await`
    // local. Initial value 0 → first allocation gets `promises`, next
    // `promises_1`, etc.
    let mut const_await_counter: usize = 0;

    // If the first non-whitespace top-level node is an async-tainted
    // ExpressionTag, prepend `<!---->` marker.
    let needs_anchor = matches!(
        nodes.first(),
        Some(FragmentChild::ExpressionTag(t)) if expr_refs_any(&t.expression, async_bindings)
    );
    if needs_anchor {
        buf.push_str("<!---->");
    }

    // Track Comment-before-Text collapse, same rule as `lower_fragment_with_marker`.
    let mut after_dropped_comment = false;
    for n in nodes.iter() {
        if after_dropped_comment {
            if let FragmentChild::Text(t) = n {
                let escaped = escape_text(&collapse_ws(&t.data));
                buf.push_str_after_comment(&escaped);
                after_dropped_comment = false;
                continue;
            }
            after_dropped_comment = false;
        }
        if let FragmentChild::Comment(_) = n {
            after_dropped_comment = true;
            continue;
        }
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
                    out.push(emit_async_wrap_with(
                        &et.expression,
                        last_group_idx,
                        promises_var,
                    ));
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
                    out.push(emit_async_wrap_with(
                        &t.expression,
                        last_group_idx,
                        promises_var,
                    ));
                } else if append_node_to_template(n, &mut buf).is_none() {
                    return None;
                }
            }
            FragmentChild::IfBlock(ib) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                let test_is_async = expr_has_await_top(&ib.test);
                let mut indices: std::collections::BTreeSet<usize> =
                    std::collections::BTreeSet::new();
                collect_if_block_indices(ib, blocker_bindings, &mut indices);
                let indices_vec: Vec<usize> = indices.into_iter().collect();
                let parent_blockers: std::collections::BTreeSet<usize> =
                    indices_vec.iter().copied().collect();
                let ctx = AsyncCtx {
                    promises_var,
                    blocker_bindings,
                    parent_blockers,
                    const_await_counter: std::cell::Cell::new(const_await_counter),
                };
                if !indices_vec.is_empty() {
                    // Build the if-chain without the outer child_block — wrap
                    // ourselves with async_block(blockers, ...). Markers always
                    // use string literals (not template literals) inside an
                    // async_block / child_block body.
                    let if_stmt = build_if_chain_server_ex(ib, 0, true, Some(&ctx))?;
                    out.push(wrap_async_block(
                        vec![if_stmt],
                        promises_var,
                        &indices_vec,
                        test_is_async,
                    ));
                } else if test_is_async {
                    // No blockers but await in test → child_block wrap.
                    let if_stmt = build_if_chain_server_ex(ib, 0, true, Some(&ctx))?;
                    out.push(wrap_child_block(vec![if_stmt]));
                } else {
                    // No blockers, no await — plain if-statement, but in
                    // async-mode markers are still single-quoted strings.
                    let if_stmt = build_if_chain_server_ex(ib, 0, true, Some(&ctx))?;
                    out.push(if_stmt);
                }
                const_await_counter = ctx.const_await_counter.get();
                buf.push_str("<!--]-->");
            }
            FragmentChild::EachBlock(eb) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.extend(lower_each_block_server(eb)?);
            }
            FragmentChild::AwaitBlock(ab) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.extend(lower_await_block_server(ab)?);
                buf.push_str("<!--]-->");
            }
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {
                if append_node_to_template(n, &mut buf).is_none() {
                    return None;
                }
            }
            FragmentChild::Component(c) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.push(lower_component_server(c)?);
                buf.push_str("<!---->");
            }
            FragmentChild::RenderTag(rt) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.push(lower_render_tag_for_select(rt)?);
                buf.push_str("<!---->");
            }
            FragmentChild::SvelteHead(sh) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.push(lower_svelte_head_server(sh)?);
            }
            FragmentChild::SvelteBoundary(sb) => {
                if let Some(stmt) = buf.flush() {
                    out.push(stmt);
                }
                out.extend(lower_svelte_boundary_server(sb)?);
            }
            FragmentChild::SvelteOptions(_) => {
                // metadata, no output
            }
            _ => return None,
        }
    }
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }
    Some(out)
}

/// True when the fragment has a `{@const X = ...}` that needs the async run-
/// array emission shape: either the initializer has top-level `await`, OR it
/// references a script binding that has its own `$$promises[idx]` blocker
/// (mirrors upstream's `2-analyze/visitors/ConstTag.js` decision).
fn fragment_has_const_with_await_or_blocker(
    f: &svelte_ast::fragment::Fragment,
    blocker_bindings: &std::collections::HashMap<String, usize>,
) -> bool {
    f.nodes.iter().any(|n| {
        if let FragmentChild::ConstTag(ct) = n {
            ct.declaration.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |i| {
                    if expr_has_await_top(i) {
                        return true;
                    }
                    let mut indices = std::collections::BTreeSet::new();
                    collect_block_indices_in_expr(i, blocker_bindings, &mut indices);
                    !indices.is_empty()
                })
            })
        } else {
            false
        }
    })
}

/// Legacy await-only check kept for callers that don't have access to
/// `blocker_bindings`. Returns true ONLY when at least one const has top-level
/// await.
fn fragment_has_const_with_await(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| {
        if let FragmentChild::ConstTag(ct) = n {
            ct.declaration.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, expr_has_await_top)
            })
        } else {
            false
        }
    })
}

/// Lower a fragment that contains `{@const X = ...}` tags with await. The
/// pattern:
///   $$renderer.push('<!--[0-->');  // marker (caller emits)
///   let a;
///   let b;
///   var promises = $$renderer.run([async () => a = (await $.save(...))(), () => b = a + 1]);
///   ...rest of fragment lowered with $$renderer.async([promises[N]], ...)...
fn lower_fragment_with_const_await(
    f: &svelte_ast::fragment::Fragment,
) -> Option<Vec<Statement>> {
    let empty: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    lower_fragment_with_const_await_with(f, &empty, "promises")
}

/// Lower a fragment that contains `{@const X = ...}` tags needing the run-
/// array emission. Each const is its own thunk (or pair of thunks when
/// blockers are present):
///   - `await E`             → `async () => X = REWRITE(E)` (one thunk)
///   - blocker-bound, no await→ `() => $$promises[idx]` then `() => X = INIT` (two thunks)
///   - plain                  → `() => X = INIT` (one thunk)
/// `promises_var` controls the local var name (default `promises`, but
/// caller may pass `promises_1` etc to dedupe across sibling fragments).
fn lower_fragment_with_const_await_with(
    f: &svelte_ast::fragment::Fragment,
    blocker_bindings: &std::collections::HashMap<String, usize>,
    promises_var: &str,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut const_names: Vec<String> = Vec::new();
    let mut groups: Vec<Expression> = Vec::new();
    let mut rest_nodes: Vec<FragmentChild> = Vec::new();
    let mut last_was_async = false;

    for n in &f.nodes {
        if let FragmentChild::ConstTag(ct) = n {
            for d in &ct.declaration.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    const_names.push(id.name.clone());

                    // Collect blockers from init (eager identifier references
                    // to script bindings that have `$$promises[idx]`).
                    let mut blocker_set: std::collections::BTreeSet<usize> =
                        std::collections::BTreeSet::new();
                    collect_block_indices_in_expr(init, blocker_bindings, &mut blocker_set);

                    let has_await = expr_has_await_top(init);

                    // Blocker thunks: emit `() => $$promises[idx]` for each
                    // blocker the init depends on. With multiple blockers
                    // upstream wraps in `Promise.all([...])`.
                    if !blocker_set.is_empty() {
                        let elems: Vec<Expression> = blocker_set
                            .iter()
                            .map(|i| Expression::Member(Box::new(MemberExpression {
                                object: t::id("$$promises"),
                                property: MemberProperty::Expression(t::lit_number(*i as f64)),
                                computed: true,
                                optional: false,
                                span: Span::ZERO,
                            })))
                            .collect();
                        let body = if elems.len() == 1 {
                            elems[0].clone()
                        } else {
                            t::call(
                                t::member_id(t::id("Promise"), "all"),
                                vec![Expression::Array(Box::new(ArrayExpression {
                                    elements: elems
                                        .into_iter()
                                        .map(ArrayElement::Expression)
                                        .collect(),
                                    span: Span::ZERO,
                                }))],
                            )
                        };
                        groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: Vec::new(),
                            body: ArrowBody::Expression(body),
                            r#async: false,
                            span: Span::ZERO,
                        })));
                    }

                    // Setter thunk.
                    let setter_init = if has_await {
                        wrap_async_test(init)
                    } else {
                        init.clone()
                    };
                    let assign = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id(&id.name)),
                        operator: AssignmentOperator::Assign,
                        right: setter_init,
                        span: Span::ZERO,
                    }));
                    groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        body: ArrowBody::Expression(assign),
                        r#async: has_await,
                        span: Span::ZERO,
                    })));
                    last_was_async = has_await;
                } else {
                    return None;
                }
            }
        } else {
            rest_nodes.push(n.clone());
        }
    }

    // Trailing `() => undefined` only when the last group is async. Mirrors
    // upstream's `b.thunk(...)` pattern: an async-trailing run-array needs a
    // sync fallback so `last_group_idx` lands on something awaitable.
    if last_was_async {
        groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            body: ArrowBody::Expression(Expression::Identifier(Identifier {
                name: "undefined".to_string(),
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        })));
    }

    // Emit hoisted lets
    for name in &const_names {
        out.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: t::pat_id(name),
                init: None,
                span: Span::ZERO,
            }],
            span: Span::ZERO,
        })));
    }
    // var <promises_var> = $$renderer.run([...]);
    out.push(t::var(
        promises_var,
        t::call(
            t::member_id(t::id("$$renderer"), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: groups.iter().cloned().map(ArrayElement::Expression).collect(),
                span: Span::ZERO,
            }))],
        ),
    ));

    // Lower the rest of the fragment with the const names treated as
    // async-tainted. `<promises_var>` is the local var name.
    let last_idx = if groups.is_empty() { 0 } else { groups.len() - 1 };
    let async_set: std::collections::HashSet<String> = const_names.into_iter().collect();
    let stub_fragment = svelte_ast::fragment::Fragment { nodes: rest_nodes };
    let empty_blockers: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    out.extend(lower_fragment_server_async_with(
        &stub_fragment,
        &async_set,
        last_idx,
        promises_var,
        &empty_blockers,
    )?);
    Some(out)
}

fn fragment_has_async(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(node_has_async)
}

fn node_has_async(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::ExpressionTag(t) => expr_has_await_top(&t.expression),
        FragmentChild::HtmlTag(t) => expr_has_await_top(&t.expression),
        FragmentChild::ConstTag(ct) => ct
            .declaration
            .declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, expr_has_await_top)),
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
        ElementAttribute::ClassDirective(c) => expr_has_await_top(&c.expression),
        ElementAttribute::StyleDirective(s) => match &s.value {
            AttributeValue::Single(tag) => expr_has_await_top(&tag.expression),
            AttributeValue::Many(parts) => parts.iter().any(|p| {
                matches!(p, AttributeValuePart::ExpressionTag(t) if expr_has_await_top(&t.expression))
            }),
            _ => false,
        },
        ElementAttribute::OnDirective(o) => o
            .expression
            .as_ref()
            .map_or(false, expr_has_await_top),
        ElementAttribute::UseDirective(u) => u
            .expression
            .as_ref()
            .map_or(false, expr_has_await_top),
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
    emit_async_wrap_with(expr, group_idx, "$$promises")
}

fn emit_async_wrap_with(expr: &Expression, group_idx: usize, promises_var: &str) -> Statement {
    let promises_slot = Expression::Member(Box::new(MemberExpression {
        object: t::id(promises_var),
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
/// The leading `<!---->` anchor mirrors upstream's `is_text_first` rule:
/// if the first non-WS child is Text or ExpressionTag (or @html), insert
/// a marker so the text node doesn't get fused with surrounding fragments.
fn lower_fragment_server(f: &svelte_ast::fragment::Fragment) -> Option<Vec<Statement>> {
    lower_fragment_with_marker(f, is_text_first(f))
}

fn is_text_first(f: &svelte_ast::fragment::Fragment) -> bool {
    let first = f.nodes.iter().find(|n| match n {
        FragmentChild::Text(t) => !t.data.trim().is_empty(),
        FragmentChild::Comment(_) => false,
        _ => true,
    });
    matches!(
        first,
        Some(FragmentChild::Text(_))
            | Some(FragmentChild::ExpressionTag(_))
            | Some(FragmentChild::HtmlTag(_))
    )
}

thread_local! {
    /// Shared each-array counter spanning every `<select>` in the current
    /// component lowering. Reset at the top of `try_typed_server_component`.
    static SELECT_EACH_COUNTER: std::cell::Cell<usize> = std::cell::Cell::new(0);
    /// Filename for the current component compile, used to seed the
    /// `$.head(HASH, ...)` hash. Set by `try_typed_server_component_with_filename`.
    static HEAD_FILENAME: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
    /// `svelte-{hash}` to append to every scoped class attribute, or None
    /// when the source has no `<style>` block.
    static CSS_HASH: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
    /// `compilerOptions.preserveComments`. When true, HTML comments
    /// (`<!-- ... -->`) are emitted verbatim in the SSR output instead of
    /// being dropped.
    static PRESERVE_COMMENTS: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    /// Counter for `$$body` / `$$body_1` / ... name allocation for
    /// content-editable bind:innerText/textContent/innerHTML + textarea.
    static BODY_VAR_COUNTER: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

/// Upstream's `hash(filename)` for `$.head(HASH, ...)`. DJB2 variant
/// (XOR rather than add) base-36 encoded as u32. Mirrors
/// `packages/svelte/src/utils.js`.
fn svelte_filename_hash(s: &str) -> String {
    let s: String = s.chars().filter(|c| *c != '\r').collect();
    let mut h: i64 = 5381;
    for c in s.chars().rev() {
        h = ((h << 5) - h) ^ (c as i64);
        h &= 0xFFFFFFFF;
    }
    let mut n = h as u32;
    if n == 0 {
        return "0".into();
    }
    let chars: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
    let mut out = String::new();
    while n > 0 {
        out.insert(0, chars[(n % 36) as usize]);
        n /= 36;
    }
    out
}

/// Lower `<svelte:head>...` into `$.head(HASH, $$renderer, ($$renderer) => { BODY })`.
/// `<title>` children inside the head become `$$renderer.title(($$renderer) => { ... })`.
fn lower_svelte_head_server(
    sh: &svelte_ast::elements::SvelteHead,
) -> Option<Statement> {
    // Compute hash from filename if set, else default to upstream's "(unknown)".
    let filename: String = HEAD_FILENAME.with(|c| {
        c.borrow().clone().unwrap_or_else(|| "(unknown)".to_string())
    });
    let hash = svelte_filename_hash(&filename);

    // Lower the head fragment body. We mirror lower_fragment_with_marker
    // logic, but with one extra rule: `<title>` children become a
    // `$$renderer.title(($$renderer) => { $$renderer.push(\`<title>...</title>\`); })`
    // call.
    let body = lower_head_fragment(&sh.fragment)?;

    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id("$"), "head"),
        vec![
            Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: hash.clone(),
                raw: Some(format!("'{hash}'")),
                span: Span::ZERO,
            }))),
            t::id("$$renderer"),
            arrow,
        ],
    )))
}

/// Lower `<svelte:boundary>...</svelte:boundary>` to
/// `$$renderer.boundary({ failed, pending }, ($$renderer) => { ... })`,
/// or — when there's no `failed`/`pending` (only `onerror` or nothing) —
/// just `<!--[--> { body } <!--]-->` with no wrap.
///
/// Snippets come in two flavors:
///   - `{#snippet failed(...)}` blocks inside the boundary fragment → hoist
///     as local function declarations + reference by name in the object.
///   - `<svelte:boundary failed={ref} pending={ref}>` attributes → already
///     bound to a snippet; emit as shorthand or key:value pairs.
fn lower_svelte_boundary_server(
    sb: &svelte_ast::elements::SvelteBoundary,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();

    // 1. Extract `failed`/`pending` snippet attributes (compile-time names
    //    that map to a snippet binding).
    let mut attr_snippets: Vec<(String, Expression)> = Vec::new();
    for a in &sb.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if attr.name == "failed" || attr.name == "pending" {
                // Take the expression behind the attribute value.
                let expr = match &attr.value {
                    AttributeValue::Single(t) => Some(t.expression.clone()),
                    AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
                        AttributeValuePart::ExpressionTag(t) => Some(t.expression.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(e) = expr {
                    attr_snippets.push((attr.name.clone(), e));
                }
            }
        }
    }

    // 2. Snippet blocks inside the boundary → hoisted function decls.
    let mut snippet_names: Vec<String> = Vec::new();
    let mut body_fragment = sb.fragment.clone();
    let mut remaining: Vec<FragmentChild> = Vec::with_capacity(body_fragment.nodes.len());
    for n in std::mem::take(&mut body_fragment.nodes) {
        if let FragmentChild::SnippetBlock(snip) = &n {
            let name = snip.expression.name.clone();
            let needs_marker = body_needs_marker(&snip.body);
            let body_stmts = lower_fragment_with_marker(&snip.body, needs_marker)?;
            let mut params = vec![t::pat_id("$$renderer")];
            for p in &snip.parameters {
                params.push(p.clone());
            }
            out.push(t::function_decl(&name, params, body_stmts));
            snippet_names.push(name);
            continue;
        }
        remaining.push(n);
    }
    body_fragment.nodes = remaining;

    // 3. Build the body statements once (used by both wrap and no-wrap paths).
    let body_stmts = lower_fragment_server(&body_fragment)?;

    // 4. If there are no failed/pending snippets at all, emit the simpler
    //    no-wrap form: just `<!--[-->` + Block + `<!--]-->`. This keeps the
    //    boundary scope markers but skips the `$$renderer.boundary(...)`
    //    call (which is only needed to install error/pending hooks).
    if snippet_names.is_empty() && attr_snippets.is_empty() {
        out.push(push_template("<!--[-->"));
        out.push(Statement::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })));
        out.push(push_template("<!--]-->"));
        return Some(out);
    }

    // 5. Otherwise, build the `{ failed, pending }` object and the
    //    `$$renderer.boundary(...)` call.
    let mut obj_props: Vec<ObjectMember> = Vec::new();
    // Attribute-style names first, then snippet-block names. Dedup by name.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, expr) in attr_snippets {
        if !seen.insert(name.clone()) {
            continue;
        }
        // Shorthand if the expression is an identifier with matching name.
        let shorthand = matches!(&expr, Expression::Identifier(i) if i.name == name);
        obj_props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: name.clone(),
                span: Span::ZERO,
            }),
            value: expr,
            kind: PropertyKind::Init,
            computed: false,
            shorthand,
            method: false,
            span: Span::ZERO,
        })));
    }
    for name in &snippet_names {
        if !seen.insert(name.clone()) {
            continue;
        }
        obj_props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: name.clone(),
                span: Span::ZERO,
            }),
            value: t::id(name),
            kind: PropertyKind::Init,
            computed: false,
            shorthand: true,
            method: false,
            span: Span::ZERO,
        })));
    }
    let snippets_obj = Expression::Object(Box::new(ObjectExpression {
        properties: obj_props,
        span: Span::ZERO,
    }));

    let mut arrow_body: Vec<Statement> = Vec::new();
    arrow_body.push(push_template("<!--[-->"));
    arrow_body.push(Statement::Block(Box::new(BlockStatement {
        body: body_stmts,
        span: Span::ZERO,
    })));
    arrow_body.push(push_template("<!--]-->"));
    let body_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    out.push(t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "boundary"),
        vec![snippets_obj, body_arrow],
    )));
    Some(out)
}

fn lower_head_fragment(
    f: &svelte_ast::fragment::Fragment,
) -> Option<Vec<Statement>> {
    let mut out: Vec<Statement> = Vec::new();
    let mut buf = TemplateBuf::new();
    let nodes = trim_boundary_whitespace(&f.nodes);
    let nodes = trim_boundary_text(nodes);
    let mut last_was_component = false;
    let mut after_dropped_comment = false;
    // Tracks whether the previous emitted node was a `$$renderer.title(...)`
    // call (or similar side-statement) — when set, the leading whitespace of
    // the next Text should be trimmed so the next `push` doesn't start with
    // ` `. Mirrors upstream's clean_nodes whitespace handling.
    let mut trim_leading_ws = false;
    for n in nodes.iter() {
        if last_was_component {
            if !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()) {
                buf.push_str("<!---->");
                last_was_component = false;
            }
        }
        if after_dropped_comment {
            if let FragmentChild::Text(t) = n {
                let escaped = escape_text(&collapse_ws(&t.data));
                buf.push_str_after_comment(&escaped);
                after_dropped_comment = false;
                continue;
            }
            after_dropped_comment = false;
        }
        if let FragmentChild::Comment(_) = n {
            after_dropped_comment = true;
            continue;
        }
        if trim_leading_ws {
            if let FragmentChild::Text(t) = n {
                let trimmed = t.data.trim_start();
                if trimmed.is_empty() {
                    trim_leading_ws = false;
                    continue;
                }
                let escaped = escape_text(&collapse_ws(trimmed));
                buf.push_str(&escaped);
                trim_leading_ws = false;
                continue;
            }
            trim_leading_ws = false;
        }
        // `<title>` (parsed as TitleElement variant) → separate
        // `$$renderer.title(($$renderer) => { ... })` call.
        if let FragmentChild::TitleElement(el) = n {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            let mut inner_buf = TemplateBuf::new();
            inner_buf.push_str("<title>");
            let kids = trim_boundary_whitespace(&el.fragment.nodes);
            let kids = trim_boundary_text(kids);
            for k in kids.iter() {
                append_node_to_template(k, &mut inner_buf)?;
            }
            inner_buf.push_str("</title>");
            let inner_body = inner_buf.flush().into_iter().collect::<Vec<_>>();
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: vec![t::pat_id("$$renderer")],
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: inner_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            out.push(t::stmt(t::call(
                t::member_id(t::id("$$renderer"), "title"),
                vec![arrow],
            )));
            last_was_component = false;
            trim_leading_ws = true;
            continue;
        }
        // Non-inline (Components, blocks) inside <svelte:head> follow the
        // same flush + emit pattern as the top-level loop.
        if let FragmentChild::RegularElement(el) = n {
            if element_contains_non_inline(el) {
                lower_element_with_non_inline_children(el, &mut buf, &mut out)?;
                last_was_component = false;
                continue;
            }
        }
        if append_node_to_template(n, &mut buf).is_none() {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            match n {
                FragmentChild::Component(c) => {
                    out.push(lower_component_server(c)?);
                    last_was_component = true;
                }
                FragmentChild::SvelteElement(el) => {
                    out.push(lower_svelte_element_server(el)?);
                }
                FragmentChild::EachBlock(eb) => {
                    out.extend(lower_each_block_server(eb)?);
                }
                FragmentChild::IfBlock(ib) => {
                    out.extend(lower_if_block_server(ib)?);
                    buf.push_str("<!--]-->");
                }
                _ => return None,
            }
        } else {
            last_was_component = false;
        }
    }
    // Trailing-Component anchor: only when the head body had preceding
    // non-Component content (mirrors the main loop's
    // `emitted_static_push && last_was_component` rule).
    let emitted_static_push = !out.is_empty();
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    } else if last_was_component && emitted_static_push {
        // Need to know if there was any non-Component statement
        // before the Component. `out` contains the function decls AND
        // any prior pushes/Component calls. Simplification: if there's
        // anything besides the last Component call, emit the anchor.
        let has_non_component = out
            .iter()
            .take(out.len().saturating_sub(1))
            .any(|s| !matches!(s, Statement::Expression(e)
                if matches!(&e.expression, Expression::Call(c)
                    if matches!(&c.callee, Expression::Identifier(id) if id.name.chars().next().map_or(false, |ch| ch.is_uppercase())))
            ));
        if has_non_component {
            out.push(push_template("<!---->"));
        }
    }
    Some(out)
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
    // Set to true after emitting a non-template side-statement
    // (`$.head(...)`, `$$renderer.title(...)`, etc.) so the next Text's
    // leading whitespace gets trimmed.
    let mut trim_leading_ws = false;
    let mut last_was_component = false;
    // True when the previous node was a Comment whose leading whitespace
    // pairs with the following text's leading whitespace — strip the lead
    // so we collapse the `text<comment>text` whitespace bridge to ONE space.
    // Extracted SnippetBlocks DON'T set this (they preserve the boundary
    // and let two adjacent texts emit two spaces).
    let mut after_dropped_comment = false;
    for n in nodes.iter() {
        // Before processing this node, if the previous node was a Component
        // and we're now about to emit anything (even whitespace), push
        // `<!---->` to buf so it anchors the hydration scope. The marker
        // goes IN FRONT of any whitespace that follows the Component, so
        // the order is `Component(); push(\`<!----> ...\`)`.
        if last_was_component {
            buf.push_str("<!---->");
            last_was_component = false;
        }
        // Text immediately after a dropped Comment: collapse leading
        // whitespace against the previous trailing whitespace.
        if after_dropped_comment {
            if let FragmentChild::Text(t) = n {
                let escaped = escape_text(&collapse_ws(&t.data));
                buf.push_str_after_comment(&escaped);
                after_dropped_comment = false;
                continue;
            }
            after_dropped_comment = false;
        }
        if let FragmentChild::Comment(_) = n {
            if !PRESERVE_COMMENTS.with(|p| p.get()) {
                after_dropped_comment = true;
                continue;
            }
            // preserveComments=true: fall through so the comment lands in
            // the template literal via append_node_to_template.
        }
        if trim_leading_ws {
            if let FragmentChild::Text(t) = n {
                let trimmed = t.data.trim_start();
                if trimmed.is_empty() {
                    trim_leading_ws = false;
                    continue;
                }
                let escaped = escape_text(&collapse_ws(trimmed));
                buf.push_str(&escaped);
                trim_leading_ws = false;
                continue;
            }
            trim_leading_ws = false;
        }
        // `<option>` ANYWHERE (even outside `<select>`) becomes
        // `$$renderer.option(...)` — mirrors upstream's `is_option_special`
        // rule which fires regardless of parent context.
        if let FragmentChild::RegularElement(el) = n {
            if el.name == "option" {
                if let Some(stmt) = buf.flush() {
                    emitted_static_push = true;
                    out.push(stmt);
                }
                out.push(lower_option_server(el)?);
                last_was_component = false;
                continue;
            }
        }
        // `bind:innerText` / `bind:textContent` / `bind:innerHTML` on a
        // RegularElement lowers to the same `$$body` extraction pattern
        // as `<textarea>` — with `$.escape` for the first two, raw value
        // for innerHTML.
        if let FragmentChild::RegularElement(el) = n {
            let bind_body: Option<(&str, Expression)> = el.attributes.iter().find_map(|a| match a {
                ElementAttribute::BindDirective(b)
                    if matches!(b.name.as_str(), "innerText" | "textContent" | "innerHTML") =>
                {
                    Some((b.name.as_str(), b.expression.clone()))
                }
                _ => None,
            });
            if let Some((name, expr)) = bind_body {
                // Open tag goes into buf (flushed before the inner stmts).
                // After the body extraction stmts, the close tag is pushed
                // back into buf so adjacent WS/elements can join with it.
                lower_content_editable_bind_inline(
                    el, name, expr, &mut buf, &mut out,
                )?;
                last_was_component = false;
                continue;
            }
        }
        // `<textarea>` with a `value=` attr OR a non-empty body uses the
        // `$$body = $.escape(...)` pattern. Otherwise it stays inline
        // (just `<textarea attrs></textarea>`).
        if let FragmentChild::RegularElement(el) = n {
            if el.name == "textarea" {
                let has_value = el.attributes.iter().any(|a| matches!(
                    a,
                    ElementAttribute::Attribute(attr) if attr.name == "value"
                ) || matches!(
                    a,
                    ElementAttribute::BindDirective(b) if b.name == "value"
                ));
                let has_body = el.fragment.nodes.iter().any(|c| match c {
                    FragmentChild::Text(t) => !t.data.trim().is_empty(),
                    FragmentChild::Comment(_) => false,
                    _ => true,
                });
                if has_value || has_body {
                    lower_textarea_server_inline(el, &mut buf, &mut out)?;
                    last_was_component = false;
                    continue;
                }
            }
        }
        // `<select value=X ...>` or `<select bind:value={x}>` → wrap shape
        //   $$renderer.select({ value: X }, ($$renderer) => { ...options... });
        // Pure `<select>` (no value attr) keeps the inline shape.
        if let FragmentChild::RegularElement(el) = n {
            if el.name == "select" {
                if let Some(value_expr) = select_value_attr(el) {
                    if let Some(stmt) = buf.flush() {
                        emitted_static_push = true;
                        out.push(stmt);
                    }
                    out.push(lower_select_with_value(el, value_expr)?);
                    last_was_component = false;
                    continue;
                }
            }
        }
        // `<select>` (or other option-child container) routes to the inline
        // emitter so blocks inside it can produce `$$renderer.option(...)` calls.
        if let FragmentChild::RegularElement(el) = n {
            if has_option_child(el) {
                emit_select_inline(el, &mut buf, &mut out)?;
                last_was_component = false;
                continue;
            }
        }
        // RegularElement wrapping a non-inlineable descendant (Component,
        // EachBlock, etc.) needs to be lowered before we attempt the
        // template-buf path — otherwise the inline path writes the open
        // tag and then bails halfway through.
        if let FragmentChild::RegularElement(el) = n {
            if element_contains_non_inline(el) {
                lower_element_with_non_inline_children(el, &mut buf, &mut out)?;
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
                FragmentChild::ConstTag(ct) => {
                    // Regular `{@const X = INIT}` without await — emit as a
                    // plain `let X = INIT;` declaration. The async-aware shape
                    // (deferred `$$renderer.run([…])`) is handled separately by
                    // `lower_fragment_with_const_await` when an await is in
                    // play.
                    out.push(Statement::Variable(Box::new(VariableDeclaration {
                        kind: VariableKind::Let,
                        declarations: ct.declaration.declarations.clone(),
                        span: Span::ZERO,
                    })));
                    last_was_component = false;
                }
                FragmentChild::SvelteHead(sh) => {
                    out.push(lower_svelte_head_server(sh)?);
                    last_was_component = false;
                    trim_leading_ws = true;
                }
                FragmentChild::SvelteOptions(_) => {
                    // `<svelte:options ...>` is compile-time metadata —
                    // css mode, namespace, custom-element flags. No output.
                    last_was_component = false;
                    trim_leading_ws = true;
                }
                FragmentChild::SvelteBoundary(sb) => {
                    out.extend(lower_svelte_boundary_server(sb)?);
                    last_was_component = false;
                }
                FragmentChild::RenderTag(rt) => {
                    out.push(lower_render_tag_for_select(rt)?);
                    // `{@render snippet()}` at top level (outside select)
                    // emits a trailing `<!---->` anchor on the next push.
                    buf.push_str("<!---->");
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
        // Mid-fragment Component with no trailing content: emit an anchor
        // `<!---->` to close the hydration scope.
        out.push(push_template("<!---->"));
    }
    Some(out)
}

/// Lower an element with `bind:innerText` / `bind:textContent` /
/// `bind:innerHTML` inline: open tag flows into `buf`, body-extraction
/// stmts go to `out`, then close tag is pushed back to `buf` so adjacent
/// content can fuse with it on the next push.
fn lower_content_editable_bind_inline(
    el: &svelte_ast::elements::RegularElement,
    bind_name: &str,
    bind_expr: Expression,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
) -> Option<()> {
    // Open tag (current buf gets the open tag string appended).
    buf.push_str("<");
    buf.push_str(&el.name);
    for attr in &el.attributes {
        if let ElementAttribute::BindDirective(b) = attr {
            if matches!(b.name.as_str(), "innerText" | "textContent" | "innerHTML") {
                continue;
            }
        }
        append_element_attribute_server(attr, buf)?;
    }
    buf.push_str(">");
    // Flush the open-tag push.
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }

    let body_source: Expression = if bind_name == "innerHTML" {
        bind_expr
    } else {
        Expression::Call(Box::new(CallExpression {
            callee: t::member_id(t::id("$"), "escape"),
            arguments: vec![Argument::Expression(bind_expr)],
            optional: false,
            span: Span::ZERO,
        }))
    };

    if bind_name == "innerHTML" {
        // innerHTML pushes the raw expression conditionally — no $$body const.
        out.push(Statement::If(Box::new(IfStatement {
            test: body_source.clone(),
            consequent: Statement::Block(Box::new(BlockStatement {
                body: vec![t::stmt(t::call(
                    t::member_id(t::id("$$renderer"), "push"),
                    vec![t::template_raw(
                        vec![String::new(), String::new()],
                        vec![body_source],
                    )],
                ))],
                span: Span::ZERO,
            })),
            alternate: Some(Statement::Block(Box::new(BlockStatement {
                body: Vec::new(),
                span: Span::ZERO,
            }))),
            span: Span::ZERO,
        })));
    } else {
        let idx = BODY_VAR_COUNTER.with(|c| {
            let i = c.get();
            c.set(i + 1);
            i
        });
        let body_var = if idx == 0 { "$$body".to_string() } else { format!("$$body_{idx}") };
        out.push(t::const_decl(&body_var, body_source));
        out.push(Statement::If(Box::new(IfStatement {
            test: t::id(&body_var),
            consequent: Statement::Block(Box::new(BlockStatement {
                body: vec![t::stmt(t::call(
                    t::member_id(t::id("$$renderer"), "push"),
                    vec![t::template_raw(
                        vec![String::new(), String::new()],
                        vec![t::id(&body_var)],
                    )],
                ))],
                span: Span::ZERO,
            })),
            alternate: Some(Statement::Block(Box::new(BlockStatement {
                body: Vec::new(),
                span: Span::ZERO,
            }))),
            span: Span::ZERO,
        })));
    }

    // Close tag goes back into buf so the next iteration's content fuses
    // with it (`</div> <div ...>` shows up as one push instead of three).
    buf.push_str(&format!("</{}>", el.name));
    Some(())
}

/// Inline version of `lower_textarea_server` that flushes the open tag
/// to `buf`/`out`, emits the body extraction stmts to `out`, then pushes
/// `</textarea>` back into `buf` so adjacent content can fuse with it.
fn lower_textarea_server_inline(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
) -> Option<()> {
    // Push open tag into buf, capture value expr.
    buf.push_str("<textarea");
    let mut value_expr: Option<Expression> = None;
    for attr in &el.attributes {
        match attr {
            ElementAttribute::Attribute(a) if a.name == "value" => {
                value_expr = match &a.value {
                    AttributeValue::Single(t) => Some(t.expression.clone()),
                    AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
                        AttributeValuePart::ExpressionTag(t) => Some(t.expression.clone()),
                        AttributeValuePart::Text(t) => Some(string_lit(&t.data)),
                    },
                    _ => None,
                };
            }
            ElementAttribute::BindDirective(b) if b.name == "value" => {
                value_expr = Some(b.expression.clone());
            }
            _ => {
                append_element_attribute_server(attr, buf)?;
            }
        }
    }
    buf.push_str(">");
    if let Some(stmt) = buf.flush() {
        out.push(stmt);
    }

    // Determine body source.
    let body_source: Expression = if let Some(v) = value_expr {
        v
    } else {
        let mut quasis: Vec<String> = Vec::new();
        let mut exprs: Vec<Expression> = Vec::new();
        let mut pending = String::new();
        let mut first_text = true;
        for child in &el.fragment.nodes {
            match child {
                FragmentChild::Text(t) => {
                    let data = if first_text {
                        first_text = false;
                        t.data.strip_prefix('\n').unwrap_or(&t.data)
                    } else {
                        &t.data
                    };
                    pending.push_str(data);
                }
                FragmentChild::ExpressionTag(tag) => {
                    first_text = false;
                    quasis.push(std::mem::take(&mut pending));
                    exprs.push(Expression::Call(Box::new(CallExpression {
                        callee: t::member_id(t::id("$"), "stringify"),
                        arguments: vec![Argument::Expression(tag.expression.clone())],
                        optional: false,
                        span: Span::ZERO,
                    })));
                }
                FragmentChild::Comment(_) => {}
                _ => return None,
            }
        }
        quasis.push(pending);
        t::template_raw(quasis, exprs)
    };

    let escape_call = Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "escape"),
        arguments: vec![Argument::Expression(body_source)],
        optional: false,
        span: Span::ZERO,
    }));

    let idx = BODY_VAR_COUNTER.with(|c| {
        let i = c.get();
        c.set(i + 1);
        i
    });
    let body_var = if idx == 0 { "$$body".to_string() } else { format!("$$body_{idx}") };

    out.push(t::const_decl(&body_var, escape_call));
    out.push(Statement::If(Box::new(IfStatement {
        test: t::id(&body_var),
        consequent: Statement::Block(Box::new(BlockStatement {
            body: vec![t::stmt(t::call(
                t::member_id(t::id("$$renderer"), "push"),
                vec![t::template_raw(
                    vec![String::new(), String::new()],
                    vec![t::id(&body_var)],
                )],
            ))],
            span: Span::ZERO,
        })),
        alternate: Some(Statement::Block(Box::new(BlockStatement {
            body: Vec::new(),
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    })));
    // Close tag goes back into buf so adjacent content fuses.
    buf.push_str("</textarea>");
    Some(())
}

/// Lower `<textarea value={expr}>` or `<textarea>BODY</textarea>` to:
///   $$renderer.push(`<textarea${attrs}>`);
///   const $$body = $.escape(VALUE_OR_BODY);
///   if ($$body) { $$renderer.push(`${$$body}`); } else {}
///   $$renderer.push(`</textarea>`);
///
/// The `$$body` source comes from the `value=` (or `bind:value=`) attr
/// when present; otherwise it's a template literal of the body children.
fn lower_textarea_server(
    el: &svelte_ast::elements::RegularElement,
) -> Option<Vec<Statement>> {
    // Open tag + non-value attributes.
    let mut open_buf = TemplateBuf::new();
    open_buf.push_str("<textarea");
    let mut value_expr: Option<Expression> = None;
    for attr in &el.attributes {
        match attr {
            ElementAttribute::Attribute(a) if a.name == "value" => {
                value_expr = match &a.value {
                    AttributeValue::Single(t) => Some(t.expression.clone()),
                    AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
                        AttributeValuePart::ExpressionTag(t) => Some(t.expression.clone()),
                        AttributeValuePart::Text(t) => Some(string_lit(&t.data)),
                    },
                    _ => None,
                };
            }
            ElementAttribute::BindDirective(b) if b.name == "value" => {
                value_expr = Some(b.expression.clone());
            }
            _ => {
                append_element_attribute_server(attr, &mut open_buf)?;
            }
        }
    }
    open_buf.push_str(">");

    // Determine the $$body source.
    let body_source: Expression = if let Some(v) = value_expr {
        v
    } else {
        // Build a template literal of the textarea's children. Each text
        // node passes through verbatim; each ExpressionTag becomes
        // `${$.stringify(EXPR)}`. Per HTML5 textarea rules, strip a
        // leading newline from the first text node (browsers ignore it).
        let mut quasis: Vec<String> = Vec::new();
        let mut exprs: Vec<Expression> = Vec::new();
        let mut pending = String::new();
        let mut first_text = true;
        for child in &el.fragment.nodes {
            match child {
                FragmentChild::Text(t) => {
                    let data = if first_text {
                        first_text = false;
                        t.data.strip_prefix('\n').unwrap_or(&t.data)
                    } else {
                        &t.data
                    };
                    pending.push_str(data);
                }
                FragmentChild::ExpressionTag(tag) => {
                    first_text = false;
                    quasis.push(std::mem::take(&mut pending));
                    exprs.push(Expression::Call(Box::new(CallExpression {
                        callee: t::member_id(t::id("$"), "stringify"),
                        arguments: vec![Argument::Expression(tag.expression.clone())],
                        optional: false,
                        span: Span::ZERO,
                    })));
                }
                FragmentChild::Comment(_) => {}
                _ => return None,
            }
        }
        quasis.push(pending);
        t::template_raw(quasis, exprs)
    };

    let escape_call = Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "escape"),
        arguments: vec![Argument::Expression(body_source)],
        optional: false,
        span: Span::ZERO,
    }));

    let idx = BODY_VAR_COUNTER.with(|c| {
        let i = c.get();
        c.set(i + 1);
        i
    });
    let body_var = if idx == 0 { "$$body".to_string() } else { format!("$$body_{idx}") };

    let mut out: Vec<Statement> = Vec::new();
    // Open tag push.
    if let Some(s) = open_buf.flush() {
        out.push(s);
    }
    out.push(t::const_decl(&body_var, escape_call));
    let push_body = t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "push"),
        vec![t::template_raw(
            vec![String::new(), String::new()],
            vec![t::id(&body_var)],
        )],
    ));
    out.push(Statement::If(Box::new(IfStatement {
        test: t::id(&body_var),
        consequent: Statement::Block(Box::new(BlockStatement {
            body: vec![push_body],
            span: Span::ZERO,
        })),
        alternate: Some(Statement::Block(Box::new(BlockStatement {
            body: Vec::new(),
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    })));
    out.push(push_template("</textarea>"));
    Some(out)
}

/// Returns the value expression for `<select value=X>` (any of the three
/// shapes: static text, `{expr}`, `bind:value={x}`) or None when the
/// select has no value attribute.
fn select_value_attr(
    el: &svelte_ast::elements::RegularElement,
) -> Option<Expression> {
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) if attr.name == "value" => {
                return match &attr.value {
                    AttributeValue::Single(t) => Some(t.expression.clone()),
                    AttributeValue::Many(parts) if parts.len() == 1 => match &parts[0] {
                        AttributeValuePart::Text(t) => Some(Expression::Literal(Box::new(
                            Literal::String(StringLiteral {
                                value: t.data.clone(),
                                raw: Some(format!("'{}'", t.data.replace('\'', "\\'"))),
                                span: Span::ZERO,
                            }),
                        ))),
                        AttributeValuePart::ExpressionTag(t) => Some(t.expression.clone()),
                    },
                    _ => None,
                };
            }
            ElementAttribute::BindDirective(b) if b.name == "value" => {
                return Some(b.expression.clone());
            }
            _ => {}
        }
    }
    None
}

/// Lower `<select value=X ...>...options...</select>` to:
///   `$$renderer.select({ value: X }, ($$renderer) => { option_calls });`
/// Other (non-value, non-bind) attributes on the `<select>` flow into the
/// first-arg object too as additional properties.
fn lower_select_with_value(
    el: &svelte_ast::elements::RegularElement,
    value_expr: Expression,
) -> Option<Statement> {
    // Build the props object preserving the source order. The `value:`
    // entry takes the same position the value attribute occupied; other
    // attrs land at their original index.
    let mut props: Vec<ObjectMember> = Vec::new();
    let mut value_pushed = false;
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) if attr.name == "value" => {
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: "value".to_string(),
                        span: Span::ZERO,
                    }),
                    value: value_expr.clone(),
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
                value_pushed = true;
            }
            ElementAttribute::BindDirective(b) if b.name == "value" => {
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: "value".to_string(),
                        span: Span::ZERO,
                    }),
                    value: value_expr.clone(),
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
                value_pushed = true;
            }
            ElementAttribute::Attribute(attr) => {
                if let Some(p) = attribute_to_object_member(attr) {
                    props.push(p);
                }
            }
            _ => {}
        }
    }
    if !value_pushed {
        // Shouldn't happen — caller guarantees value attr exists. But
        // safeguard by pushing value at the end.
        props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: "value".to_string(),
                span: Span::ZERO,
            }),
            value: value_expr,
            kind: PropertyKind::Init,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
    }
    let props_obj = Expression::Object(Box::new(ObjectExpression {
        properties: props,
        span: Span::ZERO,
    }));

    // Build the children arrow body using the existing select-child lowerer.
    let mut inner_buf = TemplateBuf::new();
    let mut inner_out: Vec<Statement> = Vec::new();
    let children = trim_boundary_whitespace(&el.fragment.nodes);
    SELECT_EACH_COUNTER.with(|c| {
        for child in children {
            let _ = lower_select_child(child, &mut inner_buf, &mut inner_out, c);
        }
    });
    if let Some(stmt) = inner_buf.flush() {
        inner_out.push(stmt);
    }

    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$renderer")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: inner_out,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    let mut select_args = vec![props_obj, arrow];
    // The 3rd arg is the `svelte-{hash}` only when the element itself
    // carries a `class=` attribute that needs scoping. Otherwise upstream
    // omits the arg.
    let has_class_attr = el.attributes.iter().any(|a| matches!(
        a, ElementAttribute::Attribute(attr) if attr.name == "class"
    ));
    if has_class_attr {
        if let Some(hash) = CSS_HASH.with(|c| c.borrow().clone()) {
            select_args.push(string_lit(&hash));
        }
    }
    Some(t::stmt(t::call(
        t::member_id(t::id("$$renderer"), "select"),
        select_args,
    )))
}

/// Inline emit `<select>` with `<option>` children — writes the open tag
/// into `buf`, interleaves option calls into `out` (flushing buf each
/// time), and finishes by writing the close tag into `buf`.
fn emit_select_inline(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
) -> Option<()> {
    SELECT_EACH_COUNTER.with(|c| emit_select_inline_with(el, buf, out, c))
}

fn emit_select_inline_with(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
    each_counter: &std::cell::Cell<usize>,
) -> Option<()> {
    buf.push_str("<");
    buf.push_str(&el.name);
    for attr in &el.attributes {
        append_element_attribute_server(attr, buf)?;
    }
    buf.push_str(">");
    let children = trim_boundary_whitespace(&el.fragment.nodes);
    for c in children {
        lower_select_child(c, buf, out, each_counter)?;
    }
    buf.push_str("</");
    buf.push_str(&el.name);
    buf.push_str(">");
    Some(())
}

/// Lower a single child of a `<select>` / `<optgroup>` / `<svelte:boundary>`
/// container. Knows that `<option>` becomes a `$$renderer.option(...)` call,
/// blocks (`{#each}`, `{#if}`, `{#key}`) recursively lower their bodies
/// "inside select context", Components / snippets / @html / @render get
/// their out-of-band emission with anchor markers.
fn lower_select_child(
    c: &FragmentChild,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
    each_counter: &std::cell::Cell<usize>,
) -> Option<()> {
    match c {
        FragmentChild::RegularElement(child) if child.name == "option" => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(lower_option_server(child)?);
        }
        FragmentChild::RegularElement(child) if child.name == "optgroup" => {
            // Inline open tag + recursive lowering + close tag.
            buf.push_str("<optgroup");
            for attr in &child.attributes {
                append_element_attribute_server(attr, buf)?;
            }
            buf.push_str(">");
            let inner_children = trim_boundary_whitespace(&child.fragment.nodes);
            for c2 in inner_children {
                lower_select_child(c2, buf, out, each_counter)?;
            }
            buf.push_str("</optgroup>");
        }
        FragmentChild::EachBlock(eb) => {
            // Fuse `<!--[-->` open marker with the preceding buf, then emit
            // the for-loop, then push the close marker into buf so it fuses
            // with the next chunk.
            buf.push_str("<!--[-->");
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            let idx = each_counter.get();
            each_counter.set(idx + 1);
            out.extend(lower_each_for_select(eb, idx, each_counter)?);
            buf.push_str("<!--]-->");
            // Trailing `<!>` anchor when the body contains rich content
            // (Component/RenderTag/HtmlTag).
            if each_body_has_rich_content(&eb.body) {
                buf.push_str("<!>");
            }
        }
        FragmentChild::IfBlock(ib) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(build_if_chain_for_select(ib, each_counter)?);
            buf.push_str("<!--]-->");
            // Trailing `<!>` anchor when any branch contains rich content.
            let rich = each_body_has_rich_content(&ib.consequent)
                || ib.alternate.as_ref().map_or(false, each_body_has_rich_content);
            if rich {
                buf.push_str("<!>");
            }
        }
        FragmentChild::KeyBlock(kb) => {
            // Fuse `<!---->` open marker with the preceding buf.
            buf.push_str("<!---->");
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            let mut inner_body: Vec<Statement> = Vec::new();
            let mut inner_buf = TemplateBuf::new();
            let inner_children = trim_boundary_whitespace(&kb.fragment.nodes);
            for c2 in inner_children {
                lower_select_child(c2, &mut inner_buf, &mut inner_body, each_counter)?;
            }
            if let Some(stmt) = inner_buf.flush() {
                inner_body.push(stmt);
            }
            out.push(Statement::Block(Box::new(BlockStatement {
                body: inner_body,
                span: Span::ZERO,
            })));
            buf.push_str("<!---->");
        }
        FragmentChild::SvelteBoundary(b) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(push_template("<!--[-->"));
            let mut inner_body: Vec<Statement> = Vec::new();
            let mut inner_buf = TemplateBuf::new();
            let inner_children = trim_boundary_whitespace(&b.fragment.nodes);
            for c2 in inner_children {
                lower_select_child(c2, &mut inner_buf, &mut inner_body, each_counter)?;
            }
            if let Some(stmt) = inner_buf.flush() {
                inner_body.push(stmt);
            }
            out.push(Statement::Block(Box::new(BlockStatement {
                body: inner_body,
                span: Span::ZERO,
            })));
            out.push(push_template("<!--]-->"));
        }
        FragmentChild::Component(comp) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(lower_component_server(comp)?);
            // Anchor marker pair after Component: `<!---->` then `<!>` (hydration).
            buf.push_str("<!---->");
            buf.push_str("<!>");
        }
        FragmentChild::RenderTag(rt) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            // `{@render snippet()}` → just call `snippet($$renderer)`.
            out.push(lower_render_tag_for_select(rt)?);
            buf.push_str("<!---->");
            buf.push_str("<!>");
        }
        FragmentChild::HtmlTag(t) => {
            // `{@html EXPR}` → `${$.html(EXPR)}` interpolation in buf.
            buf.push_expr(t::call(
                t::member_id(t::id("$"), "html"),
                vec![t.expression.clone()],
            ));
            // `@html` inside a select also emits a hydration anchor.
            buf.push_str("<!>");
        }
        FragmentChild::ConstTag(_) => {
            // ConstTag inside select fragment is lowered by the enclosing
            // each/if body, not here directly. Handled inside lower_each_for_select.
        }
        FragmentChild::Comment(_) | FragmentChild::SnippetBlock(_) => {
            // Comments dropped server-side. Top-level snippet blocks were already
            // extracted before this point.
        }
        // Whitespace-only Text between `<option>`/`<optgroup>` siblings is
        // dropped — each option emits its own self-anchored call, so there's
        // no need to preserve the inter-element whitespace.
        FragmentChild::Text(t) if t.data.trim().is_empty() => {}
        other => {
            if append_node_to_template(other, buf).is_none() {
                return None;
            }
        }
    }
    Some(())
}

/// Inside an each-block's for-loop body (within a `<select>`), Components and
/// RenderTags don't emit per-iteration anchors — those go on the trailing
/// `<!--]-->`/`<!>` push after the loop instead.
fn lower_select_child_loop_body(
    c: &FragmentChild,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
    each_counter: &std::cell::Cell<usize>,
) -> Option<()> {
    match c {
        FragmentChild::Component(comp) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(lower_component_server(comp)?);
        }
        FragmentChild::RenderTag(rt) => {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            out.push(lower_render_tag_for_select(rt)?);
        }
        FragmentChild::HtmlTag(t) => {
            buf.push_expr(t::call(
                t::member_id(t::id("$"), "html"),
                vec![t.expression.clone()],
            ));
        }
        _ => return lower_select_child(c, buf, out, each_counter),
    }
    Some(())
}

/// True if any descendant of the fragment is a Component, RenderTag, or
/// HtmlTag — meaning the each/if-block's close marker needs a trailing `<!>`.
fn each_body_has_rich_content(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| match n {
        FragmentChild::Component(_)
        | FragmentChild::RenderTag(_)
        | FragmentChild::HtmlTag(_) => true,
        FragmentChild::IfBlock(ib) => {
            each_body_has_rich_content(&ib.consequent)
                || ib.alternate.as_ref().map_or(false, each_body_has_rich_content)
        }
        FragmentChild::EachBlock(eb) => each_body_has_rich_content(&eb.body),
        FragmentChild::KeyBlock(kb) => each_body_has_rich_content(&kb.fragment),
        FragmentChild::AwaitBlock(ab) => {
            ab.pending.as_ref().map_or(false, each_body_has_rich_content)
                || ab.then.as_ref().map_or(false, each_body_has_rich_content)
                || ab.catch_.as_ref().map_or(false, each_body_has_rich_content)
        }
        FragmentChild::SvelteBoundary(b) => each_body_has_rich_content(&b.fragment),
        _ => false,
    })
}

/// `{@render fn(args)}` → `fn($$renderer, ...args)`.
fn lower_render_tag_for_select(rt: &svelte_ast::tags::RenderTag) -> Option<Statement> {
    let (callee, args) = match &rt.expression {
        Expression::Call(c) => (c.callee.clone(), c.arguments.clone()),
        _ => return None,
    };
    let mut arguments = vec![Argument::Expression(t::id("$$renderer"))];
    arguments.extend(args);
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee,
        arguments,
        optional: false,
        span: Span::ZERO,
    }))))
}

/// Each-block whose body lowers to `lower_select_child` (for use inside `<select>`).
/// `idx == 0` uses plain names (`each_array`, `$$index`, `$$length`); higher
/// indices append `_N` to dedupe across sibling each-blocks.
fn lower_each_for_select(
    eb: &svelte_ast::blocks::EachBlock,
    idx: usize,
    each_counter: &std::cell::Cell<usize>,
) -> Option<Vec<Statement>> {
    let arr_name = if idx == 0 {
        "each_array".to_string()
    } else {
        format!("each_array_{idx}")
    };
    let index_name = eb.index.clone().unwrap_or_else(|| {
        if idx == 0 {
            "$$index".to_string()
        } else {
            format!("$$index_{idx}")
        }
    });

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
                    object: t::id(&arr_name),
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
    let mut body_stmts: Vec<Statement> = Vec::new();
    if let Some(ctx) = &eb.context {
        body_stmts.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: ctx.clone(),
                init: Some(Expression::Member(Box::new(MemberExpression {
                    object: t::id(&arr_name),
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
    // Emit ConstTag declarations BEFORE the rest of the body (mirrors
    // upstream's ConstTag visitor in non-async context: `const X = INIT;`).
    for c in &eb.body.nodes {
        if let FragmentChild::ConstTag(ct) = c {
            body_stmts.push(Statement::Variable(Box::new(VariableDeclaration {
                kind: VariableKind::Const,
                declarations: ct.declaration.declarations.clone(),
                span: Span::ZERO,
            })));
        }
    }
    // Lower each-block body in select context. Components/RenderTags inside
    // a for-loop body don't emit per-iteration `<!---->`/`<!>` anchors — the
    // single trailing anchor goes AFTER the for-loop close marker (computed
    // by the caller from `each_body_has_rich_content`).
    let mut inner_buf = TemplateBuf::new();
    for c in &eb.body.nodes {
        if matches!(c, FragmentChild::ConstTag(_)) {
            continue; // already emitted above
        }
        lower_select_child_loop_body(c, &mut inner_buf, &mut body_stmts, each_counter)?;
    }
    if let Some(stmt) = inner_buf.flush() {
        body_stmts.push(stmt);
    }

    let for_stmt = Statement::For(Box::new(ForStatement {
        init: Some(ForInit::Declaration(Box::new(
            match init {
                Statement::Variable(v) => *v,
                _ => unreachable!(),
            },
        ))),
        test: Some(test),
        update: Some(update),
        body: Statement::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        span: Span::ZERO,
    }));

    let arr_decl = t::const_decl(
        &arr_name,
        t::call(
            t::member_id(t::id("$"), "ensure_array_like"),
            vec![eb.expression.clone()],
        ),
    );
    Some(vec![arr_decl, for_stmt])
}

/// If-block whose branches lower via `lower_select_child` for the `<select>`
/// context. Simpler than the async version — no blockers, no async wrap.
fn build_if_chain_for_select(
    ib: &svelte_ast::blocks::IfBlock,
    each_counter: &std::cell::Cell<usize>,
) -> Option<Statement> {
    let consequent_marker = "<!--[0-->";
    let mut consequent_body: Vec<Statement> = vec![push_string(consequent_marker)];
    {
        let mut inner_buf = TemplateBuf::new();
        let children = trim_boundary_whitespace(&ib.consequent.nodes);
        let children = trim_boundary_text(children);
        for c in children.iter() {
            lower_select_child_loop_body(c, &mut inner_buf, &mut consequent_body, each_counter)?;
        }
        if let Some(stmt) = inner_buf.flush() {
            consequent_body.push(stmt);
        }
    }
    let mut alternate_body: Vec<Statement> = vec![push_string("<!--[-1-->")];
    if let Some(alt) = &ib.alternate {
        let mut inner_buf = TemplateBuf::new();
        let children = trim_boundary_whitespace(&alt.nodes);
        let children = trim_boundary_text(children);
        for c in children.iter() {
            lower_select_child_loop_body(c, &mut inner_buf, &mut alternate_body, each_counter)?;
        }
        if let Some(stmt) = inner_buf.flush() {
            alternate_body.push(stmt);
        }
    }
    Some(Statement::If(Box::new(IfStatement {
        test: ib.test.clone(),
        consequent: Statement::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        alternate: Some(Statement::Block(Box::new(BlockStatement {
            body: alternate_body,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    })))
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
    let if_stmt = build_if_chain_server(ib, 0, test_is_async)?;
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

/// Recursively build an if/else-if chain. `branch_idx` is the current
/// branch number, used in the `<!--[N-->` marker; final else uses `-1`.
///
/// When `async_ctx` is provided, elseifs whose test has `await` (or that
/// would introduce a new blocker not in this block's set) BREAK OUT of the
/// chain — the final `else` body emits a `child_block` / `async_block`
/// wrapping a fresh if-chain starting at the broken-out elseif. Mirrors
/// upstream's IfBlock.js + analyze visitor's flattened/non-flattened split.
fn build_if_chain_server(
    ib: &svelte_ast::blocks::IfBlock,
    branch_idx: i32,
    use_async_marker: bool,
) -> Option<Statement> {
    build_if_chain_server_ex(ib, branch_idx, use_async_marker, None)
}

fn build_if_chain_server_ex(
    ib: &svelte_ast::blocks::IfBlock,
    branch_idx: i32,
    use_async_marker: bool,
    async_ctx: Option<&AsyncCtx>,
) -> Option<Statement> {
    let test_is_async = expr_has_await_top(&ib.test);
    let empty_blockers: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let blocker_bindings = async_ctx
        .map(|c| c.blocker_bindings)
        .unwrap_or(&empty_blockers);
    let consequent_has_const_await =
        fragment_has_const_with_await_or_blocker(&ib.consequent, blocker_bindings);
    let use_async_marker = use_async_marker || consequent_has_const_await;

    let consequent_marker = format!("<!--[{branch_idx}-->");
    let mut consequent_body: Vec<Statement> = Vec::new();
    // Upstream always emits the if-branch marker as a plain string literal
    // (`b.literal('<!--[0-->')`) — both inside async wraps and out.
    consequent_body.push(push_string(&consequent_marker));
    let _ = use_async_marker;
    if test_is_async {
        consequent_body.extend(lower_fragment_for_async_block(&ib.consequent)?);
    } else if consequent_has_const_await {
        // Pick a unique `promises`/`promises_1`/... name from the sibling
        // counter on AsyncCtx (or default to "promises" when there's no ctx).
        let idx = async_ctx
            .map(|c| {
                let i = c.const_await_counter.get();
                c.const_await_counter.set(i + 1);
                i
            })
            .unwrap_or(0);
        let promises_var = if idx == 0 {
            "promises".to_string()
        } else {
            format!("promises_{idx}")
        };
        consequent_body.extend(lower_fragment_with_const_await_with(
            &ib.consequent,
            blocker_bindings,
            &promises_var,
        )?);
    } else {
        // Inside an async_block / child_block (use_async_marker=true), the
        // wrap's string-literal `<!--[N-->` push already anchors the scope —
        // do NOT prepend `<!---->` to the body push template.
        let needs_marker = !use_async_marker && body_needs_marker(&ib.consequent);
        consequent_body.extend(lower_fragment_with_marker(&ib.consequent, needs_marker)?);
    }

    // Determine alternate: if alternate fragment is exactly `[IfBlock with
    // elseif=true]`, recurse to build `else if (...)` chain directly.
    let alternate_stmt: Option<Statement> = if let Some(alt) = &ib.alternate {
        let non_ws: Vec<&FragmentChild> = alt
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                _ => true,
            })
            .collect();
        // Detect a break-out: an elseif whose test has `await`, or whose own
        // blocker set introduces an index not in `parent_blockers`. In both
        // cases the chain stops here and a nested wrap takes over.
        let mut break_out_target: Option<&svelte_ast::blocks::IfBlock> = None;
        if non_ws.len() == 1 {
            if let FragmentChild::IfBlock(inner) = non_ws[0] {
                if inner.elseif {
                    let inner_test_async = expr_has_await_top(&inner.test);
                    let mut needs_break = inner_test_async;
                    if let Some(ctx) = &async_ctx {
                        let mut inner_blockers = std::collections::BTreeSet::new();
                        collect_block_indices_in_expr(
                            &inner.test,
                            ctx.blocker_bindings,
                            &mut inner_blockers,
                        );
                        if inner_blockers.iter().any(|i| !ctx.parent_blockers.contains(i)) {
                            needs_break = true;
                        }
                    }
                    if !needs_break {
                        // Flatten — recurse to extend the chain.
                        let chain = build_if_chain_server_ex(
                            inner,
                            branch_idx + 1,
                            use_async_marker,
                            async_ctx,
                        )?;
                        return Some(finalize_if(test_is_async, &ib.test, consequent_body, Some(chain)));
                    }
                    break_out_target = Some(inner);
                }
            }
        }

        if let Some(inner) = break_out_target {
            // Build the final-else body with the broken-out wrap.
            let final_marker = "<!--[-1-->";
            let mut alternate_body: Vec<Statement> = Vec::new();
            alternate_body.push(push_string(final_marker));
            // Compute inner blockers (the new chain's blocker set).
            let mut inner_blockers: std::collections::BTreeSet<usize> =
                std::collections::BTreeSet::new();
            if let Some(ctx) = &async_ctx {
                collect_if_block_indices(inner, ctx.blocker_bindings, &mut inner_blockers);
            }
            let inner_test_async = expr_has_await_top(&inner.test);
            // Build the inner if-chain with fresh branch index starting at 0.
            let inner_async_ctx = async_ctx.map(|ctx| AsyncCtx {
                promises_var: ctx.promises_var,
                blocker_bindings: ctx.blocker_bindings,
                parent_blockers: inner_blockers.clone(),
                const_await_counter: std::cell::Cell::new(ctx.const_await_counter.get()),
            });
            let inner_chain =
                build_if_chain_server_ex(inner, 0, true, inner_async_ctx.as_ref())?;
            let indices_vec: Vec<usize> = inner_blockers.into_iter().collect();
            if !indices_vec.is_empty() {
                if let Some(ctx) = &async_ctx {
                    alternate_body.push(wrap_async_block(
                        vec![inner_chain],
                        ctx.promises_var,
                        &indices_vec,
                        inner_test_async,
                    ));
                } else {
                    alternate_body.push(wrap_child_block(vec![inner_chain]));
                }
            } else {
                alternate_body.push(wrap_child_block(vec![inner_chain]));
            }
            alternate_body.push(push_template("<!--]-->"));
            Some(Statement::Block(Box::new(BlockStatement {
                body: alternate_body,
                span: Span::ZERO,
            })))
        } else {
            // Final `else` branch.
            let final_marker = "<!--[-1-->";
            let mut alternate_body: Vec<Statement> = Vec::new();
            alternate_body.push(push_string(final_marker));
            if test_is_async {
                alternate_body.extend(lower_fragment_for_async_block(alt)?);
            } else {
                let needs_marker = !use_async_marker && body_needs_marker(alt);
                alternate_body.extend(lower_fragment_with_marker(alt, needs_marker)?);
            }
            Some(Statement::Block(Box::new(BlockStatement {
                body: alternate_body,
                span: Span::ZERO,
            })))
        }
    } else {
        // No alternate at all. Still emit the final-else block with just the
        // `<!--[-1-->` marker so the close marker has a partner.
        let final_marker = "<!--[-1-->";
        let alternate_body = vec![push_string(final_marker)];
        Some(Statement::Block(Box::new(BlockStatement {
            body: alternate_body,
            span: Span::ZERO,
        })))
    };

    let test = if test_is_async {
        wrap_async_test(&ib.test)
    } else {
        ib.test.clone()
    };

    Some(Statement::If(Box::new(IfStatement {
        test,
        consequent: Statement::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        alternate: alternate_stmt,
        span: Span::ZERO,
    })))
}

/// `EXPR` → `(await $.save(EXPR))()`. Used when an async block's test
/// needs blocker tracking.
fn wrap_async_test(test: &Expression) -> Expression {
    // Recursively replace each `await X` sub-expression with `(await $.save(X))()`.
    // This mirrors upstream's PromiseOptimiser which only rewrites the
    // AwaitExpression itself, leaving surrounding binary/logical/etc. structure
    // intact (e.g. `await foo > 10` → `(await $.save(foo))() > 10`, and
    // `foo(await 1)` → `foo((await $.save(1))())`).
    fn rewrite(e: &Expression) -> Expression {
        match e {
            Expression::Await(a) => {
                let inner = rewrite(&a.argument);
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
            Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
                operator: b.operator,
                left: rewrite(&b.left),
                right: rewrite(&b.right),
                span: b.span,
            })),
            Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
                operator: l.operator,
                left: rewrite(&l.left),
                right: rewrite(&l.right),
                span: l.span,
            })),
            Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
                operator: u.operator,
                argument: rewrite(&u.argument),
                prefix: u.prefix,
                span: u.span,
            })),
            Expression::Conditional(c) => Expression::Conditional(Box::new(ConditionalExpression {
                test: rewrite(&c.test),
                consequent: rewrite(&c.consequent),
                alternate: rewrite(&c.alternate),
                span: c.span,
            })),
            Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
                expression: rewrite(&p.expression),
                span: p.span,
            })),
            Expression::Sequence(s) => Expression::Sequence(Box::new(SequenceExpression {
                expressions: s.expressions.iter().map(rewrite).collect(),
                span: s.span,
            })),
            Expression::Call(c) => Expression::Call(Box::new(CallExpression {
                callee: rewrite(&c.callee),
                arguments: c
                    .arguments
                    .iter()
                    .map(|a| match a {
                        svelte_js_ast::Argument::Expression(e) => {
                            svelte_js_ast::Argument::Expression(rewrite(e))
                        }
                        svelte_js_ast::Argument::Spread(s) => {
                            svelte_js_ast::Argument::Spread(Box::new(SpreadElement {
                                argument: rewrite(&s.argument),
                                span: s.span,
                            }))
                        }
                    })
                    .collect(),
                optional: c.optional,
                span: c.span,
            })),
            Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
                object: rewrite(&m.object),
                property: m.property.clone(),
                computed: m.computed,
                optional: m.optional,
                span: m.span,
            })),
            e => e.clone(),
        }
    }
    rewrite(test)
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
/// `<select>` containers (and `<optgroup>`/`<svelte:boundary>` inside one)
/// need the customizable-select-element call shape. Currently this returns
/// true for any `<select>` element (matching upstream's
/// `is_option_special`-style decision applied per-`<option>`, but routed at
/// the `<select>` level so the recursive lowering reaches every option).
fn has_option_child(el: &svelte_ast::elements::RegularElement) -> bool {
    el.name == "select"
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
    // Decide the body shape:
    //  - If `<option>` has rich content (any RegularElement descendant), emit
    //    the 7-arg form `option({attrs}, body_fn, void 0, void 0, void 0, void 0, true)`
    //  - If `<option>` contains exactly one ExpressionTag and nothing else, emit
    //    the 2-arg form `option({attrs}, EXPR)` — value passed directly.
    //  - Else emit `option({attrs}, ($$renderer) => { ... body })` (2-arg arrow).
    let is_rich = is_customizable_option(el);
    let non_ws: Vec<&FragmentChild> = el
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    let single_expr_value: Option<Expression> = if !is_rich
        && non_ws.len() == 1
    {
        if let FragmentChild::ExpressionTag(t) = non_ws[0] {
            Some(t.expression.clone())
        } else {
            None
        }
    } else {
        None
    };

    let body_arg = if let Some(expr) = single_expr_value {
        expr
    } else {
        let body_stmts = lower_fragment_with_marker(&el.fragment, false)?;
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$renderer")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: body_stmts,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }))
    };
    let mut arguments = vec![
        Argument::Expression(Expression::Object(Box::new(ObjectExpression {
            properties: props,
            span: Span::ZERO,
        }))),
        Argument::Expression(body_arg),
    ];
    // The 3rd arg is the `svelte-{hash}` only when the `<option>` itself
    // carries a `class=` attribute (or rich content forces the 7-arg form
    // and we need to fill slot 3). Otherwise upstream omits it.
    let has_class_attr = el.attributes.iter().any(|a| matches!(
        a, ElementAttribute::Attribute(attr) if attr.name == "class"
    ));
    let css_hash = if has_class_attr {
        CSS_HASH.with(|c| c.borrow().clone())
    } else {
        None
    };
    if let Some(hash) = css_hash.as_ref() {
        arguments.push(Argument::Expression(string_lit(hash)));
    }
    if is_rich {
        // 7-arg form: pad slots 4-6 with `void 0` (slot 3 is css_hash,
        // already filled above when scoped) and pass `true` as slot 7.
        if css_hash.is_none() {
            arguments.push(Argument::Expression(void_zero_expr()));
        }
        for _ in 0..3 {
            arguments.push(Argument::Expression(void_zero_expr()));
        }
        arguments.push(Argument::Expression(Expression::Literal(Box::new(
            Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }),
        ))));
    }
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$$renderer"), "option"),
        arguments,
        optional: false,
        span: Span::ZERO,
    }))))
}

fn void_zero_expr() -> Expression {
    Expression::Unary(Box::new(UnaryExpression {
        operator: UnaryOperator::Void,
        argument: Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: 0.0,
            raw: Some("0".to_string()),
            span: Span::ZERO,
        }))),
        prefix: true,
        span: Span::ZERO,
    }))
}

/// Mirrors upstream's `is_customizable_select_element` for the `<option>` case:
/// returns true if any descendant of the option's fragment is a RegularElement
/// (rich content like `<span>`/`<em>`), an HtmlTag (`{@html ...}`), a
/// Component, a RenderTag (`{@render ...}`), etc.
fn is_customizable_option(el: &svelte_ast::elements::RegularElement) -> bool {
    fn check(n: &FragmentChild) -> bool {
        match n {
            // Stop at: snippet/const/comment/expression/text/debug.
            FragmentChild::Text(_)
            | FragmentChild::Comment(_)
            | FragmentChild::ExpressionTag(_)
            | FragmentChild::ConstTag(_)
            | FragmentChild::SnippetBlock(_)
            | FragmentChild::DebugTag(_) => false,
            FragmentChild::RegularElement(_) => true,
            FragmentChild::IfBlock(ib) => {
                ib.consequent.nodes.iter().any(check)
                    || ib.alternate.as_ref().map_or(false, |a| a.nodes.iter().any(check))
            }
            FragmentChild::EachBlock(eb) => {
                eb.body.nodes.iter().any(check)
                    || eb.fallback.as_ref().map_or(false, |a| a.nodes.iter().any(check))
            }
            FragmentChild::KeyBlock(kb) => kb.fragment.nodes.iter().any(check),
            FragmentChild::AwaitBlock(ab) => {
                ab.pending.as_ref().map_or(false, |a| a.nodes.iter().any(check))
                    || ab.then.as_ref().map_or(false, |a| a.nodes.iter().any(check))
                    || ab.catch_.as_ref().map_or(false, |a| a.nodes.iter().any(check))
            }
            FragmentChild::SvelteBoundary(b) => b.fragment.nodes.iter().any(check),
            // Components, render tags, @html, svelte:element, svelte:component all count as rich.
            _ => true,
        }
    }
    el.fragment.nodes.iter().any(check)
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
/// Does this element contain any descendant that can't be inlined into a
/// single template-literal push? Components, blocks, await-tags etc all
/// require their own statement.
fn element_contains_non_inline(el: &svelte_ast::elements::RegularElement) -> bool {
    fn node_is_non_inline(n: &FragmentChild) -> bool {
        match n {
            FragmentChild::Component(_)
            | FragmentChild::SvelteElement(_)
            | FragmentChild::EachBlock(_)
            | FragmentChild::IfBlock(_)
            | FragmentChild::AwaitBlock(_)
            | FragmentChild::KeyBlock(_)
            | FragmentChild::RenderTag(_)
            | FragmentChild::SvelteHead(_)
            | FragmentChild::SvelteBoundary(_) => true,
            FragmentChild::RegularElement(child) => {
                child.fragment.nodes.iter().any(node_is_non_inline)
            }
            _ => false,
        }
    }
    el.fragment.nodes.iter().any(node_is_non_inline)
}

/// Lower a RegularElement whose interior contains at least one
/// non-inlineable child. Writes the open tag into `buf`, then walks each
/// child: inlineable ones flow into `buf`, non-inlineable ones flush `buf`
/// and emit their own statement (the same dispatch as the top-level loop).
/// Finishes by writing the close tag.
fn lower_element_with_non_inline_children(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
    out: &mut Vec<Statement>,
) -> Option<()> {
    // Open tag (with attributes).
    buf.push_str("<");
    buf.push_str(&el.name);
    for attr in &el.attributes {
        append_element_attribute_server(attr, buf)?;
    }
    if is_void(&el.name) {
        buf.push_str("/>");
        return Some(());
    }
    buf.push_str(">");

    let children = trim_boundary_whitespace(&el.fragment.nodes);
    let children = trim_boundary_text(&children);
    let mut last_was_component = false;
    for n in children.iter() {
        if last_was_component {
            if !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()) {
                buf.push_str("<!---->");
                last_was_component = false;
            }
        }
        if append_node_to_template(n, buf).is_none() {
            if let Some(stmt) = buf.flush() {
                out.push(stmt);
            }
            match n {
                FragmentChild::Component(c) => {
                    out.push(lower_component_server(c)?);
                    last_was_component = true;
                }
                FragmentChild::SvelteElement(child) => {
                    out.push(lower_svelte_element_server(child)?);
                }
                FragmentChild::EachBlock(eb) => {
                    out.extend(lower_each_block_server(eb)?);
                }
                FragmentChild::AwaitBlock(ab) => {
                    out.extend(lower_await_block_server(ab)?);
                    buf.push_str("<!--]-->");
                }
                FragmentChild::IfBlock(ib) => {
                    out.extend(lower_if_block_server(ib)?);
                    buf.push_str("<!--]-->");
                }
                FragmentChild::RegularElement(child) if element_contains_non_inline(child) => {
                    lower_element_with_non_inline_children(child, buf, out)?;
                }
                _ => return None,
            }
        } else {
            last_was_component = false;
        }
    }
    if last_was_component {
        buf.push_str("<!---->");
    }
    buf.push_str("</");
    buf.push_str(&el.name);
    buf.push_str(">");
    Some(())
}

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
            // If any attribute is a spread, emit the whole attribute set
            // via `${$.attributes({ ...spread1, name: val, ...spread2 })}`
            // — a single interpolation. Otherwise emit each attribute
            // inline.
            let has_spread = el.attributes.iter().any(|a| {
                matches!(a, ElementAttribute::SpreadAttribute(_))
            });
            if has_spread {
                append_attributes_call(el, buf);
                // XSS event-capture: load/error elements with spread attrs
                // get ` onload="this.__e=event" onerror="this.__e=event"`
                // appended so the runtime can capture events that may have
                // been overridden by malicious spread content. Mirrors
                // upstream's is_load_error_element list.
                if is_load_error_element(&el.name) {
                    buf.push_str(
                        " onload=\"this.__e=event\" onerror=\"this.__e=event\""
                    );
                }
            } else {
                // `<input type="file" bind:value={...}>` drops the bind in
                // SSR — the runtime computes the value attribute from
                // currentTime/etc., never re-rendered here.
                let is_file_input = el.name == "input"
                    && el.attributes.iter().any(|a| matches!(
                        a, ElementAttribute::Attribute(attr)
                        if attr.name == "type" && attr_value_is_text(&attr.value, "file")
                    ));
                // `<select bind:value={x}>` drops the bind too — the select
                // lowerer handles it via the `{ value: ... }` first arg.
                let is_select_value_bind = el.name == "select";
                // `<input type="checkbox|radio" bind:group={X}>` synthesizes
                // `checked={X === value}` (radio) or `checked={X.includes(value)}` (checkbox).
                let group_bind: Option<&svelte_ast::attributes::BindDirective> = el
                    .attributes
                    .iter()
                    .find_map(|a| match a {
                        ElementAttribute::BindDirective(b) if b.name == "group" => Some(b),
                        _ => None,
                    });
                let value_expr: Option<Expression> = if group_bind.is_some() {
                    el.attributes.iter().find_map(|a| match a {
                        ElementAttribute::Attribute(attr) if attr.name == "value" => {
                            match &attr.value {
                                AttributeValue::Single(t) => Some(t.expression.clone()),
                                AttributeValue::Many(parts) if parts.len() == 1 => {
                                    match &parts[0] {
                                        AttributeValuePart::Text(t) => Some(Expression::Literal(
                                            Box::new(Literal::String(StringLiteral {
                                                value: t.data.clone(),
                                                raw: Some(format!(
                                                    "'{}'",
                                                    t.data.replace('\'', "\\'")
                                                )),
                                                span: Span::ZERO,
                                            })),
                                        )),
                                        AttributeValuePart::ExpressionTag(e) => {
                                            Some(e.expression.clone())
                                        }
                                    }
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    })
                } else {
                    None
                };
                let is_checkbox = el.attributes.iter().any(|a| {
                    matches!(a, ElementAttribute::Attribute(attr)
                        if attr.name == "type" && attr_value_is_text(&attr.value, "checkbox"))
                });
                for attr in &el.attributes {
                    if let ElementAttribute::BindDirective(b) = attr {
                        if b.name == "value" && (is_file_input || is_select_value_bind) {
                            continue;
                        }
                        if b.name == "group" {
                            // Emit synthesized `checked={...}` AT THIS
                            // POSITION (the bind:group position), so the
                            // surrounding attribute order matches upstream.
                            if let Some(value) = value_expr.clone() {
                                let checked_expr = if is_checkbox {
                                    Expression::Call(Box::new(CallExpression {
                                        callee: Expression::Member(Box::new(MemberExpression {
                                            object: b.expression.clone(),
                                            property: MemberProperty::Identifier(Identifier {
                                                name: "includes".to_string(),
                                                span: Span::ZERO,
                                            }),
                                            computed: false,
                                            optional: false,
                                            span: Span::ZERO,
                                        })),
                                        arguments: vec![Argument::Expression(value)],
                                        optional: false,
                                        span: Span::ZERO,
                                    }))
                                } else {
                                    Expression::Binary(Box::new(BinaryExpression {
                                        operator: BinaryOperator::StrictEq,
                                        left: b.expression.clone(),
                                        right: value,
                                        span: Span::ZERO,
                                    }))
                                };
                                buf.push_expr(Expression::Call(Box::new(CallExpression {
                                    callee: t::member_id(t::id("$"), "attr"),
                                    arguments: vec![
                                        Argument::Expression(string_lit("checked")),
                                        Argument::Expression(checked_expr),
                                        Argument::Expression(Expression::Literal(Box::new(
                                            Literal::Boolean(BooleanLiteral {
                                                value: true,
                                                span: Span::ZERO,
                                            }),
                                        ))),
                                    ],
                                    optional: false,
                                    span: Span::ZERO,
                                })));
                            }
                            continue;
                        }
                    }
                    append_element_attribute_server(attr, buf)?;
                }
            }
            if is_void(&el.name) {
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
        FragmentChild::Comment(c) => {
            // HTML comments are dropped server-side by default. With
            // `preserveComments: true`, emit them verbatim.
            if PRESERVE_COMMENTS.with(|p| p.get()) {
                buf.push_str("<!--");
                buf.push_str(&escape_text(&c.data));
                buf.push_str("-->");
            }
            Some(())
        }
        FragmentChild::TitleElement(el) => {
            // Outside `<svelte:head>`, `<title>` is just an HTML element.
            // (Inside head, `lower_head_fragment` handles it specially.)
            buf.push_str("<title>");
            let kids = trim_boundary_whitespace(&el.fragment.nodes);
            let kids = trim_boundary_text(kids);
            for k in kids.iter() {
                append_node_to_template(k, buf)?;
            }
            buf.push_str("</title>");
            Some(())
        }
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
/// Build `${$.attributes({ ...spread, name: val, ... })}` for an element
/// that has at least one spread attribute. Combines all attributes
/// (statics + spreads + bind:) into a single object expression.
fn append_attributes_call(
    el: &svelte_ast::elements::RegularElement,
    buf: &mut TemplateBuf,
) {
    let mut members: Vec<ObjectMember> = Vec::new();
    for attr in &el.attributes {
        match attr {
            ElementAttribute::SpreadAttribute(s) => {
                members.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            ElementAttribute::Attribute(a) => {
                if is_event_handler_name(&a.name) {
                    continue;
                }
                if let Some(prop) = attribute_to_object_member(a) {
                    members.push(prop);
                }
            }
            ElementAttribute::BindDirective(b) if b.name != "this" => {
                members.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: b.name.clone(),
                        span: Span::ZERO,
                    }),
                    value: b.expression.clone(),
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
            }
            _ => {}
        }
    }
    let obj = Expression::Object(Box::new(ObjectExpression {
        properties: members,
        span: Span::ZERO,
    }));

    // Append a namespace flag for SVG/MathML/custom-element/input. Mirrors
    // upstream's constants:
    //   ELEMENT_IS_NAMESPACED              = 1
    //   ELEMENT_PRESERVE_ATTRIBUTE_CASE    = 2
    //   ELEMENT_IS_INPUT                   = 4
    // - SVG/MathML elements: 1 | 2 = 3
    // - custom elements (tag contains `-`): 2
    // - input: 4
    // - everything else: 0 (no flag arg emitted at all)
    let flag = if el.name == "svg" || el.name == "math" {
        Some(3.0)
    } else if el.name == "input" {
        Some(4.0)
    } else if el.name.contains('-') {
        Some(2.0)
    } else {
        None
    };
    let mut args = vec![Argument::Expression(obj)];
    if let Some(f) = flag {
        let void0 = void_zero_expr();
        args.push(Argument::Expression(void0.clone()));
        args.push(Argument::Expression(void0.clone()));
        args.push(Argument::Expression(void0));
        args.push(Argument::Expression(Expression::Literal(Box::new(Literal::Number(
            NumberLiteral {
                value: f,
                raw: Some((f as u32).to_string()),
                span: Span::ZERO,
            },
        )))));
    }
    buf.push_expr(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "attributes"),
        arguments: args,
        optional: false,
        span: Span::ZERO,
    })));
}

/// Mirrors `binding_properties[name].omit_in_ssr` from upstream's
/// `phases/bindings.js`. Returns true for bindings whose underlying
/// property is readonly/browser-only and therefore has no SSR attribute.
/// True if the attribute value is a single static Text part matching `expected`.
fn attr_value_is_text(value: &AttributeValue, expected: &str) -> bool {
    match value {
        AttributeValue::Many(parts) if parts.len() == 1 => {
            matches!(&parts[0], AttributeValuePart::Text(t) if t.data == expected)
        }
        _ => false,
    }
}

fn bind_omit_in_ssr(name: &str) -> bool {
    matches!(
        name,
        // media
        "currentTime" | "duration" | "paused" | "buffered" | "seekable"
        | "played" | "volume" | "muted" | "playbackRate" | "seeking"
        | "ended" | "readyState"
        // video
        | "videoHeight" | "videoWidth"
        // img
        | "naturalWidth" | "naturalHeight"
        // document
        | "activeElement" | "fullscreenElement" | "pointerLockElement"
        | "visibilityState"
        // window
        | "innerWidth" | "innerHeight" | "outerWidth" | "outerHeight"
        | "scrollX" | "scrollY" | "online" | "devicePixelRatio"
        // dimensions
        | "clientWidth" | "clientHeight" | "offsetWidth" | "offsetHeight"
        | "contentRect" | "contentBoxSize" | "borderBoxSize"
        | "devicePixelContentBoxSize"
        // checkbox/radio
        | "indeterminate"
        // refs / files
        | "this" | "files"
    )
}

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
            // Class attribute on a scoped element: append `svelte-{hash}`.
            if a.name == "class" {
                let hash = CSS_HASH.with(|c| c.borrow().clone());
                if let Some(hash) = hash {
                    return append_class_attribute_with_hash(&a.value, &hash, buf);
                }
            }
            append_value_attribute(&a.name, &a.value, buf)
        }
        ElementAttribute::BindDirective(b) => {
            // Bindings that resolve to readonly browser-only state aren't
            // emitted in SSR (`bind:clientWidth`, `bind:innerHeight`, etc).
            // Mirrors `binding_properties[name].omit_in_ssr`.
            if bind_omit_in_ssr(&b.name) {
                return Some(());
            }
            // `bind:value` on a `<select>` is handled by the select lowerer.
            // `bind:value` on `<input type="file">` is omitted.
            // (Both are handled elsewhere; here we just emit the attribute.)
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
            // `style={...}` / `class={...}` use dedicated SSR helpers so the
            // runtime can scope/normalize the value correctly.
            if name == "style" {
                buf.push_expr(Expression::Call(Box::new(CallExpression {
                    callee: t::member_id(t::id("$"), "attr_style"),
                    arguments: vec![Argument::Expression(tag.expression.clone())],
                    optional: false,
                    span: Span::ZERO,
                })));
                return Some(());
            }
            // Constant-fold literal-string ExpressionTags to inline form so
            // `autocomplete={'no'}` → `autocomplete="no"` instead of going
            // through `$.attr(...)`. Mirrors upstream's literal-fold pass.
            if let Some(s) = literal_expr_to_string(&tag.expression) {
                buf.push_str(" ");
                buf.push_str(name);
                buf.push_str("=\"");
                buf.push_str(&escape_attribute_text(&s));
                buf.push_str("\"");
                return Some(());
            }
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
                return Some(());
            }
            // Mixed Text + ExpressionTag → render as a template literal
            // with `$.stringify(EXPR)` interpolations, wrapped in `$.attr`.
            // Quasis come from text runs; consecutive ExpressionTags need
            // an empty quasi between them to satisfy template-literal shape
            // (N expressions → N+1 quasis).
            let mut quasis: Vec<String> = Vec::with_capacity(parts.len() + 1);
            let mut exprs: Vec<Expression> = Vec::with_capacity(parts.len());
            let mut pending: String = String::new();
            let mut expecting_quasi = true;
            for p in parts {
                match p {
                    AttributeValuePart::Text(t) => {
                        pending.push_str(&t.data);
                        expecting_quasi = false;
                    }
                    AttributeValuePart::ExpressionTag(tag) => {
                        if expecting_quasi {
                            // Two ExpressionTags in a row — flush an empty quasi.
                            quasis.push(std::mem::take(&mut pending));
                        } else {
                            quasis.push(std::mem::take(&mut pending));
                            expecting_quasi = true;
                        }
                        exprs.push(Expression::Call(Box::new(CallExpression {
                            callee: t::member_id(t::id("$"), "stringify"),
                            arguments: vec![Argument::Expression(tag.expression.clone())],
                            optional: false,
                            span: Span::ZERO,
                        })));
                    }
                }
            }
            quasis.push(pending);
            let tpl = t::template_raw(quasis, exprs);
            // `style='...'` → `${$.attr_style(\`...\`)}` (no name arg).
            if name == "style" {
                buf.push_expr(Expression::Call(Box::new(CallExpression {
                    callee: t::member_id(t::id("$"), "attr_style"),
                    arguments: vec![Argument::Expression(tpl)],
                    optional: false,
                    span: Span::ZERO,
                })));
                return Some(());
            }
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "attr"),
                arguments: vec![
                    Argument::Expression(string_lit(name)),
                    Argument::Expression(tpl),
                ],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
    }
}

/// Append the `class` attribute with the scoped CSS hash spliced into the
/// final class list. Handles three shapes:
///   - Empty: `class=""` → `class="svelte-HASH"`
///   - Single ExpressionTag: `class={x}` → `${$.attr_class(x, 'svelte-HASH')}`
///   - Many parts (Text/ExpressionTag mix): static if all text, otherwise
///     template-literal wrapped in `$.attr_class`.
fn append_class_attribute_with_hash(
    value: &AttributeValue,
    hash: &str,
    buf: &mut TemplateBuf,
) -> Option<()> {
    match value {
        AttributeValue::Empty => {
            buf.push_str(" class=\"");
            buf.push_str(hash);
            buf.push_str("\"");
            Some(())
        }
        AttributeValue::Single(tag) => {
            buf.push_expr(Expression::Call(Box::new(CallExpression {
                callee: t::member_id(t::id("$"), "attr_class"),
                arguments: vec![
                    Argument::Expression(tag.expression.clone()),
                    Argument::Expression(string_lit(hash)),
                ],
                optional: false,
                span: Span::ZERO,
            })));
            Some(())
        }
        AttributeValue::Many(parts) => {
            if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                // Static text → splice the hash into the literal value.
                let mut combined = String::new();
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        combined.push_str(&t.data);
                    }
                }
                let combined = format!("{} {}", combined.trim(), hash);
                buf.push_str(" class=\"");
                buf.push_str(&escape_attribute_text(combined.trim()));
                buf.push_str("\"");
                Some(())
            } else {
                // Dynamic — wrap in `$.attr_class(\`...\`, 'svelte-HASH')`.
                let mut quasis: Vec<String> = Vec::with_capacity(parts.len() + 1);
                let mut exprs: Vec<Expression> = Vec::with_capacity(parts.len());
                let mut pending: String = String::new();
                let mut expecting_quasi = true;
                for p in parts {
                    match p {
                        AttributeValuePart::Text(t) => {
                            pending.push_str(&t.data);
                            expecting_quasi = false;
                        }
                        AttributeValuePart::ExpressionTag(tag) => {
                            if expecting_quasi {
                                quasis.push(std::mem::take(&mut pending));
                            } else {
                                quasis.push(std::mem::take(&mut pending));
                                expecting_quasi = true;
                            }
                            exprs.push(Expression::Call(Box::new(CallExpression {
                                callee: t::member_id(t::id("$"), "stringify"),
                                arguments: vec![Argument::Expression(tag.expression.clone())],
                                optional: false,
                                span: Span::ZERO,
                            })));
                        }
                    }
                }
                quasis.push(pending);
                let tpl = t::template_raw(quasis, exprs);
                buf.push_expr(Expression::Call(Box::new(CallExpression {
                    callee: t::member_id(t::id("$"), "attr_class"),
                    arguments: vec![
                        Argument::Expression(tpl),
                        Argument::Expression(string_lit(hash)),
                    ],
                    optional: false,
                    span: Span::ZERO,
                })));
                Some(())
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

/// Mirrors upstream's `LOAD_ERROR_ELEMENTS` in `packages/svelte/src/utils.js`.
fn is_load_error_element(name: &str) -> bool {
    matches!(
        name,
        "body" | "embed" | "iframe" | "img" | "link" | "object"
        | "script" | "style" | "track"
    )
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

    /// Append `s` but dedupe a leading whitespace char when the buf already
    /// ends in space. Used after a Comment is dropped to collapse the
    /// text-comment-text whitespace bridge.
    fn push_str_after_comment(&mut self, s: &str) {
        let last = self.parts.last_mut().unwrap();
        if last.ends_with(' ') {
            let trimmed = s.trim_start_matches(|c: char| c.is_whitespace());
            last.push_str(trimmed);
        } else {
            last.push_str(s);
        }
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

    /// Flush to a `$$renderer.push(\`...\`)` statement (or None when empty).
    /// Whitespace-only buffers DO flush — they typically represent
    /// inter-element WS between side-statements that upstream preserves
    /// as `$$renderer.push(\` \`)`. Boundary trim is handled by
    /// `trim_boundary_whitespace`.
    fn flush(&mut self) -> Option<Statement> {
        if self.is_empty() {
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

/// `<svelte:element this={tag}>BODY</svelte:element>` →
/// `$.element($$renderer, tag, attrs, () => { ...body... });`
/// `attrs` is `void 0` when there are no attributes; the body arrow is
/// omitted when the body is empty.
fn lower_svelte_element_server(
    el: &svelte_ast::elements::SvelteElement,
) -> Option<Statement> {
    let mut args = vec![
        Argument::Expression(t::id("$$renderer")),
        Argument::Expression(el.tag.clone()),
    ];

    // Has any non-WS body content?
    let has_body = el.fragment.nodes.iter().any(|n| match n {
        FragmentChild::Text(t) => !t.data.trim().is_empty(),
        FragmentChild::Comment(_) => false,
        _ => true,
    });

    if has_body {
        // Build the attrs object from regular attrs/spreads (skipping
        // directives the server doesn't emit). When empty, pass `void 0`.
        let mut props: Vec<ObjectMember> = Vec::new();
        for a in &el.attributes {
            match a {
                ElementAttribute::Attribute(attr) => {
                    if is_event_handler_name(&attr.name) {
                        continue;
                    }
                    if let Some(p) = attribute_to_object_member(attr) {
                        props.push(p);
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
        if props.is_empty() {
            args.push(Argument::Expression(void_zero_expr()));
        } else {
            args.push(Argument::Expression(Expression::Object(Box::new(
                ObjectExpression {
                    properties: props,
                    span: Span::ZERO,
                },
            ))));
        }
        // Body arrow: lower the fragment without a leading marker —
        // `$.element` already handles the wrap, no need for is_text_first.
        let body_stmts = lower_fragment_with_marker(&el.fragment, false)?;
        args.push(Argument::Expression(Expression::Arrow(Box::new(
            ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: body_stmts,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            },
        ))));
    }

    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::member_id(t::id("$"), "element"),
        arguments: args,
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
            ElementAttribute::BindDirective(b) => {
                if b.name == "this" {
                    // bind:this is handled by typed_fast/typed_client_component
                    // for the simple case. Server doesn't emit a getter/setter
                    // pair for it.
                    continue;
                }
                // `bind:NAME={target}` on a Component lowers to a getter/
                // setter pair so the parent observes mutations from the
                // child. The setter also flips `$$settled = false` to
                // re-render under the do-while wrap.
                props.push(make_bind_getter(&b.name, &b.expression));
                props.push(make_bind_setter(&b.name, &b.expression));
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

/// `bind:NAME={target}` → `get NAME() { return target; }` accessor pair.
fn make_bind_getter(name: &str, target: &Expression) -> ObjectMember {
    let body = vec![Statement::Return(Box::new(ReturnStatement {
        argument: Some(target.clone()),
        span: Span::ZERO,
    }))];
    ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: name.to_string(),
            span: Span::ZERO,
        }),
        value: Expression::Function(Box::new(FunctionExpression {
            id: None,
            params: Vec::new(),
            body: BlockStatement {
                body,
                span: Span::ZERO,
            },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        })),
        kind: PropertyKind::Get,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    }))
}

/// `bind:NAME={target}` → `set NAME($$value) { target = $$value; $$settled = false; }`.
fn make_bind_setter(name: &str, target: &Expression) -> ObjectMember {
    let assign_target = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(target.clone()),
        operator: AssignmentOperator::Assign,
        right: t::id("$$value"),
        span: Span::ZERO,
    }));
    let assign_settled = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(t::id("$$settled")),
        operator: AssignmentOperator::Assign,
        right: Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: false,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(assign_target), t::stmt(assign_settled)];
    ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: name.to_string(),
            span: Span::ZERO,
        }),
        value: Expression::Function(Box::new(FunctionExpression {
            id: None,
            params: vec![t::pat_id("$$value")],
            body: BlockStatement {
                body,
                span: Span::ZERO,
            },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        })),
        kind: PropertyKind::Set,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    }))
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
                // Concatenated parts → template literal with `$.stringify`
                // interpolations.
                let mut quasis: Vec<String> = Vec::with_capacity(parts.len() + 1);
                let mut exprs: Vec<Expression> = Vec::with_capacity(parts.len());
                let mut pending: String = String::new();
                let mut expecting_quasi = true;
                for p in parts {
                    match p {
                        AttributeValuePart::Text(t) => {
                            pending.push_str(&t.data);
                            expecting_quasi = false;
                        }
                        AttributeValuePart::ExpressionTag(tag) => {
                            if expecting_quasi {
                                quasis.push(std::mem::take(&mut pending));
                            } else {
                                quasis.push(std::mem::take(&mut pending));
                                expecting_quasi = true;
                            }
                            exprs.push(Expression::Call(Box::new(CallExpression {
                                callee: t::member_id(t::id("$"), "stringify"),
                                arguments: vec![Argument::Expression(tag.expression.clone())],
                                optional: false,
                                span: Span::ZERO,
                            })));
                        }
                    }
                }
                quasis.push(pending);
                t::template_raw(quasis, exprs)
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
            FragmentChild::Text(_) => true,
            // Comments are dropped server-side but require whitespace
            // collapse against neighbouring Text nodes — that logic lives
            // in the full walker, not in typed_fast. Defer to the walker.
            FragmentChild::Comment(_) => false,
            FragmentChild::RegularElement(el) => {
                // `<option>` and `<select>` are NOT static even with no
                // attributes — they need `$$renderer.option(...)` /
                // customizable-select-element call lowering.
                if el.name == "option" || el.name == "select" {
                    return false;
                }
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
        FragmentChild::ConstTag(ct) => {
            for d in &mut ct.declaration.declarations {
                if let Some(init) = &mut d.init {
                    script::call_derived_refs(init, derived);
                }
            }
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
    // Hoist every ImportDeclaration to the top regardless of source order —
    // upstream's `<script>` parser preserves user ordering but the server
    // codegen always emits imports first (they're scoped at module level).
    let mut imports = Vec::new();
    let mut rest = Vec::new();
    for s in body {
        match s {
            Statement::Import(_) => imports.push(s.clone()),
            _ => rest.push(s.clone()),
        }
    }
    Some((imports, rest))
}
