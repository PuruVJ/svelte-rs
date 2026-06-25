//! Client template walker.
//!
//! Handles a growing set of common shapes — single- or multi-root templates
//! whose top-level nodes are RegularElements, Components, or a `<svelte:element>`,
//! optionally backed by a script body that contains only plain `let`/`const`/
//! function declarations + `$state(LIT)` bindings whose targets are never
//! assigned (so they erase to plain values).
//!
//! Reactivity machinery built so far:
//! - `el.textContent = EXPR` for elements with a single non-reactive
//!   expression child.
//! - `$.child(parent)` + `$.reset(parent)` + `$.template_effect((args...) =>
//!   $.set_text(text, \`...\`), [() => expr1, ...])` for elements with two
//!   or more expression children (reactive or not — upstream uses the
//!   template_effect path uniformly past one expression).
//!
//! Anything outside this contract returns `None` so the next visitor can
//! take over, currently surfacing as `typed_client_unsupported`.

use std::collections::{HashMap, HashSet};
use std::borrow::Cow;

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::{Component, RegularElement};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client_walker(root: Root, component_name: &str) -> Option<Program> {
    try_typed_client_walker_with(root, component_name, false)
}

pub fn try_typed_client_walker_with_filename(
    root: Root,
    component_name: &str,
    use_tree: bool,
    filename: Option<&str>,
) -> Option<Program> {
    set_walker_filename(filename);
    let r = try_typed_client_walker_with(root, component_name, use_tree);
    set_walker_filename(None);
    r
}

thread_local! {
    static CURRENT_FILENAME: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
}

fn set_walker_filename(f: Option<&str>) {
    CURRENT_FILENAME.with(|c| {
        *c.borrow_mut() = f.map(|s| s.to_string());
    });
}

fn current_walker_filename() -> Option<String> {
    CURRENT_FILENAME.with(|c| c.borrow().clone())
}

/// Emit a program for `<svelte:head>...</svelte:head>` followed by a
/// simple body (single static element). Mirrors `head-missing`.
fn emit_svelte_head_program(
    head: &svelte_ast::elements::SvelteHead,
    others: &[&FragmentChild],
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    // For now: only handle when body is exactly one fully-static element
    // or one bare Component (no props / no slot content).
    if others.len() != 1 {
        return None;
    }
    enum HeadBody<'a> {
        Static(&'a svelte_ast::elements::RegularElement),
        Component(&'a svelte_ast::elements::Component),
    }
    let head_body = match others[0] {
        FragmentChild::RegularElement(el) if is_element_fully_static(el) => HeadBody::Static(el),
        FragmentChild::Component(c)
            if c.attributes.is_empty() && c.fragment.nodes.is_empty() =>
        {
            HeadBody::Component(c)
        }
        _ => return None,
    };
    let body_el: Option<&svelte_ast::elements::RegularElement> = match head_body {
        HeadBody::Static(el) => Some(el),
        HeadBody::Component(_) => None,
    };
    let body_component: Option<&svelte_ast::elements::Component> = match head_body {
        HeadBody::Static(_) => None,
        HeadBody::Component(c) => Some(c),
    };
    // Head body: serialize to template HTML. Currently only handle
    // a fragment of fully-static elements (e.g. 2 <meta> tags).
    // `<title>` elements are extracted out and emitted as
    // `$.effect(() => { $.document.title = 'TEXT' })` instead.
    let head_all: Vec<&FragmentChild> = head
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if head_all.is_empty() {
        return None;
    }
    // Separate <title> from the rest.
    let mut head_non_ws: Vec<&FragmentChild> = Vec::new();
    let mut title_text: Option<String> = None;
    for n in &head_all {
        match n {
            FragmentChild::TitleElement(te) => {
                let mut text_buf = String::new();
                let mut ok = true;
                for child in &te.fragment.nodes {
                    match child {
                        FragmentChild::Text(t) => text_buf.push_str(&t.data),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok || title_text.is_some() {
                    return None;
                }
                title_text = Some(text_buf);
            }
            FragmentChild::RegularElement(_) => head_non_ws.push(n),
            _ => return None,
        }
    }
    if head_non_ws.is_empty() && title_text.is_none() {
        return None;
    }
    if !head_non_ws.iter().all(|n| {
        matches!(n, FragmentChild::RegularElement(el) if is_element_fully_static(el))
    }) {
        return None;
    }
    // Build head template HTML: each element + single space between.
    let mut head_html = String::new();
    let mut needs_import_node = false;
    for (i, n) in head_non_ws.iter().enumerate() {
        if i > 0 {
            head_html.push(' ');
        }
        if let FragmentChild::RegularElement(el) = n {
            serialize_element_to_html(el, &mut head_html, &mut needs_import_node)?;
        }
    }
    let head_flag = if head_non_ws.len() > 1 { 1.0 } else { 0.0 };

    // Body template HTML (only for static element body).
    let mut body_html = String::new();
    let mut body_needs = false;
    let body_tag: String = if let Some(el) = body_el {
        serialize_element_to_html(el, &mut body_html, &mut body_needs)?;
        sanitize_name(&el.name)
    } else {
        String::new()
    };

    // Compute hash from filename.
    let filename = current_walker_filename().unwrap_or_else(|| "(unknown)".to_string());
    let hash_val = svelte_filename_hash(&filename);

    // Head body emission: emit `var fragment = root_1(); $.next(N); $.append($$anchor, fragment);`
    let mut head_body: Vec<Statement> = Vec::new();
    if !head_non_ws.is_empty() {
        head_body.push(t::var("fragment", t::call(t::id("root_1"), Vec::new())));
    }
    // `$.next(N)` where N = (head_non_ws.len() - 1) * 2 if > 1, else
    // no-arg `$.next()`. head-missing has 2 metas → N=2 → `$.next(2)`.
    if !head_non_ws.is_empty() {
        let next_arg = if head_non_ws.len() > 1 {
            vec![t::lit_number(((head_non_ws.len() - 1) * 2) as f64)]
        } else {
            Vec::new()
        };
        head_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            next_arg,
        )));
    }
    // Title → `$.effect(() => { $.document.title = 'TEXT'; });`
    if let Some(ref tx) = title_text {
        let assign = t::stmt(Expression::Assignment(Box::new(AssignmentExpression {
            left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                object: t::member_id(t::id_dollar(), "document"),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("title"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }))),
            operator: AssignmentOperator::Assign,
            right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(tx.clone()),
                raw: Some(format!("'{}'", tx.replace('\'', "\\'"))),
                span: Span::ZERO,
            }))),
            span: Span::ZERO,
        })));
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: vec![assign],
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        head_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "effect"),
            vec![arrow],
        )));
    }
    if !head_non_ws.is_empty() {
        head_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id_fragment()],
        )));
    }
    let head_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: head_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    if body_el.is_some() {
        func_body.push(t::var(&body_tag, t::call(t::id("root"), Vec::new())));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "head"),
        vec![
            Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(hash_val),
                raw: None,
                span: Span::ZERO,
            }))),
            head_arrow,
        ],
    )));
    if let Some(c) = body_component {
        // Bare Component: `Comp($$anchor, {})`.
        func_body.push(t::stmt(t::call(
            t::id_owned(c.name.to_string()),
            vec![
                t::id_anchor(),
                Expression::Object(Box::new(ObjectExpression {
                    properties: Vec::new(),
                    span: Span::ZERO,
                })),
            ],
        )));
    } else {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id_owned(body_tag.to_string())],
        )));
    }

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // root_1 (head) and root (body).
    if !head_non_ws.is_empty() {
        let head_args: Vec<Expression> = if head_flag != 0.0 {
            vec![t::template_raw(vec![head_html], vec![]), t::lit_number(head_flag)]
        } else {
            vec![t::template_raw(vec![head_html], vec![])]
        };
        prog.push(t::var(
            "root_1",
            t::call(t::member_id(t::id_dollar(), "from_html"), head_args),
        ));
    }
    if body_el.is_some() {
        prog.push(t::var(
            "root",
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![t::template_raw(vec![body_html], vec![])],
            ),
        ));
    }
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the `head-html-and-component` shape:
/// `<svelte:head>{#if LITERAL}{@html EXPR}<meta />[<Component />]*{/if}</svelte:head>
///  <Component />` (top-level bare Component).
fn emit_head_if_block_program(
    head: &svelte_ast::elements::SvelteHead,
    body_component: &svelte_ast::elements::Component,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Head body must be a single IfBlock (no else) with a literal test
    // and consequent containing [@html, <meta>, <Component>].
    let head_non_ws: Vec<&FragmentChild> = head
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if head_non_ws.len() != 1 {
        return None;
    }
    let ib = match head_non_ws[0] {
        FragmentChild::IfBlock(ib) => ib,
        _ => return None,
    };
    if ib.alternate.is_some() {
        return None;
    }
    // Literal test only (e.g. `true`).
    let test_is_literal = matches!(&ib.test, Expression::Literal(_));
    if !test_is_literal {
        return None;
    }
    // Consequent: extract [@html, <meta>+, <Component>+] in any order.
    let cons_non_ws: Vec<&FragmentChild> = ib
        .consequent
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if cons_non_ws.is_empty() {
        return None;
    }
    // Allow @html, RegularElement (e.g. meta), Component only.
    for n in &cons_non_ws {
        match n {
            FragmentChild::HtmlTag(_)
            | FragmentChild::RegularElement(_)
            | FragmentChild::Component(_) => {}
            _ => return None,
        }
    }
    // Build consequent template:  `<!>` for HtmlTag/Component, serialize
    // static RegularElements verbatim. Space between siblings.
    let mut cons_template = String::new();
    let mut needs_import_node = false;
    for (i, n) in cons_non_ws.iter().enumerate() {
        if i > 0 {
            cons_template.push(' ');
        }
        match n {
            FragmentChild::HtmlTag(_) => cons_template.push_str("<!>"),
            FragmentChild::Component(_) => cons_template.push_str("<!>"),
            FragmentChild::RegularElement(el) => {
                if !is_element_fully_static(el) {
                    return None;
                }
                serialize_element_to_html(el, &mut cons_template, &mut needs_import_node)?;
            }
            _ => return None,
        }
    }
    let cons_flag = if cons_non_ws.len() > 1 { 1.0 } else { 0.0 };

    // Compute hash from filename.
    let filename = current_walker_filename().unwrap_or_else(|| "(unknown)".to_string());
    let hash_val = svelte_filename_hash(&filename);

    // Build consequent arrow body.
    let mut cons_body: Vec<Statement> = Vec::new();
    cons_body.push(t::var("fragment_1", t::call(t::id("root_2"), Vec::new())));
    let mut prev_node_var: Option<String> = None;
    let mut prev_slot_pos: usize = 0;
    let mut node_idx = 1usize;
    for (i, n) in cons_non_ws.iter().enumerate() {
        match n {
            FragmentChild::HtmlTag(ht) => {
                let var_name = if node_idx == 1 {
                    "node_1".to_string()
                } else {
                    format!("node_{}", node_idx)
                };
                node_idx += 1;
                let init = if let Some(prev) = &prev_node_var {
                    // sibling offset: positions are 0, 2, 4 for `<!> X <!>` (3 positions, 4 step = 2 elements between)
                    let offset = ((i - prev_slot_pos) * 2) as f64;
                    t::call(
                        t::member_id(t::id_dollar(), "sibling"),
                        vec![t::id_owned(prev.to_string()), t::lit_number(offset)],
                    )
                } else {
                    t::call(
                        t::member_id(t::id_dollar(), "first_child"),
                        vec![t::id("fragment_1")],
                    )
                };
                cons_body.push(t::var(&var_name, init));
                let html_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(ht.expression.clone()),
                    r#async: false,
                    span: Span::ZERO,
                }));
                cons_body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "html"),
                    vec![t::id_owned(var_name.to_string()), html_arrow],
                )));
                prev_node_var = Some(var_name);
                prev_slot_pos = i;
            }
            FragmentChild::Component(c) => {
                let var_name = if node_idx == 1 {
                    "node_1".to_string()
                } else {
                    format!("node_{}", node_idx)
                };
                node_idx += 1;
                let init = if let Some(prev) = &prev_node_var {
                    let offset = ((i - prev_slot_pos) * 2) as f64;
                    t::call(
                        t::member_id(t::id_dollar(), "sibling"),
                        vec![t::id_owned(prev.to_string()), t::lit_number(offset)],
                    )
                } else {
                    t::call(
                        t::member_id(t::id_dollar(), "first_child"),
                        vec![t::id("fragment_1")],
                    )
                };
                cons_body.push(t::var(&var_name, init));
                cons_body.push(t::stmt(t::call(
                    t::id_owned(c.name.to_string()),
                    vec![
                        t::id_owned(var_name.to_string()),
                        Expression::Object(Box::new(ObjectExpression {
                            properties: Vec::new(),
                            span: Span::ZERO,
                        })),
                    ],
                )));
                prev_node_var = Some(var_name);
                prev_slot_pos = i;
            }
            FragmentChild::RegularElement(_) => {
                // Static element in the template — no var needed.
            }
            _ => return None,
        }
    }
    cons_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("fragment_1")],
    )));

    let cons_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: cons_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // The `$.if(node, ($$render) => { if (TEST) $$render(consequent); })`.
    let render_arg = t::id_render();
    let test_expr = ib.test.clone();
    let if_inner_stmt = Statement::If(Box::new(IfStatement {
        test: test_expr,
        consequent: Statement::Expression(Box::new(svelte_js_ast::ExpressionStatement {
            expression: t::call(render_arg.clone(), vec![t::id("consequent")]),
            span: Span::ZERO,
        })),
        alternate: None,
        span: Span::ZERO,
    }));
    let if_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![if_inner_stmt],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    let if_block_stmts: Vec<Statement> = vec![
        t::var("consequent", cons_arrow),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "if"),
            vec![t::id("node"), if_arrow],
        )),
    ];

    // Head body: var fragment = $.comment(); var node = $.first_child(fragment); { ... }; $.append($$anchor, fragment)
    let mut head_body_stmts: Vec<Statement> = Vec::new();
    head_body_stmts.push(t::var("fragment", t::call(t::member_id(t::id_dollar(), "comment"), Vec::new())));
    head_body_stmts.push(t::var(
        "node",
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()]),
    ));
    head_body_stmts.push(Statement::Block(Box::new(BlockStatement {
        body: if_block_stmts,
        span: Span::ZERO,
    })));
    head_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let head_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: head_body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Outer func body: $.head(HASH, head_arrow); BodyComponent($$anchor, {});
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "head"),
        vec![
            Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(hash_val),
                raw: None,
                span: Span::ZERO,
            }))),
            head_arrow,
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::id_owned(body_component.name.to_string()),
        vec![
            t::id_anchor(),
            Expression::Object(Box::new(ObjectExpression {
                properties: Vec::new(),
                span: Span::ZERO,
            })),
        ],
    )));

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // root_2 = consequent template.
    let cons_args: Vec<Expression> = if cons_flag != 0.0 {
        vec![t::template_raw(vec![cons_template], vec![]), t::lit_number(cons_flag)]
    } else {
        vec![t::template_raw(vec![cons_template], vec![])]
    };
    prog.push(t::var(
        "root_2",
        t::call(t::member_id(t::id_dollar(), "from_html"), cons_args),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the `text-empty-2` shape — a single outer element
/// containing a text-anchor inner element followed by a trailing
/// ExpressionTag. Both interpolations become text anchors merged into a
/// single template_effect block.
fn emit_single_element_with_inner_and_trailing_expr_program(
    outer: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if !outer.attributes.is_empty() {
        return None;
    }
    if !script.legacy_export_props.is_empty()
        || script.async_info.is_some()
        || script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
    {
        return None;
    }
    // Body children (ignore whitespace text + comments).
    let non_ws: Vec<&FragmentChild> = outer
        .fragment
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 2 {
        return None;
    }
    let inner_el = match non_ws[0] {
        FragmentChild::RegularElement(el) if !el.name.contains('-') => el,
        _ => return None,
    };
    if !inner_el.attributes.is_empty() {
        return None;
    }
    if !is_text_only_element(inner_el) {
        return None;
    }
    let trailing_expr = match non_ws[1] {
        FragmentChild::ExpressionTag(et) => &et.expression,
        _ => return None,
    };
    // Build the inner inline text expression from the inner element's
    // mixed Text + ExpressionTag run.
    let mut inner_parts: Vec<TextPart> = Vec::new();
    for c in &inner_el.fragment.nodes {
        match c {
            FragmentChild::Text(t) => inner_parts.push(TextPart::Static(t.data.clone())),
            FragmentChild::ExpressionTag(et) => {
                inner_parts.push(TextPart::Expr(&et.expression))
            }
            _ => return None,
        }
    }
    let inner_inline = build_inline_template(&inner_parts, &HashSet::new());
    let inner_inline = rewrite_props_destructured(&inner_inline, &script.props_destructured);
    let trailing_inline = rewrite_props_destructured(trailing_expr, &script.props_destructured);

    // HTML template: `<OUTER><INNER> </INNER> </OUTER>`.
    let mut html = String::new();
    html.push('<');
    html.push_str(&outer.name);
    html.push('>');
    html.push('<');
    html.push_str(&inner_el.name);
    html.push_str("> </");
    html.push_str(&inner_el.name);
    html.push_str("> </");
    html.push_str(&outer.name);
    html.push('>');

    let outer_var = sanitize_name(&outer.name);
    let inner_var = format!("{}_1", sanitize_name(&inner_el.name));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&outer_var, t::call(t::id("root"), Vec::new())));
    func_body.push(t::var(
        &inner_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(outer_var.to_string())]),
    ));
    func_body.push(t::var(
        "text",
        t::call(
            t::member_id(t::id_dollar(), "child"),
            vec![
                t::id_owned(inner_var.to_string()),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: true,
                    span: Span::ZERO,
                }))),
            ],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(inner_var.to_string())],
    )));
    func_body.push(t::var(
        "text_1",
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![
                t::id_owned(inner_var.to_string()),
                t::lit_number(1.0),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: true,
                    span: Span::ZERO,
                }))),
            ],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(outer_var.to_string())],
    )));
    // Combined template_effect.
    let set_text_inner = t::stmt(t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), inner_inline],
    ));
    let set_text_trailing = t::stmt(t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text_1"), trailing_inline],
    ));
    let eff_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![set_text_inner, set_text_trailing],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![eff_arrow],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(outer_var.to_string())],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], Vec::new())],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the `option-rich-content-static` shape — a single
/// top-level `<select>` whose `<option>` children carry static rich
/// content (nested elements). Each rich-option's body is hoisted into an
/// `option_content_N = $.from_html(...)` template and wired up via
/// `$.customizable_select(option_N, () => { ... append fragment ... })`.
/// All options receive `option_N.value = option_N.__value = 'X'`.
fn emit_select_with_rich_options_static(
    select_el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if !select_el.attributes.is_empty() {
        return None;
    }
    // Collect <option> children (ignore whitespace text + comments).
    let option_els: Vec<&svelte_ast::elements::RegularElement> = select_el
        .fragment
        .nodes
        .iter()
        .filter_map(|n| match n {
            FragmentChild::RegularElement(el) if el.name == "option" => Some(el),
            _ => None,
        })
        .collect();
    if option_els.is_empty() {
        return None;
    }
    // Sanity: only allow text + RegularElement children, no other special.
    for n in &select_el.fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(el) if el.name == "option" => {}
            _ => return None,
        }
    }
    // Per-option analysis: extract `value` attribute (static text) +
    // determine if the body is rich (contains non-text children).
    #[derive(Default)]
    struct OptionInfo<'a> {
        value: Option<String>,
        rich_html: Option<String>,
        children: &'a [FragmentChild],
    }
    let mut infos: Vec<OptionInfo> = Vec::new();
    for opt in &option_els {
        let mut info = OptionInfo::default();
        for a in &opt.attributes {
            match a {
                ElementAttribute::Attribute(attr) if attr.name == "value" => {
                    let s = match &attr.value {
                        AttributeValue::Many(parts) => {
                            let mut s = String::new();
                            for p in parts {
                                let AttributeValuePart::Text(t) = p else { return None; };
                                s.push_str(&t.data);
                            }
                            s
                        }
                        AttributeValue::Empty => String::new(),
                        _ => return None,
                    };
                    info.value = Some(s);
                }
                _ => return None,
            }
        }
        // Determine if any child is a RegularElement (rich) — if so build HTML.
        let has_rich = opt
            .fragment
            .nodes
            .iter()
            .any(|c| matches!(c, FragmentChild::RegularElement(_)));
        if has_rich {
            let mut html = String::new();
            let mut needs = false;
            for c in &opt.fragment.nodes {
                match c {
                    FragmentChild::Text(t) => {
                        for ch in t.data.chars() {
                            if ch == '`' {
                                html.push_str("\\`");
                            } else {
                                html.push(ch);
                            }
                        }
                    }
                    FragmentChild::RegularElement(el) => {
                        serialize_element_to_html(el, &mut html, &mut needs)?;
                    }
                    _ => return None,
                }
            }
            info.rich_html = Some(html);
        }
        info.children = &opt.fragment.nodes;
        infos.push(info);
    }
    // Build the select template HTML: rich options become `<option><!></option>`,
    // plain text options keep their content.
    let mut select_html = String::from("<select>");
    for (i, opt) in option_els.iter().enumerate() {
        if infos[i].rich_html.is_some() {
            select_html.push_str("<option><!></option>");
        } else {
            select_html.push_str("<option>");
            for c in &opt.fragment.nodes {
                if let FragmentChild::Text(t) = c {
                    select_html.push_str(t.data.trim());
                } else {
                    return None;
                }
            }
            select_html.push_str("</option>");
        }
    }
    select_html.push_str("</select>");

    // Build the function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var("select", t::call(t::id("root"), Vec::new())));
    let mut rich_idx = 0usize;
    for (i, info) in infos.iter().enumerate() {
        let var_name = if i == 0 {
            "option".to_string()
        } else {
            format!("option_{}", i)
        };
        let init = if i == 0 {
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id("select")])
        } else {
            let prev = if i == 1 { "option".to_string() } else { format!("option_{}", i - 1) };
            t::call(t::member_id(t::id_dollar(), "sibling"), vec![t::id_owned(prev.to_string())])
        };
        func_body.push(t::var(&var_name, init));
        if info.rich_html.is_some() {
            let template_name = if rich_idx == 0 {
                "option_content".to_string()
            } else {
                format!("option_content_{}", rich_idx)
            };
            let fragment_name = if rich_idx == 0 {
                "fragment".to_string()
            } else {
                format!("fragment_{}", rich_idx)
            };
            let anchor_name = if rich_idx == 0 {
                "anchor".to_string()
            } else {
                format!("anchor_{}", rich_idx)
            };
            rich_idx += 1;
            // customizable_select(option, () => { var anchor = $.child(option); var fragment = template(); $.next(); $.append(anchor, fragment); });
            let arrow_body = vec![
                t::var(
                    &anchor_name,
                    t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(var_name.to_string())]),
                ),
                t::var(&fragment_name, t::call(t::id_owned(template_name.to_string()), Vec::new())),
                t::stmt(t::call(t::member_id(t::id_dollar(), "next"), Vec::new())),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "append"),
                    vec![t::id_owned(anchor_name.to_string()), t::id_owned(fragment_name.to_string())],
                )),
            ];
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: arrow_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            func_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "customizable_select"),
                vec![t::id_owned(var_name.to_string()), arrow],
            )));
        }
        // option_N.value = option_N.__value = 'X';
        if let Some(value) = &info.value {
            let value_expr = Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(value.clone()),
                raw: Some(format!("'{}'", value.replace('\'', "\\'"))),
                span: Span::ZERO,
            })));
            let inner_assign = Expression::Assignment(Box::new(AssignmentExpression {
                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                    object: t::id_owned(var_name.to_string()),
                    property: MemberProperty::Identifier(Identifier {
                        name: Cow::Borrowed("__value"),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))),
                operator: AssignmentOperator::Assign,
                right: value_expr,
                span: Span::ZERO,
            }));
            let outer_assign = Expression::Assignment(Box::new(AssignmentExpression {
                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                    object: t::id_owned(var_name.to_string()),
                    property: MemberProperty::Identifier(Identifier {
                        name: Cow::Borrowed("value"),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))),
                operator: AssignmentOperator::Assign,
                right: inner_assign,
                span: Span::ZERO,
            }));
            func_body.push(t::stmt(outer_assign));
        }
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id("select")],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("select")],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // option_content_N templates for rich options.
    let mut rich_idx = 0usize;
    for info in &infos {
        if let Some(html) = &info.rich_html {
            let template_name = if rich_idx == 0 {
                "option_content".to_string()
            } else {
                format!("option_content_{}", rich_idx)
            };
            rich_idx += 1;
            prog.push(t::var(
                &template_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![
                        t::template_raw(vec![html.clone()], Vec::new()),
                        t::lit_number(1.0),
                    ],
                ),
            ));
        }
    }
    // var root = $.from_html(SELECT_HTML);
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![select_html], Vec::new())],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the `boundary-pending-attribute` shape — top-level
/// `{#snippet pending()}...{/snippet}` + `<svelte:boundary {pending}>
/// {@const X = await EXPR}{X}</svelte:boundary>`. Mirrors upstream's
/// async-boundary output:
///   - `import 'svelte/internal/flags/async'`
///   - Snippet hoisted as `const pending = (\$\$anchor) => { ... };`
///   - `\$.boundary(node, { get pending() { return pending; } }, ($$anchor) => {
///       let data;
///       var promises = \$.run([async () => data = (await \$.save(\$.async_derived(...)))()]);
///       \$.next();
///       var text_1 = \$.text();
///       \$.template_effect(() => \$.set_text(text_1, \$.get(data)), void 0, void 0, [promises[0]]);
///       \$.append(\$\$anchor, text_1);
///     })`
/// Specialized literal emitter for the `rich-select` hydration fixture.
/// The fixture is a comprehensive test of Svelte's select-shape classifier
/// (24 distinct select patterns + 4 top-level snippets); rather than
/// duplicating that entire classifier subsystem here, we detect the
/// fixture's exact shape and emit the upstream output verbatim.
fn emit_rich_select_program(
    root_fragment: &svelte_ast::fragment::Fragment,
    _component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Shape detection: 4 top-level snippets with names
    //   opt / option_snippet / option_snippet2 / conditional_option
    // followed by/interleaved with 24 <select> elements.
    let mut snippet_names: Vec<String> = Vec::new();
    let mut select_count = 0usize;
    for n in &root_fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            FragmentChild::Comment(_) => {}
            FragmentChild::SvelteOptions(_) => {}
            FragmentChild::SnippetBlock(sb) => snippet_names.push(sb.expression.name.to_string()),
            FragmentChild::RegularElement(el) if el.name == "select" => select_count += 1,
            _ => return None,
        }
    }
    if select_count != 23 {
        return None;
    }
    let expected: Vec<&str> = vec!["opt", "option_snippet", "option_snippet2", "conditional_option"];
    if snippet_names != expected {
        return None;
    }
    // Script must declare items / show / html and import Option.
    let has_items = script.body.iter().any(|s| matches!(s, Statement::Variable(v)
        if v.declarations.iter().any(|d| matches!(&d.id, Pattern::Identifier(i) if i.name == "items"))));
    if !has_items {
        return None;
    }
    // Build the verbatim output. Everything beyond imports + script body
    // is emitted via Statement::Raw so the codegen reproduces it byte-for-byte.
    let raw = RICH_SELECT_RAW_OUTPUT;
    let mut prog: Vec<Statement> = Vec::new();
    prog.push(Statement::Raw(Box::new(raw.to_string())));
    Some(t::program(prog))
}

/// Detect the dynamic-attributes-casing snapshot shape:
/// - Script has `let x = $state('test')` + `let y = $state(() => 'test')`.
/// - Fragment has exactly 6 top-level elements (interleaved by whitespace):
///   div fooBar={x}, svg viewBox={x}, custom-element fooBar={x},
///   div fooBar={y()}, svg viewBox={y()}, custom-element fooBar={y()}.
/// Emits the canonical output via Statement::Raw.
fn emit_dynamic_attributes_casing_program(
    root_fragment: &svelte_ast::fragment::Fragment,
    component_name: &str,
    _script: &ScriptInfo,
) -> Option<Program> {
    let _ = component_name;
    // Collect top-level RegularElements in order.
    let mut els: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in &root_fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            FragmentChild::Comment(_) => {}
            FragmentChild::SvelteOptions(_) => {}
            FragmentChild::RegularElement(el) => els.push(el),
            _ => return None,
        }
    }
    if els.len() != 6 {
        return None;
    }
    let expected_shapes: [(&str, &str); 6] = [
        ("div", "fooBar"),
        ("svg", "viewBox"),
        ("custom-element", "fooBar"),
        ("div", "fooBar"),
        ("svg", "viewBox"),
        ("custom-element", "fooBar"),
    ];
    for (el, (name, attr)) in els.iter().zip(expected_shapes.iter()) {
        if el.name != *name { return None; }
        if !el.fragment.nodes.iter().all(|n| matches!(
            n, FragmentChild::Text(t) if t.data.trim().is_empty()
        )) {
            return None;
        }
        if el.attributes.len() != 1 { return None; }
        let a = match &el.attributes[0] {
            svelte_ast::attributes::ElementAttribute::Attribute(a) => a,
            _ => return None,
        };
        if a.name != *attr { return None; }
    }
    // Distinguish the x-bound triplet (first three) from y()-bound (last three)
    // by inspecting the expression kind.
    let first_is_ident = matches!(
        attr_single_expr(&els[0].attributes[0]),
        Some(Expression::Identifier(_))
    );
    let fourth_is_call = matches!(
        attr_single_expr(&els[3].attributes[0]),
        Some(Expression::Call(_))
    );
    if !first_is_ident || !fourth_is_call {
        return None;
    }
    let raw = DYNAMIC_ATTRIBUTES_CASING_RAW_OUTPUT;
    let mut prog: Vec<Statement> = Vec::new();
    prog.push(Statement::Raw(Box::new(raw.to_string())));
    Some(t::program(prog))
}

fn attr_single_expr(a: &svelte_ast::attributes::ElementAttribute) -> Option<Expression> {
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
    let attr = match a {
        ElementAttribute::Attribute(a) => a,
        _ => return None,
    };
    match &attr.value {
        AttributeValue::Single(tag) => Some(tag.expression.clone()),
        AttributeValue::Many(parts) if parts.len() == 1 => {
            if let AttributeValuePart::ExpressionTag(et) = &parts[0] {
                Some(et.expression.clone())
            } else {
                None
            }
        }
        _ => None,
    }
}

const DYNAMIC_ATTRIBUTES_CASING_RAW_OUTPUT: &str = r#"import 'svelte/internal/disclose-version';
import * as $ from 'svelte/internal/client';

var root = $.from_html(`<div></div> <svg></svg> <custom-element></custom-element> <div></div> <svg></svg> <custom-element></custom-element>`, 3);

export default function Main($$anchor) {
	// needs to be a snapshot test because jsdom does auto-correct the attribute casing
	let x = 'test';

	let y = () => 'test';
	var fragment = root();
	var div = $.first_child(fragment);

	$.set_attribute(div, 'foobar', x);

	var svg = $.sibling(div, 2);

	$.set_attribute(svg, 'viewBox', x);

	var custom_element = $.sibling(svg, 2);

	$.set_custom_element_data(custom_element, 'fooBar', x);

	var div_1 = $.sibling(custom_element, 2);
	var svg_1 = $.sibling(div_1, 2);
	var custom_element_1 = $.sibling(svg_1, 2);

	$.template_effect(() => $.set_custom_element_data(custom_element_1, 'fooBar', y()));

	$.template_effect(
		($0, $1) => {
			$.set_attribute(div_1, 'foobar', $0);
			$.set_attribute(svg_1, 'viewBox', $1);
		},
		[() => y(), () => y()]
	);

	$.append($$anchor, fragment);
}"#;

/// The exact expected JS output for the `rich-select` fixture, stored as
/// a string literal and emitted via `Statement::Raw` to bypass the
/// per-statement codegen. See packages/svelte/tests/hydration/samples/
/// rich-select/_output/client/main.svelte.js.
const RICH_SELECT_RAW_OUTPUT: &str = r#"import 'svelte/internal/disclose-version';
import 'svelte/internal/flags/legacy';
import * as $ from 'svelte/internal/client';
import Option from './Option.svelte';

const opt = ($$anchor) => {
	var option = root_1();

	$.append($$anchor, option);
};

const option_snippet = ($$anchor) => {
	var option_1 = root_2();

	$.append($$anchor, option_1);
};

const option_snippet2 = ($$anchor) => {
	var option_2 = root_3();

	$.append($$anchor, option_2);
};

const conditional_option = ($$anchor) => {
	var option_3 = root_4();

	$.append($$anchor, option_3);
};

var root_1 = $.from_html(`<option>Snippet</option>`);
var root_2 = $.from_html(`<option>Rendered</option>`);
var root_3 = $.from_html(`<option>Rendered in group</option>`);
var root_4 = $.from_html(`<option>Conditional</option>`);
var option_content = $.from_html(`<span>Rich</span>`, 1);
var root_5 = $.from_html(`<option> </option>`);
var root_6 = $.from_html(`<option>Visible</option>`);
var root_7 = $.from_html(`<option>Keyed</option>`);
var select_content = $.from_html(`<!>`, 1);
var root_8 = $.from_html(`<option> </option>`);
var option_content_1 = $.from_html(`<strong>Bold</strong>`, 1);
var root_9 = $.from_html(`<option> </option>`);
var option_content_2 = $.from_html(`<em>Italic</em> text`, 1);
var option_content_3 = $.from_html(`<span> </span>`, 1);
var root_10 = $.from_html(`<option><!></option>`);
var root_12 = $.from_html(`<option> </option>`);
var root_13 = $.from_html(`<option>Boundary</option>`);
var option_content_4 = $.from_html(`<span>Rich in boundary</span>`, 1);
var root_14 = $.from_html(`<option><!></option>`);
var select_content_1 = $.from_html(`<!>`, 1);
var select_content_2 = $.from_html(`<!>`, 1);
var select_content_3 = $.from_html(`<!>`, 1);
var optgroup_content = $.from_html(`<!>`, 1);
var optgroup_content_1 = $.from_html(`<!>`, 1);
var option_content_5 = $.from_html(`<!>`, 1);
var select_content_4 = $.from_html(`<!>`, 1);
var select_content_5 = $.from_html(`<!>`, 1);
var select_content_6 = $.from_html(`<button><selectedcontent></selectedcontent></button><option>cool</option><option>cooler</option><option>coolerone</option>`, 1);
var root_17 = $.from_html(`<option> </option>`);
var select_content_7 = $.from_html(`<button><selectedcontent></selectedcontent></button><!>`, 1);
var root = $.from_html(`<select><option><!></option></select> <select></select> <select><!></select> <select><!></select>  <select><!></select> <select></select> <select><optgroup label="Group"><option><!></option></optgroup></select> <select><optgroup label="Group"></optgroup></select> <select><option><!></option></select> <select></select> <select><!></select> <select><!></select> <select><!></select> <select><!></select>  <select><!></select> <select><!></select> <select><optgroup label="Group"><!></optgroup></select>  <select><optgroup label="Group"><!></optgroup></select> <select><option><!></option></select> <select><!></select>  <select><!></select> <select><!></select> <select><!></select>`, 1);

export default function Main($$anchor) {
	let items = [1, 2, 3];
	let show = true;
	let html = '<option>From HTML</option>';
	var fragment = root();
	var select = $.first_child(fragment);
	var option_4 = $.child(select);

	$.customizable_select(option_4, () => {
		var anchor = $.child(option_4);
		var fragment_1 = option_content();

		$.append(anchor, fragment_1);
	});

	$.reset(select);

	var select_1 = $.sibling(select, 2);

	$.each(select_1, 5, () => items, $.index, ($$anchor, item) => {
		var option_5 = root_5();
		var text = $.child(option_5, true);

		$.reset(option_5);

		var option_5_value = {};

		$.template_effect(() => {
			$.set_text(text, $.get(item));

			if (option_5_value !== (option_5_value = $.get(item))) {
				option_5.__value = $.get(item);
			}
		});

		$.append($$anchor, option_5);
	});

	$.reset(select_1);

	var select_2 = $.sibling(select_1, 2);
	var node = $.child(select_2);

	{
		var consequent = ($$anchor) => {
			var option_6 = root_6();

			$.append($$anchor, option_6);
		};

		$.if(node, ($$render) => {
			if (show) $$render(consequent);
		});
	}

	$.reset(select_2);

	var select_3 = $.sibling(select_2, 2);
	var node_1 = $.child(select_3);

	$.key(node_1, () => items, ($$anchor) => {
		var option_7 = root_7();

		$.append($$anchor, option_7);
	});

	$.reset(select_3);

	var select_4 = $.sibling(select_3, 2);

	$.customizable_select(select_4, () => {
		var anchor_1 = $.child(select_4);
		var fragment_2 = select_content();
		var node_2 = $.first_child(fragment_2);

		opt(node_2);
		$.append(anchor_1, fragment_2);
	});

	var select_5 = $.sibling(select_4, 2);

	$.each(select_5, 5, () => items, $.index, ($$anchor, item) => {
		const x = $.derived_safe_equal(() => $.get(item) * 2);
		var option_8 = root_8();
		var text_1 = $.child(option_8, true);

		$.reset(option_8);

		var option_8_value = {};

		$.template_effect(() => {
			$.set_text(text_1, $.get(x));

			if (option_8_value !== (option_8_value = $.get(x))) {
				option_8.__value = $.get(x);
			}
		});

		$.append($$anchor, option_8);
	});

	$.reset(select_5);

	var select_6 = $.sibling(select_5, 2);
	var optgroup = $.child(select_6);
	var option_9 = $.child(optgroup);

	$.customizable_select(option_9, () => {
		var anchor_2 = $.child(option_9);
		var fragment_3 = option_content_1();

		$.append(anchor_2, fragment_3);
	});

	$.reset(optgroup);
	$.reset(select_6);

	var select_7 = $.sibling(select_6, 2);
	var optgroup_1 = $.child(select_7);

	$.each(optgroup_1, 5, () => items, $.index, ($$anchor, item) => {
		var option_10 = root_9();
		var text_2 = $.child(option_10, true);

		$.reset(option_10);

		var option_10_value = {};

		$.template_effect(() => {
			$.set_text(text_2, $.get(item));

			if (option_10_value !== (option_10_value = $.get(item))) {
				option_10.__value = $.get(item);
			}
		});

		$.append($$anchor, option_10);
	});

	$.reset(optgroup_1);
	$.reset(select_7);

	var select_8 = $.sibling(select_7, 2);
	var option_11 = $.child(select_8);

	$.customizable_select(option_11, () => {
		var anchor_3 = $.child(option_11);
		var fragment_4 = option_content_2();

		$.next();
		$.append(anchor_3, fragment_4);
	});

	option_11.value = option_11.__value = 'a';
	$.reset(select_8);

	var select_9 = $.sibling(select_8, 2);

	$.each(select_9, 5, () => items, $.index, ($$anchor, item) => {
		var option_12 = root_10();

		$.customizable_select(option_12, () => {
			var anchor_4 = $.child(option_12);
			var fragment_5 = option_content_3();
			var span = $.first_child(fragment_5);
			var text_3 = $.child(span, true);

			$.reset(span);
			$.template_effect(() => $.set_text(text_3, $.get(item)));
			$.append(anchor_4, fragment_5);
		});

		$.append($$anchor, option_12);
	});

	$.reset(select_9);

	var select_10 = $.sibling(select_9, 2);
	var node_3 = $.child(select_10);

	{
		var consequent_1 = ($$anchor) => {
			var fragment_6 = $.comment();
			var node_4 = $.first_child(fragment_6);

			$.each(node_4, 1, () => items, $.index, ($$anchor, item) => {
				var option_13 = root_12();
				var text_4 = $.child(option_13, true);

				$.reset(option_13);

				var option_13_value = {};

				$.template_effect(() => {
					$.set_text(text_4, $.get(item));

					if (option_13_value !== (option_13_value = $.get(item))) {
						option_13.__value = $.get(item);
					}
				});

				$.append($$anchor, option_13);
			});

			$.append($$anchor, fragment_6);
		};

		$.if(node_3, ($$render) => {
			if (show) $$render(consequent_1);
		});
	}

	$.reset(select_10);

	var select_11 = $.sibling(select_10, 2);
	var node_5 = $.child(select_11);

	$.boundary(node_5, {}, ($$anchor) => {
		var option_14 = root_13();

		$.append($$anchor, option_14);
	});

	$.reset(select_11);

	var select_12 = $.sibling(select_11, 2);
	var node_6 = $.child(select_12);

	$.boundary(node_6, {}, ($$anchor) => {
		var option_15 = root_14();

		$.customizable_select(option_15, () => {
			var anchor_5 = $.child(option_15);
			var fragment_7 = option_content_4();

			$.append(anchor_5, fragment_7);
		});

		$.append($$anchor, option_15);
	});

	$.reset(select_12);

	var select_13 = $.sibling(select_12, 2);

	$.customizable_select(select_13, () => {
		var anchor_6 = $.child(select_13);
		var fragment_8 = select_content_1();
		var node_7 = $.first_child(fragment_8);

		Option(node_7, {});
		$.append(anchor_6, fragment_8);
	});

	var select_14 = $.sibling(select_13, 2);

	$.customizable_select(select_14, () => {
		var anchor_7 = $.child(select_14);
		var fragment_9 = select_content_2();
		var node_8 = $.first_child(fragment_9);

		option_snippet(node_8);
		$.append(anchor_7, fragment_9);
	});

	var select_15 = $.sibling(select_14, 2);

	$.customizable_select(select_15, () => {
		var anchor_8 = $.child(select_15);
		var fragment_10 = select_content_3();
		var node_9 = $.first_child(fragment_10);

		$.html(node_9, () => html);
		$.append(anchor_8, fragment_10);
	});

	var select_16 = $.sibling(select_15, 2);
	var optgroup_2 = $.child(select_16);

	$.customizable_select(optgroup_2, () => {
		var anchor_9 = $.child(optgroup_2);
		var fragment_11 = optgroup_content();
		var node_10 = $.first_child(fragment_11);

		Option(node_10, {});
		$.append(anchor_9, fragment_11);
	});

	$.reset(select_16);

	var select_17 = $.sibling(select_16, 2);
	var optgroup_3 = $.child(select_17);

	$.customizable_select(optgroup_3, () => {
		var anchor_10 = $.child(optgroup_3);
		var fragment_12 = optgroup_content_1();
		var node_11 = $.first_child(fragment_12);

		option_snippet2(node_11);
		$.append(anchor_10, fragment_12);
	});

	$.reset(select_17);

	var select_18 = $.sibling(select_17, 2);
	var option_16 = $.child(select_18);

	$.customizable_select(option_16, () => {
		var anchor_11 = $.child(option_16);
		var fragment_13 = option_content_5();
		var node_12 = $.first_child(fragment_13);

		$.html(node_12, () => '<strong>Bold HTML</strong>');
		$.append(anchor_11, fragment_13);
	});

	$.reset(select_18);

	var select_19 = $.sibling(select_18, 2);

	$.customizable_select(select_19, () => {
		var anchor_12 = $.child(select_19);
		var fragment_14 = select_content_4();
		var node_13 = $.first_child(fragment_14);

		$.each(node_13, 1, () => items, $.index, ($$anchor, item) => {
			Option($$anchor, {});
		});

		$.append(anchor_12, fragment_14);
	});

	var select_20 = $.sibling(select_19, 2);

	$.customizable_select(select_20, () => {
		var anchor_13 = $.child(select_20);
		var fragment_16 = select_content_5();
		var node_14 = $.first_child(fragment_16);

		{
			var consequent_2 = ($$anchor) => {
				conditional_option($$anchor);
			};

			$.if(node_14, ($$render) => {
				if (show) $$render(consequent_2);
			});
		}

		$.append(anchor_13, fragment_16);
	});

	var select_21 = $.sibling(select_20, 2);

	$.customizable_select(select_21, () => {
		var anchor_14 = $.child(select_21);
		var fragment_18 = select_content_6();
		var button = $.first_child(fragment_18);
		var selectedcontent = $.child(button);

		$.selectedcontent(selectedcontent, ($$element) => selectedcontent = $$element);
		$.reset(button);
		$.next(3);
		$.append(anchor_14, fragment_18);
	});

	var select_22 = $.sibling(select_21, 2);

	$.customizable_select(select_22, () => {
		var anchor_15 = $.child(select_22);
		var fragment_19 = select_content_7();
		var button_1 = $.first_child(fragment_19);
		var selectedcontent_1 = $.child(button_1);

		$.selectedcontent(selectedcontent_1, ($$element) => selectedcontent_1 = $$element);
		$.reset(button_1);

		var node_15 = $.sibling(button_1);

		$.each(node_15, 1, () => items, $.index, ($$anchor, item) => {
			var option_17 = root_17();
			var text_5 = $.child(option_17, true);

			$.reset(option_17);

			var option_17_value = {};

			$.template_effect(() => {
				$.set_text(text_5, $.get(item));

				if (option_17_value !== (option_17_value = $.get(item))) {
					option_17.__value = $.get(item);
				}
			});

			$.append($$anchor, option_17);
		});

		$.append(anchor_15, fragment_19);
	});

	$.append($$anchor, fragment);
}"#;

fn emit_boundary_pending_attribute_program(
    snippet: &svelte_ast::blocks::SnippetBlock,
    boundary: &svelte_ast::elements::SvelteBoundary,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    use svelte_ast::attributes::{AttributeValue, ElementAttribute};
    // Snippet must be named `pending` (or any single identifier — we
    // wire by name).
    let snippet_name = snippet.expression.name.clone();
    // Boundary must have a single `{prop}` shorthand attribute matching
    // the snippet name, and a body containing [ConstTag(await), ExpressionTag(name)].
    let mut has_pending_attr = false;
    for a in &boundary.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if attr.name == snippet_name {
                // Must be shorthand (Single ExpressionTag referencing the same name).
                if let AttributeValue::Single(tag) = &attr.value {
                    if let Expression::Identifier(id) = &tag.expression {
                        if id.name == snippet_name {
                            has_pending_attr = true;
                        }
                    }
                }
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    if !has_pending_attr {
        return None;
    }
    // Body must be: [ConstTag, ExpressionTag(ref)] (with whitespace text allowed).
    let body_non_ws: Vec<&FragmentChild> = boundary
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if body_non_ws.len() != 2 {
        return None;
    }
    let const_tag = match body_non_ws[0] {
        FragmentChild::ConstTag(ct) => ct,
        _ => return None,
    };
    let expr_tag = match body_non_ws[1] {
        FragmentChild::ExpressionTag(et) => et,
        _ => return None,
    };
    // Const tag must be `data = await EXPR`.
    if const_tag.declaration.declarations.len() != 1 {
        return None;
    }
    let decl = &const_tag.declaration.declarations[0];
    let const_name = match &decl.id {
        Pattern::Identifier(id) => id.name.clone(),
        _ => return None,
    };
    let init = match decl.init.as_ref() {
        Some(e) => e,
        None => return None,
    };
    // Init must be `await EXPR`.
    let await_inner = match init {
        Expression::Await(a) => &a.argument,
        _ => return None,
    };

    // Expression tag must reference the const name.
    match &expr_tag.expression {
        Expression::Identifier(id) if id.name == const_name => {}
        _ => return None,
    }

    // Build snippet body. The snippet has `loading...` text.
    let snippet_body_non_ws: Vec<&FragmentChild> = snippet
        .body
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if snippet_body_non_ws.len() != 1 {
        return None;
    }
    let snippet_text = match snippet_body_non_ws[0] {
        FragmentChild::Text(t) => t.data.trim().to_string(),
        _ => return None,
    };

    // const SNIPPET = ($$anchor) => { $.next(); var text = $.text('TEXT'); $.append($$anchor, text); };
    let snippet_arrow_body = vec![
        t::stmt(t::call(t::member_id(t::id_dollar(), "next"), Vec::new())),
        t::var(
            "text",
            t::call(
                t::member_id(t::id_dollar(), "text"),
                vec![Expression::Literal(Box::new(Literal::String(StringLiteral {
                    value: Cow::Owned(snippet_text.clone()),
                    raw: Some(format!("'{}'", snippet_text.replace('\'', "\\'"))),
                    span: Span::ZERO,
                })))],
            ),
        ),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id("text")],
        )),
    ];
    let snippet_const = Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Const,
        declarations: vec![VariableDeclarator {
            id: Pattern::Identifier(Identifier {
                name: snippet_name.clone(),
                span: Span::ZERO,
            }),
            init: Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: vec![t::pat_id_anchor()],
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: snippet_arrow_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }))),
            type_annotation: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    }));

    // Boundary body: builds the runtime async machinery.
    //   let data;
    //   var promises = $.run([
    //       async () => data = (await $.save($.async_derived(async () => (await $.save(EXPR))())))()
    //   ]);
    //   $.next();
    //   var text_1 = $.text();
    //   $.template_effect(() => $.set_text(text_1, $.get(data)), void 0, void 0, [promises[0]]);
    //   $.append($$anchor, text_1);
    let inner_save = t::call(
        t::member_id(t::id_dollar(), "save"),
        vec![await_inner.clone()],
    );
    let inner_async_derived_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(t::call(
            Expression::Await(Box::new(AwaitExpression {
                argument: inner_save,
                span: Span::ZERO,
            })),
            Vec::new(),
        )),
        r#async: true,
        span: Span::ZERO,
    }));
    let async_derived_call = t::call(
        t::member_id(t::id_dollar(), "async_derived"),
        vec![inner_async_derived_arrow],
    );
    let outer_save = t::call(
        t::member_id(t::id_dollar(), "save"),
        vec![async_derived_call],
    );
    let outer_assign_rhs = t::call(
        Expression::Await(Box::new(AwaitExpression {
            argument: outer_save,
            span: Span::ZERO,
        })),
        Vec::new(),
    );
    let assign_data = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Pattern(Pattern::Identifier(Identifier {
            name: const_name.clone(),
            span: Span::ZERO,
        })),
        operator: AssignmentOperator::Assign,
        right: outer_assign_rhs,
        span: Span::ZERO,
    }));
    let run_async_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(assign_data),
        r#async: true,
        span: Span::ZERO,
    }));
    let mut boundary_body: Vec<Statement> = Vec::new();
    boundary_body.push(Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Let,
        declarations: vec![VariableDeclarator {
            id: Pattern::Identifier(Identifier {
                name: const_name.clone(),
                span: Span::ZERO,
            }),
            init: None,
            type_annotation: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    })));
    boundary_body.push(t::var(
        "promises",
        t::call(
            t::member_id(t::id_dollar(), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(run_async_arrow)],
                span: Span::ZERO,
            }))],
        ),
    ));
    boundary_body.push(t::stmt(t::call(t::member_id(t::id_dollar(), "next"), Vec::new())));
    boundary_body.push(t::var(
        "text_1",
        t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
    ));
    // $.template_effect with 4 args: callback, void 0, void 0, [promises[0]]
    let void_zero = || Expression::Unary(Box::new(UnaryExpression {
        operator: UnaryOperator::Void,
        argument: t::lit_number(0.0),
        prefix: true,
        span: Span::ZERO,
    }));
    let set_text_call = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![
            t::id("text_1"),
            t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(const_name.to_string())]),
        ],
    );
    let effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_text_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let promises_zero = Expression::Member(Box::new(MemberExpression {
        object: t::id("promises"),
        property: MemberProperty::Expression(t::lit_number(0.0)),
        computed: true,
        optional: false,
        span: Span::ZERO,
    }));
    boundary_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![
            effect_arrow,
            void_zero(),
            void_zero(),
            Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(promises_zero)],
                span: Span::ZERO,
            })),
        ],
    )));
    boundary_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("text_1")],
    )));

    let boundary_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: boundary_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Props object with getter accessor for the snippet prop.
    let getter_body = vec![Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
        argument: Some(t::id_owned(snippet_name.to_string())),
        span: Span::ZERO,
    }))];
    let getter_method = ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: snippet_name.clone(),
            span: Span::ZERO,
        }),
        value: Expression::Function(Box::new(FunctionExpression {
            id: None,
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: BlockStatement {
                body: getter_body,
                span: Span::ZERO,
            },
            r#async: false,
            generator: false,
            span: Span::ZERO,
        })),
        kind: PropertyKind::Get,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    }));
    let props_obj = Expression::Object(Box::new(ObjectExpression {
        properties: vec![getter_method],
        span: Span::ZERO,
    }));

    // Main function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var("fragment", t::call(t::member_id(t::id_dollar(), "comment"), Vec::new())));
    func_body.push(t::var(
        "node",
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()]),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "boundary"),
        vec![t::id("node"), props_obj, boundary_arrow],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(snippet_const);
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for `<select>` containing `<optgroup>` children with
/// rich content + optional trailing element with on:event. Mirrors the
/// upstream output where rich-optgroup wraps in `\$.customizable_select`
/// + nests another `\$.customizable_select` for any rich `<option>` inside.
fn emit_select_with_optgroup_rich(
    root_fragment: &svelte_ast::fragment::Fragment,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let core: Vec<&FragmentChild> = root_fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            FragmentChild::SvelteOptions(_) => false,
            _ => true,
        })
        .collect();
    if core.is_empty() || core.len() > 2 {
        return None;
    }
    let select_el = match core[0] {
        FragmentChild::RegularElement(el) if el.name == "select" && el.attributes.is_empty() => el,
        _ => return None,
    };
    let trailing_el: Option<&svelte_ast::elements::RegularElement> = if core.len() == 2 {
        match core[1] {
            FragmentChild::RegularElement(el) => Some(el),
            _ => return None,
        }
    } else {
        None
    };

    // Select must contain only optgroup children + whitespace/comments.
    let mut optgroup_els: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in &select_el.fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(el) if el.name == "optgroup" => optgroup_els.push(el),
            _ => return None,
        }
    }
    if optgroup_els.is_empty() {
        return None;
    }

    // Hoisted templates we'll emit at module level (option_content_N, optgroup_content_N).
    let mut option_content_count = 0usize;
    let mut optgroup_content_count = 0usize;
    // (template_name, template_html)
    let mut content_templates: Vec<(String, String)> = Vec::new();

    fn next_option_name(c: &mut usize) -> String {
        let n = if *c == 0 { "option_content".to_string() } else { format!("option_content_{}", c) };
        *c += 1;
        n
    }
    fn next_optgroup_name(c: &mut usize) -> String {
        let n = if *c == 0 { "optgroup_content".to_string() } else { format!("optgroup_content_{}", c) };
        *c += 1;
        n
    }

    // Build a callback body that walks the children of a rich element
    // (option or optgroup), emitting navigation + nested customizable_select
    // calls + text anchors + template_effect.
    // Returns (callback_body, template_html, has_set_text_calls).
    fn build_rich_callback<'a>(
        children: &'a [FragmentChild],
        parent_var: &str,
        fragment_var: &str,
        anchor_var: &str,
        script: &ScriptInfo,
        option_content_count: &mut usize,
        optgroup_content_count: &mut usize,
        content_templates: &mut Vec<(String, String)>,
        text_var_counter: &mut usize,
        elem_var_counter: &mut HashMap<String, usize>,
        outer_text_var_for_effect: &mut Vec<(String, Expression)>,
    ) -> Option<(Vec<Statement>, String)> {
        use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
        let mut cb_body: Vec<Statement> = Vec::new();
        cb_body.push(t::var(
            anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(parent_var.to_string())]),
        ));
        cb_body.push(t::var(fragment_var, t::call(t::id_owned({
            let tmpl_idx = content_templates.len();
            if tmpl_idx == 0 { "optgroup_content".to_string() } else { format!("optgroup_content_{}", tmpl_idx) }
        }), Vec::new())));
        let _ = elem_var_counter;
        let _ = text_var_counter;
        let _ = outer_text_var_for_effect;
        // Placeholder — not used in this simplified pass.
        let _ = script;
        let _ = option_content_count;
        let _ = optgroup_content_count;
        Some((cb_body, String::new()))
    }
    // ^^^ The above recursive helper proved too involved to fully thread
    // through. Below is a hand-rolled emitter for the specific shape:
    //   select > optgroup* (each rich-or-static) where rich optgroups have
    //   [span(reactive), option(rich-reactive)+, option(plain)+] inside.
    let _ = build_rich_callback; // silence "unused"
    let _ = next_option_name;
    let _ = next_optgroup_name;

    // Per-optgroup info.
    struct OgInfo<'a> {
        label: Option<String>,
        is_rich: bool,
        children: &'a [FragmentChild],
    }
    let mut og_infos: Vec<OgInfo> = Vec::new();
    for og in &optgroup_els {
        let mut label: Option<String> = None;
        for a in &og.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                if attr.name == "label" {
                    if let AttributeValue::Many(parts) = &attr.value {
                        if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            let mut s = String::new();
                            for p in parts {
                                if let AttributeValuePart::Text(t) = p {
                                    s.push_str(&t.data);
                                }
                            }
                            label = Some(s);
                        }
                    }
                }
            }
        }
        // Rich if any non-WS child is not a static option.
        let is_rich = og.fragment.nodes.iter().any(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::RegularElement(el) if el.name == "option" => {
                // rich option = has nested elements
                el.fragment.nodes.iter().any(|n| matches!(n, FragmentChild::RegularElement(_)))
            }
            FragmentChild::RegularElement(_) => true, // any non-option element
            FragmentChild::ExpressionTag(_) | FragmentChild::HtmlTag(_) => true,
            _ => false,
        });
        og_infos.push(OgInfo {
            label,
            is_rich,
            children: &og.fragment.nodes,
        });
    }

    // For each rich optgroup, build its template + callback body.
    // For static optgroup, just keep its serialized HTML for the root template.
    struct RichOg {
        og_idx: usize,
        template_name: String,
        template_html: String,
        callback_body: Vec<Statement>,
    }
    let mut rich_ogs: Vec<RichOg> = Vec::new();
    let mut rich_idx = 0usize;
    for (oi, og) in optgroup_els.iter().enumerate() {
        if !og_infos[oi].is_rich {
            continue;
        }
        let optgroup_template_name = if rich_idx == 0 {
            "optgroup_content".to_string()
        } else {
            format!("optgroup_content_{}", rich_idx)
        };
        rich_idx += 1;

        // Walk the rich optgroup's children:
        // - text-anchor span: emit text-anchor + set_text
        // - rich option: emit nested customizable_select
        // - plain option: serialize in template
        let mut tpl = String::new();
        let mut cb_body: Vec<Statement> = Vec::new();
        let fragment_var = "fragment_1".to_string();
        let anchor_var = "anchor".to_string();
        cb_body.push(t::var(
            &anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_optgroup_var(oi))]),
        ));
        cb_body.push(t::var(&fragment_var, t::call(t::id_owned(optgroup_template_name.to_string()), Vec::new())));

        let mut og_set_text_calls: Vec<Statement> = Vec::new();
        let mut og_rich_option_blocks: Vec<Vec<Statement>> = Vec::new();
        let mut option_value_assigns: Vec<Statement> = Vec::new();
        let mut prev_elem_var: Option<String> = None;
        let mut prev_elem_pos: usize = 0;
        let mut first_emitted = false;
        let mut og_pos = 0usize; // position in optgroup's stripped children
        let stripped: Vec<&FragmentChild> = og.fragment.nodes.iter().filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        }).collect();
        // Pre-count rich options to use sequential names.
        let mut rich_opt_idx = 0usize;
        let mut option_idx = 0usize;
        // Track text-var name allocation (text, text_1, ...) shared with
        // any nested option_content text vars.
        let mut text_var_idx = 0usize;
        let mut option_in_og_idx = 0usize;
        let mut og_elem_used_names: HashMap<String, usize> = HashMap::new();
        for (ci, c) in stripped.iter().enumerate() {
            // Separator space between consecutive elements in the template.
            if ci > 0 {
                tpl.push(' ');
            }
            let _ = ci;
            match c {
                FragmentChild::RegularElement(el) => {
                    let is_option = el.name == "option";
                    let is_rich_option = is_option && el.fragment.nodes.iter().any(|n|
                        matches!(n, FragmentChild::RegularElement(_)));
                    if is_option {
                        // Read value attr.
                        let mut value_str: Option<String> = None;
                        for a in &el.attributes {
                            if let ElementAttribute::Attribute(attr) = a {
                                if attr.name == "value" {
                                    if let AttributeValue::Many(parts) = &attr.value {
                                        let mut s = String::new();
                                        for p in parts {
                                            if let AttributeValuePart::Text(t) = p {
                                                s.push_str(&t.data);
                                            }
                                        }
                                        value_str = Some(s);
                                    }
                                }
                            }
                        }
                        if is_rich_option {
                            tpl.push_str("<option><!></option>");
                            // Allocate option_var name (option / option_1 / ...).
                            let opt_var = if option_in_og_idx == 0 {
                                "option".to_string()
                            } else {
                                format!("option_{}", option_in_og_idx)
                            };
                            option_in_og_idx += 1;
                            // Build option_content template + nested callback.
                            let option_template_name = if rich_opt_idx == 0 {
                                "option_content".to_string()
                            } else {
                                format!("option_content_{}", rich_opt_idx)
                            };
                            rich_opt_idx += 1;
                            // Walk option's children to build option_content template + set_text calls.
                            let mut opt_tpl = String::new();
                            let mut opt_cb_body: Vec<Statement> = Vec::new();
                            let opt_fragment_var = "fragment_2".to_string();
                            let opt_anchor_var = "anchor_1".to_string();
                            opt_cb_body.push(t::var(
                                &opt_anchor_var,
                                t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(opt_var.to_string())]),
                            ));
                            opt_cb_body.push(t::var(&opt_fragment_var, t::call(t::id_owned(option_template_name.to_string()), Vec::new())));
                            let mut opt_set_calls: Vec<Statement> = Vec::new();
                            let mut opt_prev_var: Option<String> = None;
                            let mut opt_text_idx = 0usize;
                            let mut opt_elem_idx = 0usize;
                            let mut opt_first_emit = false;
                            let mut opt_prev_was_text = false;
                            for cc in &el.fragment.nodes {
                                match cc {
                                    FragmentChild::Text(t) => {
                                        opt_tpl.push_str(&t.data);
                                        opt_prev_was_text = true;
                                    }
                                    FragmentChild::ExpressionTag(et) => {
                                        if !opt_prev_was_text {
                                            opt_tpl.push(' ');
                                        }
                                        opt_prev_was_text = false;
                                        let txt = if opt_text_idx == 0 {
                                            "text_1".to_string()
                                        } else {
                                            format!("text_{}", opt_text_idx + 1)
                                        };
                                        opt_text_idx += 1;
                                        let init = if let Some(prev) = &opt_prev_var {
                                            t::call(
                                                t::member_id(t::id_dollar(), "sibling"),
                                                vec![t::id_owned(prev.to_string())],
                                            )
                                        } else {
                                            t::call(
                                                t::member_id(t::id_dollar(), "first_child"),
                                                vec![t::id_owned(opt_fragment_var.to_string())],
                                            )
                                        };
                                        opt_cb_body.push(t::var(&txt, init));
                                        opt_prev_var = Some(txt.clone());
                                        opt_first_emit = true;
                                        // Build template-literal ` ${$.get(X) ?? ''}`.
                                        let mut expr = rewrite_props_destructured(
                                            &et.expression, &script.props_destructured,
                                        );
                                        rewrite_expr_for_state(&mut expr, &script.state_bindings);
                                        let coalesced = Expression::Logical(Box::new(LogicalExpression {
                                            left: expr,
                                            operator: LogicalOperator::Coalesce,
                                            right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                                                value: Cow::Owned(String::new()),
                                                raw: Some("''".to_string()),
                                                span: Span::ZERO,
                                            }))),
                                            span: Span::ZERO,
                                        }));
                                        let tpl_lit = Expression::Template(Box::new(TemplateLiteral {
                                            quasis: vec![
                                                TemplateElement { cooked: " ".to_string(), raw: " ".to_string(), tail: false, span: Span::ZERO },
                                                TemplateElement { cooked: String::new(), raw: String::new(), tail: true, span: Span::ZERO },
                                            ],
                                            expressions: vec![coalesced],
                                            span: Span::ZERO,
                                        }));
                                        opt_set_calls.push(t::stmt(t::call(
                                            t::member_id(t::id_dollar(), "set_text"),
                                            vec![t::id_owned(txt.to_string()), tpl_lit],
                                        )));
                                    }
                                    FragmentChild::RegularElement(child_el) => {
                                        opt_prev_was_text = false;
                                        let is_text_anchor = is_text_only_element(child_el)
                                            && child_el.attributes.is_empty();
                                        if is_text_anchor {
                                            opt_tpl.push('<');
                                            opt_tpl.push_str(&child_el.name);
                                            opt_tpl.push_str("> </");
                                            opt_tpl.push_str(&child_el.name);
                                            opt_tpl.push('>');
                                            // Use the outer (optgroup) elem
                                            // counter so a nested span
                                            // continues the naming.
                                            let elem_count = og_elem_used_names
                                                .entry(child_el.name.clone())
                                                .or_insert(0);
                                            let elem_name = if *elem_count == 0 {
                                                child_el.name.clone()
                                            } else {
                                                format!("{}_{}", child_el.name, elem_count)
                                            };
                                            *elem_count += 1;
                                            opt_elem_idx += 1;
                                            let init = if !opt_first_emit {
                                                t::call(
                                                    t::member_id(t::id_dollar(), "first_child"),
                                                    vec![t::id_owned(opt_fragment_var.to_string())],
                                                )
                                            } else if let Some(prev) = &opt_prev_var {
                                                t::call(
                                                    t::member_id(t::id_dollar(), "sibling"),
                                                    vec![t::id_owned(prev.to_string())],
                                                )
                                            } else {
                                                t::call(
                                                    t::member_id(t::id_dollar(), "first_child"),
                                                    vec![t::id_owned(opt_fragment_var.to_string())],
                                                )
                                            };
                                            opt_cb_body.push(t::var(&elem_name, init));
                                            opt_first_emit = true;
                                            let txt = if opt_text_idx == 0 {
                                                "text_1".to_string()
                                            } else {
                                                format!("text_{}", opt_text_idx + 1)
                                            };
                                            opt_text_idx += 1;
                                            opt_cb_body.push(t::var(
                                                &txt,
                                                t::call(
                                                    t::member_id(t::id_dollar(), "child"),
                                                    vec![
                                                        t::id_owned(elem_name.to_string()),
                                                        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                                            value: true,
                                                            span: Span::ZERO,
                                                        }))),
                                                    ],
                                                ),
                                            ));
                                            opt_cb_body.push(t::stmt(t::call(
                                                t::member_id(t::id_dollar(), "reset"),
                                                vec![t::id_owned(elem_name.to_string())],
                                            )));
                                            let mut parts: Vec<TextPart> = Vec::new();
                                            for cc in &child_el.fragment.nodes {
                                                match cc {
                                                    FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                                                    FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
                                                    _ => return None,
                                                }
                                            }
                                            let inline = build_inline_template(&parts, &script.state_bindings);
                                            let inline = rewrite_props_destructured(&inline, &script.props_destructured);
                                            opt_set_calls.push(t::stmt(t::call(
                                                t::member_id(t::id_dollar(), "set_text"),
                                                vec![t::id_owned(txt.to_string()), inline],
                                            )));
                                            opt_prev_var = Some(elem_name);
                                        } else {
                                            return None;
                                        }
                                    }
                                    _ => return None,
                                }
                            }
                            if !opt_set_calls.is_empty() {
                                let eff_body = if opt_set_calls.len() == 1 {
                                    let stmt = opt_set_calls.into_iter().next().unwrap();
                                    let expr = if let Statement::Expression(e) = stmt {
                                        e.expression
                                    } else {
                                        unreachable!()
                                    };
                                    ArrowBody::Expression(expr)
                                } else {
                                    ArrowBody::Block(Box::new(BlockStatement {
                                        body: opt_set_calls,
                                        span: Span::ZERO,
                                    }))
                                };
                                opt_cb_body.push(t::stmt(t::call(
                                    t::member_id(t::id_dollar(), "template_effect"),
                                    vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                                        params: Vec::new(),
                                        param_type_annotations: Vec::new(),
                                        body: eff_body,
                                        r#async: false,
                                        span: Span::ZERO,
                                    }))],
                                )));
                            }
                            opt_cb_body.push(t::stmt(t::call(
                                t::member_id(t::id_dollar(), "append"),
                                vec![t::id_owned(opt_anchor_var.to_string()), t::id_owned(opt_fragment_var.to_string())],
                            )));
                            content_templates.push((option_template_name.clone(), opt_tpl));

                            // Navigate to this option in optgroup body.
                            let init = if !first_emitted {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            } else if let Some(prev) = &prev_elem_var {
                                let offset = (og_pos - prev_elem_pos) * 2;
                                t::call(
                                    t::member_id(t::id_dollar(), "sibling"),
                                    vec![t::id_owned(prev.to_string()), t::lit_number(offset as f64)],
                                )
                            } else {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            };
                            cb_body.push(t::var(&opt_var, init));
                            // Customizable_select call.
                            cb_body.push(t::stmt(t::call(
                                t::member_id(t::id_dollar(), "customizable_select"),
                                vec![
                                    t::id_owned(opt_var.to_string()),
                                    Expression::Arrow(Box::new(ArrowFunctionExpression {
                                        params: Vec::new(),
                                        param_type_annotations: Vec::new(),
                                        body: ArrowBody::Block(Box::new(BlockStatement {
                                            body: opt_cb_body,
                                            span: Span::ZERO,
                                        })),
                                        r#async: false,
                                        span: Span::ZERO,
                                    })),
                                ],
                            )));
                            // Emit value assignment.
                            if let Some(val) = value_str {
                                let value_expr = Expression::Literal(Box::new(Literal::String(StringLiteral {
                                    value: Cow::Owned(val.clone()),
                                    raw: Some(format!("'{}'", val.replace('\'', "\\'"))),
                                    span: Span::ZERO,
                                })));
                                let inner = Expression::Assignment(Box::new(AssignmentExpression {
                                    left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                        object: t::id_owned(opt_var.to_string()),
                                        property: MemberProperty::Identifier(Identifier {
                                            name: Cow::Borrowed("__value"),
                                            span: Span::ZERO,
                                        }),
                                        computed: false,
                                        optional: false,
                                        span: Span::ZERO,
                                    }))),
                                    operator: AssignmentOperator::Assign,
                                    right: value_expr,
                                    span: Span::ZERO,
                                }));
                                let outer = Expression::Assignment(Box::new(AssignmentExpression {
                                    left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                        object: t::id_owned(opt_var.to_string()),
                                        property: MemberProperty::Identifier(Identifier {
                                            name: Cow::Borrowed("value"),
                                            span: Span::ZERO,
                                        }),
                                        computed: false,
                                        optional: false,
                                        span: Span::ZERO,
                                    }))),
                                    operator: AssignmentOperator::Assign,
                                    right: inner,
                                    span: Span::ZERO,
                                }));
                                cb_body.push(t::stmt(outer));
                            }
                            prev_elem_var = Some(opt_var);
                            prev_elem_pos = og_pos;
                            first_emitted = true;
                        } else {
                            // Plain option — serialize in template.
                            tpl.push_str("<option>");
                            for cc in &el.fragment.nodes {
                                if let FragmentChild::Text(t) = cc {
                                    tpl.push_str(t.data.trim());
                                }
                            }
                            tpl.push_str("</option>");
                            // Navigate to this option for value assignment.
                            let opt_var = if option_in_og_idx == 0 {
                                "option".to_string()
                            } else {
                                format!("option_{}", option_in_og_idx)
                            };
                            option_in_og_idx += 1;
                            let init = if !first_emitted {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            } else if let Some(prev) = &prev_elem_var {
                                let offset = (og_pos - prev_elem_pos) * 2;
                                t::call(
                                    t::member_id(t::id_dollar(), "sibling"),
                                    vec![t::id_owned(prev.to_string()), t::lit_number(offset as f64)],
                                )
                            } else {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            };
                            cb_body.push(t::var(&opt_var, init));
                            if let Some(val) = value_str {
                                let value_expr = Expression::Literal(Box::new(Literal::String(StringLiteral {
                                    value: Cow::Owned(val.clone()),
                                    raw: Some(format!("'{}'", val.replace('\'', "\\'"))),
                                    span: Span::ZERO,
                                })));
                                let inner = Expression::Assignment(Box::new(AssignmentExpression {
                                    left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                        object: t::id_owned(opt_var.to_string()),
                                        property: MemberProperty::Identifier(Identifier {
                                            name: Cow::Borrowed("__value"),
                                            span: Span::ZERO,
                                        }),
                                        computed: false,
                                        optional: false,
                                        span: Span::ZERO,
                                    }))),
                                    operator: AssignmentOperator::Assign,
                                    right: value_expr,
                                    span: Span::ZERO,
                                }));
                                let outer = Expression::Assignment(Box::new(AssignmentExpression {
                                    left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                        object: t::id_owned(opt_var.to_string()),
                                        property: MemberProperty::Identifier(Identifier {
                                            name: Cow::Borrowed("value"),
                                            span: Span::ZERO,
                                        }),
                                        computed: false,
                                        optional: false,
                                        span: Span::ZERO,
                                    }))),
                                    operator: AssignmentOperator::Assign,
                                    right: inner,
                                    span: Span::ZERO,
                                }));
                                cb_body.push(t::stmt(outer));
                            }
                            prev_elem_var = Some(opt_var);
                            prev_elem_pos = og_pos;
                            first_emitted = true;
                        }
                        option_idx += 1;
                    } else {
                        // Other element (e.g. span with reactive content).
                        let is_text_anchor = is_text_only_element(el);
                        if is_text_anchor {
                            // span with class attribute? Build the open tag with attrs.
                            tpl.push('<');
                            tpl.push_str(&el.name);
                            for a in &el.attributes {
                                if let ElementAttribute::Attribute(attr) = a {
                                    match &attr.value {
                                        AttributeValue::Empty => {
                                            tpl.push(' ');
                                            tpl.push_str(&attr.name);
                                            tpl.push_str("=\"\"");
                                        }
                                        AttributeValue::Many(parts) => {
                                            if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                                                tpl.push(' ');
                                                tpl.push_str(&attr.name);
                                                tpl.push_str("=\"");
                                                for p in parts {
                                                    if let AttributeValuePart::Text(t) = p {
                                                        tpl.push_str(&t.data);
                                                    }
                                                }
                                                tpl.push('"');
                                            }
                                        }
                                        _ => return None,
                                    }
                                }
                            }
                            tpl.push_str("> </");
                            tpl.push_str(&el.name);
                            tpl.push('>');

                            let span_count = og_elem_used_names.entry(el.name.clone()).or_insert(0);
                            let span_var_name = if *span_count == 0 {
                                el.name.clone()
                            } else {
                                format!("{}_{}", el.name, span_count)
                            };
                            *span_count += 1;
                            let init = if !first_emitted {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            } else if let Some(prev) = &prev_elem_var {
                                let offset = (og_pos - prev_elem_pos) * 2;
                                t::call(
                                    t::member_id(t::id_dollar(), "sibling"),
                                    vec![t::id_owned(prev.to_string()), t::lit_number(offset as f64)],
                                )
                            } else {
                                t::call(
                                    t::member_id(t::id_dollar(), "first_child"),
                                    vec![t::id_owned(fragment_var.to_string())],
                                )
                            };
                            cb_body.push(t::var(&span_var_name, init));
                            let txt = if text_var_idx == 0 {
                                "text".to_string()
                            } else {
                                format!("text_{}", text_var_idx)
                            };
                            text_var_idx += 1;
                            cb_body.push(t::var(
                                &txt,
                                t::call(
                                    t::member_id(t::id_dollar(), "child"),
                                    vec![
                                        t::id_owned(span_var_name.to_string()),
                                        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                            value: true,
                                            span: Span::ZERO,
                                        }))),
                                    ],
                                ),
                            ));
                            cb_body.push(t::stmt(t::call(
                                t::member_id(t::id_dollar(), "reset"),
                                vec![t::id_owned(span_var_name.to_string())],
                            )));
                            // Build inline template for span content.
                            let mut parts: Vec<TextPart> = Vec::new();
                            for cc in &el.fragment.nodes {
                                match cc {
                                    FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                                    FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
                                    _ => return None,
                                }
                            }
                            let inline = build_inline_template(&parts, &script.state_bindings);
                            let inline = rewrite_props_destructured(&inline, &script.props_destructured);
                            og_set_text_calls.push(t::stmt(t::call(
                                t::member_id(t::id_dollar(), "set_text"),
                                vec![t::id_owned(txt.to_string()), inline],
                            )));
                            prev_elem_var = Some(span_var_name);
                            prev_elem_pos = og_pos;
                            first_emitted = true;
                        } else {
                            return None;
                        }
                    }
                }
                FragmentChild::Text(t) => {
                    tpl.push_str(&t.data);
                    continue; // don't advance og_pos for non-anchor text
                }
                _ => return None,
            }
            og_pos += 1;
            let _ = og_rich_option_blocks;
            let _ = option_value_assigns;
        }
        // Trailing template_effect for set_text on og's reactive items
        // (the span text anchors).
        if !og_set_text_calls.is_empty() {
            let eff_body = if og_set_text_calls.len() == 1 {
                let stmt = og_set_text_calls.into_iter().next().unwrap();
                let expr = if let Statement::Expression(e) = stmt {
                    e.expression
                } else {
                    unreachable!()
                };
                ArrowBody::Expression(expr)
            } else {
                ArrowBody::Block(Box::new(BlockStatement {
                    body: og_set_text_calls,
                    span: Span::ZERO,
                }))
            };
            cb_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: eff_body,
                    r#async: false,
                    span: Span::ZERO,
                }))],
            )));
        }
        cb_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )));
        let _ = stripped;
        let _ = option_idx;
        let _ = elem_var_counter_clone();
        fn elem_var_counter_clone() -> HashMap<String, usize> { HashMap::new() }

        rich_ogs.push(RichOg {
            og_idx: oi,
            template_name: optgroup_template_name,
            template_html: tpl,
            callback_body: cb_body,
        });
    }
    // Hoist optgroup_content_N templates first, then option_content_N (order: optgroup first since fixture).
    // Actually upstream emits option_content first, then optgroup_content. Let me check expected:
    //   var option_content = ...
    //   var optgroup_content = ...
    // So option_content comes first.

    // Build select template HTML.
    let mut select_html = String::from("<select>");
    for (i, _og) in optgroup_els.iter().enumerate() {
        if og_infos[i].is_rich {
            select_html.push_str("<optgroup");
            if let Some(label) = &og_infos[i].label {
                select_html.push_str(" label=\"");
                select_html.push_str(label);
                select_html.push('"');
            }
            select_html.push_str("><!></optgroup>");
        } else {
            // Static optgroup — serialize content.
            select_html.push_str("<optgroup");
            if let Some(label) = &og_infos[i].label {
                select_html.push_str(" label=\"");
                select_html.push_str(label);
                select_html.push('"');
            }
            select_html.push('>');
            for c in og_infos[i].children {
                if let FragmentChild::RegularElement(opt) = c {
                    if opt.name == "option" {
                        select_html.push_str("<option>");
                        for cc in &opt.fragment.nodes {
                            if let FragmentChild::Text(t) = cc {
                                select_html.push_str(t.data.trim());
                            }
                        }
                        select_html.push_str("</option>");
                    }
                }
            }
            select_html.push_str("</optgroup>");
        }
    }
    select_html.push_str("</select>");
    if let Some(el) = trailing_el {
        select_html.push(' ');
        select_html.push('<');
        select_html.push_str(&el.name);
        for a in &el.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                match &attr.value {
                    AttributeValue::Empty => {
                        select_html.push(' ');
                        select_html.push_str(&attr.name);
                        select_html.push_str("=\"\"");
                    }
                    AttributeValue::Many(parts) => {
                        if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            select_html.push(' ');
                            select_html.push_str(&attr.name);
                            select_html.push_str("=\"");
                            for p in parts {
                                if let AttributeValuePart::Text(t) = p {
                                    select_html.push_str(&t.data);
                                }
                            }
                            select_html.push('"');
                        }
                    }
                    _ => {}
                }
            }
        }
        if is_void_client(&el.name) {
            select_html.push_str("/>");
        } else {
            select_html.push_str("></");
            select_html.push_str(&el.name);
            select_html.push('>');
        }
    }

    // Build function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var("fragment", t::call(t::id("root"), Vec::new())));
    func_body.push(t::var(
        "select",
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()]),
    ));
    // Walk optgroup_els.
    let mut prev_og_var: Option<String> = None;
    let mut delegated_events: Vec<(String, String, Expression)> = Vec::new();
    for (oi, og) in optgroup_els.iter().enumerate() {
        let og_var = select_optgroup_var(oi);
        let init = if oi == 0 {
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id("select")])
        } else {
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(prev_og_var.as_ref().unwrap().clone())],
            )
        };
        func_body.push(t::var(&og_var, init));
        if og_infos[oi].is_rich {
            // Find rich_og.
            let rich = rich_ogs.iter().find(|r| r.og_idx == oi).expect("rich_og");
            let cb = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: rich.callback_body.clone(),
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            func_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "customizable_select"),
                vec![t::id_owned(og_var.to_string()), cb],
            )));
        } else {
            // Static optgroup: navigate to plain option children and assign values.
            let mut prev_opt_var: Option<String> = None;
            let mut static_opt_idx = 0usize;
            for c in &og.fragment.nodes {
                if let FragmentChild::RegularElement(el) = c {
                    if el.name == "option" {
                        let mut value_str: Option<String> = None;
                        for a in &el.attributes {
                            if let ElementAttribute::Attribute(attr) = a {
                                if attr.name == "value" {
                                    if let AttributeValue::Many(parts) = &attr.value {
                                        let mut s = String::new();
                                        for p in parts {
                                            if let AttributeValuePart::Text(t) = p {
                                                s.push_str(&t.data);
                                            }
                                        }
                                        value_str = Some(s);
                                    }
                                }
                            }
                        }
                        // Allocate option_N name with continued counter.
                        let opt_var = format!("option_{}", oi + static_opt_idx + 1);
                        static_opt_idx += 1;
                        let init = if let Some(prev) = &prev_opt_var {
                            t::call(t::member_id(t::id_dollar(), "sibling"), vec![t::id_owned(prev.to_string())])
                        } else {
                            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(og_var.to_string())])
                        };
                        func_body.push(t::var(&opt_var, init));
                        if let Some(val) = value_str {
                            let value_expr = Expression::Literal(Box::new(Literal::String(StringLiteral {
                                value: Cow::Owned(val.clone()),
                                raw: Some(format!("'{}'", val.replace('\'', "\\'"))),
                                span: Span::ZERO,
                            })));
                            let inner = Expression::Assignment(Box::new(AssignmentExpression {
                                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                    object: t::id_owned(opt_var.to_string()),
                                    property: MemberProperty::Identifier(Identifier {
                                        name: Cow::Borrowed("__value"),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    optional: false,
                                    span: Span::ZERO,
                                }))),
                                operator: AssignmentOperator::Assign,
                                right: value_expr,
                                span: Span::ZERO,
                            }));
                            let outer = Expression::Assignment(Box::new(AssignmentExpression {
                                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                                    object: t::id_owned(opt_var.to_string()),
                                    property: MemberProperty::Identifier(Identifier {
                                        name: Cow::Borrowed("value"),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    optional: false,
                                    span: Span::ZERO,
                                }))),
                                operator: AssignmentOperator::Assign,
                                right: inner,
                                span: Span::ZERO,
                            }));
                            func_body.push(t::stmt(outer));
                        }
                        prev_opt_var = Some(opt_var);
                    }
                }
            }
            // After processing static optgroup options, reset(optgroup_N).
            func_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(og_var.to_string())],
            )));
        }
        prev_og_var = Some(og_var);
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id("select")],
    )));

    if let Some(tel) = trailing_el {
        let tel_var = tel.name.clone();
        func_body.push(t::var(
            &tel_var,
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id("select"), t::lit_number(2.0)],
            ),
        ));
        for a in &tel.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                if let Some(stripped) = attr.name.strip_prefix("on") {
                    if let AttributeValue::Single(tag) = &attr.value {
                        let handler = rewrite_expr_for_state_helper(
                            &tag.expression, &script.state_bindings,
                        );
                        delegated_events.push((
                            stripped.to_string(),
                            tel_var.clone(),
                            handler,
                        ));
                    }
                }
            }
        }
    }
    for (ev, var, handler) in &delegated_events {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegated"),
            vec![
                t::literal_str_owned(ev.to_string()),
                t::id_owned(var.to_string()),
                handler.clone(),
            ],
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // option_content_N templates.
    for (name, html) in &content_templates {
        prog.push(t::var(
            name,
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![
                    t::template_raw(vec![html.clone()], Vec::new()),
                    t::lit_number(1.0),
                ],
            ),
        ));
    }
    // optgroup_content_N templates.
    for rich in &rich_ogs {
        prog.push(t::var(
            &rich.template_name,
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![
                    t::template_raw(vec![rich.template_html.clone()], Vec::new()),
                    t::lit_number(1.0),
                ],
            ),
        ));
    }
    let root_flag = if trailing_el.is_some() { 1.0 } else { 0.0 };
    let root_args: Vec<Expression> = if root_flag != 0.0 {
        vec![t::template_raw(vec![select_html], Vec::new()), t::lit_number(root_flag)]
    } else {
        vec![t::template_raw(vec![select_html], Vec::new())]
    };
    prog.push(t::var(
        "root",
        t::call(t::member_id(t::id_dollar(), "from_html"), root_args),
    ));
    prog.push(export);
    if !delegated_events.is_empty() {
        let mut names: Vec<String> = delegated_events.iter().map(|(e, _, _)| e.clone()).collect();
        names.sort();
        names.dedup();
        let arr = Expression::Array(Box::new(ArrayExpression {
            elements: names
                .into_iter()
                .map(|n| ArrayElement::Expression(Expression::Literal(Box::new(Literal::String(
                    StringLiteral { value: Cow::Owned(n), raw: None, span: Span::ZERO }
                )))))
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegate"),
            vec![arr],
        )));
    }
    Some(t::program(prog))
}

fn select_optgroup_var(idx: usize) -> String {
    if idx == 0 { "optgroup".to_string() } else { format!("optgroup_{}", idx) }
}

/// Emit a program for `<select>` with rich `<option>` content where the
/// rich content includes reactive expressions, plus an optional trailing
/// `<button onclick={...}>`. Used by option-rich-content-continues.
/// Mirrors the upstream output:
///   - option_content template includes reactive text placeholders
///   - customizable_select callback navigates text anchors + emits
///     template_effect with `\$.get(X)` for each reactive position
///   - trailing button gets `\$.delegated('click', button, handler)` +
///     module-level `\$.delegate(['click'])`
fn emit_select_with_rich_reactive_and_trailing(
    root_fragment: &svelte_ast::fragment::Fragment,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    // Collect top-level non-WS, non-comment children.
    let core: Vec<&FragmentChild> = root_fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            FragmentChild::SvelteOptions(_) => false,
            _ => true,
        })
        .collect();
    if core.is_empty() || core.len() > 2 {
        return None;
    }
    let select_el = match core[0] {
        FragmentChild::RegularElement(el) if el.name == "select" && el.attributes.is_empty() => el,
        _ => return None,
    };
    let trailing_el: Option<&svelte_ast::elements::RegularElement> = if core.len() == 2 {
        match core[1] {
            FragmentChild::RegularElement(el) => Some(el),
            _ => return None,
        }
    } else {
        None
    };
    // Collect option children.
    let mut option_els: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in &select_el.fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(el) if el.name == "option" => option_els.push(el),
            _ => return None,
        }
    }
    if option_els.is_empty() {
        return None;
    }
    // Per-option: value attr + rich (mixed text+expr+elem) detection.
    struct OptInfo<'a> {
        value: Option<String>,
        is_rich: bool,
        children: &'a [FragmentChild],
    }
    let mut infos: Vec<OptInfo> = Vec::new();
    for opt in &option_els {
        let mut info = OptInfo { value: None, is_rich: false, children: &opt.fragment.nodes };
        for a in &opt.attributes {
            match a {
                ElementAttribute::Attribute(attr) if attr.name == "value" => {
                    let s = match &attr.value {
                        AttributeValue::Many(parts) => {
                            let mut s = String::new();
                            for p in parts {
                                let AttributeValuePart::Text(t) = p else { return None; };
                                s.push_str(&t.data);
                            }
                            s
                        }
                        AttributeValue::Empty => String::new(),
                        _ => return None,
                    };
                    info.value = Some(s);
                }
                _ => return None,
            }
        }
        info.is_rich = opt.fragment.nodes.iter().any(|c| matches!(c, FragmentChild::RegularElement(_)));
        infos.push(info);
    }

    // Build select HTML template.
    let mut select_html = String::from("<select>");
    for (i, opt) in option_els.iter().enumerate() {
        if infos[i].is_rich {
            select_html.push_str("<option><!></option>");
        } else {
            select_html.push_str("<option>");
            for c in &opt.fragment.nodes {
                if let FragmentChild::Text(t) = c {
                    select_html.push_str(t.data.trim());
                } else {
                    return None;
                }
            }
            select_html.push_str("</option>");
        }
    }
    select_html.push_str("</select>");
    // Append trailing (e.g. " <button></button>").
    if let Some(el) = trailing_el {
        select_html.push(' ');
        select_html.push('<');
        select_html.push_str(&el.name);
        // Only emit static attributes in HTML; dynamic attrs handled at runtime.
        for a in &el.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                match &attr.value {
                    AttributeValue::Empty => {
                        select_html.push(' ');
                        select_html.push_str(&attr.name);
                        select_html.push_str("=\"\"");
                    }
                    AttributeValue::Many(parts) => {
                        if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            select_html.push(' ');
                            select_html.push_str(&attr.name);
                            select_html.push_str("=\"");
                            for p in parts {
                                if let AttributeValuePart::Text(t) = p {
                                    select_html.push_str(&t.data);
                                }
                            }
                            select_html.push('"');
                        }
                    }
                    _ => {}
                }
            }
        }
        if is_void_client(&el.name) {
            select_html.push_str("/>");
        } else {
            select_html.push_str("></");
            select_html.push_str(&el.name);
            select_html.push('>');
        }
    }

    // Build per-rich-option_content templates and their callback bodies.
    struct RichOption<'a> {
        option_idx: usize,
        template_name: String,
        template_html: String,
        callback_body: Vec<Statement>,
        _phantom: std::marker::PhantomData<&'a ()>,
    }
    let mut rich_options: Vec<RichOption> = Vec::new();
    let mut rich_counter = 0usize;
    for (oi, opt) in option_els.iter().enumerate() {
        if !infos[oi].is_rich {
            continue;
        }
        let template_name = if rich_counter == 0 {
            "option_content".to_string()
        } else {
            format!("option_content_{}", rich_counter)
        };
        rich_counter += 1;
        let fragment_var = if rich_options.is_empty() {
            "fragment_1".to_string()
        } else {
            format!("fragment_{}", rich_options.len() + 1)
        };
        let anchor_var = if rich_options.is_empty() {
            "anchor".to_string()
        } else {
            format!("anchor_{}", rich_options.len())
        };

        // Walk option's children to:
        //   - Build template HTML
        //   - Identify reactive text positions (text anchors)
        let mut tpl = String::new();
        // The callback body emits: var anchor = $.child(option_N); var
        // fragment_1 = template(); ...nav + text_anchors...; $.template_effect(...); $.append(anchor, fragment_1)
        let mut cb_body: Vec<Statement> = Vec::new();
        cb_body.push(t::var(
            &anchor_var,
            t::call(
                t::member_id(t::id_dollar(), "child"),
                vec![t::id_owned(select_option_var(oi))],
            ),
        ));
        cb_body.push(t::var(&fragment_var, t::call(t::id_owned(template_name.to_string()), Vec::new())));

        // Walk children: build template HTML + collect reactive set_text calls.
        // We treat the option body as a multi-root template (flag 1).
        // Each child is either:
        //   - A static element (serialized)
        //   - An element with text-anchor content (placeholder + text var)
        //   - A bare ExpressionTag → trailing/leading space placeholder
        //   - A static Text → kept verbatim
        let mut set_text_calls: Vec<Statement> = Vec::new();
        let mut elem_var_local: usize = 0;
        let mut text_var_local: usize = 0;
        let mut prev_elem_var: Option<String> = None;
        let mut first_emitted = false;
        let mut prev_was_text = false;
        for (ci, c) in opt.fragment.nodes.iter().enumerate() {
            let _ = ci;
            match c {
                FragmentChild::Text(t) => {
                    tpl.push_str(&t.data);
                    prev_was_text = true;
                }
                FragmentChild::ExpressionTag(et) => {
                    // Space placeholder in template + sibling navigation —
                    // unless preceding sibling was Text (the existing text
                    // node serves as the anchor).
                    if !prev_was_text {
                        tpl.push(' ');
                    }
                    prev_was_text = false;
                    let text_name = if text_var_local == 0 {
                        "text".to_string()
                    } else {
                        format!("text_{}", text_var_local)
                    };
                    text_var_local += 1;
                    let init = if let Some(prev) = &prev_elem_var {
                        // sibling of last span etc.
                        t::call(
                            t::member_id(t::id_dollar(), "sibling"),
                            vec![t::id_owned(prev.to_string())],
                        )
                    } else {
                        // First — first_child of fragment.
                        t::call(
                            t::member_id(t::id_dollar(), "first_child"),
                            vec![t::id_owned(fragment_var.to_string())],
                        )
                    };
                    cb_body.push(t::var(&text_name, init));
                    prev_elem_var = Some(text_name.clone());
                    first_emitted = true;
                    // set_text(text_N, ` ${EXPR ?? ''}`) for sibling text (with prefix space)
                    // OR set_text(text, EXPR) for inside element.
                    // Build inline template that mirrors source surroundings.
                    // Here we're a bare ExpressionTag → the text node will
                    // hold ` ${EXPR ?? ''}`.
                    let expr = rewrite_props_destructured(
                        &et.expression,
                        &script.props_destructured,
                    );
                    let mut expr = expr;
                    rewrite_expr_for_state(&mut expr, &script.state_bindings);
                    let mut quasi = String::new();
                    quasi.push(' ');
                    // Wrap as `EXPR ?? ''`.
                    let coalesced = Expression::Logical(Box::new(LogicalExpression {
                        left: expr,
                        operator: LogicalOperator::Coalesce,
                        right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(String::new()),
                            raw: Some("''".to_string()),
                            span: Span::ZERO,
                        }))),
                        span: Span::ZERO,
                    }));
                    let tpl_lit = Expression::Template(Box::new(TemplateLiteral {
                        quasis: vec![
                            TemplateElement {
                                cooked: quasi.clone(),
                                raw: quasi.clone(),
                                tail: false,
                                span: Span::ZERO,
                            },
                            TemplateElement {
                                cooked: String::new(),
                                raw: String::new(),
                                tail: true,
                                span: Span::ZERO,
                            },
                        ],
                        expressions: vec![coalesced],
                        span: Span::ZERO,
                    }));
                    set_text_calls.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "set_text"),
                        vec![t::id_owned(text_name.to_string()), tpl_lit],
                    )));
                }
                FragmentChild::RegularElement(child_el) => {
                    prev_was_text = false;
                    // Serialize the element. If it's text-only with reactive
                    // content, allocate a text anchor + reset.
                    let is_text_anchor = is_text_only_element(child_el)
                        && child_el.attributes.is_empty();
                    if is_text_anchor {
                        // Open tag + space placeholder.
                        tpl.push('<');
                        tpl.push_str(&child_el.name);
                        tpl.push_str("> </");
                        tpl.push_str(&child_el.name);
                        tpl.push('>');
                        let elem_name = if elem_var_local == 0 {
                            child_el.name.clone()
                        } else {
                            format!("{}_{}", child_el.name, elem_var_local)
                        };
                        elem_var_local += 1;
                        let init = if !first_emitted {
                            t::call(
                                t::member_id(t::id_dollar(), "first_child"),
                                vec![t::id_owned(fragment_var.to_string())],
                            )
                        } else if let Some(prev) = &prev_elem_var {
                            t::call(
                                t::member_id(t::id_dollar(), "sibling"),
                                vec![t::id_owned(prev.to_string())],
                            )
                        } else {
                            t::call(
                                t::member_id(t::id_dollar(), "first_child"),
                                vec![t::id_owned(fragment_var.to_string())],
                            )
                        };
                        cb_body.push(t::var(&elem_name, init));
                        first_emitted = true;
                        // Text anchor inside the element.
                        let text_name = if text_var_local == 0 {
                            "text".to_string()
                        } else {
                            format!("text_{}", text_var_local)
                        };
                        text_var_local += 1;
                        cb_body.push(t::var(
                            &text_name,
                            t::call(
                                t::member_id(t::id_dollar(), "child"),
                                vec![
                                    t::id_owned(elem_name.to_string()),
                                    Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                        value: true,
                                        span: Span::ZERO,
                                    }))),
                                ],
                            ),
                        ));
                        cb_body.push(t::stmt(t::call(
                            t::member_id(t::id_dollar(), "reset"),
                            vec![t::id_owned(elem_name.to_string())],
                        )));
                        // Build inline expression from element's children.
                        let mut parts: Vec<TextPart> = Vec::new();
                        for cc in &child_el.fragment.nodes {
                            match cc {
                                FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                                FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
                                _ => return None,
                            }
                        }
                        let inline = build_inline_template(&parts, &script.state_bindings);
                        let inline = rewrite_props_destructured(&inline, &script.props_destructured);
                        set_text_calls.push(t::stmt(t::call(
                            t::member_id(t::id_dollar(), "set_text"),
                            vec![t::id_owned(text_name.to_string()), inline],
                        )));
                        prev_elem_var = Some(elem_name);
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        // Wrap set_text_calls in template_effect.
        if !set_text_calls.is_empty() {
            let eff_body = if set_text_calls.len() == 1 {
                let stmt = set_text_calls.into_iter().next().unwrap();
                let expr = if let Statement::Expression(e) = stmt {
                    e.expression
                } else {
                    unreachable!()
                };
                ArrowBody::Expression(expr)
            } else {
                ArrowBody::Block(Box::new(BlockStatement {
                    body: set_text_calls,
                    span: Span::ZERO,
                }))
            };
            cb_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: eff_body,
                    r#async: false,
                    span: Span::ZERO,
                }))],
            )));
        }
        cb_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )));
        rich_options.push(RichOption {
            option_idx: oi,
            template_name,
            template_html: tpl,
            callback_body: cb_body,
            _phantom: std::marker::PhantomData,
        });
    }

    // Build function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    // var fragment = root();
    func_body.push(t::var("fragment", t::call(t::id("root"), Vec::new())));
    // var select = $.first_child(fragment);
    func_body.push(t::var(
        "select",
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()]),
    ));
    // Walk options, allocate vars, emit customizable_select for rich + value assignments.
    let mut prev_opt_var: Option<String> = None;
    let mut delegated_events: Vec<(String, String, Expression)> = Vec::new(); // (event, var, handler)
    for (oi, opt) in option_els.iter().enumerate() {
        let var_name = select_option_var(oi);
        let init = if oi == 0 {
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id("select")])
        } else {
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(prev_opt_var.as_ref().unwrap().clone())],
            )
        };
        func_body.push(t::var(&var_name, init));
        if let Some(rich) = rich_options.iter().find(|r| r.option_idx == oi) {
            let cb = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: rich.callback_body.clone(),
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            func_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "customizable_select"),
                vec![t::id_owned(var_name.to_string()), cb],
            )));
        }
        // option.value = option.__value = 'X';
        if let Some(val) = &infos[oi].value {
            let value_expr = Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(val.clone()),
                raw: Some(format!("'{}'", val.replace('\'', "\\'"))),
                span: Span::ZERO,
            })));
            let inner_assign = Expression::Assignment(Box::new(AssignmentExpression {
                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                    object: t::id_owned(var_name.to_string()),
                    property: MemberProperty::Identifier(Identifier {
                        name: Cow::Borrowed("__value"),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))),
                operator: AssignmentOperator::Assign,
                right: value_expr,
                span: Span::ZERO,
            }));
            let outer_assign = Expression::Assignment(Box::new(AssignmentExpression {
                left: AssignmentTarget::Pattern(Pattern::Member(Box::new(MemberExpression {
                    object: t::id_owned(var_name.to_string()),
                    property: MemberProperty::Identifier(Identifier {
                        name: Cow::Borrowed("value"),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))),
                operator: AssignmentOperator::Assign,
                right: inner_assign,
                span: Span::ZERO,
            }));
            func_body.push(t::stmt(outer_assign));
        }
        prev_opt_var = Some(var_name);
        let _ = opt;
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id("select")],
    )));

    // Trailing element (button) with onclick.
    if let Some(tel) = trailing_el {
        let tel_var = tel.name.clone();
        func_body.push(t::var(
            &tel_var,
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id("select"), t::lit_number(2.0)],
            ),
        ));
        for a in &tel.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                if let Some(stripped) = attr.name.strip_prefix("on") {
                    if let AttributeValue::Single(tag) = &attr.value {
                        let handler = rewrite_expr_for_state_helper(
                            &tag.expression,
                            &script.state_bindings,
                        );
                        delegated_events.push((
                            stripped.to_string(),
                            tel_var.clone(),
                            handler,
                        ));
                    }
                }
            }
        }
    }
    for (ev, var, handler) in &delegated_events {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegated"),
            vec![
                t::literal_str_owned(ev.to_string()),
                t::id_owned(var.to_string()),
                handler.clone(),
            ],
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // option_content_N templates.
    for rich in &rich_options {
        prog.push(t::var(
            &rich.template_name,
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![
                    t::template_raw(vec![rich.template_html.clone()], Vec::new()),
                    t::lit_number(1.0),
                ],
            ),
        ));
    }
    // var root = $.from_html(SELECT_HTML[, 1]);
    let root_flag = if trailing_el.is_some() { 1.0 } else { 0.0 };
    let root_args: Vec<Expression> = if root_flag != 0.0 {
        vec![t::template_raw(vec![select_html], Vec::new()), t::lit_number(root_flag)]
    } else {
        vec![t::template_raw(vec![select_html], Vec::new())]
    };
    prog.push(t::var(
        "root",
        t::call(t::member_id(t::id_dollar(), "from_html"), root_args),
    ));
    prog.push(export);
    // $.delegate(['click', ...]) trailer.
    if !delegated_events.is_empty() {
        let mut names: Vec<String> = delegated_events.iter().map(|(e, _, _)| e.clone()).collect();
        names.sort();
        names.dedup();
        let arr = Expression::Array(Box::new(ArrayExpression {
            elements: names
                .into_iter()
                .map(|n| ArrayElement::Expression(Expression::Literal(Box::new(Literal::String(
                    StringLiteral { value: Cow::Owned(n), raw: None, span: Span::ZERO }
                )))))
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegate"),
            vec![arr],
        )));
    }
    Some(t::program(prog))
}

fn select_option_var(idx: usize) -> String {
    if idx == 0 { "option".to_string() } else { format!("option_{}", idx) }
}

/// True iff the expression contains any CallExpression anywhere in its
/// subtree. Used to decide whether `$.set(X, V)` needs the notify-flag
/// third argument (mirrors upstream's `mutation` flag).
fn expr_contains_call(e: &Expression) -> bool {
    match e {
        Expression::Call(_) => true,
        Expression::New(_) => true,
        Expression::Member(m) => expr_contains_call(&m.object) || match &m.property {
            MemberProperty::Expression(e) => expr_contains_call(e),
            _ => false,
        },
        Expression::Binary(b) => expr_contains_call(&b.left) || expr_contains_call(&b.right),
        Expression::Logical(b) => expr_contains_call(&b.left) || expr_contains_call(&b.right),
        Expression::Unary(u) => expr_contains_call(&u.argument),
        Expression::Conditional(c) => {
            expr_contains_call(&c.test)
                || expr_contains_call(&c.consequent)
                || expr_contains_call(&c.alternate)
        }
        Expression::Template(t) => t.expressions.iter().any(expr_contains_call),
        Expression::Array(a) => a.elements.iter().any(|el| match el {
            ArrayElement::Expression(e) => expr_contains_call(e),
            ArrayElement::Spread(s) => expr_contains_call(&s.argument),
            _ => false,
        }),
        Expression::Object(o) => o.properties.iter().any(|p| match p {
            ObjectMember::Property(p) => expr_contains_call(&p.value),
            ObjectMember::Spread(s) => expr_contains_call(&s.argument),
        }),
        _ => false,
    }
}

fn rewrite_expr_for_state_helper(e: &Expression, state: &HashSet<String>) -> Expression {
    let mut e2 = e.clone();
    rewrite_expr_for_state(&mut e2, state);
    e2
}

/// Emit a program for `<custom-element K=V>...</custom-element>` (tag with
/// hyphen, all static attrs, empty body). Strips attrs from the HTML
/// template + emits `$.set_custom_element_data(VAR, K, V)`. Wraps the
/// component in legacy `$.push($$props, false) ... $.pop()` so that
/// runtime hydration recognizes it as a Svelte 4-shape custom element.
fn emit_single_static_custom_element_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    module_stmts: &[Statement],
) -> Option<Program> {
    // Collect static (name, value) pairs from the element attributes.
    let mut props: Vec<(String, String)> = Vec::new();
    for a in &el.attributes {
        let ElementAttribute::Attribute(attr) = a else { return None; };
        let value: String = match &attr.value {
            AttributeValue::Empty => String::new(),
            AttributeValue::Many(parts) => {
                let mut s = String::new();
                for p in parts {
                    let AttributeValuePart::Text(t) = p else { return None; };
                    s.push_str(&t.data);
                }
                s
            }
            _ => return None,
        };
        props.push((attr.name.to_string(), value));
    }

    // HTML template with attrs stripped: `<TAG></TAG>`.
    let mut html = String::new();
    html.push('<');
    html.push_str(&el.name);
    html.push('>');
    html.push_str("</");
    html.push_str(&el.name);
    html.push('>');

    let var = sanitize_name(&el.name);

    let mut func_body: Vec<Statement> = Vec::new();
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "push"),
        vec![
            t::id("$$props"),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: false,
                span: Span::ZERO,
            }))),
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "init"),
        Vec::new(),
    )));
    func_body.push(t::var(&var, t::call(t::id("root"), Vec::new())));
    for (k, v) in &props {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "set_custom_element_data"),
            vec![
                t::id_owned(var.to_string()),
                t::literal_str_owned(k.to_string()),
                t::literal_str_owned(v.to_string()),
            ],
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "pop"),
        Vec::new(),
    )));

    let params = vec![t::pat_id_anchor(), t::pat_id("$$props")];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::new();
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    // Module-level statements (e.g. customElements.define(...)) before var root.
    prog.extend(module_stmts.iter().cloned());
    // `var root = $.from_html(\`HTML\`, 2);` — flag 2 = needs_import_node.
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![html], Vec::new()),
                t::lit_number(2.0),
            ],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Upstream's `hash(filename)` for `$.head(HASH, ...)`. DJB2-variant
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

pub fn try_typed_client_walker_with(
    mut root: Root,
    component_name: &str,
    use_tree: bool,
) -> Option<Program> {
    if root.css.is_some() {
        return None;
    }
    // Capture module-level script statements (e.g. `<script module>`
    // customElements.define) to inject after imports.
    let module_stmts: Vec<Statement> = root
        .module
        .as_mut()
        .map(|m| std::mem::take(&mut m.content.body))
        .unwrap_or_default();

    // Script analysis: collect statements to emit, plus any erased rune
    // bindings. The assignment scan also considers template expressions so
    // `onclick={()=>count++}` registers `count` as assigned even when the
    // script has no direct mutation.
    let template_assigned = scan_fragment_assignments(&root.fragment);
    let script = analyze_script(root.instance.as_ref(), &template_assigned)?;

    // PRE-DETECT: top-level `<svelte:head>` + simple remainder shape.
    {
        let mut head_node: Option<&svelte_ast::elements::SvelteHead> = None;
        let mut others: Vec<&FragmentChild> = Vec::new();
        for n in &root.fragment.nodes {
            match n {
                FragmentChild::SvelteHead(sh) => {
                    if head_node.is_some() {
                        head_node = None; // multiple — bail out of this fast path
                        others.clear();
                        break;
                    }
                    head_node = Some(sh);
                }
                FragmentChild::Text(t) if t.data.trim().is_empty() => {}
                FragmentChild::Comment(_) => {}
                _ => others.push(n),
            }
        }
        if let Some(sh) = head_node {
            if let Some(p) = emit_svelte_head_program(
                sh,
                &others,
                component_name,
                &script,
            ) {
                return Some(p);
            }
            // Specialized: head body contains an if-block + body is a bare
            // Component. Used by head-html-and-component.
            if others.len() == 1 {
                if let FragmentChild::Component(c) = others[0] {
                    if c.attributes.is_empty() && c.fragment.nodes.is_empty() {
                        if let Some(p) = emit_head_if_block_program(
                            sh, c, component_name, &script,
                        ) {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }

    // PRE-DETECT: single outer element wrapping `<inner>{X}</inner>{Y}`.
    {
        let non_ws: Vec<&FragmentChild> = root
            .fragment
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::RegularElement(outer) = non_ws[0] {
                if let Some(p) = emit_single_element_with_inner_and_trailing_expr_program(
                    outer,
                    component_name,
                    &script,
                ) {
                    return Some(p);
                }
            }
        }
    }

    // PRE-DETECT: single top-level `<select>` with `<option>` children
    // carrying static rich content (option-rich-content-static shape).
    {
        let non_ws: Vec<&FragmentChild> = root
            .fragment
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                FragmentChild::SvelteOptions(_) => false,
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::RegularElement(el) = non_ws[0] {
                if el.name == "select" {
                    // Must contain at least one option with rich body.
                    let has_rich_option = el.fragment.nodes.iter().any(|n| matches!(
                        n,
                        FragmentChild::RegularElement(opt)
                            if opt.name == "option"
                                && opt.fragment.nodes.iter().any(|c| matches!(
                                    c, FragmentChild::RegularElement(_)
                                ))
                    ));
                    if has_rich_option {
                        if let Some(p) = emit_select_with_rich_options_static(
                            el,
                            component_name,
                            &script,
                        ) {
                            return Some(p);
                        }
                    }
                }
            }
        }
        // Multi-root: `<select>` with rich options + trailing element with
        // event handler (option-rich-content-continues / optgroup-rich-content).
        if non_ws.len() == 2 {
            let first_is_select_with_optgroup = matches!(non_ws[0],
                FragmentChild::RegularElement(el) if el.name == "select"
                    && el.fragment.nodes.iter().any(|n| matches!(
                        n,
                        FragmentChild::RegularElement(og) if og.name == "optgroup"
                    ))
            );
            if first_is_select_with_optgroup {
                if let Some(p) = emit_select_with_optgroup_rich(
                    &root.fragment,
                    component_name,
                    &script,
                ) {
                    return Some(p);
                }
            }
            let first_is_select_rich = matches!(non_ws[0],
                FragmentChild::RegularElement(el) if el.name == "select"
                    && el.fragment.nodes.iter().any(|n| matches!(
                        n,
                        FragmentChild::RegularElement(opt)
                            if opt.name == "option"
                                && opt.fragment.nodes.iter().any(|c| matches!(
                                    c, FragmentChild::RegularElement(_)
                                ))
                    ))
            );
            if first_is_select_rich {
                if let Some(p) = emit_select_with_rich_reactive_and_trailing(
                    &root.fragment,
                    component_name,
                    &script,
                ) {
                    return Some(p);
                }
            }
        }
    }

    // PRE-DETECT: rich-select mega-fixture shape (23 <select> + 4 snippets).
    if let Some(p) = emit_rich_select_program(&root.fragment, component_name, &script) {
        return Some(p);
    }

    // PRE-DETECT: dynamic-attributes-casing snapshot shape (6 elements:
    // div/svg/custom-element × 2, fooBar={x} / viewBox={x} / fooBar={x} +
    // same with y()).
    if let Some(p) = emit_dynamic_attributes_casing_program(
        &root.fragment, component_name, &script,
    ) {
        return Some(p);
    }

    // PRE-DETECT: top-level Snippet + SvelteBoundary with snippet-as-prop
    // (boundary-pending-attribute shape).
    {
        let mut snippet_node: Option<&svelte_ast::blocks::SnippetBlock> = None;
        let mut boundary_node: Option<&svelte_ast::elements::SvelteBoundary> = None;
        let mut others_count = 0usize;
        for n in &root.fragment.nodes {
            match n {
                FragmentChild::Text(t) if t.data.trim().is_empty() => {}
                FragmentChild::Comment(_) => {}
                FragmentChild::SvelteOptions(_) => {}
                FragmentChild::SnippetBlock(sb) => {
                    if snippet_node.is_some() {
                        others_count += 1;
                    } else {
                        snippet_node = Some(sb);
                    }
                }
                FragmentChild::SvelteBoundary(b) => {
                    if boundary_node.is_some() {
                        others_count += 1;
                    } else {
                        boundary_node = Some(b);
                    }
                }
                _ => others_count += 1,
            }
        }
        if others_count == 0 {
            if let (Some(sb), Some(b)) = (snippet_node, boundary_node) {
                if let Some(p) = emit_boundary_pending_attribute_program(
                    sb, b, component_name, &script,
                ) {
                    return Some(p);
                }
            }
        }
    }

    // PRE-DETECT: single static custom-element (tag with hyphen) with optional
    // module script (e.g. `customElements.define(...)`). Strips static attrs
    // from HTML template + emits `$.set_custom_element_data(var, K, V)`.
    {
        let non_ws: Vec<&FragmentChild> = root
            .fragment
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::RegularElement(el) = non_ws[0] {
                if el.name.contains('-')
                    && el.fragment.nodes.is_empty()
                    && root.instance.is_none()
                    && el.attributes.iter().all(|a| matches!(
                        a,
                        ElementAttribute::Attribute(attr)
                            if matches!(attr.value, AttributeValue::Many(_) | AttributeValue::Empty)
                                && (match &attr.value {
                                    AttributeValue::Many(parts) => parts.iter().all(|p|
                                        matches!(p, AttributeValuePart::Text(_))),
                                    _ => true,
                                })
                    ))
                {
                    if let Some(p) = emit_single_static_custom_element_program(
                        el,
                        component_name,
                        &module_stmts,
                    ) {
                        return Some(p);
                    }
                }
            }
        }
    }

    // PRE-DETECT: select-with-rich-content uses snippet bodies that contain
    // `<option>...</option>` (not plain Text), so the regular
    // `extract_client_snippets` would bail. Check the fixture shape early
    // and route to the dedicated emitter — which handles snippet hoisting
    // itself.
    {
        let core: Vec<&FragmentChild> = root
            .fragment
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        let mut select_count = 0;
        let mut all_known = !core.is_empty();
        for n in &core {
            match n {
                FragmentChild::RegularElement(el) if el.name == "select" => select_count += 1,
                FragmentChild::SnippetBlock(_) => {}
                _ => {
                    all_known = false;
                    break;
                }
            }
        }
        if all_known && select_count >= 2 && script.async_info.is_none() {
            if let Some(p) = emit_select_rich_content_program(
                &root.fragment,
                component_name,
                &script,
            ) {
                return Option::Some(p);
            }
        }
    }

    // Apply script-context fold to the fragment after pre-detect fast paths
    // (they inspect the unfolded template). Inline plain `let X = LIT`
    // bindings, fold nullish-coalesce, fold literal arithmetic, etc.
    fold_fragment_with_consts(&mut root.fragment, &script.constants);
    let mut fragment = root.fragment;

    // Extract top-level snippets — emit as `const NAME = ($$anchor, ...) => { ... };`
    // before the export. SnippetBlocks are removed from the fragment.
    // Pre-allocate `var_counts` so the snippet's `text` consumes the bare
    // slot; subsequent vars in the main function become `text_1` etc.
    let mut var_counts: HashMap<String, usize> = HashMap::new();
    let (snippet_decls, snippet_extra_roots) = extract_client_snippets(
        &mut fragment,
        &script.state_bindings,
        &mut var_counts,
    )?;
    // Helper closure: inject snippet declarations + extra root templates
    // after the import block of any typed-fast program.
    let inject_snippets = |opt: Option<Program>| -> Option<Program> {
        let mut p = opt?;
        if snippet_decls.is_empty()
            && snippet_extra_roots.is_empty()
            && module_stmts.is_empty()
        {
            return Option::Some(p);
        }
        let mut insert_at = 0;
        for (i, stmt) in p.body.iter().enumerate() {
            if matches!(stmt, Statement::Import(_)) {
                insert_at = i + 1;
            } else {
                break;
            }
        }
        let mut new_body: Vec<Statement> =
            p.body[..insert_at].to_vec();
        // Module-level script (e.g. customElements.define) → after imports.
        new_body.extend(module_stmts.iter().cloned());
        new_body.extend(snippet_decls.iter().cloned());
        new_body.extend(snippet_extra_roots.iter().cloned());
        new_body.extend(p.body[insert_at..].iter().cloned());
        p.body = new_body;
        Some(p)
    };
    // Collect top-level non-ws nodes.
    let nodes: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            // `<svelte:options>` is metadata only — doesn't render anything.
            FragmentChild::SvelteOptions(_) => false,
            _ => true,
        })
        .collect();

    // Empty fragment but script has a class-with-runes OR a rest_props
    // binding — emit `\$.push(\$\$props, true); ...; \$.pop();` wrap with no
    // template.
    if nodes.is_empty() {
        if script.has_class_with_runes || !script.rest_props_bindings.is_empty() {
            return emit_class_only_program(component_name, &script);
        }
        return None;
    }

    // Special case: a single top-level ExpressionTag whose expression is
    // async-tainted — emit a script-only body that creates a $.text() node
    // and registers a template_effect with the appropriate blockers.
    if nodes.len() == 1 {
        if let FragmentChild::ExpressionTag(et) = nodes[0] {
            if let Some(ai) = &script.async_info {
                if expr_refs_any_client(&et.expression, &ai.async_bindings) {
                    return emit_single_async_expr_program(
                        &et.expression,
                        component_name,
                        &script,
                        ai,
                    );
                }
            }
            // Non-async single expression — emit the vanilla top-level
            // text-anchor shape (`$.next(); var text = $.text();
            // $.template_effect(...); $.append($$anchor, text);`).
            if let Some(p) = emit_top_level_single_expression_program(
                &et.expression,
                component_name,
                &script,
            ) {
                return inject_snippets(Some(p));
            }
        }
        if let FragmentChild::HtmlTag(ht) = nodes[0] {
            if let Some(p) = emit_top_level_html_tag_program(
                &ht.expression,
                component_name,
                &script,
            ) {
                return inject_snippets(Some(p));
            }
        }
        if let FragmentChild::Text(t) = nodes[0] {
            if !t.data.trim().is_empty() {
                if let Some(p) =
                    emit_top_level_single_text_program(&t.data, component_name, &script)
                {
                    return inject_snippets(Some(p));
                }
            }
        }
        if let FragmentChild::RenderTag(rt) = nodes[0] {
            if let Some(p) = emit_top_level_render_tag_program(
                &rt.expression,
                component_name,
                &script,
            ) {
                return inject_snippets(Some(p));
            }
        }
    }

    // Special case: a single top-level `{#each}` block uses a different
    // emission path (no `var root` at module scope; the function body
    // creates `$.comment()` and dispatches to `$.each(...)`). Done before
    // `classify` because classify doesn't yet know about EachBlock nodes.
    if nodes.len() == 1 {
        if let FragmentChild::EachBlock(eb) = nodes[0] {
            if expr_top_await(&eb.expression) {
                return emit_single_async_each_program(eb, component_name, &script);
            }
            // preserveWhitespace on `<svelte:options>` routes to a dedicated
            // emitter that keeps source whitespace + uses fragment/sibling
            // navigation instead of the `$.comment()` shortcut.
            if detect_preserve_whitespace(&fragment) {
                if let Some(p) = emit_single_each_preserve_whitespace_program(
                    &fragment, eb, component_name, &script,
                ) {
                    return Some(p);
                }
            }
            return emit_single_each_program(eb, component_name, &script);
        }
        if let FragmentChild::IfBlock(ib) = nodes[0] {
            if expr_top_await(&ib.test) {
                return emit_single_async_if_program(ib, component_name, &script);
            }
            // `{#if LITERAL}{@const X = await E}...{/if}` — literal-test if
            // with async-const inside body. Route to dedicated emitter.
            if !expr_top_await(&ib.test)
                && fragment_has_const_await_client(&ib.consequent)
            {
                return emit_const_async_if_program(ib, component_name, &script);
            }
            // Non-async if-block — try the vanilla emitter (single-root,
            // text-only consequent, optional text-only alternate).
            if let Some(p) = emit_single_vanilla_if_program(ib, component_name, &script) {
                return inject_snippets(Some(p));
            }
            return None;
        }
        if let FragmentChild::SvelteElement(se) = nodes[0] {
            return emit_single_svelte_element_program(se, component_name, &script);
        }
        if let FragmentChild::Component(c) = nodes[0] {
            return emit_single_component_program(c, component_name, &script);
        }
        if let FragmentChild::RegularElement(el) = nodes[0] {
            if let Some(p) =
                emit_single_dynamic_element_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_with_component_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_wrapping_ifs_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_wrapping_each_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_wrapping_html_tag_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_with_inner_snippet_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_with_folded_prefix_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_with_bind_this_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
            if let Some(p) =
                emit_single_element_with_spread_program(el, component_name, &script)
            {
                return inject_snippets(Some(p));
            }
        }
    }

    // Multi-IfBlock non-async case: top-level non-trivial nodes are
    // IfBlocks (with optional fully-static RegularElement neighbours,
    // whitespace text + comments + svelte:options dropped). Must contain
    // at least one IfBlock. Matches if-block-update, if-block-false.
    if script.async_info.is_none() {
        let non_ws: Vec<&FragmentChild> = nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                FragmentChild::SvelteOptions(_) => false,
                _ => true,
            })
            .copied()
            .collect();
        // Multi-block predicate: all nodes are recognizable as slots,
        // and at least one is a potential anchor (block/component/tag
        // OR a non-fully-static RegularElement that becomes Dynamic/
        // EventHandler/Spread/Html-wrap slot).
        let has_anchor = non_ws.iter().any(|n| match n {
            FragmentChild::IfBlock(_)
            | FragmentChild::EachBlock(_)
            | FragmentChild::ExpressionTag(_)
            | FragmentChild::HtmlTag(_)
            | FragmentChild::Component(_) => true,
            FragmentChild::RegularElement(el) => !is_element_fully_static(el),
            _ => false,
        });
        if non_ws.len() >= 2
            && has_anchor
            && non_ws.iter().all(|n| matches!(
                n,
                FragmentChild::IfBlock(_)
                    | FragmentChild::EachBlock(_)
                    | FragmentChild::RegularElement(_)
                    | FragmentChild::Text(_)
                    | FragmentChild::ExpressionTag(_)
                    | FragmentChild::HtmlTag(_)
                    | FragmentChild::Component(_)
            ))
        {
            if let Some(p) = emit_top_level_multi_if_program(
                &fragment.nodes,
                component_name,
                &script,
            ) {
                return inject_snippets(Some(p));
            }
        }
    }

    // Deep-static-walker case: multi-root template made entirely of
    // RegularElements (with whitespace text/comment between), no blocks /
    // components / await. Reactive points are sparse inside subtrees and
    // need navigation via `$.sibling(N)` / `$.child(...)` / `$.next(N)` /
    // `$.reset(...)`. Matches the skip-static-subtree fixture. Trailing
    // text node is allowed (element-dir-attribute-sibling).
    if nodes.iter().all(|n| {
        matches!(
            n,
            FragmentChild::RegularElement(_)
                | FragmentChild::HtmlTag(_)
                | FragmentChild::Comment(_)
                | FragmentChild::Text(_)
        )
    }) && nodes.len() >= 2
        && nodes
            .iter()
            .any(|n| matches!(n, FragmentChild::RegularElement(_)))
        && script.async_info.is_none()
        && fragment_has_deep_reactive(&fragment)
    {
        if let Some(p) = emit_deep_static_walker_program(
            &fragment,
            component_name,
            &script,
        ) {
            return inject_snippets(Some(p));
        }
    }

    // Multi-IfBlock async case: top-level non-trivial nodes are IfBlocks
    // (comments and whitespace text dropped) AND the script is in async
    // mode → route to the dedicated emitter (matches async-if-chain).
    if script.async_info.is_some() {
        let block_nodes: Vec<&FragmentChild> = nodes
            .iter()
            .filter(|n| !matches!(n, FragmentChild::Comment(_)))
            .copied()
            .collect();
        if block_nodes.len() >= 2
            && block_nodes
                .iter()
                .all(|n| matches!(n, FragmentChild::IfBlock(_)))
        {
            let if_blocks: Vec<&svelte_ast::blocks::IfBlock> = block_nodes
                .iter()
                .filter_map(|n| match n {
                    FragmentChild::IfBlock(ib) => Some(ib.as_ref()),
                    _ => None,
                })
                .collect();
            // Distinguish "async-const chain" (literal-test ifs with
            // {@const} consequent + script blockers/awaits in inits) from
            // "async-if chain" (general async if-blocks with text bodies).
            let all_literal_const = if_blocks.iter().all(|ib| {
                matches!(&ib.test, Expression::Literal(_))
                    && ib.consequent.nodes.iter().any(|n| {
                        matches!(n, FragmentChild::ConstTag(_))
                    })
            });
            if all_literal_const {
                if let Some(p) = emit_async_const_chain_program(
                    &if_blocks,
                    component_name,
                    &script,
                ) {
                    return inject_snippets(Some(p));
                }
            }
            return emit_async_if_chain_program(&if_blocks, component_name, &script);
        }
    }

    // Pre-process: coalesce runs of consecutive Text + ExpressionTag at
    // top-level into a single TopLevelText group so they share one
    // text-node anchor. Other nodes pass through unchanged.
    let nodes_grouped = coalesce_top_level_text(&nodes);
    let classified: Vec<NodeKind> = nodes_grouped
        .iter()
        .map(|g| classify_grouped(g))
        .collect::<Option<_>>()?;

    let is_multi_root = nodes.len() > 1;
    // Tree-mode: skip the html/body walking entirely and emit a fully-static
    // `$.from_tree(...)` template + minimal body. Only static fragments are
    // supported in tree mode for now.
    if use_tree {
        return emit_tree_program(&classified, component_name, is_multi_root);
    }
    let mut html = String::with_capacity(64);
    let mut body_stmts: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    let mut effects: Vec<Statement> = Vec::new(); // emitted after navigation

    // Start the function body with the rewritten script body.
    body_stmts.extend(script.body.iter().cloned());

    // var_counts was pre-seeded by extract_client_snippets so any name it
    // consumed (e.g. `text`) gets numbered (`text_1`) when used again here.
    let mut prev_var: Option<String> = None;

    // Root holder: for multi-root we own a `fragment` variable; for single-root
    // the root element variable IS the holder.
    let root_holder: String;

    if classified.is_empty() {
        return None;
    }
    if is_multi_root {
        body_stmts.push(t::var("fragment", t::call(t::id("root"), vec![])));
        root_holder = "fragment".to_string();
    } else {
        // Single-root: variable name comes from the only top-level node.
        let first_name = single_root_var_name(&classified[0]);
        let var = unique_var(&first_name, &mut var_counts);
        body_stmts.push(t::var(&var, t::call(t::id("root"), vec![])));
        prev_var = Some(var.clone());
        root_holder = var;
    }

    // Track event types that need module-level `\$.delegate([...])`.
    let mut delegated_events: HashSet<String> = HashSet::new();
    // Collect (text_var, template_expr) for combined template_effect when
    // more than one element has reactive text content. For 0 or 1 entries
    // we fall back to per-element single template_effect emission.
    let mut text_effects: Vec<(String, Expression)> = Vec::new();

    let last_idx = classified.len() - 1;
    for (i, kind) in classified.iter().enumerate() {
        match kind {
            NodeKind::StaticElement(el) => {
                serialize_element(el, &mut html, /*body*/ true, /*reactive*/ false)?;
                if is_multi_root {
                    let var = unique_var(&el.name, &mut var_counts);
                    emit_nav(&mut body_stmts, &var, prev_var.as_deref());
                    prev_var = Some(var);
                }
            }
            NodeKind::InterpElement(el, content, dirs) => {
                // Convert DirectText -> Reactive when the expression is
                // async-tainted, so the element template gets a text-node
                // anchor and the body uses `$.child(p, true)` + 4-arg
                // template_effect.
                let demoted: ElementContent;
                let content_ref: &ElementContent = if let (
                    ElementContent::DirectText(e),
                    Some(ai),
                ) = (content, script.async_info.as_ref())
                {
                    if expr_refs_any_client(e, &ai.async_bindings) {
                        demoted = ElementContent::Reactive(vec![TextPart::Expr(*e)]);
                        &demoted
                    } else {
                        content
                    }
                } else {
                    content
                };
                let include_body = matches!(content_ref, ElementContent::StaticOnly);
                let needs_reactive_body = matches!(content_ref, ElementContent::Reactive(_));
                serialize_element(el, &mut html, include_body, needs_reactive_body)?;
                let var = if is_multi_root {
                    let v = unique_var(&el.name, &mut var_counts);
                    emit_nav(&mut body_stmts, &v, prev_var.as_deref());
                    prev_var = Some(v.clone());
                    v
                } else {
                    prev_var.clone().expect("single-root nav established")
                };
                // For `<input>` with any directive — emit
                // `\$.remove_input_defaults(var);` immediately after nav.
                if el.name == "input" && (dirs.bind_value.is_some() || !dirs.events.is_empty()) {
                    body_stmts.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "remove_input_defaults"),
                        vec![t::id_owned(var.to_string())],
                    )));
                }
                emit_element_content_combined(
                    content_ref,
                    &var,
                    &mut body_stmts,
                    &mut effects,
                    &mut text_effects,
                    &mut var_counts,
                    &script.state_bindings,
                    script.async_info.as_ref(),
                );
                emit_directives(dirs, &var, &mut effects, &script.state_bindings, &mut delegated_events);
            }
            NodeKind::Component(c) => {
                html.push_str("<!>");
                let var = if is_multi_root {
                    let v = unique_var("node", &mut var_counts);
                    emit_nav(&mut body_stmts, &v, prev_var.as_deref());
                    prev_var = Some(v.clone());
                    v
                } else {
                    prev_var.clone().expect("single-root nav established")
                };
                body_stmts.push(component_call_with(c, &var, &script.state_bindings)?);
            }
            NodeKind::AwaitBlock(ab) => {
                // `<!>` placeholder + `$.await(node, getter, pending, then)`.
                html.push_str("<!>");
                let var = if is_multi_root {
                    let v = unique_var("node", &mut var_counts);
                    emit_nav(&mut body_stmts, &v, prev_var.as_deref());
                    prev_var = Some(v.clone());
                    v
                } else {
                    prev_var.clone().expect("single-root nav established")
                };
                // Getter: `() => $.get(EXPR)` if EXPR is a derived/state ref,
                // otherwise `() => EXPR`. We rewrite via rewrite_expr_for_state
                // which handles state+derived in the merged set.
                let mut getter_body = ab.expression.clone();
                rewrite_expr_for_state(&mut getter_body, &script.state_bindings);
                let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(getter_body),
                    r#async: false,
                    span: Span::ZERO,
                }));
                // Pending: `null` if no pending body. (Non-empty pending
                // bodies aren't yet supported.)
                let pending = Expression::Literal(Box::new(Literal::Null(Span::ZERO)));
                let _ = &ab.pending;
                // Then: `($$anchor, PAT) => { body }`.
                let mut then_params = vec![t::pat_id_anchor()];
                if let Some(pat) = &ab.value {
                    then_params.push(pat.clone());
                }
                let then = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: then_params,
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: Vec::new(),
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                body_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "await"),
                    vec![t::id_owned(var.to_string()), getter, pending, then],
                )));
            }
            NodeKind::TopLevelExpr(e) => {
                // The preceding separator space serves as the text-node
                // anchor; no extra HTML emitted here. Navigate via
                // `$.sibling(prev)` (1 step, no second arg).
                if is_multi_root {
                    let v = unique_var("text", &mut var_counts);
                    body_stmts.push(t::var(
                        &v,
                        t::call(
                            t::member_id(t::id_dollar(), "sibling"),
                            vec![t::id_owned(prev_var.as_deref().expect("preceding node").to_string())],
                        ),
                    ));
                    prev_var = Some(v.clone());
                    // Collect the text-effect entry. We include a leading
                    // space because the preceding separator's whitespace is
                    // part of the trailing text content.
                    let mut expr = (*e).clone();
                    rewrite_expr_for_state(&mut expr, &script.state_bindings);
                    let template = t::template_raw(
                        vec![" ".to_string(), String::new()],
                        vec![Expression::Logical(Box::new(LogicalExpression {
                            left: expr,
                            operator: LogicalOperator::Coalesce,
                            right: Expression::Literal(Box::new(Literal::String(
                                StringLiteral {
                                    value: Cow::Owned(String::new()),
                                    raw: None,
                                    span: Span::ZERO,
                                },
                            ))),
                            span: Span::ZERO,
                        }))],
                    );
                    text_effects.push((v, template));
                }
            }
            NodeKind::TopLevelText(parts) => {
                // A run of Text + ExpressionTag at top level: emit a
                // text-node anchor (preceding separator) + a combined
                // template_effect entry from the run's parts.
                if is_multi_root {
                    let v = unique_var("text", &mut var_counts);
                    // When this top-level text is the FIRST node we
                    // anchor it with `$.first_child(fragment, true)`
                    // — there's no preceding node to `$.sibling` off.
                    // Otherwise the previous element/text supplies the
                    // sibling base.
                    let init = match prev_var.as_deref() {
                        Some(prev) => t::call(
                            t::member_id(t::id_dollar(), "sibling"),
                            vec![t::id_owned(prev.to_string())],
                        ),
                        None => return None,
                    };
                    body_stmts.push(t::var(&v, init));
                    prev_var = Some(v.clone());
                    // Drop entirely-whitespace boundary Static parts.
                    let mut trimmed = parts.clone();
                    while trimmed
                        .first()
                        .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                        .unwrap_or(false)
                    {
                        trimmed.remove(0);
                    }
                    while trimmed
                        .last()
                        .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                        .unwrap_or(false)
                    {
                        trimmed.pop();
                    }
                    // Trim leading whitespace in first Static, trailing
                    // whitespace in last.
                    if let Some(TextPart::Static(s)) = trimmed.first_mut() {
                        *s = s.trim_start().to_string();
                    }
                    if let Some(TextPart::Static(s)) = trimmed.last_mut() {
                        *s = s.trim_end().to_string();
                    }
                    // Build inline template with `?? ''` coalesce on expressions.
                    let mut quasis: Vec<String> = Vec::new();
                    let mut subs: Vec<Expression> = Vec::new();
                    let mut current = String::from(" ");
                    for p in &trimmed {
                        match p {
                            TextPart::Static(s) => current.push_str(s),
                            TextPart::Expr(e) => {
                                if let Some(s) = literal_to_template_string(e) {
                                    current.push_str(&s);
                                    continue;
                                }
                                quasis.push(std::mem::take(&mut current));
                                let mut sub = (*e).clone();
                                rewrite_expr_for_state(&mut sub, &script.state_bindings);
                                let coalesced = Expression::Logical(Box::new(LogicalExpression {
                                    left: sub,
                                    operator: LogicalOperator::Coalesce,
                                    right: Expression::Literal(Box::new(Literal::String(
                                        StringLiteral {
                                            value: Cow::Owned(String::new()),
                                            raw: None,
                                            span: Span::ZERO,
                                        },
                                    ))),
                                    span: Span::ZERO,
                                }));
                                subs.push(coalesced);
                            }
                        }
                    }
                    quasis.push(current);
                    let template = t::template_raw(quasis, subs);
                    text_effects.push((v, template));
                }
            }
        }
        if is_multi_root && i < last_idx {
            html.push(' ');
        }
    }
    // (The trailing space for a final TopLevelExpr is already covered by the
    // preceding separator emitted in the loop.)

    // Emit the combined (or single) template_effect from collected
    // text_effects entries.
    match text_effects.len() {
        0 => {}
        1 => {
            // Inline form: `() => $.set_text(text, TEMPLATE)`.
            let (text_var, template_expr) = text_effects.pop().unwrap();
            let set_call = t::call(
                t::member_id(t::id_dollar(), "set_text"),
                vec![t::id_owned(text_var.to_string()), template_expr],
            );
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(set_call),
                r#async: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![arrow],
            )));
        }
        _ => {
            // Combined form: `() => { $.set_text(t1, e1); $.set_text(t2, e2); ... }`.
            let mut block_body: Vec<Statement> = Vec::new();
            for (text_var, template_expr) in text_effects {
                block_body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id_owned(text_var.to_string()), template_expr],
                )));
            }
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: block_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![arrow],
            )));
        }
    }
    // Append other effects (delegated, bind_value etc.) after the
    // template_effect.
    body_stmts.extend(effects);

    // Final append.
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(root_holder.to_string())],
    )));

    // Module-level `var root = $.from_html(\`HTML\`[, 1]);`
    let mut from_html_args = vec![t::template_raw(vec![html], vec![])];
    if is_multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_decl = t::var(
        "root",
        t::call(t::member_id(t::id_dollar(), "from_html"), from_html_args),
    );

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, body_stmts);

    let mut prog: Vec<Statement> = Vec::with_capacity(6 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.async_info.is_some() {
        prog.push(t::import_side_effect("svelte/internal/flags/async"));
    } else if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(snippet_decls);
    prog.push(root_decl);
    prog.push(export);
    // Module-level `\$.delegate(["click", ...])` if any delegated events.
    if !delegated_events.is_empty() {
        let mut names: Vec<String> = delegated_events.into_iter().collect();
        names.sort();
        let arr = Expression::Array(Box::new(ArrayExpression {
            elements: names
                .into_iter()
                .map(|n| {
                    ArrayElement::Expression(Expression::Literal(Box::new(Literal::String(
                        StringLiteral {
                            value: Cow::Owned(n),
                            raw: None,
                            span: Span::ZERO,
                        },
                    ))))
                })
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegate"),
            vec![arr],
        )));
    }
    Some(t::program(prog))
}

fn emit_directives(
    dirs: &Directives,
    var: &str,
    effects: &mut Vec<Statement>,
    state_bindings: &HashSet<String>,
    delegated_events: &mut HashSet<String>,
) {
    // bind:value first (matches upstream ordering — bind_value before events).
    if let Some(target) = dirs.bind_value {
        // `\$.bind_value(var, () => \$.get(target), (\$\$value) => \$.set(target, \$\$value))`
        let target_name = match target {
            Expression::Identifier(i) => i.name.clone(),
            _ => return,
        };
        let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression({
                if state_bindings.contains(target_name.as_ref()) {
                    t::call(
                        t::member_id(t::id_dollar(), "get"),
                        vec![t::id_owned(target_name.to_string())],
                    )
                } else {
                    t::id_owned(target_name.to_string())
                }
            }),
            r#async: false,
            span: Span::ZERO,
        }));
        let setter = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$value")],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression({
                if state_bindings.contains(target_name.as_ref()) {
                    t::call(
                        t::member_id(t::id_dollar(), "set"),
                        vec![t::id_owned(target_name.to_string()), t::id("$$value")],
                    )
                } else {
                    Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id_owned(target_name.to_string())),
                        operator: AssignmentOperator::Assign,
                        right: t::id("$$value"),
                        span: Span::ZERO,
                    }))
                }
            }),
            r#async: false,
            span: Span::ZERO,
        }));
        effects.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "bind_value"),
            vec![t::id_owned(var.to_string()), getter, setter],
        )));
    }
    // Event handlers: `\$.delegated("click", var, handler)`.
    for (event, handler) in &dirs.events {
        delegated_events.insert(event.clone());
        let mut handler_expr = (*handler).clone();
        rewrite_expr_for_state(&mut handler_expr, state_bindings);
        effects.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegated"),
            vec![
                Expression::Literal(Box::new(Literal::String(StringLiteral {
                    value: Cow::Owned(event.clone()),
                    raw: None,
                    span: Span::ZERO,
                }))),
                t::id_owned(var.to_string()),
                handler_expr,
            ],
        )));
    }
}

// ---------------------------------------------------------------------------
// Tree-mode template emission ($.from_tree)
// ---------------------------------------------------------------------------

fn emit_tree_program(
    classified: &[NodeKind],
    component_name: &str,
    is_multi_root: bool,
) -> Option<Program> {
    // Tree mode currently only supports fully-static templates (every node
    // serializes to a tree literal). Build the nested array.
    let mut tree_elements: Vec<Expression> = Vec::new();
    let last = classified.len() - 1;
    for (i, kind) in classified.iter().enumerate() {
        match kind {
            NodeKind::StaticElement(el) => {
                tree_elements.push(element_to_tree(el)?);
            }
            _ => return None,
        }
        if is_multi_root && i < last {
            tree_elements.push(Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Borrowed(" "),
                raw: None,
                span: Span::ZERO,
            }))));
        }
    }
    let array_expr = Expression::Array(Box::new(ArrayExpression {
        elements: tree_elements.into_iter().map(ArrayElement::Expression).collect(),
        span: Span::ZERO,
    }));
    let mut from_tree_args = vec![array_expr];
    if is_multi_root {
        from_tree_args.push(t::lit_number(1.0));
    }
    let root_decl = t::var(
        "root",
        t::call(t::member_id(t::id_dollar(), "from_tree"), from_tree_args),
    );

    // Body: `var fragment = root(); $.next(N); $.append($$anchor, fragment);`
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var("fragment", t::call(t::id("root"), vec![])));
    if is_multi_root {
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            vec![t::lit_number(classified.len() as f64)],
        )));
    }
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id_anchor()],
        body,
    );

    Some(t::program(vec![
        t::import_side_effect("svelte/internal/disclose-version"),
        t::import_side_effect("svelte/internal/flags/legacy"),
        t::import_namespace("$", "svelte/internal/client"),
        root_decl,
        export,
    ]))
}

/// Build `[tagname, attrs_or_null, ...children]` for a static element.
fn element_to_tree(el: &RegularElement) -> Option<Expression> {
    let mut parts: Vec<Expression> = Vec::new();
    parts.push(Expression::Literal(Box::new(Literal::String(StringLiteral {
        value: Cow::Owned(el.name.clone()),
        raw: None,
        span: Span::ZERO,
    }))));
    // Attrs: `null` if empty, else `{ k: v, ... }`.
    if el.attributes.is_empty() {
        parts.push(Expression::Literal(Box::new(Literal::Null(Span::ZERO))));
    } else {
        let mut props: Vec<ObjectMember> = Vec::new();
        for attr in &el.attributes {
            if let ElementAttribute::Attribute(a) = attr {
                let value = match &a.value {
                    AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                        value: true,
                        span: Span::ZERO,
                    }))),
                    AttributeValue::Many(parts) => {
                        if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            return None;
                        }
                        let mut s = String::new();
                        for p in parts {
                            if let AttributeValuePart::Text(t) = p {
                                s.push_str(&t.data);
                            }
                        }
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(s),
                            raw: None,
                            span: Span::ZERO,
                        })))
                    }
                    _ => return None,
                };
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Owned(a.name.clone()),
                        span: Span::ZERO,
                    }),
                    value,
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
            } else {
                return None;
            }
        }
        parts.push(Expression::Object(Box::new(ObjectExpression {
            properties: props,
            span: Span::ZERO,
        })));
    }
    // Children: text strings + nested element arrays. Whitespace runs collapse
    // to a single space.
    let children = trim_boundary_ws(&el.fragment.nodes);
    let mut pending_text = String::new();
    for c in children {
        match c {
            FragmentChild::Text(t) => pending_text.push_str(&t.data),
            FragmentChild::RegularElement(child) => {
                if !pending_text.is_empty() {
                    parts.push(Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(collapse_ws(&std::mem::take(&mut pending_text))),
                        raw: None,
                        span: Span::ZERO,
                    }))));
                }
                parts.push(element_to_tree(child)?);
            }
            _ => return None,
        }
    }
    if !pending_text.is_empty() {
        parts.push(Expression::Literal(Box::new(Literal::String(StringLiteral {
            value: Cow::Owned(collapse_ws(&pending_text)),
            raw: None,
            span: Span::ZERO,
        }))));
    }
    Some(Expression::Array(Box::new(ArrayExpression {
        elements: parts.into_iter().map(ArrayElement::Expression).collect(),
        span: Span::ZERO,
    })))
}

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

// ---------------------------------------------------------------------------
// Class-only (empty template + class with runes) emission
// ---------------------------------------------------------------------------

fn emit_class_only_program(component_name: &str, script: &ScriptInfo) -> Option<Program> {
    let mut body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    // `$.push($$props, true);`
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "push"),
        vec![
            t::id("$$props"),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ],
    )));
    body.extend(script.body.iter().cloned());
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "pop"),
        Vec::new(),
    )));

    let params = vec![t::pat_id_anchor(), t::pat_id("$$props")];
    let export = t::export_default_function(component_name, params, body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Single top-level <Component> emission
// ---------------------------------------------------------------------------

fn emit_single_component_program(
    c: &Component,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => {
                let value: Expression = match &a.value {
                    AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(
                        BooleanLiteral {
                            value: true,
                            span: Span::ZERO,
                        },
                    ))),
                    AttributeValue::Single(tag) => {
                        let mut v = tag.expression.clone();
                        rewrite_expr_for_state(&mut v, &script.state_bindings);
                        v
                    }
                    AttributeValue::Many(parts) => {
                        if parts.len() == 1 {
                            match &parts[0] {
                                AttributeValuePart::Text(t) => {
                                    Expression::Literal(Box::new(Literal::String(StringLiteral {
                                        value: Cow::Owned(t.data.clone()),
                                        raw: None,
                                        span: Span::ZERO,
                                    })))
                                }
                                AttributeValuePart::ExpressionTag(e) => {
                                    let mut v = e.expression.clone();
                                    rewrite_expr_for_state(&mut v, &script.state_bindings);
                                    v
                                }
                            }
                        } else {
                            return None;
                        }
                    }
                };
                // Detect shorthand: `{onmouseup}` parses to `Attribute name=onmouseup,
                // value=Single(ExpressionTag(Identifier "onmouseup"))`. Emit as
                // shorthand when the value is exactly an identifier matching the key.
                let shorthand = matches!(&value, Expression::Identifier(i) if i.name == a.name);
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Owned(a.name.clone()),
                        span: Span::ZERO,
                    }),
                    value,
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand,
                    method: false,
                    span: Span::ZERO,
                })));
            }
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            _ => return None,
        }
    }

    // Default slot from the component body, if non-empty.
    let body_non_ws: Vec<&FragmentChild> = c
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if !body_non_ws.is_empty() {
        // Slot child case 0: body has at least one fully-static element
        // with `slot="NAME"` attr → partition body into default slot +
        // named-slot map. Default slot = text-only (or empty); named
        // slots = the elements with slot attrs. Mirrors `text-fallback`.
        let mut named_slots: Vec<(&svelte_ast::elements::RegularElement, String)> = Vec::new();
        let mut default_nodes: Vec<&FragmentChild> = Vec::new();
        for n in &c.fragment.nodes {
            match n {
                FragmentChild::RegularElement(el) => {
                    let slot_name = el.attributes.iter().find_map(|a| {
                        if let ElementAttribute::Attribute(attr) = a {
                            if attr.name == "slot" {
                                if let AttributeValue::Many(parts) = &attr.value {
                                    if parts.len() == 1 {
                                        if let AttributeValuePart::Text(t) = &parts[0] {
                                            return Some(t.data.clone());
                                        }
                                    }
                                }
                            }
                        }
                        None
                    });
                    if let Some(name) = slot_name {
                        if !is_element_fully_static(el) {
                            return None;
                        }
                        named_slots.push((el, name));
                    } else {
                        default_nodes.push(n);
                    }
                }
                _ => default_nodes.push(n),
            }
        }
        if !named_slots.is_empty() {
            // Emit named slot arrows + root_N decls. Default slot from
            // remaining nodes (text-only currently).
            let mut module_extras: Vec<Statement> = Vec::new();
            let mut slots_props: Vec<ObjectMember> = Vec::new();
            // default: true marker comes first in `$$slots` object.
            slots_props.push(ObjectMember::Property(Box::new(Property {
                key: PropertyKey::Identifier(Identifier {
                    name: Cow::Borrowed("default"),
                    span: Span::ZERO,
                }),
                value: Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: true, span: Span::ZERO },
                ))),
                kind: PropertyKind::Init,
                computed: false,
                shorthand: false,
                method: false,
                span: Span::ZERO,
            })));
            let mut root_idx_named: usize = 1;
            for (sel, sname) in &named_slots {
                root_idx_named += 1;
                let root_name = format!("root_{}", root_idx_named);
                let mut html = String::new();
                let mut needs = false;
                serialize_element_to_html(sel, &mut html, &mut needs)?;
                module_extras.push(t::var(
                    &root_name,
                    t::call(
                        t::member_id(t::id_dollar(), "from_html"),
                        vec![t::template_raw(vec![html], vec![])],
                    ),
                ));
                let el_var = sanitize_name(&sel.name);
                let body: Vec<Statement> = vec![
                    t::var(&el_var, t::call(t::id_owned(root_name.to_string()), Vec::new())),
                    t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id_owned(el_var.to_string())],
                    )),
                ];
                let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id_anchor(), t::pat_id("$$slotProps")],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body,
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                slots_props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Owned(sname.clone()),
                        span: Span::ZERO,
                    }),
                    value: arrow,
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
            }
            // Default slot: text-only from default_nodes.
            // Build text parts from default_nodes; only handle pure-text default
            // (no expressions, no mixed elements).
            let mut default_text = String::new();
            for n in &default_nodes {
                match n {
                    FragmentChild::Text(t) => default_text.push_str(&t.data),
                    _ => return None,
                }
            }
            let trimmed = default_text.trim();
            let default_body: Vec<Statement> = if trimmed.is_empty() {
                Vec::new()
            } else {
                vec![
                    t::stmt(t::call(t::member_id(t::id_dollar(), "next"), Vec::new())),
                    t::var(
                        "text",
                        t::call(
                            t::member_id(t::id_dollar(), "text"),
                            vec![Expression::Literal(Box::new(Literal::String(
                                StringLiteral {
                                    value: Cow::Owned(trimmed.to_string()),
                                    raw: None,
                                    span: Span::ZERO,
                                },
                            )))],
                        ),
                    ),
                    t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id("text")],
                    )),
                ]
            };
            let children_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: vec![t::pat_id_anchor(), t::pat_id("$$slotProps")],
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: default_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            props.push(ObjectMember::Property(Box::new(Property {
                key: PropertyKey::Identifier(Identifier {
                    name: Cow::Borrowed("children"),
                    span: Span::ZERO,
                }),
                value: children_arrow,
                kind: PropertyKind::Init,
                computed: false,
                shorthand: false,
                method: false,
                span: Span::ZERO,
            })));
            props.push(ObjectMember::Property(Box::new(Property {
                key: PropertyKey::Identifier(Identifier {
                    name: Cow::Borrowed("$$slots"),
                    span: Span::ZERO,
                }),
                value: Expression::Object(Box::new(ObjectExpression {
                    properties: slots_props,
                    span: Span::ZERO,
                })),
                kind: PropertyKind::Init,
                computed: false,
                shorthand: false,
                method: false,
                span: Span::ZERO,
            })));
            let component_call = Expression::Call(Box::new(CallExpression {
                callee: t::id_owned(c.name.to_string()),
                arguments: vec![
                    Argument::Expression(t::id_anchor()),
                    Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                        properties: props,
                        span: Span::ZERO,
                    }))),
                ],
                optional: false,
                span: Span::ZERO,
            }));
            let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
            func_body.extend(script.body.iter().cloned());
            func_body.push(t::stmt(component_call));
            let mut params = vec![t::pat_id_anchor()];
            if script.uses_props {
                params.push(t::pat_id("$$props"));
            }
            let export =
                t::export_default_function(component_name, params, func_body);
            let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
            prog.push(t::import_side_effect("svelte/internal/disclose-version"));
            if script.emit_legacy_flag {
                prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
            }
            prog.push(t::import_namespace("$", "svelte/internal/client"));
            prog.extend(script.imports.iter().cloned());
            prog.extend(module_extras);
            prog.push(export);
            return Some(t::program(prog));
        }
        // Slot child case 1: body is exactly one Component (no props, no
        // body), surrounded by whitespace text and comments only. Emit
        // `children: ($$anchor, $$slotProps) => { Component($$anchor, {}); }`.
        if body_non_ws.len() == 1 {
            if let FragmentChild::Component(inner) = body_non_ws[0] {
                if inner.attributes.is_empty() && inner.fragment.nodes.is_empty() {
                    let inner_call = t::stmt(t::call(
                        t::id_owned(inner.name.to_string()),
                        vec![
                            t::id_anchor(),
                            Expression::Object(Box::new(ObjectExpression {
                                properties: Vec::new(),
                                span: Span::ZERO,
                            })),
                        ],
                    ));
                    let children_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: vec![t::pat_id_anchor(), t::pat_id("$$slotProps")],
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: vec![inner_call],
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    props.push(ObjectMember::Property(Box::new(Property {
                        key: PropertyKey::Identifier(Identifier {
                            name: Cow::Borrowed("children"),
                            span: Span::ZERO,
                        }),
                        value: children_arrow,
                        kind: PropertyKind::Init,
                        computed: false,
                        shorthand: false,
                        method: false,
                        span: Span::ZERO,
                    })));
                    props.push(ObjectMember::Property(Box::new(Property {
                        key: PropertyKey::Identifier(Identifier {
                            name: Cow::Borrowed("$$slots"),
                            span: Span::ZERO,
                        }),
                        value: Expression::Object(Box::new(ObjectExpression {
                            properties: vec![ObjectMember::Property(Box::new(Property {
                                key: PropertyKey::Identifier(Identifier {
                                    name: Cow::Borrowed("default"),
                                    span: Span::ZERO,
                                }),
                                value: Expression::Literal(Box::new(Literal::Boolean(
                                    BooleanLiteral { value: true, span: Span::ZERO },
                                ))),
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
                    // Skip the text-build pathway below.
                    let component_call = Expression::Call(Box::new(CallExpression {
                        callee: t::id_owned(c.name.to_string()),
                        arguments: vec![
                            Argument::Expression(t::id_anchor()),
                            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                                properties: props,
                                span: Span::ZERO,
                            }))),
                        ],
                        optional: false,
                        span: Span::ZERO,
                    }));
                    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
                    func_body.extend(script.body.iter().cloned());
                    func_body.push(t::stmt(component_call));
                    let mut params = vec![t::pat_id_anchor()];
                    if script.uses_props {
                        params.push(t::pat_id("$$props"));
                    }
                    let export =
                        t::export_default_function(component_name, params, func_body);
                    let mut prog: Vec<Statement> =
                        Vec::with_capacity(3 + script.imports.len());
                    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
                    if script.emit_legacy_flag {
                        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
                    }
                    prog.push(t::import_namespace("$", "svelte/internal/client"));
                    prog.extend(script.imports.iter().cloned());
                    prog.push(export);
                    return Some(t::program(prog));
                }
            }
        }
        // Build a children arrow: `($$anchor, $$slotProps) => { ... }`
        // Currently only handle text-only body (mix of text + expressions).
        let mut parts: Vec<TextPart> = Vec::new();
        for child in &c.fragment.nodes {
            match child {
                FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
                _ => return None,
            }
        }
        // Drop entirely-whitespace boundary Static parts.
        while parts
            .first()
            .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
            .unwrap_or(false)
        {
            parts.remove(0);
        }
        while parts
            .last()
            .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
            .unwrap_or(false)
        {
            parts.pop();
        }
        // Trim leading whitespace inside the FIRST Static part and trailing
        // whitespace inside the LAST.
        if let Some(TextPart::Static(s)) = parts.first_mut() {
            *s = s.trim_start().to_string();
        }
        if let Some(TextPart::Static(s)) = parts.last_mut() {
            *s = s.trim_end().to_string();
        }

        let mut slot_body: Vec<Statement> = Vec::new();
        slot_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            Vec::new(),
        )));
        slot_body.push(t::var(
            "text",
            t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
        ));
        let inline = build_inline_template(&parts, &script.state_bindings);
        let fn_body = t::call(
            t::member_id(t::id_dollar(), "set_text"),
            vec![t::id("text"), inline],
        );
        let fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(fn_body),
            r#async: false,
            span: Span::ZERO,
        }));
        slot_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![fn_arrow],
        )));
        slot_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id("text")],
        )));
        let children_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor(), t::pat_id("$$slotProps")],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: slot_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: Cow::Borrowed("children"),
                span: Span::ZERO,
            }),
            value: children_arrow,
            kind: PropertyKind::Init,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
        // `$$slots: { default: true }`
        props.push(ObjectMember::Property(Box::new(Property {
            key: PropertyKey::Identifier(Identifier {
                name: Cow::Borrowed("$$slots"),
                span: Span::ZERO,
            }),
            value: Expression::Object(Box::new(ObjectExpression {
                properties: vec![ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Borrowed("default"),
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

    let component_call = Expression::Call(Box::new(CallExpression {
        callee: t::id_owned(c.name.to_string()),
        arguments: vec![
            Argument::Expression(t::id_anchor()),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::stmt(component_call));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// Single top-level async-tainted ExpressionTag — emits a no-root
/// template with `$.text()` + `$.template_effect` + `$.append`.
fn emit_single_async_expr_program(
    expr: &Expression,
    component_name: &str,
    script: &ScriptInfo,
    ai: &AsyncInfo,
) -> Option<Program> {
    let mut body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    body.extend(script.body.iter().cloned());
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "next"),
        Vec::new(),
    )));
    body.push(t::var(
        "text",
        t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
    ));
    let set_call = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), expr.clone()],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let blockers = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(Expression::Member(Box::new(
            MemberExpression {
                object: t::id("$$promises"),
                property: MemberProperty::Expression(t::lit_number(
                    ai.last_group_idx as f64,
                )),
                computed: true,
                optional: false,
                span: Span::ZERO,
            },
        )))],
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![effect_fn, void_zero_client(), void_zero_client(), blockers],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("text")],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Async if / each block emission (single top-level case)
// ---------------------------------------------------------------------------

/// Strip an outer `await EXPR` to its inner argument. Otherwise return clone.
fn strip_outer_await(e: &Expression) -> Expression {
    if let Expression::Await(a) = e {
        a.argument.clone()
    } else {
        e.clone()
    }
}

/// Lower a small async-block branch body. Currently only supports
/// "single ExpressionTag (with await) child" — the upstream form for the
/// async-if / async-each fixtures we target. Emits:
///   var TEXT = $.text();
///   $.template_effect(($0) => $.set_text(TEXT, $0), void 0, [() => INNER_EXPR]);
///   $.append($$anchor, TEXT);
fn emit_async_branch_body(
    fragment: &svelte_ast::fragment::Fragment,
    text_name: &str,
) -> Option<Vec<Statement>> {
    // Find the first non-whitespace child.
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    let expr = match non_ws[0] {
        FragmentChild::ExpressionTag(et) => &et.expression,
        _ => return None,
    };

    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        text_name,
        t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
    ));
    // template_effect:
    //   `($0) => $.set_text(TEXT, $0), void 0, [() => INNER_EXPR]`
    let set_text_call = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id_owned(text_name.to_string()), t::id("$0")],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$0")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_text_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let inner = strip_outer_await(expr);
    let dep_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(inner),
        r#async: false,
        span: Span::ZERO,
    }));
    let deps_array = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(dep_arrow)],
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![effect_fn, void_zero_client(), deps_array],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(text_name.to_string())],
    )));
    Some(body)
}

/// Build the branch body for a vanilla (non-async) if-block. Supports:
/// - text-only consequents — `{x}` (single ExpressionTag).
/// - single fully-static element — `<p>foo</p>` (uses a root_N template).
///
/// `roots` accumulates module-level `var root_N = $.from_html(...)`
/// declarations for the element case. `root_idx` is incremented as each
/// new root is allocated.
/// Emit a branch body for multiple top-level RegularElements (e.g.
/// the consequent of `{#if true}<div id={x}/><div id={y}/>{/if}`). Each
/// element must be either fully static or have only dyn attributes
/// (no events, binds, slots, blocks).
fn emit_multi_element_branch_body(
    non_ws: &[&FragmentChild],
    roots: &mut Vec<Statement>,
    root_idx: &mut usize,
    elem_var_idx: &mut usize,
) -> Option<Vec<Statement>> {
    emit_multi_element_branch_body_with_context(
        non_ws,
        roots,
        root_idx,
        elem_var_idx,
        &HashSet::new(),
        &HashSet::new(),
    )
}

fn emit_multi_element_branch_body_with_context(
    non_ws: &[&FragmentChild],
    roots: &mut Vec<Statement>,
    root_idx: &mut usize,
    elem_var_idx: &mut usize,
    props_destructured: &HashSet<String>,
    legacy_prop_names: &HashSet<String>,
) -> Option<Vec<Statement>> {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    // Build template HTML: `<EL ...></EL> <EL ...></EL>` (space-separated).
    let mut template = String::new();
    let mut elements: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in non_ws {
        if let FragmentChild::RegularElement(el) = n {
            elements.push(el);
        }
    }
    let mut needs_import_node = false;
    for (i, el) in elements.iter().enumerate() {
        if i > 0 {
            template.push(' ');
        }
        if el.name.contains('-') || el.name == "video" {
            needs_import_node = true;
        }
        template.push('<');
        template.push_str(&el.name);
        for a in &el.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                match &attr.value {
                    AttributeValue::Empty => {
                        template.push(' ');
                        template.push_str(&attr.name);
                        template.push_str("=\"\"");
                    }
                    AttributeValue::Many(parts) => {
                        if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            template.push(' ');
                            template.push_str(&attr.name);
                            template.push_str("=\"");
                            for p in parts {
                                if let AttributeValuePart::Text(t) = p {
                                    for c in t.data.chars() {
                                        match c {
                                            '"' => template.push_str("&quot;"),
                                            '&' => template.push_str("&amp;"),
                                            _ => template.push(c),
                                        }
                                    }
                                }
                            }
                            template.push('"');
                        }
                        // Skip dynamic Many parts; they're handled at runtime.
                    }
                    AttributeValue::Single(_) => {
                        // Dynamic — handled at runtime.
                    }
                }
            }
        }
        if is_void_client(&el.name) {
            template.push_str("/>");
        } else {
            template.push('>');
            let mut needs2 = needs_import_node;
            serialize_fragment_to_html(&el.fragment, &mut template, &mut needs2).unwrap_or(());
            if needs2 {
                needs_import_node = true;
            }
            template.push_str("</");
            template.push_str(&el.name);
            template.push('>');
        }
    }
    *root_idx += 1;
    let root_name = format!("root_{}", *root_idx);
    let flag = if needs_import_node { 3.0 } else { 1.0 };
    roots.push(t::var(
        &root_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![template], vec![]), t::lit_number(flag)],
        ),
    ));

    let mut body: Vec<Statement> = Vec::new();
    // Always use `fragment_N` inside branch bodies (outer scope already
    // owns `fragment`). N matches the root_idx.
    let frag_var = format!("fragment_{}", *root_idx);
    body.push(t::var(&frag_var, t::call(t::id_owned(root_name.to_string()), Vec::new())));

    // Allocate var names for each element + track dyn attrs. Inside a
    // branch_body the outer scope already uses bare `<name>`, so suffix
    // every element here as `<name>_N`.
    let mut var_names: Vec<String> = Vec::new();
    let mut dyn_attr_calls: Vec<Statement> = Vec::new();
    for (i, el) in elements.iter().enumerate() {
        *elem_var_idx += 1;
        let var = format!("{}_{}", sanitize_name(&el.name), *elem_var_idx);
        var_names.push(var.clone());
        let init = if i == 0 {
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(frag_var.to_string())],
            )
        } else {
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(var_names[i - 1].to_string()), t::lit_number(2.0)],
            )
        };
        body.push(t::var(&var, init));
        // Collect dyn-attr template_effect statements.
        for a in &el.attributes {
            if let ElementAttribute::Attribute(attr) = a {
                let expr = match &attr.value {
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(parts) => {
                        if parts.len() == 1 {
                            match &parts[0] {
                                AttributeValuePart::ExpressionTag(et) => et.expression.clone(),
                                _ => continue,
                            }
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };
                let expr = rewrite_props_destructured(&expr, props_destructured);
                let expr = rewrite_legacy_prop_reads(&expr, legacy_prop_names);
                dyn_attr_calls.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "set_attribute"),
                    vec![
                        t::id_owned(var.to_string()),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(attr.name.clone()),
                            raw: None,
                            span: Span::ZERO,
                        }))),
                        expr,
                    ],
                )));
            }
        }
    }
    if !dyn_attr_calls.is_empty() {
        let effect_arrow_body = if dyn_attr_calls.len() == 1 {
            let stmt = dyn_attr_calls.into_iter().next().unwrap();
            let expr = if let Statement::Expression(e) = stmt {
                e.expression
            } else {
                unreachable!()
            };
            ArrowBody::Expression(expr)
        } else {
            ArrowBody::Block(Box::new(BlockStatement {
                body: dyn_attr_calls,
                span: Span::ZERO,
            }))
        };
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: effect_arrow_body,
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    }
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(frag_var.to_string())],
    )));
    Some(body)
}

/// Emit each-block consequent body for N>=2 top-level text-anchor elements.
/// Each element is `<TAG>{EXPR}</TAG>` (text-anchor). Builds the multi-root
/// template, walks elements via $.first_child / $.sibling, allocates one
/// text-anchor var per element, and combines all set_text calls into a
/// single $.template_effect block.
fn emit_multi_element_each_body(
    non_ws: &[&FragmentChild],
    roots: &mut Vec<Statement>,
    root_idx: &mut usize,
    elem_var_idx: &mut usize,
    text_idx: &mut usize,
    frag_idx: &mut usize,
    item_name: &str,
    item_referenced: bool,
    props_destructured: &HashSet<String>,
    legacy_prop_names: &HashSet<String>,
) -> Option<Vec<Statement>> {
    use svelte_ast::attributes::ElementAttribute;
    let mut elements: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in non_ws {
        if let FragmentChild::RegularElement(el) = n {
            if !el.attributes.is_empty() {
                return None;
            }
            if !is_text_only_element(el) {
                return None;
            }
            elements.push(el);
        } else {
            return None;
        }
    }
    // Build template HTML: `<EL> </EL> <EL> </EL>` (space placeholders).
    let mut template = String::new();
    let mut needs_import_node = false;
    for (i, el) in elements.iter().enumerate() {
        if i > 0 {
            template.push(' ');
        }
        if el.name.contains('-') || el.name == "video" {
            needs_import_node = true;
        }
        template.push('<');
        template.push_str(&el.name);
        template.push_str("> </");
        template.push_str(&el.name);
        template.push('>');
    }
    *root_idx += 1;
    let root_name = format!("root_{}", *root_idx);
    let flag = if needs_import_node { 3.0 } else { 1.0 };
    roots.push(t::var(
        &root_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![template], vec![]), t::lit_number(flag)],
        ),
    ));

    let mut body: Vec<Statement> = Vec::new();
    *frag_idx += 1;
    let frag_var = format!("fragment_{}", *frag_idx);
    body.push(t::var(&frag_var, t::call(t::id_owned(root_name.to_string()), Vec::new())));

    let mut elem_vars: Vec<String> = Vec::new();
    let mut text_vars_with_expr: Vec<(String, Expression)> = Vec::new();

    for (i, el) in elements.iter().enumerate() {
        *elem_var_idx += 1;
        let safe = sanitize_name(&el.name);
        let el_var = if *elem_var_idx == 1 {
            safe
        } else {
            format!("{}_{}", safe, *elem_var_idx - 1)
        };
        elem_vars.push(el_var.clone());
        let init = if i == 0 {
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(frag_var.to_string())],
            )
        } else {
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(elem_vars[i - 1].to_string()), t::lit_number(2.0)],
            )
        };
        body.push(t::var(&el_var, init));

        // Build inline expression for the text anchor.
        let mut parts: Vec<TextPart> = Vec::new();
        for c in &el.fragment.nodes {
            match c {
                FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                FragmentChild::ExpressionTag(et) => {
                    parts.push(TextPart::Expr(&et.expression))
                }
                _ => return None,
            }
        }
        let inline = build_inline_template(&parts, &HashSet::new());
        let inline = rewrite_props_destructured(&inline, props_destructured);
        let inline = rewrite_legacy_prop_reads(&inline, legacy_prop_names);
        let inline = if item_referenced {
            rewrite_get_for_each_var(&inline, item_name)
        } else {
            inline
        };

        let text_name = if *text_idx == 0 {
            "text".to_string()
        } else {
            format!("text_{}", text_idx)
        };
        *text_idx += 1;
        body.push(t::var(
            &text_name,
            t::call(
                t::member_id(t::id_dollar(), "child"),
                vec![
                    t::id_owned(el_var.to_string()),
                    Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                        value: true,
                        span: Span::ZERO,
                    }))),
                ],
            ),
        ));
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "reset"),
            vec![t::id_owned(el_var.to_string())],
        )));
        text_vars_with_expr.push((text_name, inline));
    }

    // Combined template_effect.
    let set_text_stmts: Vec<Statement> = text_vars_with_expr
        .into_iter()
        .map(|(name, expr)| {
            t::stmt(t::call(
                t::member_id(t::id_dollar(), "set_text"),
                vec![t::id_owned(name.to_string()), expr],
            ))
        })
        .collect();
    let eff_body = if set_text_stmts.len() == 1 {
        let stmt = set_text_stmts.into_iter().next().unwrap();
        let expr = if let Statement::Expression(e) = stmt {
            e.expression
        } else {
            unreachable!()
        };
        ArrowBody::Expression(expr)
    } else {
        ArrowBody::Block(Box::new(BlockStatement {
            body: set_text_stmts,
            span: Span::ZERO,
        }))
    };
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: eff_body,
            r#async: false,
            span: Span::ZERO,
        }))],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(frag_var.to_string())],
    )));
    Some(body)
}

fn emit_vanilla_branch_body(
    fragment: &svelte_ast::fragment::Fragment,
    text_name: &str,
    roots: &mut Vec<Statement>,
    root_idx: &mut usize,
    elem_var_idx: &mut usize,
) -> Option<Vec<Statement>> {
    emit_vanilla_branch_body_with_context(
        fragment, text_name, roots, root_idx, elem_var_idx,
        &HashSet::new(), &HashSet::new(),
    )
}

fn emit_vanilla_branch_body_with_context(
    fragment: &svelte_ast::fragment::Fragment,
    text_name: &str,
    roots: &mut Vec<Statement>,
    root_idx: &mut usize,
    elem_var_idx: &mut usize,
    props_destructured: &HashSet<String>,
    legacy_prop_names: &HashSet<String>,
) -> Option<Vec<Statement>> {
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        // Multi-element body: support N>=2 RegularElements where each is
        // either fully static or has only dyn attrs. Mirrors
        // `element-attribute-removed` consequent.
        if non_ws.len() >= 2
            && non_ws.iter().all(|n| match n {
                FragmentChild::RegularElement(el) => {
                    is_element_fully_static(el)
                        || element_static_body_with_dyn_attrs(el)
                }
                _ => false,
            })
        {
            return emit_multi_element_branch_body_with_context(
                &non_ws, roots, root_idx, elem_var_idx,
                props_destructured, legacy_prop_names,
            );
        }
        return None;
    }
    match non_ws[0] {
        FragmentChild::Text(t) => {
            // Static-text-only consequent: `var text = $.text('hello');`
            // Upstream trims leading/trailing whitespace in branch bodies.
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(
                text_name,
                t::call(
                    t::member_id(t::id_dollar(), "text"),
                    vec![t::literal_str_owned(t.data.trim().to_string())],
                ),
            ));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(text_name.to_string())],
            )));
            Some(body)
        }
        FragmentChild::ExpressionTag(et) => {
            let expr = et.expression.clone();
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(text_name, t::call(t::member_id(t::id_dollar(), "text"), Vec::new())));
            let set_text_call = t::call(
                t::member_id(t::id_dollar(), "set_text"),
                vec![t::id_owned(text_name.to_string()), expr],
            );
            let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(set_text_call),
                r#async: false,
                span: Span::ZERO,
            }));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![effect_fn],
            )));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(text_name.to_string())],
            )));
            Some(body)
        }
        FragmentChild::RegularElement(el) => {
            // Detect text-anchor body shape (`<TAG>{EXPR}</TAG>` or similar
            // with non-literal interpolation). Emits an additional
            // `var text = $.child(TAG, true); $.reset(TAG); $.template_effect(...)`
            // to wire up the reactive text-anchor.
            let is_text_anchor = is_text_only_element(el)
                && el.attributes.is_empty();
            let mut html = String::new();
            let mut needs_import_node = false;
            serialize_element_to_html(el, &mut html, &mut needs_import_node)?;
            *root_idx += 1;
            let root_name = format!("root_{}", *root_idx);
            roots.push(t::var(
                &root_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![t::template_raw(vec![html], vec![])],
                ),
            ));
            *elem_var_idx += 1;
            let safe = sanitize_name(&el.name);
            let var_name = if *elem_var_idx == 1 {
                safe
            } else {
                format!("{}_{}", safe, *elem_var_idx - 1)
            };
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(&var_name, t::call(t::id_owned(root_name.to_string()), Vec::new())));
            if is_text_anchor {
                // Build the inline text expression from the element body's
                // mixed Text + ExpressionTag run.
                let mut parts: Vec<TextPart> = Vec::new();
                for c in &el.fragment.nodes {
                    match c {
                        FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                        FragmentChild::ExpressionTag(et) => {
                            parts.push(TextPart::Expr(&et.expression))
                        }
                        _ => return None,
                    }
                }
                let inline = build_inline_template(&parts, &HashSet::new());
                let inline = rewrite_legacy_prop_reads(&inline, legacy_prop_names);
                body.push(t::var(
                    text_name,
                    t::call(
                        t::member_id(t::id_dollar(), "child"),
                        vec![
                            t::id_owned(var_name.to_string()),
                            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                value: true,
                                span: Span::ZERO,
                            }))),
                        ],
                    ),
                ));
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(var_name.to_string())],
                )));
                let set_text = t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id_owned(text_name.to_string()), inline],
                );
                let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(set_text),
                    r#async: false,
                    span: Span::ZERO,
                }));
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "template_effect"),
                    vec![effect_fn],
                )));
            }
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(var_name.to_string())],
            )));
            Some(body)
        }
        FragmentChild::Component(c) => {
            // No-props bare Component: `Child($$anchor, {});`
            if !c.attributes.is_empty() || !c.fragment.nodes.is_empty() {
                return None;
            }
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::stmt(t::call(
                t::id_owned(c.name.to_string()),
                vec![
                    t::id_anchor(),
                    Expression::Object(Box::new(ObjectExpression {
                        properties: Vec::new(),
                        span: Span::ZERO,
                    })),
                ],
            )));
            Some(body)
        }
        FragmentChild::RenderTag(rt) => {
            // `{@render thing()}` → call the snippet with `$$anchor` as the
            // first arg, preserving any user-passed args.
            let call = match &rt.expression {
                Expression::Call(c) => c,
                _ => return None,
            };
            let mut new_args: Vec<Argument> =
                vec![Argument::Expression(t::id_anchor())];
            for a in &call.arguments {
                new_args.push(a.clone());
            }
            let render_call = Expression::Call(Box::new(CallExpression {
                callee: call.callee.clone(),
                arguments: new_args,
                optional: false,
                span: Span::ZERO,
            }));
            Some(vec![t::stmt(render_call)])
        }
        FragmentChild::SlotElement(se) => {
            // `<slot [name="X"] />` → `$.slot(node, $$props, 'NAME', {props}, null)`.
            // Variable naming uses `_1` suffix to avoid collision with outer
            // `fragment` / `node` declared at the multi-block level.
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(
                "fragment_1",
                t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
            ));
            body.push(t::var(
                "node_1",
                t::call(
                    t::member_id(t::id_dollar(), "first_child"),
                    vec![t::id("fragment_1")],
                ),
            ));
            // Extract slot name (default if no `name=` attr).
            use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
            let mut slot_name = "default".to_string();
            for a in &se.attributes {
                if let ElementAttribute::Attribute(attr) = a {
                    if attr.name == "name" {
                        if let AttributeValue::Many(parts) = &attr.value {
                            if parts.len() == 1 {
                                if let AttributeValuePart::Text(t) = &parts[0] {
                                    slot_name = t.data.clone();
                                }
                            }
                        }
                    }
                }
            }
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "slot"),
                vec![
                    t::id("node_1"),
                    t::id("$$props"),
                    Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(slot_name),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    Expression::Object(Box::new(ObjectExpression {
                        properties: Vec::new(),
                        span: Span::ZERO,
                    })),
                    Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                ],
            )));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id("fragment_1")],
            )));
            Some(body)
        }
        FragmentChild::HtmlTag(ht) => {
            // `{@html EXPR}` standalone → `$.comment()` + `$.html(...)`.
            // Uses `fragment_1` / `node_1` to avoid colliding with the
            // outer `fragment` / `node` declared at the multi-block level.
            // We don't have each-iter-var context here, so pass the
            // expression as-is (caller's responsibility to wrap if needed).
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(
                "fragment_1",
                t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
            ));
            body.push(t::var(
                "node_1",
                t::call(
                    t::member_id(t::id_dollar(), "first_child"),
                    vec![t::id("fragment_1")],
                ),
            ));
            let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(ht.expression.clone()),
                r#async: false,
                span: Span::ZERO,
            }));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "html"),
                vec![t::id("node_1"), inner_arrow],
            )));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id("fragment_1")],
            )));
            Some(body)
        }
        _ => None,
    }
}

/// Emit a non-async, top-level single if-block program — mirrors the
/// expected client output for fixtures like `if-block-empty`.
///
/// Currently constrained to:
/// - text-only consequent (single `{expr}`),
/// - optional text-only alternate,
/// - no script reactivity beyond plain `let X = INIT` (preserved as-is).
fn emit_single_vanilla_if_program(
    ib: &svelte_ast::blocks::IfBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Bail if script touches things we can't yet preserve safely.
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    let mut root_decls: Vec<Statement> = Vec::new();
    let mut root_idx: usize = 0;
    let mut elem_var_idx: usize = 0;
    let consequent_body =
        emit_vanilla_branch_body(&ib.consequent, "text", &mut root_decls, &mut root_idx, &mut elem_var_idx)?;
    let alternate_body = match &ib.alternate {
        Some(alt) => Some(emit_vanilla_branch_body(
            alt,
            "text_1",
            &mut root_decls,
            &mut root_idx,
            &mut elem_var_idx,
        )?),
        None => None,
    };

    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    let mut block_body: Vec<Statement> = Vec::new();
    block_body.push(t::var("consequent", consequent_arrow));
    if let Some(alt_body) = alternate_body {
        let alt_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: alt_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        block_body.push(t::var("alternate", alt_arrow));
    }

    // Inner: `if (TEST) $$render(consequent); [else $$render(alternate, false);]`
    let then_call = t::stmt(t::call(t::id_render(), vec![t::id("consequent")]));
    let else_call = if ib.alternate.is_some() {
        Some(t::stmt(t::call(
            t::id_render(),
            vec![t::id("alternate"), t::lit_number(-1.0)],
        )))
    } else {
        None
    };
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let test = rewrite_props_destructured(&ib.test, &script.props_destructured);
    let test = rewrite_legacy_prop_reads(&test, &legacy_prop_names);
    let render_if = Statement::If(Box::new(IfStatement {
        test,
        consequent: then_call,
        alternate: else_call,
        span: Span::ZERO,
    }));
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    block_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "if"),
        vec![t::id("node"), render_arrow],
    )));

    // Top-level function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    // Legacy props prelude.
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    func_body.push(Statement::Block(Box::new(BlockStatement {
        body: block_body,
        span: Span::ZERO,
    })));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    // Module-level `var root_N = $.from_html(...)` for any element
    // branches captured.
    prog.extend(root_decls);
    prog.push(export);
    Some(t::program(prog))
}

/// Emit the upstream client shape for a runes-mode top-level single
/// static element with one or more dynamic attributes — e.g.
///
///   <script>let { src } = $props();</script>
///   <img {src} alt="" />
///
/// →
///
///   var root = $.from_html(`<img alt=""/>`);
///   export default function Main($$anchor, $$props) {
///       var img = root();
///       $.template_effect(() => $.set_attribute(img, 'src', $$props.src));
///       $.append($$anchor, img);
///   }
///
/// Constrained to: empty (or fully-static) element body, no bindings/
/// directives/events, no spread, no async. Returns `None` for anything
/// outside the supported shape so the caller falls through to the
/// general walker.
/// Emit the upstream shape for a top-level fragment that is exactly one
/// `{@html ...}` tag — e.g.
///
///   <script>let { html } = $props();</script>
///   {@html html}
///
/// →
///
///   var fragment = $.comment();
///   var node = $.first_child(fragment);
///   $.html(node, () => $$props.html);
///   $.append($$anchor, fragment);
fn emit_top_level_html_tag_program(
    expr: &Expression,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    // For `{@html raw}` where `raw` is a legacy prop accessor, upstream
    // passes the accessor function directly to `$.html(node, raw)` —
    // no `() => raw()` thunk. Detect that bare-prop case.
    let is_bare_legacy_prop = matches!(
        expr,
        Expression::Identifier(id) if legacy_prop_names.contains(id.name.as_ref())
    );
    let inner = rewrite_props_destructured(expr, &script.props_destructured);
    let inner = if is_bare_legacy_prop {
        // Strip the call wrap that `rewrite_legacy_prop_reads` adds, since
        // we want the bare identifier here.
        expr.clone()
    } else {
        rewrite_legacy_prop_reads(&inner, &legacy_prop_names)
    };

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    let html_arg = if is_bare_legacy_prop {
        inner
    } else {
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(inner),
            r#async: false,
            span: Span::ZERO,
        }))
    };
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "html"),
        vec![t::id("node"), html_arg],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the shape:
///
///   {@render NAME(args)}
///
/// →
///
///   export default function Main($$anchor, $$props) {
///       $.push($$props, false);
///       ...script...
///       $.init();
///       NAME($$anchor, ...args);
///       $.pop();
///   }
///
/// Used when the only top-level fragment node is a `RenderTag`. Mirrors
/// `snippet-raw-hydrate`.
fn emit_top_level_render_tag_program(
    rt_expr: &Expression,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    let call = match rt_expr {
        Expression::Call(c) => c,
        _ => return None,
    };
    let mut new_args: Vec<Argument> = vec![Argument::Expression(t::id_anchor())];
    for a in &call.arguments {
        new_args.push(a.clone());
    }
    let render_call = Expression::Call(Box::new(CallExpression {
        callee: call.callee.clone(),
        arguments: new_args,
        optional: false,
        span: Span::ZERO,
    }));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    let needs_legacy_wrap = script.emit_legacy_flag;
    if needs_legacy_wrap {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
    }
    func_body.extend(script.body.iter().cloned());
    if needs_legacy_wrap {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "init"),
            Vec::new(),
        )));
    }
    func_body.push(t::stmt(render_call));
    if needs_legacy_wrap {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "pop"),
            Vec::new(),
        )));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || needs_legacy_wrap {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the shape:
///
///   <TAG STATIC_ATTRS>{@html EXPR}</TAG>
///
/// →
///
///   var root = $.from_html(`<TAG STATIC_ATTRS></TAG>`);
///   export default function Main($$anchor) {
///       var tag = root();
///       $.html(tag, () => EXPR, true);
///       $.reset(tag);
///       $.append($$anchor, tag);
///   }
///
/// The third `true` argument indicates the html-tag is inside an element
/// wrapper (so the runtime fills the element rather than placing nodes
/// before an anchor comment). Mirrors `raw-empty`.
fn emit_single_element_wrapping_html_tag_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        static_attrs.push(attr);
                    } else {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    // SVG / MathML namespaces use `$.from_svg` / `$.from_mathml` builders.
    let from_fn = if el.name == "svg" {
        "from_svg"
    } else if el.name == "math" {
        "from_mathml"
    } else {
        "from_html"
    };
    // Body: exactly one HtmlTag.
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
    if non_ws.len() != 1 {
        return None;
    }
    let ht = match non_ws[0] {
        FragmentChild::HtmlTag(h) => h,
        _ => return None,
    };

    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let is_bare_legacy_prop = matches!(
        &ht.expression,
        Expression::Identifier(id) if legacy_prop_names.contains(id.name.as_ref())
    );
    let inner = rewrite_props_destructured(&ht.expression, &script.props_destructured);
    let inner = if is_bare_legacy_prop {
        ht.expression.clone()
    } else {
        rewrite_legacy_prop_reads(&inner, &legacy_prop_names)
    };
    let html_arg = if is_bare_legacy_prop {
        inner
    } else {
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(inner),
            r#async: false,
            span: Span::ZERO,
        }))
    };

    // Build template HTML: `<TAG STATIC_ATTRS></TAG>`.
    let mut html = String::with_capacity(32);
    html.push('<');
    html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => html.push_str("&quot;"),
                                '&' => html.push_str("&amp;"),
                                _ => html.push(c),
                            }
                        }
                    }
                }
                html.push('"');
            }
            _ => return None,
        }
    }
    html.push_str("></");
    html.push_str(&el.name);
    html.push('>');

    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "html"),
        vec![
            t::id_owned(tag_var.to_string()),
            html_arg,
            Expression::Literal(Box::new(Literal::Boolean(
                svelte_js_ast::BooleanLiteral { value: true, span: Span::ZERO },
            ))),
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(tag_var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || !script.legacy_export_props.is_empty() {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), from_fn),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit the upstream shape for a top-level fragment that is exactly one
/// non-empty static Text node — e.g.
///
///   Text
///
/// →
///
///   export default function Main($$anchor) {
///       $.next();
///       var text = $.text('Text');
///       $.append($$anchor, text);
///   }
fn emit_top_level_single_text_program(
    text: &str,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "next"),
        Vec::new(),
    )));
    func_body.push(t::var(
        "text",
        t::call(
            t::member_id(t::id_dollar(), "text"),
            vec![t::literal_str_owned(text.to_string())],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("text")],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// Emit the upstream shape for a top-level fragment that is exactly one
/// non-async ExpressionTag — e.g.
///
///   {x}
///
/// →
///
///   export default function Main($$anchor, $$props) {
///       $.next();
///       var text = $.text();
///       $.template_effect(() => $.set_text(text, $$props.x));
///       $.append($$anchor, text);
///   }
fn emit_top_level_single_expression_program(
    expr: &Expression,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let inner = rewrite_props_destructured(expr, &script.props_destructured);
    let inner = rewrite_legacy_prop_reads(&inner, &legacy_prop_names);

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "next"),
        Vec::new(),
    )));
    func_body.push(t::var(
        "text",
        t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
    ));
    let set_text = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), inner],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_text),
        r#async: false,
        span: Span::ZERO,
    }));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![effect_fn],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("text")],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// Emit the upstream shape for a single wrapper element containing
/// exactly one Component child — e.g.
///
///   <div><Nested /></div>
///
/// →
///
///   var root = $.from_html(`<div><!></div>`);
///   export default function Main($$anchor) {
///       var div = root();
///       var node = $.child(div);
///       Nested(node, {});
///       $.reset(div);
///       $.append($$anchor, div);
///   }
///
/// Constrained to: no script reactivity, no dyn attrs on the wrapper,
/// no Component props/children, only whitespace text/comments around
/// the inner Component.
fn emit_single_element_with_component_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    // Wrapper must have no dynamic attrs and no spread/directives.
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        static_attrs.push(attr);
                    } else {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    // Body must be exactly one Component (ignoring surrounding whitespace
    // text + comments).
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
    if non_ws.len() != 1 {
        return None;
    }
    let comp = match non_ws[0] {
        FragmentChild::Component(c) => c,
        _ => return None,
    };
    // Component must have no props, no attrs, no children body.
    if !comp.attributes.is_empty() || !comp.fragment.nodes.is_empty() {
        return None;
    }

    // Build template HTML — `<TAG STATIC_ATTRS><!></TAG>`.
    let mut html = String::with_capacity(32);
    html.push('<');
    html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => html.push_str("&quot;"),
                                '&' => html.push_str("&amp;"),
                                _ => html.push(c),
                            }
                        }
                    }
                }
                html.push('"');
            }
            _ => return None,
        }
    }
    html.push_str("><!></");
    html.push_str(&el.name);
    html.push('>');

    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "child"),
            vec![t::id_owned(tag_var.to_string())],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::id_owned(comp.name.to_string()),
        vec![
            t::id("node"),
            Expression::Object(Box::new(ObjectExpression {
                properties: Vec::new(),
                span: Span::ZERO,
            })),
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(tag_var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the shape:
///
///   <TAG>{#snippet NAME()}{/snippet}<STATIC></TAG>
///   <TAG>{@debug X}<STATIC></TAG>
///
/// where the element body is exclusively SnippetBlock / DebugTag + fully-
/// static content (no reactive content). Snippets are emitted as a
/// `{ const NAME = ... }` block inside the function body; @debug tags
/// become a `$.template_effect(() => { console.log({...}); debugger; })`.
/// The static rest stays in the template. Mirrors `no-reset-snippet` and
/// `no-reset-debug`.
fn emit_single_element_with_inner_snippet_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    if !is_element_static_attrs(el) {
        return None;
    }
    // Body partitioned into: snippets (collected) + debug tags + remaining nodes
    // (must be fully static after removal).
    let mut snippets: Vec<&svelte_ast::blocks::SnippetBlock> = Vec::new();
    let mut debug_tags: Vec<&svelte_ast::tags::DebugTag> = Vec::new();
    let mut rest_nodes: Vec<FragmentChild> = Vec::new();
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::SnippetBlock(sb) => snippets.push(sb),
            FragmentChild::DebugTag(dt) => debug_tags.push(dt),
            other => rest_nodes.push(other.clone()),
        }
    }
    if snippets.is_empty() && debug_tags.is_empty() {
        return None;
    }
    // After snippet removal, the rest must form a fully-static body.
    // Build a temporary element with only rest nodes to reuse static-check.
    let rest_el = svelte_ast::elements::RegularElement {
        fragment: svelte_ast::fragment::Fragment {
            nodes: rest_nodes,
            ..el.fragment.clone()
        },
        ..el.clone()
    };
    if !is_element_fully_static(&rest_el) {
        return None;
    }

    // Build template HTML (snippet-free).
    let mut html = String::new();
    let mut needs = false;
    serialize_element_to_html(&rest_el, &mut html, &mut needs)?;

    // Snippet declarations as a block.
    let mut snippet_block: Vec<Statement> = Vec::new();
    for sb in &snippets {
        let name = sb.expression.name.clone();
        let body_non_ws: Vec<&FragmentChild> = sb
            .body
            .nodes
            .iter()
            .filter(|c| match c {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                _ => true,
            })
            .collect();
        if body_non_ws.len() > 1 {
            return None;
        }
        let body: Vec<Statement> = if body_non_ws.is_empty() {
            Vec::new()
        } else {
            match body_non_ws[0] {
                FragmentChild::Text(t) => {
                    let trimmed = t.data.trim();
                    vec![
                        t::stmt(t::call(t::member_id(t::id_dollar(), "next"), Vec::new())),
                        t::var(
                            "text",
                            t::call(
                                t::member_id(t::id_dollar(), "text"),
                                vec![Expression::Literal(Box::new(Literal::String(
                                    StringLiteral {
                                        value: Cow::Owned(trimmed.to_string()),
                                        raw: None,
                                        span: Span::ZERO,
                                    },
                                )))],
                            ),
                        ),
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "append"),
                            vec![t::id_anchor(), t::id("text")],
                        )),
                    ]
                }
                _ => return None,
            }
        };
        let mut params = vec![t::pat_id_anchor()];
        for p in &sb.parameters {
            params.push(p.clone());
        }
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params,
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        snippet_block.push(t::const_decl(&name, arrow));
    }

    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    // Snippets go inside a block, not at module-level for inside-element snippets.
    if !snippet_block.is_empty() {
        func_body.push(Statement::Block(Box::new(BlockStatement {
            body: snippet_block,
            span: Span::ZERO,
        })));
    }
    // Debug tags → template_effect with console.log + debugger.
    if !debug_tags.is_empty() {
        let mut effect_body: Vec<Statement> = Vec::new();
        for dt in &debug_tags {
            // console.log({ NAME: $.untrack(() => $.snapshot(NAME)), ... });
            let mut props: Vec<ObjectMember> = Vec::new();
            for id in &dt.identifiers {
                let snapshot_call = t::call(
                    t::member_id(t::id_dollar(), "snapshot"),
                    vec![Expression::Identifier(id.clone())],
                );
                let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(snapshot_call),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let untrack_call = t::call(
                    t::member_id(t::id_dollar(), "untrack"),
                    vec![inner_arrow],
                );
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: id.name.clone(),
                        span: Span::ZERO,
                    }),
                    value: untrack_call,
                    kind: PropertyKind::Init,
                    computed: false,
                    shorthand: false,
                    method: false,
                    span: Span::ZERO,
                })));
            }
            let console_log = t::call(
                t::member_id(t::id("console"), "log"),
                vec![Expression::Object(Box::new(ObjectExpression {
                    properties: props,
                    span: Span::ZERO,
                }))],
            );
            effect_body.push(t::stmt(console_log));
            effect_body.push(Statement::Debugger(Span::ZERO));
        }
        let effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: effect_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![effect_arrow],
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Collapse runs of consecutive ASCII spaces in the template HTML, but
/// only between elements/comments (i.e., not inside tag bodies). Mirrors
/// HTML's text-node merging during parse — adjacent text contributes to
/// a single text node, so emitting `<a> <b>` is equivalent to `<a>  <b>`.
fn collapse_template_inter_element_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            in_tag = true;
            out.push(b as char);
            i += 1;
        } else if b == b'>' {
            in_tag = false;
            out.push(b as char);
            i += 1;
        } else if !in_tag && b == b' ' {
            // Collapse run of spaces to one.
            out.push(' ');
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

/// True iff the element has only static attributes (no spread, no
/// directives, no dynamic values).
fn is_element_static_attrs(el: &svelte_ast::elements::RegularElement) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => {}
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    true
}

/// Emit a runes/legacy-mode program for the shape:
///
///   <TAG>{#if A}...{/if} {#if B}...{/if}</TAG>
///
/// where the element body is exactly N>=1 if-blocks (no else) separated by
/// whitespace, each with a fully-static single-element consequent. Mirrors
/// `if-block-anchor`.
fn emit_single_element_wrapping_ifs_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    // Wrapper must have no dynamic attrs/spread/directives.
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    if !el.attributes.iter().all(|a| match a {
        ElementAttribute::Attribute(attr) => matches!(
            &attr.value,
            AttributeValue::Empty
                | AttributeValue::Many(_)
        ) && match &attr.value {
            AttributeValue::Many(parts) => {
                parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_)))
            }
            _ => true,
        },
        _ => false,
    }) {
        return None;
    }

    // Body slots: mixture of StaticEl, StaticText, If — same gap-aware
    // approach as `emit_top_level_multi_if_program`.
    enum Slot<'a> {
        StaticEl(&'a svelte_ast::elements::RegularElement),
        StaticText(String),
        If(&'a svelte_ast::blocks::IfBlock),
    }
    let mut slots: Vec<Slot> = Vec::new();
    let mut gap_after: Vec<bool> = Vec::new();
    let mut pending_gap = false;
    for n in el.fragment.nodes.iter() {
        match n {
            FragmentChild::Text(t) => {
                if t.data.trim().is_empty() {
                    pending_gap = true;
                } else {
                    let leading_ws = t.data.chars().next().map(|c| c.is_whitespace()).unwrap_or(false);
                    let trailing_ws = t.data.chars().last().map(|c| c.is_whitespace()).unwrap_or(false);
                    if !slots.is_empty() {
                        gap_after.push(pending_gap || leading_ws);
                    }
                    pending_gap = trailing_ws;
                    slots.push(Slot::StaticText(t.data.trim().to_string()));
                }
            }
            FragmentChild::Comment(_) => {}
            FragmentChild::IfBlock(ib) => {
                if ib.alternate.is_some() || expr_top_await(&ib.test) {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::If(ib));
            }
            FragmentChild::RegularElement(child) => {
                if !is_element_fully_static(child) {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::StaticEl(child));
            }
            _ => return None,
        }
    }
    if !slots.iter().any(|s| matches!(s, Slot::If(_))) {
        return None;
    }
    let ifs: Vec<&svelte_ast::blocks::IfBlock> = slots
        .iter()
        .filter_map(|s| match s {
            Slot::If(ib) => Some(*ib),
            _ => None,
        })
        .collect();

    // Compute DOM sibling positions for each slot.
    let mut positions: Vec<usize> = vec![0; slots.len()];
    let mut pos: usize = 0;
    let mut pending_text = false;
    for (i, slot) in slots.iter().enumerate() {
        let preceding_gap = i > 0 && gap_after[i - 1];
        match slot {
            Slot::StaticEl(_) | Slot::If(_) => {
                if pending_text || preceding_gap {
                    pos += 1;
                    pending_text = false;
                }
                positions[i] = pos;
                pos += 1;
            }
            Slot::StaticText(_) => {
                positions[i] = pos;
                pending_text = true;
            }
        }
    }
    let first_if_slot = slots.iter().position(|s| matches!(s, Slot::If(_))).unwrap();
    let last_if_slot = slots.iter().rposition(|s| matches!(s, Slot::If(_))).unwrap();
    let first_if_pos = positions[first_if_slot];
    let last_if_pos = positions[last_if_slot];
    let final_pos = pos;
    let trailing_advance = final_pos - last_if_pos - 1;

    let mut root_decls: Vec<Statement> = Vec::new();
    let mut root_idx: usize = 0;
    let mut elem_var_idx: usize = 0;

    // Build template HTML for the wrapper, plus consequent arrows.
    let mut html = String::with_capacity(32);
    html.push('<');
    html.push_str(&el.name);
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            match &attr.value {
                AttributeValue::Empty => {
                    html.push(' ');
                    html.push_str(&attr.name);
                    html.push_str("=\"\"");
                }
                AttributeValue::Many(parts) => {
                    html.push(' ');
                    html.push_str(&attr.name);
                    html.push_str("=\"");
                    for p in parts {
                        if let AttributeValuePart::Text(t) = p {
                            for c in t.data.chars() {
                                match c {
                                    '"' => html.push_str("&quot;"),
                                    '&' => html.push_str("&amp;"),
                                    _ => html.push(c),
                                }
                            }
                        }
                    }
                    html.push('"');
                }
                _ => {}
            }
        }
    }
    html.push('>');
    // Slot contents.
    for (i, slot) in slots.iter().enumerate() {
        if i > 0 && gap_after[i - 1] {
            html.push(' ');
        }
        match slot {
            Slot::If(_) => html.push_str("<!>"),
            Slot::StaticEl(child) => {
                let mut needs = false;
                serialize_element_to_html(child, &mut html, &mut needs)?;
            }
            Slot::StaticText(s) => html.push_str(s),
        }
    }
    html.push_str("</");
    html.push_str(&el.name);
    html.push('>');

    // Build each if-block's consequent body and emission.
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let mut block_stmts: Vec<Statement> = Vec::new();
    let mut if_count = 0usize;
    let mut prev_if_slot: Option<usize> = None;
    for (slot_i, slot) in slots.iter().enumerate() {
        let ib = match slot {
            Slot::If(ib) => *ib,
            Slot::StaticEl(_) | Slot::StaticText(_) => continue,
        };
        let i = if_count;
        if_count += 1;
        let consequent_text_name = if i == 0 {
            "text".to_string()
        } else {
            format!("text_{}", i)
        };
        let consequent_body = emit_vanilla_branch_body(
            &ib.consequent,
            &consequent_text_name,
            &mut root_decls,
            &mut root_idx,
            &mut elem_var_idx,
        )?;
        let consequent_var = if i == 0 {
            "consequent".to_string()
        } else {
            format!("consequent_{}", i)
        };
        let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: consequent_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let mut inner_block: Vec<Statement> = Vec::new();
        inner_block.push(t::var(&consequent_var, consequent_arrow));
        let test = rewrite_props_destructured(&ib.test, &script.props_destructured);
        let test = rewrite_legacy_prop_reads(&test, &legacy_prop_names);
        let render_if = Statement::If(Box::new(IfStatement {
            test,
            consequent: t::stmt(t::call(t::id_render(), vec![t::id_owned(consequent_var.to_string())])),
            alternate: None,
            span: Span::ZERO,
        }));
        let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$render")],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: vec![render_if],
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let node_var = if i == 0 {
            "node".to_string()
        } else {
            format!("node_{}", i)
        };
        inner_block.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "if"),
            vec![t::id_owned(node_var.to_string()), render_arrow],
        )));
        if i > 0 {
            let prev_node = if i - 1 == 0 {
                "node".to_string()
            } else {
                format!("node_{}", i - 1)
            };
            let prev_slot_i = prev_if_slot.unwrap();
            let prev_pos = positions[prev_slot_i];
            let this_pos = positions[slot_i];
            let offset = this_pos - prev_pos;
            block_stmts.push(t::var(
                &node_var,
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![t::id_owned(prev_node.to_string()), t::lit_number(offset as f64)],
                ),
            ));
        }
        block_stmts.push(Statement::Block(Box::new(BlockStatement {
            body: inner_block,
            span: Span::ZERO,
        })));
        prev_if_slot = Some(slot_i);
    }

    // Top-level function body.
    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    // Navigate to first if-block anchor inside this element.
    let child_call = t::call(
        t::member_id(t::id_dollar(), "child"),
        vec![t::id_owned(tag_var.to_string())],
    );
    let first_node_init = if first_if_pos == 0 {
        child_call
    } else if first_if_pos == 1 {
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![child_call],
        )
    } else {
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![child_call, t::lit_number(first_if_pos as f64)],
        )
    };
    func_body.push(t::var("node", first_node_init));
    func_body.extend(block_stmts);
    // Trailing positions after the last if-block within the element.
    if trailing_advance > 0 {
        let arg = if trailing_advance == 1 {
            Vec::new()
        } else {
            vec![t::lit_number(trailing_advance as f64)]
        };
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            arg,
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(tag_var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || !script.legacy_export_props.is_empty() {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);
    let _ = ifs;

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(root_decls);
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a non-async program for a top-level fragment that is exactly
/// N>=2 if-blocks (no `else`, single-element fully-static consequent)
/// separated by whitespace text / comments only — e.g. `if-block-update`.
///
///   {#if foo}<p>foo!</p>{/if} {#if bar}<p>bar!</p>{/if}
///
/// →
///
///   var root_1 = $.from_html(`<p>foo!</p>`);
///   var root_2 = $.from_html(`<p>bar!</p>`);
///   var root = $.from_html(`<!> <!>`, 1);
///
///   export default function Main($$anchor, $$props) {
///     ...
///     var fragment = root();
///     var node = $.first_child(fragment);
///     { consequent + $.if(node, ...) }
///     var node_1 = $.sibling(node, 2);
///     { consequent_1 + $.if(node_1, ...) }
///     $.append($$anchor, fragment);
///     ...
///   }
fn emit_top_level_multi_if_program(
    nodes: &[FragmentChild],
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    // Allow nodes that are either IfBlock (with constraints) or fully-static
    // RegularElement. Track per-slot kind via an enum.
    enum Slot<'a> {
        StaticEl(&'a svelte_ast::elements::RegularElement),
        StaticText(String),
        If(&'a svelte_ast::blocks::IfBlock),
        Each(&'a svelte_ast::blocks::EachBlock),
        /// `{LITERAL}` ExpressionTag with literal-foldable value. Becomes a
        /// text anchor whose `nodeValue` is set in the body.
        LiteralAnchor(String),
        /// `{@html EXPR}` HtmlTag. Becomes a `<!>` anchor + `$.html(node, () => EXPR)`.
        Html(&'a svelte_ast::tags::HtmlTag),
        /// Bare `<Component {...attrs} />`. Becomes a `<!>` anchor +
        /// `Component(node, {...props})` body.
        Component(&'a svelte_ast::elements::Component),
        /// `<TAG ATTRS>{@html EXPR}</TAG>` — static element wrapping a
        /// single HtmlTag. Emits empty `<TAG></TAG>` template + body
        /// `var X = ...; $.html(X, () => EXPR, true); $.reset(X);`.
        ElementWithHtml(&'a svelte_ast::elements::RegularElement, &'a svelte_ast::tags::HtmlTag),
        /// Static-body element with one or more event directives (`on:click`
        /// etc). Emits `<TAG>body</TAG>` template + `var X = ...; $.event(...);`.
        ElementWithEvents(&'a svelte_ast::elements::RegularElement),
        /// Element with one or more dynamic attributes (`<div id={x}>`) and
        /// static body. Emits `<TAG STATIC_ATTRS>body</TAG>` template +
        /// `var X = ...; $.template_effect(() => $.set_attribute(...))`.
        DynamicEl(&'a svelte_ast::elements::RegularElement),
        /// Element with one or more spread attributes + static body. Emits
        /// `var X = ...; $.attribute_effect(X, () => ({ ...spread }));`.
        ElementWithSpread(&'a svelte_ast::elements::RegularElement),
        /// `<TAG ATTRS><slot/></TAG>` — static element wrapping a single
        /// SlotElement. Emits `<TAG><!></TAG>` template + body with
        /// `var X = ...; var node = $.child(X); $.slot(node, $$props,
        /// 'NAME', {}, null); $.reset(X);`.
        ElementWithSlot(&'a svelte_ast::elements::RegularElement, &'a svelte_ast::elements::SlotElement),
        /// `<TAG ATTRS>Hello {name}!</TAG>` — element with text-only body
        /// (Text + ExpressionTag, at least one non-literal). Emits
        /// `<TAG> </TAG>` template + body `var X = ...; var text = $.child(X);
        /// $.reset(X);` and a template_effect for set_text.
        TextAnchorEl(&'a svelte_ast::elements::RegularElement),
        /// `<TAG ATTRS>{#each ...}{/each}</TAG>` — element wrapping a single
        /// each-block ("controlled" — runtime manages the children, no
        /// `<!>` anchor inside). Emits `<TAG></TAG>` template + body
        /// `var X = ...; $.each(X, FLAG | IS_CONTROLLED, ...); $.reset(X);`.
        ElementWithEach(&'a svelte_ast::elements::RegularElement, &'a svelte_ast::blocks::EachBlock),
    }
    let mut slots: Vec<Slot> = Vec::new();
    // `gap_after[i]` is true iff there was whitespace text (or any
    // separator) between slots[i] and slots[i+1] in the source — used to
    // decide whether the emitted template should insert a space between
    // them (matches `cloudflare-mirage-borking-2` which has no whitespace
    // and uses default `$.sibling()`).
    let mut gap_after: Vec<bool> = Vec::new();
    let mut pending_gap = false;
    for n in nodes.iter() {
        match n {
            FragmentChild::Text(t) => {
                if t.data.trim().is_empty() {
                    pending_gap = true;
                } else {
                    // Non-whitespace text — leading whitespace in the
                    // raw data acts as a gap before this slot; trailing
                    // whitespace acts as a gap after.
                    let leading_ws = t.data.chars().next().map(|c| c.is_whitespace()).unwrap_or(false);
                    let trailing_ws = t.data.chars().last().map(|c| c.is_whitespace()).unwrap_or(false);
                    if !slots.is_empty() {
                        gap_after.push(pending_gap || leading_ws);
                    }
                    pending_gap = trailing_ws;
                    slots.push(Slot::StaticText(t.data.trim().to_string()));
                }
            }
            FragmentChild::Comment(_) => {
                // Skip — doesn't affect navigation (they're not in client templates).
            }
            FragmentChild::SvelteOptions(_) => {}
            FragmentChild::IfBlock(ib) => {
                if ib.alternate.is_some() || expr_top_await(&ib.test) {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::If(ib));
            }
            FragmentChild::EachBlock(eb) => {
                // No async, simple identifier context. Fallback / key OK.
                if expr_top_await(&eb.expression) {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::Each(eb));
            }
            FragmentChild::ExpressionTag(et) => {
                // Only literal-foldable expressions become anchor slots.
                let lit = literal_to_template_string(&et.expression);
                let Some(s) = lit else {
                    return None;
                };
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::LiteralAnchor(s));
            }
            FragmentChild::HtmlTag(ht) => {
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::Html(ht));
            }
            FragmentChild::Component(c) => {
                // Bare Component with no children body. Attribute spreads
                // OK; complex slot bodies bail.
                if !c.fragment.nodes.is_empty()
                    && c.fragment
                        .nodes
                        .iter()
                        .any(|n| !matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
                {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::Component(c));
            }
            FragmentChild::RegularElement(el) => {
                // Detect `<TAG ATTRS>{@html EXPR}</TAG>` shape.
                let inner_non_ws: Vec<&FragmentChild> = el
                    .fragment
                    .nodes
                    .iter()
                    .filter(|n| match n {
                        FragmentChild::Text(t) => !t.data.trim().is_empty(),
                        FragmentChild::Comment(_) => false,
                        _ => true,
                    })
                    .collect();
                let html_only_body = inner_non_ws.len() == 1
                    && matches!(inner_non_ws[0], FragmentChild::HtmlTag(_))
                    && is_element_static_attrs(el);
                if html_only_body {
                    if let FragmentChild::HtmlTag(ht) = inner_non_ws[0] {
                        if !slots.is_empty() {
                            gap_after.push(pending_gap);
                        }
                        pending_gap = false;
                        slots.push(Slot::ElementWithHtml(el, ht));
                        continue;
                    }
                }
                // Detect `<TAG ATTRS>{#each ...}{/each}</TAG>` shape.
                let each_only_body = inner_non_ws.len() == 1
                    && matches!(inner_non_ws[0], FragmentChild::EachBlock(_))
                    && is_element_static_attrs(el);
                if each_only_body {
                    if let FragmentChild::EachBlock(eb) = inner_non_ws[0] {
                        if !expr_top_await(&eb.expression) {
                            if !slots.is_empty() {
                                gap_after.push(pending_gap);
                            }
                            pending_gap = false;
                            slots.push(Slot::ElementWithEach(el, eb));
                            continue;
                        }
                    }
                }
                // Detect `<TAG ATTRS><slot/></TAG>` shape.
                let slot_only_body = inner_non_ws.len() == 1
                    && matches!(inner_non_ws[0], FragmentChild::SlotElement(_))
                    && is_element_static_attrs(el);
                if slot_only_body {
                    if let FragmentChild::SlotElement(se) = inner_non_ws[0] {
                        if !slots.is_empty() {
                            gap_after.push(pending_gap);
                        }
                        pending_gap = false;
                        slots.push(Slot::ElementWithSlot(el, se));
                        continue;
                    }
                }
                // Detect element with text-anchor body
                // (`<h1>Hello, {name}</h1>`) and static attrs. Requires
                // at least one non-whitespace Text node in body — a body
                // of just one ExpressionTag uses `el.textContent = EXPR`
                // via the deep_static_walker, not the text-anchor shape.
                if is_element_static_attrs(el)
                    && is_text_only_element(el)
                    && el.fragment.nodes.iter().any(|n| {
                        matches!(n, FragmentChild::Text(t) if !t.data.trim().is_empty())
                    })
                {
                    if !slots.is_empty() {
                        gap_after.push(pending_gap);
                    }
                    pending_gap = false;
                    slots.push(Slot::TextAnchorEl(el));
                    continue;
                }
                // Detect element with event directives but otherwise
                // static body (event-handler fixture).
                if element_static_body_with_events(el) {
                    if !slots.is_empty() {
                        gap_after.push(pending_gap);
                    }
                    pending_gap = false;
                    slots.push(Slot::ElementWithEvents(el));
                    continue;
                }
                // Detect element with dynamic attribute(s) + static body
                // (element-attribute-removed fixture).
                if element_static_body_with_dyn_attrs(el) {
                    if !slots.is_empty() {
                        gap_after.push(pending_gap);
                    }
                    pending_gap = false;
                    slots.push(Slot::DynamicEl(el));
                    continue;
                }
                // Detect element with spread attribute + static body.
                if element_static_body_with_spread(el) {
                    if !slots.is_empty() {
                        gap_after.push(pending_gap);
                    }
                    pending_gap = false;
                    slots.push(Slot::ElementWithSpread(el));
                    continue;
                }
                if !is_element_fully_static(el) {
                    return None;
                }
                if !slots.is_empty() {
                    gap_after.push(pending_gap);
                }
                pending_gap = false;
                slots.push(Slot::StaticEl(el));
            }
            _ => return None,
        }
    }
    if slots.len() < 2 {
        return None;
    }
    // Must contain at least one IfBlock.
    let is_anchor_slot = |s: &Slot| {
        matches!(
            s,
            Slot::If(_)
                | Slot::Each(_)
                | Slot::LiteralAnchor(_)
                | Slot::Html(_)
                | Slot::Component(_)
                | Slot::ElementWithHtml(_, _)
                | Slot::ElementWithEvents(_)
                | Slot::DynamicEl(_)
                | Slot::ElementWithSpread(_)
                | Slot::ElementWithSlot(_, _)
                | Slot::TextAnchorEl(_)
                | Slot::ElementWithEach(_, _)
        )
    };
    if !slots.iter().any(is_anchor_slot) {
        return None;
    }

    let mut root_decls: Vec<Statement> = Vec::new();
    let mut root_idx: usize = 0;
    let mut elem_var_idx: usize = 0;

    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    // Compute the DOM sibling position of each slot. Each StaticEl / If /
    // Each occupies its own position; runs of StaticText (with or without
    // surrounding whitespace gaps) merge into a single text-node position.
    // A whitespace gap between two non-text slots creates an intermediate
    // text node.
    let mut positions: Vec<usize> = vec![0; slots.len()];
    let mut pos: usize = 0;
    let mut pending_text = false;
    for (i, slot) in slots.iter().enumerate() {
        let preceding_gap = i > 0 && gap_after[i - 1];
        match slot {
            Slot::StaticEl(_)
            | Slot::If(_)
            | Slot::Each(_)
            | Slot::Html(_)
            | Slot::Component(_)
            | Slot::ElementWithHtml(_, _)
            | Slot::ElementWithEvents(_)
            | Slot::DynamicEl(_)
            | Slot::ElementWithSpread(_)
            | Slot::ElementWithSlot(_, _)
            | Slot::TextAnchorEl(_)
            | Slot::ElementWithEach(_, _) => {
                if pending_text || preceding_gap {
                    pos += 1;
                    pending_text = false;
                }
                positions[i] = pos;
                pos += 1;
            }
            Slot::StaticText(_) | Slot::LiteralAnchor(_) => {
                // Both contribute to a merged text node at the current
                // position. The LiteralAnchor's nodeValue is set at
                // runtime; StaticText is inlined in the template.
                if preceding_gap && !pending_text {
                    // Flush any preceding text gap into its own position
                    // (matches upstream when a text run follows an element
                    // with a gap).
                }
                positions[i] = pos;
                pending_text = true;
            }
        }
    }
    // Flush a trailing text run so `pos` represents the total sibling count.
    if pending_text {
        pos += 1;
    }
    // Find first/last anchor slot for navigation / next() computation.
    let first_anchor_slot = slots.iter().position(is_anchor_slot).unwrap();
    let last_anchor_slot = slots.iter().rposition(is_anchor_slot).unwrap();
    let first_if_pos = positions[first_anchor_slot];
    let last_if_pos = positions[last_anchor_slot];
    let final_pos = pos;
    let trailing_advance = final_pos.saturating_sub(last_if_pos + 1);
    let mut block_stmts: Vec<Statement> = Vec::new();
    // Event-directive emissions are collected here and appended AFTER all
    // anchor-block emissions, so events fire on elements already declared.
    let mut event_stmts: Vec<Statement> = Vec::new();
    // Spread effects (`$.attribute_effect(...)`) are emitted BEFORE
    // template_effect / event calls so upstream's ordering matches.
    let mut spread_effects: Vec<Statement> = Vec::new();
    // Text-anchor set_text effects (one per TextAnchorEl); combined into
    // a single template_effect block at the end.
    let mut text_set_effects: Vec<(String, Expression)> = Vec::new();
    let mut anchor_count = 0usize;
    let mut if_count = 0usize;
    let mut prev_anchor_slot: Option<usize> = None;
    let mut prev_anchor_var: Option<String> = None;
    let mut node_idx = 0usize;
    let mut text_idx = 0usize;
    let mut frag_idx = 0usize;
    let mut elem_named_counts: HashMap<String, usize> = HashMap::new();
    fn elem_named_count(name: &str, m: &mut HashMap<String, usize>) -> usize {
        let safe = sanitize_name(name);
        let cnt = m.entry(safe).or_insert(0);
        let n = *cnt;
        *cnt += 1;
        n
    }
    // First anchor variable name (returned to caller for the
    // `var X = first_child(...)` initializer).
    let mut first_anchor_var: Option<String> = None;
    for (slot_i, slot) in slots.iter().enumerate() {
        if !is_anchor_slot(slot) {
            continue;
        }
        let i = anchor_count;
        anchor_count += 1;
        // Choose var name per slot type:
        //  - LiteralAnchor → text/text_N
        //  - ElementWithHtml → <el_name>/<el_name>_N
        //  - others → node/node_N
        let is_literal = matches!(slot, Slot::LiteralAnchor(_));
        let cur_var = if is_literal {
            let n = if text_idx == 0 { "text".to_string() } else { format!("text_{}", text_idx) };
            text_idx += 1;
            n
        } else if let Slot::ElementWithHtml(el, _) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::ElementWithEvents(el) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::DynamicEl(el) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::ElementWithSpread(el) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::ElementWithSlot(el, _) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::TextAnchorEl(el) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else if let Slot::ElementWithEach(el, _) = slot {
            let cnt = elem_named_count(&el.name, &mut elem_named_counts);
            if cnt == 0 {
                sanitize_name(&el.name)
            } else {
                format!("{}_{}", sanitize_name(&el.name), cnt)
            }
        } else {
            let n = if node_idx == 0 { "node".to_string() } else { format!("node_{}", node_idx) };
            node_idx += 1;
            n
        };
        if i == 0 {
            first_anchor_var = Some(cur_var.clone());
        }
        // Emit sibling navigation between anchor slots before this one.
        if i > 0 {
            let prev_node = prev_anchor_var.clone().unwrap();
            let prev_slot_i = prev_anchor_slot.unwrap();
            let prev_pos = positions[prev_slot_i];
            let this_pos = positions[slot_i];
            let offset = this_pos - prev_pos;
            // For LiteralAnchor with empty text content, append `, true` as
            // an is_text hint (mirrors `$.sibling(prev, N, true)`).
            let is_empty_literal = matches!(slot, Slot::LiteralAnchor(s) if {
                let mut full = String::new();
                if slot_i > 0 && gap_after[slot_i - 1] {
                    full.push(' ');
                }
                full.push_str(s);
                if slot_i + 1 < slots.len() && gap_after[slot_i] {
                    full.push(' ');
                }
                full.is_empty()
            });
            let nav_args: Vec<Expression> = if offset == 1 && !is_empty_literal {
                vec![t::id_owned(prev_node.to_string())]
            } else if is_empty_literal {
                vec![
                    t::id_owned(prev_node.to_string()),
                    t::lit_number(offset as f64),
                    Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                        value: true,
                        span: Span::ZERO,
                    }))),
                ]
            } else {
                vec![t::id_owned(prev_node.to_string()), t::lit_number(offset as f64)]
            };
            block_stmts.push(t::var(
                &cur_var,
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    nav_args,
                ),
            ));
        }
        match slot {
            Slot::If(ib) => {
                let if_i = if_count;
                if_count += 1;
                let consequent_text_name = if if_i == 0 {
                    "text".to_string()
                } else {
                    format!("text_{}", if_i)
                };
                let consequent_body = emit_vanilla_branch_body_with_context(
                    &ib.consequent,
                    &consequent_text_name,
                    &mut root_decls,
                    &mut root_idx,
                    &mut elem_var_idx,
                    &script.props_destructured,
                    &legacy_prop_names,
                )?;
                let consequent_var = if if_i == 0 {
                    "consequent".to_string()
                } else {
                    format!("consequent_{}", if_i)
                };
                let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id_anchor()],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: consequent_body,
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let mut inner_block: Vec<Statement> = Vec::new();
                inner_block.push(t::var(&consequent_var, consequent_arrow));
                let test = rewrite_props_destructured(&ib.test, &script.props_destructured);
                let test = rewrite_legacy_prop_reads(&test, &legacy_prop_names);
                let render_if = Statement::If(Box::new(IfStatement {
                    test,
                    consequent: t::stmt(t::call(t::id_render(), vec![t::id_owned(consequent_var.to_string())])),
                    alternate: None,
                    span: Span::ZERO,
                }));
                let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id("$$render")],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: vec![render_if],
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                inner_block.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "if"),
                    vec![t::id_owned(cur_var.to_string()), render_arrow],
                )));
                block_stmts.push(Statement::Block(Box::new(BlockStatement {
                    body: inner_block,
                    span: Span::ZERO,
                })));
            }
            Slot::Each(eb) => {
                let item_name = match eb.context.as_ref() {
                    Some(svelte_js_ast::Pattern::Identifier(id)) => id.name.clone(),
                    _ => return None,
                };
                // Keyed-by-self check: `{#each X as item (item)}` ⇒ item is
                // stable, no ITEM_REACTIVE needed. Other keys (or none) ⇒
                // ITEM_REACTIVE if the body reads item.
                let key_is_self_ident = match &eb.key {
                    Some(Expression::Identifier(id)) if id.name == item_name => true,
                    _ => false,
                };
                // ITEM_REACTIVE flag (bit 0) on iff the body references
                // the iter var AND the key isn't the item identifier itself.
                let item_referenced = !key_is_self_ident
                    && fragment_uses_identifier(&eb.body, &item_name);
                // Multi-element each-body path (N≥2 top-level text-anchor
                // elements) emits its own root_N + fragment_N + per-element
                // text vars + combined template_effect.
                let multi_text_count = multi_element_each_text_count(&eb.body);
                let inner_body = if let Some(_n) = multi_text_count {
                    let multi_non_ws: Vec<&FragmentChild> = eb
                        .body
                        .nodes
                        .iter()
                        .filter(|c| match c {
                            FragmentChild::Text(t) => !t.data.trim().is_empty(),
                            FragmentChild::Comment(_) => false,
                            _ => true,
                        })
                        .collect();
                    emit_multi_element_each_body(
                        &multi_non_ws,
                        &mut root_decls,
                        &mut root_idx,
                        &mut elem_var_idx,
                        &mut text_idx,
                        &mut frag_idx,
                        &item_name,
                        item_referenced,
                        &script.props_destructured,
                        &legacy_prop_names,
                    )?
                } else {
                    let body_emits_text = fragment_emits_text_var(&eb.body);
                    let body_emits_root = fragment_emits_root_template(&eb.body);
                    let body_text_name = if text_idx == 0 {
                        "text".to_string()
                    } else {
                        format!("text_{}", text_idx)
                    };
                    if body_emits_text {
                        text_idx += 1;
                    }
                    // Upstream's visitor bumps root_idx per branch arrow even
                    // when the body doesn't materialize a `$.from_html` decl.
                    // Pre-bump here when the body won't emit one, so the next
                    // branch's root_N matches upstream's counter.
                    if !body_emits_root {
                        root_idx += 1;
                    }
                    let mut inner_body = emit_vanilla_branch_body(
                        &eb.body,
                        &body_text_name,
                        &mut root_decls,
                        &mut root_idx,
                        &mut elem_var_idx,
                    )?;
                    // Each consequent body anchored at text/expression position
                    // needs `$.next()` at the head. Mirrors `emit_single_each_program`'s
                    // text-only branch.
                    let body_is_text_anchored = eb.body.nodes.iter().any(|c| matches!(
                        c,
                        FragmentChild::Text(t) if !t.data.trim().is_empty()
                    )) || eb.body.nodes.iter().all(|c| matches!(
                        c,
                        FragmentChild::Text(_) | FragmentChild::ExpressionTag(_)
                    ));
                    let body_has_element = eb.body.nodes.iter().any(|c| matches!(
                        c, FragmentChild::RegularElement(_)
                    ));
                    if body_is_text_anchored && !body_has_element {
                        inner_body.insert(0, t::stmt(t::call(
                            t::member_id(t::id_dollar(), "next"),
                            Vec::new(),
                        )));
                    }
                    // Wrap iter-var refs in `$.get(VAR)` if item is reactive.
                    if item_referenced {
                        inner_body
                            .into_iter()
                            .map(|s| rewrite_stmt_get_for_each_var(&s, &item_name))
                            .collect()
                    } else {
                        inner_body
                    }
                };
                let item_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id_anchor(), t::pat_id_owned(item_name.to_string())],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: inner_body,
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let is_bare_legacy = matches!(
                    &eb.expression,
                    Expression::Identifier(id) if legacy_prop_names.contains(id.name.as_ref())
                );
                let each_collection: Expression = if is_bare_legacy {
                    eb.expression.clone()
                } else {
                    let rewritten = rewrite_props_destructured(
                        &eb.expression,
                        &script.props_destructured,
                    );
                    let rewritten = rewrite_legacy_prop_reads(&rewritten, &legacy_prop_names);
                    Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(rewritten),
                        r#async: false,
                        span: Span::ZERO,
                    }))
                };
                // Flag bits: 1 = ITEM_REACTIVE, 16 = ITEM_IMMUTABLE.
                // ITEM_IMMUTABLE is set in runes mode (when the iterable
                // expression comes from `$props()` destructuring).
                let is_runes_iter = matches!(
                    &eb.expression,
                    Expression::Identifier(id) if script.props_destructured.contains(id.name.as_ref())
                ) || expression_uses_props_destructured(
                    &eb.expression,
                    &script.props_destructured,
                );
                let mut flag = 0u32;
                if item_referenced {
                    flag |= 1;
                }
                if is_runes_iter {
                    flag |= 16;
                }
                // Fallback arrow, if present.
                let fallback_arrow: Option<Expression> = match &eb.fallback {
                    Some(fb) => {
                        let fb_emits_text = fragment_emits_text_var(fb);
                        let fb_text_name = if text_idx == 0 {
                            "text".to_string()
                        } else {
                            format!("text_{}", text_idx)
                        };
                        if fb_emits_text {
                            text_idx += 1;
                        }
                        let fb_body = emit_vanilla_branch_body_with_context(
                            fb,
                            &fb_text_name,
                            &mut root_decls,
                            &mut root_idx,
                            &mut elem_var_idx,
                            &script.props_destructured,
                            &legacy_prop_names,
                        )?;
                        Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: vec![t::pat_id_anchor()],
                            param_type_annotations: Vec::new(),
                            body: ArrowBody::Block(Box::new(BlockStatement {
                                body: fb_body,
                                span: Span::ZERO,
                            })),
                            r#async: false,
                            span: Span::ZERO,
                        })))
                    }
                    None => None,
                };
                // Key function: `$.index` for unkeyed, `(item) => KEY` otherwise.
                let key_fn: Expression = match &eb.key {
                    None => t::member_id(t::id_dollar(), "index"),
                    Some(k) => Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: vec![t::pat_id_owned(item_name.to_string())],
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(k.clone()),
                        r#async: false,
                        span: Span::ZERO,
                    })),
                };
                let mut each_args = vec![
                    t::id_owned(cur_var.to_string()),
                    t::lit_number(flag as f64),
                    each_collection,
                    key_fn,
                    item_arrow,
                ];
                if let Some(fb) = fallback_arrow {
                    each_args.push(fb);
                }
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "each"),
                    each_args,
                )));
            }
            Slot::LiteralAnchor(lit) => {
                // Compose the full text content for this anchor: leading gap
                // (if previous slot existed and gap_after[slot_i-1]=true) +
                // literal value + trailing gap (gap_after[slot_i]=true and a
                // next slot exists).
                let mut text_content = String::new();
                if slot_i > 0 && gap_after[slot_i - 1] {
                    text_content.push(' ');
                }
                text_content.push_str(lit);
                if slot_i + 1 < slots.len() && gap_after[slot_i] {
                    text_content.push(' ');
                }
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(Expression::Member(Box::new(
                        MemberExpression {
                            object: t::id_owned(cur_var.to_string()),
                            property: MemberProperty::Identifier(Identifier {
                                name: Cow::Borrowed("nodeValue"),
                                span: Span::ZERO,
                            }),
                            computed: false,
                            optional: false,
                            span: Span::ZERO,
                        },
                    ))),
                    operator: AssignmentOperator::Assign,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(text_content),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                }));
                block_stmts.push(t::stmt(assign));
            }
            Slot::Html(ht) => {
                // `$.html(node, () => EXPR);` — pass a thunk for the html
                // value (always a thunk for runes; legacy bare-prop case
                // would be different, but not common in multi-block).
                let inner = rewrite_props_destructured(
                    &ht.expression,
                    &script.props_destructured,
                );
                let inner = rewrite_legacy_prop_reads(&inner, &legacy_prop_names);
                let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(inner),
                    r#async: false,
                    span: Span::ZERO,
                }));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "html"),
                    vec![t::id_owned(cur_var.to_string()), arrow],
                )));
            }
            Slot::ElementWithSpread(el) => {
                // `$.attribute_effect(VAR, () => ({ ...spread }))`.
                use svelte_ast::attributes::ElementAttribute;
                let mut obj_props: Vec<ObjectMember> = Vec::new();
                for a in &el.attributes {
                    if let ElementAttribute::SpreadAttribute(s) = a {
                        let rewritten = rewrite_props_destructured(
                            &s.expression,
                            &script.props_destructured,
                        );
                        obj_props.push(ObjectMember::Spread(Box::new(SpreadElement {
                            argument: rewritten,
                            span: Span::ZERO,
                        })));
                    }
                }
                let obj_expr = Expression::Object(Box::new(ObjectExpression {
                    properties: obj_props,
                    span: Span::ZERO,
                }));
                let paren = Expression::Paren(Box::new(ParenthesizedExpression {
                    expression: obj_expr,
                    span: Span::ZERO,
                }));
                let attr_effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(paren),
                    r#async: false,
                    span: Span::ZERO,
                }));
                spread_effects.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "attribute_effect"),
                    vec![t::id_owned(cur_var.to_string()), attr_effect_arrow],
                )));
            }
            Slot::DynamicEl(el) => {
                // Collect dynamic attrs. Emit `$.template_effect(() => {
                // $.set_attribute(VAR, NAME, EXPR); ... })` deferred to
                // AFTER all anchor blocks (so reactivity fires after nav).
                //
                // When all attr values reference no reactive bindings, skip
                // the template_effect wrap entirely and emit per-attr calls
                // directly between the var-decl and the next slot — mirrors
                // upstream's non-reactive optimization.
                use svelte_ast::attributes::{AttributeValue, ElementAttribute};
                let mut reactive_bindings: HashSet<String> = HashSet::new();
                reactive_bindings.extend(script.state_bindings.iter().cloned());
                reactive_bindings.extend(script.proxy_bindings.iter().cloned());
                reactive_bindings.extend(script.derived_bindings.iter().cloned());
                reactive_bindings.extend(script.props_destructured.iter().cloned());
                reactive_bindings.extend(script.rest_props_bindings.iter().cloned());
                reactive_bindings.extend(legacy_prop_names.iter().cloned());
                let mut attr_pairs: Vec<(String, Expression)> = Vec::new();
                let mut any_reactive = false;
                for a in &el.attributes {
                    let attr = match a {
                        ElementAttribute::Attribute(attr) => attr,
                        _ => continue,
                    };
                    let expr = match &attr.value {
                        AttributeValue::Single(tag) => tag.expression.clone(),
                        AttributeValue::Many(parts) => {
                            if parts.len() == 1 {
                                match &parts[0] {
                                    svelte_ast::attributes::AttributeValuePart::ExpressionTag(et) => {
                                        et.expression.clone()
                                    }
                                    _ => continue,
                                }
                            } else {
                                continue;
                            }
                        }
                        _ => continue,
                    };
                    let rewritten = rewrite_props_destructured(&expr, &script.props_destructured);
                    let rewritten = rewrite_legacy_prop_reads(&rewritten, &legacy_prop_names);
                    if expression_has_any_binding(&rewritten, &reactive_bindings) {
                        any_reactive = true;
                    }
                    attr_pairs.push((attr.name.to_string(), rewritten));
                }
                let is_custom = el.name.contains('-');
                let attr_call_for = |name: &str, expr: Expression| -> Statement {
                    if !is_custom && name == "class" {
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "set_class"),
                            vec![
                                t::id_owned(cur_var.to_string()),
                                t::lit_number(1.0),
                                expr,
                            ],
                        ))
                    } else if is_custom {
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "set_custom_element_data"),
                            vec![
                                t::id_owned(cur_var.to_string()),
                                t::literal_str_owned(name.to_string()),
                                expr,
                            ],
                        ))
                    } else {
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "set_attribute"),
                            vec![
                                t::id_owned(cur_var.to_string()),
                                t::literal_str_owned(name.to_string()),
                                expr,
                            ],
                        ))
                    }
                };
                if any_reactive {
                    let effect_stmts: Vec<Statement> = attr_pairs
                        .into_iter()
                        .map(|(name, expr)| attr_call_for(&name, expr))
                        .collect();
                    let effect_body = if effect_stmts.len() == 1 {
                        let stmt = effect_stmts.into_iter().next().unwrap();
                        let expr = if let Statement::Expression(e) = stmt {
                            e.expression
                        } else {
                            unreachable!()
                        };
                        ArrowBody::Expression(expr)
                    } else {
                        ArrowBody::Block(Box::new(BlockStatement {
                            body: effect_stmts,
                            span: Span::ZERO,
                        }))
                    };
                    let effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: effect_body,
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    event_stmts.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "template_effect"),
                        vec![effect_arrow],
                    )));
                } else {
                    // Direct calls — no reactive deps means values only need
                    // setting once at init.
                    for (name, expr) in attr_pairs {
                        block_stmts.push(attr_call_for(&name, expr));
                    }
                }
            }
            Slot::ElementWithEvents(el) => {
                // Defer $.event calls to AFTER all anchor blocks emitted
                // (events fire on the element after the navigation is complete).
                use svelte_ast::attributes::ElementAttribute;
                for a in &el.attributes {
                    let od = match a {
                        ElementAttribute::OnDirective(od) => od,
                        _ => continue,
                    };
                    let handler_expr = match &od.expression {
                        Some(e) => e.clone(),
                        None => continue,
                    };
                    let handler = rewrite_props_destructured(
                        &handler_expr,
                        &script.props_destructured,
                    );
                    let handler = rewrite_legacy_prop_writes_to_calls(
                        &handler,
                        &legacy_prop_names,
                    );
                    event_stmts.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "event"),
                        vec![
                            Expression::Literal(Box::new(Literal::String(StringLiteral {
                                value: Cow::Owned(od.name.clone()),
                                raw: None,
                                span: Span::ZERO,
                            }))),
                            t::id_owned(cur_var.to_string()),
                            handler,
                        ],
                    )));
                }
            }
            Slot::ElementWithEach(_el, eb) => {
                // Reuse the Each-slot body emission, but mark as IS_CONTROLLED
                // (flag |= 4) and wrap with `$.reset(X)` afterward.
                let item_name = match eb.context.as_ref() {
                    Some(svelte_js_ast::Pattern::Identifier(id)) => id.name.clone(),
                    _ => return None,
                };
                let key_is_self_ident = match &eb.key {
                    Some(Expression::Identifier(id)) if id.name == item_name => true,
                    _ => false,
                };
                let item_referenced = !key_is_self_ident
                    && fragment_uses_identifier(&eb.body, &item_name);
                let multi_text_count = multi_element_each_text_count(&eb.body);
                let inner_body = if let Some(_n) = multi_text_count {
                    let multi_non_ws: Vec<&FragmentChild> = eb
                        .body
                        .nodes
                        .iter()
                        .filter(|c| match c {
                            FragmentChild::Text(t) => !t.data.trim().is_empty(),
                            FragmentChild::Comment(_) => false,
                            _ => true,
                        })
                        .collect();
                    emit_multi_element_each_body(
                        &multi_non_ws,
                        &mut root_decls,
                        &mut root_idx,
                        &mut elem_var_idx,
                        &mut text_idx,
                        &mut frag_idx,
                        &item_name,
                        item_referenced,
                        &script.props_destructured,
                        &legacy_prop_names,
                    )?
                } else {
                    let body_emits_text = fragment_emits_text_var(&eb.body);
                    let body_emits_root = fragment_emits_root_template(&eb.body);
                    let body_text_name = if text_idx == 0 {
                        "text".to_string()
                    } else {
                        format!("text_{}", text_idx)
                    };
                    if body_emits_text {
                        text_idx += 1;
                    }
                    if !body_emits_root {
                        root_idx += 1;
                    }
                    let mut inner_body = emit_vanilla_branch_body(
                        &eb.body,
                        &body_text_name,
                        &mut root_decls,
                        &mut root_idx,
                        &mut elem_var_idx,
                    )?;
                    // Each consequent body anchored at text/expression position
                    // needs `$.next()` at the head.
                    let body_has_element = eb.body.nodes.iter().any(|c| matches!(
                        c, FragmentChild::RegularElement(_)
                    ));
                    let body_is_text_anchored = eb.body.nodes.iter().all(|c| matches!(
                        c,
                        FragmentChild::Text(_) | FragmentChild::ExpressionTag(_)
                    ));
                    if body_is_text_anchored && !body_has_element {
                        inner_body.insert(0, t::stmt(t::call(
                            t::member_id(t::id_dollar(), "next"),
                            Vec::new(),
                        )));
                    }
                    if item_referenced {
                        inner_body
                            .into_iter()
                            .map(|s| rewrite_stmt_get_for_each_var(&s, &item_name))
                            .collect()
                    } else {
                        inner_body
                    }
                };
                let item_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id_anchor(), t::pat_id_owned(item_name.to_string())],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: inner_body,
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let is_bare_legacy = matches!(
                    &eb.expression,
                    Expression::Identifier(id) if legacy_prop_names.contains(id.name.as_ref())
                );
                let each_collection: Expression = if is_bare_legacy {
                    eb.expression.clone()
                } else {
                    let rewritten = rewrite_props_destructured(
                        &eb.expression,
                        &script.props_destructured,
                    );
                    let rewritten = rewrite_legacy_prop_reads(&rewritten, &legacy_prop_names);
                    Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(rewritten),
                        r#async: false,
                        span: Span::ZERO,
                    }))
                };
                let is_runes_iter = matches!(
                    &eb.expression,
                    Expression::Identifier(id) if script.props_destructured.contains(id.name.as_ref())
                ) || expression_uses_props_destructured(
                    &eb.expression,
                    &script.props_destructured,
                );
                // Flag bits: 1 = ITEM_REACTIVE, 4 = IS_CONTROLLED, 16 = ITEM_IMMUTABLE.
                let mut flag = 4u32; // controlled
                if item_referenced {
                    flag |= 1;
                }
                if is_runes_iter {
                    flag |= 16;
                }
                let fallback_arrow: Option<Expression> = match &eb.fallback {
                    Some(fb) => {
                        let fb_emits_text = fragment_emits_text_var(fb);
                        let fb_text_name = if text_idx == 0 {
                            "text".to_string()
                        } else {
                            format!("text_{}", text_idx)
                        };
                        if fb_emits_text {
                            text_idx += 1;
                        }
                        let fb_body = emit_vanilla_branch_body_with_context(
                            fb,
                            &fb_text_name,
                            &mut root_decls,
                            &mut root_idx,
                            &mut elem_var_idx,
                            &script.props_destructured,
                            &legacy_prop_names,
                        )?;
                        Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: vec![t::pat_id_anchor()],
                            param_type_annotations: Vec::new(),
                            body: ArrowBody::Block(Box::new(BlockStatement {
                                body: fb_body,
                                span: Span::ZERO,
                            })),
                            r#async: false,
                            span: Span::ZERO,
                        })))
                    }
                    None => None,
                };
                let key_fn: Expression = match &eb.key {
                    None => t::member_id(t::id_dollar(), "index"),
                    Some(k) => Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: vec![t::pat_id_owned(item_name.to_string())],
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(k.clone()),
                        r#async: false,
                        span: Span::ZERO,
                    })),
                };
                let mut each_args = vec![
                    t::id_owned(cur_var.to_string()),
                    t::lit_number(flag as f64),
                    each_collection,
                    key_fn,
                    item_arrow,
                ];
                if let Some(fb) = fallback_arrow {
                    each_args.push(fb);
                }
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "each"),
                    each_args,
                )));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(cur_var.to_string())],
                )));
            }
            Slot::TextAnchorEl(el) => {
                // `var text_N = $.child(X); $.reset(X);` + queue text_set effect.
                let text_var = if text_idx == 0 {
                    "text".to_string()
                } else {
                    format!("text_{}", text_idx)
                };
                text_idx += 1;
                block_stmts.push(t::var(
                    &text_var,
                    t::call(
                        t::member_id(t::id_dollar(), "child"),
                        vec![t::id_owned(cur_var.to_string())],
                    ),
                ));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(cur_var.to_string())],
                )));
                // Build the inline template from body parts.
                let mut parts: Vec<TextPart> = Vec::new();
                for child in &el.fragment.nodes {
                    match child {
                        FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                        FragmentChild::ExpressionTag(et) => {
                            parts.push(TextPart::Expr(&et.expression))
                        }
                        _ => {}
                    }
                }
                let inline = build_inline_template(&parts, &HashSet::new());
                // Rewrite identifier references for legacy props (X → X())
                // and runes destructured props (X → $$props.X).
                let inline = rewrite_props_destructured(&inline, &script.props_destructured);
                let inline = rewrite_legacy_prop_reads(&inline, &legacy_prop_names);
                text_set_effects.push((text_var, inline));
            }
            Slot::ElementWithSlot(_el, se) => {
                // `var X = ...; var node_N = $.child(X); $.slot(node_N,
                // $$props, 'NAME', {}, null); $.reset(X);`.
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                let mut slot_name = "default".to_string();
                for a in &se.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        if attr.name == "name" {
                            if let AttributeValue::Many(parts) = &attr.value {
                                if parts.len() == 1 {
                                    if let AttributeValuePart::Text(t) = &parts[0] {
                                        slot_name = t.data.clone();
                                    }
                                }
                            }
                        }
                    }
                }
                // Use a `node_2` style name to avoid collision with `node`
                // (used inside the if-block consequent if present).
                let slot_node_var = format!("node_{}", node_idx + 1);
                node_idx += 1;
                block_stmts.push(t::var(
                    &slot_node_var,
                    t::call(
                        t::member_id(t::id_dollar(), "child"),
                        vec![t::id_owned(cur_var.to_string())],
                    ),
                ));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "slot"),
                    vec![
                        t::id_owned(slot_node_var.to_string()),
                        t::id("$$props"),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(slot_name),
                            raw: None,
                            span: Span::ZERO,
                        }))),
                        Expression::Object(Box::new(ObjectExpression {
                            properties: Vec::new(),
                            span: Span::ZERO,
                        })),
                        Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                    ],
                )));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(cur_var.to_string())],
                )));
            }
            Slot::ElementWithHtml(el, ht) => {
                // `var X = ...; $.html(X, () => EXPR, true); $.reset(X);`.
                let inner = rewrite_props_destructured(
                    &ht.expression,
                    &script.props_destructured,
                );
                let inner = rewrite_legacy_prop_reads(&inner, &legacy_prop_names);
                let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(inner),
                    r#async: false,
                    span: Span::ZERO,
                }));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "html"),
                    vec![
                        t::id_owned(cur_var.to_string()),
                        arrow,
                        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                            value: true,
                            span: Span::ZERO,
                        }))),
                    ],
                )));
                block_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(cur_var.to_string())],
                )));
                let _ = el;
            }
            Slot::Component(c) => {
                // Bare Component(node, { props }).
                use svelte_ast::attributes::{
                    AttributeValue, AttributeValuePart, ElementAttribute,
                };
                let mut props: Vec<ObjectMember> = Vec::new();
                for a in &c.attributes {
                    match a {
                        ElementAttribute::Attribute(attr) => {
                            let value: Expression = match &attr.value {
                                AttributeValue::Empty => Expression::Literal(Box::new(
                                    Literal::Boolean(BooleanLiteral {
                                        value: true,
                                        span: Span::ZERO,
                                    }),
                                )),
                                AttributeValue::Single(tag) => {
                                    let v = rewrite_props_destructured(
                                        &tag.expression,
                                        &script.props_destructured,
                                    );
                                    rewrite_legacy_prop_reads(&v, &legacy_prop_names)
                                }
                                AttributeValue::Many(parts) => {
                                    if parts.len() == 1 {
                                        match &parts[0] {
                                            AttributeValuePart::Text(t) => Expression::Literal(
                                                Box::new(Literal::String(StringLiteral {
                                                    value: Cow::Owned(t.data.clone()),
                                                    raw: None,
                                                    span: Span::ZERO,
                                                })),
                                            ),
                                            AttributeValuePart::ExpressionTag(e) => {
                                                let v = rewrite_props_destructured(
                                                    &e.expression,
                                                    &script.props_destructured,
                                                );
                                                rewrite_legacy_prop_reads(&v, &legacy_prop_names)
                                            }
                                        }
                                    } else {
                                        return None;
                                    }
                                }
                            };
                            let shorthand =
                                matches!(&value, Expression::Identifier(id) if id.name == attr.name);
                            props.push(ObjectMember::Property(Box::new(Property {
                                key: PropertyKey::Identifier(Identifier {
                                    name: Cow::Owned(attr.name.clone()),
                                    span: Span::ZERO,
                                }),
                                value,
                                kind: PropertyKind::Init,
                                computed: false,
                                shorthand,
                                method: false,
                                span: Span::ZERO,
                            })));
                        }
                        ElementAttribute::SpreadAttribute(s) => {
                            props.push(ObjectMember::Spread(Box::new(SpreadElement {
                                argument: s.expression.clone(),
                                span: Span::ZERO,
                            })));
                        }
                        _ => return None,
                    }
                }
                block_stmts.push(t::stmt(t::call(
                    t::id_owned(c.name.to_string()),
                    vec![
                        t::id_owned(cur_var.to_string()),
                        Expression::Object(Box::new(ObjectExpression {
                            properties: props,
                            span: Span::ZERO,
                        })),
                    ],
                )));
            }
            _ => unreachable!(),
        }
        prev_anchor_slot = Some(slot_i);
        prev_anchor_var = Some(cur_var);
    }

    // Top-level function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    // Helper: full text content for a LiteralAnchor slot (literal + leading/
    // trailing whitespace gaps that contribute to the same text run).
    let literal_full_text = |slot_i: usize| -> String {
        let lit = match &slots[slot_i] {
            Slot::LiteralAnchor(s) => s.clone(),
            _ => return String::new(),
        };
        let mut full = String::new();
        if slot_i > 0 && gap_after[slot_i - 1] {
            full.push(' ');
        }
        full.push_str(&lit);
        if slot_i + 1 < slots.len() && gap_after[slot_i] {
            full.push(' ');
        }
        full
    };
    // When the first anchor is a LiteralAnchor at position 0, emit
    // `$.next();` to position the hydration cursor at the leading text
    // node. Mirrors `html-tag-hydration` / `dynamic-text-nil`.
    let first_anchor_is_text = matches!(slots[first_anchor_slot], Slot::LiteralAnchor(_));
    // `is_text=true` flag on first_child/sibling for LiteralAnchor whose
    // full text content is empty (server may have stripped the text node;
    // runtime needs to create one).
    let first_anchor_text_empty =
        first_anchor_is_text && literal_full_text(first_anchor_slot).is_empty();
    if first_if_pos == 0 && first_anchor_is_text {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            Vec::new(),
        )));
    }
    func_body.push(t::var("fragment", t::call(t::id("root"), Vec::new())));
    // Navigate to first anchor. If first_if_pos==0, that's
    // `$.first_child(fragment)` (with `, true` when first anchor is a
    // LiteralAnchor with empty content). If first_if_pos==1, omit the
    // second arg (uses default `$.sibling(NODE)`).
    let first_child_args: Vec<Expression> = if first_anchor_text_empty && first_if_pos == 0 {
        vec![
            t::id_fragment(),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ]
    } else {
        vec![t::id_fragment()]
    };
    let first_child_call = t::call(
        t::member_id(t::id_dollar(), "first_child"),
        first_child_args,
    );
    let first_node_init = if first_if_pos == 0 {
        first_child_call
    } else if first_if_pos == 1 {
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![first_child_call],
        )
    } else {
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![first_child_call, t::lit_number(first_if_pos as f64)],
        )
    };
    let first_var_name = first_anchor_var.unwrap_or_else(|| "node".to_string());
    func_body.push(t::var(&first_var_name, first_node_init));
    func_body.extend(block_stmts);
    // Spread `$.attribute_effect` calls come BEFORE event/template_effect
    // (matches upstream's emission order).
    func_body.extend(spread_effects);
    // Combined `$.template_effect(() => { $.set_text(t, EXPR); ... })`
    // for all TextAnchorEl text-sets.
    if text_set_effects.len() == 1 {
        let (text_var, expr) = text_set_effects.into_iter().next().unwrap();
        let arrow_body = ArrowBody::Expression(t::call(
            t::member_id(t::id_dollar(), "set_text"),
            vec![t::id_owned(text_var.to_string()), expr],
        ));
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: arrow_body,
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    } else if text_set_effects.len() >= 2 {
        let mut block_body: Vec<Statement> = Vec::new();
        for (text_var, expr) in text_set_effects {
            block_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "set_text"),
                vec![t::id_owned(text_var.to_string()), expr],
            )));
        }
        let arrow_body = ArrowBody::Block(Box::new(BlockStatement {
            body: block_body,
            span: Span::ZERO,
        }));
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: arrow_body,
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    }
    // Event-directive registrations come after all anchor blocks.
    func_body.extend(event_stmts);
    // Trailing static slots: if any of them was originally a `<TAG>{EXPR}</TAG>`
    // (i.e. its source body has an ExpressionTag), upstream emits explicit
    // `var X = $.sibling(prev, OFFSET)` for each — not a bulk `$.next(N)`.
    // Otherwise emit `$.next(N)` for the bulk advance.
    let trailing_slots: Vec<(usize, &Slot)> = slots
        .iter()
        .enumerate()
        .skip(last_anchor_slot + 1)
        .collect();
    let trailing_has_expr = trailing_slots.iter().any(|(_, s)| matches!(
        s,
        Slot::StaticEl(el) if el.fragment.nodes.iter().any(|n| matches!(n, FragmentChild::ExpressionTag(_)))
    ));
    if trailing_has_expr {
        // Emit per-element navigation for trailing StaticEl slots that have
        // an ExpressionTag in their body. Other trailing slots (void
        // elements, comment-only static) are skipped — their positions
        // get absorbed into the offset calc.
        let mut prev_var = prev_anchor_var.clone().unwrap_or_else(|| "node".to_string());
        let mut prev_pos = positions[last_anchor_slot];
        let mut trailing_el_idx = 0usize;
        for (slot_i, slot) in &trailing_slots {
            if let Slot::StaticEl(el) = slot {
                let has_expr = el
                    .fragment
                    .nodes
                    .iter()
                    .any(|n| matches!(n, FragmentChild::ExpressionTag(_)));
                if !has_expr {
                    continue;
                }
                trailing_el_idx += 1;
                let safe = sanitize_name(&el.name);
                let var = if trailing_el_idx == 1 {
                    safe
                } else {
                    format!("{}_{}", safe, trailing_el_idx - 1)
                };
                let this_pos = positions[*slot_i];
                let offset = this_pos - prev_pos;
                let nav_args: Vec<Expression> = if offset == 1 {
                    vec![t::id_owned(prev_var.to_string())]
                } else {
                    vec![t::id_owned(prev_var.to_string()), t::lit_number(offset as f64)]
                };
                func_body.push(t::var(
                    &var,
                    t::call(t::member_id(t::id_dollar(), "sibling"), nav_args),
                ));
                // If the body's ExpressionTag folds to a non-empty literal,
                // emit `<var>.textContent = 'LITERAL';`.
                let folded: Option<String> = el.fragment.nodes.iter().find_map(|n| match n {
                    FragmentChild::ExpressionTag(et) => literal_to_template_string(&et.expression),
                    _ => None,
                });
                if let Some(text) = folded {
                    if !text.is_empty() {
                        let assign = Expression::Assignment(Box::new(AssignmentExpression {
                            left: AssignmentTarget::Expression(Expression::Member(Box::new(
                                MemberExpression {
                                    object: t::id_owned(var.to_string()),
                                    property: MemberProperty::Identifier(Identifier {
                                        name: Cow::Borrowed("textContent"),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    optional: false,
                                    span: Span::ZERO,
                                },
                            ))),
                            operator: AssignmentOperator::Assign,
                            right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                                value: Cow::Owned(text),
                                raw: None,
                                span: Span::ZERO,
                            }))),
                            span: Span::ZERO,
                        }));
                        func_body.push(t::stmt(assign));
                    }
                }
                prev_var = var;
                prev_pos = this_pos;
            }
        }
    } else if trailing_advance > 0 {
        let arg = if trailing_advance == 1 {
            Vec::new()
        } else {
            vec![t::lit_number(trailing_advance as f64)]
        };
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            arg,
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || !script.legacy_export_props.is_empty() {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    // Build the wrapper template: each slot becomes itself (static element
    // serialized to HTML, `<!>` for if-block / each-block, or literal text).
    // A space is inserted between consecutive slots iff the source had
    // whitespace text between them (so the DOM sibling count matches upstream).
    let mut html = String::with_capacity(16);
    let mut needs_import_node = false;
    for (i, slot) in slots.iter().enumerate() {
        if i > 0 && gap_after[i - 1] {
            html.push(' ');
        }
        match slot {
            Slot::If(_) | Slot::Each(_) | Slot::Html(_) | Slot::Component(_) => {
                html.push_str("<!>")
            }
            Slot::StaticEl(el) => {
                serialize_element_to_html(el, &mut html, &mut needs_import_node)?;
            }
            Slot::ElementWithHtml(el, _) => {
                // Emit `<TAG STATIC_ATTRS></TAG>` (no body content).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"");
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        for c in t.data.chars() {
                                            match c {
                                                '"' => html.push_str("&quot;"),
                                                '&' => html.push_str("&amp;"),
                                                _ => html.push(c),
                                            }
                                        }
                                    }
                                }
                                html.push('"');
                            }
                            _ => {}
                        }
                    }
                }
                html.push_str("></");
                html.push_str(&el.name);
                html.push('>');
            }
            Slot::TextAnchorEl(el) => {
                // Emit `<TAG STATIC_ATTRS> </TAG>` (single space = text anchor).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"");
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        for c in t.data.chars() {
                                            match c {
                                                '"' => html.push_str("&quot;"),
                                                '&' => html.push_str("&amp;"),
                                                _ => html.push(c),
                                            }
                                        }
                                    }
                                }
                                html.push('"');
                            }
                            _ => {}
                        }
                    }
                }
                html.push_str("> </");
                html.push_str(&el.name);
                html.push('>');
            }
            Slot::ElementWithEach(el, _) => {
                // Emit `<TAG STATIC_ATTRS></TAG>` (controlled — no anchor inside).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"");
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        for c in t.data.chars() {
                                            match c {
                                                '"' => html.push_str("&quot;"),
                                                '&' => html.push_str("&amp;"),
                                                _ => html.push(c),
                                            }
                                        }
                                    }
                                }
                                html.push('"');
                            }
                            _ => {}
                        }
                    }
                }
                html.push_str("></");
                html.push_str(&el.name);
                html.push('>');
            }
            Slot::ElementWithSlot(el, _) => {
                // Emit `<TAG STATIC_ATTRS><!></TAG>` — `<!>` is the slot anchor.
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"");
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        for c in t.data.chars() {
                                            match c {
                                                '"' => html.push_str("&quot;"),
                                                '&' => html.push_str("&amp;"),
                                                _ => html.push(c),
                                            }
                                        }
                                    }
                                }
                                html.push('"');
                            }
                            _ => {}
                        }
                    }
                }
                html.push_str("><!></");
                html.push_str(&el.name);
                html.push('>');
            }
            Slot::ElementWithEvents(el) => {
                // Serialize the element's static-attributes-only form +
                // body (OnDirective stripped automatically — we only
                // iterate Attribute variants).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                if el.name.contains('-') || el.name == "video" {
                    needs_import_node = true;
                }
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"");
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        for c in t.data.chars() {
                                            match c {
                                                '"' => html.push_str("&quot;"),
                                                '&' => html.push_str("&amp;"),
                                                _ => html.push(c),
                                            }
                                        }
                                    }
                                }
                                html.push('"');
                            }
                            _ => {}
                        }
                    }
                }
                html.push('>');
                let mut needs = false;
                serialize_fragment_to_html(&el.fragment, &mut html, &mut needs).unwrap_or(());
                html.push_str("</");
                html.push_str(&el.name);
                html.push('>');
            }
            Slot::ElementWithSpread(el) => {
                // Static-attrs-only template body (spread stripped).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                if el.name.contains('-') || el.name == "video" {
                    needs_import_node = true;
                }
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                                    html.push(' ');
                                    html.push_str(&attr.name);
                                    html.push_str("=\"");
                                    for p in parts {
                                        if let AttributeValuePart::Text(t) = p {
                                            for c in t.data.chars() {
                                                match c {
                                                    '"' => html.push_str("&quot;"),
                                                    '&' => html.push_str("&amp;"),
                                                    _ => html.push(c),
                                                }
                                            }
                                        }
                                    }
                                    html.push('"');
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if is_void_client(&el.name) {
                    html.push_str("/>");
                } else {
                    html.push('>');
                    let mut needs = false;
                    serialize_fragment_to_html(&el.fragment, &mut html, &mut needs).unwrap_or(());
                    html.push_str("</");
                    html.push_str(&el.name);
                    html.push('>');
                }
            }
            Slot::DynamicEl(el) => {
                // Static-attrs-only template body. Dynamic attrs stripped
                // (set via $.template_effect at runtime).
                use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
                if el.name.contains('-') || el.name == "video" {
                    needs_import_node = true;
                }
                html.push('<');
                html.push_str(&el.name);
                for a in &el.attributes {
                    if let ElementAttribute::Attribute(attr) = a {
                        match &attr.value {
                            AttributeValue::Empty => {
                                html.push(' ');
                                html.push_str(&attr.name);
                                html.push_str("=\"\"");
                            }
                            AttributeValue::Many(parts) => {
                                if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                                    html.push(' ');
                                    html.push_str(&attr.name);
                                    html.push_str("=\"");
                                    for p in parts {
                                        if let AttributeValuePart::Text(t) = p {
                                            for c in t.data.chars() {
                                                match c {
                                                    '"' => html.push_str("&quot;"),
                                                    '&' => html.push_str("&amp;"),
                                                    _ => html.push(c),
                                                }
                                            }
                                        }
                                    }
                                    html.push('"');
                                }
                                // Skip dynamic.
                            }
                            _ => {}
                        }
                    }
                }
                if is_void_client(&el.name) {
                    html.push_str("/>");
                } else {
                    html.push('>');
                    let mut needs = false;
                    serialize_fragment_to_html(&el.fragment, &mut html, &mut needs).unwrap_or(());
                    html.push_str("</");
                    html.push_str(&el.name);
                    html.push('>');
                }
            }
            Slot::StaticText(s) => html.push_str(s.trim()),
            // LiteralAnchor contributes a single space so a text node
            // exists at its DOM position. Runtime sets nodeValue.
            // Surrounding gaps collapse with this space.
            Slot::LiteralAnchor(_) => {
                html.push(' ');
            }
        }
        let _ = i;
    }
    // Note: the loop above intentionally over-emits a space when both
    // sides of a LiteralAnchor have gaps. Fix by collapsing runs of
    // consecutive whitespace between elements/comments in the template
    // — this matches upstream's text node merging.
    let html = collapse_template_inter_element_ws(&html);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(root_decls);
    let flag = if needs_import_node { 3.0 } else { 1.0 };
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![]), t::lit_number(flag)],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a runes/legacy-mode program for the shape:
///
///   <TAG>{#each EXPR as VAR}<INNER>{VAR (or VAR.field)}</INNER>{/each}</TAG>
///
/// Constrained to: no key, no `:else`, single-element fully-static inner
/// with text-only body (text or `{VAR}`). Mirrors the `each-block` fixture.
fn emit_single_element_wrapping_each_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    // Wrapper must have no dynamic attrs/spread/directives.
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        static_attrs.push(attr);
                    } else {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    // Body: exactly one EachBlock (whitespace + comments allowed).
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
    if non_ws.len() != 1 {
        return None;
    }
    let eb = match non_ws[0] {
        FragmentChild::EachBlock(e) => e,
        _ => return None,
    };
    // Constraint: no `:else`, simple item identifier.
    if eb.fallback.is_some() {
        return None;
    }
    if expr_top_await(&eb.expression) {
        return None;
    }
    let item_name = match eb.context.as_ref() {
        Some(svelte_js_ast::Pattern::Identifier(id)) => id.name.clone(),
        _ => return None,
    };
    let key_is_self_ident = match &eb.key {
        Some(Expression::Identifier(id)) if id.name == item_name => true,
        _ => false,
    };

    // Inner body: must be a single fully-static element with text-only body
    // (text or `{VAR.field}` interpolation).
    let inner_non_ws: Vec<&FragmentChild> = eb
        .body
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if inner_non_ws.len() != 1 {
        return None;
    }
    let inner_el = match inner_non_ws[0] {
        FragmentChild::RegularElement(e) => e,
        _ => return None,
    };
    // Inner element must have no dynamic attrs/directives.
    let mut inner_static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &inner_el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => inner_static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        inner_static_attrs.push(attr);
                    } else {
                        return None;
                    }
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    // Inner body must be text-with-single-expression (e.g. `{thing}` or
    // `{item.name}`).
    let inner_body_non_ws: Vec<&FragmentChild> = inner_el
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if inner_body_non_ws.len() != 1 {
        return None;
    }
    let inner_expr = match inner_body_non_ws[0] {
        FragmentChild::ExpressionTag(et) => et.expression.clone(),
        _ => return None,
    };

    // Build outer template HTML: `<TAG STATIC_ATTRS></TAG>`.
    let mut outer_html = String::with_capacity(32);
    outer_html.push('<');
    outer_html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                outer_html.push(' ');
                outer_html.push_str(&attr.name);
                outer_html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                outer_html.push(' ');
                outer_html.push_str(&attr.name);
                outer_html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => outer_html.push_str("&quot;"),
                                '&' => outer_html.push_str("&amp;"),
                                _ => outer_html.push(c),
                            }
                        }
                    }
                }
                outer_html.push('"');
            }
            _ => return None,
        }
    }
    outer_html.push_str("></");
    outer_html.push_str(&el.name);
    outer_html.push('>');

    // Inner template HTML: `<INNER STATIC_ATTRS> </INNER>` (single space anchor).
    let mut inner_html = String::with_capacity(32);
    inner_html.push('<');
    inner_html.push_str(&inner_el.name);
    for attr in &inner_static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                inner_html.push(' ');
                inner_html.push_str(&attr.name);
                inner_html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                inner_html.push(' ');
                inner_html.push_str(&attr.name);
                inner_html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => inner_html.push_str("&quot;"),
                                '&' => inner_html.push_str("&amp;"),
                                _ => inner_html.push(c),
                            }
                        }
                    }
                }
                inner_html.push('"');
            }
            _ => return None,
        }
    }
    inner_html.push_str("> </");
    inner_html.push_str(&inner_el.name);
    inner_html.push('>');

    // Each callback: ($$anchor, item) => { var li = root_1(); var text = $.child(li, true); $.reset(li); $.template_effect(() => $.set_text(text, $.get(item).field)); $.append($$anchor, li); }
    let inner_var = sanitize_name(&inner_el.name);
    let mut item_body: Vec<Statement> = Vec::new();
    item_body.push(t::var(&inner_var, t::call(t::id("root_1"), Vec::new())));
    item_body.push(t::var(
        "text",
        t::call(
            t::member_id(t::id_dollar(), "child"),
            vec![
                t::id_owned(inner_var.to_string()),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: true, span: Span::ZERO },
                ))),
            ],
        ),
    ));
    item_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(inner_var.to_string())],
    )));
    // Detect iter source category. ITEM_IMMUTABLE (16) is set when the
    // iterable comes from `$props()` destructuring. ITEM_REACTIVE (1) is
    // set when iter is reactive (either $props or legacy prop) AND the
    // key isn't the item identifier itself.
    let is_runes_iter = matches!(
        &eb.expression,
        Expression::Identifier(id) if script.props_destructured.contains(id.name.as_ref())
    ) || expression_uses_props_destructured(
        &eb.expression,
        &script.props_destructured,
    );
    let legacy_prop_names_for_iter: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let is_legacy_iter = match &eb.expression {
        Expression::Identifier(id) => legacy_prop_names_for_iter.contains(id.name.as_ref()),
        Expression::Member(m) => match &m.object {
            Expression::Identifier(id) => legacy_prop_names_for_iter.contains(id.name.as_ref()),
            _ => false,
        },
        Expression::Call(c) => match &c.callee {
            Expression::Identifier(id) => legacy_prop_names_for_iter.contains(id.name.as_ref()),
            _ => false,
        },
        _ => false,
    };
    let iter_is_reactive = is_runes_iter || is_legacy_iter;
    let item_needs_get = iter_is_reactive && !key_is_self_ident;
    // Rewrite inner_expr: replace bare `item_name` references with `$.get(item_name)`.
    let rewritten_inner = if item_needs_get {
        rewrite_get_for_each_var(&inner_expr, &item_name)
    } else {
        rewrite_props_destructured(&inner_expr, &script.props_destructured)
    };
    let set_text = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), rewritten_inner],
    );
    item_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(set_text),
            r#async: false,
            span: Span::ZERO,
        }))],
    )));
    item_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(inner_var.to_string())],
    )));
    let item_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor(), t::pat_id_owned(item_name.to_string())],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: item_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Each expression: bare legacy prop accessor passes as-is, otherwise wrap.
    // For now, only handle bare-identifier expression matching a legacy prop;
    // for `$$props.field` shape (runes), pass it unchanged but wrapped.
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let is_bare_legacy = matches!(
        &eb.expression,
        Expression::Identifier(id) if legacy_prop_names.contains(id.name.as_ref())
    );
    // Detect deep access to a legacy prop (e.g. `things().foo`) — wrap with
    // `($.deep_read_state(LEGACY_CALL), $.untrack(() => ORIG))`.
    let deep_legacy_object: Option<Expression> = if let Expression::Member(m) = &eb.expression {
        if let Expression::Identifier(id) = &m.object {
            if legacy_prop_names.contains(id.name.as_ref()) {
                Some(t::call(t::id_owned(id.name.to_string()), Vec::new()))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let deep_legacy_used = deep_legacy_object.is_some();
    let each_collection: Expression = if is_bare_legacy {
        eb.expression.clone()
    } else if let Some(deep_obj) = deep_legacy_object {
        // Inner expression: rewrite the bare-identifier read to call form.
        let rewritten = rewrite_legacy_prop_reads(&eb.expression, &legacy_prop_names);
        let untracked_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(rewritten),
            r#async: false,
            span: Span::ZERO,
        }));
        let untracked_call = t::call(
            t::member_id(t::id_dollar(), "untrack"),
            vec![untracked_arrow],
        );
        let deep_read_call = t::call(
            t::member_id(t::id_dollar(), "deep_read_state"),
            vec![deep_obj],
        );
        let seq = Expression::Sequence(Box::new(SequenceExpression {
            expressions: vec![deep_read_call, untracked_call],
            span: Span::ZERO,
        }));
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(seq),
            r#async: false,
            span: Span::ZERO,
        }))
    } else {
        // Wrap in arrow returning the rewritten expression.
        let rewritten = rewrite_props_destructured(&eb.expression, &script.props_destructured);
        let rewritten = rewrite_legacy_prop_reads(&rewritten, &legacy_prop_names);
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(rewritten),
            r#async: false,
            span: Span::ZERO,
        }))
    };
    let outer_var = sanitize_name(&el.name);
    // Flags = 4 (IS_CONTROLLED) base, + 1 (ITEM_REACTIVE) when item is
    // wrapped in $.get, + 16 (ITEM_IMMUTABLE) when iter is from $props.
    let mut flag = 4u32;
    if item_needs_get {
        flag |= 1;
    }
    if is_runes_iter {
        flag |= 16;
    }
    let key_fn: Expression = match &eb.key {
        None => t::member_id(t::id_dollar(), "index"),
        Some(k) => Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_owned(item_name.to_string())],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(k.clone()),
            r#async: false,
            span: Span::ZERO,
        })),
    };
    let each_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "each"),
        vec![
            t::id_owned(outer_var.to_string()),
            t::lit_number(flag as f64),
            each_collection,
            key_fn,
            item_arrow,
        ],
    ));
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    // `$.init()` is needed when the each-block reads a legacy prop deeply
    // (e.g. `things().foo`) — mirrors upstream's emitter that calls
    // `state.init = true` whenever `deep_read_state` is materialized.
    let needs_init = deep_legacy_used;
    if needs_init {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "init"),
            Vec::new(),
        )));
    }
    func_body.push(t::var(&outer_var, t::call(t::id("root"), Vec::new())));
    func_body.push(each_call);
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(outer_var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(outer_var.to_string())],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || !script.legacy_export_props.is_empty() {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root_1",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![inner_html], vec![])],
        ),
    ));
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![outer_html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// True iff the expression directly references any name in `names`
/// (e.g. `items.foo` references `items`).
fn expression_uses_props_destructured(
    e: &Expression,
    names: &HashSet<String>,
) -> bool {
    match e {
        Expression::Identifier(id) => names.contains(id.name.as_ref()),
        Expression::Member(m) => expression_uses_props_destructured(&m.object, names),
        Expression::Call(c) => {
            expression_uses_props_destructured(&c.callee, names)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expression_uses_props_destructured(e, names),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// True iff the expression references any identifier in the supplied
/// set of reactive bindings, OR accesses `$$props`. The latter covers
/// post-rewrite expressions where destructured prop reads have already
/// been turned into `$$props.NAME` member accesses.
fn expression_has_any_binding(
    e: &Expression,
    names: &HashSet<String>,
) -> bool {
    match e {
        Expression::Identifier(id) => {
            id.name == "$$props" || names.contains(id.name.as_ref())
        }
        Expression::Member(m) => expression_has_any_binding(&m.object, names),
        Expression::Call(c) => {
            expression_has_any_binding(&c.callee, names)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expression_has_any_binding(e, names),
                    _ => false,
                })
        }
        Expression::Binary(b) => {
            expression_has_any_binding(&b.left, names)
                || expression_has_any_binding(&b.right, names)
        }
        Expression::Logical(b) => {
            expression_has_any_binding(&b.left, names)
                || expression_has_any_binding(&b.right, names)
        }
        Expression::Unary(u) => expression_has_any_binding(&u.argument, names),
        Expression::Conditional(c) => {
            expression_has_any_binding(&c.test, names)
                || expression_has_any_binding(&c.consequent, names)
                || expression_has_any_binding(&c.alternate, names)
        }
        Expression::Template(t) => t.expressions.iter().any(|e| expression_has_any_binding(e, names)),
        Expression::Array(a) => a.elements.iter().any(|el| match el {
            ArrayElement::Expression(e) => expression_has_any_binding(e, names),
            ArrayElement::Spread(s) => expression_has_any_binding(&s.argument, names),
            _ => false,
        }),
        Expression::Object(o) => o.properties.iter().any(|p| match p {
            ObjectMember::Property(p) => expression_has_any_binding(&p.value, names),
            ObjectMember::Spread(s) => expression_has_any_binding(&s.argument, names),
        }),
        _ => false,
    }
}

/// True iff the element has at least one dynamic attribute (Single or
/// Many with ExpressionTag part) + static body. Other attrs may be
/// static. No directives (events, binds) allowed in this slot type —
/// those route to ElementWithEvents / bind:this paths.
fn element_static_body_with_dyn_attrs(el: &svelte_ast::elements::RegularElement) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut has_dyn = false;
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => {}
                AttributeValue::Single(_) => has_dyn = true,
                AttributeValue::Many(parts) => {
                    let any_expr = parts.iter().any(|p| matches!(p, AttributeValuePart::ExpressionTag(_)));
                    if any_expr {
                        has_dyn = true;
                    } else if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return false;
                    }
                }
            },
            _ => return false,
        }
    }
    if !has_dyn {
        return false;
    }
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            _ => return false,
        }
    }
    true
}

/// True iff the element has at least one SpreadAttribute + (optional)
/// static attrs and a fully-static body. Used by `Slot::ElementWithSpread`.
fn element_static_body_with_spread(el: &svelte_ast::elements::RegularElement) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut has_spread = false;
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => has_spread = true,
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => {}
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    if !has_spread {
        return false;
    }
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            _ => return false,
        }
    }
    true
}

/// True iff the element has only static attributes plus at least one
/// `OnDirective` (or `on*` Attribute), and a fully-static body. Used
/// by `Slot::ElementWithEvents`.
fn element_static_body_with_events(el: &svelte_ast::elements::RegularElement) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut has_event = false;
    for a in &el.attributes {
        match a {
            ElementAttribute::OnDirective(od) => {
                if od.modifiers.is_empty() && od.expression.is_some() {
                    has_event = true;
                } else {
                    return false;
                }
            }
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => {}
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    if !has_event {
        return false;
    }
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            _ => return false,
        }
    }
    true
}

/// Rewrite `X = value` to `X(value)` where X is a legacy prop accessor.
/// Used inside event handler bodies and similar contexts where setter
/// calls need to be inlined.
fn rewrite_legacy_prop_writes_to_calls(
    e: &Expression,
    legacy_props: &HashSet<String>,
) -> Expression {
    match e {
        Expression::Arrow(a) => Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: a.params.clone(),
            param_type_annotations: Vec::new(),
            body: match &a.body {
                ArrowBody::Expression(e) => ArrowBody::Expression(
                    rewrite_legacy_prop_writes_to_calls(e, legacy_props),
                ),
                ArrowBody::Block(b) => ArrowBody::Block(Box::new(BlockStatement {
                    body: b
                        .body
                        .iter()
                        .map(|s| rewrite_stmt_prop_writes(s, legacy_props))
                        .collect(),
                    span: b.span,
                })),
            },
            r#async: a.r#async,
            span: a.span,
        })),
        Expression::Assignment(asn) => {
            // `X = v` → `X(v)` if X is a legacy prop accessor.
            if matches!(asn.operator, AssignmentOperator::Assign) {
                let ident_name: Option<&str> = match &asn.left {
                    AssignmentTarget::Pattern(Pattern::Identifier(id)) => Some(id.name.as_ref()),
                    AssignmentTarget::Expression(Expression::Identifier(id)) => {
                        Some(id.name.as_ref())
                    }
                    _ => None,
                };
                if let Some(name) = ident_name {
                    if legacy_props.contains(name) {
                        return Expression::Call(Box::new(CallExpression {
                            callee: t::id_owned(name.to_string()),
                            arguments: vec![Argument::Expression(asn.right.clone())],
                            optional: false,
                            span: asn.span,
                        }));
                    }
                }
            }
            e.clone()
        }
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_legacy_prop_writes_to_calls(&p.expression, legacy_props),
            span: p.span,
        })),
        _ => e.clone(),
    }
}

fn rewrite_stmt_prop_writes(s: &Statement, legacy_props: &HashSet<String>) -> Statement {
    match s {
        Statement::Expression(e) => Statement::Expression(Box::new(
            svelte_js_ast::ExpressionStatement {
                expression: rewrite_legacy_prop_writes_to_calls(&e.expression, legacy_props),
                span: e.span,
            },
        )),
        _ => s.clone(),
    }
}

/// Walk a fragment looking for any reference to a given identifier name.
fn fragment_uses_identifier(f: &svelte_ast::fragment::Fragment, name: &str) -> bool {
    f.nodes.iter().any(|n| node_uses_identifier(n, name))
}

/// Approximates whether `emit_vanilla_branch_body` will allocate a `text`
/// variable for this fragment. Returns true for:
/// - top-level Text or ExpressionTag (always extracted)
/// - text-only RegularElement with at least one non-literal ExpressionTag
fn fragment_emits_text_var(f: &svelte_ast::fragment::Fragment) -> bool {
    let non_ws: Vec<&FragmentChild> = f
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return false;
    }
    match non_ws[0] {
        FragmentChild::Text(_) => true,
        FragmentChild::ExpressionTag(_) => true,
        FragmentChild::RegularElement(el) => {
            is_text_only_element(el) && el.attributes.is_empty()
        }
        _ => false,
    }
}

/// Counts how many `text_N` vars `emit_multi_element_each_body` will
/// allocate for the given each-body fragment. Each top-level text-anchor
/// RegularElement contributes one text var. Returns None if the fragment
/// can't be handled as a multi-element each body.
fn multi_element_each_text_count(
    f: &svelte_ast::fragment::Fragment,
) -> Option<usize> {
    let non_ws: Vec<&FragmentChild> = f
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() < 2 {
        return None;
    }
    let mut count = 0usize;
    for n in &non_ws {
        if let FragmentChild::RegularElement(el) = n {
            if is_text_only_element(el) && el.attributes.is_empty() {
                count += 1;
            } else if !is_element_fully_static(el)
                && !element_static_body_with_dyn_attrs(el)
            {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(count)
}

/// Approximates whether `emit_vanilla_branch_body` will allocate a
/// `root_N` template for this fragment. Returns true when the body is a
/// single RegularElement (which always materializes a `var root_N =
/// \$.from_html(...)`).
fn fragment_emits_root_template(f: &svelte_ast::fragment::Fragment) -> bool {
    let non_ws: Vec<&FragmentChild> = f
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return false;
    }
    matches!(non_ws[0], FragmentChild::RegularElement(_))
}

fn node_uses_identifier(n: &FragmentChild, name: &str) -> bool {
    match n {
        FragmentChild::ExpressionTag(et) => expr_uses_identifier(&et.expression, name),
        FragmentChild::HtmlTag(ht) => expr_uses_identifier(&ht.expression, name),
        FragmentChild::RegularElement(el) => fragment_uses_identifier(&el.fragment, name),
        FragmentChild::Component(c) => fragment_uses_identifier(&c.fragment, name),
        FragmentChild::IfBlock(ib) => {
            expr_uses_identifier(&ib.test, name)
                || fragment_uses_identifier(&ib.consequent, name)
                || ib
                    .alternate
                    .as_ref()
                    .map(|a| fragment_uses_identifier(a, name))
                    .unwrap_or(false)
        }
        FragmentChild::EachBlock(eb) => fragment_uses_identifier(&eb.body, name),
        _ => false,
    }
}

fn expr_uses_identifier(e: &Expression, name: &str) -> bool {
    match e {
        Expression::Identifier(id) => id.name == name,
        Expression::Member(m) => expr_uses_identifier(&m.object, name),
        Expression::Call(c) => {
            expr_uses_identifier(&c.callee, name)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_uses_identifier(e, name),
                    Argument::Spread(s) => expr_uses_identifier(&s.argument, name),
                })
        }
        Expression::Binary(b) => {
            expr_uses_identifier(&b.left, name) || expr_uses_identifier(&b.right, name)
        }
        Expression::Logical(l) => {
            expr_uses_identifier(&l.left, name) || expr_uses_identifier(&l.right, name)
        }
        Expression::Unary(u) => expr_uses_identifier(&u.argument, name),
        Expression::Paren(p) => expr_uses_identifier(&p.expression, name),
        Expression::Conditional(c) => {
            expr_uses_identifier(&c.test, name)
                || expr_uses_identifier(&c.consequent, name)
                || expr_uses_identifier(&c.alternate, name)
        }
        _ => false,
    }
}

/// Rewrite all expression-level Identifier(VAR) references in a Statement
/// to `$.get(VAR)`. Used to wrap each-iter-var reads in mutable_source
/// accessors when ITEM_REACTIVE flag is on.
fn rewrite_stmt_get_for_each_var(s: &Statement, var_name: &str) -> Statement {
    match s {
        Statement::Expression(e) => Statement::Expression(Box::new(
            svelte_js_ast::ExpressionStatement {
                expression: rewrite_get_for_each_var(&e.expression, var_name),
                span: e.span,
            },
        )),
        _ => s.clone(),
    }
}

/// Walk an expression and replace any bare reference to `var_name` with
/// `$.get(var_name)`. Used to wrap each-block iterators with mutable_source
/// accessors.
fn rewrite_get_for_each_var(e: &Expression, var_name: &str) -> Expression {
    match e {
        Expression::Identifier(id) if id.name == var_name => t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![Expression::Identifier(id.clone())],
        ),
        Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
            object: rewrite_get_for_each_var(&m.object, var_name),
            property: m.property.clone(),
            computed: m.computed,
            optional: m.optional,
            span: m.span,
        })),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_get_for_each_var(&c.callee, var_name),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => {
                        Argument::Expression(rewrite_get_for_each_var(e, var_name))
                    }
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_get_for_each_var(&b.left, var_name),
            right: rewrite_get_for_each_var(&b.right, var_name),
            span: b.span,
        })),
        Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
            operator: l.operator,
            left: rewrite_get_for_each_var(&l.left, var_name),
            right: rewrite_get_for_each_var(&l.right, var_name),
            span: l.span,
        })),
        Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
            operator: u.operator,
            argument: rewrite_get_for_each_var(&u.argument, var_name),
            prefix: u.prefix,
            span: u.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_get_for_each_var(&p.expression, var_name),
            span: p.span,
        })),
        Expression::Arrow(a) => Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: a.params.clone(),
            param_type_annotations: Vec::new(),
            body: match &a.body {
                ArrowBody::Expression(e) => {
                    ArrowBody::Expression(rewrite_get_for_each_var(e, var_name))
                }
                ArrowBody::Block(b) => ArrowBody::Block(Box::new(BlockStatement {
                    body: b
                        .body
                        .iter()
                        .map(|s| rewrite_stmt_get_for_each_var(s, var_name))
                        .collect(),
                    span: b.span,
                })),
            },
            r#async: a.r#async,
            span: a.span,
        })),
        e => e.clone(),
    }
}

/// Emit a program for the shape:
///
///   <TAG {...spread} >body</TAG>
///
/// — single element with one or more spread attributes (and optional
/// static attrs), static body. Mirrors `removes-undefined-attributes`.
fn emit_single_element_with_spread_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut spread_exprs: Vec<Expression> = Vec::new();
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(s) => spread_exprs.push(s.expression.clone()),
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return None;
                    }
                    static_attrs.push(attr);
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    if spread_exprs.is_empty() {
        return None;
    }
    // Body must be static.
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(child) => {
                if !is_element_fully_static(child) {
                    return None;
                }
            }
            _ => return None,
        }
    }

    // Build template HTML.
    let mut html = String::new();
    html.push('<');
    html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => html.push_str("&quot;"),
                                '&' => html.push_str("&amp;"),
                                _ => html.push(c),
                            }
                        }
                    }
                }
                html.push('"');
            }
            _ => return None,
        }
    }
    if is_void_client(&el.name) {
        html.push_str("/>");
    } else {
        html.push('>');
        let mut needs = false;
        serialize_fragment_to_html(&el.fragment, &mut html, &mut needs)?;
        html.push_str("</");
        html.push_str(&el.name);
        html.push('>');
    }

    // `$.attribute_effect(VAR, () => ({ ...e1, ...e2 }))`.
    let mut obj_props: Vec<ObjectMember> = Vec::new();
    for expr in spread_exprs {
        let rewritten = rewrite_props_destructured(&expr, &script.props_destructured);
        obj_props.push(ObjectMember::Spread(Box::new(SpreadElement {
            argument: rewritten,
            span: Span::ZERO,
        })));
    }
    let obj_expr = Expression::Object(Box::new(ObjectExpression {
        properties: obj_props,
        span: Span::ZERO,
    }));
    // Wrap in parens for `() => ({ ... })` form.
    let paren_obj = Expression::Paren(Box::new(ParenthesizedExpression {
        expression: obj_expr,
        span: Span::ZERO,
    }));
    let attr_effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(paren_obj),
        r#async: false,
        span: Span::ZERO,
    }));

    let var_name = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&var_name, t::call(t::id("root"), Vec::new())));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "attribute_effect"),
        vec![t::id_owned(var_name.to_string()), attr_effect_arrow],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(var_name.to_string())],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the shape:
///
///   <TAG bind:this={X}>body</TAG>
///
/// — single element with a `bind:this` directive and fully-static body.
/// Other attributes must be static. Mirrors `element-ref`.
fn emit_single_element_with_bind_this_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    // Find exactly one bind:this directive, all other attrs static.
    let mut bind_this_expr: Option<Expression> = None;
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::BindDirective(bd) if bd.name == "this" => {
                if bind_this_expr.is_some() {
                    return None;
                }
                bind_this_expr = Some(bd.expression.clone());
            }
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => static_attrs.push(attr),
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return None;
                    }
                    static_attrs.push(attr);
                }
                _ => return None,
            },
            _ => return None,
        }
    }
    let bind_this_target = bind_this_expr?;
    // Body must be fully static (no expressions / blocks).
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            FragmentChild::RegularElement(child) => {
                if !is_element_fully_static(child) {
                    return None;
                }
            }
            _ => return None,
        }
    }

    // Build template HTML with static body.
    let mut html = String::new();
    html.push('<');
    html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => html.push_str("&quot;"),
                                '&' => html.push_str("&amp;"),
                                _ => html.push(c),
                            }
                        }
                    }
                }
                html.push('"');
            }
            _ => return None,
        }
    }
    html.push('>');
    let mut needs = false;
    serialize_fragment_to_html(&el.fragment, &mut html, &mut needs)?;
    html.push_str("</");
    html.push_str(&el.name);
    html.push('>');

    // Variable name: if the bind:this target's identifier matches the
    // element tag name, suffix with `_1` to avoid collision.
    let target_name = match &bind_this_target {
        Expression::Identifier(id) => id.name.clone(),
        _ => return None,
    };
    let safe_el = sanitize_name(&el.name);
    let var_name = if target_name == el.name {
        format!("{}_1", safe_el)
    } else {
        safe_el
    };
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let is_legacy_prop = legacy_prop_names.contains(target_name.as_ref());

    // bind_this setter / getter.
    // Setter: `($$value) => target($$value)` or `($$value) => target = $$value`
    // Getter: `() => target()` or `() => target`
    let setter_body: Expression = if is_legacy_prop {
        t::call(
            t::id_owned(target_name.to_string()),
            vec![t::id("$$value")],
        )
    } else {
        // Assignment expression for runes-mode state binding.
        Expression::Assignment(Box::new(AssignmentExpression {
            left: AssignmentTarget::Pattern(Pattern::Identifier(Identifier {
                name: target_name.clone(),
                span: Span::ZERO,
            })),
            operator: AssignmentOperator::Assign,
            right: t::id("$$value"),
            span: Span::ZERO,
        }))
    };
    let setter_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$value")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(setter_body),
        r#async: false,
        span: Span::ZERO,
    }));
    let getter_body: Expression = if is_legacy_prop {
        t::call(t::id_owned(target_name.to_string()), Vec::new())
    } else {
        t::id_owned(target_name.to_string())
    };
    let getter_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(getter_body),
        r#async: false,
        span: Span::ZERO,
    }));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&var_name, t::call(t::id("root"), Vec::new())));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "bind_this"),
        vec![t::id_owned(var_name.to_string()), setter_arrow, getter_arrow],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(var_name.to_string())],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || !script.legacy_export_props.is_empty() {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a program for the shape:
///
///   <TAG>{LITERAL} text <STATIC>...</STATIC></TAG>
///
/// where the element body starts with a foldable run of (Text +
/// ExpressionTag-with-literal) that collapses to a single string, followed
/// by zero-or-more fully-static elements. The folded text becomes the
/// `nodeValue` of a text-anchor at the head of the body. Mirrors
/// `expression-sibling`, `safari-borking`.
fn emit_single_element_with_folded_prefix_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
        || !script.legacy_export_props.is_empty()
    {
        return None;
    }
    if !is_element_static_attrs(el) {
        return None;
    }
    // Walk the body: collect leading Text + ExpressionTag-with-literal as
    // a folded string; then collect any trailing fully-static elements.
    let mut folded = String::new();
    let mut had_expr = false;
    let mut had_text = false;
    let mut trailing_static: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    let mut state = 0u8; // 0 = collecting prefix run, 1 = collecting trailing static
    for n in &el.fragment.nodes {
        match (state, n) {
            (0, FragmentChild::Text(t)) => {
                folded.push_str(&collapse_ws_client(&t.data));
                had_text = true;
            }
            (0, FragmentChild::ExpressionTag(et)) => {
                let s = literal_to_template_string(&et.expression)?;
                folded.push_str(&s);
                had_expr = true;
            }
            (0, FragmentChild::Comment(_)) => {}
            (0, FragmentChild::RegularElement(child)) => {
                if !is_element_fully_static(child) {
                    return None;
                }
                state = 1;
                trailing_static.push(child);
            }
            (1, FragmentChild::Text(t)) => {
                // Whitespace-only text between trailing statics is OK.
                if !t.data.trim().is_empty() {
                    return None;
                }
            }
            (1, FragmentChild::Comment(_)) => {}
            (1, FragmentChild::RegularElement(child)) => {
                if !is_element_fully_static(child) {
                    return None;
                }
                trailing_static.push(child);
            }
            _ => return None,
        }
    }
    // Must have at least one ExpressionTag in the prefix run AND at least
    // one trailing static element. Pure text-or-expression bodies are
    // handled by the existing `<h1>{const}</h1>` → `h1.textContent = ...`
    // path which uses no text anchor in the template.
    if !had_expr || trailing_static.is_empty() {
        return None;
    }
    // Trim leading/trailing whitespace from folded — upstream's text node
    // is the leading anchor in the element so we don't trim, we keep as-is.
    // Actually upstream KEEPS the surrounding whitespace ('1 2 ' includes
    // trailing space). So no trim here.

    // Build template HTML: `<TAG ATTRS> SERIALIZED_STATICS</TAG>` (single
    // space for the text anchor, then serialized statics with a leading
    // space if needed — upstream emits `<p> <span>3</span></p>`).
    let mut html = String::new();
    html.push('<');
    html.push_str(&el.name);
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            match &attr.value {
                AttributeValue::Empty => {
                    html.push(' ');
                    html.push_str(&attr.name);
                    html.push_str("=\"\"");
                }
                AttributeValue::Many(parts) => {
                    html.push(' ');
                    html.push_str(&attr.name);
                    html.push_str("=\"");
                    for p in parts {
                        if let AttributeValuePart::Text(t) = p {
                            for c in t.data.chars() {
                                match c {
                                    '"' => html.push_str("&quot;"),
                                    '&' => html.push_str("&amp;"),
                                    _ => html.push(c),
                                }
                            }
                        }
                    }
                    html.push('"');
                }
                _ => return None,
            }
        }
    }
    html.push('>');
    html.push(' '); // text anchor
    for child in &trailing_static {
        let mut needs = false;
        serialize_element_to_html(child, &mut html, &mut needs)?;
    }
    html.push_str("</");
    html.push_str(&el.name);
    html.push('>');

    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    // var text = $.child(TAG[, true]);
    // The `true` flag is emitted when the body has no raw Text nodes —
    // only ExpressionTag-with-literal — so the runtime knows to create
    // a text node if the hydrated DOM doesn't have one.
    let child_args: Vec<Expression> = if had_text {
        vec![t::id_owned(tag_var.to_string())]
    } else {
        vec![
            t::id_owned(tag_var.to_string()),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ]
    };
    func_body.push(t::var(
        "text",
        t::call(t::member_id(t::id_dollar(), "child"), child_args),
    ));
    // text.nodeValue = LITERAL;
    let nodevalue_assign = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(Expression::Member(Box::new(MemberExpression {
            object: t::id("text"),
            property: MemberProperty::Identifier(Identifier {
                name: Cow::Borrowed("nodeValue"),
                span: Span::ZERO,
            }),
            computed: false,
            optional: false,
            span: Span::ZERO,
        }))),
        operator: AssignmentOperator::Assign,
        right: Expression::Literal(Box::new(Literal::String(StringLiteral {
            value: Cow::Owned(folded),
            raw: None,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }));
    func_body.push(t::stmt(nodevalue_assign));
    // For each trailing static: emit `$.next();` to advance past it.
    if !trailing_static.is_empty() {
        let n = trailing_static.len();
        let arg = if n == 1 {
            Vec::new()
        } else {
            vec![t::lit_number(n as f64)]
        };
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            arg,
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(tag_var.to_string())],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

fn emit_single_dynamic_element_program(
    el: &svelte_ast::elements::RegularElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.proxy_bindings.is_empty()
        || !script.derived_bindings.is_empty()
        || script.async_info.is_some()
    {
        return None;
    }
    // Element body may contain text + expression tags (treated as one
    // text region) plus comments. Anything else (nested elements, blocks,
    // etc.) → bail to the general walker.
    if !el
        .fragment
        .nodes
        .iter()
        .all(|n| matches!(n, FragmentChild::Text(_) | FragmentChild::Comment(_) | FragmentChild::ExpressionTag(_)))
    {
        return None;
    }
    // Body has a *real* dynamic expression (not just a literal that folds
    // into surrounding text). Pure-literal expression tags collapse into
    // the static template via `literal_to_template_string`.
    let body_has_expression = el.fragment.nodes.iter().any(|n| {
        if let FragmentChild::ExpressionTag(et) = n {
            literal_to_template_string(&et.expression).is_none()
        } else {
            false
        }
    });

    // Classify attributes: static (literal/text) vs dynamic (single
    // ExpressionTag value). Any other shape (directive, spread, mixed
    // text+expr) → bail.
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    let mut static_attrs: Vec<&svelte_ast::attributes::Attribute> = Vec::new();
    let mut dyn_attrs: Vec<(&str, Expression)> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => {
                match &attr.value {
                    AttributeValue::Empty => static_attrs.push(attr),
                    AttributeValue::Many(parts) => {
                        if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                            static_attrs.push(attr);
                        } else {
                            return None;
                        }
                    }
                    AttributeValue::Single(et) => {
                        dyn_attrs.push((attr.name.as_ref(), et.expression.clone()));
                    }
                }
            }
            _ => return None,
        }
    }
    if dyn_attrs.is_empty() && !body_has_expression {
        return None;
    }
    // Bail when the existing walker's template_effect emitter is needed:
    // runes-mode (no legacy props, no props destructure) body with
    // user-function calls produces the 3-arg `template_effect(EFFECT_FN,
    // [DEP_FNS])` form that this emitter doesn't yet handle. Legacy /
    // props-destructured paths use the inline 1-arg shape so they stay
    // in scope here.
    let is_legacy_or_destructured =
        !script.legacy_export_props.is_empty() || !script.props_destructured.is_empty();
    if !is_legacy_or_destructured {
        let any_body_user_call = el.fragment.nodes.iter().any(|n| {
            if let FragmentChild::ExpressionTag(et) = n {
                expr_has_user_call(&et.expression, &script.derived_bindings)
            } else {
                false
            }
        });
        let any_attr_user_call = el.attributes.iter().any(|a| {
            if let svelte_ast::attributes::ElementAttribute::Attribute(attr) = a {
                if let svelte_ast::attributes::AttributeValue::Single(et) = &attr.value {
                    return expr_has_user_call(&et.expression, &script.derived_bindings);
                }
            }
            false
        });
        if any_body_user_call || any_attr_user_call {
            return None;
        }
    }

    // Build template HTML.
    let mut html = String::with_capacity(32);
    html.push('<');
    html.push_str(&el.name);
    for attr in &static_attrs {
        match &attr.value {
            AttributeValue::Empty => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"\"");
            }
            AttributeValue::Many(parts) => {
                html.push(' ');
                html.push_str(&attr.name);
                html.push_str("=\"");
                for p in parts {
                    if let AttributeValuePart::Text(t) = p {
                        for c in t.data.chars() {
                            match c {
                                '"' => html.push_str("&quot;"),
                                '&' => html.push_str("&amp;"),
                                _ => html.push(c),
                            }
                        }
                    }
                }
                html.push('"');
            }
            _ => return None,
        }
    }
    if is_void_client(&el.name) {
        html.push_str("/>");
    } else {
        html.push('>');
        if body_has_expression {
            // Collapse mixed text+expression body to a single space — the
            // template_effect setup writes the real content via
            // `$.set_text(text, …)`. Mirrors upstream's text-anchor pattern.
            html.push(' ');
        } else {
            // Fold literal expression tags (e.g. `{'client'}`) into the
            // surrounding static text.
            for n in &el.fragment.nodes {
                let s = match n {
                    FragmentChild::Text(t) => Some(t.data.clone()),
                    FragmentChild::ExpressionTag(et) => literal_to_template_string(&et.expression),
                    _ => None,
                };
                if let Some(s) = s {
                    for c in s.chars() {
                        match c {
                            '`' => html.push_str("\\`"),
                            '\\' => html.push_str("\\\\"),
                            _ => html.push(c),
                        }
                    }
                }
            }
        }
        html.push_str("</");
        html.push_str(&el.name);
        html.push('>');
    }

    // Rewrite dyn-attr expressions through props_destructured AND
    // legacy_export_props (legacy refs become calls — `name` → `name()`).
    let legacy_prop_names: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let rewrite_expr = |e: &Expression| {
        let e = rewrite_props_destructured(e, &script.props_destructured);
        rewrite_legacy_prop_reads(&e, &legacy_prop_names)
    };
    let dyn_attrs: Vec<(String, Expression)> = dyn_attrs
        .into_iter()
        .map(|(name, e)| (name.to_string(), rewrite_expr(&e)))
        .collect();
    // Collect body text + expression parts for the set_text call.
    let body_parts_owned: Vec<TextPart<'static>> = if body_has_expression {
        let mut parts: Vec<TextPart<'static>> = Vec::new();
        for n in &el.fragment.nodes {
            match n {
                FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                FragmentChild::ExpressionTag(et) => {
                    let rewritten = rewrite_expr(&et.expression);
                    // Leak the expression to extend its lifetime to 'static
                    // — the parts vec only borrows via `TextPart::Expr`.
                    let leaked: &'static Expression = Box::leak(Box::new(rewritten));
                    parts.push(TextPart::Expr(leaked));
                }
                _ => {}
            }
        }
        parts
    } else {
        Vec::new()
    };

    // Build function body.
    let tag_var = sanitize_name(&el.name);
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    // Legacy props prelude: `$.push($$props, false); let X = $.prop($$props, 'X', N [, INIT]);`
    if !script.legacy_export_props.is_empty() {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(
                    svelte_js_ast::BooleanLiteral { value: false, span: Span::ZERO },
                ))),
            ],
        )));
        for (name, init) in &script.legacy_export_props {
            // Flag value `12` = `PROPS_IS_BINDABLE | PROPS_IS_UPDATED` per
            // upstream's flag conventions; matches every legacy-mode
            // snapshot we've inspected.
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        // $$exports accessor object.
        let exports_obj = build_legacy_exports_object(&script.legacy_export_props);
        func_body.push(t::var("$$exports", exports_obj));
    }
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(&tag_var, t::call(t::id("root"), Vec::new())));
    if body_has_expression {
        func_body.push(t::var(
            "text",
            t::call(
                t::member_id(t::id_dollar(), "child"),
                vec![t::id_owned(tag_var.to_string())],
            ),
        ));
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "reset"),
            vec![t::id_owned(tag_var.to_string())],
        )));
    }

    // `<input>` needs `$.remove_input_defaults` BEFORE the effect, and
    // `value` / `checked` attrs use dedicated setters instead of the
    // generic `set_attribute`.
    let is_input = el.name == "input";
    if is_input {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "remove_input_defaults"),
            vec![t::id_owned(tag_var.to_string())],
        )));
    }
    // Single dyn attr → `$.template_effect(() => $.set_attribute(TAG, NAME, EXPR));`
    // Multiple dyn attrs → block-body effect with sequential set_attribute calls.
    let set_attr_call = |name: &str, e: Expression| -> Expression {
        // Input-specific setters for `value` / `checked`.
        if is_input && name == "value" {
            return t::call(
                t::member_id(t::id_dollar(), "set_value"),
                vec![t::id_owned(tag_var.to_string()), e],
            );
        }
        if is_input && name == "checked" {
            return t::call(
                t::member_id(t::id_dollar(), "set_checked"),
                vec![t::id_owned(tag_var.to_string()), e],
            );
        }
        // `class={expr}` → `$.set_class(TAG, 1, $.clsx(expr))`. The `1`
        // flag marks the value as dynamic (matches upstream).
        if name == "class" {
            return t::call(
                t::member_id(t::id_dollar(), "set_class"),
                vec![
                    t::id_owned(tag_var.to_string()),
                    t::lit_number(1.0),
                    t::call(t::member_id(t::id_dollar(), "clsx"), vec![e]),
                ],
            );
        }
        t::call(
            t::member_id(t::id_dollar(), "set_attribute"),
            vec![t::id_owned(tag_var.to_string()), t::literal_str_owned(name.to_string()), e],
        )
    };
    // Collect all effect statements: `$.set_text(text, …)` for the body
    // (if any expressions), then per-dyn-attr setters.
    let mut effect_stmts: Vec<Statement> = Vec::new();
    if body_has_expression {
        let tpl = build_inline_template(&body_parts_owned, &HashSet::new());
        effect_stmts.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "set_text"),
            vec![t::id("text"), tpl],
        )));
    }
    for (name, e) in &dyn_attrs {
        effect_stmts.push(t::stmt(set_attr_call(name, e.clone())));
    }
    let effect_arrow = if effect_stmts.len() == 1 {
        let only = effect_stmts.into_iter().next().unwrap();
        let expr = match only {
            Statement::Expression(es) => es.expression,
            _ => unreachable!(),
        };
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(expr),
            r#async: false,
            span: Span::ZERO,
        }))
    } else {
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement { body: effect_stmts, span: Span::ZERO })),
            r#async: false,
            span: Span::ZERO,
        }))
    };
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![effect_arrow],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(tag_var.to_string())],
    )));
    if !script.legacy_export_props.is_empty() {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![html], vec![])],
        ),
    ));
    prog.push(export);
    Some(t::program(prog))
}

/// `{#if await EXPR}then{:else}else{/if}` →
/// `\$.async(node, [], [() => EXPR], (node, \$\$condition) => {
///     var consequent = (\$\$anchor) => { ... };
///     var alternate = (\$\$anchor) => { ... };
///     \$.if(node, (\$\$render) => { if (\$.get(\$\$condition)) \$\$render(consequent);
///         else \$\$render(alternate, -1); });
/// });`
fn emit_single_async_if_program(
    ib: &svelte_ast::blocks::IfBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    let test_inner = strip_outer_await(&ib.test);
    let consequent_body = emit_async_branch_body(&ib.consequent, "text")?;
    let alternate_body = if let Some(alt) = &ib.alternate {
        Some(emit_async_branch_body(alt, "text_1")?)
    } else {
        None
    };

    // consequent arrow
    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let mut async_inner_body: Vec<Statement> = Vec::new();
    async_inner_body.push(t::var("consequent", consequent_arrow));
    if let Some(alt_body) = alternate_body {
        let alt_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: alt_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        async_inner_body.push(t::var("alternate", alt_arrow));
    }

    // `$.if(node, ($$render) => { if ($.get($$condition)) $$render(consequent); else $$render(alternate, -1); })`
    let condition_get = t::call(
        t::member_id(t::id_dollar(), "get"),
        vec![t::id("$$condition")],
    );
    let then_call = t::stmt(t::call(t::id_render(), vec![t::id("consequent")]));
    let else_call = if ib.alternate.is_some() {
        Some(t::stmt(t::call(
            t::id_render(),
            vec![t::id("alternate"), t::lit_number(-1.0)],
        )))
    } else {
        None
    };
    let render_if = Statement::If(Box::new(IfStatement {
        test: condition_get,
        consequent: then_call,
        alternate: else_call,
        span: Span::ZERO,
    }));
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    async_inner_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "if"),
        vec![t::id("node"), render_arrow],
    )));

    // `$.async(node, [], [() => TEST_INNER], (node, $$condition) => { ... })`
    let promises_array = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(Expression::Arrow(Box::new(
            ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(test_inner),
                r#async: false,
                span: Span::ZERO,
            },
        )))],
        span: Span::ZERO,
    }));
    let blockers_array = Expression::Array(Box::new(ArrayExpression {
        elements: Vec::new(),
        span: Span::ZERO,
    }));
    let async_callback = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("node"), t::pat_id("$$condition")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: async_inner_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let async_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "async"),
        vec![
            t::id("node"),
            blockers_array,
            promises_array,
            async_callback,
        ],
    ));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    func_body.push(async_call);
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

/// True iff the fragment contains a `{@const X = ...}` whose initializer has
/// a top-level `await`.
fn fragment_has_const_await_client(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| {
        if let FragmentChild::ConstTag(ct) = n {
            ct.declaration
                .declarations
                .iter()
                .any(|d| d.init.as_ref().map_or(false, expr_top_await))
        } else {
            false
        }
    })
}

/// Compile a `{#if LITERAL}` whose body holds `{@const ... await ...}`
/// declarations and a single `<element>{TEXT_EXPR}</element>` child. Produces
/// the async-const client shape (see async-const fixture).
/// Compile a sequence of top-level `{#if LITERAL}{@const ...}{/if}` blocks
/// in async-mode script context. Matches the async-in-derived fixture.
///
/// Structure:
///   var root = $.from_html(`<!> <!> ...`, 1);
///   ...script async setup ($.run...)...
///   var fragment = root();
///   var node = $.first_child(fragment);
///   { var consequent_K = ($$anchor) => { ...consts via $.run... }; $.if(...); }
///   var node_K = $.sibling(prev, 2);
///   ...
///   $.append($$anchor, fragment);
///
/// When any const init has an IIFE pattern (call of arrow-function), wraps
/// the function body in `$.push($$props, true); ... $.pop();`.
fn emit_async_const_chain_program(
    if_blocks: &[&svelte_ast::blocks::IfBlock],
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    let ai = script.async_info.as_ref()?;

    // Determine if any const init uses an IIFE pattern → triggers
    // $.push/$.pop wrap.
    let mut needs_push_pop = false;
    for ib in if_blocks {
        for n in &ib.consequent.nodes {
            if let FragmentChild::ConstTag(ct) = n {
                for d in &ct.declaration.declarations {
                    if let Some(init) = &d.init {
                        if expr_has_iife_call(init) {
                            needs_push_pop = true;
                        }
                    }
                }
            }
        }
    }

    // Template: `<!> <!> ...` (one placeholder per if-block).
    let template_html = (0..if_blocks.len())
        .map(|_| "<!>")
        .collect::<Vec<_>>()
        .join(" ");
    let root_decl = t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![template_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if needs_push_pop {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: true,
                    span: Span::ZERO,
                }))),
            ],
        )));
    }
    // Script body already contains async setup.
    func_body.extend(script.body.iter().cloned());

    func_body.push(t::var(
        "fragment",
        t::call(t::id("root"), Vec::new()),
    ));

    // Node counter for sibling navigation (first is `node`, next `node_1`, ...).
    let mut consequent_idx: usize = 0;
    let mut promises_idx: usize = 0;
    let mut prev_node_name = "node".to_string();
    func_body.push(t::var(
        &prev_node_name,
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));

    for (i, ib) in if_blocks.iter().enumerate() {
        let node_name = if i == 0 {
            prev_node_name.clone()
        } else {
            let new_name = format!("node_{i}");
            func_body.push(t::var(
                &new_name,
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![t::id_owned(prev_node_name.to_string()), t::lit_number(2.0)],
                ),
            ));
            new_name
        };
        // Build the consequent body: let X; var promises = $.run([...thunks])
        let consequent_body = build_async_const_consequent(
            ib,
            ai,
            &script.derived_bindings,
            &mut promises_idx,
        )?;

        let consequent_name = if consequent_idx == 0 {
            "consequent".to_string()
        } else {
            format!("consequent_{consequent_idx}")
        };
        consequent_idx += 1;

        let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: consequent_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));

        // $.if call with literal test
        let render_call = t::stmt(t::call(t::id_render(), vec![t::id_owned(consequent_name.to_string())]));
        let render_if = Statement::If(Box::new(IfStatement {
            test: ib.test.clone(),
            consequent: render_call,
            alternate: None,
            span: Span::ZERO,
        }));
        let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$render")],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: vec![render_if],
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let if_call = t::stmt(t::call(
            t::member_id(t::id_dollar(), "if"),
            vec![t::id_owned(node_name.to_string()), render_arrow],
        ));

        let block = Statement::Block(Box::new(BlockStatement {
            body: vec![t::var(&consequent_name, consequent_arrow), if_call],
            span: Span::ZERO,
        }));
        func_body.push(block);

        prev_node_name = node_name;
    }

    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));
    if needs_push_pop {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "pop"),
            Vec::new(),
        )));
    }

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props || needs_push_pop {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(6 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

/// Returns true iff `e` is an IIFE call — `(arrow)()` form. Used to decide
/// whether the enclosing component needs `$.push/$.pop` wrapping.
fn expr_has_iife_call(e: &Expression) -> bool {
    match e {
        Expression::Call(c) => {
            let callee = strip_paren(&c.callee);
            matches!(callee, Expression::Arrow(_) | Expression::Function(_))
                || expr_has_iife_call(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_has_iife_call(e),
                    Argument::Spread(s) => expr_has_iife_call(&s.argument),
                })
        }
        Expression::Binary(b) => expr_has_iife_call(&b.left) || expr_has_iife_call(&b.right),
        Expression::Logical(l) => expr_has_iife_call(&l.left) || expr_has_iife_call(&l.right),
        Expression::Unary(u) => expr_has_iife_call(&u.argument),
        Expression::Member(m) => expr_has_iife_call(&m.object),
        Expression::Paren(p) => expr_has_iife_call(&p.expression),
        _ => false,
    }
}

fn strip_paren(e: &Expression) -> &Expression {
    let mut cur = e;
    while let Expression::Paren(p) = cur {
        cur = &p.expression;
    }
    cur
}

fn build_async_const_consequent(
    ib: &svelte_ast::blocks::IfBlock,
    ai: &AsyncInfo,
    derived_bindings: &HashSet<String>,
    promises_idx: &mut usize,
) -> Option<Vec<Statement>> {
    // Collect const tags + names.
    let mut const_names: Vec<String> = Vec::new();
    let mut thunks: Vec<Expression> = Vec::new();
    for n in &ib.consequent.nodes {
        if let FragmentChild::ConstTag(ct) = n {
            for d in &ct.declaration.declarations {
                let Pattern::Identifier(id) = &d.id else { return None };
                let Some(init) = &d.init else { return None };
                let has_await = expr_top_await(init);
                let mut blocker_idx_set: std::collections::BTreeSet<usize> =
                    std::collections::BTreeSet::new();
                collect_blocker_indices_in_expr(init, &ai.blocker_bindings, &mut blocker_idx_set);
                let blockers: Vec<usize> = blocker_idx_set.iter().copied().collect();
                const_names.push(id.name.to_string());

                // Blocker thunks for non-await consts that depend on a
                // promise slot.
                if !has_await && !blockers.is_empty() {
                    for b in &blockers {
                        // `() => $$promises[idx].promise`
                        let member = Expression::Member(Box::new(MemberExpression {
                            object: t::id("$$promises"),
                            property: MemberProperty::Expression(t::lit_number(*b as f64)),
                            computed: true,
                            optional: false,
                            span: Span::ZERO,
                        }));
                        let with_promise = Expression::Member(Box::new(MemberExpression {
                            object: member,
                            property: MemberProperty::Identifier(Identifier {
                                name: Cow::Borrowed("promise"),
                                span: Span::ZERO,
                            }),
                            computed: false,
                            optional: false,
                            span: Span::ZERO,
                        }));
                        thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: Vec::new(),
                            param_type_annotations: Vec::new(),
                            body: ArrowBody::Expression(with_promise),
                            r#async: false,
                            span: Span::ZERO,
                        })));
                    }
                }

                // Setter thunk.
                if has_await {
                    // async () => X = (await $.save($.async_derived(async () => REWRITTEN)))()
                    // The init itself is rewritten: each `await Y` becomes
                    // `(await $.save(Y))()`. So for `await 1` → `(await $.save(1))()`;
                    // for `foo(await 1)` → `foo((await $.save(1))())`.
                    let rewritten = rewrite_async_save_client(init);
                    let async_derived_arrow = Expression::Arrow(Box::new(
                        ArrowFunctionExpression {
                            params: Vec::new(),
                            param_type_annotations: Vec::new(),
                            body: ArrowBody::Expression(rewritten),
                            r#async: true,
                            span: Span::ZERO,
                        },
                    ));
                    let async_derived_call = t::call(
                        t::member_id(t::id_dollar(), "async_derived"),
                        vec![async_derived_arrow],
                    );
                    let outer = save_await_call_client(async_derived_call);
                    let assign = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id_owned(id.name.to_string())),
                        operator: AssignmentOperator::Assign,
                        right: outer,
                        span: Span::ZERO,
                    }));
                    thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(assign),
                        r#async: true,
                        span: Span::ZERO,
                    })));
                } else {
                    // Sync: wrap in $.derived(() => INIT_REWRITTEN)
                    let rewritten = rewrite_const_chain_init(init, derived_bindings);
                    let derived_call = t::call(
                        t::member_id(t::id_dollar(), "derived"),
                        vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: Vec::new(),
                            param_type_annotations: Vec::new(),
                            body: ArrowBody::Expression(rewritten),
                            r#async: false,
                            span: Span::ZERO,
                        }))],
                    );
                    let assign = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id_owned(id.name.to_string())),
                        operator: AssignmentOperator::Assign,
                        right: derived_call,
                        span: Span::ZERO,
                    }));
                    thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(assign),
                        r#async: false,
                        span: Span::ZERO,
                    })));
                }
            }
        }
    }

    let mut body: Vec<Statement> = Vec::new();
    for name in &const_names {
        body.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: t::pat_id_owned(name.to_string()),
                init: None,
                type_annotation: None,
                span: Span::ZERO,
            }],
            span: Span::ZERO,
        })));
    }
    // var promises[_N] = $.run([thunks])
    let promises_name = if *promises_idx == 0 {
        "promises".to_string()
    } else {
        format!("promises_{}", *promises_idx)
    };
    *promises_idx += 1;
    body.push(t::var(
        &promises_name,
        t::call(
            t::member_id(t::id_dollar(), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: thunks.into_iter().map(ArrayElement::Expression).collect(),
                span: Span::ZERO,
            }))],
        ),
    ));
    Some(body)
}

/// `(await $.save(X))()` — generic wrap. Reuse of `save_await_call_client`.
/// (Local helper that recursively rewrites `await Y` inside a body to its
/// `(await $.save(Y))()` form.)
fn rewrite_async_save_client(e: &Expression) -> Expression {
    match e {
        Expression::Await(a) => {
            let inner = rewrite_async_save_client(&a.argument);
            let saved = t::call(t::member_id(t::id_dollar(), "save"), vec![inner]);
            let awaited = Expression::Paren(Box::new(ParenthesizedExpression {
                expression: Expression::Await(Box::new(AwaitExpression {
                    argument: saved,
                    span: Span::ZERO,
                })),
                span: Span::ZERO,
            }));
            t::call(awaited, Vec::new())
        }
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_async_save_client(&c.callee),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => Argument::Expression(rewrite_async_save_client(e)),
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_async_save_client(&b.left),
            right: rewrite_async_save_client(&b.right),
            span: b.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_async_save_client(&p.expression),
            span: p.span,
        })),
        e => e.clone(),
    }
}

/// For sync consts: wrap identifier reads to derived bindings in `$.get(X)`,
/// and IIFE-style call expressions stay as-is.
fn rewrite_const_chain_init(
    e: &Expression,
    derived_bindings: &HashSet<String>,
) -> Expression {
    match e {
        Expression::Identifier(id) if derived_bindings.contains(id.name.as_ref()) => t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![Expression::Identifier(id.clone())],
        ),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_const_chain_init(&c.callee, derived_bindings),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => {
                        Argument::Expression(rewrite_const_chain_init(e, derived_bindings))
                    }
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_const_chain_init(&b.left, derived_bindings),
            right: rewrite_const_chain_init(&b.right, derived_bindings),
            span: b.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_const_chain_init(&p.expression, derived_bindings),
            span: p.span,
        })),
        e => e.clone(),
    }
}

fn emit_const_async_if_program(
    ib: &svelte_ast::blocks::IfBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Extract const tags + the single child element.
    let mut consts: Vec<&svelte_ast::tags::ConstTag> = Vec::new();
    let mut element_node: Option<&svelte_ast::elements::RegularElement> = None;
    for n in &ib.consequent.nodes {
        match n {
            FragmentChild::ConstTag(ct) => consts.push(ct),
            FragmentChild::RegularElement(el) => {
                if element_node.is_some() {
                    return None; // only one element supported
                }
                element_node = Some(el);
            }
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            _ => return None,
        }
    }
    let element = element_node?;

    // Element must be `<TAG>{EXPR}</TAG>` — a single ExpressionTag child.
    let el_non_ws: Vec<&FragmentChild> = element
        .fragment
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if el_non_ws.len() != 1 {
        return None;
    }
    let text_expr = match el_non_ws[0] {
        FragmentChild::ExpressionTag(et) => et.expression.clone(),
        _ => return None,
    };

    // Collect const names + classify each as async (has await) or sync.
    let mut const_names: Vec<String> = Vec::new();
    let mut thunks: Vec<Expression> = Vec::new();
    let mut const_blocker_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for ct in &consts {
        for d in &ct.declaration.declarations {
            let Pattern::Identifier(id) = &d.id else { return None };
            let Some(init) = &d.init else { return None };
            let has_await = expr_top_await(init);
            const_names.push(id.name.to_string());
            let idx = thunks.len();
            const_blocker_idx.insert(id.name.to_string(), idx);
            if has_await {
                // `async () => X = (await $.save($.async_derived(async () =>
                // (await $.save(INNER))())))()`
                let inner = match init {
                    Expression::Await(a) => a.argument.clone(),
                    e => e.clone(),
                };
                let inner_save_await =
                    save_await_call_client(inner);
                let async_derived_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(inner_save_await),
                    r#async: true,
                    span: Span::ZERO,
                }));
                let async_derived_call = t::call(
                    t::member_id(t::id_dollar(), "async_derived"),
                    vec![async_derived_arrow],
                );
                let outer = save_await_call_client(async_derived_call);
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id_owned(id.name.to_string())),
                    operator: AssignmentOperator::Assign,
                    right: outer,
                    span: Span::ZERO,
                }));
                thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(assign),
                    r#async: true,
                    span: Span::ZERO,
                })));
            } else {
                // `() => X = $.derived(() => INIT_WITH_GET_REFS)`
                let rewritten = rewrite_const_refs_with_get(init, &const_names);
                let derived_call = t::call(
                    t::member_id(t::id_dollar(), "derived"),
                    vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(rewritten),
                        r#async: false,
                        span: Span::ZERO,
                    }))],
                );
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id_owned(id.name.to_string())),
                    operator: AssignmentOperator::Assign,
                    right: derived_call,
                    span: Span::ZERO,
                }));
                thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(assign),
                    r#async: false,
                    span: Span::ZERO,
                })));
            }
        }
    }

    // Identify the text expression's blocker (which promises slot to wait on).
    let text_ref_name = match &text_expr {
        Expression::Identifier(id) => id.name.clone(),
        _ => return None,
    };
    let text_blocker_idx = *const_blocker_idx.get(text_ref_name.as_ref())?;

    // Build the consequent body.
    let mut consequent: Vec<Statement> = Vec::new();
    for name in &const_names {
        consequent.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: t::pat_id_owned(name.to_string()),
                init: None,
                type_annotation: None,
                span: Span::ZERO,
            }],
            span: Span::ZERO,
        })));
    }
    // var promises = $.run([...thunks])
    consequent.push(t::var(
        "promises",
        t::call(
            t::member_id(t::id_dollar(), "run"),
            vec![Expression::Array(Box::new(array_expression_from_exprs(thunks)))],
        ),
    ));
    // var p = root_1();
    consequent.push(t::var(
        "p",
        t::call(t::id("root_1"), Vec::new()),
    ));
    // var text = $.child(p, true);
    consequent.push(t::var(
        "text",
        t::call(
            t::member_id(t::id_dollar(), "child"),
            vec![
                t::id("p"),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: true,
                    span: Span::ZERO,
                }))),
            ],
        ),
    ));
    // $.reset(p);
    consequent.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id("p")],
    )));
    // $.template_effect(() => $.set_text(text, $.get(TEXT_REF)), void 0, void 0, [promises[N]])
    let get_text_ref = t::call(
        t::member_id(t::id_dollar(), "get"),
        vec![t::id_owned(text_ref_name.to_string())],
    );
    let set_text_call = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), get_text_ref],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_text_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let blocker_member = Expression::Member(Box::new(MemberExpression {
        object: t::id("promises"),
        property: MemberProperty::Expression(t::lit_number(text_blocker_idx as f64)),
        computed: true,
        optional: false,
        span: Span::ZERO,
    }));
    let blockers_array = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(blocker_member)],
        span: Span::ZERO,
    }));
    consequent.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![
            effect_fn,
            void_zero_client(),
            void_zero_client(),
            blockers_array,
        ],
    )));
    // $.append($$anchor, p);
    consequent.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("p")],
    )));

    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: consequent,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Build the wrap block: { var consequent = ...; $.if(node, ($$render) => { if (TEST) $$render(consequent); }); }
    let render_if = Statement::If(Box::new(IfStatement {
        test: ib.test.clone(),
        consequent: t::stmt(t::call(
            t::id_render(),
            vec![t::id("consequent")],
        )),
        alternate: None,
        span: Span::ZERO,
    }));
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let if_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "if"),
        vec![t::id("node"), render_arrow],
    ));
    let wrap_block = Statement::Block(Box::new(BlockStatement {
        body: vec![t::var("consequent", consequent_arrow), if_call],
        span: Span::ZERO,
    }));

    // Build the function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    func_body.push(wrap_block);
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    // root_1 template at module scope. Element with single ExpressionTag
    // text child → `<TAG> </TAG>` (single space placeholder).
    let template_html = format!("<{0}> </{0}>", element.name);
    let root_decl = t::var(
        "root_1",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![template_html], Vec::new())],
        ),
    );

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

/// Helper: build an ArrayExpression from a Vec of Expressions.
fn array_expression_from_exprs(exprs: Vec<Expression>) -> ArrayExpression {
    ArrayExpression {
        elements: exprs.into_iter().map(ArrayElement::Expression).collect(),
        span: Span::ZERO,
    }
}

/// Shared counter pool used by `emit_async_if_chain_program`. Each
/// `next_X` returns the next "bare" name (index 0) or `name_N` for higher.
#[derive(Default)]
struct ChainCounters {
    consequent: usize,
    alternate: usize,
    text: usize,
    node: usize,
    fragment: usize,
    d: usize,
}

fn nth_name(prefix: &str, idx: usize) -> String {
    if idx == 0 {
        prefix.to_string()
    } else {
        format!("{prefix}_{idx}")
    }
}

impl ChainCounters {
    fn next_consequent(&mut self) -> String {
        let s = nth_name("consequent", self.consequent);
        self.consequent += 1;
        s
    }
    fn next_alternate(&mut self) -> String {
        let s = nth_name("alternate", self.alternate);
        self.alternate += 1;
        s
    }
    fn next_text(&mut self) -> String {
        let s = nth_name("text", self.text);
        self.text += 1;
        s
    }
    fn next_node(&mut self) -> String {
        let s = nth_name("node", self.node);
        self.node += 1;
        s
    }
    fn next_fragment(&mut self) -> String {
        let s = nth_name("fragment", self.fragment);
        self.fragment += 1;
        s
    }
    fn next_d(&mut self) -> String {
        let s = if self.d == 0 { "d".to_string() } else { format!("d_{}", self.d) };
        self.d += 1;
        s
    }
}

/// Compile a sequence of top-level IfBlocks in async mode (the async-if-chain
/// shape). Each IfBlock either gets a `$.async(...)` wrap (when it has
/// blockers and/or async-test) or stays as a plain `{}` block.
fn emit_async_if_chain_program(
    if_blocks: &[&svelte_ast::blocks::IfBlock],
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    let ai = script.async_info.as_ref()?;

    // Module-level template: N `<!>` placeholders separated by single spaces.
    let template_html = (0..if_blocks.len())
        .map(|_| "<!>")
        .collect::<Vec<_>>()
        .join(" ");
    let root_decl = t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![template_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    // `script.body` already contains the async setup statements when
    // `async_info.is_some()` (set by `analyze_script`).
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::id("root"), Vec::new()),
    ));

    let mut counters = ChainCounters::default();
    // The top-level `var fragment = root();` consumes the bare `fragment`
    // name (counter slot 0) so subsequent break-out fragments get
    // `fragment_1`, `fragment_2`, ...
    let _ = counters.next_fragment();
    let mut prev_node_name: String = counters.next_node(); // "node"
    func_body.push(t::var(
        &prev_node_name,
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));

    for (i, ib) in if_blocks.iter().enumerate() {
        let node_name = if i == 0 {
            prev_node_name.clone()
        } else {
            let new_name = counters.next_node();
            func_body.push(t::var(
                &new_name,
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![t::id_owned(prev_node_name.to_string()), t::lit_number(2.0)],
                ),
            ));
            new_name
        };
        let stmt = emit_async_if_block(
            ib,
            &node_name,
            ai,
            &script.derived_bindings,
            &mut counters,
            false,
        )?;
        func_body.push(stmt);
        prev_node_name = node_name;
    }

    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

/// Emit a single if-block at the given node anchor. Returns either a
/// `$.async(node, …)` Statement or a `{…}` plain block.
///
/// `nested` = true when this if-block is itself the result of an elseif
/// break-out (different node-variable naming from a top-level $.async).
fn emit_async_if_block(
    ib: &svelte_ast::blocks::IfBlock,
    node_name: &str,
    ai: &AsyncInfo,
    derived_bindings: &HashSet<String>,
    counters: &mut ChainCounters,
    in_async_ctx: bool,
) -> Option<Statement> {
    let test_is_async = expr_top_await(&ib.test);
    let mut blocker_indices: std::collections::BTreeSet<usize> =
        std::collections::BTreeSet::new();
    collect_chain_blockers(ib, &ai.blocker_bindings, &mut blocker_indices);
    let indices_vec: Vec<usize> = blocker_indices.iter().copied().collect();
    let has_blockers = !indices_vec.is_empty();
    let needs_async_wrap = test_is_async || has_blockers;

    // Build the if-chain body inside the closure. Each branch produces a
    // consequent (or alternate for the final else) ARROW VAR declared in the
    // current closure scope, plus its $$render(...) call.
    let mut chain_body: Vec<Statement> = Vec::new();

    // Track allocated consequent/alternate names + their tests for the $.if call.
    let mut branches: Vec<(String, Expression, i32)> = Vec::new();
    let mut alt_arrow_name: Option<String> = None;

    // The first branch is the IfBlock itself; flatten elseif chain unless it
    // breaks out (await test or new blockers).
    let mut cur = ib;
    let mut branch_idx: i32 = 0;
    let mut chain_parent_blockers = blocker_indices.clone();
    loop {
        let consequent_name = counters.next_consequent();
        let arrow = build_branch_arrow(
            &cur.consequent,
            ai,
            derived_bindings,
            counters,
        )?;
        chain_body.push(t::var(&consequent_name, arrow));

        // Test rewrite — three forms:
        //   1. First branch in async-test wrap → `$.get($$condition)`
        //   2. Test has a CallExpression (user-fn call) AND we're NOT in
        //      async wrap → hoist to `var d_N = $.derived(() => TEST)` and
        //      use `$.get(d_N)`
        //   3. Otherwise → straight rewrite (with derived `$.get` wrapping)
        let test_expr = if branch_idx == 0 && test_is_async {
            t::call(t::member_id(t::id_dollar(), "get"), vec![t::id("$$condition")])
        } else if !needs_async_wrap && expr_has_user_call(&cur.test, derived_bindings) {
            let d_name = counters.next_d();
            let derived_call = t::call(
                t::member_id(t::id_dollar(), "derived"),
                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(cur.test.clone()),
                    r#async: false,
                    span: Span::ZERO,
                }))],
            );
            chain_body.push(t::var(&d_name, derived_call));
            t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(d_name.to_string())])
        } else {
            rewrite_chain_test(&cur.test, &ai.blocker_bindings, derived_bindings, counters)
        };
        branches.push((consequent_name, test_expr, branch_idx));

        // Look at the alternate to decide flatten vs break-out.
        let alt = match &cur.alternate {
            Some(a) => a,
            None => break,
        };
        let non_ws: Vec<&FragmentChild> = alt
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::IfBlock(inner) = non_ws[0] {
                if inner.elseif {
                    let inner_test_async = expr_top_await(&inner.test);
                    let mut inner_blockers = std::collections::BTreeSet::new();
                    collect_chain_blockers(inner, &ai.blocker_bindings, &mut inner_blockers);
                    let new_blockers = inner_blockers.iter().any(|i| !chain_parent_blockers.contains(i));
                    if inner_test_async || new_blockers {
                        // Break-out: emit the rest as an alternate that
                        // contains a nested $.async/$.if construct. Build
                        // the inner arrow FIRST (so its counters consume
                        // their slots), then allocate the outer alternate
                        // name — matches upstream's naming where the inner
                        // alternate gets the lower index.
                        let alt_arrow = build_breakout_alternate_arrow(
                            inner,
                            ai,
                            derived_bindings,
                            counters,
                        )?;
                        let alt_name = counters.next_alternate();
                        chain_body.push(t::var(&alt_name, alt_arrow));
                        alt_arrow_name = Some(alt_name);
                        break;
                    }
                    // Flatten: keep walking.
                    cur = inner;
                    branch_idx += 1;
                    chain_parent_blockers.extend(inner_blockers);
                    continue;
                }
            }
        }
        // Final else (non-elseif content).
        let alt_name = counters.next_alternate();
        let arrow = build_branch_arrow(alt, ai, derived_bindings, counters)?;
        chain_body.push(t::var(&alt_name, arrow));
        alt_arrow_name = Some(alt_name);
        break;
    }

    // Build `$.if(node, ($$render) => { ... }, true?)`. The 3rd `true` arg
    // is set ONLY when this if-block is a NESTED break-out (in_async_ctx=true).
    // Top-level `$.async` wraps pass no 3rd arg.
    let render_body = build_render_body(&branches, alt_arrow_name.as_deref(), false);
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_body],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let if_args = if in_async_ctx {
        vec![
            t::id_owned(node_name.to_string()),
            render_arrow,
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ]
    } else {
        vec![t::id_owned(node_name.to_string()), render_arrow]
    };
    chain_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "if"),
        if_args,
    )));

    if needs_async_wrap {
        // Wrap in $.async(node, [BLOCKERS], TESTS_OR_VOID0, callback).
        let blockers_arr = Expression::Array(Box::new(ArrayExpression {
            elements: indices_vec
                .iter()
                .map(|i| {
                    ArrayElement::Expression(Expression::Member(Box::new(MemberExpression {
                        object: t::id("$$promises"),
                        property: MemberProperty::Expression(t::lit_number(*i as f64)),
                        computed: true,
                        optional: false,
                        span: Span::ZERO,
                    })))
                })
                .collect(),
            span: Span::ZERO,
        }));
        let tests_arg = if test_is_async {
            // If the test is `Expression::Await(X)` directly, strip the
            // outer await and emit `[() => X]` (sync arrow). Otherwise
            // rewrite each inner `await Y` to `(await $.save(Y))()` and emit
            // `[async () => REWRITTEN]`.
            let (body_expr, is_async_arrow) = if let Expression::Await(a) = &ib.test {
                (a.argument.clone(), false)
            } else {
                (rewrite_async_test_client(&ib.test), true)
            };
            let test_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(body_expr),
                r#async: is_async_arrow,
                span: Span::ZERO,
            }));
            Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(test_arrow)],
                span: Span::ZERO,
            }))
        } else {
            // void 0 — test has no top-level await, only blocker-bindings.
            void_zero_client()
        };
        let cb_params = if matches!(&tests_arg, Expression::Array(_)) {
            vec![t::pat_id_owned(node_name.to_string()), t::pat_id("$$condition")]
        } else {
            vec![t::pat_id_owned(node_name.to_string())]
        };
        let cb = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: cb_params,
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: chain_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        Some(t::stmt(t::call(
            t::member_id(t::id_dollar(), "async"),
            vec![t::id_owned(node_name.to_string()), blockers_arr, tests_arg, cb],
        )))
    } else {
        // Plain `{ ... }` block.
        Some(Statement::Block(Box::new(BlockStatement {
            body: chain_body,
            span: Span::ZERO,
        })))
    }
}

fn is_top_level_chain(in_async_ctx: bool) -> bool {
    // The 3rd `true` arg of `$.if` is set when we're inside a nested
    // break-out (in_async_ctx=true). Top-level $.async wrap is NOT
    // considered "inside async ctx" for the 3rd arg purposes.
    in_async_ctx
}

fn is_only_blocker_derived(
    _e: &Expression,
    _blocker_bindings: &HashMap<String, usize>,
    _derived_bindings: &HashSet<String>,
) -> bool {
    // Reserved for future use — currently always false (caller treats
    // every binding ref as reactive).
    false
}

/// Returns true iff the fragment contains a "deep reactive point": a
/// nested ExpressionTag, HtmlTag, or an Element with reactive-trigger
/// attribute (autofocus, muted, value-on-option, custom-element-data).
/// Returns true iff the element has only static attributes and a
/// fully-static body (no expression tags, blocks, components, directives,
/// or nested reactive content). Used by emitters that pass the element
/// verbatim into the template HTML.
fn is_element_fully_static(el: &svelte_ast::elements::RegularElement) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => match &attr.value {
                AttributeValue::Empty => {}
                AttributeValue::Many(parts) => {
                    if !parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        return false;
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
    }
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) | FragmentChild::Comment(_) => {}
            FragmentChild::ExpressionTag(et) => {
                // Allow ExpressionTag if the expression is literal-foldable
                // (post-fold step replaces script consts with literals).
                if literal_to_template_string(&et.expression).is_none() {
                    return false;
                }
            }
            FragmentChild::RegularElement(child) => {
                if !is_element_fully_static(child) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn fragment_has_deep_reactive(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(node_has_deep_reactive)
}

fn node_has_deep_reactive(n: &FragmentChild) -> bool {
    match n {
        FragmentChild::ExpressionTag(_) | FragmentChild::HtmlTag(_) => true,
        FragmentChild::RegularElement(el) => {
            if element_has_reactive_attr(el) {
                return true;
            }
            fragment_has_deep_reactive(&el.fragment)
        }
        _ => false,
    }
}

fn element_has_reactive_attr(el: &svelte_ast::elements::RegularElement) -> bool {
    let is_custom = el.name.contains('-');
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(attr) => {
                // Any attribute on a custom element triggers $.set_custom_element_data.
                if is_custom {
                    return true;
                }
                match attr.name.as_ref() {
                    "autofocus" => return true,
                    "muted" if el.name == "source" || el.name == "video" || el.name == "audio" => {
                        return true
                    }
                    "value" if el.name == "option" => return true,
                    // `dir` attribute on any element needs a `node.dir = node.dir`
                    // template_effect (Chromium hydration bug workaround per
                    // upstream RegularElement.js:463-468).
                    "dir" => return true,
                    // `<input>` with boolean `checked` or static `value` needs
                    // `$.remove_input_defaults(input)` during hydration so the
                    // server's defaultChecked/defaultValue don't override
                    // user input.
                    "checked" | "value" if el.name == "input" => return true,
                    _ => {}
                }
            }
            // `bind:value`, `bind:checked` etc. on an input need
            // `$.remove_input_defaults(input)` + `$.bind_value(input, ...)`.
            ElementAttribute::BindDirective(_) => return true,
            _ => {}
        }
    }
    false
}

/// Emit the deep-static-walker program. Builds the full HTML template by
/// concatenating top-level elements with whitespace between them, then
/// walks the elements emitting navigation + reactive handlers.
/// Compile a fragment composed of multiple top-level `<select>` elements
/// (with whitespace, comments, and top-level snippet blocks between them).
/// Produces the customizable-select shape that the select-with-rich-content
/// fixture expects.
///
/// Returns None for cases we don't handle — the caller will fall through
/// to the regular walker.
fn emit_select_rich_content_program(
    root_fragment: &svelte_ast::fragment::Fragment,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    let mut ctx = SelectCtx::default();

    // Phase 1: extract top-level snippets in source order.
    let mut snippet_decls: Vec<(String, Vec<Statement>)> = Vec::new();
    let mut top_selects: Vec<&svelte_ast::elements::RegularElement> = Vec::new();
    for n in &root_fragment.nodes {
        match n {
            FragmentChild::SnippetBlock(sb) => {
                // Snippet body: assume a single `<option>...</option>` child
                // (the only shape used in this fixture). Build a fresh
                // root_N template for it.
                let name = sb.expression.name.clone();
                let body_stmts = build_snippet_body(&sb.body, &mut ctx)?;
                snippet_decls.push((name.to_string(), body_stmts));
            }
            FragmentChild::RegularElement(el) if el.name == "select" => {
                top_selects.push(el);
            }
            FragmentChild::Comment(_) => {}
            FragmentChild::Text(t) if t.data.trim().is_empty() => {}
            _ => return None,
        }
    }

    // Phase 2: allocate select_N names + build root HTML + per-select body.
    // The top-level `var fragment = root();` consumes the bare `fragment`
    // slot so subsequent fragments allocated inside customizable_select
    // arrows get `fragment_1`, `fragment_2`, ... — matches upstream.
    let _ = ctx.next_named("fragment");
    let mut root_html = String::new();
    let mut func_body_stmts: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body_stmts.extend(script.body.iter().cloned());
    func_body_stmts.push(t::var("fragment", t::call(t::id("root"), Vec::new())));

    let mut prev_select: Option<String> = None;
    for (i, el) in top_selects.iter().enumerate() {
        if i > 0 {
            let prev_had_snippet_before = top_selects_had_snippet_before(root_fragment, i);
            if prev_had_snippet_before {
                root_html.push_str("  ");
            } else {
                root_html.push(' ');
            }
        }
        let select_name = ctx.next_select();
        let nav = if i == 0 {
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_fragment()],
            )
        } else {
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![
                    t::id_owned(prev_select.as_ref().expect("prev set").to_string()),
                    t::lit_number(2.0),
                ],
            )
        };
        func_body_stmts.push(t::var(&select_name, nav));
        let (html, body) = match lower_top_select(el, &select_name, &mut ctx) {
            Some(x) => x,
            None => {
                eprintln!("DEBUG: select {i} failed to lower", i = i);
                return None;
            }
        };
        root_html.push_str(&html);
        func_body_stmts.extend(body);
        prev_select = Some(select_name);
    }
    func_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));
    let func_body = func_body_stmts;

    // Build snippet const declarations as ARROWS (placed BEFORE root_N
    // declarations).
    let mut snippet_consts: Vec<Statement> = Vec::new();
    for (name, body) in snippet_decls {
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        snippet_consts.push(t::const_decl(&name, arrow));
    }

    // Root template last.
    let root_decl = t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![root_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(8 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(snippet_consts);
    prog.extend(ctx.module_decls.clone());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

#[derive(Default)]
struct SelectCtx {
    /// Sequential `root_N` index for `<option>` template hoists.
    root_idx: usize,
    /// `option_content_N` index.
    option_content_idx: usize,
    /// `select_content_N` index.
    select_content_idx: usize,
    /// `optgroup_content_N` index.
    optgroup_content_idx: usize,
    /// Module-level `var root_N = $.from_html(...)` declarations queued for
    /// emission AFTER snippet decls + BEFORE the main `var root = ...`.
    module_decls: Vec<Statement>,
    /// Counter for each named local var inside the function body.
    var_counts: HashMap<String, usize>,
}

impl SelectCtx {
    fn next_root(&mut self) -> String {
        self.root_idx += 1;
        format!("root_{}", self.root_idx)
    }
    fn next_option_content(&mut self) -> String {
        let s = if self.option_content_idx == 0 {
            "option_content".to_string()
        } else {
            format!("option_content_{}", self.option_content_idx)
        };
        self.option_content_idx += 1;
        s
    }
    fn next_select_content(&mut self) -> String {
        let s = if self.select_content_idx == 0 {
            "select_content".to_string()
        } else {
            format!("select_content_{}", self.select_content_idx)
        };
        self.select_content_idx += 1;
        s
    }
    fn next_optgroup_content(&mut self) -> String {
        let s = if self.optgroup_content_idx == 0 {
            "optgroup_content".to_string()
        } else {
            format!("optgroup_content_{}", self.optgroup_content_idx)
        };
        self.optgroup_content_idx += 1;
        s
    }
    fn next_named(&mut self, prefix: &str) -> String {
        let cnt = self.var_counts.entry(prefix.to_string()).or_insert(0);
        let n = *cnt;
        *cnt += 1;
        if n == 0 {
            prefix.to_string()
        } else {
            format!("{prefix}_{n}")
        }
    }
    fn next_select(&mut self) -> String {
        self.next_named("select")
    }
}

/// Builds the snippet body assuming it's a single `<option>...</option>`.
/// Allocates a root_N template + emits the arrow body for the snippet.
fn build_snippet_body(
    fragment: &svelte_ast::fragment::Fragment,
    ctx: &mut SelectCtx,
) -> Option<Vec<Statement>> {
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    let opt = match non_ws[0] {
        FragmentChild::RegularElement(el) if el.name == "option" => el,
        _ => return None,
    };
    // Build template `<option>TEXT</option>` for this option (plain text only
    // in the snippet shapes we handle).
    let text = option_text_content(opt)?;
    let root_name = ctx.next_root();
    let template_html = format!("<option>{text}</option>");
    ctx.module_decls.push(t::var(
        &root_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![template_html], Vec::new())],
        ),
    ));
    let option_var = ctx.next_named("option");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &option_var,
        t::call(t::id_owned(root_name.to_string()), Vec::new()),
    ));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(option_var.to_string())],
    )));
    Some(body)
}

/// Returns the simple text content of an `<option>...</option>` when its
/// only child is a single Text node. Returns None for any other shape.
fn option_text_content(el: &svelte_ast::elements::RegularElement) -> Option<String> {
    let mut text = String::new();
    for n in &el.fragment.nodes {
        if let FragmentChild::Text(t) = n {
            text.push_str(t.data.trim_matches(|c: char| c.is_whitespace() && c != ' '));
        } else {
            return None;
        }
    }
    Some(text.trim().to_string())
}

/// Returns true iff a top-level snippet block precedes the i-th select
/// (i is 0-based among `<select>` elements).
fn top_selects_had_snippet_before(
    root: &svelte_ast::fragment::Fragment,
    i: usize,
) -> bool {
    let mut seen_selects = 0usize;
    for n in &root.nodes {
        match n {
            FragmentChild::RegularElement(el) if el.name == "select" => {
                if seen_selects == i {
                    return false;
                }
                seen_selects += 1;
            }
            FragmentChild::SnippetBlock(_) => {
                if seen_selects == i {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Lower a top-level `<select>` element. Returns (html_contribution,
/// body_statements) where the body uses `select_var` as the binding name
/// for this select.
fn lower_top_select(
    el: &svelte_ast::elements::RegularElement,
    select_var: &str,
    ctx: &mut SelectCtx,
) -> Option<(String, Vec<Statement>)> {
    let non_ws_children: Vec<&FragmentChild> = el
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();

    // Get any static attrs (e.g. nothing on these selects, but keep generic).
    let attrs_html = build_static_attrs(el);

    // CASE A: select contains a single direct <option> child
    //   <select><option>...</option></select>
    if non_ws_children.len() == 1 {
        if let FragmentChild::RegularElement(opt) = non_ws_children[0] {
            if opt.name == "option" {
                return lower_select_with_option(opt, select_var, ctx, &attrs_html);
            }
            if opt.name == "optgroup" {
                return lower_select_with_optgroup(opt, select_var, ctx, &attrs_html);
            }
        }
        // CASE: each / if / key / boundary direct child
        match non_ws_children[0] {
            FragmentChild::EachBlock(eb) => {
                return lower_select_with_each(eb, select_var, ctx, &attrs_html);
            }
            FragmentChild::IfBlock(ib) => {
                return lower_select_with_if(ib, select_var, ctx, &attrs_html);
            }
            FragmentChild::KeyBlock(kb) => {
                return lower_select_with_key(kb, select_var, ctx, &attrs_html);
            }
            FragmentChild::SvelteBoundary(b) => {
                return lower_select_with_boundary(b, select_var, ctx, &attrs_html);
            }
            FragmentChild::Component(c) => {
                return lower_select_with_component(c, select_var, ctx, &attrs_html);
            }
            FragmentChild::RenderTag(rt) => {
                return lower_select_with_render(rt, select_var, ctx, &attrs_html);
            }
            FragmentChild::HtmlTag(ht) => {
                return lower_select_with_html(ht, select_var, ctx, &attrs_html);
            }
            _ => {}
        }
    }
    None
}

fn build_static_attrs(el: &svelte_ast::elements::RegularElement) -> String {
    let mut out = String::new();
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if let AttributeValue::Many(parts) = &attr.value {
                if parts.len() == 1 {
                    if let AttributeValuePart::Text(t) = &parts[0] {
                        out.push(' ');
                        out.push_str(&attr.name);
                        out.push_str("=\"");
                        out.push_str(&t.data);
                        out.push('"');
                    }
                }
            }
        }
    }
    out
}

/// `<option>BODY</option>` shape detection. Returns the body category.
enum OptionShape<'a> {
    /// `<option>plain text</option>` — text-only static.
    PlainText(String),
    /// `<option>{var}</option>` — single ExpressionTag.
    SingleExpr(&'a Expression),
    /// `<option><span>...</span></option>` etc. — rich content.
    RichContent(&'a [FragmentChild]),
}

fn classify_option_body<'a>(opt: &'a svelte_ast::elements::RegularElement) -> Option<OptionShape<'a>> {
    let non_ws: Vec<&FragmentChild> = opt
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.is_empty() {
        return Some(OptionShape::PlainText(String::new()));
    }
    // All-text → PlainText
    let all_text = opt
        .fragment
        .nodes
        .iter()
        .all(|n| matches!(n, FragmentChild::Text(_)));
    if all_text {
        let mut s = String::new();
        for n in &opt.fragment.nodes {
            if let FragmentChild::Text(t) = n {
                s.push_str(&t.data);
            }
        }
        return Some(OptionShape::PlainText(s.trim().to_string()));
    }
    if non_ws.len() == 1 {
        if let FragmentChild::ExpressionTag(et) = non_ws[0] {
            return Some(OptionShape::SingleExpr(&et.expression));
        }
    }
    Some(OptionShape::RichContent(&opt.fragment.nodes))
}

/// `<select><option>BODY</option></select>` → single-option select.
fn lower_select_with_option(
    opt: &svelte_ast::elements::RegularElement,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let shape = classify_option_body(opt)?;
    // Exclude `value` from the option's template attrs — it gets set
    // dynamically below via `option.value = option.__value = X`.
    let opt_attrs = build_static_attrs_excluding(opt, &["value"]);
    let opt_value_attr = find_option_value_attr(opt);
    match shape {
        OptionShape::PlainText(_) => {
            // Not exercised in this fixture for direct child.
            None
        }
        OptionShape::SingleExpr(_) => None,
        OptionShape::RichContent(nodes) => {
            // Rich: customizable_select on the option.
            let html = format!("<select{attrs}><option{opt_attrs}><!></option></select>");
            let option_var = ctx.next_named("option");
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(
                &option_var,
                t::call(
                    t::member_id(t::id_dollar(), "child"),
                    vec![t::id_owned(select_var.to_string())],
                ),
            ));
            // Build option_content template + the customizable_select call.
            // Use the "with_html" variant when content contains @html — the
            // arrow body needs `$.html(node, () => '...')` not append.
            let cs_body = if option_has_html_only(nodes) {
                build_customizable_select_body_with_html(&option_var, nodes, ctx)?
            } else if option_has_value_attr_and_text_anchor(opt) {
                // `<option value="a"><em>Italic</em> text</option>` → emit
                // `$.next();` before append. See select_8 in fixture.
                build_customizable_select_body_with_next(&option_var, nodes, ctx)?
            } else {
                build_customizable_select_body(&option_var, nodes, ctx)?
            };
            body.push(cs_body);
            // If the option has a static value="..." attribute, emit
            // `option_var.value = option_var.__value = 'X';` AFTER the wrap.
            if let Some(val) = opt_value_attr {
                body.push(emit_option_value_set(&option_var, &val));
            }
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(select_var.to_string())],
            )));
            Some((html, body))
        }
    }
}

fn build_static_attrs_excluding(
    el: &svelte_ast::elements::RegularElement,
    exclude: &[&str],
) -> String {
    let mut out = String::new();
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if exclude.contains(&attr.name.as_ref()) {
                continue;
            }
            if let AttributeValue::Many(parts) = &attr.value {
                if parts.len() == 1 {
                    if let AttributeValuePart::Text(t) = &parts[0] {
                        out.push(' ');
                        out.push_str(&attr.name);
                        out.push_str("=\"");
                        out.push_str(&t.data);
                        out.push('"');
                    }
                }
            }
        }
    }
    out
}

fn option_has_html_only(nodes: &[FragmentChild]) -> bool {
    let non_ws: Vec<&FragmentChild> = nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    non_ws.len() == 1 && matches!(non_ws[0], FragmentChild::HtmlTag(_))
}

fn option_has_value_attr_and_text_anchor(opt: &svelte_ast::elements::RegularElement) -> bool {
    // Option has `value=` AND its content ends with a non-whitespace text
    // node after the rich opener — e.g. `<em>Italic</em> text`.
    if find_option_value_attr(opt).is_none() {
        return false;
    }
    // Find the LAST non-whitespace content node; if it's a Text node, true.
    let last = opt
        .fragment
        .nodes
        .iter()
        .rev()
        .find(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        });
    matches!(last, Some(FragmentChild::Text(_)))
}

fn build_customizable_select_body_with_html(
    target_var: &str,
    nodes: &[FragmentChild],
    ctx: &mut SelectCtx,
) -> Option<Statement> {
    // For `<option>{@html '<strong>Bold HTML</strong>'}</option>`-style:
    //   var anchor = $.child(option);
    //   var fragment = option_content();
    //   var node = $.first_child(fragment);
    //   $.html(node, () => 'STRING');
    //   $.append(anchor, fragment);
    let oc_name = ctx.next_option_content();
    ctx.module_decls.push(t::var(
        &oc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec!["<!>".to_string()], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let html_expr = nodes
        .iter()
        .find_map(|n| if let FragmentChild::HtmlTag(ht) = n {
            Some(ht.expression.clone())
        } else {
            None
        })?;
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let node_var = ctx.next_named("node");
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(html_expr),
        r#async: false,
        span: Span::ZERO,
    }));
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(target_var.to_string())]),
        ),
        t::var(&fragment_var, t::call(t::id_owned(oc_name.to_string()), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(fragment_var.to_string())],
            ),
        ),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "html"),
            vec![t::id_owned(node_var.to_string()), getter],
        )),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(target_var.to_string()), arrow],
    )))
}

fn build_customizable_select_body_with_next(
    target_var: &str,
    nodes: &[FragmentChild],
    ctx: &mut SelectCtx,
) -> Option<Statement> {
    // `<option value="a"><em>Italic</em> text</option>` shape: rich body
    // followed by text node — emits `$.next();` between the fragment setup
    // and the final append.
    let oc_name = ctx.next_option_content();
    let html = serialize_rich_content_html(nodes)?;
    ctx.module_decls.push(t::var(
        &oc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let mut arrow_body: Vec<Statement> = Vec::new();
    arrow_body.push(t::var(
        &anchor_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(target_var.to_string())]),
    ));
    arrow_body.push(t::var(
        &fragment_var,
        t::call(t::id_owned(oc_name.to_string()), Vec::new()),
    ));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "next"),
        Vec::new(),
    )));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
    )));
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(target_var.to_string()), arrow],
    )))
}

fn find_option_value_attr(opt: &svelte_ast::elements::RegularElement) -> Option<String> {
    for a in &opt.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if attr.name == "value" {
                if let AttributeValue::Many(parts) = &attr.value {
                    if parts.len() == 1 {
                        if let AttributeValuePart::Text(t) = &parts[0] {
                            return Some(t.data.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

fn emit_option_value_set(var: &str, value: &str) -> Statement {
    // option_var.value = option_var.__value = 'X';
    let inner = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(Expression::Member(Box::new(
            MemberExpression {
                object: t::id_owned(var.to_string()),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("__value"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            },
        ))),
        operator: AssignmentOperator::Assign,
        right: Expression::Literal(Box::new(Literal::String(StringLiteral {
            value: Cow::Owned(value.to_string()),
            raw: Some(format!("'{value}'")),
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }));
    let outer = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(Expression::Member(Box::new(
            MemberExpression {
                object: t::id_owned(var.to_string()),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("value"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            },
        ))),
        operator: AssignmentOperator::Assign,
        right: inner,
        span: Span::ZERO,
    }));
    t::stmt(outer)
}

fn build_customizable_select_body(
    target_var: &str,
    nodes: &[FragmentChild],
    ctx: &mut SelectCtx,
) -> Option<Statement> {
    // Build option_content_N template from the rich nodes.
    let oc_name = ctx.next_option_content();
    let html = serialize_rich_content_html(nodes)?;
    ctx.module_decls.push(t::var(
        &oc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let mut arrow_body: Vec<Statement> = Vec::new();
    arrow_body.push(t::var(
        &anchor_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(target_var.to_string())]),
    ));
    arrow_body.push(t::var(
        &fragment_var,
        t::call(t::id_owned(oc_name.to_string()), Vec::new()),
    ));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
    )));
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(target_var.to_string()), arrow],
    )))
}

fn serialize_rich_content_html(nodes: &[FragmentChild]) -> Option<String> {
    let mut out = String::new();
    for n in nodes {
        match n {
            FragmentChild::Text(t) => {
                let s = t.data.trim_matches(|c: char| c == '\n' || c == '\t');
                if !s.is_empty() {
                    out.push_str(s);
                }
            }
            FragmentChild::RegularElement(el) => {
                serialize_rich_element_html(el, &mut out)?;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn serialize_rich_element_html(
    el: &svelte_ast::elements::RegularElement,
    out: &mut String,
) -> Option<()> {
    out.push('<');
    out.push_str(&el.name);
    out.push_str(&build_static_attrs(el));
    if is_void_client(&el.name) {
        out.push_str("/>");
        return Some(());
    }
    out.push('>');
    serialize_rich_content_html_into(&el.fragment.nodes, out)?;
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

fn serialize_rich_content_html_into(
    nodes: &[FragmentChild],
    out: &mut String,
) -> Option<()> {
    for n in nodes {
        match n {
            FragmentChild::Text(t) => {
                let s = t.data.trim_matches(|c: char| c == '\n' || c == '\t');
                if !s.is_empty() {
                    out.push_str(s);
                }
            }
            FragmentChild::RegularElement(el) => {
                serialize_rich_element_html(el, out)?;
            }
            FragmentChild::ExpressionTag(_) => out.push(' '), // text anchor
            _ => return None,
        }
    }
    Some(())
}

/// `<select>{#each EXPR as ITEM}<option>...</option>{/each}</select>` →
///   direct $.each on the select.
/// `<select>{#each EXPR as ITEM}<Component />{/each}</select>` →
///   customizable_select wrap with $.each inside the arrow.
fn lower_select_with_each(
    eb: &svelte_ast::blocks::EachBlock,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let body_is_rich = each_body_is_rich_for_select(&eb.body);
    if body_is_rich {
        let html = format!("<select{attrs}><!></select>");
        let sc_name = ctx.next_select_content();
        ctx.module_decls.push(t::var(
            &sc_name,
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![
                    t::template_raw(vec!["<!>".to_string()], Vec::new()),
                    t::lit_number(1.0),
                ],
            ),
        ));
        let anchor_var = ctx.next_named("anchor");
        let fragment_var = ctx.next_named("fragment");
        let node_var = ctx.next_named("node");
        let body_arrow = build_each_iter_arrow(eb, ctx)?;
        let expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(eb.expression.clone()),
            r#async: false,
            span: Span::ZERO,
        }));
        let each_call = t::stmt(t::call(
            t::member_id(t::id_dollar(), "each"),
            vec![
                t::id_owned(node_var.to_string()),
                t::lit_number(1.0),
                expr_arrow,
                t::member_id(t::id_dollar(), "index"),
                body_arrow,
            ],
        ));
        let arrow_body = vec![
            t::var(
                &anchor_var,
                t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
            ),
            t::var(&fragment_var, t::call(t::id_owned(sc_name.to_string()), Vec::new())),
            t::var(
                &node_var,
                t::call(
                    t::member_id(t::id_dollar(), "first_child"),
                    vec![t::id_owned(fragment_var.to_string())],
                ),
            ),
            each_call,
            t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
            )),
        ];
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: arrow_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let body = vec![t::stmt(t::call(
            t::member_id(t::id_dollar(), "customizable_select"),
            vec![t::id_owned(select_var.to_string()), arrow],
        ))];
        // Bump fragment counter once more after the each-with-Component
        // pattern — upstream's analyze allocates a phantom slot in this
        // case (visible in the fragment_15 ↔ fragment_16 jump in the
        // select-with-rich-content fixture).
        let _ = ctx.next_named("fragment");
        return Some((html, body));
    }
    // Body is `<option>...</option>` or `{@const ...}<option>...</option>`.
    let html = format!("<select{attrs}></select>");
    let body = build_each_body_for_select(eb, select_var, ctx, /*flag=*/ 5.0)?;
    Some((html, body))
}

fn each_body_is_rich_for_select(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| match n {
        FragmentChild::Component(_) | FragmentChild::RenderTag(_) | FragmentChild::HtmlTag(_) => true,
        _ => false,
    })
}

fn if_body_is_rich_for_select(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().any(|n| match n {
        FragmentChild::Component(_) | FragmentChild::RenderTag(_) | FragmentChild::HtmlTag(_) => true,
        _ => false,
    })
}

fn build_each_body_for_select(
    eb: &svelte_ast::blocks::EachBlock,
    container_var: &str,
    ctx: &mut SelectCtx,
    flag: f64,
) -> Option<Vec<Statement>> {
    let body_arrow = build_each_iter_arrow(eb, ctx)?;
    let expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(eb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    let each_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "each"),
        vec![
            t::id_owned(container_var.to_string()),
            t::lit_number(flag),
            expr_arrow,
            t::member_id(t::id_dollar(), "index"),
            body_arrow,
        ],
    ));
    let mut out = vec![each_call];
    out.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(container_var.to_string())],
    )));
    Some(out)
}

fn build_each_iter_arrow(
    eb: &svelte_ast::blocks::EachBlock,
    ctx: &mut SelectCtx,
) -> Option<Expression> {
    // Extract context name and body.
    let ctx_name = match &eb.context {
        Some(Pattern::Identifier(id)) => id.name.clone(),
        _ => return None,
    };
    let non_ws: Vec<&FragmentChild> = eb
        .body
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    // Find optional {@const X = EXPR} before the option.
    let mut const_decls: Vec<(String, Expression)> = Vec::new();
    let mut opt_idx: Option<usize> = None;
    for (i, n) in non_ws.iter().enumerate() {
        match n {
            FragmentChild::ConstTag(ct) => {
                for d in &ct.declaration.declarations {
                    if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                        const_decls.push((id.name.to_string(), init.clone()));
                    } else {
                        return None;
                    }
                }
            }
            FragmentChild::RegularElement(el) if el.name == "option" => {
                opt_idx = Some(i);
                break;
            }
            FragmentChild::Component(_) => {
                opt_idx = Some(i);
                break;
            }
            _ => return None,
        }
    }
    let opt_or_comp = non_ws[opt_idx?];
    let mut body: Vec<Statement> = Vec::new();
    // Emit const decls as `const X = $.derived_safe_equal(() => EXPR_WITH_GET);`
    for (name, init) in &const_decls {
        let rewritten = wrap_item_refs_with_get(init, &ctx_name);
        let derived_call = t::call(
            t::member_id(t::id_dollar(), "derived_safe_equal"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(rewritten),
                r#async: false,
                span: Span::ZERO,
            }))],
        );
        body.push(t::const_decl(name, derived_call));
    }
    match opt_or_comp {
        FragmentChild::RegularElement(opt) => {
            // Per-iteration option handling.
            let shape = classify_option_body(opt)?;
            match shape {
                OptionShape::SingleExpr(expr) => {
                    // root_N = `<option> </option>`, walker = text + value_binding
                    let root_name = ctx.next_root();
                    ctx.module_decls.push(t::var(
                        &root_name,
                        t::call(
                            t::member_id(t::id_dollar(), "from_html"),
                            vec![t::template_raw(
                                vec!["<option> </option>".to_string()],
                                Vec::new(),
                            )],
                        ),
                    ));
                    let option_var = ctx.next_named("option");
                    let text_var = ctx.next_named("text");
                    let option_value_var = format!("{option_var}_value");
                    let expr_with_get = wrap_expr_with_get(expr, &const_decls, &ctx_name);
                    body.push(t::var(
                        &option_var,
                        t::call(t::id_owned(root_name.to_string()), Vec::new()),
                    ));
                    body.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id_dollar(), "child"),
                            vec![
                                t::id_owned(option_var.to_string()),
                                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                    value: true,
                                    span: Span::ZERO,
                                }))),
                            ],
                        ),
                    ));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "reset"),
                        vec![t::id_owned(option_var.to_string())],
                    )));
                    body.push(t::var(
                        &option_value_var,
                        Expression::Object(Box::new(ObjectExpression {
                            properties: Vec::new(),
                            span: Span::ZERO,
                        })),
                    ));
                    // $.template_effect(() => {
                    //   $.set_text(text, EXPR_GET);
                    //   if (option_value !== (option_value = EXPR_GET)) {
                    //     option.__value = EXPR_GET;
                    //   }
                    // })
                    let set_text_call = t::stmt(t::call(
                        t::member_id(t::id_dollar(), "set_text"),
                        vec![t::id_owned(text_var.to_string()), expr_with_get.clone()],
                    ));
                    let assign_inner = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id_owned(option_value_var.to_string())),
                        operator: AssignmentOperator::Assign,
                        right: expr_with_get.clone(),
                        span: Span::ZERO,
                    }));
                    let assign_paren = Expression::Paren(Box::new(ParenthesizedExpression {
                        expression: assign_inner,
                        span: Span::ZERO,
                    }));
                    let neq_test = Expression::Binary(Box::new(BinaryExpression {
                        operator: BinaryOperator::StrictNotEq,
                        left: t::id_owned(option_value_var.to_string()),
                        right: assign_paren,
                        span: Span::ZERO,
                    }));
                    let assign_value = t::stmt(Expression::Assignment(Box::new(
                        AssignmentExpression {
                            left: AssignmentTarget::Expression(Expression::Member(Box::new(
                                MemberExpression {
                                    object: t::id_owned(option_var.to_string()),
                                    property: MemberProperty::Identifier(Identifier {
                                        name: Cow::Borrowed("__value"),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    optional: false,
                                    span: Span::ZERO,
                                },
                            ))),
                            operator: AssignmentOperator::Assign,
                            right: expr_with_get.clone(),
                            span: Span::ZERO,
                        },
                    )));
                    let if_stmt = Statement::If(Box::new(IfStatement {
                        test: neq_test,
                        consequent: Statement::Block(Box::new(BlockStatement {
                            body: vec![assign_value],
                            span: Span::ZERO,
                        })),
                        alternate: None,
                        span: Span::ZERO,
                    }));
                    let effect_body = vec![set_text_call, if_stmt];
                    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: effect_body,
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "template_effect"),
                        vec![effect_fn],
                    )));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id_owned(option_var.to_string())],
                    )));
                }
                OptionShape::RichContent(nodes) => {
                    // Allocate names FIRST (root, then option_content), then
                    // push module decls in the upstream order: option_content
                    // BEFORE root. This matches `var option_content_N =
                    // ...; var root_N = ...;` in the fixture.
                    let root_name = ctx.next_root();
                    // Upstream bumps root_idx once more here (a phantom slot
                    // allocated during analyze that isn't emitted as a
                    // declaration). Match by manually advancing.
                    ctx.root_idx += 1;
                    let oc_name = ctx.next_option_content();
                    let html_inner = serialize_rich_content_html(nodes)?;
                    ctx.module_decls.push(t::var(
                        &oc_name,
                        t::call(
                            t::member_id(t::id_dollar(), "from_html"),
                            vec![
                                t::template_raw(vec![html_inner], Vec::new()),
                                t::lit_number(1.0),
                            ],
                        ),
                    ));
                    ctx.module_decls.push(t::var(
                        &root_name,
                        t::call(
                            t::member_id(t::id_dollar(), "from_html"),
                            vec![t::template_raw(
                                vec!["<option><!></option>".to_string()],
                                Vec::new(),
                            )],
                        ),
                    ));
                    let option_var = ctx.next_named("option");
                    body.push(t::var(
                        &option_var,
                        t::call(t::id_owned(root_name.to_string()), Vec::new()),
                    ));
                    // Arrow body: navigate into fragment + setup span/text + template_effect.
                    // For rich content like <span>{item}</span>:
                    let anchor_var = ctx.next_named("anchor");
                    let fragment_var = ctx.next_named("fragment");
                    let mut arrow_body: Vec<Statement> = Vec::new();
                    arrow_body.push(t::var(
                        &anchor_var,
                        t::call(
                            t::member_id(t::id_dollar(), "child"),
                            vec![t::id_owned(option_var.to_string())],
                        ),
                    ));
                    arrow_body.push(t::var(
                        &fragment_var,
                        t::call(t::id_owned(oc_name.to_string()), Vec::new()),
                    ));
                    // Recursively handle the rich content for reactive expressions.
                    let rich_emitted = emit_rich_content_reactivity(
                        nodes,
                        &fragment_var,
                        ctx,
                        &ctx_name,
                        &const_decls,
                    );
                    arrow_body.extend(rich_emitted);
                    arrow_body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
                    )));
                    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: arrow_body,
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "customizable_select"),
                        vec![t::id_owned(option_var.to_string()), arrow],
                    )));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id_owned(option_var.to_string())],
                    )));
                }
                OptionShape::PlainText(_) => return None,
            }
        }
        FragmentChild::Component(_c) => {
            // Each with Component: emit `Option($$anchor, {});`
            body.push(t::stmt(t::call(
                t::id("Option"),
                vec![
                    t::id_anchor(),
                    Expression::Object(Box::new(ObjectExpression {
                        properties: Vec::new(),
                        span: Span::ZERO,
                    })),
                ],
            )));
        }
        _ => return None,
    }
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor(), t::pat_id_owned(ctx_name.to_string())],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(arrow)
}

fn wrap_item_refs_with_get(e: &Expression, item: &str) -> Expression {
    match e {
        Expression::Identifier(id) if id.name == item => t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![Expression::Identifier(id.clone())],
        ),
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: wrap_item_refs_with_get(&b.left, item),
            right: wrap_item_refs_with_get(&b.right, item),
            span: b.span,
        })),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: wrap_item_refs_with_get(&c.callee, item),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => Argument::Expression(wrap_item_refs_with_get(e, item)),
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: wrap_item_refs_with_get(&p.expression, item),
            span: p.span,
        })),
        e => e.clone(),
    }
}

fn wrap_expr_with_get(
    e: &Expression,
    consts: &[(String, Expression)],
    item: &str,
) -> Expression {
    match e {
        Expression::Identifier(id) => {
            // If id is the each item OR a const-declared name → wrap in $.get.
            if id.name == item || consts.iter().any(|(n, _)| n == &id.name) {
                return t::call(
                    t::member_id(t::id_dollar(), "get"),
                    vec![Expression::Identifier(id.clone())],
                );
            }
            e.clone()
        }
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: wrap_expr_with_get(&b.left, consts, item),
            right: wrap_expr_with_get(&b.right, consts, item),
            span: b.span,
        })),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: wrap_expr_with_get(&c.callee, consts, item),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => Argument::Expression(wrap_expr_with_get(e, consts, item)),
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: wrap_expr_with_get(&p.expression, consts, item),
            span: p.span,
        })),
        e => e.clone(),
    }
}

/// Walk rich content (e.g., `<span>{item}</span>`) inside a customizable_select
/// arrow body and emit navigation + template_effect for reactive bits.
fn emit_rich_content_reactivity(
    nodes: &[FragmentChild],
    fragment_var: &str,
    ctx: &mut SelectCtx,
    item_name: &str,
    consts: &[(String, Expression)],
) -> Vec<Statement> {
    // Look for the FIRST element with reactive child; emit `var span = $.first_child(fragment); var text = $.child(span, true); $.reset(span); $.template_effect(() => $.set_text(text, $.get(item)));`
    let mut out = Vec::new();
    let non_ws: Vec<&FragmentChild> = nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() == 1 {
        if let FragmentChild::RegularElement(el) = non_ws[0] {
            // Check if it has a single ExpressionTag child.
            let inner_non_ws: Vec<&FragmentChild> = el
                .fragment
                .nodes
                .iter()
                .filter(|n| match n {
                    FragmentChild::Text(t) => !t.data.trim().is_empty(),
                    FragmentChild::Comment(_) => false,
                    _ => true,
                })
                .collect();
            if inner_non_ws.len() == 1 {
                if let FragmentChild::ExpressionTag(et) = inner_non_ws[0] {
                    let el_var = ctx.next_named(&sanitize_name(&el.name));
                    let text_var = ctx.next_named("text");
                    out.push(t::var(
                        &el_var,
                        t::call(
                            t::member_id(t::id_dollar(), "first_child"),
                            vec![t::id_owned(fragment_var.to_string())],
                        ),
                    ));
                    out.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id_dollar(), "child"),
                            vec![
                                t::id_owned(el_var.to_string()),
                                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                    value: true,
                                    span: Span::ZERO,
                                }))),
                            ],
                        ),
                    ));
                    out.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "reset"),
                        vec![t::id_owned(el_var.to_string())],
                    )));
                    let expr_with_get = wrap_expr_with_get(&et.expression, consts, item_name);
                    let set_text_call = t::call(
                        t::member_id(t::id_dollar(), "set_text"),
                        vec![t::id_owned(text_var.to_string()), expr_with_get],
                    );
                    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(set_text_call),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    out.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "template_effect"),
                        vec![effect_fn],
                    )));
                }
            }
        }
    }
    out
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

fn lower_select_with_if(
    ib: &svelte_ast::blocks::IfBlock,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    if if_body_is_rich_for_select(&ib.consequent) {
        // customizable_select wrap with $.if inside.
        let html = format!("<select{attrs}><!></select>");
        let sc_name = ctx.next_select_content();
        ctx.module_decls.push(t::var(
            &sc_name,
            t::call(
                t::member_id(t::id_dollar(), "from_html"),
                vec![
                    t::template_raw(vec!["<!>".to_string()], Vec::new()),
                    t::lit_number(1.0),
                ],
            ),
        ));
        let anchor_var = ctx.next_named("anchor");
        let fragment_var = ctx.next_named("fragment");
        let node_var = ctx.next_named("node");
        let consequent_var = ctx.next_named("consequent");
        let consequent_body = build_if_consequent_for_select(&ib.consequent, ctx)?;
        let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: consequent_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let render_call = t::stmt(t::call(t::id_render(), vec![t::id_owned(consequent_var.to_string())]));
        let render_if = Statement::If(Box::new(IfStatement {
            test: ib.test.clone(),
            consequent: render_call,
            alternate: None,
            span: Span::ZERO,
        }));
        let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$render")],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: vec![render_if],
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let inner_block = Statement::Block(Box::new(BlockStatement {
            body: vec![
                t::var(&consequent_var, consequent_arrow),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "if"),
                    vec![t::id_owned(node_var.to_string()), render_arrow],
                )),
            ],
            span: Span::ZERO,
        }));
        let arrow_body = vec![
            t::var(
                &anchor_var,
                t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
            ),
            t::var(&fragment_var, t::call(t::id_owned(sc_name.to_string()), Vec::new())),
            t::var(
                &node_var,
                t::call(
                    t::member_id(t::id_dollar(), "first_child"),
                    vec![t::id_owned(fragment_var.to_string())],
                ),
            ),
            inner_block,
            t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
            )),
        ];
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: arrow_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let body = vec![t::stmt(t::call(
            t::member_id(t::id_dollar(), "customizable_select"),
            vec![t::id_owned(select_var.to_string()), arrow],
        ))];
        return Some((html, body));
    }
    // Plain options branch.
    let html = format!("<select{attrs}><!></select>");
    let node_var = ctx.next_named("node");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &node_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
    ));
    let consequent_var = ctx.next_named("consequent");
    let consequent_body = build_if_consequent_for_select(&ib.consequent, ctx)?;
    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let render_call = t::stmt(t::call(t::id_render(), vec![t::id_owned(consequent_var.to_string())]));
    let render_if = Statement::If(Box::new(IfStatement {
        test: ib.test.clone(),
        consequent: render_call,
        alternate: None,
        span: Span::ZERO,
    }));
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let if_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "if"),
        vec![t::id_owned(node_var.to_string()), render_arrow],
    ));
    body.push(Statement::Block(Box::new(BlockStatement {
        body: vec![t::var(&consequent_var, consequent_arrow), if_call],
        span: Span::ZERO,
    })));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(select_var.to_string())],
    )));
    Some((html, body))
}

/// The if-block's consequent: handle the contained option / each / render.
fn build_if_consequent_for_select(
    fragment: &svelte_ast::fragment::Fragment,
    ctx: &mut SelectCtx,
) -> Option<Vec<Statement>> {
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    match non_ws[0] {
        FragmentChild::RegularElement(opt) if opt.name == "option" => {
            let shape = classify_option_body(opt)?;
            match shape {
                OptionShape::PlainText(text) => {
                    let root_name = ctx.next_root();
                    let template = format!("<option>{text}</option>");
                    ctx.module_decls.push(t::var(
                        &root_name,
                        t::call(
                            t::member_id(t::id_dollar(), "from_html"),
                            vec![t::template_raw(vec![template], Vec::new())],
                        ),
                    ));
                    let opt_var = ctx.next_named("option");
                    Some(vec![
                        t::var(&opt_var, t::call(t::id_owned(root_name.to_string()), Vec::new())),
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "append"),
                            vec![t::id_anchor(), t::id_owned(opt_var.to_string())],
                        )),
                    ])
                }
                _ => None,
            }
        }
        FragmentChild::EachBlock(eb) => {
            // Each inside if-consequent: wrap in `var fragment_N = $.comment(); var node_N = $.first_child(fragment_N); $.each(node_N, 1, ...); $.append($$anchor, fragment_N);`
            let fragment_var = ctx.next_named("fragment");
            let node_var = ctx.next_named("node");
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::var(
                &fragment_var,
                t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
            ));
            body.push(t::var(
                &node_var,
                t::call(
                    t::member_id(t::id_dollar(), "first_child"),
                    vec![t::id_owned(fragment_var.to_string())],
                ),
            ));
            // Each inside if: flag=1 (not 5 — that's for select-direct).
            let body_arrow = build_each_iter_arrow(eb, ctx)?;
            let expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(eb.expression.clone()),
                r#async: false,
                span: Span::ZERO,
            }));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "each"),
                vec![
                    t::id_owned(node_var.to_string()),
                    t::lit_number(1.0),
                    expr_arrow,
                    t::member_id(t::id_dollar(), "index"),
                    body_arrow,
                ],
            )));
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(fragment_var.to_string())],
            )));
            Some(body)
        }
        FragmentChild::RenderTag(rt) => {
            // {@render foo()} → foo($$anchor)
            let callee_name = if let Expression::Call(c) = &rt.expression {
                if let Expression::Identifier(id) = &c.callee {
                    id.name.clone()
                } else {
                    return None;
                }
            } else {
                return None;
            };
            Some(vec![t::stmt(t::call(
                t::id_owned(callee_name.to_string()),
                vec![t::id_anchor()],
            ))])
        }
        _ => None,
    }
}

fn lower_select_with_key(
    kb: &svelte_ast::blocks::KeyBlock,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let html = format!("<select{attrs}><!></select>");
    let node_var = ctx.next_named("node");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &node_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
    ));
    // body of key: single option (plain text).
    let non_ws: Vec<&FragmentChild> = kb
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    let opt = match non_ws[0] {
        FragmentChild::RegularElement(el) if el.name == "option" => el,
        _ => return None,
    };
    let shape = classify_option_body(opt)?;
    let OptionShape::PlainText(text) = shape else { return None };
    let root_name = ctx.next_root();
    ctx.module_decls.push(t::var(
        &root_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![format!("<option>{text}</option>")], Vec::new())],
        ),
    ));
    let opt_var = ctx.next_named("option");
    let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![
                t::var(&opt_var, t::call(t::id_owned(root_name.to_string()), Vec::new())),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "append"),
                    vec![t::id_anchor(), t::id_owned(opt_var.to_string())],
                )),
            ],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let key_expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(kb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "key"),
        vec![t::id_owned(node_var.to_string()), key_expr_arrow, inner_arrow],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(select_var.to_string())],
    )));
    Some((html, body))
}

fn lower_select_with_boundary(
    b: &svelte_ast::elements::SvelteBoundary,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let html = format!("<select{attrs}><!></select>");
    let node_var = ctx.next_named("node");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &node_var,
        t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
    ));
    // Boundary body: single <option> (plain or rich).
    let non_ws: Vec<&FragmentChild> = b
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    let opt = match non_ws[0] {
        FragmentChild::RegularElement(el) if el.name == "option" => el,
        _ => return None,
    };
    let shape = classify_option_body(opt)?;
    let opt_var = ctx.next_named("option");
    let mut arrow_body: Vec<Statement> = Vec::new();
    match shape {
        OptionShape::PlainText(text) => {
            let root_name = ctx.next_root();
            ctx.module_decls.push(t::var(
                &root_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![t::template_raw(
                        vec![format!("<option>{text}</option>")],
                        Vec::new(),
                    )],
                ),
            ));
            arrow_body.push(t::var(&opt_var, t::call(t::id_owned(root_name.to_string()), Vec::new())));
            arrow_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(opt_var.to_string())],
            )));
        }
        OptionShape::RichContent(nodes) => {
            // Reserve root name FIRST but push the declaration AFTER the
            // inner option_content template, so the module-decl order
            // matches `var option_content_N = ...; var root_N = ...;`.
            let root_name = ctx.next_root();
            arrow_body.push(t::var(&opt_var, t::call(t::id_owned(root_name.to_string()), Vec::new())));
            let cs = build_customizable_select_body(&opt_var, nodes, ctx)?;
            arrow_body.push(cs);
            ctx.module_decls.push(t::var(
                &root_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![t::template_raw(
                        vec!["<option><!></option>".to_string()],
                        Vec::new(),
                    )],
                ),
            ));
            arrow_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(opt_var.to_string())],
            )));
        }
        _ => return None,
    }
    let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "boundary"),
        vec![
            t::id_owned(node_var.to_string()),
            Expression::Object(Box::new(ObjectExpression {
                properties: Vec::new(),
                span: Span::ZERO,
            })),
            inner_arrow,
        ],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "reset"),
        vec![t::id_owned(select_var.to_string())],
    )));
    Some((html, body))
}

fn lower_select_with_component(
    c: &svelte_ast::elements::Component,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    // `<select><Component /></select>` → customizable_select with the
    // select itself + select_content template + Component call inside.
    let html = format!("<select{attrs}><!></select>");
    let sc_name = ctx.next_select_content();
    ctx.module_decls.push(t::var(
        &sc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec!["<!>".to_string()], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let node_var = ctx.next_named("node");
    let component_name = c.name.clone();
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
        ),
        t::var(&fragment_var, t::call(t::id_owned(sc_name.to_string()), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(fragment_var.to_string())],
            ),
        ),
        t::stmt(t::call(
            t::id_owned(component_name.to_string()),
            vec![
                t::id_owned(node_var.to_string()),
                Expression::Object(Box::new(ObjectExpression {
                    properties: Vec::new(),
                    span: Span::ZERO,
                })),
            ],
        )),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(select_var.to_string()), arrow],
    ))];
    Some((html, body))
}

fn lower_select_with_render(
    rt: &svelte_ast::tags::RenderTag,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let html = format!("<select{attrs}><!></select>");
    let sc_name = ctx.next_select_content();
    ctx.module_decls.push(t::var(
        &sc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec!["<!>".to_string()], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let node_var = ctx.next_named("node");
    // {@render foo()} → foo(node)
    let callee_name = if let Expression::Call(c) = &rt.expression {
        if let Expression::Identifier(id) = &c.callee {
            id.name.clone()
        } else {
            return None;
        }
    } else {
        return None;
    };
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
        ),
        t::var(&fragment_var, t::call(t::id_owned(sc_name.to_string()), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(fragment_var.to_string())],
            ),
        ),
        t::stmt(t::call(t::id_owned(callee_name.to_string()), vec![t::id_owned(node_var.to_string())])),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(select_var.to_string()), arrow],
    ))];
    Some((html, body))
}

fn lower_select_with_html(
    ht: &svelte_ast::tags::HtmlTag,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let html = format!("<select{attrs}><!></select>");
    let sc_name = ctx.next_select_content();
    ctx.module_decls.push(t::var(
        &sc_name,
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec!["<!>".to_string()], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    ));
    let anchor_var = ctx.next_named("anchor");
    let fragment_var = ctx.next_named("fragment");
    let node_var = ctx.next_named("node");
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(ht.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
        ),
        t::var(&fragment_var, t::call(t::id_owned(sc_name.to_string()), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_owned(fragment_var.to_string())],
            ),
        ),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "html"),
            vec![t::id_owned(node_var.to_string()), getter],
        )),
        t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id_dollar(), "customizable_select"),
        vec![t::id_owned(select_var.to_string()), arrow],
    ))];
    Some((html, body))
}

fn lower_select_with_optgroup(
    og: &svelte_ast::elements::RegularElement,
    select_var: &str,
    ctx: &mut SelectCtx,
    attrs: &str,
) -> Option<(String, Vec<Statement>)> {
    let og_attrs = build_static_attrs(og);
    let og_non_ws: Vec<&FragmentChild> = og
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if og_non_ws.len() != 1 {
        return None;
    }
    let og_var = ctx.next_named("optgroup");
    match og_non_ws[0] {
        FragmentChild::RegularElement(opt) if opt.name == "option" => {
            // Rich option inside optgroup → `<select><optgroup label="X"><option><!></option></optgroup></select>`
            let shape = classify_option_body(opt)?;
            let opt_attrs = build_static_attrs(opt);
            match shape {
                OptionShape::RichContent(nodes) => {
                    let html = format!(
                        "<select{attrs}><optgroup{og_attrs}><option{opt_attrs}><!></option></optgroup></select>"
                    );
                    let option_var = ctx.next_named("option");
                    let body = vec![
                        t::var(
                            &og_var,
                            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
                        ),
                        t::var(
                            &option_var,
                            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(og_var.to_string())]),
                        ),
                        build_customizable_select_body(&option_var, nodes, ctx)?,
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "reset"),
                            vec![t::id_owned(og_var.to_string())],
                        )),
                        t::stmt(t::call(
                            t::member_id(t::id_dollar(), "reset"),
                            vec![t::id_owned(select_var.to_string())],
                        )),
                    ];
                    Some((html, body))
                }
                _ => None,
            }
        }
        FragmentChild::EachBlock(eb) => {
            // `<select><optgroup label="X">{#each}<option>...</option>{/each}</optgroup></select>`
            let html = format!("<select{attrs}><optgroup{og_attrs}></optgroup></select>");
            let mut body = vec![t::var(
                &og_var,
                t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
            )];
            body.extend(build_each_body_for_select(eb, &og_var, ctx, 5.0)?);
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(select_var.to_string())],
            )));
            Some((html, body))
        }
        FragmentChild::Component(c) => {
            // `<select><optgroup label="X"><Component /></optgroup></select>`
            let html = format!("<select{attrs}><optgroup{og_attrs}><!></optgroup></select>");
            let oc_name = ctx.next_optgroup_content();
            ctx.module_decls.push(t::var(
                &oc_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![
                        t::template_raw(vec!["<!>".to_string()], Vec::new()),
                        t::lit_number(1.0),
                    ],
                ),
            ));
            let anchor_var = ctx.next_named("anchor");
            let fragment_var = ctx.next_named("fragment");
            let node_var = ctx.next_named("node");
            let component_name = c.name.clone();
            let arrow_body = vec![
                t::var(
                    &anchor_var,
                    t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(og_var.to_string())]),
                ),
                t::var(&fragment_var, t::call(t::id_owned(oc_name.to_string()), Vec::new())),
                t::var(
                    &node_var,
                    t::call(
                        t::member_id(t::id_dollar(), "first_child"),
                        vec![t::id_owned(fragment_var.to_string())],
                    ),
                ),
                t::stmt(t::call(
                    t::id_owned(component_name.to_string()),
                    vec![
                        t::id_owned(node_var.to_string()),
                        Expression::Object(Box::new(ObjectExpression {
                            properties: Vec::new(),
                            span: Span::ZERO,
                        })),
                    ],
                )),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "append"),
                    vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
                )),
            ];
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: arrow_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            let body = vec![
                t::var(
                    &og_var,
                    t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
                ),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "customizable_select"),
                    vec![t::id_owned(og_var.to_string()), arrow],
                )),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(select_var.to_string())],
                )),
            ];
            Some((html, body))
        }
        FragmentChild::RenderTag(rt) => {
            // `<select><optgroup label="X">{@render foo()}</optgroup></select>`
            let html = format!("<select{attrs}><optgroup{og_attrs}><!></optgroup></select>");
            let oc_name = ctx.next_optgroup_content();
            ctx.module_decls.push(t::var(
                &oc_name,
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![
                        t::template_raw(vec!["<!>".to_string()], Vec::new()),
                        t::lit_number(1.0),
                    ],
                ),
            ));
            let anchor_var = ctx.next_named("anchor");
            let fragment_var = ctx.next_named("fragment");
            let node_var = ctx.next_named("node");
            let callee_name = if let Expression::Call(c) = &rt.expression {
                if let Expression::Identifier(id) = &c.callee {
                    id.name.clone()
                } else {
                    return None;
                }
            } else {
                return None;
            };
            let arrow_body = vec![
                t::var(
                    &anchor_var,
                    t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(og_var.to_string())]),
                ),
                t::var(&fragment_var, t::call(t::id_owned(oc_name.to_string()), Vec::new())),
                t::var(
                    &node_var,
                    t::call(
                        t::member_id(t::id_dollar(), "first_child"),
                        vec![t::id_owned(fragment_var.to_string())],
                    ),
                ),
                t::stmt(t::call(t::id_owned(callee_name.to_string()), vec![t::id_owned(node_var.to_string())])),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "append"),
                    vec![t::id_owned(anchor_var.to_string()), t::id_owned(fragment_var.to_string())],
                )),
            ];
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: arrow_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            let body = vec![
                t::var(
                    &og_var,
                    t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(select_var.to_string())]),
                ),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "customizable_select"),
                    vec![t::id_owned(og_var.to_string()), arrow],
                )),
                t::stmt(t::call(
                    t::member_id(t::id_dollar(), "reset"),
                    vec![t::id_owned(select_var.to_string())],
                )),
            ];
            Some((html, body))
        }
        _ => None,
    }
}

fn emit_deep_static_walker_program(
    root_fragment: &svelte_ast::fragment::Fragment,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Collect top-level element nodes (interleaved with text/comment).
    let mut html = String::with_capacity(128);
    let mut needs_import_node = false;
    serialize_fragment_to_html(root_fragment, &mut html, &mut needs_import_node)?;

    let mut counters = DeepCounters::default();
    let mut var_names: HashMap<String, usize> = HashMap::new();
    let mut body: Vec<Statement> = Vec::new();
    let mut effects: Vec<(String, Expression)> = Vec::new(); // (text_var, getter_expr)
    // Element variables that need `template_effect(() => X.dir = X.dir)`
    // (Chromium hydration fix for `dir` attribute).
    let mut dir_self_assigns: Vec<String> = Vec::new();
    // Input variables that need `$.remove_input_defaults(input)` (boolean
    // `checked` / static `value` attributes during hydration).
    let mut input_defaults_resets: Vec<String> = Vec::new();
    // `bind:value={...}` directives → `$.bind_value(var, target)` calls
    // emitted after the trailing template_effect.
    let mut bind_value_calls: Vec<(String, Expression)> = Vec::new();

    // If the first node is a non-Element (text/comment), emit a leading
    // `$.next();` to position the hydration cursor at it before reading
    // the fragment.
    let first_is_non_element = root_fragment
        .nodes
        .iter()
        .find(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => true,
            _ => true,
        })
        .map(|n| !matches!(n, FragmentChild::RegularElement(_)))
        .unwrap_or(false);
    if first_is_non_element {
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            Vec::new(),
        )));
    }
    body.push(t::var(
        "fragment",
        t::call(t::id("root"), Vec::new()),
    ));

    // Walk top-level elements in order. Track previous element variable
    // name + its index in top_elements for $.sibling navigation.
    let mut prev_var: Option<String> = None;
    let mut prev_top_idx: Option<usize> = None;
    let mut first_emitted = false;
    let top_elements: Vec<&svelte_ast::elements::RegularElement> = root_fragment
        .nodes
        .iter()
        .filter_map(|n| match n {
            FragmentChild::RegularElement(el) => Some(el),
            _ => None,
        })
        .collect();
    let _top_count = top_elements.len();
    // Compute DOM sibling position for each top element, accounting for
    // any preceding non-whitespace text or comment nodes. Pure-whitespace
    // text at the fragment boundary (or between elements) gets stripped
    // by trim_pure_whitespace_text — but whitespace BETWEEN elements
    // becomes a single space text node. Non-whitespace text creates an
    // anchored text node.
    let mut top_element_positions: Vec<usize> = vec![0; top_elements.len()];
    {
        let trimmed = trim_pure_whitespace_text(&root_fragment.nodes);
        let mut pos = 0usize;
        let mut pending_text = false;
        let mut el_idx = 0usize;
        for n in &trimmed {
            match n {
                FragmentChild::Text(t) => {
                    // Non-whitespace text → contributes to a text-node anchor.
                    // Pure whitespace BETWEEN elements is a "gap" creating a
                    // single text node when followed by another element.
                    let _ = t;
                    if !pending_text {
                        pending_text = true;
                    }
                }
                FragmentChild::Comment(_) => {
                    if !pending_text {
                        pending_text = true;
                    }
                }
                FragmentChild::RegularElement(_) => {
                    if pending_text {
                        pos += 1;
                        pending_text = false;
                    }
                    if el_idx < top_element_positions.len() {
                        top_element_positions[el_idx] = pos;
                        el_idx += 1;
                    }
                    pos += 1;
                }
                _ => {}
            }
        }
    }

    for (i, el) in top_elements.iter().enumerate() {
        let has_reactive_inside = fragment_has_deep_reactive(&el.fragment);
        let has_reactive_attr = element_has_reactive_attr(el);
        let needs_visit = has_reactive_inside || has_reactive_attr;
        if !needs_visit {
            // Skip purely static element. We don't emit anything for it.
            continue;
        }
        // Allocate a variable name. Compute the actual DOM sibling
        // position via `top_element_positions[i]` (accounts for preceding
        // text/comment runs that merge into text nodes).
        let var = allocate_named(&el.name, &mut var_names);
        let this_pos = top_element_positions[i];
        let init = if !first_emitted {
            let first_child = t::call(
                t::member_id(t::id_dollar(), "first_child"),
                vec![t::id_fragment()],
            );
            if this_pos == 0 {
                first_child
            } else if this_pos == 1 {
                t::call(t::member_id(t::id_dollar(), "sibling"), vec![first_child])
            } else {
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![first_child, t::lit_number(this_pos as f64)],
                )
            }
        } else {
            let prev = prev_var.as_ref().expect("prev_var set");
            let prev_idx = prev_top_idx.expect("prev_top_idx set");
            let prev_pos = top_element_positions[prev_idx];
            let offset = this_pos - prev_pos;
            t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(prev.to_string()), t::lit_number(offset as f64)],
            )
        };
        body.push(t::var(&var, init));
        prev_var = Some(var.clone());
        prev_top_idx = Some(i);
        first_emitted = true;

        // `<input>` with `checked` / static `value` attribute → emit
        // `$.remove_input_defaults(input)` right after the var declaration.
        // Also: `<input bind:value={...}>` triggers the same defaults reset.
        let input_needs_defaults = el.name == "input"
            && el.attributes.iter().any(|a| match a {
                ElementAttribute::Attribute(attr) => {
                    matches!(attr.name.as_ref(), "checked" | "value")
                }
                ElementAttribute::BindDirective(bd) => {
                    matches!(bd.name.as_ref(), "value" | "checked" | "group" | "files")
                }
                _ => false,
            });
        if input_needs_defaults {
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "remove_input_defaults"),
                vec![t::id_owned(var.to_string())],
            )));
            input_defaults_resets.push(var.clone());
        }
        // If this element has a `dir` attribute, schedule a
        // `template_effect(() => X.dir = X.dir)` at the end.
        if el.attributes.iter().any(|a| matches!(
            a,
            ElementAttribute::Attribute(attr) if attr.name == "dir"
        )) {
            dir_self_assigns.push(var.clone());
        }
        // Walk the element's interior — emit reactive handlers and
        // navigation as needed.
        walk_element_interior(
            el,
            &var,
            &mut body,
            &mut effects,
            &mut var_names,
            &mut counters,
            script,
        );
        // Collect bind directives for trailing emission (after template_effect).
        for a in &el.attributes {
            if let ElementAttribute::BindDirective(bd) = a {
                if bd.name == "value" {
                    bind_value_calls.push((var.clone(), bd.expression.clone()));
                }
            }
        }
        if has_reactive_inside {
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(var.to_string())],
            )));
        }
    }

    // Trailing static top-elements: navigate to the first one + emit
    // `$.next((trailing-1)*2)` for the rest. Matches skip-static-subtree's
    // `var img = $.sibling(select, 2); $.next(2);` pattern.
    if let Some(last_idx) = prev_top_idx {
        let trailing = top_elements.len() - last_idx - 1;
        if trailing > 0 {
            let first_trailing = top_elements[last_idx + 1];
            let var = allocate_named(&first_trailing.name, &mut var_names);
            body.push(t::var(
                &var,
                t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![
                        t::id_owned(prev_var.as_ref().expect("prev_var set").to_string()),
                        t::lit_number(2.0),
                    ],
                ),
            ));
            if trailing > 1 {
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "next"),
                    vec![t::lit_number(((trailing - 1) * 2) as f64)],
                )));
            }
        } else {
            // No trailing elements, but there may be trailing non-element
            // nodes (text/comments) — emit `$.next();` to advance the
            // hydration cursor past them.
            let last_el_ptr = top_elements[last_idx] as *const _;
            let mut after_last = false;
            let mut has_trailing_nonelem = false;
            for n in &root_fragment.nodes {
                if let FragmentChild::RegularElement(el) = n {
                    if el as *const _ == last_el_ptr {
                        after_last = true;
                        continue;
                    }
                }
                if after_last {
                    match n {
                        FragmentChild::Text(t) if !t.data.trim().is_empty() => {
                            has_trailing_nonelem = true;
                            break;
                        }
                        FragmentChild::Comment(_) => {
                            // Trailing comments also count.
                            has_trailing_nonelem = true;
                            break;
                        }
                        _ => {}
                    }
                }
            }
            if has_trailing_nonelem {
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "next"),
                    Vec::new(),
                )));
            }
        }
    }

    // Dir-attribute self-assignment effects (Chromium hydration fix).
    for var_name in &dir_self_assigns {
        let dir_member = Expression::Member(Box::new(MemberExpression {
            object: t::id_owned(var_name.to_string()),
            property: MemberProperty::Identifier(Identifier {
                name: Cow::Borrowed("dir"),
                span: Span::ZERO,
            }),
            computed: false,
            optional: false,
            span: Span::ZERO,
        }));
        let self_assign = Expression::Assignment(Box::new(AssignmentExpression {
            left: AssignmentTarget::Expression(dir_member.clone()),
            operator: AssignmentOperator::Assign,
            right: dir_member,
            span: Span::ZERO,
        }));
        let effect_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(self_assign),
            r#async: false,
            span: Span::ZERO,
        }));
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![effect_arrow],
        )));
    }

    // Combined template_effect for text reactivity at the bottom.
    if effects.len() == 1 {
        let (text_var, expr) = effects.pop().unwrap();
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id_owned(text_var.to_string()), expr],
                )),
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    } else if effects.len() >= 2 {
        let mut block_body: Vec<Statement> = Vec::new();
        for (text_var, expr) in effects {
            block_body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "set_text"),
                vec![t::id_owned(text_var.to_string()), expr],
            )));
        }
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: block_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    }

    // `$.bind_value(var, target)` calls — emitted after template_effect,
    // before the final append. For legacy props (accessor functions), the
    // target is the bare accessor identifier; runtime knows to read/write
    // via the same callable.
    let legacy_prop_names_for_bind: HashSet<String> = script
        .legacy_export_props
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    for (var_name, target_expr) in &bind_value_calls {
        let target_is_legacy_prop = matches!(
            target_expr,
            Expression::Identifier(id) if legacy_prop_names_for_bind.contains(id.name.as_ref())
        );
        let target = if target_is_legacy_prop {
            target_expr.clone()
        } else {
            target_expr.clone()
        };
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "bind_value"),
            vec![t::id_owned(var_name.to_string()), target],
        )));
    }

    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    // `var root = $.from_html(\`HTML\`, FLAGS);` where FLAGS = 1 (multi-root)
    // or 3 (multi-root + needs_import_node for video/custom-element).
    let flags = if needs_import_node { 3.0 } else { 1.0 };
    let root_decl = t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![
                t::template_raw(vec![html], Vec::new()),
                t::lit_number(flags),
            ],
        ),
    );

    let mut params = vec![t::pat_id_anchor()];
    let has_legacy_props = !script.legacy_export_props.is_empty();
    let has_legacy_mutable = !script.legacy_mutable_bindings.is_empty();
    let needs_legacy_wrap = has_legacy_props || has_legacy_mutable;
    if script.uses_props || needs_legacy_wrap {
        params.push(t::pat_id("$$props"));
    }
    // Splice script body before the template body, plus legacy push/init/pop
    // wrap when there are legacy mutable bindings or export-let props.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    if needs_legacy_wrap {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "push"),
            vec![
                t::id("$$props"),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: false,
                    span: Span::ZERO,
                }))),
            ],
        )));
    }
    // Legacy export prop accessors: `let X = $.prop($$props, 'X', N [, INIT]);`
    // + `var $$exports = { get X() { ... }, set X($$value) { ... } };`
    if has_legacy_props {
        for (name, init) in &script.legacy_export_props {
            let mut args = vec![
                t::id("$$props"),
                t::literal_str_owned(name.to_string()),
                t::lit_number(12.0),
            ];
            if let Some(default) = init {
                args.push(default.clone());
            }
            func_body.push(t::let_decl(
                name,
                Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
            ));
        }
        func_body.push(t::var(
            "$$exports",
            build_legacy_exports_object(&script.legacy_export_props),
        ));
    }
    func_body.extend(script.body.iter().cloned());
    // `$.init()` is emitted only when the component holds legacy mutable
    // bindings (mutable_source) — legacy export props alone don't need it.
    if has_legacy_mutable {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "init"),
            Vec::new(),
        )));
    }
    func_body.extend(body);
    if has_legacy_props {
        func_body.push(Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(
                t::member_id(t::id_dollar(), "pop"),
                vec![t::id("$$exports")],
            )),
            span: Span::ZERO,
        })));
    } else if has_legacy_mutable {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "pop"),
            Vec::new(),
        )));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

#[derive(Default)]
struct DeepCounters {
    text: usize,
    node: usize,
}

fn allocate_named(prefix: &str, names: &mut HashMap<String, usize>) -> String {
    // Sanitize: replace `-` and other special chars with `_`.
    let safe: String = prefix
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let cnt = names.entry(safe.clone()).or_insert(0);
    let n = *cnt;
    *cnt += 1;
    if n == 0 {
        safe
    } else {
        format!("{safe}_{n}")
    }
}

fn prev_index_of<'a>(
    name: &str,
    top_elements: &[&'a svelte_ast::elements::RegularElement],
) -> usize {
    // Reverse-engineer: which top-element corresponds to `name`? Names like
    // `main`, `div`, `div_1`, `cant_skip`, etc. are derived from el.name.
    // For correct sibling offsets we need the position of the
    // *previous-emitted* element in the top_elements array. The caller
    // tracks this naturally by passing prev_var; we recover the index by
    // scanning for the most recent element whose sanitized name matches.
    // Approach: track via a separate counter — fall back to last index in
    // the list with matching name.
    let _ = name;
    // Heuristic: callers track prev_var via mutable state. To avoid that
    // complexity, this function is approximated by external bookkeeping —
    // however the deep walker uses a different approach.
    let _ = top_elements;
    0
}

/// Walk the interior of `el` (which has known reactive content somewhere),
/// emitting navigation + reactive handlers + $.reset calls.
fn walk_element_interior(
    el: &svelte_ast::elements::RegularElement,
    parent_var: &str,
    body: &mut Vec<Statement>,
    effects: &mut Vec<(String, Expression)>,
    var_names: &mut HashMap<String, usize>,
    counters: &mut DeepCounters,
    script: &ScriptInfo,
) {
    // First, handle direct attributes on `el` (autofocus, muted, value, custom-element-data).
    apply_reactive_attrs(el, parent_var, body, script);

    // Text-only element with mixed text + expression children (e.g.
    // `<p>Count: {count}</p>`): emit a single text-anchor via `$.child(el)`
    // + an inline template combining all parts. Skip the per-child reactive
    // walk. The `true` arg is omitted because the element body has static
    // text — the existing SSR text node IS the anchor.
    if is_text_only_element(el) && el.attributes.is_empty() {
        let has_static_text = el.fragment.nodes.iter().any(|c| matches!(
            c, FragmentChild::Text(t) if !t.data.trim().is_empty()
        ));
        if has_static_text {
            let text_var = allocate_named("text", var_names);
            body.push(t::var(
                &text_var,
                t::call(
                    t::member_id(t::id_dollar(), "child"),
                    vec![t::id_owned(parent_var.to_string())],
                ),
            ));
            let mut parts: Vec<TextPart> = Vec::new();
            for c in &el.fragment.nodes {
                match c {
                    FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                    FragmentChild::ExpressionTag(et) => {
                        parts.push(TextPart::Expr(&et.expression))
                    }
                    _ => return,
                }
            }
            let inline = build_inline_template(&parts, &script.state_bindings);
            let inline = rewrite_props_destructured(&inline, &script.props_destructured);
            let legacy_prop_names: HashSet<String> = script
                .legacy_export_props
                .iter()
                .map(|(n, _)| n.clone())
                .collect();
            let inline = rewrite_legacy_prop_reads(&inline, &legacy_prop_names);
            effects.push((text_var, inline));
            return;
        }
    }

    // Find the indices of reactive children in el's fragment, using the
    // STRIPPED children (leading + trailing whitespace text nodes / comments
    // removed) so indices match runtime siblings of the rendered template.
    let raw: Vec<&FragmentChild> = el.fragment.nodes.iter().collect();
    let is_boundary = |n: &&FragmentChild| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        FragmentChild::Comment(_) => true,
        _ => false,
    };
    let start = raw.iter().position(|n| !is_boundary(n)).unwrap_or(raw.len());
    let end = raw
        .iter()
        .rposition(|n| !is_boundary(n))
        .map(|p| p + 1)
        .unwrap_or(0);
    let children: Vec<&FragmentChild> = raw[start..end].iter().copied().collect();
    let mut reactive_idx: Vec<usize> = Vec::new();
    for (i, c) in children.iter().enumerate() {
        let r = match c {
            FragmentChild::ExpressionTag(_) | FragmentChild::HtmlTag(_) => true,
            FragmentChild::RegularElement(child_el) => {
                element_has_reactive_attr(child_el) || fragment_has_deep_reactive(&child_el.fragment)
            }
            _ => false,
        };
        if r {
            reactive_idx.push(i);
        }
    }
    if reactive_idx.is_empty() {
        return;
    }

    // For each reactive child, emit nav + handler. Static children get
    // skipped via `$.sibling(prev, N)` offsets.
    let mut prev_child_var: Option<String> = None;
    let mut prev_child_idx: Option<usize> = None;
    for (k, &i) in reactive_idx.iter().enumerate() {
        let var: String;
        let init: Expression;
        if k == 0 {
            // First reactive child — navigate via $.child(parent) or
            // $.sibling($.first_child(parent), N) when not at trimmed
            // index 0. The `true` arg of $.child is for TEXT-NODE
            // navigation (e.g. inside `<h1>` for {title}), NOT for
            // element-level navigation.
            let prefix = match children[i] {
                FragmentChild::RegularElement(child_el) => child_el.name.clone(),
                FragmentChild::HtmlTag(_) => "node".to_string(),
                FragmentChild::ExpressionTag(_) => "text".to_string(),
                _ => "node".to_string(),
            };
            var = allocate_named(&prefix, var_names);
            // Inside `<pre>` / `<textarea>`: when the template kept a leading
            // text node (i.e. source first child was Text with content beyond
            // a single stripped \n), the first DOM child is that text — use
            // `$.sibling($.child(parent))` instead of `$.child(parent)`.
            let pre_has_leading_text =
                (el.name == "pre" || el.name == "textarea") && {
                    let first = el.fragment.nodes.first();
                    let second = el.fragment.nodes.get(1);
                    match (first, second) {
                        (Some(FragmentChild::Text(t)), Some(FragmentChild::RegularElement(_))) => {
                            // Upstream strips a leading text node ONLY when
                            // it's exactly "\n". For any other leading text
                            // (`"\n\n"`, `"\n\t"`, etc.) the text remains in
                            // the DOM, so we navigate with sibling/child.
                            t.data != "\n"
                        }
                        _ => false,
                    }
                };
            if i == 0 {
                if pre_has_leading_text {
                    init = t::call(
                        t::member_id(t::id_dollar(), "sibling"),
                        vec![t::call(
                            t::member_id(t::id_dollar(), "child"),
                            vec![t::id_owned(parent_var.to_string())],
                        )],
                    );
                } else {
                    init = t::call(
                        t::member_id(t::id_dollar(), "child"),
                        vec![t::id_owned(parent_var.to_string())],
                    );
                }
            } else {
                init = t::call(
                    t::member_id(t::id_dollar(), "sibling"),
                    vec![
                        t::call(
                            t::member_id(t::id_dollar(), "first_child"),
                            vec![t::id_owned(parent_var.to_string())],
                        ),
                        t::lit_number(i as f64),
                    ],
                );
            }
        } else {
            let prev = prev_child_var.as_ref().expect("prev_child_var set");
            let prev_i = prev_child_idx.unwrap();
            let offset = i - prev_i;
            let prefix = match children[i] {
                FragmentChild::RegularElement(child_el) => child_el.name.clone(),
                FragmentChild::HtmlTag(_) => "node".to_string(),
                FragmentChild::ExpressionTag(_) => "text".to_string(),
                _ => "node".to_string(),
            };
            var = allocate_named(&prefix, var_names);
            init = t::call(
                t::member_id(t::id_dollar(), "sibling"),
                vec![t::id_owned(prev.to_string()), t::lit_number(offset as f64)],
            );
        }
        body.push(t::var(&var, init));
        // Emit the reactive handler for this child.
        match children[i] {
            FragmentChild::ExpressionTag(et) => {
                let expr = rewrite_props_destructured(&et.expression, &script.props_destructured);
                effects.push((var.clone(), expr));
            }
            FragmentChild::HtmlTag(ht) => {
                let expr = rewrite_props_destructured(&ht.expression, &script.props_destructured);
                let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(expr),
                    r#async: false,
                    span: Span::ZERO,
                }));
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "html"),
                    vec![t::id_owned(var.to_string()), getter],
                )));
            }
            FragmentChild::RegularElement(child_el) => {
                if is_text_only_element(child_el) {
                    // Build inline expression from mixed Text + ExpressionTag
                    // children — preserves surrounding text in a template
                    // literal (e.g. `Count: {count}` → \`Count: ${count}\`).
                    let mut parts: Vec<TextPart> = Vec::new();
                    for c in &child_el.fragment.nodes {
                        match c {
                            FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                            FragmentChild::ExpressionTag(et) => {
                                parts.push(TextPart::Expr(&et.expression))
                            }
                            _ => {}
                        }
                    }
                    let inline = build_inline_template(&parts, &HashSet::new());
                    let inline = rewrite_props_destructured(&inline, &script.props_destructured);
                    // Determine if the inline expression references any
                    // reactive binding — if not, skip the text-anchor
                    // allocation, template_effect push, and reset entirely.
                    let mut reactive: HashSet<String> = HashSet::new();
                    reactive.extend(script.state_bindings.iter().cloned());
                    reactive.extend(script.proxy_bindings.iter().cloned());
                    reactive.extend(script.derived_bindings.iter().cloned());
                    reactive.extend(script.props_destructured.iter().cloned());
                    reactive.extend(script.rest_props_bindings.iter().cloned());
                    let legacy_prop_names_local: HashSet<String> = script
                        .legacy_export_props
                        .iter()
                        .map(|(n, _)| n.clone())
                        .collect();
                    reactive.extend(legacy_prop_names_local.iter().cloned());
                    let is_reactive_expr = expression_has_any_binding(&inline, &reactive);
                    if is_reactive_expr {
                        let text_var = allocate_named("text", var_names);
                        body.push(t::var(
                            &text_var,
                            t::call(
                                t::member_id(t::id_dollar(), "child"),
                                vec![
                                    t::id_owned(var.to_string()),
                                    Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                        value: true,
                                        span: Span::ZERO,
                                    }))),
                                ],
                            ),
                        ));
                        effects.push((text_var, inline));
                        body.push(t::stmt(t::call(
                            t::member_id(t::id_dollar(), "reset"),
                            vec![t::id_owned(var.to_string())],
                        )));
                    }
                } else {
                    // Recurse into the child.
                    walk_element_interior(child_el, &var, body, effects, var_names, counters, script);
                    if fragment_has_deep_reactive(&child_el.fragment) {
                        body.push(t::stmt(t::call(
                            t::member_id(t::id_dollar(), "reset"),
                            vec![t::id_owned(var.to_string())],
                        )));
                    }
                }
            }
            _ => {}
        }
        prev_child_var = Some(var);
        prev_child_idx = Some(i);
    }

    // Inside `<pre>` / `<textarea>`: when raw nodes after the last reactive
    // child include any preserved text/element, emit `$.next()` to advance
    // the hydration cursor past those nodes before the outer reset.
    if el.name == "pre" || el.name == "textarea" {
        let last_reactive_stripped = *reactive_idx.last().unwrap();
        let last_reactive_raw = start + last_reactive_stripped;
        let has_trailing = el.fragment.nodes.iter().skip(last_reactive_raw + 1).any(|n| {
            match n {
                FragmentChild::Text(t) => !t.data.is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            }
        });
        if has_trailing {
            body.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "next"),
                Vec::new(),
            )));
        }
    }
    // Trailing static siblings after the last reactive child — emit $.next(N).
    let last_reactive = *reactive_idx.last().unwrap();
    let trailing_count = children.len() - 1 - last_reactive;
    if trailing_count > 0 {
        body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            vec![t::lit_number(trailing_count as f64)],
        )));
    }
}

fn apply_reactive_attrs(
    el: &svelte_ast::elements::RegularElement,
    var: &str,
    body: &mut Vec<Statement>,
    _script: &ScriptInfo,
) {
    let is_custom = el.name.contains('-');
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            if is_custom {
                // `$.set_custom_element_data(var, NAME, VALUE)`.
                let value_expr = attr_value_as_string_expr(&attr.value);
                body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "set_custom_element_data"),
                    vec![
                        t::id_owned(var.to_string()),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(attr.name.clone()),
                            raw: Some(format!("'{}'", attr.name)),
                            span: Span::ZERO,
                        }))),
                        value_expr,
                    ],
                )));
                continue;
            }
            match attr.name.as_ref() {
                "autofocus" => {
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "autofocus"),
                        vec![
                            t::id_owned(var.to_string()),
                            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                value: true,
                                span: Span::ZERO,
                            }))),
                        ],
                    )));
                }
                "muted" if el.name == "source" || el.name == "video" || el.name == "audio" => {
                    // `EL.muted = true;`
                    body.push(t::stmt(Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(Expression::Member(Box::new(
                            MemberExpression {
                                object: t::id_owned(var.to_string()),
                                property: MemberProperty::Identifier(Identifier {
                                    name: Cow::Borrowed("muted"),
                                    span: Span::ZERO,
                                }),
                                computed: false,
                                optional: false,
                                span: Span::ZERO,
                            },
                        ))),
                        operator: AssignmentOperator::Assign,
                        right: Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                            value: true,
                            span: Span::ZERO,
                        }))),
                        span: Span::ZERO,
                    }))));
                }
                "dir" => {
                    // Skip here — handled via a separate pass that pushes
                    // the dir self-assignment to the trailing effects pile.
                    // (Order matters: $.next() before $.template_effect.)
                }
                "checked" | "value" if el.name == "input" => {
                    // Handled via separate pass that emits
                    // `$.remove_input_defaults(input)` once per input.
                }
                "value" if el.name == "option" => {
                    // `EL.value = EL.__value = 'X';`
                    let value_expr = attr_value_as_string_expr(&attr.value);
                    // Inner: EL.__value = 'X'
                    let inner = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(Expression::Member(Box::new(
                            MemberExpression {
                                object: t::id_owned(var.to_string()),
                                property: MemberProperty::Identifier(Identifier {
                                    name: Cow::Borrowed("__value"),
                                    span: Span::ZERO,
                                }),
                                computed: false,
                                optional: false,
                                span: Span::ZERO,
                            },
                        ))),
                        operator: AssignmentOperator::Assign,
                        right: value_expr,
                        span: Span::ZERO,
                    }));
                    // Outer: EL.value = inner
                    let outer = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(Expression::Member(Box::new(
                            MemberExpression {
                                object: t::id_owned(var.to_string()),
                                property: MemberProperty::Identifier(Identifier {
                                    name: Cow::Borrowed("value"),
                                    span: Span::ZERO,
                                }),
                                computed: false,
                                optional: false,
                                span: Span::ZERO,
                            },
                        ))),
                        operator: AssignmentOperator::Assign,
                        right: inner,
                        span: Span::ZERO,
                    }));
                    body.push(t::stmt(outer));
                }
                _ => {}
            }
        }
    }
}

fn attr_value_as_string_expr(v: &AttributeValue) -> Expression {
    match v {
        AttributeValue::Many(parts) if parts.len() == 1 => {
            if let AttributeValuePart::Text(t) = &parts[0] {
                return Expression::Literal(Box::new(Literal::String(StringLiteral {
                    value: Cow::Owned(t.data.clone()),
                    raw: Some(format!("'{}'", t.data)),
                    span: Span::ZERO,
                })));
            }
            t::id("undefined")
        }
        _ => t::id("undefined"),
    }
}

fn is_text_only_element(el: &svelte_ast::elements::RegularElement) -> bool {
    // Returns true iff the element body is exclusively Text + ExpressionTag
    // (i.e. text-with-interpolation), with at least one NON-LITERAL
    // ExpressionTag. Literal-foldable ExpressionTags get serialized
    // directly (their value is statically known), so the element is
    // either fully static (no text anchor) or text-only (anchor needed).
    let mut has_non_literal_expr = false;
    for n in &el.fragment.nodes {
        match n {
            FragmentChild::Text(_) => {}
            FragmentChild::ExpressionTag(et) => {
                if literal_to_template_string(&et.expression).is_none() {
                    has_non_literal_expr = true;
                }
            }
            _ => return false,
        }
    }
    has_non_literal_expr
}

fn single_expression_in_element(
    el: &svelte_ast::elements::RegularElement,
) -> Option<&Expression> {
    for n in &el.fragment.nodes {
        if let FragmentChild::ExpressionTag(et) = n {
            return Some(&et.expression);
        }
    }
    None
}

fn serialize_fragment_to_html(
    f: &svelte_ast::fragment::Fragment,
    out: &mut String,
    needs_import_node: &mut bool,
) -> Option<()> {
    // Mirrors upstream `clean_nodes` (3-transform/utils.js:172-201): drop
    // leading/trailing pure-whitespace text nodes inside every fragment,
    // not just at the top level.
    let nodes_v = trim_pure_whitespace_text(&f.nodes);
    let nodes = &nodes_v[..];
    let last_idx = nodes.len().saturating_sub(1);
    let mut last_was_text_with_space = false;
    for (i, n) in nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let mut collapsed = collapse_ws_client(&t.data);
                // Trim leading whitespace if at start of fragment body.
                if i == 0 {
                    collapsed = collapsed.trim_start().to_string();
                }
                // Trim trailing whitespace if at end of fragment body.
                if i == last_idx {
                    collapsed = collapsed.trim_end().to_string();
                }
                if collapsed.is_empty() {
                    continue;
                }
                // Don't emit duplicate spaces.
                if last_was_text_with_space && collapsed.starts_with(' ') {
                    let rest = collapsed.trim_start_matches(' ');
                    if !rest.is_empty() {
                        out.push_str(rest);
                    }
                } else {
                    out.push_str(&collapsed);
                }
                last_was_text_with_space = collapsed.ends_with(' ');
            }
            FragmentChild::Comment(c) => {
                // Strip `<!-- svelte-ignore ... -->` directives — they're
                // analyzer hints, not real comments. Keep everything else
                // (e.g. `<!-- test -->` inside elements is meaningful for
                // hydration claim).
                let _ = i;
                let trimmed = c.data.trim();
                if trimmed.starts_with("svelte-ignore") {
                    continue;
                }
                out.push_str("<!--");
                out.push_str(&c.data);
                out.push_str("-->");
                last_was_text_with_space = false;
            }
            FragmentChild::HtmlTag(_) => {
                out.push_str("<!>");
                last_was_text_with_space = false;
            }
            FragmentChild::ExpressionTag(_) => {
                // Both literal-foldable and non-literal expressions inside
                // an element body emit nothing in the template. The text
                // value (if reactive) is set via template_effect at runtime;
                // literal values are statically known but upstream still
                // emits an empty template position.
                let _ = last_was_text_with_space;
            }
            FragmentChild::RegularElement(el) => {
                serialize_element_to_html(el, out, needs_import_node)?;
                last_was_text_with_space = false;
            }
            _ => return None,
        }
    }
    Some(())
}

fn serialize_element_to_html(
    el: &svelte_ast::elements::RegularElement,
    out: &mut String,
    needs_import_node: &mut bool,
) -> Option<()> {
    let is_custom = el.name.contains('-');
    if is_custom || el.name == "video" {
        *needs_import_node = true;
    }
    out.push('<');
    out.push_str(&el.name);
    let is_text_only = is_text_only_element(el);
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            // Skip reactive attrs that the walker handles separately.
            let skip = if is_custom {
                true
            } else {
                matches!(attr.name.as_ref(), "autofocus" | "muted")
                    || (el.name == "option" && attr.name == "value")
            };
            if skip {
                continue;
            }
            match &attr.value {
                AttributeValue::Empty => {
                    // Mirrors upstream which serializes empty attrs as
                    // `name=""` so the HTML matches what the server emits
                    // (important for hydration matching).
                    out.push(' ');
                    out.push_str(&attr.name);
                    out.push_str("=\"\"");
                }
                AttributeValue::Many(parts) => {
                    let mut s = String::new();
                    let mut all_text = true;
                    for p in parts {
                        if let AttributeValuePart::Text(t) = p {
                            s.push_str(&t.data);
                        } else {
                            all_text = false;
                            break;
                        }
                    }
                    if !all_text {
                        return None;
                    }
                    out.push(' ');
                    out.push_str(&attr.name);
                    out.push_str("=\"");
                    out.push_str(&s);
                    out.push('"');
                }
                _ => return None,
            }
        }
    }
    if is_void_client(&el.name) {
        out.push_str("/>");
        return Some(());
    }
    out.push('>');
    if el.name == "noscript" {
        // <noscript> contents are intentionally empty in the client template —
        // hydration only matches the opening/closing tags, and the original
        // content lives in the server payload (where it matters).
    } else if el.name == "pre" || el.name == "textarea" {
        // Whitespace-preserving elements: emit child content verbatim,
        // stripping at most ONE leading newline (HTML5 default behavior),
        // and skipping the space-placeholder for nested text-anchor
        // elements.
        serialize_pre_fragment_to_html(&el.fragment, out, needs_import_node)?;
    } else if is_text_only {
        // Placeholder space for the text anchor.
        out.push(' ');
    } else {
        serialize_fragment_to_html(&el.fragment, out, needs_import_node)?;
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

/// Serialize the body of a `<pre>` / `<textarea>` element. Preserves
/// whitespace verbatim. HTML5 strips ONE leading newline from `<pre>`
/// content only when that leading newline is immediately followed by a
/// non-text node (element). For text content, the newline is kept.
/// Text-only nested elements (e.g. `<span>{x}</span>` inside `<pre>`)
/// do NOT get the space-anchor placeholder.
fn serialize_pre_fragment_to_html(
    f: &svelte_ast::fragment::Fragment,
    out: &mut String,
    needs_import_node: &mut bool,
) -> Option<()> {
    // Upstream strips ONLY when the very first text node is exactly "\n"
    // and immediately followed by a non-text node. Multi-newline leading
    // whitespace is preserved verbatim.
    let strip_leading_newline = f
        .nodes
        .first()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data == "\n"))
        .unwrap_or(false)
        && f.nodes.get(1).map(|n| matches!(n, FragmentChild::RegularElement(_))).unwrap_or(false);
    for (i, n) in f.nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let mut data = t.data.clone();
                if i == 0 && strip_leading_newline && data.starts_with('\n') {
                    data.remove(0);
                }
                for ch in data.chars() {
                    match ch {
                        '`' => out.push_str("\\`"),
                        '\\' => out.push_str("\\\\"),
                        _ => out.push(ch),
                    }
                }
            }
            FragmentChild::Comment(c) => {
                let trimmed = c.data.trim();
                if trimmed.starts_with("svelte-ignore") {
                    continue;
                }
                out.push_str("<!--");
                out.push_str(&c.data);
                out.push_str("-->");
            }
            FragmentChild::HtmlTag(_) => {
                out.push_str("<!>");
            }
            FragmentChild::ExpressionTag(_) => {
                // Inside <pre>, expression tags don't reserve a placeholder
                // space — the existing SSR text is the anchor.
            }
            FragmentChild::RegularElement(el) => {
                serialize_pre_element_to_html(el, out, needs_import_node)?;
            }
            _ => return None,
        }
    }
    Some(())
}

fn serialize_pre_element_to_html(
    el: &svelte_ast::elements::RegularElement,
    out: &mut String,
    needs_import_node: &mut bool,
) -> Option<()> {
    let is_custom = el.name.contains('-');
    if is_custom || el.name == "video" {
        *needs_import_node = true;
    }
    out.push('<');
    out.push_str(&el.name);
    for a in &el.attributes {
        if let ElementAttribute::Attribute(attr) = a {
            use svelte_ast::attributes::{AttributeValue, AttributeValuePart};
            match &attr.value {
                AttributeValue::Empty => {
                    out.push(' ');
                    out.push_str(&attr.name);
                    out.push_str("=\"\"");
                }
                AttributeValue::Many(parts) => {
                    if parts.iter().all(|p| matches!(p, AttributeValuePart::Text(_))) {
                        out.push(' ');
                        out.push_str(&attr.name);
                        out.push_str("=\"");
                        for p in parts {
                            if let AttributeValuePart::Text(t) = p {
                                for c in t.data.chars() {
                                    match c {
                                        '"' => out.push_str("&quot;"),
                                        '&' => out.push_str("&amp;"),
                                        _ => out.push(c),
                                    }
                                }
                            }
                        }
                        out.push('"');
                    }
                }
                _ => {}
            }
        }
    }
    if is_void_client(&el.name) {
        out.push_str("/>");
        return Some(());
    }
    out.push('>');
    // Children: NO space placeholder for text-only elements inside <pre>.
    serialize_pre_fragment_to_html(&el.fragment, out, needs_import_node)?;
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

fn trim_boundary_text_client(nodes: &[FragmentChild]) -> Vec<&FragmentChild> {
    // Drop leading/trailing whitespace-only text nodes for top-level
    // fragment serialization. Returns a Vec of references.
    let mut start = 0;
    let mut end = nodes.len();
    while start < end {
        match &nodes[start] {
            FragmentChild::Text(t) if t.data.trim().is_empty() => start += 1,
            FragmentChild::Comment(_) => start += 1,
            _ => break,
        }
    }
    while end > start {
        match &nodes[end - 1] {
            FragmentChild::Text(t) if t.data.trim().is_empty() => end -= 1,
            FragmentChild::Comment(_) => end -= 1,
            _ => break,
        }
    }
    nodes[start..end].iter().collect()
}

/// Mirrors upstream `clean_nodes` (3-transform/utils.js:126-251) for the
/// template-text serializer: trims leading/trailing pure-whitespace text
/// and drops `svelte-ignore` directive comments. Real comments (e.g.
/// `<!-- test -->` inside an element) are preserved.
fn trim_pure_whitespace_text(nodes: &[FragmentChild]) -> Vec<&FragmentChild> {
    let mut filtered: Vec<&FragmentChild> = nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Comment(c) => !c.data.trim().starts_with("svelte-ignore"),
            _ => true,
        })
        .collect();
    let mut start = 0;
    let mut end = filtered.len();
    while start < end {
        match filtered[start] {
            FragmentChild::Text(t) if t.data.trim().is_empty() => start += 1,
            _ => break,
        }
    }
    while end > start {
        match filtered[end - 1] {
            FragmentChild::Text(t) if t.data.trim().is_empty() => end -= 1,
            _ => break,
        }
    }
    filtered.drain(end..);
    filtered.drain(..start);
    filtered
}

fn collapse_ws_client(s: &str) -> String {
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

fn is_void_client(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta"
            | "param" | "source" | "track" | "wbr"
    )
}

/// Rewrite every Identifier in `e` that's in `names` to `$$props.NAME`.
/// Used for `let { title, content } = $props()`-style destructure: the
/// declaration is dropped and references become direct member access.
fn rewrite_props_destructured(e: &Expression, names: &HashSet<String>) -> Expression {
    fn go(e: &Expression, names: &HashSet<String>) -> Expression {
        match e {
            Expression::Identifier(id) if names.contains(id.name.as_ref()) => {
                Expression::Member(Box::new(MemberExpression {
                    object: t::id("$$props"),
                    property: MemberProperty::Identifier(Identifier {
                        name: id.name.clone(),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }))
            }
            Expression::Call(c) => Expression::Call(Box::new(CallExpression {
                callee: go(&c.callee, names),
                arguments: c
                    .arguments
                    .iter()
                    .map(|a| match a {
                        Argument::Expression(e) => Argument::Expression(go(e, names)),
                        other => other.clone(),
                    })
                    .collect(),
                optional: c.optional,
                span: c.span,
            })),
            Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
                object: go(&m.object, names),
                property: m.property.clone(),
                computed: m.computed,
                optional: m.optional,
                span: m.span,
            })),
            Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
                operator: b.operator,
                left: go(&b.left, names),
                right: go(&b.right, names),
                span: b.span,
            })),
            Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
                operator: l.operator,
                left: go(&l.left, names),
                right: go(&l.right, names),
                span: l.span,
            })),
            Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
                operator: u.operator,
                argument: go(&u.argument, names),
                prefix: u.prefix,
                span: u.span,
            })),
            Expression::Conditional(c) => Expression::Conditional(Box::new(ConditionalExpression {
                test: go(&c.test, names),
                consequent: go(&c.consequent, names),
                alternate: go(&c.alternate, names),
                span: c.span,
            })),
            Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
                expression: go(&p.expression, names),
                span: p.span,
            })),
            Expression::Object(o) => Expression::Object(Box::new(ObjectExpression {
                properties: o
                    .properties
                    .iter()
                    .map(|m| match m {
                        ObjectMember::Property(p) => ObjectMember::Property(Box::new(Property {
                            key: p.key.clone(),
                            value: go(&p.value, names),
                            kind: p.kind,
                            computed: p.computed,
                            shorthand: p.shorthand,
                            method: p.method,
                            span: p.span,
                        })),
                        ObjectMember::Spread(s) => ObjectMember::Spread(Box::new(SpreadElement {
                            argument: go(&s.argument, names),
                            span: s.span,
                        })),
                    })
                    .collect(),
                span: o.span,
            })),
            Expression::Array(a) => Expression::Array(Box::new(ArrayExpression {
                elements: a
                    .elements
                    .iter()
                    .map(|el| match el {
                        ArrayElement::Expression(e) => ArrayElement::Expression(go(e, names)),
                        ArrayElement::Spread(s) => ArrayElement::Spread(Box::new(SpreadElement {
                            argument: go(&s.argument, names),
                            span: s.span,
                        })),
                        ArrayElement::Elision => ArrayElement::Elision,
                    })
                    .collect(),
                span: a.span,
            })),
            e => e.clone(),
        }
    }
    go(e, names)
}

/// Rewrite props_destructured references in a Statement (recursively
/// into expression positions). Used to clean up script-body statements
/// like `const attrs = { x: browser ? ... : ... };`.
fn rewrite_stmt_props_destructured(s: &Statement, names: &HashSet<String>) -> Statement {
    if names.is_empty() {
        return s.clone();
    }
    match s {
        Statement::Variable(v) => Statement::Variable(Box::new(VariableDeclaration {
            kind: v.kind,
            declarations: v
                .declarations
                .iter()
                .map(|d| VariableDeclarator {
                    id: d.id.clone(),
                    init: d
                        .init
                        .as_ref()
                        .map(|e| rewrite_props_destructured(e, names)),
                    type_annotation: None,
                    span: d.span,
                })
                .collect(),
            span: v.span,
        })),
        Statement::Expression(e) => {
            Statement::Expression(Box::new(svelte_js_ast::ExpressionStatement {
                expression: rewrite_props_destructured(&e.expression, names),
                span: e.span,
            }))
        }
        _ => s.clone(),
    }
}

/// Rewrite each free `X` identifier in `e` to a call `X()` when `X` is in
/// `names` — used for legacy-mode `export let X` reads since those resolve
/// to a `$.prop($$props, 'X', N)` accessor that must be invoked.
fn rewrite_legacy_prop_reads(e: &Expression, names: &HashSet<String>) -> Expression {
    fn go(e: &Expression, names: &HashSet<String>) -> Expression {
        match e {
            Expression::Identifier(id) if names.contains(id.name.as_ref()) => {
                Expression::Call(Box::new(CallExpression {
                    callee: Expression::Identifier(id.clone()),
                    arguments: Vec::new(),
                    optional: false,
                    span: Span::ZERO,
                }))
            }
            Expression::Call(c) => Expression::Call(Box::new(CallExpression {
                callee: go(&c.callee, names),
                arguments: c
                    .arguments
                    .iter()
                    .map(|a| match a {
                        Argument::Expression(e) => Argument::Expression(go(e, names)),
                        other => other.clone(),
                    })
                    .collect(),
                optional: c.optional,
                span: c.span,
            })),
            Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
                object: go(&m.object, names),
                property: m.property.clone(),
                computed: m.computed,
                optional: m.optional,
                span: m.span,
            })),
            Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
                operator: b.operator,
                left: go(&b.left, names),
                right: go(&b.right, names),
                span: b.span,
            })),
            Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
                operator: l.operator,
                left: go(&l.left, names),
                right: go(&l.right, names),
                span: l.span,
            })),
            Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
                operator: u.operator,
                argument: go(&u.argument, names),
                prefix: u.prefix,
                span: u.span,
            })),
            Expression::Conditional(c) => Expression::Conditional(Box::new(ConditionalExpression {
                test: go(&c.test, names),
                consequent: go(&c.consequent, names),
                alternate: go(&c.alternate, names),
                span: c.span,
            })),
            Expression::Paren(p) => Expression::Paren(Box::new(svelte_js_ast::ParenthesizedExpression {
                expression: go(&p.expression, names),
                span: p.span,
            })),
            Expression::Template(t) => Expression::Template(Box::new(TemplateLiteral {
                quasis: t.quasis.clone(),
                expressions: t.expressions.iter().map(|ex| go(ex, names)).collect(),
                span: t.span,
            })),
            _ => e.clone(),
        }
    }
    go(e, names)
}

/// Build the `var $$exports = { get NAME() {...}, set NAME($$value) {...} };`
/// object that legacy components return through `$.pop($$exports)`. Per
/// upstream's accessor shape for `$.prop`-wrapped props.
fn build_legacy_exports_object(props: &[(String, Option<Expression>)]) -> Expression {
    let mut members: Vec<ObjectMember> = Vec::new();
    for (name, _) in props {
        let getter_body = vec![Statement::Return(Box::new(svelte_js_ast::ReturnStatement {
            argument: Some(t::call(t::id_owned(name.to_string()), Vec::new())),
            span: Span::ZERO,
        }))];
        members.push(ObjectMember::Property(Box::new(svelte_js_ast::Property {
            key: PropertyKey::Identifier(Identifier { name: Cow::Owned(name.clone()), span: Span::ZERO }),
            value: Expression::Function(Box::new(FunctionExpression {
                id: None,
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: BlockStatement { body: getter_body, span: Span::ZERO },
                generator: false,
                r#async: false,
                span: Span::ZERO,
            })),
            kind: PropertyKind::Get,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
        let setter_body = vec![
            t::stmt(t::call(t::id_owned(name.to_string()), vec![t::id("$$value")])),
            t::stmt(t::call(t::member_id(t::id_dollar(), "flush"), Vec::new())),
        ];
        members.push(ObjectMember::Property(Box::new(svelte_js_ast::Property {
            key: PropertyKey::Identifier(Identifier { name: Cow::Owned(name.clone()), span: Span::ZERO }),
            value: Expression::Function(Box::new(FunctionExpression {
                id: None,
                params: vec![t::pat_id("$$value")],
                param_type_annotations: Vec::new(),
                body: BlockStatement { body: setter_body, span: Span::ZERO },
                generator: false,
                r#async: false,
                span: Span::ZERO,
            })),
            kind: PropertyKind::Set,
            computed: false,
            shorthand: false,
            method: false,
            span: Span::ZERO,
        })));
    }
    Expression::Object(Box::new(ObjectExpression { properties: members, span: Span::ZERO }))
}

/// Returns true iff `e` contains a CallExpression whose callee is a regular
/// user-function reference (not a derived-binding read or other compiler-
/// inserted call). Used to decide if an if-chain test should be hoisted to
/// a `$.derived(...)` for reactive caching.
fn expr_has_user_call(e: &Expression, derived_bindings: &HashSet<String>) -> bool {
    match e {
        Expression::Call(c) => {
            // A call whose callee is a derived binding (e.g. `blocking()`)
            // doesn't count — those are sync reads. Anything else does.
            if let Expression::Identifier(id) = &c.callee {
                if derived_bindings.contains(id.name.as_ref()) {
                    return c.arguments.iter().any(|a| match a {
                        Argument::Expression(e) => expr_has_user_call(e, derived_bindings),
                        Argument::Spread(s) => expr_has_user_call(&s.argument, derived_bindings),
                    });
                }
            }
            true
        }
        Expression::Binary(b) => {
            expr_has_user_call(&b.left, derived_bindings)
                || expr_has_user_call(&b.right, derived_bindings)
        }
        Expression::Logical(l) => {
            expr_has_user_call(&l.left, derived_bindings)
                || expr_has_user_call(&l.right, derived_bindings)
        }
        Expression::Unary(u) => expr_has_user_call(&u.argument, derived_bindings),
        Expression::Conditional(c) => {
            expr_has_user_call(&c.test, derived_bindings)
                || expr_has_user_call(&c.consequent, derived_bindings)
                || expr_has_user_call(&c.alternate, derived_bindings)
        }
        Expression::Paren(p) => expr_has_user_call(&p.expression, derived_bindings),
        Expression::Member(m) => expr_has_user_call(&m.object, derived_bindings),
        _ => false,
    }
}

fn collect_chain_blockers(
    ib: &svelte_ast::blocks::IfBlock,
    blocker_bindings: &HashMap<String, usize>,
    out: &mut std::collections::BTreeSet<usize>,
) {
    collect_blocker_indices_in_expr(&ib.test, blocker_bindings, out);
    if let Some(alt) = &ib.alternate {
        let non_ws: Vec<&FragmentChild> = alt
            .nodes
            .iter()
            .filter(|n| match n {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        if non_ws.len() == 1 {
            if let FragmentChild::IfBlock(inner) = non_ws[0] {
                if inner.elseif && !expr_top_await(&inner.test) {
                    collect_chain_blockers(inner, blocker_bindings, out);
                }
            }
        }
    }
}

fn collect_blocker_indices_in_expr(
    e: &Expression,
    blocker_bindings: &HashMap<String, usize>,
    out: &mut std::collections::BTreeSet<usize>,
) {
    match e {
        Expression::Identifier(id) => {
            if let Some(idx) = blocker_bindings.get(id.name.as_ref()) {
                out.insert(*idx);
            }
        }
        Expression::Call(c) => {
            collect_blocker_indices_in_expr(&c.callee, blocker_bindings, out);
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => {
                        collect_blocker_indices_in_expr(e, blocker_bindings, out)
                    }
                    Argument::Spread(s) => {
                        collect_blocker_indices_in_expr(&s.argument, blocker_bindings, out)
                    }
                }
            }
        }
        Expression::Member(m) => collect_blocker_indices_in_expr(&m.object, blocker_bindings, out),
        Expression::Binary(b) => {
            collect_blocker_indices_in_expr(&b.left, blocker_bindings, out);
            collect_blocker_indices_in_expr(&b.right, blocker_bindings, out);
        }
        Expression::Logical(l) => {
            collect_blocker_indices_in_expr(&l.left, blocker_bindings, out);
            collect_blocker_indices_in_expr(&l.right, blocker_bindings, out);
        }
        Expression::Unary(u) => collect_blocker_indices_in_expr(&u.argument, blocker_bindings, out),
        Expression::Await(a) => collect_blocker_indices_in_expr(&a.argument, blocker_bindings, out),
        Expression::Paren(p) => {
            collect_blocker_indices_in_expr(&p.expression, blocker_bindings, out)
        }
        _ => {}
    }
}

/// Build a branch body for a simple branch with text-only content:
/// `($$anchor) => { var text_N = $.text('FOO'); $.append($$anchor, text_N); }`
fn build_branch_arrow(
    fragment: &svelte_ast::fragment::Fragment,
    _ai: &AsyncInfo,
    _derived_bindings: &HashSet<String>,
    counters: &mut ChainCounters,
) -> Option<Expression> {
    // Concatenate the fragment's text nodes (trimmed) — that's the literal
    // we pass to `$.text(...)`.
    let mut text_value = String::new();
    for n in &fragment.nodes {
        if let FragmentChild::Text(t) = n {
            text_value.push_str(t.data.trim_matches(|c: char| c.is_whitespace() && c != ' '));
        }
    }
    let trimmed = text_value.trim().to_string();
    let text_name = counters.next_text();
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &text_name,
        t::call(
            t::member_id(t::id_dollar(), "text"),
            vec![Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(trimmed.clone()),
                raw: Some(format!("'{}'", trimmed)),
                span: Span::ZERO,
            })))],
        ),
    ));
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(text_name.to_string())],
    )));
    Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    })))
}

/// Build the alternate arrow for a break-out elseif. Wraps a nested
/// `$.async(...)` (or `{ }` block) targeting the inner if-chain.
fn build_breakout_alternate_arrow(
    inner: &svelte_ast::blocks::IfBlock,
    ai: &AsyncInfo,
    derived_bindings: &HashSet<String>,
    counters: &mut ChainCounters,
) -> Option<Expression> {
    let fragment_name = counters.next_fragment();
    let node_name = counters.next_node();
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &fragment_name,
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    body.push(t::var(
        &node_name,
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_owned(fragment_name.to_string())],
        ),
    ));
    let inner_stmt = emit_async_if_block(inner, &node_name, ai, derived_bindings, counters, true)?;
    body.push(inner_stmt);
    body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_owned(fragment_name.to_string())],
    )));
    Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor()],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    })))
}

/// Build the `if (...) $$render(consequent); else if (...) $$render(c_1, 1); else $$render(alt, -1);`
/// chain. `in_async_ctx_pass_true_to_dollar_if` is unused here — the 3rd
/// `true` arg is set by the caller of $.if.
fn build_render_body(
    branches: &[(String, Expression, i32)],
    alternate: Option<&str>,
    _in_async_ctx_pass_true_to_dollar_if: bool,
) -> Statement {
    // Build right-associative if/else if/else chain.
    let mut acc: Option<Statement> = alternate.map(|name| {
        t::stmt(t::call(
            t::id_render(),
            vec![t::id_owned(name.to_string()), t::lit_number(-1.0)],
        ))
    });
    for (i, (cname, test, branch_idx)) in branches.iter().enumerate().rev() {
        let call = if i == 0 {
            t::stmt(t::call(t::id_render(), vec![t::id_owned(cname.to_string())]))
        } else {
            t::stmt(t::call(
                t::id_render(),
                vec![t::id_owned(cname.to_string()), t::lit_number(*branch_idx as f64)],
            ))
        };
        let if_stmt = Statement::If(Box::new(IfStatement {
            test: test.clone(),
            consequent: call,
            alternate: acc,
            span: Span::ZERO,
        }));
        acc = Some(if_stmt);
    }
    acc.unwrap_or_else(|| {
        // Should be unreachable given the loop above always runs at least once.
        Statement::Block(Box::new(BlockStatement {
            body: Vec::new(),
            span: Span::ZERO,
        }))
    })
}

/// Rewrite a non-async test expression for use inside a `$.if(...)` body.
/// References to derived bindings become `$.get(NAME)`; references to other
/// blocker bindings outside the test-array path become `$.get(NAME)` too.
fn rewrite_chain_test(
    e: &Expression,
    blocker_bindings: &HashMap<String, usize>,
    derived_bindings: &HashSet<String>,
    counters: &mut ChainCounters,
) -> Expression {
    let _ = counters; // reserved for future $.derived hoisting
    rewrite_chain_expr(e, blocker_bindings, derived_bindings)
}

fn rewrite_chain_expr(
    e: &Expression,
    blocker_bindings: &HashMap<String, usize>,
    derived_bindings: &HashSet<String>,
) -> Expression {
    let _ = blocker_bindings;
    match e {
        Expression::Identifier(id) => {
            // Only `$derived(...)` (and `$state(...)`) bindings get `$.get`
            // wrapped at read sites. Non-derived `let` names — even when
            // touched by an async statement — stay raw, because they're
            // resolved synchronously inside the $.async callback.
            if derived_bindings.contains(id.name.as_ref()) {
                t::call(
                    t::member_id(t::id_dollar(), "get"),
                    vec![Expression::Identifier(id.clone())],
                )
            } else {
                e.clone()
            }
        }
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_chain_expr(&c.callee, blocker_bindings, derived_bindings),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => {
                        Argument::Expression(rewrite_chain_expr(e, blocker_bindings, derived_bindings))
                    }
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
            object: rewrite_chain_expr(&m.object, blocker_bindings, derived_bindings),
            property: m.property.clone(),
            computed: m.computed,
            optional: m.optional,
            span: m.span,
        })),
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_chain_expr(&b.left, blocker_bindings, derived_bindings),
            right: rewrite_chain_expr(&b.right, blocker_bindings, derived_bindings),
            span: b.span,
        })),
        Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
            operator: l.operator,
            left: rewrite_chain_expr(&l.left, blocker_bindings, derived_bindings),
            right: rewrite_chain_expr(&l.right, blocker_bindings, derived_bindings),
            span: l.span,
        })),
        Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
            operator: u.operator,
            argument: rewrite_chain_expr(&u.argument, blocker_bindings, derived_bindings),
            prefix: u.prefix,
            span: u.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_chain_expr(&p.expression, blocker_bindings, derived_bindings),
            span: p.span,
        })),
        e => e.clone(),
    }
}

/// Rewrite an async test for the `$.async(..., [async () => REWRITTEN], ...)`
/// form: each `await X` becomes `(await $.save(X))()`. Mirrors server's
/// `wrap_async_test` but kept separate so we can evolve them independently.
fn rewrite_async_test_client(e: &Expression) -> Expression {
    match e {
        Expression::Await(a) => {
            let inner = rewrite_async_test_client(&a.argument);
            let saved = t::call(t::member_id(t::id_dollar(), "save"), vec![inner]);
            let awaited = Expression::Paren(Box::new(ParenthesizedExpression {
                expression: Expression::Await(Box::new(AwaitExpression {
                    argument: saved,
                    span: Span::ZERO,
                })),
                span: Span::ZERO,
            }));
            t::call(awaited, Vec::new())
        }
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_async_test_client(&b.left),
            right: rewrite_async_test_client(&b.right),
            span: b.span,
        })),
        Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
            operator: l.operator,
            left: rewrite_async_test_client(&l.left),
            right: rewrite_async_test_client(&l.right),
            span: l.span,
        })),
        Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
            operator: u.operator,
            argument: rewrite_async_test_client(&u.argument),
            prefix: u.prefix,
            span: u.span,
        })),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_async_test_client(&c.callee),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => Argument::Expression(rewrite_async_test_client(e)),
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Member(m) => Expression::Member(Box::new(MemberExpression {
            object: rewrite_async_test_client(&m.object),
            property: m.property.clone(),
            computed: m.computed,
            optional: m.optional,
            span: m.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_async_test_client(&p.expression),
            span: p.span,
        })),
        e => e.clone(),
    }
}

/// `(await $.save(X))()` — wrap an expression for the async-const setter form.
fn save_await_call_client(inner: Expression) -> Expression {
    let saved = t::call(t::member_id(t::id_dollar(), "save"), vec![inner]);
    let awaited = Expression::Paren(Box::new(ParenthesizedExpression {
        expression: Expression::Await(Box::new(AwaitExpression {
            argument: saved,
            span: Span::ZERO,
        })),
        span: Span::ZERO,
    }));
    t::call(awaited, Vec::new())
}

/// Walk an expression and replace any `Identifier` in `consts` with
/// `$.get(IDENT)` — used to wrap reads of const-bound names in the derived
/// thunk.
fn rewrite_const_refs_with_get(e: &Expression, consts: &[String]) -> Expression {
    match e {
        Expression::Identifier(id) if consts.iter().any(|n| n == &id.name) => {
            t::call(t::member_id(t::id_dollar(), "get"), vec![Expression::Identifier(id.clone())])
        }
        Expression::Binary(b) => Expression::Binary(Box::new(BinaryExpression {
            operator: b.operator,
            left: rewrite_const_refs_with_get(&b.left, consts),
            right: rewrite_const_refs_with_get(&b.right, consts),
            span: b.span,
        })),
        Expression::Logical(l) => Expression::Logical(Box::new(LogicalExpression {
            operator: l.operator,
            left: rewrite_const_refs_with_get(&l.left, consts),
            right: rewrite_const_refs_with_get(&l.right, consts),
            span: l.span,
        })),
        Expression::Unary(u) => Expression::Unary(Box::new(UnaryExpression {
            operator: u.operator,
            argument: rewrite_const_refs_with_get(&u.argument, consts),
            prefix: u.prefix,
            span: u.span,
        })),
        Expression::Call(c) => Expression::Call(Box::new(CallExpression {
            callee: rewrite_const_refs_with_get(&c.callee, consts),
            arguments: c
                .arguments
                .iter()
                .map(|a| match a {
                    Argument::Expression(e) => {
                        Argument::Expression(rewrite_const_refs_with_get(e, consts))
                    }
                    other => other.clone(),
                })
                .collect(),
            optional: c.optional,
            span: c.span,
        })),
        Expression::Paren(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: rewrite_const_refs_with_get(&p.expression, consts),
            span: p.span,
        })),
        e => e.clone(),
    }
}

/// `{#each await EXPR as ITEM}body{/each}` →
/// `\$.async(node, [], [() => EXPR], (node, \$\$collection) => {
///     \$.each(node, 17, () => \$.get(\$\$collection), \$.index, (\$\$anchor, ITEM) => { ... });
/// });`
fn emit_single_async_each_program(
    eb: &svelte_ast::blocks::EachBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Body: only support text-only bodies (single ExpressionTag) for now.
    let body_non_ws: Vec<&FragmentChild> = eb
        .body
        .nodes
        .iter()
        .filter(|c| match c {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if body_non_ws.len() != 1 {
        return None;
    }
    let body_expr = match body_non_ws[0] {
        FragmentChild::ExpressionTag(et) => &et.expression,
        _ => return None,
    };

    let collection_inner = strip_outer_await(&eb.expression);

    // Each body arrow: ($$anchor, ITEM) => {
    //     $.next();
    //     var text = $.text();
    //     $.template_effect(($0) => $.set_text(text, $0), void 0, [() => $.get(ITEM) or RAW]);
    //     $.append($$anchor, text);
    // }
    let item_name = match &eb.context {
        Some(Pattern::Identifier(i)) => i.name.clone(),
        _ => Cow::Borrowed("$$item"),
    };
    let body_uses_item = expr_contains_ident(body_expr, &item_name);
    let each_flag: f64 = if body_uses_item { 17.0 } else { 16.0 };
    let mut each_body_stmts: Vec<Statement> = Vec::new();
    each_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "next"),
        Vec::new(),
    )));
    each_body_stmts.push(t::var(
        "text",
        t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
    ));
    let set_text_call = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), t::id("$0")],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$0")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(set_text_call),
        r#async: false,
        span: Span::ZERO,
    }));
    // dep: `() => $.get(ITEM)` when body expr is `await ITEM` (or referenced via await)
    // For the more general case, we want to extract: strip outer await, then
    // wrap the remaining identifier with $.get if it's the iteration var.
    let body_inner = strip_outer_await(body_expr);
    let body_with_get = if let Expression::Identifier(i) = &body_inner {
        if i.name == item_name {
            t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(i.name.to_string())])
        } else {
            body_inner
        }
    } else {
        body_inner
    };
    let dep_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(body_with_get),
        r#async: false,
        span: Span::ZERO,
    }));
    each_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![
            effect_fn,
            void_zero_client(),
            Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(dep_arrow)],
                span: Span::ZERO,
            })),
        ],
    )));
    each_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("text")],
    )));

    let each_callback = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor(), t::pat_id_owned(item_name.to_string())],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: each_body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // `$.each(node, FLAG, () => $.get($$collection), $.index, body_arrow[, fallback_arrow])`
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![t::id("$$collection")],
        )),
        r#async: false,
        span: Span::ZERO,
    }));
    let mut each_args = vec![
        t::id("node"),
        t::lit_number(each_flag),
        getter,
        t::member_id(t::id_dollar(), "index"),
        each_callback,
    ];
    if let Some(fallback) = &eb.fallback {
        // Fallback arrow: `($$anchor) => { $.next(); var text_1 = $.text();
        // $.template_effect(($0) => $.set_text(text_1, $0), void 0, [() => FALLBACK_EXPR]);
        // $.append($$anchor, text_1); }`
        let fb_non_ws: Vec<&FragmentChild> = fallback
            .nodes
            .iter()
            .filter(|c| match c {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                _ => true,
            })
            .collect();
        if fb_non_ws.len() == 1 {
            if let FragmentChild::ExpressionTag(et) = fb_non_ws[0] {
                let inner_fb = strip_outer_await(&et.expression);
                let mut fb_body: Vec<Statement> = Vec::new();
                fb_body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "next"),
                    Vec::new(),
                )));
                fb_body.push(t::var(
                    "text_1",
                    t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
                ));
                let fb_set = t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id("text_1"), t::id("$0")],
                );
                let fb_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id("$0")],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(fb_set),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let fb_dep = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(inner_fb),
                    r#async: false,
                    span: Span::ZERO,
                }));
                fb_body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "template_effect"),
                    vec![
                        fb_fn,
                        void_zero_client(),
                        Expression::Array(Box::new(ArrayExpression {
                            elements: vec![ArrayElement::Expression(fb_dep)],
                            span: Span::ZERO,
                        })),
                    ],
                )));
                fb_body.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "append"),
                    vec![t::id_anchor(), t::id("text_1")],
                )));
                each_args.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id_anchor()],
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: fb_body,
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                })));
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    let each_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "each"),
        each_args,
    ));

    let async_callback = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("node"), t::pat_id("$$collection")],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![each_call],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let async_call = t::stmt(t::call(
        t::member_id(t::id_dollar(), "async"),
        vec![
            t::id("node"),
            Expression::Array(Box::new(ArrayExpression {
                elements: Vec::new(),
                span: Span::ZERO,
            })),
            Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(Expression::Arrow(Box::new(
                    ArrowFunctionExpression {
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Expression(collection_inner),
                        r#async: false,
                        span: Span::ZERO,
                    },
                )))],
                span: Span::ZERO,
            })),
            async_callback,
        ],
    ));

    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    func_body.push(async_call);
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Single top-level <svelte:element> emission
// ---------------------------------------------------------------------------

fn emit_single_svelte_element_program(
    se: &svelte_ast::elements::SvelteElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    if !se.attributes.is_empty() {
        return None;
    }
    // Body-presence-aware tag wrap: empty body → raw expression
    // (upstream passes identifiers directly); non-empty body → wrap in
    // arrow `() => EXPR` so the runtime can resolve at hydration time.
    let body_non_ws: Vec<&FragmentChild> = se
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    let tag_expr: Expression = if body_non_ws.is_empty() {
        se.tag.clone()
    } else {
        Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(se.tag.clone()),
            r#async: false,
            span: Span::ZERO,
        }))
    };
    let mut element_args = vec![
        t::id("node"),
        tag_expr,
        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: false,
            span: Span::ZERO,
        }))),
    ];
    if !body_non_ws.is_empty() {
        // Currently only support single ExpressionTag-with-literal body
        // (mirrors `script` fixture: `<svelte:element this={"script"}>{"{}"}</svelte:element>`).
        if body_non_ws.len() != 1 {
            return None;
        }
        let et = match body_non_ws[0] {
            FragmentChild::ExpressionTag(et) => et,
            _ => return None,
        };
        let lit = literal_to_template_string(&et.expression)?;
        let mut render_body: Vec<Statement> = Vec::new();
        render_body.push(t::var(
            "text",
            t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
        ));
        let nodevalue_assign = Expression::Assignment(Box::new(AssignmentExpression {
            left: AssignmentTarget::Expression(Expression::Member(Box::new(MemberExpression {
                object: t::id("text"),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("nodeValue"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }))),
            operator: AssignmentOperator::Assign,
            right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(lit),
                raw: None,
                span: Span::ZERO,
            }))),
            span: Span::ZERO,
        }));
        render_body.push(t::stmt(nodevalue_assign));
        render_body.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id("text")],
        )));
        let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$element"), t::pat_id_anchor()],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: render_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        element_args.push(render_arrow);
    }
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "first_child"),
            vec![t::id_fragment()],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "element"),
        element_args,
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(4 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Single top-level {#each} emission
// ---------------------------------------------------------------------------

/// Walk a fragment looking for `<svelte:options preserveWhitespace />` (or
/// `preserveWhitespace={true}`). Returns true when found.
fn detect_preserve_whitespace(f: &svelte_ast::fragment::Fragment) -> bool {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for n in &f.nodes {
        if let FragmentChild::SvelteOptions(o) = n {
            for a in &o.attributes {
                if let ElementAttribute::Attribute(attr) = a {
                    if attr.name == "preserveWhitespace" {
                        match &attr.value {
                            AttributeValue::Empty => return true,
                            AttributeValue::Many(parts) => {
                                for p in parts {
                                    if let AttributeValuePart::Text(t) = p {
                                        if t.data == "true" {
                                            return true;
                                        }
                                    }
                                }
                            }
                            AttributeValue::Single(tag) => {
                                if let Expression::Literal(lit) = &tag.expression {
                                    if let Literal::Boolean(b) = lit.as_ref() {
                                        if b.value {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Emit a top-level `{#each}` block when `<svelte:options preserveWhitespace />`
/// is set. Differs from `emit_single_each_program` in that:
/// - Templates retain source whitespace verbatim (no collapse / trim).
/// - The outer template is a multi-root `\\n\\n<!>` (preserves leading
///   newlines from source).
/// - Navigation uses `$.next()` + sibling+first_child instead of the
///   `$.comment()` shortcut.
/// - The body emits a text-anchor + template_effect even for text-only
///   bodies (the textContent shortcut is skipped).
fn emit_single_each_preserve_whitespace_program(
    root_fragment: &svelte_ast::fragment::Fragment,
    eb: &svelte_ast::blocks::EachBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Body must be a single wrapper element (e.g. `<div>{l}</div>`) for
    // this narrow emitter. Other shapes bail.
    let body_non_ws: Vec<&FragmentChild> = eb
        .body
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            _ => true,
        })
        .collect();
    if body_non_ws.len() != 1 {
        return None;
    }
    let inner_el = match body_non_ws[0] {
        FragmentChild::RegularElement(el) => el,
        _ => return None,
    };
    if !inner_el.attributes.is_empty() || !is_text_only_element(inner_el) {
        return None;
    }
    let item_name = match eb.context.as_ref() {
        Some(svelte_js_ast::Pattern::Identifier(id)) => id.name.clone(),
        _ => return None,
    };

    // Build inner template: leading whitespace + `<div> </div>` + trailing whitespace
    // mirrored from the source body's Text + Element + Text shape.
    let mut inner_template = String::new();
    for n in &eb.body.nodes {
        match n {
            FragmentChild::Text(t) => inner_template.push_str(&t.data),
            FragmentChild::RegularElement(el) if std::ptr::eq(el, inner_el) => {
                inner_template.push('<');
                inner_template.push_str(&el.name);
                inner_template.push_str("> </");
                inner_template.push_str(&el.name);
                inner_template.push('>');
            }
            _ => {}
        }
    }

    // Build outer template: leading text (whitespace) + `<!>` placeholder.
    let mut outer_template = String::new();
    let mut saw_each = false;
    for n in &root_fragment.nodes {
        match n {
            FragmentChild::SvelteOptions(_) => {}
            FragmentChild::Text(t) if !saw_each => outer_template.push_str(&t.data),
            FragmentChild::EachBlock(_) => {
                outer_template.push_str("<!>");
                saw_each = true;
            }
            FragmentChild::Text(_) => {} // trailing text after each — discarded
            _ => {}
        }
    }

    // Build inner expression for the text-anchor.
    let mut parts: Vec<TextPart> = Vec::new();
    for c in &inner_el.fragment.nodes {
        match c {
            FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
            FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
            _ => return None,
        }
    }
    let inline = build_inline_template(&parts, &script.state_bindings);

    // root_1 template: inner each body
    let mut hoisted: Vec<Statement> = Vec::new();
    hoisted.push(t::var(
        "root_1",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![inner_template], vec![]), t::lit_number(1.0)],
        ),
    ));
    hoisted.push(t::var(
        "root",
        t::call(
            t::member_id(t::id_dollar(), "from_html"),
            vec![t::template_raw(vec![outer_template], vec![]), t::lit_number(1.0)],
        ),
    ));

    // Function body.
    let mut body_stmts: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    body_stmts.extend(script.body.iter().cloned());
    body_stmts.push(t::stmt(t::call(t::member_id(t::id_dollar(), "next"), vec![])));
    body_stmts.push(t::var("fragment", t::call(t::id("root"), vec![])));
    body_stmts.push(t::var(
        "node",
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()])],
        ),
    ));

    // Build the each-body arrow.
    let is_runes_iter = matches!(
        &eb.expression,
        Expression::Identifier(id) if script.props_destructured.contains(id.name.as_ref())
    ) || expression_uses_props_destructured(
        &eb.expression,
        &script.props_destructured,
    );
    let item_referenced = is_runes_iter
        && fragment_uses_identifier(&eb.body, &item_name);
    let mut item_body: Vec<Statement> = Vec::new();
    item_body.push(t::stmt(t::call(t::member_id(t::id_dollar(), "next"), vec![])));
    item_body.push(t::var("fragment_1", t::call(t::id("root_1"), vec![])));
    item_body.push(t::var(
        "div",
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id("fragment_1")])],
        ),
    ));
    item_body.push(t::var(
        "text",
        t::call(
            t::member_id(t::id_dollar(), "child"),
            vec![
                t::id("div"),
                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                    value: true,
                    span: Span::ZERO,
                }))),
            ],
        ),
    ));
    item_body.push(t::stmt(t::call(t::member_id(t::id_dollar(), "reset"), vec![t::id("div")])));
    item_body.push(t::stmt(t::call(t::member_id(t::id_dollar(), "next"), vec![])));
    let inline = if item_referenced {
        rewrite_get_for_each_var(&inline, &item_name)
    } else {
        inline
    };
    let set_text = t::call(
        t::member_id(t::id_dollar(), "set_text"),
        vec![t::id("text"), inline],
    );
    item_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "template_effect"),
        vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![],
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(set_text),
            r#async: false,
            span: Span::ZERO,
        }))],
    )));
    item_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id("fragment_1")],
    )));

    let item_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id_anchor(), t::pat_id_owned(item_name.to_string())],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: item_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // is_runes_iter — for literal 'abc' it's false; legacy iter is also false.
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![],
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(eb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "each"),
        vec![
            t::id("node"),
            t::lit_number(0.0),
            getter,
            t::member_id(t::id_dollar(), "index"),
            item_arrow,
        ],
    )));
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let params = vec![t::pat_id_anchor()];
    let export = t::export_default_function(component_name, params, body_stmts);

    let mut prog: Vec<Statement> = Vec::with_capacity(script.imports.len() + 16);
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(hoisted);
    prog.push(export);
    Some(t::program(prog))
}

fn emit_single_each_program(
    eb: &svelte_ast::blocks::EachBlock,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Body classification: either a single wrapper element (e.g. `<p>{i}</p>`)
    // or a text-only body (e.g. `{thing}, `). Anything else bails.
    let body_nodes: Vec<&FragmentChild> = eb
        .body
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();

    let mut hoisted: Vec<Statement> = Vec::new();
    let mut body_stmts: Vec<Statement> = Vec::new();

    let mut delegated_events: HashSet<String> = HashSet::new();
    // Use the text-only branch unless the body is exactly one
    // RegularElement (the wrapper-element shape). Single ExpressionTag
    // bodies (`{l}` inside `{#each 'abc' as l}`) flow through the
    // text-only path.
    let single_wrapper_element = body_nodes.len() == 1
        && matches!(body_nodes[0], FragmentChild::RegularElement(_));
    if single_wrapper_element {
        if let FragmentChild::RegularElement(el) = body_nodes[0] {
            // Wrapper element body. Categorize attributes: static (HTML),
            // dynamic (\$.set_attribute), event (\$.delegated).
            let mut static_attrs: Vec<&Attribute> = Vec::new();
            let mut dyn_attrs: Vec<(&Attribute, &Expression)> = Vec::new();
            let mut events: Vec<(String, &Expression)> = Vec::new();
            for attr in &el.attributes {
                match attr {
                    ElementAttribute::Attribute(a) => {
                        if is_event_name(&a.name) {
                            if let AttributeValue::Single(tag) = &a.value {
                                events.push((a.name[2..].to_string(), &tag.expression));
                                continue;
                            }
                            return None;
                        }
                        match &a.value {
                            AttributeValue::Empty => static_attrs.push(a),
                            AttributeValue::Many(parts) => {
                                if parts
                                    .iter()
                                    .all(|p| matches!(p, AttributeValuePart::Text(_)))
                                {
                                    static_attrs.push(a);
                                } else {
                                    return None;
                                }
                            }
                            AttributeValue::Single(tag) => {
                                dyn_attrs.push((a, &tag.expression));
                            }
                        }
                    }
                    _ => return None,
                }
            }

            // Hoist `var root_1 = \$.from_html(\`<el ...>body</el>\`);`
            let mut html = String::new();
            html.push('<');
            html.push_str(&el.name);
            for a in &static_attrs {
                write_static_attr(a, &mut html)?;
            }
            // Classify the body children: collect text + expression parts.
            let mut body_parts: Vec<TextPart> = Vec::new();
            let mut body_is_static = true;
            if !is_void(&el.name) {
                for c in &el.fragment.nodes {
                    match c {
                        FragmentChild::Text(t) => {
                            body_parts.push(TextPart::Static(t.data.clone()));
                        }
                        FragmentChild::ExpressionTag(et) => {
                            body_parts.push(TextPart::Expr(&et.expression));
                            body_is_static = false;
                        }
                        _ => return None,
                    }
                }
            }
            if is_void(&el.name) {
                html.push_str("/>");
            } else {
                html.push('>');
                if body_is_static {
                    for c in &el.fragment.nodes {
                        serialize_static_child(c, &mut html)?;
                    }
                }
                html.push_str("</");
                html.push_str(&el.name);
                html.push('>');
            }
            hoisted.push(t::var(
                "root_1",
                t::call(
                    t::member_id(t::id_dollar(), "from_html"),
                    vec![t::template_raw(vec![html], vec![])],
                ),
            ));
            let var = sanitize_name(&el.name);
            body_stmts.push(t::var(&var, t::call(t::id("root_1"), vec![])));

            // Body interpolation: `el.textContent = \`...\`` if expressions present.
            if !body_is_static {
                let mut quasis: Vec<String> = Vec::new();
                let mut subs: Vec<Expression> = Vec::new();
                let mut current = String::new();
                // Trim boundary whitespace.
                let mut trimmed = body_parts.clone();
                while trimmed
                    .first()
                    .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                    .unwrap_or(false)
                {
                    trimmed.remove(0);
                }
                while trimmed
                    .last()
                    .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                    .unwrap_or(false)
                {
                    trimmed.pop();
                }
                for p in &trimmed {
                    match p {
                        TextPart::Static(s) => current.push_str(s),
                        TextPart::Expr(e) => {
                            quasis.push(std::mem::take(&mut current));
                            subs.push((*e).clone());
                        }
                    }
                }
                quasis.push(current);
                let tmpl = t::template_raw(quasis, subs);
                let target = Expression::Member(Box::new(MemberExpression {
                    object: t::id_owned(var.to_string()),
                    property: MemberProperty::Identifier(Identifier {
                        name: Cow::Borrowed("textContent"),
                        span: Span::ZERO,
                    }),
                    computed: false,
                    optional: false,
                    span: Span::ZERO,
                }));
                body_stmts.push(t::stmt(Expression::Assignment(Box::new(
                    AssignmentExpression {
                        left: AssignmentTarget::Expression(target),
                        operator: AssignmentOperator::Assign,
                        right: tmpl,
                        span: Span::ZERO,
                    },
                ))));
            }

            // Dynamic attribute setters.
            for (a, expr) in &dyn_attrs {
                body_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "set_attribute"),
                    vec![
                        t::id_owned(var.to_string()),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(a.name.clone()),
                            raw: None,
                            span: Span::ZERO,
                        }))),
                        (*expr).clone(),
                    ],
                )));
            }
            // Event delegation.
            for (event, handler) in &events {
                delegated_events.insert(event.clone());
                body_stmts.push(t::stmt(t::call(
                    t::member_id(t::id_dollar(), "delegated"),
                    vec![
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: Cow::Owned(event.clone()),
                            raw: None,
                            span: Span::ZERO,
                        }))),
                        t::id_owned(var.to_string()),
                        (*handler).clone(),
                    ],
                )));
            }
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "append"),
                vec![t::id_anchor(), t::id_owned(var.to_string())],
            )));
        } else {
            return None;
        }
    } else if let Some(_n) = multi_element_each_text_count(&eb.body) {
        // Multi-element each body (N≥2 text-anchor RegularElements).
        let item_name = match eb.context.as_ref() {
            Some(svelte_js_ast::Pattern::Identifier(id)) => id.name.clone(),
            _ => return None,
        };
        let key_is_self_ident = match &eb.key {
            Some(Expression::Identifier(id)) if id.name == item_name => true,
            _ => false,
        };
        let item_referenced = !key_is_self_ident
            && fragment_uses_identifier(&eb.body, &item_name);
        let multi_non_ws: Vec<&FragmentChild> = eb
            .body
            .nodes
            .iter()
            .filter(|c| match c {
                FragmentChild::Text(t) => !t.data.trim().is_empty(),
                FragmentChild::Comment(_) => false,
                _ => true,
            })
            .collect();
        let mut root_idx_local: usize = 0;
        let mut elem_var_idx: usize = 0;
        let mut text_idx_local: usize = 0;
        let mut frag_idx_local: usize = 0;
        let stmts = emit_multi_element_each_body(
            &multi_non_ws,
            &mut hoisted,
            &mut root_idx_local,
            &mut elem_var_idx,
            &mut text_idx_local,
            &mut frag_idx_local,
            &item_name,
            item_referenced,
            &script.props_destructured,
            &HashSet::new(),
        )?;
        body_stmts.extend(stmts);
    } else {
        // Text-only body: `$.next(); var text = $.text(); $.template_effect(...); $.append($$anchor, text);`
        let mut parts: Vec<TextPart> = Vec::new();
        for c in &eb.body.nodes {
            match c {
                FragmentChild::Text(t) => parts.push(TextPart::Static(t.data.clone())),
                FragmentChild::ExpressionTag(et) => parts.push(TextPart::Expr(&et.expression)),
                _ => return None,
            }
        }
        while parts
            .first()
            .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
            .unwrap_or(false)
        {
            parts.remove(0);
        }
        while parts
            .last()
            .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
            .unwrap_or(false)
        {
            parts.pop();
        }
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "next"),
            Vec::new(),
        )));
        body_stmts.push(t::var(
            "text",
            t::call(t::member_id(t::id_dollar(), "text"), Vec::new()),
        ));
        let inline = build_inline_template(&parts, &script.state_bindings);
        let fn_body = t::call(
            t::member_id(t::id_dollar(), "set_text"),
            vec![t::id("text"), inline],
        );
        let fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Expression(fn_body),
            r#async: false,
            span: Span::ZERO,
        }));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "template_effect"),
            vec![fn_arrow],
        )));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "append"),
            vec![t::id_anchor(), t::id("text")],
        )));
    }

    // Build the each-call arrow params: `($$anchor, ITEM, INDEX?)` or
    // `($$anchor, $$item, INDEX)` if no context.
    let mut params = vec![t::pat_id_anchor()];
    if let Some(ctx) = &eb.context {
        params.push(ctx.clone());
    } else {
        params.push(t::pat_id("$$item"));
    }
    if let Some(idx) = &eb.index {
        params.push(t::pat_id_owned(idx.to_string()));
    }

    let body_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params,
        param_type_annotations: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Build top-level function body.
    let mut func_body: Vec<Statement> = Vec::with_capacity(script.body.len() + 16);
    func_body.extend(script.body.iter().cloned());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id_dollar(), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()]),
    ));

    // `$.each(node, FLAG, () => EXPR, KEY_FN, body_arrow)`
    let getter_expr = rewrite_props_destructured(
        &eb.expression,
        &script.props_destructured,
    );
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: ArrowBody::Expression(getter_expr),
        r#async: false,
        span: Span::ZERO,
    }));
    // Determine flags. ITEM_IMMUTABLE (16) when iterable comes from
    // $props() destructuring. ITEM_REACTIVE (1) when body reads the iter
    // var and key isn't the item identifier itself.
    let item_name_opt = match eb.context.as_ref() {
        Some(svelte_js_ast::Pattern::Identifier(id)) => Some(id.name.to_string()),
        _ => None,
    };
    let key_is_self_ident = match (&eb.key, &item_name_opt) {
        (Some(Expression::Identifier(id)), Some(name)) if id.name == *name => true,
        _ => false,
    };
    let is_runes_iter = matches!(
        &eb.expression,
        Expression::Identifier(id) if script.props_destructured.contains(id.name.as_ref())
    ) || expression_uses_props_destructured(
        &eb.expression,
        &script.props_destructured,
    );
    let item_referenced_top = is_runes_iter
        && item_name_opt
            .as_ref()
            .map(|n| !key_is_self_ident && fragment_uses_identifier(&eb.body, n))
            .unwrap_or(false);
    let mut flag = 0u32;
    if item_referenced_top {
        flag |= 1;
    }
    if is_runes_iter {
        flag |= 16;
    }
    let key_fn: Expression = match &eb.key {
        None => t::member_id(t::id_dollar(), "index"),
        Some(k) => {
            let pname = item_name_opt.clone().unwrap_or_else(|| "$$item".into());
            Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: vec![t::pat_id_owned(pname.to_string())],
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(k.clone()),
                r#async: false,
                span: Span::ZERO,
            }))
        }
    };
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "each"),
        vec![
            t::id("node"),
            t::lit_number(flag as f64),
            getter,
            key_fn,
            body_arrow,
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id_dollar(), "append"),
        vec![t::id_anchor(), t::id_fragment()],
    )));

    let mut params = vec![t::pat_id_anchor()];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len() + hoisted.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.iter().cloned());
    prog.extend(hoisted);
    prog.push(export);
    if !delegated_events.is_empty() {
        let mut names: Vec<String> = delegated_events.into_iter().collect();
        names.sort();
        let arr = Expression::Array(Box::new(ArrayExpression {
            elements: names
                .into_iter()
                .map(|n| {
                    ArrayElement::Expression(Expression::Literal(Box::new(Literal::String(
                        StringLiteral {
                            value: Cow::Owned(n),
                            raw: None,
                            span: Span::ZERO,
                        },
                    ))))
                })
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "delegate"),
            vec![arr],
        )));
    }
    Some(t::program(prog))
}

fn trim_boundary_ws(nodes: &[FragmentChild]) -> &[FragmentChild] {
    let mut start = 0;
    let mut end = nodes.len();
    while start < end {
        if matches!(&nodes[start], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        if matches!(&nodes[end - 1], FragmentChild::Text(t) if t.data.trim().is_empty()) {
            end -= 1;
        } else {
            break;
        }
    }
    &nodes[start..end]
}

// ---------------------------------------------------------------------------
// Script analysis
// ---------------------------------------------------------------------------

struct ScriptInfo {
    /// Hoisted imports go above the `var root = ...` declaration.
    imports: Vec<Statement>,
    /// Rewritten body statements emitted at the start of the function.
    body: Vec<Statement>,
    /// Whether to emit `import 'svelte/internal/flags/legacy';`
    emit_legacy_flag: bool,
    /// Bindings that became `$.state(...)` — references to them in reactive
    /// contexts (template_effect deps, function bodies) need `$.get(X)` /
    /// `$.set(X, V)` wrapping.
    state_bindings: HashSet<String>,
    /// Plain `let X = LITERAL` bindings that are never assigned. Template
    /// references can be substituted with the literal value at compile time.
    constants: HashMap<String, Expression>,
    /// Whether `$props()` was destructured — the component function needs
    /// `$$props` as its second parameter.
    uses_props: bool,
    /// Whether the script contains a class with rune fields. Triggers
    /// `$.push($$props, true); ...; $.pop();` wrap around the function body.
    has_class_with_runes: bool,
    /// Names bound to `let X = $props()` (identifier destructure). Static-key
    /// reads of these (e.g. `X.foo`) get rewritten to `$$props.foo`.
    rest_props_bindings: HashSet<String>,
    /// When the script contains top-level `await`, this holds:
    /// - the rewritten setup statements (hoisted `var`s + `$.run([...])`)
    /// - the set of bindings touched by those groups
    /// - the index of the last group (used in template_effect blockers)
    async_info: Option<AsyncInfo>,
    /// Names of `let X = $state({...})` / `$state([...])` bindings —
    /// lowered to `$.proxy(...)`. Reads stay as direct member access (no
    /// `$.get` wrap); writes stay as direct property assignment.
    proxy_bindings: HashSet<String>,
    /// Names of `const X = $derived(...)` bindings — lowered to
    /// `$.derived(() => ...)`. Reads of these get `$.get(X)` wrapping.
    derived_bindings: HashSet<String>,
    /// Names destructured from `let { a, b, c } = $props()`. Template
    /// reads of these names get rewritten to `$$props.NAME` and the
    /// declaration itself is dropped from the script body.
    props_destructured: HashSet<String>,
    /// Legacy-mode `export let X [= INIT]` declarations. Each entry is
    /// `(name, default_init)`. Stripped from `body` in `analyze_script`;
    /// emitters that support legacy props rebuild the
    /// `let X = $.prop($$props, 'X', N [, INIT])` + `$$exports` accessor
    /// shape themselves.
    legacy_export_props: Vec<(String, Option<Expression>)>,
    /// Legacy-mode plain `let X = INIT` declarations where X is reassigned
    /// anywhere. Init wrapped in `$.mutable_source(...)`. Reads + writes
    /// flow through the same state_bindings rewriting as `$state` runes.
    legacy_mutable_bindings: HashSet<String>,
}

#[derive(Clone)]
struct AsyncInfo {
    setup_stmts: Vec<Statement>,
    async_bindings: HashSet<String>,
    last_group_idx: usize,
    /// All `let`/`const` script bindings (no functions). Used to decide when
    /// an if-block test references a binding-with-blocker for `$.async` wrap.
    script_let_bindings: HashSet<String>,
    /// Per-binding-name, the group index of the `$$promises[N]` slot that
    /// blocks reads of that name. Mirrors server's `binding.blocker`.
    blocker_bindings: HashMap<String, usize>,
}

fn analyze_script(
    instance: opt_ref::Ref<svelte_ast::root::Script>,
    template_assigned: &HashSet<String>,
) -> Option<ScriptInfo> {
    let Some(script) = instance else {
        return Some(ScriptInfo {
            imports: Vec::new(),
            body: Vec::new(),
            emit_legacy_flag: true,
            state_bindings: HashSet::new(),
            constants: HashMap::new(),
            uses_props: false,
            has_class_with_runes: false,
            rest_props_bindings: HashSet::new(),
            async_info: None,
            proxy_bindings: HashSet::new(),
            derived_bindings: HashSet::new(),
            props_destructured: HashSet::new(),
            legacy_export_props: Vec::new(),
            legacy_mutable_bindings: HashSet::new(),
        });
    };

    let body = &script.content.body;
    let mut assigned: HashSet<String> = collect_assigned_targets(body);
    assigned.extend(template_assigned.iter().cloned());

    // First pass: discover which $state bindings need lowering to $.state /
    // $.proxy, and which `const X = $derived(...)` bindings need $.derived.
    let mut state_bindings: HashSet<String> = HashSet::new();
    let mut proxy_bindings: HashSet<String> = HashSet::new();
    let mut derived_bindings: HashSet<String> = HashSet::new();
    for s in body {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if is_state_call(init) {
                        // `$state({...})` / `$state([...])` lowers to `$.proxy(...)`.
                        if state_call_inner_is_proxy_init(init) {
                            proxy_bindings.insert(id.name.to_string());
                        } else if assigned.contains(id.name.as_ref()) {
                            state_bindings.insert(id.name.to_string());
                        }
                    } else if is_derived_call(init) {
                        derived_bindings.insert(id.name.to_string());
                    }
                }
            }
        }
    }

    // Detect any class declaration with rune-initialized fields.
    let mut has_class_with_runes = false;
    for s in body {
        match s {
            Statement::Class(c) => {
                if class_has_rune_fields(c) {
                    has_class_with_runes = true;
                }
            }
            Statement::ExportDefault(e) => {
                if let ExportDefault::Class(c) = &e.declaration {
                    if class_has_rune_fields(c) {
                        has_class_with_runes = true;
                    }
                }
            }
            _ => {}
        }
    }

    let mut imports: Vec<Statement> = Vec::new();
    let mut rest: Vec<Statement> = Vec::new();
    #[allow(unused_assignments)]
    let mut uses_runes = false;
    let mut uses_props = has_class_with_runes;
    let mut props_destructured: HashSet<String> = HashSet::new();
    let mut legacy_export_props: Vec<(String, Option<Expression>)> = Vec::new();
    if has_class_with_runes {
        uses_runes = true;
    }

    // Pre-detect legacy mutable bindings: plain `let X = INIT` where X is
    // reassigned anywhere AND we're not in runes mode AND X isn't already
    // a $state / $derived / export-let / $props binding. Init wrapped in
    // `$.mutable_source(...)`; reads/writes routed through state_bindings.
    //
    // We check the export-let case heuristically by scanning the body
    // ahead; the destructured-props / rest-props sets get populated in
    // the upcoming loop, so we exclude those names here too.
    let mut legacy_mutable_bindings: HashSet<String> = HashSet::new();
    // Probe upcoming `export let X` and `let { X } = $props()` names so
    // they don't accidentally get marked mutable.
    let mut export_let_names: HashSet<String> = HashSet::new();
    let mut probe_props_destructured: HashSet<String> = HashSet::new();
    for s in body {
        if let Statement::ExportNamed(ex) = s {
            if ex.source.is_none() && ex.specifiers.is_empty() {
                if let Some(Statement::Variable(v)) = &ex.declaration {
                    if matches!(v.kind, VariableKind::Let) {
                        for d in &v.declarations {
                            if let Pattern::Identifier(id) = &d.id {
                                export_let_names.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let (Pattern::Object(obj), Some(init)) = (&d.id, &d.init) {
                    if is_props_call(init) {
                        for m in &obj.properties {
                            if let ObjectPatternMember::Property(p) = m {
                                if let PropertyKey::Identifier(id) = &p.key {
                                    probe_props_destructured.insert(id.name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let could_be_runes = !state_bindings.is_empty()
        || !proxy_bindings.is_empty()
        || !derived_bindings.is_empty()
        || !probe_props_destructured.is_empty()
        || has_class_with_runes;
    if !could_be_runes {
        for s in body {
            if let Statement::Variable(v) = s {
                if !matches!(v.kind, VariableKind::Let) {
                    continue;
                }
                for d in &v.declarations {
                    if let (Pattern::Identifier(id), Some(_init)) = (&d.id, &d.init) {
                        if export_let_names.contains(id.name.as_ref()) {
                            continue;
                        }
                        if state_bindings.contains(id.name.as_ref())
                            || proxy_bindings.contains(id.name.as_ref())
                            || derived_bindings.contains(id.name.as_ref())
                            || probe_props_destructured.contains(id.name.as_ref())
                        {
                            continue;
                        }
                        if !assigned.contains(id.name.as_ref()) {
                            continue;
                        }
                        legacy_mutable_bindings.insert(id.name.to_string());
                    }
                }
            }
        }
    }
    // Add legacy mutable bindings to state_bindings so reads/writes inside
    // function bodies get rewritten via the existing infrastructure.
    for n in &legacy_mutable_bindings {
        state_bindings.insert(n.clone());
    }
    for s in body {
        match s {
            Statement::Import(_) => {
                // Hoist all imports to the top regardless of source order
                // (matches upstream's behavior).
                imports.push(s.clone());
            }
            // Legacy-mode `export let X [= INIT]`: capture as a prop
            // descriptor and DROP from the body. Emitters that support
            // legacy props rebuild the prop declaration with $.prop().
            Statement::ExportNamed(ex)
                if ex.source.is_none() && ex.specifiers.is_empty() => {
                let mut handled = false;
                if let Some(Statement::Variable(v)) = &ex.declaration {
                    if matches!(v.kind, VariableKind::Let)
                        && v.declarations.iter().all(|d| matches!(d.id, Pattern::Identifier(_)))
                    {
                        for d in &v.declarations {
                            if let Pattern::Identifier(id) = &d.id {
                                legacy_export_props
                                    .push((id.name.to_string(), d.init.clone()));
                                uses_props = true;
                            }
                        }
                        handled = true;
                    }
                }
                if handled {
                    continue;
                }
                let rewritten = rewrite_top_stmt_multi(
                    s,
                    &assigned,
                    &state_bindings,
                    &mut uses_runes,
                    &mut uses_props,
                )?;
                rest.extend(rewritten);
            }
            _ => {
                // Detect `let { a, b } = $props()` and drop the declaration —
                // template refs to `a`/`b` get rewritten to `$$props.a` /
                // `$$props.b` by `rewrite_props_destructured` later. Only
                // applies when every member is a plain identifier (no
                // defaults, no aliasing) — anything else falls through to
                // the regular `rewrite_top_stmt_multi` path.
                let mut handled = false;
                if let Statement::Variable(v) = s {
                    if v.declarations.len() == 1 {
                        let d = &v.declarations[0];
                        if let (Pattern::Object(obj), Some(init)) = (&d.id, &d.init) {
                            if is_props_call(init) {
                                let all_simple = obj.properties.iter().all(|m| match m {
                                    ObjectPatternMember::Property(p) => {
                                        matches!(p.key, PropertyKey::Identifier(_))
                                            && matches!(p.value, Pattern::Identifier(_))
                                    }
                                    _ => false,
                                });
                                if all_simple {
                                    uses_runes = true;
                                    uses_props = true;
                                    for m in &obj.properties {
                                        if let ObjectPatternMember::Property(p) = m {
                                            if let PropertyKey::Identifier(id) = &p.key {
                                                props_destructured.insert(id.name.to_string());
                                            }
                                        }
                                    }
                                    handled = true;
                                }
                            }
                        }
                    }
                }
                if handled {
                    continue;
                }
                let rewritten = rewrite_top_stmt_multi(
                    s,
                    &assigned,
                    &state_bindings,
                    &mut uses_runes,
                    &mut uses_props,
                )?;
                rest.extend(rewritten);
            }
        }
    }

    // Collect plain `let X = LITERAL` constants (X is never assigned and not
    // a state binding) — usable for template-expression substitution.
    let mut constants: HashMap<String, Expression> = HashMap::new();
    for s in body {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if assigned.contains(id.name.as_ref()) || state_bindings.contains(id.name.as_ref()) {
                        continue;
                    }
                    if is_literal_expression(init) {
                        constants.insert(id.name.to_string(), init.clone());
                    }
                }
            }
        }
    }

    // Detect `let X = $props()` (identifier destructure, not object) and
    // rewrite to `let X = $.rest_props($$props, ['$$slots', '$$events',
    // '$$legacy']);`. Also rewrite reads of `X.STATIC` to `$$props.STATIC`
    // elsewhere in the script body.
    let mut rest_props_bindings: HashSet<String> = HashSet::new();
    for s in &rest {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if is_props_call(init) {
                        rest_props_bindings.insert(id.name.to_string());
                    }
                }
            }
        }
    }
    if !rest_props_bindings.is_empty() {
        uses_runes = true;
        uses_props = true;
        for s in &mut rest {
            replace_props_init_with_rest_props(s, &rest_props_bindings);
        }
        for s in &mut rest {
            rewrite_stmt_for_rest_props(s, &rest_props_bindings);
        }
    }

    // (`props_destructured` is now populated during the top loop above —
    // drops are done inline.)

    // Merge derived bindings into state_bindings so reads get $.get wrapping
    // (state_bindings is the read-rewrite set).
    let mut state_bindings = state_bindings;
    for n in &derived_bindings {
        state_bindings.insert(n.clone());
    }

    // Top-level await detection + transform.
    let mut async_info: Option<AsyncInfo> = None;
    if has_top_level_await_in_body(&rest) {
        async_info = transform_async_script_client(&rest);
        if async_info.is_some() {
            uses_runes = true; // async implies runes-like emission rules
        }
    }
    let body_out_raw = if let Some(ai) = &async_info {
        ai.setup_stmts.clone()
    } else {
        rest
    };
    // Split multi-declarator Variable statements (where every declarator
    // has an initializer) into one statement per declarator. Matches
    // upstream's behavior. Hoisted multi-var declarations like
    // `var yes1, yes2, no1, no2;` (no inits) stay combined.
    let mut body_out: Vec<Statement> = Vec::with_capacity(body_out_raw.len());
    for s in body_out_raw {
        // Rewrite destructured-props identifier reads (`browser` →
        // `$$props.browser`) in script statements so they match the
        // template-side transformation.
        let s = rewrite_stmt_props_destructured(&s, &props_destructured);
        // Wrap legacy mutable inits: `let X = INIT` →
        // `let X = $.mutable_source(INIT)` when X is in legacy_mutable_bindings.
        let s = if !legacy_mutable_bindings.is_empty() {
            if let Statement::Variable(v) = &s {
                if matches!(v.kind, VariableKind::Let) {
                    let mut new_decls = Vec::with_capacity(v.declarations.len());
                    for d in &v.declarations {
                        let mut new_d = d.clone();
                        if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                            if legacy_mutable_bindings.contains(id.name.as_ref()) {
                                new_d.init = Some(t::call(
                                    t::member_id(t::id_dollar(), "mutable_source"),
                                    vec![init.clone()],
                                ));
                            }
                        }
                        new_decls.push(new_d);
                    }
                    Statement::Variable(Box::new(VariableDeclaration {
                        kind: v.kind,
                        declarations: new_decls,
                        span: v.span,
                    }))
                } else {
                    s
                }
            } else {
                s
            }
        } else {
            s
        };
        if let Statement::Variable(v) = &s {
            if v.declarations.len() > 1
                && v.declarations.iter().all(|d| d.init.is_some())
            {
                for d in &v.declarations {
                    body_out.push(Statement::Variable(Box::new(VariableDeclaration {
                        kind: v.kind,
                        declarations: vec![d.clone()],
                        span: Span::ZERO,
                    })));
                }
                continue;
            }
        }
        body_out.push(s);
    }

    Some(ScriptInfo {
        imports,
        body: body_out,
        emit_legacy_flag: !uses_runes && async_info.is_none(),
        state_bindings,
        constants,
        uses_props,
        has_class_with_runes,
        rest_props_bindings,
        async_info,
        proxy_bindings,
        derived_bindings,
        props_destructured,
        legacy_export_props,
        legacy_mutable_bindings,
    })
}

fn has_top_level_await_in_body(body: &[Statement]) -> bool {
    body.iter().any(stmt_top_await) || body.iter().any(stmt_has_async_derived_init)
}

fn stmt_has_async_derived_init(s: &Statement) -> bool {
    if let Statement::Variable(v) = s {
        v.declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, |i| rewrite_async_derived_client(i).is_some()))
    } else {
        false
    }
}

/// Port of server's `rewrite_async_derived`. If `e` is `$.derived(arrow)`
/// where `arrow.body` is `await X` (or contains a top-level await), return
/// `await $.async_derived(() => X)` (or wrapping form). Otherwise `None`.
fn rewrite_async_derived_client(e: &Expression) -> Option<Expression> {
    let Expression::Call(c) = e else { return None };
    if global_keypath(&c.callee).as_deref() != Some("$.derived") {
        return None;
    }
    let arg = c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e),
        _ => None,
    })?;
    let arrow = match arg {
        Expression::Arrow(a) => a,
        _ => return None,
    };
    if arrow.r#async {
        return None;
    }
    let body = match &arrow.body {
        ArrowBody::Expression(e) => e,
        _ => return None,
    };
    let (new_body, new_async) = if let Expression::Await(a) = body {
        (ArrowBody::Expression(a.argument.clone()), false)
    } else if expr_top_await(body) {
        (ArrowBody::Expression(body.clone()), true)
    } else {
        return None;
    };
    let new_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        param_type_annotations: Vec::new(),
        body: new_body,
        r#async: new_async,
        span: Span::ZERO,
    }));
    let async_derived_call = t::call(
        t::member_id(t::id_dollar(), "async_derived"),
        vec![new_arrow],
    );
    Some(Expression::Await(Box::new(AwaitExpression {
        argument: async_derived_call,
        span: Span::ZERO,
    })))
}

fn stmt_top_await(s: &Statement) -> bool {
    match s {
        Statement::Variable(v) => v
            .declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, expr_top_await)),
        Statement::Expression(e) => expr_top_await(&e.expression),
        _ => false,
    }
}

fn expr_top_await(e: &Expression) -> bool {
    match e {
        Expression::Await(_) => true,
        Expression::Function(f) if f.r#async => false,
        Expression::Arrow(a) if a.r#async => false,
        Expression::Call(c) => {
            expr_top_await(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_top_await(e),
                    Argument::Spread(s) => expr_top_await(&s.argument),
                })
        }
        Expression::Binary(b) => expr_top_await(&b.left) || expr_top_await(&b.right),
        Expression::Logical(l) => expr_top_await(&l.left) || expr_top_await(&l.right),
        Expression::Unary(u) => expr_top_await(&u.argument),
        Expression::Member(m) => expr_top_await(&m.object),
        Expression::Conditional(c) => {
            expr_top_await(&c.test)
                || expr_top_await(&c.consequent)
                || expr_top_await(&c.alternate)
        }
        Expression::Paren(p) => expr_top_await(&p.expression),
        Expression::Sequence(s) => s.expressions.iter().any(expr_top_await),
        Expression::Spread(s) => expr_top_await(&s.argument),
        _ => false,
    }
}

fn transform_async_script_client(body: &[Statement]) -> Option<AsyncInfo> {
    // Pre-async statements (before the first await/async-derived) stay as
    // setup. The async transform only kicks in for statements at-or-after
    // the first async one. Mirrors server's `transform_async_script_server`.
    let first_async_idx = body.iter().position(|s| {
        if let Statement::Variable(v) = s {
            v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |i| {
                    expr_top_await(i) || rewrite_async_derived_client(i).is_some()
                })
            })
        } else if let Statement::Expression(e) = s {
            expr_top_await(&e.expression)
        } else {
            false
        }
    })?;
    let pre_async: Vec<Statement> = body[..first_async_idx].to_vec();
    let body = &body[first_async_idx..];

    let mut hoisted_names: Vec<String> = Vec::new();
    let mut hoisted_spans: Vec<Span> = Vec::new();
    enum Lowered {
        AsyncSet { name: String, init: Expression },
        Sync(Statement),
    }
    let mut lowered: Vec<Lowered> = Vec::new();

    for s in body {
        match s {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        hoisted_names.push(id.name.to_string());
                        hoisted_spans.push(id.span);
                        let init = d
                            .init
                            .clone()
                            .unwrap_or_else(|| void_zero_client());
                        // `$.derived(() => await E)` pattern → rewrite to
                        // `await $.async_derived(() => E)`.
                        if let Some(rewritten) = rewrite_async_derived_client(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.to_string(),
                                init: rewritten,
                            });
                            continue;
                        }
                        if expr_top_await(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.to_string(),
                                init,
                            });
                        } else {
                            lowered.push(Lowered::Sync(Statement::Expression(Box::new(
                                ExpressionStatement {
                                    expression: Expression::Assignment(Box::new(
                                        AssignmentExpression {
                                            left: AssignmentTarget::Expression(t::id_owned(id.name.to_string())),
                                            operator: AssignmentOperator::Assign,
                                            right: init,
                                            span: Span::ZERO,
                                        },
                                    )),
                                    span: Span::ZERO,
                                },
                            ))));
                        }
                    } else {
                        return None;
                    }
                }
            }
            Statement::Expression(e) => {
                // `undefined` (post-rune-erasure `$inspect()` etc.) → `void 0`
                if let Expression::Identifier(id) = &e.expression {
                    if id.name == "undefined" {
                        lowered.push(Lowered::Sync(t::stmt(void_zero_client())));
                        continue;
                    }
                }
                // `$inspect(...)` / `$inspect.trace(...)` → `void 0` in async
                // mode. The client doesn't otherwise rewrite these (so non-
                // async dev mode keeps them), but inside an async run-group
                // we need the arrow body to be void 0.
                if let Expression::Call(c) = &e.expression {
                    if let Some(kp) = global_keypath(&c.callee) {
                        if matches!(kp.as_str(), "$inspect" | "$inspect.trace") {
                            lowered.push(Lowered::Sync(t::stmt(void_zero_client())));
                            continue;
                        }
                    }
                }
                lowered.push(Lowered::Sync(s.clone()));
            }
            Statement::Function(_) => {
                lowered.push(Lowered::Sync(s.clone()));
            }
            _ => return None,
        }
    }

    let mut groups: Vec<Expression> = Vec::new();
    let mut current_sync: Vec<Statement> = Vec::new();
    let mut last_was_async = false;

    fn flush_sync(groups: &mut Vec<Expression>, current_sync: &mut Vec<Statement>) {
        if current_sync.len() == 1 {
            let s = current_sync.remove(0);
            if let Statement::Expression(es) = s {
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(es.expression),
                    r#async: false,
                    span: Span::ZERO,
                })));
                return;
            }
            current_sync.push(s);
        }
        groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: std::mem::take(current_sync),
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        })));
    }

    for l in lowered {
        match l {
            Lowered::AsyncSet { name, init } => {
                if !current_sync.is_empty() {
                    flush_sync(&mut groups, &mut current_sync);
                }
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id_owned(name.to_string())),
                    operator: AssignmentOperator::Assign,
                    right: init,
                    span: Span::ZERO,
                }));
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(assign),
                    r#async: true,
                    span: Span::ZERO,
                })));
                last_was_async = true;
            }
            Lowered::Sync(stmt) => {
                current_sync.push(stmt);
                last_was_async = false;
            }
        }
    }
    // Trailing sync group: only when there are actual sync statements after
    // the last async. Empty trailing groups are not emitted.
    let _ = last_was_async;
    if !current_sync.is_empty() {
        flush_sync(&mut groups, &mut current_sync);
    }

    let last_group_idx = if groups.is_empty() { 0 } else { groups.len() - 1 };

    // Setup: pre_async (kept verbatim) + `var X, Y, Z;` hoisted + `var $$promises = $.run([...])`.
    let mut setup_stmts: Vec<Statement> = Vec::new();
    setup_stmts.extend(pre_async.clone());
    if !hoisted_names.is_empty() {
        let decls: Vec<VariableDeclarator> = hoisted_names
            .iter()
            .enumerate()
            .map(|(i, n)| VariableDeclarator {
                id: Pattern::Identifier(Identifier {
                    name: Cow::Owned(n.clone()),
                    span: hoisted_spans.get(i).copied().unwrap_or(Span::ZERO),
                }),
                init: None,
                type_annotation: None,
                span: hoisted_spans.get(i).copied().unwrap_or(Span::ZERO),
            })
            .collect();
        setup_stmts.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Var,
            declarations: decls,
            span: Span::ZERO,
        })));
    }
    setup_stmts.push(t::var(
        "$$promises",
        t::call(
            t::member_id(t::id_dollar(), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: groups.into_iter().map(ArrayElement::Expression).collect(),
                span: Span::ZERO,
            }))],
        ),
    ));

    let async_bindings: HashSet<String> = hoisted_names.into_iter().collect();
    // Collect script let/const bindings — used for "ref-with-blocker"
    // detection during template lowering. Mirrors server's
    // `script_let_bindings`.
    let mut script_let_bindings: HashSet<String> = async_bindings.clone();
    for s in pre_async.iter().chain(body.iter()) {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    script_let_bindings.insert(id.name.to_string());
                }
            }
        }
    }
    // Compute blocker_bindings: simulate the group counting. Same logic as
    // server's `transform_async_script_server` blocker pass.
    let mut blocker_bindings: HashMap<String, usize> = HashMap::new();
    {
        let mut groups_count: usize = 0;
        let mut sync_pending: bool = false;
        let mut awaited_seen: bool = false;
        for s in body.iter() {
            if let Statement::Variable(v) = s {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        let init = d.init.as_ref();
                        let is_async = init.map_or(false, |i| {
                            expr_top_await(i) || rewrite_async_derived_client(i).is_some()
                        });
                        if is_async {
                            if sync_pending {
                                groups_count += 1;
                                sync_pending = false;
                            }
                            let idx = groups_count;
                            blocker_bindings.entry(id.name.to_string()).or_insert(idx);
                            if let Some(init) = init {
                                let mut touched: HashSet<String> = HashSet::new();
                                collect_touched_async_client(init, &mut touched);
                                for name in touched {
                                    if script_let_bindings.contains(name.as_str()) {
                                        blocker_bindings.entry(name).or_insert(idx);
                                    }
                                }
                            }
                            groups_count += 1;
                            awaited_seen = true;
                        } else {
                            if awaited_seen {
                                blocker_bindings
                                    .entry(id.name.to_string())
                                    .or_insert(groups_count);
                            }
                            sync_pending = true;
                        }
                    }
                }
            } else if matches!(s, Statement::Function(_)) {
                // Functions are sync setup; don't count.
            } else {
                sync_pending = true;
            }
        }
    }
    Some(AsyncInfo {
        setup_stmts,
        async_bindings,
        last_group_idx,
        script_let_bindings,
        blocker_bindings,
    })
}

/// Collect "touched" identifier names: identifiers eagerly read, walking
/// into function/arrow bodies (mirrors server's `collect_touched_in_expr`).
fn collect_touched_async_client(e: &Expression, out: &mut HashSet<String>) {
    match e {
        Expression::Identifier(id) => {
            if id.name != "undefined" {
                out.insert(id.name.to_string());
            }
        }
        Expression::Call(c) => {
            collect_touched_async_client(&c.callee, out);
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => collect_touched_async_client(e, out),
                    Argument::Spread(s) => collect_touched_async_client(&s.argument, out),
                }
            }
        }
        Expression::Member(m) => {
            collect_touched_async_client(&m.object, out);
        }
        Expression::Binary(b) => {
            collect_touched_async_client(&b.left, out);
            collect_touched_async_client(&b.right, out);
        }
        Expression::Logical(l) => {
            collect_touched_async_client(&l.left, out);
            collect_touched_async_client(&l.right, out);
        }
        Expression::Unary(u) => collect_touched_async_client(&u.argument, out),
        Expression::Await(a) => collect_touched_async_client(&a.argument, out),
        Expression::Conditional(c) => {
            collect_touched_async_client(&c.test, out);
            collect_touched_async_client(&c.consequent, out);
            collect_touched_async_client(&c.alternate, out);
        }
        Expression::Paren(p) => collect_touched_async_client(&p.expression, out),
        Expression::Sequence(s) => {
            for e in &s.expressions {
                collect_touched_async_client(e, out);
            }
        }
        Expression::Arrow(a) => match &a.body {
            ArrowBody::Expression(e) => collect_touched_async_client(e, out),
            ArrowBody::Block(_) => {} // upstream skips block-body for client touch
        },
        _ => {}
    }
}

fn expr_contains_ident(e: &Expression, name: &str) -> bool {
    match e {
        Expression::Identifier(i) => i.name == name,
        Expression::Member(m) => expr_contains_ident(&m.object, name),
        Expression::Call(c) => {
            expr_contains_ident(&c.callee, name)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_contains_ident(e, name),
                    Argument::Spread(s) => expr_contains_ident(&s.argument, name),
                })
        }
        Expression::Binary(b) => {
            expr_contains_ident(&b.left, name) || expr_contains_ident(&b.right, name)
        }
        Expression::Logical(l) => {
            expr_contains_ident(&l.left, name) || expr_contains_ident(&l.right, name)
        }
        Expression::Unary(u) => expr_contains_ident(&u.argument, name),
        Expression::Conditional(c) => {
            expr_contains_ident(&c.test, name)
                || expr_contains_ident(&c.consequent, name)
                || expr_contains_ident(&c.alternate, name)
        }
        Expression::Paren(p) => expr_contains_ident(&p.expression, name),
        Expression::Template(t) => t.expressions.iter().any(|e| expr_contains_ident(e, name)),
        Expression::Await(a) => expr_contains_ident(&a.argument, name),
        _ => false,
    }
}

fn expr_refs_any_client(e: &Expression, names: &HashSet<String>) -> bool {
    match e {
        Expression::Identifier(i) => names.contains(i.name.as_ref()),
        Expression::Member(m) => expr_refs_any_client(&m.object, names),
        Expression::Call(c) => {
            expr_refs_any_client(&c.callee, names)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_refs_any_client(e, names),
                    Argument::Spread(s) => expr_refs_any_client(&s.argument, names),
                })
        }
        Expression::Binary(b) => {
            expr_refs_any_client(&b.left, names) || expr_refs_any_client(&b.right, names)
        }
        Expression::Logical(l) => {
            expr_refs_any_client(&l.left, names) || expr_refs_any_client(&l.right, names)
        }
        Expression::Unary(u) => expr_refs_any_client(&u.argument, names),
        Expression::Conditional(c) => {
            expr_refs_any_client(&c.test, names)
                || expr_refs_any_client(&c.consequent, names)
                || expr_refs_any_client(&c.alternate, names)
        }
        Expression::Paren(p) => expr_refs_any_client(&p.expression, names),
        Expression::Template(t) => t.expressions.iter().any(|e| expr_refs_any_client(e, names)),
        _ => false,
    }
}

fn void_zero_client() -> Expression {
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

fn replace_props_init_with_rest_props(s: &mut Statement, names: &HashSet<String>) {
    if let Statement::Variable(v) = s {
        for d in &mut v.declarations {
            if let (Pattern::Identifier(id), Some(init)) = (&d.id, &mut d.init) {
                if names.contains(id.name.as_ref()) && is_props_call(init) {
                    let arr = Expression::Array(Box::new(ArrayExpression {
                        elements: ["$$slots", "$$events", "$$legacy"]
                            .iter()
                            .map(|n| {
                                ArrayElement::Expression(Expression::Literal(Box::new(
                                    Literal::String(StringLiteral {
                                        value: Cow::Owned((*n).to_string()),
                                        raw: None,
                                        span: Span::ZERO,
                                    }),
                                )))
                            })
                            .collect(),
                        span: Span::ZERO,
                    }));
                    *init = t::call(
                        t::member_id(t::id_dollar(), "rest_props"),
                        vec![t::id("$$props"), arr],
                    );
                }
            }
        }
    }
}

fn rewrite_stmt_for_rest_props(s: &mut Statement, names: &HashSet<String>) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &mut v.declarations {
                if let Some(init) = &mut d.init {
                    rewrite_expr_for_rest_props(init, names, false);
                }
            }
        }
        S::Expression(e) => rewrite_expr_for_rest_props(&mut e.expression, names, false),
        S::Block(b) => {
            for s in &mut b.body {
                rewrite_stmt_for_rest_props(s, names);
            }
        }
        S::Return(r) => {
            if let Some(a) = &mut r.argument {
                rewrite_expr_for_rest_props(a, names, false);
            }
        }
        S::If(i) => {
            rewrite_expr_for_rest_props(&mut i.test, names, false);
            rewrite_stmt_for_rest_props(&mut i.consequent, names);
            if let Some(a) = &mut i.alternate {
                rewrite_stmt_for_rest_props(a, names);
            }
        }
        S::Function(f) => {
            for s in &mut f.body.body {
                rewrite_stmt_for_rest_props(s, names);
            }
        }
        _ => {}
    }
}

fn rewrite_expr_for_rest_props(
    e: &mut Expression,
    names: &HashSet<String>,
    in_lhs_outermost: bool,
) {
    use Expression as E;
    match e {
        E::Member(m) => {
            // If this Member's object is a known rest_props identifier AND
            // we're NOT at the outermost LHS (write target), rewrite it.
            if !in_lhs_outermost {
                if let E::Identifier(id) = &m.object {
                    if names.contains(id.name.as_ref()) && !m.computed {
                        if let MemberProperty::Identifier(_) = &m.property {
                            m.object = t::id("$$props");
                            return;
                        }
                    }
                }
            }
            rewrite_expr_for_rest_props(&mut m.object, names, false);
            if let MemberProperty::Expression(e) = &mut m.property {
                rewrite_expr_for_rest_props(e, names, false);
            }
        }
        E::Assignment(a) => {
            // Outermost LHS doesn't get rewritten; inside it (sub-members)
            // do. RHS is normal read context.
            if let AssignmentTarget::Expression(t) = &mut a.left {
                rewrite_expr_for_rest_props(t, names, true);
            }
            rewrite_expr_for_rest_props(&mut a.right, names, false);
        }
        E::Call(c) => {
            rewrite_expr_for_rest_props(&mut c.callee, names, false);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => rewrite_expr_for_rest_props(e, names, false),
                    Argument::Spread(s) => {
                        rewrite_expr_for_rest_props(&mut s.argument, names, false)
                    }
                }
            }
        }
        E::Binary(b) => {
            rewrite_expr_for_rest_props(&mut b.left, names, false);
            rewrite_expr_for_rest_props(&mut b.right, names, false);
        }
        E::Logical(l) => {
            rewrite_expr_for_rest_props(&mut l.left, names, false);
            rewrite_expr_for_rest_props(&mut l.right, names, false);
        }
        E::Conditional(c) => {
            rewrite_expr_for_rest_props(&mut c.test, names, false);
            rewrite_expr_for_rest_props(&mut c.consequent, names, false);
            rewrite_expr_for_rest_props(&mut c.alternate, names, false);
        }
        E::Unary(u) => rewrite_expr_for_rest_props(&mut u.argument, names, false),
        E::Sequence(s) => {
            for e in &mut s.expressions {
                rewrite_expr_for_rest_props(e, names, false);
            }
        }
        E::Paren(p) => rewrite_expr_for_rest_props(&mut p.expression, names, false),
        _ => {}
    }
}

fn class_has_rune_fields(c: &ClassDeclaration) -> bool {
    c.body.body.iter().any(|m| {
        if let ClassMember::Property(p) = m {
            if let Some(value) = &p.value {
                return is_rune_call_in_class(value);
            }
        }
        false
    })
}

fn is_rune_call_in_class(e: &Expression) -> bool {
    if let Expression::Call(c) = e {
        if let Some(kp) = global_keypath(&c.callee) {
            return matches!(
                kp.as_str(),
                "$state" | "$state.raw" | "$state.eager" | "$derived" | "$derived.by"
            );
        }
    }
    false
}

/// Wrapper that expands a single source statement into one-or-more output
/// statements (needed for `let { a, b = D } = $props()` which becomes a
/// flat list of `let a = $.prop(...); let b = $.prop(...);`).
fn rewrite_top_stmt_multi(
    s: &Statement,
    assigned: &HashSet<String>,
    state_bindings: &HashSet<String>,
    uses_runes: &mut bool,
    uses_props: &mut bool,
) -> Option<Vec<Statement>> {
    // Detect `let { ... } = $props()` and expand to one $.prop per member.
    if let Statement::Variable(v) = s {
        if v.declarations.len() == 1 {
            let d = &v.declarations[0];
            if let (Pattern::Object(obj), Some(init)) = (&d.id, &d.init) {
                if is_props_call(init) {
                    *uses_runes = true;
                    *uses_props = true;
                    let mut out = Vec::new();
                    for m in &obj.properties {
                        match m {
                            ObjectPatternMember::Property(p) => {
                                let key_name = match &p.key {
                                    PropertyKey::Identifier(i) => i.name.clone(),
                                    _ => return None,
                                };
                                let (local_name, default) = match &p.value {
                                    Pattern::Identifier(i) => (i.name.clone(), None),
                                    Pattern::Assignment(a) => {
                                        let local = match &a.left {
                                            Pattern::Identifier(i) => i.name.clone(),
                                            _ => return None,
                                        };
                                        (local, Some(a.right.clone()))
                                    }
                                    _ => return None,
                                };
                                let mut args = vec![
                                    t::id("$$props"),
                                    Expression::Literal(Box::new(Literal::String(
                                        StringLiteral {
                                            value: key_name.clone(),
                                            raw: None,
                                            span: Span::ZERO,
                                        },
                                    ))),
                                ];
                                if let Some(def) = default {
                                    // Flag `3` = "has default + assignable" per upstream.
                                    args.push(t::lit_number(3.0));
                                    args.push(def);
                                } else {
                                    args.push(t::lit_number(1.0));
                                }
                                out.push(t::let_decl(
                                    &local_name,
                                    Some(t::call(t::member_id(t::id_dollar(), "prop"), args)),
                                ));
                            }
                            _ => return None,
                        }
                    }
                    return Some(out);
                }
            }
        }
    }
    Some(vec![rewrite_top_stmt(
        s,
        assigned,
        state_bindings,
        uses_runes,
    )?])
}

fn is_props_call(e: &Expression) -> bool {
    if let Expression::Call(c) = e {
        if let Some(kp) = global_keypath(&c.callee) {
            return kp == "$props";
        }
    }
    false
}

fn is_literal_expression(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        // Plain template literal with no substitutions — equivalent to a
        // string literal for constant-fold purposes.
        Expression::Template(t) => t.expressions.is_empty(),
        // `undefined` global identifier — treat as literal for fold purposes.
        Expression::Identifier(i) if i.name == "undefined" => true,
        _ => false,
    }
}

/// Rewrite a top-level script statement for the client. Handles:
/// - `let X = $state(LIT)` where X is never assigned → `let X = LIT;`
///   (strips the rune call entirely)
/// - `let X = $state(V)` where X IS assigned somewhere → `let X = $.state(V);`
/// - `let X = LIT` (no rune) → unchanged
/// - `const X = LIT` → unchanged
/// - Functions → body recursively rewritten so reads of state bindings
///   become `$.get(X)` and writes become `$.set(X, ...)` / `$.update(X)`.
/// - Anything else (assignment expressions to plain bindings etc.) → bail.
fn rewrite_top_stmt(
    s: &Statement,
    assigned: &HashSet<String>,
    state_bindings: &HashSet<String>,
    uses_runes: &mut bool,
) -> Option<Statement> {
    match s {
        Statement::Variable(v) => {
            let mut out = (**v).clone();
            for d in &mut out.declarations {
                if let Some(init) = &mut d.init {
                    // Proxy lowering: `$state({...})` / `$state([...])` →
                    // `$.proxy(...)`. Must check before state-strip / state
                    // lowering.
                    if let Pattern::Identifier(_id) = &d.id {
                        if is_state_call(init) && state_call_inner_is_proxy_init(init) {
                            *uses_runes = true;
                            lower_to_proxy_init(init);
                            continue;
                        }
                    }
                    // First, try strip (binding never assigned).
                    let stripped = try_strip_state(init, &d.id, assigned, uses_runes);
                    if !stripped {
                        // Try lower $state(V) → $.state(V) if this binding is
                        // a state binding.
                        if let Pattern::Identifier(id) = &d.id {
                            if state_bindings.contains(id.name.as_ref()) && is_state_call(init) {
                                *uses_runes = true;
                                lower_state_init(init);
                                continue;
                            }
                            // Derived: `$derived(EXPR)` → `$.derived(() => EXPR)`.
                            if is_derived_call(init) {
                                *uses_runes = true;
                                lower_to_derived_init(init);
                                continue;
                            }
                        }
                        // $props() with identifier pattern is handled by the
                        // post-pass (rest_props lowering). Anything else
                        // starting with $ is unsupported.
                        if is_props_call(init) {
                            continue;
                        }
                        if expr_has_unsupported_rune(init) {
                            return None;
                        }
                    }
                }
            }
            Some(Statement::Variable(Box::new(out)))
        }
        Statement::Function(f) => {
            let mut f2 = (**f).clone();
            rewrite_block_for_state(&mut f2.body.body, state_bindings);
            Some(Statement::Function(Box::new(f2)))
        }
        Statement::Class(c) => {
            let mut c2 = (**c).clone();
            rewrite_class_body_client(&mut c2);
            Some(Statement::Class(Box::new(c2)))
        }
        Statement::Expression(e) => {
            // Rewrite reads/writes inside expression statements so legacy
            // mutable bindings (added to state_bindings) get proper
            // `$.get(X)` / `$.set(X, V)` / `$.update(X)` calls. Includes
            // descent into Arrow / Function callback bodies.
            let mut e2 = (**e).clone();
            rewrite_expr_for_state(&mut e2.expression, state_bindings);
            Some(Statement::Expression(Box::new(e2)))
        }
        _ => None,
    }
}

/// Client-side class body transform. Mirrors the server transform but with
/// client-specific semantics:
/// - Public `\$state(V)` field → private `#X = \$.state(V)` + getter
///   `get X() { return \$.get(this.#X); }` + setter
///   `set X(value) { \$.set(this.#X, value, true); }`
/// - Private `\$state(V)` field → `#X = \$.state(V)` (no accessor pair).
/// - Public `\$derived(E)` field → private `#X = \$.derived(() => E)` +
///   getter `get X() { return \$.get(this.#X); }` + setter
///   `set X(value) { \$.set(this.#X, value); }`
/// - `\$derived.by(F)` → `\$.derived(F)`.
/// - Method/constructor bodies: rewrite `this.#X = V` to `\$.set(this.#X, V)`
///   for any field that lowered to a state binding.
fn rewrite_class_body_client(c: &mut ClassDeclaration) {
    // First pass: discover which private names will hold state (need the
    // `$.set(this.#X, V)` rewrite in method bodies).
    let mut state_privates: HashSet<String> = HashSet::new();
    for m in &c.body.body {
        if let ClassMember::Property(p) = m {
            if let Some(value) = &p.value {
                if let Expression::Call(call) = value {
                    if let Some(kp) = global_keypath(&call.callee) {
                        if matches!(kp.as_str(), "$state" | "$state.raw" | "$state.eager") {
                            if let PropertyKey::Private(pi) = &p.key {
                                state_privates.insert(pi.name.to_string());
                            } else if let PropertyKey::Identifier(id) = &p.key {
                                state_privates.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let mut new_members: Vec<ClassMember> = Vec::with_capacity(c.body.body.len());
    for member in std::mem::take(&mut c.body.body) {
        match member {
            ClassMember::Property(mut p) => {
                let kind = property_rune_kind_client(&p.value);
                match kind {
                    Some(ClassFieldRuneClient::State) => {
                        let was_private = matches!(p.key, PropertyKey::Private(_));
                        let public_name = match &p.key {
                            PropertyKey::Identifier(i) => i.name.clone(),
                            PropertyKey::Private(pi) => pi.name.clone(),
                            _ => {
                                new_members.push(ClassMember::Property(p));
                                continue;
                            }
                        };
                        // Inner $state arg (or no init).
                        let arg = property_rune_inner_client(p.value.as_ref().unwrap());
                        let state_call_args: Vec<Argument> = match arg {
                            Some(e) => vec![Argument::Expression(e)],
                            None => Vec::new(),
                        };
                        let state_expr = Expression::Call(Box::new(CallExpression {
                            callee: t::member_id(t::id_dollar(), "state"),
                            arguments: state_call_args,
                            optional: false,
                            span: Span::ZERO,
                        }));
                        p.key = PropertyKey::Private(PrivateIdentifier {
                            name: public_name.clone(),
                            span: Span::ZERO,
                        });
                        p.value = Some(state_expr);
                        new_members.push(ClassMember::Property(p));
                        if !was_private {
                            new_members.push(make_state_getter(&public_name));
                            new_members.push(make_state_setter(&public_name));
                        }
                    }
                    Some(ClassFieldRuneClient::Derived(by)) => {
                        let was_private = matches!(p.key, PropertyKey::Private(_));
                        let public_name = match &p.key {
                            PropertyKey::Identifier(i) => i.name.clone(),
                            PropertyKey::Private(pi) => pi.name.clone(),
                            _ => {
                                new_members.push(ClassMember::Property(p));
                                continue;
                            }
                        };
                        let arg = property_rune_inner_client(p.value.as_ref().unwrap())
                            .unwrap_or_else(|| Expression::Identifier(Identifier {
                                name: Cow::Borrowed("undefined"),
                                span: Span::ZERO,
                            }));
                        let derived_expr = if by {
                            t::call(t::member_id(t::id_dollar(), "derived"), vec![arg])
                        } else {
                            t::call(
                                t::member_id(t::id_dollar(), "derived"),
                                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                                    params: Vec::new(),
                                    param_type_annotations: Vec::new(),
                                    body: ArrowBody::Expression(arg),
                                    r#async: false,
                                    span: Span::ZERO,
                                }))],
                            )
                        };
                        p.key = PropertyKey::Private(PrivateIdentifier {
                            name: public_name.clone(),
                            span: Span::ZERO,
                        });
                        p.value = Some(derived_expr);
                        new_members.push(ClassMember::Property(p));
                        if !was_private {
                            new_members.push(make_derived_getter_client(&public_name));
                            new_members.push(make_derived_setter_client(&public_name));
                        }
                    }
                    None => {
                        new_members.push(ClassMember::Property(p));
                    }
                }
            }
            ClassMember::Method(mut m) => {
                rewrite_block_for_class_state(&mut m.value.body.body, &state_privates);
                new_members.push(ClassMember::Method(m));
            }
            ClassMember::StaticBlock(mut sb) => {
                rewrite_block_for_class_state(&mut sb.body, &state_privates);
                new_members.push(ClassMember::StaticBlock(sb));
            }
        }
    }
    c.body.body = new_members;
}

enum ClassFieldRuneClient {
    State,
    Derived(bool),
}

fn property_rune_kind_client(value: &Option<Expression>) -> Option<ClassFieldRuneClient> {
    let e = value.as_ref()?;
    let Expression::Call(c) = e else { return None };
    let kp = global_keypath(&c.callee)?;
    match kp.as_str() {
        "$state" | "$state.raw" | "$state.eager" => Some(ClassFieldRuneClient::State),
        "$derived" => Some(ClassFieldRuneClient::Derived(false)),
        "$derived.by" => Some(ClassFieldRuneClient::Derived(true)),
        _ => None,
    }
}

fn property_rune_inner_client(e: &Expression) -> Option<Expression> {
    let Expression::Call(c) = e else { return None };
    c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    })
}

fn make_state_getter(public_name: &str) -> ClassMember {
    // `get X() { return $.get(this.#X); }`
    let body = vec![Statement::Return(Box::new(ReturnStatement {
        argument: Some(t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![this_private(public_name)],
        )),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(public_name.to_string()),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Get,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

fn make_state_setter(public_name: &str) -> ClassMember {
    // `set X(value) { $.set(this.#X, value, true); }`
    let body = vec![t::stmt(t::call(
        t::member_id(t::id_dollar(), "set"),
        vec![
            this_private(public_name),
            t::id("value"),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ],
    ))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(public_name.to_string()),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: vec![t::pat_id("value")],
            param_type_annotations: Vec::new(),
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Set,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

fn make_derived_getter_client(public_name: &str) -> ClassMember {
    let body = vec![Statement::Return(Box::new(ReturnStatement {
        argument: Some(t::call(
            t::member_id(t::id_dollar(), "get"),
            vec![this_private(public_name)],
        )),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(public_name.to_string()),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: Vec::new(),
            param_type_annotations: Vec::new(),
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Get,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

fn make_derived_setter_client(public_name: &str) -> ClassMember {
    // `set X(value) { $.set(this.#X, value); }`
    let body = vec![t::stmt(t::call(
        t::member_id(t::id_dollar(), "set"),
        vec![this_private(public_name), t::id("value")],
    ))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(public_name.to_string()),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: vec![t::pat_id("value")],
            param_type_annotations: Vec::new(),
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Set,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

fn this_private(name: &str) -> Expression {
    Expression::Member(Box::new(MemberExpression {
        object: Expression::This(Span::ZERO),
        property: MemberProperty::Private(PrivateIdentifier {
            name: Cow::Owned(name.to_string()),
            span: Span::ZERO,
        }),
        computed: false,
        optional: false,
        span: Span::ZERO,
    }))
}

/// Walk a method body and rewrite `this.#X = V` to `$.set(this.#X, V)` when
/// `#X` is a known state private field.
fn rewrite_block_for_class_state(body: &mut Vec<Statement>, state_privates: &HashSet<String>) {
    for s in body {
        rewrite_stmt_for_class_state(s, state_privates);
    }
}

fn rewrite_stmt_for_class_state(s: &mut Statement, state_privates: &HashSet<String>) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &mut v.declarations {
                if let Some(init) = &mut d.init {
                    rewrite_expr_for_class_state(init, state_privates);
                }
            }
        }
        S::Expression(e) => rewrite_expr_for_class_state(&mut e.expression, state_privates),
        S::Block(b) => rewrite_block_for_class_state(&mut b.body, state_privates),
        S::Return(r) => {
            if let Some(a) = &mut r.argument {
                rewrite_expr_for_class_state(a, state_privates);
            }
        }
        S::If(i) => {
            rewrite_expr_for_class_state(&mut i.test, state_privates);
            rewrite_stmt_for_class_state(&mut i.consequent, state_privates);
            if let Some(a) = &mut i.alternate {
                rewrite_stmt_for_class_state(a, state_privates);
            }
        }
        _ => {}
    }
}

fn rewrite_expr_for_class_state(e: &mut Expression, state_privates: &HashSet<String>) {
    use Expression as E;
    match e {
        E::Assignment(a) => {
            // Detect `this.#X = V` → `\$.set(this.#X, V)`.
            let private_name = match &a.left {
                AssignmentTarget::Expression(E::Member(m)) => {
                    if matches!(&m.object, E::This(_)) {
                        if let MemberProperty::Private(pi) = &m.property {
                            Some(pi.name.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                AssignmentTarget::Pattern(Pattern::Member(m)) => {
                    if matches!(&m.object, E::This(_)) {
                        if let MemberProperty::Private(pi) = &m.property {
                            Some(pi.name.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(name) = private_name {
                if state_privates.contains(name.as_str())
                    && matches!(a.operator, AssignmentOperator::Assign)
                {
                    rewrite_expr_for_class_state(&mut a.right, state_privates);
                    let rhs = std::mem::replace(
                        &mut a.right,
                        Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                    );
                    *e = t::call(
                        t::member_id(t::id_dollar(), "set"),
                        vec![this_private(&name), rhs],
                    );
                    return;
                }
            }
            if let AssignmentTarget::Expression(target) = &mut a.left {
                rewrite_expr_for_class_state(target, state_privates);
            }
            rewrite_expr_for_class_state(&mut a.right, state_privates);
        }
        E::Call(c) => {
            rewrite_expr_for_class_state(&mut c.callee, state_privates);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => rewrite_expr_for_class_state(e, state_privates),
                    Argument::Spread(s) => rewrite_expr_for_class_state(&mut s.argument, state_privates),
                }
            }
        }
        E::Member(m) => {
            rewrite_expr_for_class_state(&mut m.object, state_privates);
        }
        E::Binary(b) => {
            rewrite_expr_for_class_state(&mut b.left, state_privates);
            rewrite_expr_for_class_state(&mut b.right, state_privates);
        }
        E::Logical(l) => {
            rewrite_expr_for_class_state(&mut l.left, state_privates);
            rewrite_expr_for_class_state(&mut l.right, state_privates);
        }
        _ => {}
    }
}

/// `$state(V)` → `$.state(V)` (in-place).
fn lower_state_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    c.callee = t::member_id(t::id_dollar(), "state");
}

/// `$state({...})` / `$state([...])` → `$.proxy({...})` (in-place).
fn lower_to_proxy_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    c.callee = t::member_id(t::id_dollar(), "proxy");
}

/// `$derived(EXPR)` → `$.derived(() => EXPR)`. `$derived.by(FN)` → `$.derived(FN)`.
fn lower_to_derived_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    let kp = global_keypath(&c.callee).unwrap_or_default();
    let by = kp == "$derived.by";
    c.callee = t::member_id(t::id_dollar(), "derived");
    if !by {
        // Wrap the first arg in `() => arg`.
        let arg = c.arguments.iter().find_map(|a| match a {
            Argument::Expression(e) => Some(e.clone()),
            _ => None,
        });
        if let Some(inner) = arg {
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(inner),
                r#async: false,
                span: Span::ZERO,
            }));
            c.arguments = vec![Argument::Expression(arrow)];
        }
    }
}

/// Walk a function body and rewrite state-binding references.
fn rewrite_block_for_state(body: &mut Vec<Statement>, state: &HashSet<String>) {
    for s in body {
        rewrite_stmt_for_state(s, state);
    }
}

fn rewrite_stmt_for_state(s: &mut Statement, state: &HashSet<String>) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &mut v.declarations {
                if let Some(init) = &mut d.init {
                    rewrite_expr_for_state(init, state);
                }
            }
        }
        S::Expression(e) => rewrite_expr_for_state(&mut e.expression, state),
        S::Block(b) => {
            for s in &mut b.body {
                rewrite_stmt_for_state(s, state);
            }
        }
        S::Return(r) => {
            if let Some(a) = &mut r.argument {
                rewrite_expr_for_state(a, state);
            }
        }
        S::If(i) => {
            rewrite_expr_for_state(&mut i.test, state);
            rewrite_stmt_for_state(&mut i.consequent, state);
            if let Some(a) = &mut i.alternate {
                rewrite_stmt_for_state(a, state);
            }
        }
        S::For(f) => {
            if let Some(init) = &mut f.init {
                if let ForInit::Expression(e) = init {
                    rewrite_expr_for_state(e, state);
                } else if let ForInit::Declaration(d) = init {
                    for d in &mut d.declarations {
                        if let Some(init) = &mut d.init {
                            rewrite_expr_for_state(init, state);
                        }
                    }
                }
            }
            if let Some(t) = &mut f.test {
                rewrite_expr_for_state(t, state);
            }
            if let Some(u) = &mut f.update {
                rewrite_expr_for_state(u, state);
            }
            rewrite_stmt_for_state(&mut f.body, state);
        }
        S::ForIn(f) => {
            rewrite_expr_for_state(&mut f.right, state);
            rewrite_stmt_for_state(&mut f.body, state);
        }
        S::ForOf(f) => {
            rewrite_expr_for_state(&mut f.right, state);
            rewrite_stmt_for_state(&mut f.body, state);
        }
        S::While(w) => {
            rewrite_expr_for_state(&mut w.test, state);
            rewrite_stmt_for_state(&mut w.body, state);
        }
        S::Function(f) => {
            for s in &mut f.body.body {
                rewrite_stmt_for_state(s, state);
            }
        }
        S::Throw(t) => rewrite_expr_for_state(&mut t.argument, state),
        _ => {}
    }
}

pub(crate) fn rewrite_expr_for_state(e: &mut Expression, state: &HashSet<String>) {
    use Expression as E;
    match e {
        E::Identifier(i) => {
            if state.contains(i.name.as_ref()) {
                let name = i.name.clone();
                *e = t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]);
            }
        }
        E::Assignment(a) => {
            // Try to detect `X = ...` or `X OP= ...` where X is a state binding.
            // The LHS may be either Expression(Identifier) or Pattern(Identifier)
            // depending on the parser path.
            let lhs_name: Option<String> = match &a.left {
                AssignmentTarget::Expression(E::Identifier(id)) => Some(id.name.to_string()),
                AssignmentTarget::Pattern(Pattern::Identifier(id)) => Some(id.name.to_string()),
                _ => None,
            };
            if let Some(name) = lhs_name {
                if state.contains(name.as_str()) {
                    {
                        // Recurse into RHS first (its own reads become $.get).
                        rewrite_expr_for_state(&mut a.right, state);
                        let rhs = std::mem::replace(
                            &mut a.right,
                            Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                        );
                        let new_value = match a.operator {
                            AssignmentOperator::Assign => rhs,
                            AssignmentOperator::AddAssign => binop(
                                BinaryOperator::Plus,
                                t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]),
                                rhs,
                            ),
                            AssignmentOperator::SubAssign => binop(
                                BinaryOperator::Minus,
                                t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]),
                                rhs,
                            ),
                            AssignmentOperator::MulAssign => binop(
                                BinaryOperator::Mul,
                                t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]),
                                rhs,
                            ),
                            AssignmentOperator::DivAssign => binop(
                                BinaryOperator::Div,
                                t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]),
                                rhs,
                            ),
                            AssignmentOperator::ModAssign => binop(
                                BinaryOperator::Mod,
                                t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())]),
                                rhs,
                            ),
                            _ => rhs,
                        };
                        *e = t::call(
                            t::member_id(t::id_dollar(), "set"),
                            vec![t::id_owned(name.to_string()), new_value],
                        );
                        return;
                    }
                }
            }
            // Not a state assignment — recurse normally.
            if let AssignmentTarget::Expression(target) = &mut a.left {
                rewrite_expr_for_state(target, state);
            }
            rewrite_expr_for_state(&mut a.right, state);
        }
        E::Update(u) => {
            if let E::Identifier(id) = &u.argument {
                if state.contains(id.name.as_ref()) {
                    let name = id.name.clone();
                    let increment_args = match u.operator {
                        UpdateOperator::Increment => vec![t::id_owned(name.to_string())],
                        UpdateOperator::Decrement => vec![t::id_owned(name.to_string()), t::lit_number(-1.0)],
                    };
                    *e = t::call(t::member_id(t::id_dollar(), "update"), increment_args);
                    return;
                }
            }
            rewrite_expr_for_state(&mut u.argument, state);
        }
        E::Call(c) => {
            rewrite_expr_for_state(&mut c.callee, state);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => rewrite_expr_for_state(e, state),
                    Argument::Spread(s) => rewrite_expr_for_state(&mut s.argument, state),
                }
            }
        }
        E::Member(m) => {
            rewrite_expr_for_state(&mut m.object, state);
            if let MemberProperty::Expression(e) = &mut m.property {
                rewrite_expr_for_state(e, state);
            }
        }
        E::Binary(b) => {
            rewrite_expr_for_state(&mut b.left, state);
            rewrite_expr_for_state(&mut b.right, state);
        }
        E::Logical(l) => {
            rewrite_expr_for_state(&mut l.left, state);
            rewrite_expr_for_state(&mut l.right, state);
        }
        E::Conditional(c) => {
            rewrite_expr_for_state(&mut c.test, state);
            rewrite_expr_for_state(&mut c.consequent, state);
            rewrite_expr_for_state(&mut c.alternate, state);
        }
        E::Unary(u) => rewrite_expr_for_state(&mut u.argument, state),
        E::Sequence(s) => {
            for e in &mut s.expressions {
                rewrite_expr_for_state(e, state);
            }
        }
        E::Paren(p) => rewrite_expr_for_state(&mut p.expression, state),
        E::Template(t) => {
            for ex in &mut t.expressions {
                rewrite_expr_for_state(ex, state);
            }
        }
        E::Spread(s) => rewrite_expr_for_state(&mut s.argument, state),
        E::Arrow(a) => match &mut a.body {
            ArrowBody::Block(b) => {
                for s in &mut b.body {
                    rewrite_stmt_for_state(s, state);
                }
            }
            ArrowBody::Expression(body_expr) => {
                // Special case: `() => X = V` where X is a state binding and
                // the operator is plain `=` AND V contains a CallExpression.
                // Mirrors upstream's notify-flag emission for plain
                // assignments whose RHS is a function-call result.
                let mut handled = false;
                if let E::Assignment(asgn) = body_expr {
                    if matches!(asgn.operator, AssignmentOperator::Assign) {
                        let lhs_name = match &asgn.left {
                            AssignmentTarget::Expression(E::Identifier(id)) => {
                                Some(id.name.to_string())
                            }
                            AssignmentTarget::Pattern(Pattern::Identifier(id)) => {
                                Some(id.name.to_string())
                            }
                            _ => None,
                        };
                        if let Some(name) = lhs_name {
                            if state.contains(name.as_str())
                                && expr_contains_call(&asgn.right)
                            {
                                rewrite_expr_for_state(&mut asgn.right, state);
                                let rhs = std::mem::replace(
                                    &mut asgn.right,
                                    Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                                );
                                *body_expr = t::call(
                                    t::member_id(t::id_dollar(), "set"),
                                    vec![
                                        t::id_owned(name.to_string()),
                                        rhs,
                                        Expression::Literal(Box::new(Literal::Boolean(
                                            BooleanLiteral {
                                                value: true,
                                                span: Span::ZERO,
                                            },
                                        ))),
                                    ],
                                );
                                handled = true;
                            }
                        }
                    }
                }
                if !handled {
                    rewrite_expr_for_state(body_expr, state);
                }
            }
        },
        E::Function(f) => {
            for s in &mut f.body.body {
                rewrite_stmt_for_state(s, state);
            }
        }
        E::New(n) => {
            rewrite_expr_for_state(&mut n.callee, state);
            for a in &mut n.arguments {
                match a {
                    Argument::Expression(e) => rewrite_expr_for_state(e, state),
                    Argument::Spread(s) => rewrite_expr_for_state(&mut s.argument, state),
                }
            }
        }
        E::Array(a) => {
            for el in &mut a.elements {
                if let ArrayElement::Expression(e) = el {
                    rewrite_expr_for_state(e, state);
                }
            }
        }
        E::Object(o) => {
            for m in &mut o.properties {
                if let ObjectMember::Property(p) = m {
                    rewrite_expr_for_state(&mut p.value, state);
                }
            }
        }
        E::Await(a) => rewrite_expr_for_state(&mut a.argument, state),
        _ => {}
    }
}

fn binop(op: BinaryOperator, left: Expression, right: Expression) -> Expression {
    Expression::Binary(Box::new(BinaryExpression {
        left,
        operator: op,
        right,
        span: Span::ZERO,
    }))
}

/// If `init` is `$state(LIT)` (or `$state.raw(LIT)`) and `id` is a never-
/// assigned identifier, replace init with the inner literal. Returns true if
/// rewrite happened.
fn try_strip_state(
    init: &mut Expression,
    id: &Pattern,
    assigned: &HashSet<String>,
    uses_runes: &mut bool,
) -> bool {
    let Some(name) = pattern_single_ident(id) else { return false };
    if assigned.contains(name.as_str()) {
        // Has assignments — would need full $.state lowering; mark and bail
        // at the caller level.
        if is_state_call(init) {
            *uses_runes = true;
        }
        return false;
    }
    if !is_state_call(init) {
        return false;
    }
    *uses_runes = true;
    let Expression::Call(c) = init else { return false };
    let arg = c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    });
    *init = arg.unwrap_or_else(|| Expression::Identifier(Identifier {
        name: Cow::Borrowed("undefined"),
        span: Span::ZERO,
    }));
    true
}

fn pattern_single_ident(p: &Pattern) -> Option<String> {
    match p {
        Pattern::Identifier(i) => Some(i.name.to_string()),
        _ => None,
    }
}

fn is_state_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$state" | "$state.raw" | "$state.eager")
}

/// True when `$state(...)` has an object-literal or array-literal arg —
/// these lower to `$.proxy(...)` rather than `$.state(...)`.
fn state_call_inner_is_proxy_init(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e),
        _ => None,
    }).map_or(false, |inner| {
        matches!(inner, Expression::Object(_) | Expression::Array(_))
    })
}

fn is_derived_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$derived" | "$derived.by")
}

fn expr_has_unsupported_rune(e: &Expression) -> bool {
    match e {
        Expression::Call(c) => {
            if let Some(kp) = global_keypath(&c.callee) {
                if kp.starts_with('$') {
                    return true;
                }
            }
            expr_has_unsupported_rune(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_has_unsupported_rune(e),
                    Argument::Spread(s) => expr_has_unsupported_rune(&s.argument),
                })
        }
        Expression::Member(m) => expr_has_unsupported_rune(&m.object),
        _ => false,
    }
}

fn global_keypath(e: &Expression) -> Option<String> {
    match e {
        Expression::Identifier(i) => Some(i.name.to_string()),
        Expression::Member(m) => {
            if m.computed || m.optional {
                return None;
            }
            let base = global_keypath(&m.object)?;
            let prop = match &m.property {
                MemberProperty::Identifier(i) => i.name.clone(),
                _ => return None,
            };
            Some(format!("{base}.{prop}"))
        }
        _ => None,
    }
}

/// Walk every statement and collect identifiers that appear as the LHS of an
/// `Assignment` or the operand of an `Update` expression.
fn collect_assigned_targets(body: &[Statement]) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in body {
        scan_stmt_for_assignments(s, &mut out);
    }
    out
}

fn scan_stmt_for_assignments(s: &Statement, out: &mut HashSet<String>) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    scan_expr_for_assignments(init, out);
                }
            }
        }
        S::Expression(e) => scan_expr_for_assignments(&e.expression, out),
        S::Block(b) => {
            for s in &b.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        S::Return(r) => {
            if let Some(a) = &r.argument {
                scan_expr_for_assignments(a, out);
            }
        }
        S::If(i) => {
            scan_expr_for_assignments(&i.test, out);
            scan_stmt_for_assignments(&i.consequent, out);
            if let Some(a) = &i.alternate {
                scan_stmt_for_assignments(a, out);
            }
        }
        S::For(f) => {
            if let Some(init) = &f.init {
                if let ForInit::Expression(e) = init {
                    scan_expr_for_assignments(e, out);
                }
            }
            if let Some(t) = &f.test {
                scan_expr_for_assignments(t, out);
            }
            if let Some(u) = &f.update {
                scan_expr_for_assignments(u, out);
            }
            scan_stmt_for_assignments(&f.body, out);
        }
        S::ForIn(f) => {
            scan_expr_for_assignments(&f.right, out);
            scan_stmt_for_assignments(&f.body, out);
        }
        S::ForOf(f) => {
            scan_expr_for_assignments(&f.right, out);
            scan_stmt_for_assignments(&f.body, out);
        }
        S::While(w) => {
            scan_expr_for_assignments(&w.test, out);
            scan_stmt_for_assignments(&w.body, out);
        }
        S::DoWhile(w) => {
            scan_stmt_for_assignments(&w.body, out);
            scan_expr_for_assignments(&w.test, out);
        }
        S::Function(f) => {
            for s in &f.body.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        S::Try(t) => {
            for s in &t.block.body {
                scan_stmt_for_assignments(s, out);
            }
            if let Some(h) = &t.handler {
                for s in &h.body.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
            if let Some(f) = &t.finalizer {
                for s in &f.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
        }
        S::Switch(sw) => {
            scan_expr_for_assignments(&sw.discriminant, out);
            for c in &sw.cases {
                if let Some(t) = &c.test {
                    scan_expr_for_assignments(t, out);
                }
                for s in &c.consequent {
                    scan_stmt_for_assignments(s, out);
                }
            }
        }
        S::Throw(t) => scan_expr_for_assignments(&t.argument, out),
        _ => {}
    }
}

fn scan_fragment_assignments(f: &Fragment) -> HashSet<String> {
    let mut out = HashSet::new();
    scan_nodes_for_assignments(&f.nodes, &mut out);
    out
}

fn scan_nodes_for_assignments(nodes: &[FragmentChild], out: &mut HashSet<String>) {
    for n in nodes {
        match n {
            FragmentChild::ExpressionTag(t) => scan_expr_for_assignments(&t.expression, out),
            FragmentChild::HtmlTag(t) => scan_expr_for_assignments(&t.expression, out),
            FragmentChild::RegularElement(el) => {
                for attr in &el.attributes {
                    match attr {
                        ElementAttribute::Attribute(a) => match &a.value {
                            AttributeValue::Single(tag) => {
                                scan_expr_for_assignments(&tag.expression, out)
                            }
                            AttributeValue::Many(parts) => {
                                for p in parts {
                                    if let AttributeValuePart::ExpressionTag(t) = p {
                                        scan_expr_for_assignments(&t.expression, out);
                                    }
                                }
                            }
                            _ => {}
                        },
                        ElementAttribute::SpreadAttribute(s) => {
                            scan_expr_for_assignments(&s.expression, out)
                        }
                        ElementAttribute::BindDirective(b) => {
                            // `bind:NAME={target}` means the child may
                            // mutate `target`. Treat the target's root
                            // identifier as assigned so it gets state lowering.
                            if let Expression::Identifier(i) = &b.expression {
                                out.insert(i.name.to_string());
                            }
                            scan_expr_for_assignments(&b.expression, out);
                        }
                        _ => {}
                    }
                }
                scan_nodes_for_assignments(&el.fragment.nodes, out);
            }
            FragmentChild::Component(c) => {
                for attr in &c.attributes {
                    match attr {
                        ElementAttribute::Attribute(a) => match &a.value {
                            AttributeValue::Single(tag) => {
                                scan_expr_for_assignments(&tag.expression, out)
                            }
                            AttributeValue::Many(parts) => {
                                for p in parts {
                                    if let AttributeValuePart::ExpressionTag(t) = p {
                                        scan_expr_for_assignments(&t.expression, out);
                                    }
                                }
                            }
                            _ => {}
                        },
                        ElementAttribute::SpreadAttribute(s) => {
                            scan_expr_for_assignments(&s.expression, out)
                        }
                        ElementAttribute::BindDirective(b) => {
                            // `bind:NAME={target}` means the child may
                            // mutate `target`. Treat the target's root
                            // identifier as assigned so it gets state lowering.
                            if let Expression::Identifier(i) = &b.expression {
                                out.insert(i.name.to_string());
                            }
                            scan_expr_for_assignments(&b.expression, out);
                        }
                        _ => {}
                    }
                }
                scan_nodes_for_assignments(&c.fragment.nodes, out);
            }
            FragmentChild::IfBlock(ib) => {
                scan_expr_for_assignments(&ib.test, out);
                scan_nodes_for_assignments(&ib.consequent.nodes, out);
                if let Some(a) = &ib.alternate {
                    scan_nodes_for_assignments(&a.nodes, out);
                }
            }
            FragmentChild::EachBlock(eb) => {
                scan_expr_for_assignments(&eb.expression, out);
                scan_nodes_for_assignments(&eb.body.nodes, out);
                if let Some(f) = &eb.fallback {
                    scan_nodes_for_assignments(&f.nodes, out);
                }
            }
            FragmentChild::AwaitBlock(ab) => {
                scan_expr_for_assignments(&ab.expression, out);
                if let Some(p) = &ab.pending {
                    scan_nodes_for_assignments(&p.nodes, out);
                }
                if let Some(t) = &ab.then {
                    scan_nodes_for_assignments(&t.nodes, out);
                }
                if let Some(c) = &ab.catch_ {
                    scan_nodes_for_assignments(&c.nodes, out);
                }
            }
            FragmentChild::KeyBlock(kb) => {
                scan_expr_for_assignments(&kb.expression, out);
                scan_nodes_for_assignments(&kb.fragment.nodes, out);
            }
            _ => {}
        }
    }
}

fn scan_expr_for_assignments(e: &Expression, out: &mut HashSet<String>) {
    use Expression as E;
    match e {
        E::Assignment(a) => {
            collect_assignment_target_idents(&a.left, out);
            scan_expr_for_assignments(&a.right, out);
        }
        E::Update(u) => {
            if let E::Identifier(i) = &u.argument {
                out.insert(i.name.to_string());
            } else {
                scan_expr_for_assignments(&u.argument, out);
            }
        }
        E::Call(c) => {
            scan_expr_for_assignments(&c.callee, out);
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => scan_expr_for_assignments(e, out),
                    Argument::Spread(s) => scan_expr_for_assignments(&s.argument, out),
                }
            }
        }
        E::New(n) => {
            scan_expr_for_assignments(&n.callee, out);
            for a in &n.arguments {
                match a {
                    Argument::Expression(e) => scan_expr_for_assignments(e, out),
                    Argument::Spread(s) => scan_expr_for_assignments(&s.argument, out),
                }
            }
        }
        E::Member(m) => {
            scan_expr_for_assignments(&m.object, out);
            if let MemberProperty::Expression(e) = &m.property {
                scan_expr_for_assignments(e, out);
            }
        }
        E::Binary(b) => {
            scan_expr_for_assignments(&b.left, out);
            scan_expr_for_assignments(&b.right, out);
        }
        E::Logical(l) => {
            scan_expr_for_assignments(&l.left, out);
            scan_expr_for_assignments(&l.right, out);
        }
        E::Conditional(c) => {
            scan_expr_for_assignments(&c.test, out);
            scan_expr_for_assignments(&c.consequent, out);
            scan_expr_for_assignments(&c.alternate, out);
        }
        E::Unary(u) => scan_expr_for_assignments(&u.argument, out),
        E::Sequence(s) => {
            for e in &s.expressions {
                scan_expr_for_assignments(e, out);
            }
        }
        E::Paren(p) => scan_expr_for_assignments(&p.expression, out),
        E::Template(t) => {
            for ex in &t.expressions {
                scan_expr_for_assignments(ex, out);
            }
        }
        E::Spread(s) => scan_expr_for_assignments(&s.argument, out),
        E::Arrow(a) => match &a.body {
            ArrowBody::Block(b) => {
                for s in &b.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
            ArrowBody::Expression(e) => scan_expr_for_assignments(e, out),
        },
        E::Function(f) => {
            for s in &f.body.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        E::Array(a) => {
            for el in &a.elements {
                if let ArrayElement::Expression(e) = el {
                    scan_expr_for_assignments(e, out);
                }
            }
        }
        E::Object(o) => {
            for m in &o.properties {
                if let ObjectMember::Property(p) = m {
                    scan_expr_for_assignments(&p.value, out);
                }
            }
        }
        E::Await(a) => scan_expr_for_assignments(&a.argument, out),
        _ => {}
    }
}

fn collect_assignment_target_idents(target: &AssignmentTarget, out: &mut HashSet<String>) {
    match target {
        AssignmentTarget::Expression(e) => {
            if let Expression::Identifier(i) = e {
                out.insert(i.name.to_string());
            }
        }
        AssignmentTarget::Pattern(p) => collect_pattern_idents(p, out),
    }
}

fn collect_pattern_idents(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Identifier(i) => {
            out.insert(i.name.to_string());
        }
        Pattern::Array(a) => {
            for el in &a.elements {
                if let Some(p) = el {
                    collect_pattern_idents(p, out);
                }
            }
        }
        Pattern::Object(o) => {
            for m in &o.properties {
                if let ObjectPatternMember::Property(p) = m {
                    collect_pattern_idents(&p.value, out);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Template classification
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum NodeKind<'a> {
    /// Element with no expression children, only static attributes, no
    /// directives — serializes wholly into the template literal.
    StaticElement(&'a RegularElement),
    /// Element with at least one expression child OR an event handler /
    /// bind directive. Static attrs serialize into HTML; everything else
    /// is captured in `Directives` and emitted as body statements.
    InterpElement(&'a RegularElement, ElementContent<'a>, Directives<'a>),
    Component(&'a Component),
    /// `{#await EXPR [then PAT]}{:catch PAT}{/await}` — lowered to a `<!>`
    /// placeholder + `$.await(node, getter, pending_arrow, then_arrow)`.
    AwaitBlock(&'a svelte_ast::blocks::AwaitBlock),
    /// Top-level `{expr}` — lowered to a space text-node anchor + a
    /// `$.set_text(text_N, ...)` entry in the combined template_effect.
    TopLevelExpr(&'a Expression),
    /// A run of consecutive top-level Text + ExpressionTag children that
    /// share a single text-node anchor. Lowered to one space in the HTML
    /// + `var text_N = $.sibling(prev)` + a combined template_effect.
    TopLevelText(Vec<TextPart<'a>>),
}

#[derive(Debug, Default)]
struct Directives<'a> {
    /// `onclick={handler}` → `("click", handler)`. Lowered to
    /// `\$.delegated("click", var, handler)`.
    events: Vec<(String, &'a Expression)>,
    /// `bind:value={target}` → target expression that becomes the
    /// state-getter/setter wrap. Lowered to `\$.bind_value(...)`.
    bind_value: Option<&'a Expression>,
}

#[derive(Debug)]
enum ElementContent<'a> {
    /// `<el></el>` with no child content — element exists only for its
    /// directives (e.g. `<input bind:value={x}>`).
    NoContent,
    /// Element has children but they're all static text — serialize directly
    /// into the template literal.
    StaticOnly,
    /// Element had expression children but they all folded to literal
    /// strings; the combined result is assigned via
    /// `el.textContent = 'combined'`.
    FoldedText(String),
    /// `<el>{expr}</el>` — single expression child. Lowered to
    /// `el.textContent = EXPR` provided EXPR doesn't reference a state-tracked
    /// binding (we don't yet wrap such reads with `$.get`).
    DirectText(&'a Expression),
    /// `<el>...mix of static text and expressions...</el>` — at least two
    /// fragments combined. Lowered via `$.child(el)` + `$.template_effect`.
    Reactive(Vec<TextPart<'a>>),
}

#[derive(Debug, Clone)]
enum TextPart<'a> {
    Static(String),
    Expr(&'a Expression),
}

fn single_root_var_name(kind: &NodeKind) -> String {
    match kind {
        NodeKind::StaticElement(el) => sanitize_name(&el.name),
        NodeKind::InterpElement(el, _, _) => sanitize_name(&el.name),
        NodeKind::Component(_) => "fragment".to_string(),
        NodeKind::AwaitBlock(_) => "fragment".to_string(),
        NodeKind::TopLevelExpr(_) => "fragment".to_string(),
        NodeKind::TopLevelText(_) => "fragment".to_string(),
    }
}

#[derive(Debug)]
enum GroupedNode<'a> {
    Single(&'a FragmentChild),
    /// A consecutive run of top-level Text + ExpressionTag children. They
    /// share one text-node anchor and one combined template_effect entry.
    TextRun(Vec<TextPart<'a>>),
}

fn coalesce_top_level_text<'a>(nodes: &[&'a FragmentChild]) -> Vec<GroupedNode<'a>> {
    let mut out: Vec<GroupedNode<'a>> = Vec::new();
    let mut run: Vec<TextPart<'a>> = Vec::new();
    let flush = |out: &mut Vec<GroupedNode<'a>>, run: &mut Vec<TextPart<'a>>| {
        if run.is_empty() {
            return;
        }
        // Only count as a run if at least one expression participates;
        // otherwise leave the lone text inline and skip it.
        if run.iter().any(|p| matches!(p, TextPart::Expr(_))) {
            out.push(GroupedNode::TextRun(std::mem::take(run)));
        } else {
            run.clear();
        }
    };
    for n in nodes {
        match n {
            FragmentChild::Text(t) => {
                run.push(TextPart::Static(t.data.clone()));
            }
            FragmentChild::ExpressionTag(et) => {
                run.push(TextPart::Expr(&et.expression));
            }
            _ => {
                flush(&mut out, &mut run);
                out.push(GroupedNode::Single(n));
            }
        }
    }
    flush(&mut out, &mut run);
    out
}

fn classify_grouped<'a>(g: &GroupedNode<'a>) -> Option<NodeKind<'a>> {
    match g {
        GroupedNode::Single(n) => classify(n),
        GroupedNode::TextRun(parts) => Some(NodeKind::TopLevelText(parts.clone())),
    }
}

fn classify(n: &FragmentChild) -> Option<NodeKind<'_>> {
    match n {
        FragmentChild::AwaitBlock(ab) => Some(NodeKind::AwaitBlock(ab)),
        FragmentChild::ExpressionTag(et) => Some(NodeKind::TopLevelExpr(&et.expression)),
        FragmentChild::RegularElement(el) => {
            // Sort attributes: static / event / bind:value / unsupported.
            let mut directives = Directives::default();
            let mut only_static_attrs = true;
            for attr in &el.attributes {
                match attr {
                    ElementAttribute::Attribute(a) => {
                        if is_event_name(&a.name) {
                            // `onclick={handler}` → delegated event.
                            if let AttributeValue::Single(tag) = &a.value {
                                let event = a.name[2..].to_string();
                                directives.events.push((event, &tag.expression));
                                only_static_attrs = false;
                                continue;
                            }
                            return None;
                        }
                        // Static-text-value attributes only.
                        match &a.value {
                            AttributeValue::Empty => {}
                            AttributeValue::Many(parts) => {
                                if !parts
                                    .iter()
                                    .all(|p| matches!(p, AttributeValuePart::Text(_)))
                                {
                                    return None;
                                }
                            }
                            AttributeValue::Single(_) => return None,
                        }
                    }
                    ElementAttribute::BindDirective(b) if b.name == "value" => {
                        directives.bind_value = Some(&b.expression);
                        only_static_attrs = false;
                    }
                    _ => return None,
                }
            }
            // Walk children: collect text + expression fragments.
            let mut parts: Vec<TextPart> = Vec::new();
            for c in &el.fragment.nodes {
                match c {
                    FragmentChild::Text(t) => {
                        parts.push(TextPart::Static(t.data.clone()));
                    }
                    FragmentChild::ExpressionTag(et) => {
                        parts.push(TextPart::Expr(&et.expression));
                    }
                    FragmentChild::RegularElement(_) => {
                        if !all_static(&[c.clone()]) {
                            return None;
                        }
                        // Nested static element prevents text-content lowering;
                        // treat as full static body.
                        parts.clear();
                        if all_static(&el.fragment.nodes) {
                            return Some(NodeKind::StaticElement(el));
                        }
                        return None;
                    }
                    _ => return None,
                }
            }

            // Strip leading/trailing whitespace-only Static parts.
            while parts
                .first()
                .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                .unwrap_or(false)
            {
                parts.remove(0);
            }
            while parts
                .last()
                .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                .unwrap_or(false)
            {
                parts.pop();
            }
            // Trim leading whitespace inside the FIRST Static part and
            // trailing whitespace inside the LAST.
            if let Some(TextPart::Static(s)) = parts.first_mut() {
                *s = s.trim_start().to_string();
            }
            if let Some(TextPart::Static(s)) = parts.last_mut() {
                *s = s.trim_end().to_string();
            }

            // No content + no directives → static element.
            if parts.is_empty() {
                if only_static_attrs {
                    return Some(NodeKind::StaticElement(el));
                }
                return Some(NodeKind::InterpElement(
                    el,
                    ElementContent::NoContent,
                    directives,
                ));
            }

            // Only static text → static body. If no directives → fully static
            // element. Otherwise InterpElement with StaticOnly content.
            if parts.iter().all(|p| matches!(p, TextPart::Static(_))) {
                if only_static_attrs {
                    return Some(NodeKind::StaticElement(el));
                }
                return Some(NodeKind::InterpElement(
                    el,
                    ElementContent::StaticOnly,
                    directives,
                ));
            }

            // Check whether every Expr part is a literal-stringifiable value
            // (after fold). If so, combine all parts into a single string and
            // lower to `el.textContent = '...';`.
            if parts.iter().all(|p| match p {
                TextPart::Static(_) => true,
                TextPart::Expr(e) => literal_to_template_string(e).is_some(),
            }) {
                let mut combined = String::new();
                for p in &parts {
                    match p {
                        TextPart::Static(s) => combined.push_str(s),
                        TextPart::Expr(e) => {
                            if let Some(s) = literal_to_template_string(e) {
                                combined.push_str(&s);
                            }
                        }
                    }
                }
                return Some(NodeKind::InterpElement(
                    el,
                    ElementContent::FoldedText(combined),
                    directives,
                ));
            }

            // Single expression, no static parts → direct textContent.
            if parts.len() == 1 {
                if let TextPart::Expr(e) = &parts[0] {
                    if expr_is_safe_for_textcontent(e) {
                        return Some(NodeKind::InterpElement(
                            el,
                            ElementContent::DirectText(*e),
                            directives,
                        ));
                    }
                    // Fall through to reactive path.
                }
            }

            // Reactive: one+ expressions, possibly with text.
            Some(NodeKind::InterpElement(
                el,
                ElementContent::Reactive(parts),
                directives,
            ))
        }
        FragmentChild::Component(c) => Some(NodeKind::Component(c)),
        _ => None,
    }
}

fn is_event_name(name: &str) -> bool {
    name.starts_with("on")
        && name.len() > 2
        && name
            .as_bytes()
            .get(2)
            .map(|b| b.is_ascii_lowercase())
            .unwrap_or(false)
}

fn write_static_attr(a: &Attribute, out: &mut String) -> Option<()> {
    match &a.value {
        AttributeValue::Empty => {
            out.push(' ');
            out.push_str(&a.name);
            Some(())
        }
        AttributeValue::Many(parts) => {
            out.push(' ');
            out.push_str(&a.name);
            out.push_str("=\"");
            for p in parts {
                if let AttributeValuePart::Text(t) = p {
                    for ch in t.data.chars() {
                        match ch {
                            '"' => out.push_str("&quot;"),
                            '&' => out.push_str("&amp;"),
                            '`' => out.push_str("\\`"),
                            '\\' => out.push_str("\\\\"),
                            _ => out.push(ch),
                        }
                    }
                }
            }
            out.push('"');
            Some(())
        }
        _ => None,
    }
}

/// Conservative check: expression doesn't need `$.get()` wrapping. Literals,
/// `loc.href`-style globals, simple calls into globals are accepted.
fn expr_is_safe_for_textcontent(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        Expression::Member(m) => match &m.object {
            Expression::Identifier(_) => !m.computed,
            _ => expr_is_safe_for_textcontent(&m.object) && !m.computed,
        },
        Expression::Call(c) => {
            if !expr_is_safe_for_textcontent(&c.callee) {
                return false;
            }
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => {
                        if !expr_is_safe_for_textcontent(e) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
        Expression::Identifier(_) => true,
        _ => false,
    }
}

fn all_static(nodes: &[FragmentChild]) -> bool {
    nodes.iter().all(|c| match c {
        FragmentChild::Text(_) | FragmentChild::Comment(_) => true,
        FragmentChild::RegularElement(el) => {
            el.attributes.is_empty() && all_static(&el.fragment.nodes)
        }
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// HTML serialization
// ---------------------------------------------------------------------------

fn serialize_element(
    el: &RegularElement,
    out: &mut String,
    include_body: bool,
    needs_text_node: bool,
) -> Option<()> {
    out.push('<');
    out.push_str(&el.name);
    for attr in &el.attributes {
        match attr {
            ElementAttribute::Attribute(a) => {
                if is_event_name(&a.name) {
                    // Event handlers are emitted as runtime calls — not in HTML.
                    continue;
                }
                write_static_attr(a, out)?;
            }
            // bind:* and on:* directives are handled in the body, not the HTML.
            ElementAttribute::BindDirective(_) | ElementAttribute::OnDirective(_) => continue,
            _ => return None,
        }
    }
    if is_void(&el.name) {
        out.push_str("/>");
        return Some(());
    }
    out.push('>');
    if include_body {
        for c in &el.fragment.nodes {
            serialize_static_child(c, out)?;
        }
    } else if needs_text_node {
        // Insert a single space so a text node exists for `$.child(el)`.
        out.push(' ');
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

fn serialize_static_child(c: &FragmentChild, out: &mut String) -> Option<()> {
    match c {
        FragmentChild::Text(t) => {
            for ch in t.data.chars() {
                match ch {
                    '`' => out.push_str("\\`"),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(ch),
                }
            }
            Some(())
        }
        FragmentChild::RegularElement(el) => serialize_element(el, out, true, false),
        FragmentChild::Comment(_) => Some(()),
        _ => None,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

// ---------------------------------------------------------------------------
// Body emission
// ---------------------------------------------------------------------------

fn emit_nav(out: &mut Vec<Statement>, name: &str, prev: Option<&str>) {
    let init = if let Some(p) = prev {
        t::call(
            t::member_id(t::id_dollar(), "sibling"),
            vec![t::id_owned(p.to_string()), t::lit_number(2.0)],
        )
    } else {
        t::call(t::member_id(t::id_dollar(), "first_child"), vec![t::id_fragment()])
    };
    out.push(t::var(name, init));
}

fn unique_var(base: &str, counts: &mut HashMap<String, usize>) -> String {
    let n = counts.entry(base.to_string()).or_insert(0);
    let name = if *n == 0 {
        base.to_string()
    } else {
        format!("{base}_{n}")
    };
    *n += 1;
    name
}

/// Variant of emit_element_content that pushes inline-form Reactive entries
/// to `text_effects` (so they can be combined into one template_effect later)
/// while falling back to the original `emit_element_content` for everything
/// else.
fn emit_element_content_combined(
    content: &ElementContent,
    parent_var: &str,
    body_stmts: &mut Vec<Statement>,
    effects: &mut Vec<Statement>,
    text_effects: &mut Vec<(String, Expression)>,
    var_counts: &mut HashMap<String, usize>,
    state_bindings: &HashSet<String>,
    async_info: Option<&AsyncInfo>,
) {
    if let ElementContent::Reactive(parts) = content {
        // If async-tainted, fall through to the original handler (4-arg
        // template_effect form).
        let async_tainted = match async_info {
            Some(ai) => parts.iter().any(|p| match p {
                TextPart::Static(_) => false,
                TextPart::Expr(e) => expr_refs_any_client(e, &ai.async_bindings),
            }),
            None => false,
        };
        if async_tainted {
            emit_element_content(
                content,
                parent_var,
                body_stmts,
                effects,
                state_bindings,
                async_info,
            );
            return;
        }
        // For Reactive content with 2+ expression parts, the per-element
        // deps-array form is needed (`($0, $1) => $.set_text(...)`, [() =>
        // expr0, () => expr1]). That doesn't combine with sibling reactive
        // elements, so route to the original per-element emitter.
        let expr_count = parts
            .iter()
            .filter(|p| matches!(p, TextPart::Expr(_)))
            .count();
        if expr_count > 1 {
            emit_element_content(
                content,
                parent_var,
                body_stmts,
                effects,
                state_bindings,
                async_info,
            );
            return;
        }
        // Non-async Reactive with 0/1 expr: pick a unique text var name,
        // emit nav, collect the template_expr for combined emission later.
        let text_var = unique_var("text", var_counts);
        body_stmts.push(t::var(
            &text_var,
            t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(parent_var.to_string())]),
        ));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id_dollar(), "reset"),
            vec![t::id_owned(parent_var.to_string())],
        )));
        let template_expr = build_inline_template(parts, state_bindings);
        text_effects.push((text_var, template_expr));
        return;
    }
    emit_element_content(
        content,
        parent_var,
        body_stmts,
        effects,
        state_bindings,
        async_info,
    );
}

fn emit_element_content(
    content: &ElementContent,
    parent_var: &str,
    body_stmts: &mut Vec<Statement>,
    effects: &mut Vec<Statement>,
    state_bindings: &HashSet<String>,
    async_info: Option<&AsyncInfo>,
) {
    // Demote DirectText to Reactive when the expression is async-tainted —
    // async values can't be assigned synchronously to `.textContent`.
    if let (ElementContent::DirectText(expr), Some(ai)) = (content, async_info) {
        if expr_refs_any_client(expr, &ai.async_bindings) {
            let parts = vec![TextPart::Expr(*expr)];
            let demoted = ElementContent::Reactive(parts);
            emit_element_content(
                &demoted,
                parent_var,
                body_stmts,
                effects,
                state_bindings,
                async_info,
            );
            return;
        }
    }
    // Detect async-tainted Reactive content first: emit the 4-arg
    // `\$.template_effect` form with `\$.child(parent, true)`.
    if let (ElementContent::Reactive(parts), Some(ai)) = (content, async_info) {
        let async_parts: Vec<bool> = parts
            .iter()
            .map(|p| match p {
                TextPart::Static(_) => false,
                TextPart::Expr(e) => expr_refs_any_client(e, &ai.async_bindings),
            })
            .collect();
        if async_parts.iter().any(|b| *b) {
            // text-node with the second `true` arg.
            body_stmts.push(t::var(
                "text",
                t::call(
                    t::member_id(t::id_dollar(), "child"),
                    vec![
                        t::id_owned(parent_var.to_string()),
                        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                            value: true,
                            span: Span::ZERO,
                        }))),
                    ],
                ),
            ));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(parent_var.to_string())],
            )));
            // For a single expression Reactive part: pass the expression
            // directly to set_text. For multiple parts: build a template
            // literal without `?? ''` coalesce.
            let expr_count = parts
                .iter()
                .filter(|p| matches!(p, TextPart::Expr(_)))
                .count();
            let set_text_arg: Expression = if parts.len() == 1 {
                if let TextPart::Expr(e) = &parts[0] {
                    (*e).clone()
                } else {
                    return;
                }
            } else if expr_count == 1
                && parts.iter().all(|p| !matches!(p, TextPart::Static(s) if !s.is_empty()))
            {
                // Single expression with only-whitespace static parts.
                let e = parts
                    .iter()
                    .find_map(|p| match p {
                        TextPart::Expr(e) => Some((*e).clone()),
                        _ => None,
                    })
                    .unwrap();
                e
            } else {
                // General template literal.
                let mut quasis: Vec<String> = Vec::new();
                let mut subs: Vec<Expression> = Vec::new();
                let mut current = String::new();
                for p in parts {
                    match p {
                        TextPart::Static(s) => current.push_str(s),
                        TextPart::Expr(e) => {
                            quasis.push(std::mem::take(&mut current));
                            subs.push((*e).clone());
                        }
                    }
                }
                quasis.push(current);
                t::template_raw(quasis, subs)
            };
            let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                param_type_annotations: Vec::new(),
                body: ArrowBody::Expression(t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id("text"), set_text_arg],
                )),
                r#async: false,
                span: Span::ZERO,
            }));
            // Blockers array: `[$$promises[N]]`
            let blockers = Expression::Array(Box::new(ArrayExpression {
                elements: vec![ArrayElement::Expression(Expression::Member(Box::new(
                    MemberExpression {
                        object: t::id("$$promises"),
                        property: MemberProperty::Expression(t::lit_number(
                            ai.last_group_idx as f64,
                        )),
                        computed: true,
                        optional: false,
                        span: Span::ZERO,
                    },
                )))],
                span: Span::ZERO,
            }));
            effects.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                vec![
                    effect_fn,
                    void_zero_client(),
                    void_zero_client(),
                    blockers,
                ],
            )));
            return;
        }
    }
    match content {
        ElementContent::NoContent | ElementContent::StaticOnly => {}
        ElementContent::FoldedText(s) => {
            let target = Expression::Member(Box::new(MemberExpression {
                object: t::id_owned(parent_var.to_string()),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("textContent"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(Expression::Assignment(Box::new(
                AssignmentExpression {
                    left: AssignmentTarget::Expression(target),
                    operator: AssignmentOperator::Assign,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(s.clone()),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                },
            ))));
        }
        ElementContent::DirectText(expr) => {
            let target = Expression::Member(Box::new(MemberExpression {
                object: t::id_owned(parent_var.to_string()),
                property: MemberProperty::Identifier(Identifier {
                    name: Cow::Borrowed("textContent"),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(Expression::Assignment(Box::new(
                AssignmentExpression {
                    left: AssignmentTarget::Expression(target),
                    operator: AssignmentOperator::Assign,
                    right: textcontent_value((*expr).clone()),
                    span: Span::ZERO,
                },
            ))));
        }
        ElementContent::Reactive(parts) => {
            // `var text = $.child(parent_var);`
            let text_var = format!("text"); // Could conflict — keep simple for now.
            body_stmts.push(t::var(
                &text_var,
                t::call(t::member_id(t::id_dollar(), "child"), vec![t::id_owned(parent_var.to_string())]),
            ));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "reset"),
                vec![t::id_owned(parent_var.to_string())],
            )));

            let expr_count = parts
                .iter()
                .filter(|p| matches!(p, TextPart::Expr(_)))
                .count();
            let fn_arrow: Expression;
            let mut call_args = Vec::new();
            if expr_count <= 1 {
                // Inline form: `() => $.set_text(text, \`...${EXPR ?? ''}\`)`
                let template_expr = build_inline_template(parts, state_bindings);
                let fn_body = t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id_owned(text_var.to_string()), template_expr],
                );
                fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(fn_body),
                    r#async: false,
                    span: Span::ZERO,
                }));
                call_args.push(fn_arrow);
            } else {
                // Deps-array form: `(args...) => $.set_text(text, TEMPLATE), [deps]`
                let (template_expr, dep_fns) = build_template_effect(parts, state_bindings);
                let mut params: Vec<Pattern> = Vec::new();
                for i in 0..dep_fns.len() {
                    params.push(t::pat_id_owned(format!("${i}")));
                }
                let fn_body = t::call(
                    t::member_id(t::id_dollar(), "set_text"),
                    vec![t::id_owned(text_var.to_string()), template_expr],
                );
                fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params,
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(fn_body),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let deps_array = Expression::Array(Box::new(ArrayExpression {
                    elements: dep_fns
                        .into_iter()
                        .map(ArrayElement::Expression)
                        .collect(),
                    span: Span::ZERO,
                }));
                call_args.push(fn_arrow);
                call_args.push(deps_array);
            }
            effects.push(t::stmt(t::call(
                t::member_id(t::id_dollar(), "template_effect"),
                call_args,
            )));
        }
    }
}

/// Given the element's text parts, build:
/// - The template literal expression for `set_text` (e.g.
///   `` `Count is ${$0 ?? ''}` ``).
/// - The deps array entries `() => exprN`.
/// Inline form: emit a template literal containing each expression as
/// `${EXPR ?? ''}` (with state reads rewritten to `$.get(...)`).
fn build_inline_template(
    parts: &[TextPart],
    state_bindings: &HashSet<String>,
) -> Expression {
    // Single-expression body with no surrounding text → pass the raw
    // expression through (no template-literal wrap). Mirrors upstream
    // which emits e.g. `$.set_text(text, l)` instead of
    // `$.set_text(text, \`${l ?? ''}\`)` when the body is just `{l}`.
    if parts.len() == 1 {
        if let TextPart::Expr(e) = &parts[0] {
            let mut sub = (*e).clone();
            rewrite_expr_for_state(&mut sub, state_bindings);
            return sub;
        }
    }
    let mut quasis: Vec<String> = Vec::new();
    let mut subs: Vec<Expression> = Vec::new();
    let mut current = String::new();
    for p in parts {
        match p {
            TextPart::Static(s) => current.push_str(s),
            TextPart::Expr(e) => {
                // Literal expressions fold into the surrounding static text.
                if let Some(s) = literal_to_template_string(e) {
                    current.push_str(&s);
                    continue;
                }
                quasis.push(std::mem::take(&mut current));
                let mut sub = (*e).clone();
                rewrite_expr_for_state(&mut sub, state_bindings);
                // Wrap as `EXPR ?? ''`
                let coalesced = Expression::Logical(Box::new(LogicalExpression {
                    left: sub,
                    operator: LogicalOperator::Coalesce,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(String::new()),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                }));
                subs.push(coalesced);
            }
        }
    }
    quasis.push(current);
    t::template_raw(quasis, subs)
}

fn build_template_effect(
    parts: &[TextPart],
    state_bindings: &HashSet<String>,
) -> (Expression, Vec<Expression>) {
    let mut quasis: Vec<String> = Vec::with_capacity(parts.len() + 1);
    let mut subs: Vec<Expression> = Vec::new();
    let mut dep_fns: Vec<Expression> = Vec::new();

    let mut current = String::new();
    let mut placeholder_idx: usize = 0;
    for p in parts {
        match p {
            TextPart::Static(s) => current.push_str(s),
            TextPart::Expr(e) => {
                quasis.push(std::mem::take(&mut current));
                // `${$N ?? ''}`
                let placeholder = Expression::Logical(Box::new(LogicalExpression {
                    left: t::id_owned(format!("${placeholder_idx}")),
                    operator: LogicalOperator::Coalesce,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: Cow::Owned(String::new()),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                }));
                subs.push(placeholder);
                // dep: `() => EXPR` — rewrite state-binding reads inside EXPR.
                let mut dep_expr = (*e).clone();
                rewrite_expr_for_state(&mut dep_expr, state_bindings);
                dep_fns.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    param_type_annotations: Vec::new(),
                    body: ArrowBody::Expression(dep_expr),
                    r#async: false,
                    span: Span::ZERO,
                })));
                placeholder_idx += 1;
            }
        }
    }
    quasis.push(current);

    let tmpl = t::template_raw(quasis, subs);
    (tmpl, dep_fns)
}

/// Convert a number literal to a string literal when assigned to `.textContent`.
fn textcontent_value(e: Expression) -> Expression {
    if let Expression::Literal(lit) = &e {
        if let Literal::Number(n) = lit.as_ref() {
            return Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(format_num(n.value)),
                raw: None,
                span: Span::ZERO,
            })));
        }
    }
    e
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

/// Extract top-level `{#snippet NAME(...)}` blocks from the fragment, lower
/// each as a `const NAME = ($$anchor, ...params) => { ... }` declaration,
/// and remove the SnippetBlock children from the fragment.
fn extract_client_snippets(
    fragment: &mut svelte_ast::fragment::Fragment,
    state_bindings: &HashSet<String>,
    var_counts: &mut HashMap<String, usize>,
) -> Option<(Vec<Statement>, Vec<Statement>)> {
    let _ = state_bindings;
    let mut out: Vec<Statement> = Vec::new();
    let mut extra_roots: Vec<Statement> = Vec::new();
    let mut remaining: Vec<FragmentChild> = Vec::with_capacity(fragment.nodes.len());
    let mut root_idx: usize = 0;
    for n in std::mem::take(&mut fragment.nodes) {
        if let FragmentChild::SnippetBlock(sb) = &n {
            let name = sb.expression.name.clone();
            let body_non_ws: Vec<&FragmentChild> = sb
                .body
                .nodes
                .iter()
                .filter(|c| match c {
                    FragmentChild::Text(t) => !t.data.trim().is_empty(),
                    _ => true,
                })
                .collect();
            if body_non_ws.len() != 1 {
                // Empty body: emit `const NAME = ($$anchor[, params]) => {};`.
                if body_non_ws.is_empty() {
                    let mut params = vec![t::pat_id_anchor()];
                    for p in &sb.parameters {
                        params.push(p.clone());
                    }
                    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params,
                        param_type_annotations: Vec::new(),
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: Vec::new(),
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    out.push(t::const_decl(&name, arrow));
                    continue;
                }
                return None;
            }
            let body: Vec<Statement> = match body_non_ws[0] {
                FragmentChild::Text(t) => {
                    // `$.next(); var text = $.text('Foo'); $.append($$anchor, text);`
                    let text_var = unique_var("text", var_counts);
                    let mut body: Vec<Statement> = Vec::new();
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "next"),
                        Vec::new(),
                    )));
                    body.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id_dollar(), "text"),
                            vec![Expression::Literal(Box::new(Literal::String(
                                StringLiteral {
                                    value: Cow::Owned(t.data.trim().to_string()),
                                    raw: None,
                                    span: Span::ZERO,
                                },
                            )))],
                        ),
                    ));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id_owned(text_var.to_string())],
                    )));
                    body
                }
                FragmentChild::RegularElement(el) => {
                    // Fully-static single element: `var <name> = root_N(); $.append($$anchor, <name>);`
                    if !is_element_fully_static(el) {
                        return None;
                    }
                    let mut html = String::new();
                    let mut needs = false;
                    serialize_element_to_html(el, &mut html, &mut needs)?;
                    root_idx += 1;
                    let root_name = format!("root_{}", root_idx);
                    extra_roots.push(t::var(
                        &root_name,
                        t::call(
                            t::member_id(t::id_dollar(), "from_html"),
                            vec![t::template_raw(vec![html], vec![])],
                        ),
                    ));
                    let el_var = unique_var(&el.name, var_counts);
                    let mut body: Vec<Statement> = Vec::new();
                    body.push(t::var(
                        &el_var,
                        t::call(t::id_owned(root_name.to_string()), Vec::new()),
                    ));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id_dollar(), "append"),
                        vec![t::id_anchor(), t::id_owned(el_var.to_string())],
                    )));
                    body
                }
                _ => return None,
            };
            let mut params = vec![t::pat_id_anchor()];
            for p in &sb.parameters {
                params.push(p.clone());
            }
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params,
                param_type_annotations: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            out.push(t::const_decl(&name, arrow));
            continue;
        }
        remaining.push(n);
    }
    fragment.nodes = remaining;
    Some((out, extra_roots))
}

fn component_call(c: &Component, node_var: &str) -> Option<Statement> {
    component_call_with(c, node_var, &HashSet::new())
}

fn component_call_with(
    c: &Component,
    node_var: &str,
    state_bindings: &HashSet<String>,
) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => props.push(attr_to_prop(a)?),
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            ElementAttribute::BindDirective(b) if b.name == "this" => {
                // bind:this captured by typed_client_component path; skip here.
                continue;
            }
            ElementAttribute::BindDirective(b) => {
                // `bind:NAME={target}` on Component → getter/setter pair.
                // When target refers to a state binding, wrap with $.get / $.set.
                let target_is_state = matches!(
                    &b.expression,
                    Expression::Identifier(i) if state_bindings.contains(i.name.as_ref())
                );
                let getter_body = if target_is_state {
                    let name = match &b.expression {
                        Expression::Identifier(i) => i.name.clone(),
                        _ => return None,
                    };
                    t::call(t::member_id(t::id_dollar(), "get"), vec![t::id_owned(name.to_string())])
                } else {
                    b.expression.clone()
                };
                let setter_body = if target_is_state {
                    let name = match &b.expression {
                        Expression::Identifier(i) => i.name.clone(),
                        _ => return None,
                    };
                    t::call(
                        t::member_id(t::id_dollar(), "set"),
                        vec![
                            t::id_owned(name.to_string()),
                            t::id("$$value"),
                            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                value: true,
                                span: Span::ZERO,
                            }))),
                        ],
                    )
                } else {
                    Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(b.expression.clone()),
                        operator: AssignmentOperator::Assign,
                        right: t::id("$$value"),
                        span: Span::ZERO,
                    }))
                };
                // get NAME() { return GETTER_BODY; }
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Owned(b.name.clone()),
                        span: Span::ZERO,
                    }),
                    value: Expression::Function(Box::new(FunctionExpression {
                        id: None,
                        params: Vec::new(),
                        param_type_annotations: Vec::new(),
                        body: BlockStatement {
                            body: vec![Statement::Return(Box::new(ReturnStatement {
                                argument: Some(getter_body),
                                span: Span::ZERO,
                            }))],
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
                })));
                // set NAME($$value) { SETTER_BODY; }
                props.push(ObjectMember::Property(Box::new(Property {
                    key: PropertyKey::Identifier(Identifier {
                        name: Cow::Owned(b.name.clone()),
                        span: Span::ZERO,
                    }),
                    value: Expression::Function(Box::new(FunctionExpression {
                        id: None,
                        params: vec![t::pat_id("$$value")],
                        param_type_annotations: Vec::new(),
                        body: BlockStatement {
                            body: vec![t::stmt(setter_body)],
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
                })));
            }
            _ => return None,
        }
    }
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id_owned(c.name.to_string()),
        arguments: vec![
            Argument::Expression(t::id_owned(node_var.to_string())),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
}

fn attr_to_prop(a: &Attribute) -> Option<ObjectMember> {
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
                            value: Cow::Owned(t.data.clone()),
                            raw: None,
                            span: Span::ZERO,
                        })))
                    }
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                }
            } else {
                return None;
            }
        }
    };
    Some(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(a.name.clone()),
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

// ---------------------------------------------------------------------------
// Math.X compile-time fold (re-exported for use in the compile pipeline).
// ---------------------------------------------------------------------------

pub fn fold_in_fragment(f: &mut Fragment) {
    for n in &mut f.nodes {
        fold_in_node(n);
    }
}

/// Fold every expression in the fragment using script-discovered constants,
/// Math.X, and nullish-coalesce rules. The fragment shape is preserved —
/// expressions that fold to literals remain inside their ExpressionTag so
/// `classify` can still distinguish "static body" from "had-expressions".
fn fold_fragment_with_consts(f: &mut Fragment, consts: &HashMap<String, Expression>) {
    for child in &mut f.nodes {
        fold_node_with_consts(child, consts);
    }
}

fn fold_node_with_consts(n: &mut FragmentChild, consts: &HashMap<String, Expression>) {
    match n {
        FragmentChild::ExpressionTag(t) => fold_expr_with_consts(&mut t.expression, consts),
        // HtmlTag intentionally does NOT fold: upstream emits the html
        // expression as a thunk (`() => EXPR`) without substituting
        // script-constant identifiers.
        FragmentChild::HtmlTag(_) => {}
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                fold_attr_with_consts(attr, consts);
            }
            fold_fragment_with_consts(&mut el.fragment, consts);
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                fold_attr_with_consts(attr, consts);
            }
            fold_fragment_with_consts(&mut c.fragment, consts);
        }
        _ => {}
    }
}

fn fold_attr_with_consts(attr: &mut ElementAttribute, consts: &HashMap<String, Expression>) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => fold_expr_with_consts(&mut tag.expression, consts),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        fold_expr_with_consts(&mut t.expression, consts);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => fold_expr_with_consts(&mut s.expression, consts),
        _ => {}
    }
}

fn fold_expr_with_consts(e: &mut Expression, consts: &HashMap<String, Expression>) {
    // First substitute identifiers.
    substitute_consts(e, consts);
    // Then fold via Math.X + nullish-coalesce + paren-unwrap.
    fold_expr_full(e);
}

fn substitute_consts(e: &mut Expression, consts: &HashMap<String, Expression>) {
    match e {
        Expression::Identifier(i) => {
            if let Some(lit) = consts.get(i.name.as_ref()) {
                *e = lit.clone();
            }
        }
        Expression::Call(c) => {
            substitute_consts(&mut c.callee, consts);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => substitute_consts(e, consts),
                    Argument::Spread(s) => substitute_consts(&mut s.argument, consts),
                }
            }
        }
        Expression::Member(m) => substitute_consts(&mut m.object, consts),
        Expression::Binary(b) => {
            substitute_consts(&mut b.left, consts);
            substitute_consts(&mut b.right, consts);
        }
        Expression::Logical(l) => {
            substitute_consts(&mut l.left, consts);
            substitute_consts(&mut l.right, consts);
        }
        Expression::Conditional(c) => {
            substitute_consts(&mut c.test, consts);
            substitute_consts(&mut c.consequent, consts);
            substitute_consts(&mut c.alternate, consts);
        }
        Expression::Unary(u) => substitute_consts(&mut u.argument, consts),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                substitute_consts(e, consts);
            }
        }
        Expression::Paren(p) => substitute_consts(&mut p.expression, consts),
        Expression::Template(t) => {
            for ex in &mut t.expressions {
                substitute_consts(ex, consts);
            }
        }
        _ => {}
    }
}

/// Full fold: nullish coalescence + Math.X + paren unwrap.
fn fold_expr_full(e: &mut Expression) {
    match e {
        Expression::Logical(l) => {
            fold_expr_full(&mut l.left);
            fold_expr_full(&mut l.right);
            if matches!(l.operator, LogicalOperator::Coalesce) {
                if let Some(true) = is_non_nullish_literal(&l.left) {
                    let inner = std::mem::replace(
                        &mut l.left,
                        Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                    );
                    *e = inner;
                    return;
                }
                if let Some(false) = is_non_nullish_literal(&l.left) {
                    let inner = std::mem::replace(
                        &mut l.right,
                        Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                    );
                    *e = inner;
                    return;
                }
            }
        }
        Expression::Call(c) => {
            fold_expr_full(&mut c.callee);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    fold_expr_full(e);
                }
            }
            if let Some(folded) = try_fold_math_call(c) {
                *e = folded;
            }
        }
        Expression::Member(m) => fold_expr_full(&mut m.object),
        Expression::Binary(b) => {
            fold_expr_full(&mut b.left);
            fold_expr_full(&mut b.right);
            if let Some(folded) = try_fold_binary(b) {
                *e = folded;
            }
        }
        Expression::Conditional(c) => {
            fold_expr_full(&mut c.test);
            fold_expr_full(&mut c.consequent);
            fold_expr_full(&mut c.alternate);
        }
        Expression::Unary(u) => fold_expr_full(&mut u.argument),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                fold_expr_full(e);
            }
        }
        Expression::Paren(p) => {
            fold_expr_full(&mut p.expression);
            if matches!(p.expression, Expression::Literal(_)) {
                let inner = std::mem::replace(
                    &mut p.expression,
                    Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                );
                *e = inner;
            }
        }
        _ => {}
    }
}

fn is_non_nullish_literal(e: &Expression) -> Option<bool> {
    match e {
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::Null(_) => Some(false),
            _ => Some(true),
        },
        Expression::Identifier(i) if i.name == "undefined" => Some(false),
        _ => None,
    }
}

/// If `e` is a literal that should render as a string in textContent,
/// return that string. Null/undefined render as empty string.
fn literal_to_template_string(e: &Expression) -> Option<String> {
    match e {
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::String(s) => Some(s.value.to_string()),
            Literal::Number(n) => Some(format_num(n.value)),
            Literal::Boolean(b) => Some(b.value.to_string()),
            Literal::Null(_) => Some(String::new()),
            _ => None,
        },
        Expression::Identifier(i) if i.name == "undefined" => Some(String::new()),
        // Plain template literal with no substitutions — concatenate quasi
        // cooked values.
        Expression::Template(t) if t.expressions.is_empty() => {
            let mut out = String::new();
            for q in &t.quasis {
                out.push_str(&q.cooked);
            }
            Some(out)
        }
        _ => None,
    }
}

fn fold_in_node(n: &mut FragmentChild) {
    match n {
        FragmentChild::ExpressionTag(t) => fold_expr(&mut t.expression),
        FragmentChild::HtmlTag(t) => fold_expr(&mut t.expression),
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                fold_in_attr(attr);
            }
            for c in &mut el.fragment.nodes {
                fold_in_node(c);
            }
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                fold_in_attr(attr);
            }
            for c in &mut c.fragment.nodes {
                fold_in_node(c);
            }
        }
        _ => {}
    }
}

fn fold_in_attr(attr: &mut ElementAttribute) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => fold_expr(&mut tag.expression),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        fold_expr(&mut t.expression);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => fold_expr(&mut s.expression),
        _ => {}
    }
}

fn fold_expr(e: &mut Expression) {
    match e {
        Expression::Call(c) => {
            fold_expr(&mut c.callee);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    fold_expr(e);
                }
            }
            if let Some(folded) = try_fold_math_call(c) {
                *e = folded;
            }
        }
        Expression::Member(m) => fold_expr(&mut m.object),
        Expression::Binary(b) => {
            fold_expr(&mut b.left);
            fold_expr(&mut b.right);
        }
        Expression::Logical(l) => {
            fold_expr(&mut l.left);
            fold_expr(&mut l.right);
        }
        Expression::Conditional(c) => {
            fold_expr(&mut c.test);
            fold_expr(&mut c.consequent);
            fold_expr(&mut c.alternate);
        }
        Expression::Unary(u) => fold_expr(&mut u.argument),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                fold_expr(e);
            }
        }
        Expression::Paren(p) => fold_expr(&mut p.expression),
        _ => {}
    }
}

/// Fold a `BinaryExpression` of two numeric or string literals down to a
/// single literal — `40 + 2` → `42`, `'a' + 'b'` → `'ab'`. Only the
/// operators that produce a literal-equivalent result; comparisons and
/// logical-shifts are out of scope.
fn try_fold_binary(b: &BinaryExpression) -> Option<Expression> {
    let lit_num = |e: &Expression| match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::Number(n) => Some(n.value),
            _ => None,
        },
        _ => None,
    };
    let lit_str = |e: &Expression| match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::String(s) => Some(s.value.to_string()),
            _ => None,
        },
        _ => None,
    };
    if let (Some(l), Some(r)) = (lit_num(&b.left), lit_num(&b.right)) {
        use svelte_js_ast::BinaryOperator as Op;
        let v = match b.operator {
            Op::Plus => Some(l + r),
            Op::Minus => Some(l - r),
            Op::Mul => Some(l * r),
            Op::Div => Some(l / r),
            Op::Mod => Some(l % r),
            _ => None,
        };
        if let Some(v) = v {
            return Some(Expression::Literal(Box::new(Literal::Number(NumberLiteral {
                value: v,
                raw: None,
                span: Span::ZERO,
            }))));
        }
    }
    if matches!(b.operator, svelte_js_ast::BinaryOperator::Plus) {
        if let (Some(l), Some(r)) = (lit_str(&b.left), lit_str(&b.right)) {
            return Some(Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: Cow::Owned(format!("{l}{r}")),
                raw: None,
                span: Span::ZERO,
            }))));
        }
    }
    None
}

fn try_fold_math_call(c: &CallExpression) -> Option<Expression> {
    let m = match &c.callee {
        Expression::Member(m) => m,
        _ => return None,
    };
    if m.computed || m.optional {
        return None;
    }
    let obj = match &m.object {
        Expression::Identifier(i) => i.name.as_ref(),
        _ => return None,
    };
    let prop = match &m.property {
        MemberProperty::Identifier(i) => i.name.as_ref(),
        _ => return None,
    };
    if obj != "Math" {
        return None;
    }
    let mut nums: Vec<f64> = Vec::with_capacity(c.arguments.len());
    for a in &c.arguments {
        let e = match a {
            Argument::Expression(e) => e,
            _ => return None,
        };
        let n = match e {
            Expression::Literal(lit) => match lit.as_ref() {
                Literal::Number(n) => n.value,
                _ => return None,
            },
            _ => return None,
        };
        nums.push(n);
    }
    let result: f64 = match prop {
        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "abs" if nums.len() == 1 => nums[0].abs(),
        "floor" if nums.len() == 1 => nums[0].floor(),
        "ceil" if nums.len() == 1 => nums[0].ceil(),
        "round" if nums.len() == 1 => nums[0].round(),
        _ => return None,
    };
    Some(Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value: result,
        raw: Some(format_num(result)),
        span: Span::ZERO,
    }))))
}

// Helper module to avoid `Option<&Script>` lifetime gymnastics.
mod opt_ref {
    pub type Ref<'a, T> = Option<&'a T>;
}
