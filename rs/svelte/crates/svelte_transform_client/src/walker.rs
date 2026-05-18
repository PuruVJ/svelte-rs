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

    // Extract top-level snippets — emit as `const NAME = ($$anchor, ...) => { ... };`
    // before the export. SnippetBlocks are removed from the fragment.
    // Pre-allocate `var_counts` so the snippet's `text` consumes the bare
    // slot; subsequent vars in the main function become `text_1` etc.
    let mut var_counts: HashMap<String, usize> = HashMap::new();
    let snippet_decls = extract_client_snippets(
        &mut fragment,
        &script.state_bindings,
        &mut var_counts,
    )?;
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
            // Non-async if-block not yet handled by walker.
            return None;
        }
        if let FragmentChild::SvelteElement(se) = nodes[0] {
            return emit_single_svelte_element_program(se, component_name, &script);
        }
        if let FragmentChild::Component(c) = nodes[0] {
            return emit_single_component_program(c, component_name, &script);
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
    let mut body_stmts: Vec<Statement> = Vec::new();
    let mut effects: Vec<Statement> = Vec::new(); // emitted after navigation

    // Start the function body with the rewritten script body.
    body_stmts.extend(script.body.clone());

    // var_counts was pre-seeded by extract_client_snippets so any name it
    // consumed (e.g. `text`) gets numbered (`text_1`) when used again here.
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
                        t::member_id(t::id("$"), "remove_input_defaults"),
                        vec![t::id(&var)],
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
                    body: ArrowBody::Expression(getter_body),
                    r#async: false,
                    span: Span::ZERO,
                }));
                // Pending: `null` if no pending body. (Non-empty pending
                // bodies aren't yet supported.)
                let pending = Expression::Literal(Box::new(Literal::Null(Span::ZERO)));
                let _ = &ab.pending;
                // Then: `($$anchor, PAT) => { body }`.
                let mut then_params = vec![t::pat_id("$$anchor")];
                if let Some(pat) = &ab.value {
                    then_params.push(pat.clone());
                }
                let then = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: then_params,
                    body: ArrowBody::Block(Box::new(BlockStatement {
                        body: Vec::new(),
                        span: Span::ZERO,
                    })),
                    r#async: false,
                    span: Span::ZERO,
                }));
                body_stmts.push(t::stmt(t::call(
                    t::member_id(t::id("$"), "await"),
                    vec![t::id(&var), getter, pending, then],
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
                            t::member_id(t::id("$"), "sibling"),
                            vec![t::id(
                                prev_var.as_deref().expect("preceding node"),
                            )],
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
                                    value: String::new(),
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
                    body_stmts.push(t::var(
                        &v,
                        t::call(
                            t::member_id(t::id("$"), "sibling"),
                            vec![t::id(prev_var.as_deref().expect("preceding node"))],
                        ),
                    ));
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
                                            value: String::new(),
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
                t::member_id(t::id("$"), "set_text"),
                vec![t::id(&text_var), template_expr],
            );
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Expression(set_call),
                r#async: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "template_effect"),
                vec![arrow],
            )));
        }
        _ => {
            // Combined form: `() => { $.set_text(t1, e1); $.set_text(t2, e2); ... }`.
            let mut block_body: Vec<Statement> = Vec::new();
            for (text_var, template_expr) in text_effects {
                block_body.push(t::stmt(t::call(
                    t::member_id(t::id("$"), "set_text"),
                    vec![t::id(&text_var), template_expr],
                )));
            }
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: block_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "template_effect"),
                vec![arrow],
            )));
        }
    }
    // Append other effects (delegated, bind_value etc.) after the
    // template_effect.
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
    if script.async_info.is_some() {
        prog.push(t::import_side_effect("svelte/internal/flags/async"));
    } else if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
// Class-only (empty template + class with runes) emission
// ---------------------------------------------------------------------------

