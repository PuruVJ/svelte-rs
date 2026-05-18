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

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::{Component, RegularElement};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client_walker(root: &Root, component_name: &str) -> Option<Program> {
    try_typed_client_walker_with(root, component_name, false)
}

pub fn try_typed_client_walker_with(
    root: &Root,
    component_name: &str,
    use_tree: bool,
) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    // Script analysis: collect statements to emit, plus any erased rune
    // bindings. The assignment scan also considers template expressions so
    // `onclick={()=>count++}` registers `count` as assigned even when the
    // script has no direct mutation.
    let template_assigned = scan_fragment_assignments(&root.fragment);
    let script = analyze_script(root.instance.as_ref(), &template_assigned)?;

    // Apply script-context fold to the fragment: inline plain `let X = LIT`
    // bindings, fold nullish-coalesce, and any nested Math.X calls. Mutates
    // a local clone of the fragment so we don't disturb the caller.
    let mut fragment = root.fragment.clone();
    if !script.constants.is_empty() {
        fold_fragment_with_consts(&mut fragment, &script.constants);
    }
    let root_owned = svelte_ast::root::Root {
        fragment,
        ..root.clone()
    };
    let root = &root_owned;

    // Collect top-level non-ws nodes.
    let nodes: Vec<&FragmentChild> = root
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if nodes.is_empty() {
        return None;
    }

    // Special case: a single top-level `{#each}` block uses a different
    // emission path (no `var root` at module scope; the function body
    // creates `$.comment()` and dispatches to `$.each(...)`). Done before
    // `classify` because classify doesn't yet know about EachBlock nodes.
    if nodes.len() == 1 {
        if let FragmentChild::EachBlock(eb) = nodes[0] {
            return emit_single_each_program(eb, component_name, &script);
        }
        if let FragmentChild::SvelteElement(se) = nodes[0] {
            return emit_single_svelte_element_program(se, component_name, &script);
        }
    }

    let classified: Vec<NodeKind> = nodes.iter().map(|n| classify(n)).collect::<Option<_>>()?;

    let is_multi_root = nodes.len() > 1;
    // Tree-mode: skip the html/body walking entirely and emit a fully-static
    // `$.from_tree(...)` template + minimal body. Only static fragments are
    // supported in tree mode for now.
    if use_tree {
        return emit_tree_program(&classified, component_name, is_multi_root);
    }
    let mut html = String::with_capacity(64);
    let mut body_stmts: Vec<Statement> = Vec::new();
    let mut effects: Vec<Statement> = Vec::new(); // emitted after navigation

    // Start the function body with the rewritten script body.
    body_stmts.extend(script.body.clone());

    let mut var_counts: HashMap<String, usize> = HashMap::new();
    let mut prev_var: Option<String> = None;

    // Root holder: for multi-root we own a `fragment` variable; for single-root
    // the root element variable IS the holder.
    let root_holder: String;

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
                let include_body = matches!(content, ElementContent::StaticOnly);
                let needs_reactive_body = matches!(content, ElementContent::Reactive(_));
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
                        t::member_id(t::id("$"), "remove_input_defaults"),
                        vec![t::id(&var)],
                    )));
                }
                emit_element_content(
                    content,
                    &var,
                    &mut body_stmts,
                    &mut effects,
                    &script.state_bindings,
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
                body_stmts.push(component_call(c, &var)?);
            }
        }
        if is_multi_root && i < last_idx {
            html.push(' ');
        }
    }

    // Append effects (template_effect calls etc.) after all navigation.
    body_stmts.extend(effects);

    // Final append.
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&root_holder)],
    )));

    // Module-level `var root = $.from_html(\`HTML\`[, 1]);`
    let mut from_html_args = vec![t::template_raw(vec![html], vec![])];
    if is_multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_decl = t::var(
        "root",
        t::call(t::member_id(t::id("$"), "from_html"), from_html_args),
    );

    let mut params = vec![t::pat_id("$$anchor")];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, body_stmts);

    let mut prog: Vec<Statement> = Vec::with_capacity(6 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
                            value: n,
                            raw: None,
                            span: Span::ZERO,
                        },
                    ))))
                })
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id("$"), "delegate"),
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
            body: ArrowBody::Expression({
                if state_bindings.contains(&target_name) {
                    t::call(
                        t::member_id(t::id("$"), "get"),
                        vec![t::id(&target_name)],
                    )
                } else {
                    t::id(&target_name)
                }
            }),
            r#async: false,
            span: Span::ZERO,
        }));
        let setter = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$value")],
            body: ArrowBody::Expression({
                if state_bindings.contains(&target_name) {
                    t::call(
                        t::member_id(t::id("$"), "set"),
                        vec![t::id(&target_name), t::id("$$value")],
                    )
                } else {
                    Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id(&target_name)),
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
            t::member_id(t::id("$"), "bind_value"),
            vec![t::id(var), getter, setter],
        )));
    }
    // Event handlers: `\$.delegated("click", var, handler)`.
    for (event, handler) in &dirs.events {
        delegated_events.insert(event.clone());
        let mut handler_expr = (*handler).clone();
        rewrite_expr_for_state(&mut handler_expr, state_bindings);
        effects.push(t::stmt(t::call(
            t::member_id(t::id("$"), "delegated"),
            vec![
                Expression::Literal(Box::new(Literal::String(StringLiteral {
                    value: event.clone(),
                    raw: None,
                    span: Span::ZERO,
                }))),
                t::id(var),
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
                value: " ".to_string(),
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
        t::call(t::member_id(t::id("$"), "from_tree"), from_tree_args),
    );

    // Body: `var fragment = root(); $.next(N); $.append($$anchor, fragment);`
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var("fragment", t::call(t::id("root"), vec![])));
    if is_multi_root {
        body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "next"),
            vec![t::lit_number(classified.len() as f64)],
        )));
    }
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
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
        value: el.name.clone(),
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
                            value: s,
                            raw: None,
                            span: Span::ZERO,
                        })))
                    }
                    _ => return None,
                };
                props.push(ObjectMember::Property(Box::new(Property {
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
                        value: collapse_ws(&std::mem::take(&mut pending_text)),
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
            value: collapse_ws(&pending_text),
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
// Single top-level <svelte:element> emission
// ---------------------------------------------------------------------------

fn emit_single_svelte_element_program(
    se: &svelte_ast::elements::SvelteElement,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program> {
    // Only handles `<svelte:element this={EXPR} />` with no children + no
    // other attributes for now.
    if !se.attributes.is_empty() || !se.fragment.nodes.is_empty() {
        return None;
    }
    let mut func_body: Vec<Statement> = Vec::new();
    func_body.extend(script.body.clone());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id("$"), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(
            t::member_id(t::id("$"), "first_child"),
            vec![t::id("fragment")],
        ),
    ));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "element"),
        vec![
            t::id("node"),
            se.tag.clone(),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: false,
                span: Span::ZERO,
            }))),
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

    let mut params = vec![t::pat_id("$$anchor")];
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
    prog.extend(script.imports.clone());
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Single top-level {#each} emission
// ---------------------------------------------------------------------------

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
    if body_nodes.len() == 1 {
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
                    t::member_id(t::id("$"), "from_html"),
                    vec![t::template_raw(vec![html], vec![])],
                ),
            ));
            let var = el.name.clone();
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
                    object: t::id(&var),
                    property: MemberProperty::Identifier(Identifier {
                        name: "textContent".to_string(),
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
                    t::member_id(t::id("$"), "set_attribute"),
                    vec![
                        t::id(&var),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: a.name.clone(),
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
                    t::member_id(t::id("$"), "delegated"),
                    vec![
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: event.clone(),
                            raw: None,
                            span: Span::ZERO,
                        }))),
                        t::id(&var),
                        (*handler).clone(),
                    ],
                )));
            }
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id("$$anchor"), t::id(&var)],
            )));
        } else {
            return None;
        }
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
            t::member_id(t::id("$"), "next"),
            Vec::new(),
        )));
        body_stmts.push(t::var(
            "text",
            t::call(t::member_id(t::id("$"), "text"), Vec::new()),
        ));
        let inline = build_inline_template(&parts, &script.state_bindings);
        let fn_body = t::call(
            t::member_id(t::id("$"), "set_text"),
            vec![t::id("text"), inline],
        );
        let fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            body: ArrowBody::Expression(fn_body),
            r#async: false,
            span: Span::ZERO,
        }));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id("$"), "template_effect"),
            vec![fn_arrow],
        )));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id("$$anchor"), t::id("text")],
        )));
    }

    // Build the each-call arrow params: `($$anchor, ITEM, INDEX?)` or
    // `($$anchor, $$item, INDEX)` if no context.
    let mut params = vec![t::pat_id("$$anchor")];
    if let Some(ctx) = &eb.context {
        params.push(ctx.clone());
    } else {
        params.push(t::pat_id("$$item"));
    }
    if let Some(idx) = &eb.index {
        params.push(t::pat_id(idx));
    }

    let body_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params,
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: body_stmts,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));

    // Build top-level function body.
    let mut func_body: Vec<Statement> = Vec::new();
    func_body.extend(script.body.clone());
    func_body.push(t::var(
        "fragment",
        t::call(t::member_id(t::id("$"), "comment"), Vec::new()),
    ));
    func_body.push(t::var(
        "node",
        t::call(t::member_id(t::id("$"), "first_child"), vec![t::id("fragment")]),
    ));

    // `$.each(node, 0, () => EXPR, $.index, body_arrow)`
    let getter = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(eb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    let key_fn = t::member_id(t::id("$"), "index");
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "each"),
        vec![
            t::id("node"),
            t::lit_number(0.0),
            getter,
            key_fn,
            body_arrow,
        ],
    )));
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

    let mut params = vec![t::pat_id("$$anchor")];
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
    prog.extend(script.imports.clone());
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
                            value: n,
                            raw: None,
                            span: Span::ZERO,
                        },
                    ))))
                })
                .collect(),
            span: Span::ZERO,
        }));
        prog.push(t::stmt(t::call(
            t::member_id(t::id("$"), "delegate"),
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
        });
    };

    let body = &script.content.body;
    let mut assigned: HashSet<String> = collect_assigned_targets(body);
    assigned.extend(template_assigned.iter().cloned());

    // First pass: discover which $state bindings need lowering to $.state.
    let mut state_bindings: HashSet<String> = HashSet::new();
    for s in body {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if is_state_call(init) && assigned.contains(&id.name) {
                        state_bindings.insert(id.name.clone());
                    }
                }
            }
        }
    }

    let mut imports: Vec<Statement> = Vec::new();
    let mut rest: Vec<Statement> = Vec::new();
    let mut uses_runes = false;
    let mut uses_props = false;
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
                    if assigned.contains(&id.name) || state_bindings.contains(&id.name) {
                        continue;
                    }
                    if is_literal_expression(init) {
                        constants.insert(id.name.clone(), init.clone());
                    }
                }
            }
        }
    }

    Some(ScriptInfo {
        imports,
        body: rest,
        emit_legacy_flag: !uses_runes,
        state_bindings,
        constants,
        uses_props,
    })
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
                                            value: key_name,
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
                                    Some(t::call(t::member_id(t::id("$"), "prop"), args)),
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
    matches!(e, Expression::Literal(_))
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
                    // First, try strip (binding never assigned).
                    let stripped = try_strip_state(init, &d.id, assigned, uses_runes);
                    if !stripped {
                        // Try lower $state(V) → $.state(V) if this binding is
                        // a state binding.
                        if let Pattern::Identifier(id) = &d.id {
                            if state_bindings.contains(&id.name) && is_state_call(init) {
                                *uses_runes = true;
                                lower_state_init(init);
                                continue;
                            }
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
        Statement::Expression(_) => Some(s.clone()),
        _ => None,
    }
}

