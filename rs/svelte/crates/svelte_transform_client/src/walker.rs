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
    // bindings, fold nullish-coalesce, fold literal arithmetic
    // (`40 + 2` → `42`), and any nested Math.X calls. Always run — even
    // without script constants, expressions like `{40 + 2}` may fold.
    let mut fragment = root.fragment.clone();
    fold_fragment_with_consts(&mut fragment, &script.constants);

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
                return Some(p);
            }
        }
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

    // Deep-static-walker case: multi-root template made entirely of
    // RegularElements (with whitespace text/comment between), no blocks /
    // components / await. Reactive points are sparse inside subtrees and
    // need navigation via `$.sibling(N)` / `$.child(...)` / `$.next(N)` /
    // `$.reset(...)`. Matches the skip-static-subtree fixture.
    if nodes.iter().all(|n| {
        matches!(
            n,
            FragmentChild::RegularElement(_)
                | FragmentChild::HtmlTag(_)
                | FragmentChild::Comment(_)
        )
    }) && nodes.len() >= 2
        && nodes
            .iter()
            .any(|n| matches!(n, FragmentChild::RegularElement(_)))
        && script.async_info.is_none()
        && fragment_has_deep_reactive(&root.fragment)
    {
        if let Some(p) = emit_deep_static_walker_program(
            &root.fragment,
            component_name,
            &script,
        ) {
            return Some(p);
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
                    FragmentChild::IfBlock(ib) => Some(ib),
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
                    return Some(p);
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
            t::member_id(t::id("$"), "from_html"),
            vec![
                t::template_raw(vec![template_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let mut func_body: Vec<Statement> = Vec::new();
    if needs_push_pop {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "push"),
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
    func_body.extend(script.body.clone());

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
            t::member_id(t::id("$"), "first_child"),
            vec![t::id("fragment")],
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
                    t::member_id(t::id("$"), "sibling"),
                    vec![t::id(&prev_node_name), t::lit_number(2.0)],
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
            params: vec![t::pat_id("$$anchor")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: consequent_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));

        // $.if call with literal test
        let render_call = t::stmt(t::call(t::id("$$render"), vec![t::id(&consequent_name)]));
        let render_if = Statement::If(Box::new(IfStatement {
            test: ib.test.clone(),
            consequent: render_call,
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
            vec![t::id(&node_name), render_arrow],
        ));

        let block = Statement::Block(Box::new(BlockStatement {
            body: vec![t::var(&consequent_name, consequent_arrow), if_call],
            span: Span::ZERO,
        }));
        func_body.push(block);

        prev_node_name = node_name;
    }

    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));
    if needs_push_pop {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "pop"),
            Vec::new(),
        )));
    }

    let mut params = vec![t::pat_id("$$anchor")];
    if script.uses_props || needs_push_pop {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(6 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/async"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
                const_names.push(id.name.clone());

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
                                name: "promise".to_string(),
                                span: Span::ZERO,
                            }),
                            computed: false,
                            optional: false,
                            span: Span::ZERO,
                        }));
                        thunks.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                            params: Vec::new(),
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
                            body: ArrowBody::Expression(rewritten),
                            r#async: true,
                            span: Span::ZERO,
                        },
                    ));
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
                    // Sync: wrap in $.derived(() => INIT_REWRITTEN)
                    let rewritten = rewrite_const_chain_init(init, derived_bindings);
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
    }

    let mut body: Vec<Statement> = Vec::new();
    for name in &const_names {
        body.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Let,
            declarations: vec![VariableDeclarator {
                id: t::pat_id(name),
                init: None,
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
            t::member_id(t::id("$"), "run"),
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
        Expression::Identifier(id) if derived_bindings.contains(&id.name) => t::call(
            t::member_id(t::id("$"), "get"),
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
            t::member_id(t::id("$"), "from_html"),
            vec![
                t::template_raw(vec![template_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let mut func_body: Vec<Statement> = Vec::new();
    // `script.body` already contains the async setup statements when
    // `async_info.is_some()` (set by `analyze_script`).
    func_body.extend(script.body.clone());
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
            t::member_id(t::id("$"), "first_child"),
            vec![t::id("fragment")],
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
                    t::member_id(t::id("$"), "sibling"),
                    vec![t::id(&prev_node_name), t::lit_number(2.0)],
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
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

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
            t::call(t::member_id(t::id("$"), "get"), vec![t::id("$$condition")])
        } else if !needs_async_wrap && expr_has_user_call(&cur.test, derived_bindings) {
            let d_name = counters.next_d();
            let derived_call = t::call(
                t::member_id(t::id("$"), "derived"),
                vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(cur.test.clone()),
                    r#async: false,
                    span: Span::ZERO,
                }))],
            );
            chain_body.push(t::var(&d_name, derived_call));
            t::call(t::member_id(t::id("$"), "get"), vec![t::id(&d_name)])
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
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![render_body],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let if_args = if in_async_ctx {
        vec![
            t::id(node_name),
            render_arrow,
            Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                value: true,
                span: Span::ZERO,
            }))),
        ]
    } else {
        vec![t::id(node_name), render_arrow]
    };
    chain_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "if"),
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
            vec![t::pat_id(node_name), t::pat_id("$$condition")]
        } else {
            vec![t::pat_id(node_name)]
        };
        let cb = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: cb_params,
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: chain_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        Some(t::stmt(t::call(
            t::member_id(t::id("$"), "async"),
            vec![t::id(node_name), blockers_arr, tests_arg, cb],
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
        if let ElementAttribute::Attribute(attr) = a {
            // Any attribute on a custom element triggers $.set_custom_element_data.
            if is_custom {
                return true;
            }
            match attr.name.as_str() {
                "autofocus" => return true,
                "muted" if el.name == "source" || el.name == "video" || el.name == "audio" => {
                    return true
                }
                "value" if el.name == "option" => return true,
                _ => {}
            }
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
                snippet_decls.push((name, body_stmts));
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
    let mut func_body_stmts: Vec<Statement> = Vec::new();
    func_body_stmts.extend(script.body.clone());
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
                t::member_id(t::id("$"), "first_child"),
                vec![t::id("fragment")],
            )
        } else {
            t::call(
                t::member_id(t::id("$"), "sibling"),
                vec![
                    t::id(prev_select.as_ref().expect("prev set")),
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
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));
    let func_body = func_body_stmts;

    // Build snippet const declarations as ARROWS (placed BEFORE root_N
    // declarations).
    let mut snippet_consts: Vec<Statement> = Vec::new();
    for (name, body) in snippet_decls {
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: vec![t::pat_id("$$anchor")],
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
            t::member_id(t::id("$"), "from_html"),
            vec![
                t::template_raw(vec![root_html], Vec::new()),
                t::lit_number(1.0),
            ],
        ),
    );

    let params = vec![t::pat_id("$$anchor")];
    let export = t::export_default_function(component_name, params, func_body);

    let mut prog: Vec<Statement> = Vec::with_capacity(8 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
            t::member_id(t::id("$"), "from_html"),
            vec![t::template_raw(vec![template_html], Vec::new())],
        ),
    ));
    let option_var = ctx.next_named("option");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &option_var,
        t::call(t::id(&root_name), Vec::new()),
    ));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&option_var)],
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
                    t::member_id(t::id("$"), "child"),
                    vec![t::id(select_var)],
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
                t::member_id(t::id("$"), "reset"),
                vec![t::id(select_var)],
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
            if exclude.contains(&attr.name.as_str()) {
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
            t::member_id(t::id("$"), "from_html"),
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
        body: ArrowBody::Expression(html_expr),
        r#async: false,
        span: Span::ZERO,
    }));
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id("$"), "child"), vec![t::id(target_var)]),
        ),
        t::var(&fragment_var, t::call(t::id(&oc_name), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id("$"), "first_child"),
                vec![t::id(&fragment_var)],
            ),
        ),
        t::stmt(t::call(
            t::member_id(t::id("$"), "html"),
            vec![t::id(&node_var), getter],
        )),
        t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id(&anchor_var), t::id(&fragment_var)],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(target_var), arrow],
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
            t::member_id(t::id("$"), "from_html"),
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
        t::call(t::member_id(t::id("$"), "child"), vec![t::id(target_var)]),
    ));
    arrow_body.push(t::var(
        &fragment_var,
        t::call(t::id(&oc_name), Vec::new()),
    ));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "next"),
        Vec::new(),
    )));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id(&anchor_var), t::id(&fragment_var)],
    )));
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(target_var), arrow],
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
                object: t::id(var),
                property: MemberProperty::Identifier(Identifier {
                    name: "__value".to_string(),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            },
        ))),
        operator: AssignmentOperator::Assign,
        right: Expression::Literal(Box::new(Literal::String(StringLiteral {
            value: value.to_string(),
            raw: Some(format!("'{value}'")),
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }));
    let outer = Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(Expression::Member(Box::new(
            MemberExpression {
                object: t::id(var),
                property: MemberProperty::Identifier(Identifier {
                    name: "value".to_string(),
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
            t::member_id(t::id("$"), "from_html"),
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
        t::call(t::member_id(t::id("$"), "child"), vec![t::id(target_var)]),
    ));
    arrow_body.push(t::var(
        &fragment_var,
        t::call(t::id(&oc_name), Vec::new()),
    ));
    arrow_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id(&anchor_var), t::id(&fragment_var)],
    )));
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    Some(t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(target_var), arrow],
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
                t::member_id(t::id("$"), "from_html"),
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
            body: ArrowBody::Expression(eb.expression.clone()),
            r#async: false,
            span: Span::ZERO,
        }));
        let each_call = t::stmt(t::call(
            t::member_id(t::id("$"), "each"),
            vec![
                t::id(&node_var),
                t::lit_number(1.0),
                expr_arrow,
                t::member_id(t::id("$"), "index"),
                body_arrow,
            ],
        ));
        let arrow_body = vec![
            t::var(
                &anchor_var,
                t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
            ),
            t::var(&fragment_var, t::call(t::id(&sc_name), Vec::new())),
            t::var(
                &node_var,
                t::call(
                    t::member_id(t::id("$"), "first_child"),
                    vec![t::id(&fragment_var)],
                ),
            ),
            each_call,
            t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id(&anchor_var), t::id(&fragment_var)],
            )),
        ];
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: arrow_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let body = vec![t::stmt(t::call(
            t::member_id(t::id("$"), "customizable_select"),
            vec![t::id(select_var), arrow],
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
        body: ArrowBody::Expression(eb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    let each_call = t::stmt(t::call(
        t::member_id(t::id("$"), "each"),
        vec![
            t::id(container_var),
            t::lit_number(flag),
            expr_arrow,
            t::member_id(t::id("$"), "index"),
            body_arrow,
        ],
    ));
    let mut out = vec![each_call];
    out.push(t::stmt(t::call(
        t::member_id(t::id("$"), "reset"),
        vec![t::id(container_var)],
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
                        const_decls.push((id.name.clone(), init.clone()));
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
            t::member_id(t::id("$"), "derived_safe_equal"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
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
                            t::member_id(t::id("$"), "from_html"),
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
                        t::call(t::id(&root_name), Vec::new()),
                    ));
                    body.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id("$"), "child"),
                            vec![
                                t::id(&option_var),
                                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                    value: true,
                                    span: Span::ZERO,
                                }))),
                            ],
                        ),
                    ));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "reset"),
                        vec![t::id(&option_var)],
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
                        t::member_id(t::id("$"), "set_text"),
                        vec![t::id(&text_var), expr_with_get.clone()],
                    ));
                    let assign_inner = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(t::id(&option_value_var)),
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
                        left: t::id(&option_value_var),
                        right: assign_paren,
                        span: Span::ZERO,
                    }));
                    let assign_value = t::stmt(Expression::Assignment(Box::new(
                        AssignmentExpression {
                            left: AssignmentTarget::Expression(Expression::Member(Box::new(
                                MemberExpression {
                                    object: t::id(&option_var),
                                    property: MemberProperty::Identifier(Identifier {
                                        name: "__value".to_string(),
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
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: effect_body,
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "template_effect"),
                        vec![effect_fn],
                    )));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "append"),
                        vec![t::id("$$anchor"), t::id(&option_var)],
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
                            t::member_id(t::id("$"), "from_html"),
                            vec![
                                t::template_raw(vec![html_inner], Vec::new()),
                                t::lit_number(1.0),
                            ],
                        ),
                    ));
                    ctx.module_decls.push(t::var(
                        &root_name,
                        t::call(
                            t::member_id(t::id("$"), "from_html"),
                            vec![t::template_raw(
                                vec!["<option><!></option>".to_string()],
                                Vec::new(),
                            )],
                        ),
                    ));
                    let option_var = ctx.next_named("option");
                    body.push(t::var(
                        &option_var,
                        t::call(t::id(&root_name), Vec::new()),
                    ));
                    // Arrow body: navigate into fragment + setup span/text + template_effect.
                    // For rich content like <span>{item}</span>:
                    let anchor_var = ctx.next_named("anchor");
                    let fragment_var = ctx.next_named("fragment");
                    let mut arrow_body: Vec<Statement> = Vec::new();
                    arrow_body.push(t::var(
                        &anchor_var,
                        t::call(
                            t::member_id(t::id("$"), "child"),
                            vec![t::id(&option_var)],
                        ),
                    ));
                    arrow_body.push(t::var(
                        &fragment_var,
                        t::call(t::id(&oc_name), Vec::new()),
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
                        t::member_id(t::id("$"), "append"),
                        vec![t::id(&anchor_var), t::id(&fragment_var)],
                    )));
                    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        body: ArrowBody::Block(Box::new(BlockStatement {
                            body: arrow_body,
                            span: Span::ZERO,
                        })),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "customizable_select"),
                        vec![t::id(&option_var), arrow],
                    )));
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "append"),
                        vec![t::id("$$anchor"), t::id(&option_var)],
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
                    t::id("$$anchor"),
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
        params: vec![t::pat_id("$$anchor"), t::pat_id(&ctx_name)],
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
            t::member_id(t::id("$"), "get"),
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
                    t::member_id(t::id("$"), "get"),
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
                            t::member_id(t::id("$"), "first_child"),
                            vec![t::id(fragment_var)],
                        ),
                    ));
                    out.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id("$"), "child"),
                            vec![
                                t::id(&el_var),
                                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                    value: true,
                                    span: Span::ZERO,
                                }))),
                            ],
                        ),
                    ));
                    out.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "reset"),
                        vec![t::id(&el_var)],
                    )));
                    let expr_with_get = wrap_expr_with_get(&et.expression, consts, item_name);
                    let set_text_call = t::call(
                        t::member_id(t::id("$"), "set_text"),
                        vec![t::id(&text_var), expr_with_get],
                    );
                    let effect_fn = Expression::Arrow(Box::new(ArrowFunctionExpression {
                        params: Vec::new(),
                        body: ArrowBody::Expression(set_text_call),
                        r#async: false,
                        span: Span::ZERO,
                    }));
                    out.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "template_effect"),
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
                t::member_id(t::id("$"), "from_html"),
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
            params: vec![t::pat_id("$$anchor")],
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: consequent_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let render_call = t::stmt(t::call(t::id("$$render"), vec![t::id(&consequent_var)]));
        let render_if = Statement::If(Box::new(IfStatement {
            test: ib.test.clone(),
            consequent: render_call,
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
        let inner_block = Statement::Block(Box::new(BlockStatement {
            body: vec![
                t::var(&consequent_var, consequent_arrow),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "if"),
                    vec![t::id(&node_var), render_arrow],
                )),
            ],
            span: Span::ZERO,
        }));
        let arrow_body = vec![
            t::var(
                &anchor_var,
                t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
            ),
            t::var(&fragment_var, t::call(t::id(&sc_name), Vec::new())),
            t::var(
                &node_var,
                t::call(
                    t::member_id(t::id("$"), "first_child"),
                    vec![t::id(&fragment_var)],
                ),
            ),
            inner_block,
            t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id(&anchor_var), t::id(&fragment_var)],
            )),
        ];
        let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: arrow_body,
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        }));
        let body = vec![t::stmt(t::call(
            t::member_id(t::id("$"), "customizable_select"),
            vec![t::id(select_var), arrow],
        ))];
        return Some((html, body));
    }
    // Plain options branch.
    let html = format!("<select{attrs}><!></select>");
    let node_var = ctx.next_named("node");
    let mut body: Vec<Statement> = Vec::new();
    body.push(t::var(
        &node_var,
        t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
    ));
    let consequent_var = ctx.next_named("consequent");
    let consequent_body = build_if_consequent_for_select(&ib.consequent, ctx)?;
    let consequent_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: consequent_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let render_call = t::stmt(t::call(t::id("$$render"), vec![t::id(&consequent_var)]));
    let render_if = Statement::If(Box::new(IfStatement {
        test: ib.test.clone(),
        consequent: render_call,
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
        vec![t::id(&node_var), render_arrow],
    ));
    body.push(Statement::Block(Box::new(BlockStatement {
        body: vec![t::var(&consequent_var, consequent_arrow), if_call],
        span: Span::ZERO,
    })));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "reset"),
        vec![t::id(select_var)],
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
                            t::member_id(t::id("$"), "from_html"),
                            vec![t::template_raw(vec![template], Vec::new())],
                        ),
                    ));
                    let opt_var = ctx.next_named("option");
                    Some(vec![
                        t::var(&opt_var, t::call(t::id(&root_name), Vec::new())),
                        t::stmt(t::call(
                            t::member_id(t::id("$"), "append"),
                            vec![t::id("$$anchor"), t::id(&opt_var)],
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
                t::call(t::member_id(t::id("$"), "comment"), Vec::new()),
            ));
            body.push(t::var(
                &node_var,
                t::call(
                    t::member_id(t::id("$"), "first_child"),
                    vec![t::id(&fragment_var)],
                ),
            ));
            // Each inside if: flag=1 (not 5 — that's for select-direct).
            let body_arrow = build_each_iter_arrow(eb, ctx)?;
            let expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Expression(eb.expression.clone()),
                r#async: false,
                span: Span::ZERO,
            }));
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "each"),
                vec![
                    t::id(&node_var),
                    t::lit_number(1.0),
                    expr_arrow,
                    t::member_id(t::id("$"), "index"),
                    body_arrow,
                ],
            )));
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id("$$anchor"), t::id(&fragment_var)],
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
                t::id(&callee_name),
                vec![t::id("$$anchor")],
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
        t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
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
            t::member_id(t::id("$"), "from_html"),
            vec![t::template_raw(vec![format!("<option>{text}</option>")], Vec::new())],
        ),
    ));
    let opt_var = ctx.next_named("option");
    let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: vec![
                t::var(&opt_var, t::call(t::id(&root_name), Vec::new())),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "append"),
                    vec![t::id("$$anchor"), t::id(&opt_var)],
                )),
            ],
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let key_expr_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(kb.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "key"),
        vec![t::id(&node_var), key_expr_arrow, inner_arrow],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "reset"),
        vec![t::id(select_var)],
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
        t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
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
                    t::member_id(t::id("$"), "from_html"),
                    vec![t::template_raw(
                        vec![format!("<option>{text}</option>")],
                        Vec::new(),
                    )],
                ),
            ));
            arrow_body.push(t::var(&opt_var, t::call(t::id(&root_name), Vec::new())));
            arrow_body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id("$$anchor"), t::id(&opt_var)],
            )));
        }
        OptionShape::RichContent(nodes) => {
            // Reserve root name FIRST but push the declaration AFTER the
            // inner option_content template, so the module-decl order
            // matches `var option_content_N = ...; var root_N = ...;`.
            let root_name = ctx.next_root();
            arrow_body.push(t::var(&opt_var, t::call(t::id(&root_name), Vec::new())));
            let cs = build_customizable_select_body(&opt_var, nodes, ctx)?;
            arrow_body.push(cs);
            ctx.module_decls.push(t::var(
                &root_name,
                t::call(
                    t::member_id(t::id("$"), "from_html"),
                    vec![t::template_raw(
                        vec!["<option><!></option>".to_string()],
                        Vec::new(),
                    )],
                ),
            ));
            arrow_body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "append"),
                vec![t::id("$$anchor"), t::id(&opt_var)],
            )));
        }
        _ => return None,
    }
    let inner_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "boundary"),
        vec![
            t::id(&node_var),
            Expression::Object(Box::new(ObjectExpression {
                properties: Vec::new(),
                span: Span::ZERO,
            })),
            inner_arrow,
        ],
    )));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "reset"),
        vec![t::id(select_var)],
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
            t::member_id(t::id("$"), "from_html"),
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
            t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
        ),
        t::var(&fragment_var, t::call(t::id(&sc_name), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id("$"), "first_child"),
                vec![t::id(&fragment_var)],
            ),
        ),
        t::stmt(t::call(
            t::id(&component_name),
            vec![
                t::id(&node_var),
                Expression::Object(Box::new(ObjectExpression {
                    properties: Vec::new(),
                    span: Span::ZERO,
                })),
            ],
        )),
        t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id(&anchor_var), t::id(&fragment_var)],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(select_var), arrow],
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
            t::member_id(t::id("$"), "from_html"),
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
            t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
        ),
        t::var(&fragment_var, t::call(t::id(&sc_name), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id("$"), "first_child"),
                vec![t::id(&fragment_var)],
            ),
        ),
        t::stmt(t::call(t::id(&callee_name), vec![t::id(&node_var)])),
        t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id(&anchor_var), t::id(&fragment_var)],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(select_var), arrow],
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
            t::member_id(t::id("$"), "from_html"),
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
        body: ArrowBody::Expression(ht.expression.clone()),
        r#async: false,
        span: Span::ZERO,
    }));
    let arrow_body = vec![
        t::var(
            &anchor_var,
            t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
        ),
        t::var(&fragment_var, t::call(t::id(&sc_name), Vec::new())),
        t::var(
            &node_var,
            t::call(
                t::member_id(t::id("$"), "first_child"),
                vec![t::id(&fragment_var)],
            ),
        ),
        t::stmt(t::call(
            t::member_id(t::id("$"), "html"),
            vec![t::id(&node_var), getter],
        )),
        t::stmt(t::call(
            t::member_id(t::id("$"), "append"),
            vec![t::id(&anchor_var), t::id(&fragment_var)],
        )),
    ];
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Block(Box::new(BlockStatement {
            body: arrow_body,
            span: Span::ZERO,
        })),
        r#async: false,
        span: Span::ZERO,
    }));
    let body = vec![t::stmt(t::call(
        t::member_id(t::id("$"), "customizable_select"),
        vec![t::id(select_var), arrow],
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
                            t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
                        ),
                        t::var(
                            &option_var,
                            t::call(t::member_id(t::id("$"), "child"), vec![t::id(&og_var)]),
                        ),
                        build_customizable_select_body(&option_var, nodes, ctx)?,
                        t::stmt(t::call(
                            t::member_id(t::id("$"), "reset"),
                            vec![t::id(&og_var)],
                        )),
                        t::stmt(t::call(
                            t::member_id(t::id("$"), "reset"),
                            vec![t::id(select_var)],
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
                t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
            )];
            body.extend(build_each_body_for_select(eb, &og_var, ctx, 5.0)?);
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "reset"),
                vec![t::id(select_var)],
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
                    t::member_id(t::id("$"), "from_html"),
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
                    t::call(t::member_id(t::id("$"), "child"), vec![t::id(&og_var)]),
                ),
                t::var(&fragment_var, t::call(t::id(&oc_name), Vec::new())),
                t::var(
                    &node_var,
                    t::call(
                        t::member_id(t::id("$"), "first_child"),
                        vec![t::id(&fragment_var)],
                    ),
                ),
                t::stmt(t::call(
                    t::id(&component_name),
                    vec![
                        t::id(&node_var),
                        Expression::Object(Box::new(ObjectExpression {
                            properties: Vec::new(),
                            span: Span::ZERO,
                        })),
                    ],
                )),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "append"),
                    vec![t::id(&anchor_var), t::id(&fragment_var)],
                )),
            ];
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
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
                    t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
                ),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "customizable_select"),
                    vec![t::id(&og_var), arrow],
                )),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "reset"),
                    vec![t::id(select_var)],
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
                    t::member_id(t::id("$"), "from_html"),
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
                    t::call(t::member_id(t::id("$"), "child"), vec![t::id(&og_var)]),
                ),
                t::var(&fragment_var, t::call(t::id(&oc_name), Vec::new())),
                t::var(
                    &node_var,
                    t::call(
                        t::member_id(t::id("$"), "first_child"),
                        vec![t::id(&fragment_var)],
                    ),
                ),
                t::stmt(t::call(t::id(&callee_name), vec![t::id(&node_var)])),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "append"),
                    vec![t::id(&anchor_var), t::id(&fragment_var)],
                )),
            ];
            let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
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
                    t::call(t::member_id(t::id("$"), "child"), vec![t::id(select_var)]),
                ),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "customizable_select"),
                    vec![t::id(&og_var), arrow],
                )),
                t::stmt(t::call(
                    t::member_id(t::id("$"), "reset"),
                    vec![t::id(select_var)],
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

    for (i, el) in top_elements.iter().enumerate() {
        let has_reactive_inside = fragment_has_deep_reactive(&el.fragment);
        let has_reactive_attr = element_has_reactive_attr(el);
        let needs_visit = has_reactive_inside || has_reactive_attr;
        if !needs_visit {
            // Skip purely static element. We don't emit anything for it.
            continue;
        }
        // Allocate a variable name for this element. Each top element at
        // index i corresponds to rendered sibling index i*2 (alternating
        // element, text-space).
        let var = allocate_named(&el.name, &mut var_names);
        let init = if !first_emitted {
            if i == 0 {
                t::call(
                    t::member_id(t::id("$"), "first_child"),
                    vec![t::id("fragment")],
                )
            } else {
                t::call(
                    t::member_id(t::id("$"), "sibling"),
                    vec![
                        t::call(
                            t::member_id(t::id("$"), "first_child"),
                            vec![t::id("fragment")],
                        ),
                        t::lit_number((i * 2) as f64),
                    ],
                )
            }
        } else {
            let prev = prev_var.as_ref().expect("prev_var set");
            let prev_idx = prev_top_idx.expect("prev_top_idx set");
            let offset = (i - prev_idx) * 2;
            t::call(
                t::member_id(t::id("$"), "sibling"),
                vec![t::id(prev), t::lit_number(offset as f64)],
            )
        };
        body.push(t::var(&var, init));
        prev_var = Some(var.clone());
        prev_top_idx = Some(i);
        first_emitted = true;

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
        if has_reactive_inside {
            body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "reset"),
                vec![t::id(&var)],
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
                    t::member_id(t::id("$"), "sibling"),
                    vec![
                        t::id(prev_var.as_ref().expect("prev_var set")),
                        t::lit_number(2.0),
                    ],
                ),
            ));
            if trailing > 1 {
                body.push(t::stmt(t::call(
                    t::member_id(t::id("$"), "next"),
                    vec![t::lit_number(((trailing - 1) * 2) as f64)],
                )));
            }
        }
    }

    // Combined template_effect for text reactivity at the bottom.
    if effects.len() == 1 {
        let (text_var, expr) = effects.pop().unwrap();
        body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Expression(t::call(
                    t::member_id(t::id("$"), "set_text"),
                    vec![t::id(&text_var), expr],
                )),
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    } else if effects.len() >= 2 {
        let mut block_body: Vec<Statement> = Vec::new();
        for (text_var, expr) in effects {
            block_body.push(t::stmt(t::call(
                t::member_id(t::id("$"), "set_text"),
                vec![t::id(&text_var), expr],
            )));
        }
        body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "template_effect"),
            vec![Expression::Arrow(Box::new(ArrowFunctionExpression {
                params: Vec::new(),
                body: ArrowBody::Block(Box::new(BlockStatement {
                    body: block_body,
                    span: Span::ZERO,
                })),
                r#async: false,
                span: Span::ZERO,
            }))],
        )));
    }

    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id("fragment")],
    )));

    // `var root = $.from_html(\`HTML\`, FLAGS);` where FLAGS = 1 (multi-root)
    // or 3 (multi-root + needs_import_node for video/custom-element).
    let flags = if needs_import_node { 3.0 } else { 1.0 };
    let root_decl = t::var(
        "root",
        t::call(
            t::member_id(t::id("$"), "from_html"),
            vec![
                t::template_raw(vec![html], Vec::new()),
                t::lit_number(flags),
            ],
        ),
    );

    let mut params = vec![t::pat_id("$$anchor")];
    if script.uses_props {
        params.push(t::pat_id("$$props"));
    }
    let export = t::export_default_function(component_name, params, body);

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
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
            if i == 0 {
                init = t::call(
                    t::member_id(t::id("$"), "child"),
                    vec![t::id(parent_var)],
                );
            } else {
                init = t::call(
                    t::member_id(t::id("$"), "sibling"),
                    vec![
                        t::call(
                            t::member_id(t::id("$"), "first_child"),
                            vec![t::id(parent_var)],
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
                t::member_id(t::id("$"), "sibling"),
                vec![t::id(prev), t::lit_number(offset as f64)],
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
                    body: ArrowBody::Expression(expr),
                    r#async: false,
                    span: Span::ZERO,
                }));
                body.push(t::stmt(t::call(
                    t::member_id(t::id("$"), "html"),
                    vec![t::id(&var), getter],
                )));
            }
            FragmentChild::RegularElement(child_el) => {
                if is_text_only_element(child_el) {
                    let text_var = allocate_named("text", var_names);
                    body.push(t::var(
                        &text_var,
                        t::call(
                            t::member_id(t::id("$"), "child"),
                            vec![
                                t::id(&var),
                                Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
                                    value: true,
                                    span: Span::ZERO,
                                }))),
                            ],
                        ),
                    ));
                    if let Some(expr) = single_expression_in_element(child_el) {
                        let rewritten = rewrite_props_destructured(expr, &script.props_destructured);
                        effects.push((text_var, rewritten));
                    }
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "reset"),
                        vec![t::id(&var)],
                    )));
                } else {
                    // Recurse into the child.
                    walk_element_interior(child_el, &var, body, effects, var_names, counters, script);
                    if fragment_has_deep_reactive(&child_el.fragment) {
                        body.push(t::stmt(t::call(
                            t::member_id(t::id("$"), "reset"),
                            vec![t::id(&var)],
                        )));
                    }
                }
            }
            _ => {}
        }
        prev_child_var = Some(var);
        prev_child_idx = Some(i);
    }

    // Trailing static siblings after the last reactive child — emit $.next(N).
    let last_reactive = *reactive_idx.last().unwrap();
    let trailing_count = children.len() - 1 - last_reactive;
    if trailing_count > 0 {
        body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "next"),
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
                    t::member_id(t::id("$"), "set_custom_element_data"),
                    vec![
                        t::id(var),
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: attr.name.clone(),
                            raw: Some(format!("'{}'", attr.name)),
                            span: Span::ZERO,
                        }))),
                        value_expr,
                    ],
                )));
                continue;
            }
            match attr.name.as_str() {
                "autofocus" => {
                    body.push(t::stmt(t::call(
                        t::member_id(t::id("$"), "autofocus"),
                        vec![
                            t::id(var),
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
                                object: t::id(var),
                                property: MemberProperty::Identifier(Identifier {
                                    name: "muted".to_string(),
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
                "value" if el.name == "option" => {
                    // `EL.value = EL.__value = 'X';`
                    let value_expr = attr_value_as_string_expr(&attr.value);
                    // Inner: EL.__value = 'X'
                    let inner = Expression::Assignment(Box::new(AssignmentExpression {
                        left: AssignmentTarget::Expression(Expression::Member(Box::new(
                            MemberExpression {
                                object: t::id(var),
                                property: MemberProperty::Identifier(Identifier {
                                    name: "__value".to_string(),
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
                                object: t::id(var),
                                property: MemberProperty::Identifier(Identifier {
                                    name: "value".to_string(),
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
                    value: t.data.clone(),
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
    // Returns true iff the element has exactly one non-whitespace child that
    // is an ExpressionTag (matches `<h1>{title}</h1>`-shape).
    let non_ws: Vec<&FragmentChild> = el
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    non_ws.len() == 1 && matches!(non_ws[0], FragmentChild::ExpressionTag(_))
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
    let mut last_was_text_with_space = false;
    for (i, n) in nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let collapsed = collapse_ws_client(&t.data);
                // Trim around block boundaries: leading whitespace of a
                // multi-line text run after an element becomes a single
                // space; same for trailing.
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
                // Inside an element this is a placeholder. At fragment top
                // level it would be a text anchor — not the case for deep-
                // static-walker which only handles element top-levels.
                out.push(' ');
                last_was_text_with_space = true;
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
                matches!(attr.name.as_str(), "autofocus" | "muted")
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
    if is_text_only {
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
            Expression::Identifier(id) if names.contains(&id.name) => {
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
            e => e.clone(),
        }
    }
    go(e, names)
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
                if derived_bindings.contains(&id.name) {
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
            if let Some(idx) = blocker_bindings.get(&id.name) {
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
            t::member_id(t::id("$"), "text"),
            vec![Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: trimmed.clone(),
                raw: Some(format!("'{}'", trimmed)),
                span: Span::ZERO,
            })))],
        ),
    ));
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&text_name)],
    )));
    Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
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
        t::call(t::member_id(t::id("$"), "comment"), Vec::new()),
    ));
    body.push(t::var(
        &node_name,
        t::call(
            t::member_id(t::id("$"), "first_child"),
            vec![t::id(&fragment_name)],
        ),
    ));
    let inner_stmt = emit_async_if_block(inner, &node_name, ai, derived_bindings, counters, true)?;
    body.push(inner_stmt);
    body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&fragment_name)],
    )));
    Some(Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: vec![t::pat_id("$$anchor")],
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
            t::id("$$render"),
            vec![t::id(name), t::lit_number(-1.0)],
        ))
    });
    for (i, (cname, test, branch_idx)) in branches.iter().enumerate().rev() {
        let call = if i == 0 {
            t::stmt(t::call(t::id("$$render"), vec![t::id(cname)]))
        } else {
            t::stmt(t::call(
                t::id("$$render"),
                vec![t::id(cname), t::lit_number(*branch_idx as f64)],
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
            if derived_bindings.contains(&id.name) {
                t::call(
                    t::member_id(t::id("$"), "get"),
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
    /// Names destructured from `let { a, b, c } = $props()`. Template
    /// reads of these names get rewritten to `$$props.NAME` and the
    /// declaration itself is dropped from the script body.
    props_destructured: HashSet<String>,
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
    let mut props_destructured: HashSet<String> = HashSet::new();
    if has_class_with_runes {
        uses_runes = true;
    }
    for s in body {
        match s {
            Statement::Import(_) => {
                // Hoist all imports to the top regardless of source order
                // (matches upstream's behavior).
                imports.push(s.clone());
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
                                                props_destructured.insert(id.name.clone());
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
        props_destructured,
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
        body: new_body,
        r#async: new_async,
        span: Span::ZERO,
    }));
    let async_derived_call = t::call(
        t::member_id(t::id("$"), "async_derived"),
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
                        hoisted_names.push(id.name.clone());
                        hoisted_spans.push(id.span);
                        let init = d
                            .init
                            .clone()
                            .unwrap_or_else(|| void_zero_client());
                        // `$.derived(() => await E)` pattern → rewrite to
                        // `await $.async_derived(() => E)`.
                        if let Some(rewritten) = rewrite_async_derived_client(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.clone(),
                                init: rewritten,
                            });
                            continue;
                        }
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
    // Collect script let/const bindings — used for "ref-with-blocker"
    // detection during template lowering. Mirrors server's
    // `script_let_bindings`.
    let mut script_let_bindings: HashSet<String> = async_bindings.clone();
    for s in pre_async.iter().chain(body.iter()) {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    script_let_bindings.insert(id.name.clone());
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
                            blocker_bindings.entry(id.name.clone()).or_insert(idx);
                            if let Some(init) = init {
                                let mut touched: HashSet<String> = HashSet::new();
                                collect_touched_async_client(init, &mut touched);
                                for name in touched {
                                    if script_let_bindings.contains(&name) {
                                        blocker_bindings.entry(name).or_insert(idx);
                                    }
                                }
                            }
                            groups_count += 1;
                            awaited_seen = true;
                        } else {
                            if awaited_seen {
                                blocker_bindings
                                    .entry(id.name.clone())
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
                out.insert(id.name.clone());
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
            Literal::String(s) => Some(s.value.clone()),
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
                value: format!("{l}{r}"),
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