fn emit_class_only_program(component_name: &str, script: &ScriptInfo) -> Option<Program> {
    let mut body: Vec<Statement> = Vec::new();
    // `$.push($$props, true);`
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "push"),
        vec![
            t::id("$$props"),
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ],
    )));
    body.extend(script.body.clone());
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "pop"),
        Vec::new(),
    )));

    let params = vec![t::pat_id("$$anchor"), t::pat_id("$$props")];
    let export = t::export_default_function(component_name, params, body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
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
                                        value: t.data.clone(),
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
                        name: a.name.clone(),
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
            t::member_id(t::id("$"), "next"),
            Vec::new(),
        )));
        slot_body.push(t::var(
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
        slot_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "template_effect"),
            vec![fn_arrow],
        )));
        slot_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id("$$anchor"), t::id("text")],
        )));
        let children_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$anchor"), t::pat_id("$$slotProps")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: slot_body,
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
        // `$$slots: { default: true }`
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

    let component_call = Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: vec![
            Argument::Expression(t::id("$$anchor")),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }));

    let mut func_body: Vec<Statement> = Vec::new();
    func_body.extend(script.body.clone());
    func_body.push(t::stmt(component_call));

    let mut params = vec![t::pat_id("$$anchor")];
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
    prog.extend(script.imports.clone());
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
    let mut body: Vec<Statement> = Vec::new();
    body.extend(script.body.clone());
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "next"),
        Vec::new(),
    )));
    body.push(t::var(
        "text",
        t::call(t::member_id(t::id("$"), "text"), Vec::new()),
    ));
    let set_call = t::call(
        t::member_id(t::id("$"), "set_text"),
        vec![t::id("text"), expr.clone()],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
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
        t::member_id(t::id("$"), "template_effect"),
        vec![effect_fn, void_zero_client(), void_zero_client(), blockers],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("text")],
    )));

    let mut params = vec![t::pat_id("$$anchor")];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, body);

    let mut prog: Vec<Statement> = Vec::with_capacity(3 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
        t::call(t::member_id(t::id("$"), "text"), Vec::new()),
    ));
    // template_effect:
    //   `($0) => $.set_text(TEXT, $0), void 0, [() => INNER_EXPR]`
    let set_text_call = t::call(
        t::member_id(t::id("$"), "set_text"),
        vec![t::id(text_name), t::id("$0")],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$0")],
        body: ArrowBody::Expression(set_text_call),
        r#async: false,
        span: Span::ZERO,
    }));
    let inner = strip_outer_await(expr);
    let dep_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(inner),
        r#async: false,
        span: Span::ZERO,
    }));
    let deps_array = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(dep_arrow)],
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "template_effect"),
        vec![effect_fn, void_zero_client(), deps_array],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(text_name)],
    )));
    Some(body)
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
        params: vec![t::pat_id("$$anchor")],
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
            params: vec![t::pat_id("$$anchor")],
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
        t::member_id(t::id("$"), "get"),
        vec![t::id("$$condition")],
    );
    let then_call = t::stmt(t::call(t::id("$$render"), vec![t::id("consequent")]));
    let else_call = if ib.alternate.is_some() {
        Some(t::stmt(t::call(
            t::id("$$render"),
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
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    async_inner_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "if"),
        vec![t::id("node"), render_arrow],
    )));

    // `$.async(node, [], [() => TEST_INNER], (node, $$condition) => { ... })`
    let promises_array = Expression::Array(Box::new(ArrayExpression {
        elements: vec![ArrayElement::Expression(Expression::Arrow(Box::new(
            ArrowFunctionExpression {
                params: Vec::new(),
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
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: async_inner_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let async_call = t::stmt(t::call(
        t::member_id(t::id("$"), "async"),
        vec![
            t::id("node"),
            blockers_array,
            promises_array,
            async_callback,
        ],
    ));

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
    func_body.push(async_call);
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
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
            const_names.push(id.name.clone());
            let idx = thunks.len();
            const_blocker_idx.insert(id.name.clone(), idx);
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
                    body: ArrowBody::Expression(inner_save_await),
                    r#async: true,
                    span: Span::ZERO,
                }));
                let async_derived_call = t::call(
                    t::member_id(t::id("$"), "async_derived"),
                    vec![async_derived_arrow],
                );
                let outer = save_await_call_client(async_derived_call);
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id(&id.name)),
                    operator: AssignmentOperator::Assign,
                    right: outer,
                    span: Span::ZERO,
                }));
                thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(assign),
                    r#async: true,
                    span: Span::ZERO,
                })));
            } else {
                // `() => X = $.derived(() => INIT_WITH_GET_REFS)`
                let rewritten = rewrite_const_refs_with_get(init, &const_names);
                let derived_call = t::call(
                    t::member_id(t::id("$"), "derived"),
                    vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        body: ArrowBody::Expression(rewritten),
                        r#async: false,
                        span: Span::ZERO,
                    }))],
                );
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id(&id.name)),
                    operator: AssignmentOperator::Assign,
                    right: derived_call,
                    span: Span::ZERO,
                }));
                thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
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
    let text_blocker_idx = *const_blocker_idx.get(&text_ref_name)?;

    // Build the consequent body.
    let mut consequent: Vec<Statement> = Vec::new();
    for name in &const_names {
        consequent.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: t::pat_id(name),
                init: None,
                span: Span::ZERO,
            }],
            span: Span::ZERO,
        })));
    }
    // var promises = $.run([...thunks])
    consequent.push(t::var(
        "promises",
        t::call(
            t::member_id(t::id("$"), "run"),
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
            t::member_id(t::id("$"), "child"),
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
        t::member_id(t::id("$"), "reset"),
        vec![t::id("p")],
    )));
    // $.template_effect(() => $.set_text(text, $.get(TEXT_REF)), void 0, void 0, [promises[N]])
    let get_text_ref = t::call(
        t::member_id(t::id("$"), "get"),
        vec![t::id(&text_ref_name)],
    );
    let set_text_call = t::call(
        t::member_id(t::id("$"), "set_text"),
        vec![t::id("text"), get_text_ref],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
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
        t::member_id(t::id("$"), "template_effect"),
        vec![
            effect_fn,
            void_zero_client(),
            void_zero_client(),
            blockers_array,
        ],
    )));
    // $.append($$anchor, p);
    consequent.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("p")],
    )));

    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
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
            t::id("$$render"),
            vec![t::id("consequent")],
        )),
        alternate: None,
        span: Span::ZERO,
    }));
    let render_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$render")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_if],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let if_call = t::stmt(t::call(
        t::member_id(t::id("$"), "if"),
        vec![t::id("node"), render_arrow],
    ));
    let wrap_block = Statement::Block(Box::new(BlockStatement {
        body: vec![t::var("consequent", consequent_arrow), if_call],
        span: Span::ZERO,
    }));

    // Build the function body.
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
    func_body.push(wrap_block);
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

    // root_1 template at module scope. Element with single ExpressionTag
    // text child → `<TAG> </TAG>` (single space placeholder).
    let template_html = format!("<{0}> </{0}>", element.name);
    let root_decl = t::var(
        "root_1",
        t::call(
            t::member_id(t::id("$"), "from_html"),
            vec![t::template_raw(vec![template_html], Vec::new())],
        ),
    );

    let mut params = vec![t::pat_id("$$anchor")];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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