/// `$state(V)` → `$.state(V)` (in-place).
fn lower_state_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    c.callee = t::member_id(t::id("$"), "state");
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
            if state.contains(&i.name) {
                let name = i.name.clone();
                *e = t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]);
            }
        }
        E::Assignment(a) => {
            // Try to detect `X = ...` or `X OP= ...` where X is a state binding.
            // The LHS may be either Expression(Identifier) or Pattern(Identifier)
            // depending on the parser path.
            let lhs_name: Option<String> = match &a.left {
                AssignmentTarget::Expression(E::Identifier(id)) => Some(id.name.clone()),
                AssignmentTarget::Pattern(Pattern::Identifier(id)) => Some(id.name.clone()),
                _ => None,
            };
            if let Some(name) = lhs_name {
                if state.contains(&name) {
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
                                t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]),
                                rhs,
                            ),
                            AssignmentOperator::SubAssign => binop(
                                BinaryOperator::Minus,
                                t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]),
                                rhs,
                            ),
                            AssignmentOperator::MulAssign => binop(
                                BinaryOperator::Mul,
                                t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]),
                                rhs,
                            ),
                            AssignmentOperator::DivAssign => binop(
                                BinaryOperator::Div,
                                t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]),
                                rhs,
                            ),
                            AssignmentOperator::ModAssign => binop(
                                BinaryOperator::Mod,
                                t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)]),
                                rhs,
                            ),
                            _ => rhs,
                        };
                        *e = t::call(
                            t::member_id(t::id("$"), "set"),
                            vec![t::id(&name), new_value],
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
                if state.contains(&id.name) {
                    let name = id.name.clone();
                    let increment_args = match u.operator {
                        UpdateOperator::Increment => vec![t::id(&name)],
                        UpdateOperator::Decrement => vec![t::id(&name), t::lit_number(-1.0)],
                    };
                    *e = t::call(t::member_id(t::id("$"), "update"), increment_args);
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
            ArrowBody::Expression(e) => rewrite_expr_for_state(e, state),
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
    if assigned.contains(&name) {
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
        name: "undefined".to_string(),
        span: Span::ZERO,
    }));
    true
}

fn pattern_single_ident(p: &Pattern) -> Option<String> {
    match p {
        Pattern::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

fn is_state_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$state" | "$state.raw" | "$state.eager")
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
        Expression::Identifier(i) => Some(i.name.clone()),
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
                            scan_expr_for_assignments(&b.expression, out)
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
                            scan_expr_for_assignments(&b.expression, out)
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
                out.insert(i.name.clone());
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
                out.insert(i.name.clone());
            }
        }
        AssignmentTarget::Pattern(p) => collect_pattern_idents(p, out),
    }
}

fn collect_pattern_idents(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Identifier(i) => {
            out.insert(i.name.clone());
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
        NodeKind::StaticElement(el) => el.name.clone(),
        NodeKind::InterpElement(el, _, _) => el.name.clone(),
        NodeKind::Component(_) => "fragment".to_string(),
    }
}

fn classify(n: &FragmentChild) -> Option<NodeKind<'_>> {
    match n {
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
            t::member_id(t::id("$"), "sibling"),
            vec![t::id(p), t::lit_number(2.0)],
        )
    } else {
        t::call(t::member_id(t::id("$"), "first_child"), vec![t::id("fragment")])
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

fn emit_element_content(
    content: &ElementContent,
    parent_var: &str,
    body_stmts: &mut Vec<Statement>,
    effects: &mut Vec<Statement>,
    state_bindings: &HashSet<String>,
) {
    match content {
        ElementContent::NoContent | ElementContent::StaticOnly => {}
        ElementContent::FoldedText(s) => {
            let target = Expression::Member(Box::new(MemberExpression {
                object: t::id(parent_var),
                property: MemberProperty::Identifier(Identifier {
                    name: "textContent".to_string(),
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
                        value: s.clone(),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                },
            ))));
        }
        ElementContent::DirectText(expr) => {
            let target = Expression::Member(Box::new(MemberExpression {
                object: t::id(parent_var),
                property: MemberProperty::Identifier(Identifier {
                    name: "textContent".to_string(),
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
                t::call(t::member_id(t::id("$"), "child"), vec![t::id(parent_var)]),
            ));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "reset"),
                vec![t::id(parent_var)],
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
                    t::member_id(t::id("$"), "set_text"),
                    vec![t::id(&text_var), template_expr],
                );
                fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
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
                    params.push(t::pat_id(&format!("${i}")));
                }
                let fn_body = t::call(
                    t::member_id(t::id("$"), "set_text"),
                    vec![t::id(&text_var), template_expr],
                );
                fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params,
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
                t::member_id(t::id("$"), "template_effect"),
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
                        value: String::new(),
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
                    left: t::id(&format!("${placeholder_idx}")),
                    operator: LogicalOperator::Coalesce,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: String::new(),
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
                value: format_num(n.value),
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

fn component_call(c: &Component, node_var: &str) -> Option<Statement> {
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
            _ => return None,
        }
    }
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: vec![
            Argument::Expression(t::id(node_var)),
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
                            value: t.data.clone(),
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
        FragmentChild::HtmlTag(t) => fold_expr_with_consts(&mut t.expression, consts),
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
            if let Some(lit) = consts.get(&i.name) {
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
            Literal::String(s) => Some(s.value.clone()),
            Literal::Number(n) => Some(format_num(n.value)),
            Literal::Boolean(b) => Some(b.value.to_string()),
            Literal::Null(_) => Some(String::new()),
            _ => None,
        },
        Expression::Identifier(i) if i.name == "undefined" => Some(String::new()),
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

fn try_fold_math_call(c: &CallExpression) -> Option<Expression> {
    let m = match &c.callee {
        Expression::Member(m) => m,
        _ => return None,
    };
    if m.computed || m.optional {
        return None;
    }
    let obj = match &m.object {
        Expression::Identifier(i) => i.name.as_str(),
        _ => return None,
    };
    let prop = match &m.property {
        MemberProperty::Identifier(i) => i.name.as_str(),
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