/// `(await $.save(X))()` — wrap an expression for the async-const setter form.
fn save_await_call_client(inner: Expression) -> Expression {
    let saved = t::call(t::member_id(t::id("$"), "save"), vec![inner]);
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
            t::call(t::member_id(t::id("$"), "get"), vec![Expression::Identifier(id.clone())])
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
        _ => "$$item".to_string(),
    };
    let body_uses_item = expr_contains_ident(body_expr, &item_name);
    let each_flag: f64 = if body_uses_item { 17.0 } else { 16.0 };
    let mut each_body_stmts: Vec<Statement> = Vec::new();
    each_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id("$"), "next"),
        Vec::new(),
    )));
    each_body_stmts.push(t::var(
        "text",
        t::call(t::member_id(t::id("$"), "text"), Vec::new()),
    ));
    let set_text_call = t::call(
        t::member_id(t::id("$"), "set_text"),
        vec![t::id("text"), t::id("$0")],
    );
    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$0")],
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
            t::call(t::member_id(t::id("$"), "get"), vec![t::id(&i.name)])
        } else {
            body_inner
        }
    } else {
        body_inner
    };
    let dep_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(body_with_get),
        r#async: false,
        span: Span::ZERO,
    }));
    each_body_stmts.push(t::stmt(t::call(
        t::member_id(t::id("$"), "template_effect"),
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
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("text")],
    )));

    let each_callback = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor"), t::pat_id(&item_name)],
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
        body: ArrowBody::Expression(t::call(
            t::member_id(t::id("$"), "get"),
            vec![t::id("$$collection")],
        )),
        r#async: false,
        span: Span::ZERO,
    }));
    let mut each_args = vec![
        t::id("node"),
        t::lit_number(each_flag),
        getter,
        t::member_id(t::id("$"), "index"),
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
                    t::member_id(t::id("$"), "next"),
                    Vec::new(),
                )));
                fb_body.push(t::var(
                    "text_1",
                    t::call(t::member_id(t::id("$"), "text"), Vec::new()),
                ));
                let fb_set = t::call(
                    t::member_id(t::id("$"), "set_text"),
                    vec![t::id("text_1"), t::id("$0")],
                );
                let fb_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id("$0")],
                    body: ArrowBody::Expression(fb_set),
                    r#async: false,
                    span: Span::ZERO,
                }));
                let fb_dep = Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(inner_fb),
                    r#async: false,
                    span: Span::ZERO,
                }));
                fb_body.push(t::stmt(t::call(
                    t::member_id(t::id("$"), "template_effect"),
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
                    t::member_id(t::id("$"), "append"),
                    vec![t::id("$$anchor"), t::id("text_1")],
                )));
                each_args.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: vec![t::pat_id("$$anchor")],
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
        t::member_id(t::id("$"), "each"),
        each_args,
    ));

    let async_callback = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("node"), t::pat_id("$$collection")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![each_call],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let async_call = t::stmt(t::call(
        t::member_id(t::id("$"), "async"),
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
    func_body.push(async_call);
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
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
}

#[derive(Clone)]
struct AsyncInfo {
    setup_stmts: Vec<Statement>,
    async_bindings: HashSet<String>,
    last_group_idx: usize,
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
                            proxy_bindings.insert(id.name.clone());
                        } else if assigned.contains(&id.name) {
                            state_bindings.insert(id.name.clone());
                        }
                    } else if is_derived_call(init) {
                        derived_bindings.insert(id.name.clone());
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
    let mut saw_non_import = false;
    if has_class_with_runes {
        uses_runes = true;
    }
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
                        rest_props_bindings.insert(id.name.clone());
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
    let body_out = if let Some(ai) = &async_info {
        ai.setup_stmts.clone()
    } else {
        rest
    };

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
    })
}

fn has_top_level_await_in_body(body: &[Statement]) -> bool {
    body.iter().any(stmt_top_await)
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
                        hoisted_names.push(id.name.clone());
                        hoisted_spans.push(id.span);
                        let init = d
                            .init
                            .clone()
                            .unwrap_or_else(|| void_zero_client());
                        if expr_top_await(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.clone(),
                                init,
                            });
                        } else {
                            lowered.push(Lowered::Sync(Statement::Expression(Box::new(
                                ExpressionStatement {
                                    expression: Expression::Assignment(Box::new(
                                        AssignmentExpression {
                                            left: AssignmentTarget::Expression(t::id(&id.name)),
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
                    left: AssignmentTarget::Expression(t::id(&name)),
                    operator: AssignmentOperator::Assign,
                    right: init,
                    span: Span::ZERO,
                }));
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
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
    if !current_sync.is_empty() || last_was_async {
        if current_sync.is_empty() {
            current_sync.push(t::stmt(void_zero_client()));
        }
        flush_sync(&mut groups, &mut current_sync);
    }

    let last_group_idx = if groups.is_empty() { 0 } else { groups.len() - 1 };

    let mut setup_stmts: Vec<Statement> = Vec::new();
    if !hoisted_names.is_empty() {
        let decls: Vec<VariableDeclarator> = hoisted_names
            .iter()
            .enumerate()
            .map(|(i, n)| VariableDeclarator {
                id: Pattern::Identifier(Identifier {
                    name: n.clone(),
                    span: hoisted_spans.get(i).copied().unwrap_or(Span::ZERO),
                }),
                init: None,
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
            t::member_id(t::id("$"), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: groups.into_iter().map(ArrayElement::Expression).collect(),
                span: Span::ZERO,
            }))],
        ),
    ));

    let async_bindings: HashSet<String> = hoisted_names.into_iter().collect();
    Some(AsyncInfo {
        setup_stmts,
        async_bindings,
        last_group_idx,
    })
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
        Expression::Identifier(i) => names.contains(&i.name),
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
                if names.contains(&id.name) && is_props_call(init) {
                    let arr = Expression::Array(Box::new(ArrayExpression {
                        elements: ["$$slots", "$$events", "$$legacy"]
                            .iter()
                            .map(|n| {
                                ArrayElement::Expression(Expression::Literal(Box::new(
                                    Literal::String(StringLiteral {
                                        value: (*n).to_string(),
                                        raw: None,
                                        span: Span::ZERO,
                                    }),
                                )))
                            })
                            .collect(),
                        span: Span::ZERO,
                    }));
                    *init = t::call(
                        t::member_id(t::id("$"), "rest_props"),
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
                    if names.contains(&id.name) && !m.computed {
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
                            if state_bindings.contains(&id.name) && is_state_call(init) {
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
        Statement::Expression(_) => Some(s.clone()),
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
                                state_privates.insert(pi.name.clone());
                            } else if let PropertyKey::Identifier(id) = &p.key {
                                state_privates.insert(id.name.clone());
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
                            callee: t::member_id(t::id("$"), "state"),
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
                                name: "undefined".to_string(),
                                span: Span::ZERO,
                            }));
                        let derived_expr = if by {
                            t::call(t::member_id(t::id("$"), "derived"), vec![arg])
                        } else {
                            t::call(
                                t::member_id(t::id("$"), "derived"),
                                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                                    params: Vec::new(),
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
            t::member_id(t::id("$"), "get"),
            vec![this_private(public_name)],
        )),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: Vec::new(),
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
        t::member_id(t::id("$"), "set"),
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
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: vec![t::pat_id("value")],
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
            t::member_id(t::id("$"), "get"),
            vec![this_private(public_name)],
        )),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: Vec::new(),
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
        t::member_id(t::id("$"), "set"),
        vec![this_private(public_name), t::id("value")],
    ))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: vec![t::pat_id("value")],
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
            name: name.to_string(),
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
                            Some(pi.name.clone())
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
                            Some(pi.name.clone())
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
                if state_privates.contains(&name)
                    && matches!(a.operator, AssignmentOperator::Assign)
                {
                    rewrite_expr_for_class_state(&mut a.right, state_privates);
                    let rhs = std::mem::replace(
                        &mut a.right,
                        Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                    );
                    *e = t::call(
                        t::member_id(t::id("$"), "set"),
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
    c.callee = t::member_id(t::id("$"), "state");
}

/// `$state({...})` / `$state([...])` → `$.proxy({...})` (in-place).
fn lower_to_proxy_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    c.callee = t::member_id(t::id("$"), "proxy");
}

/// `$derived(EXPR)` → `$.derived(() => EXPR)`. `$derived.by(FN)` → `$.derived(FN)`.
fn lower_to_derived_init(init: &mut Expression) {
    let Expression::Call(c) = init else { return };
    let kp = global_keypath(&c.callee).unwrap_or_default();
    let by = kp == "$derived.by";
    c.callee = t::member_id(t::id("$"), "derived");
    if !by {
        // Wrap the first arg in `() => arg`.
        let arg = c.arguments.iter().find_map(|a| match a {
            Argument::Expression(e) => Some(e.clone()),
            _ => None,
        });
        if let Some(inner) = arg {
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
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
            ArrowBody::Expression(body_expr) => {
                // Special case: `() => X = V` where X is a state binding and
                // the operator is plain `=`. The arrow body's expression
                // value is observed (event handler return value), so emit
                // `$.set(X, V, true)` with the notify flag.
                let mut handled = false;
                if let E::Assignment(asgn) = body_expr {
                    if matches!(asgn.operator, AssignmentOperator::Assign) {
                        let lhs_name = match &asgn.left {
                            AssignmentTarget::Expression(E::Identifier(id)) => {
                                Some(id.name.clone())
                            }
                            AssignmentTarget::Pattern(Pattern::Identifier(id)) => {
                                Some(id.name.clone())
                            }
                            _ => None,
                        };
                        if let Some(name) = lhs_name {
                            if state.contains(&name) {
                                rewrite_expr_for_state(&mut asgn.right, state);
                                let rhs = std::mem::replace(
                                    &mut asgn.right,
                                    Expression::Literal(Box::new(Literal::Null(Span::ZERO))),
                                );
                                *body_expr = t::call(
                                    t::member_id(t::id("$"), "set"),
                                    vec![
                                        t::id(&name),
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
                            // `bind:NAME={target}` means the child may
                            // mutate `target`. Treat the target's root
                            // identifier as assigned so it gets state lowering.
                            if let Expression::Identifier(i) = &b.expression {
                                out.insert(i.name.clone());
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
                                out.insert(i.name.clone());
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
        NodeKind::StaticElement(el) => el.name.clone(),
        NodeKind::InterpElement(el, _, _) => el.name.clone(),
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
            t::call(t::member_id(t::id("$"), "child"), vec![t::id(parent_var)]),
        ));
        body_stmts.push(t::stmt(t::call(
            t::member_id(t::id("$"), "reset"),
            vec![t::id(parent_var)],
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
                    t::member_id(t::id("$"), "child"),
                    vec![
                        t::id(parent_var),
                        Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                            value: true,
                            span: Span::ZERO,
                        }))),
                    ],
                ),
            ));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "reset"),
                vec![t::id(parent_var)],
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
                body: ArrowBody::Expression(t::call(
                    t::member_id(t::id("$"), "set_text"),
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
                t::member_id(t::id("$"), "template_effect"),
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

/// Extract top-level `{#snippet NAME(...)}` blocks from the fragment, lower
/// each as a `const NAME = ($$anchor, ...params) => { ... }` declaration,
/// and remove the SnippetBlock children from the fragment.
fn extract_client_snippets(
    fragment: &mut svelte_ast::fragment::Fragment,
    state_bindings: &HashSet<String>,
    var_counts: &mut HashMap<String, usize>,
) -> Option<Vec<Statement>> {
    let _ = state_bindings;
    let mut out: Vec<Statement> = Vec::new();
    let mut remaining: Vec<FragmentChild> = Vec::with_capacity(fragment.nodes.len());
    for n in std::mem::take(&mut fragment.nodes) {
        if let FragmentChild::SnippetBlock(sb) = &n {
            let name = sb.expression.name.clone();
            // Snippet body: `$.next(); var text = $.text('Something'); $.append($$anchor, text);`
            // for a single static text body. More complex bodies fall back to
            // None (caller would bail).
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
                return None;
            }
            let text_value = match body_non_ws[0] {
                FragmentChild::Text(t) => t.data.trim().to_string(),
                _ => return None,
            };
            let text_var = unique_var("text", var_counts);
            let mut body: Vec<Statement> = Vec::new();
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "next"),
                Vec::new(),
            )));
            body.push(t::var(
                &text_var,
                t::call(
                    t::member_id(t::id("$"), "text"),
                    vec![Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: text_value,
                        raw: None,
                        span: Span::ZERO,
                    })))],
                ),
            ));
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id("$$anchor"), t::id(&text_var)],
            )));
            let mut params = vec![t::pat_id("$$anchor")];
            for p in &sb.parameters {
                params.push(p.clone());
            }
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params,
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
    Some(out)
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
                    Expression::Identifier(i) if state_bindings.contains(&i.name)
                );
                let getter_body = if target_is_state {
                    let name = match &b.expression {
                        Expression::Identifier(i) => i.name.clone(),
                        _ => return None,
                    };
                    t::call(t::member_id(t::id("$"), "get"), vec![t::id(&name)])
                } else {
                    b.expression.clone()
                };
                let setter_body = if target_is_state {
                    let name = match &b.expression {
                        Expression::Identifier(i) => i.name.clone(),
                        _ => return None,
                    };
                    t::call(
                        t::member_id(t::id("$"), "set"),
                        vec![
                            t::id(&name),
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
                        name: b.name.clone(),
                        span: Span::ZERO,
                    }),
                    value: Expression::Function(Box::new(FunctionExpression {
                        id: None,
                        params: Vec::new(),
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
                        name: b.name.clone(),
                        span: Span::ZERO,
                    }),
                    value: Expression::Function(Box::new(FunctionExpression {
                        id: None,
                        params: vec![t::pat_id("$$value")],
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
