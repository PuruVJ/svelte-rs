//! Phase 3 client transform.
//!
//! Mirrors `packages/svelte/src/compiler/phases/3-transform/client/`.
//!
//! Coverage so far: structural skeleton + static HTML + simple Component
//! invocations + HMR wrapper + runes lowering (client-side: `$state(x)` →
//! `$.state(x)`, `$derived(x)` → `$.derived(() => x)`, etc.).
//!
//! The real 53-visitor port is being filled in incrementally driven by
//! failing snapshot fixtures. See `rs_port_status.md` for the up-to-date
//! progress sheet.

#![forbid(unsafe_code)]

use serde_json::Value;
use svelte_ast::root::Root;
use svelte_transform_shared::builders as b;

pub mod rewrite;
pub mod template;

pub use template::serialize_static_html;

/// Options forwarded to the client transform. Matches the relevant subset of
/// upstream's `ValidatedCompileOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub hmr: bool,
    pub dev: bool,
    pub filename: Option<String>,
    /// When set to `Tree`, multi-root templates emit `\$.from_tree(...)` with
    /// an array-of-arrays structure instead of `\$.from_html(\`...\`)`.
    pub fragments: FragmentsMode,
    /// `experimental.async` — enables the async pipeline (`\$.run`,
    /// `\$.async`, `\$.async_derived`, `\$.save`, deps-aware
    /// `\$.template_effect`). Imports `svelte/internal/flags/async`.
    pub experimental_async: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FragmentsMode {
    #[default]
    Html,
    Tree,
}

/// Transform a parsed `Root` into a client-side ESTree `Program`.
pub fn client_component(root: &Root, component_name: &str) -> Value {
    client_component_with_options(root, component_name, &ClientOptions::default())
}

/// Like [`client_component`] but accepts compile options (HMR, dev mode, etc.).
pub fn client_component_with_options(
    root: &Root,
    component_name: &str,
    options: &ClientOptions,
) -> Value {
    let html = template::serialize_static_html(&root.fragment);
    let runes_mode = uses_runes(root);

    let mut program_body: Vec<Value> = Vec::new();
    program_body.push(import_side_effect("svelte/internal/disclose-version"));
    if options.experimental_async {
        program_body.push(import_side_effect("svelte/internal/flags/async"));
    } else if !runes_mode {
        program_body.push(import_side_effect("svelte/internal/flags/legacy"));
    }
    program_body.push(b::import_all("$", "svelte/internal/client"));

    // Collect template-side reassignments (e.g. `onclick={()=>count++}`)
    // so the rewrite pass doesn't incorrectly unwrap `$.state` as
    // never-reassigned.
    let template_reassignments = collect_template_reassignments(&root.fragment);

    // Hoisted instance imports + state-name collection (for template-side
    // expression rewriting).
    let (rewritten_instance, state_names): (Option<Value>, std::collections::HashSet<String>) =
        match root.instance.as_ref() {
            Some(i) => {
                let (prog, names) = rewrite::rewrite_program_with_state_and_hints(
                    i.content.clone(),
                    &template_reassignments,
                );
                (Some(prog), names)
            }
            None => (None, Default::default()),
        };

    // Rewrite template-embedded expressions to use $.get/$.set for state.
    let mut root_owned: Root = root.clone();
    if !state_names.is_empty() {
        rewrite_fragment_state_refs(&mut root_owned.fragment, &state_names);
    }
    // Hoist top-level {#snippet name(...)}{/snippet} blocks. Each becomes
    // `const name = ($$anchor, ...params) => { ... };` in program scope.
    let hoisted_snippets = extract_top_level_snippets_client(&mut root_owned.fragment);
    let root = &root_owned;

    if let Some(rewritten) = &rewritten_instance {
        let body = rewritten
            .get("body")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for stmt in body {
            if stmt.get("type").and_then(|v| v.as_str()) == Some("ImportDeclaration") {
                program_body.push(stmt);
            }
        }
    }

    // Insert hoisted snippet declarations between imports and the `var root` /
    // component function.
    for snip in &hoisted_snippets {
        program_body.push(snip.clone());
    }

    let mut fn_body: Vec<Value> = Vec::new();

    // Async transform produces a mapping of var-name → last $$promises index
    // that touched it (for template_effect deps).
    let mut async_var_last_idx: std::collections::HashMap<String, usize> = Default::default();

    // Instance-script non-import statements appear inside the component fn.
    if let Some(rewritten) = &rewritten_instance {
        let body = rewritten
            .get("body")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let non_imports: Vec<Value> = body
            .into_iter()
            .filter(|stmt| {
                stmt.get("type").and_then(|v| v.as_str()) != Some("ImportDeclaration")
            })
            .collect();
        if options.experimental_async && body_has_top_level_await(&non_imports) {
            let (transformed, last_idx) = transform_async_script(non_imports);
            async_var_last_idx = last_idx;
            for stmt in transformed {
                fn_body.push(stmt);
            }
        } else {
            for stmt in non_imports {
                fn_body.push(stmt);
            }
        }
    }

    // Collect compile-time constant bindings (let X = literal, never-reassigned)
    // for downstream folding inside templates.
    let constants: std::collections::HashMap<String, String> = {
        let body = rewritten_instance
            .as_ref()
            .and_then(|p| p.get("body"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut all_reassigned = template_reassignments.clone();
        all_reassigned.extend(state_names.iter().cloned());
        collect_constant_bindings(&body, &all_reassigned)
    };

    // Each hoisted snippet that creates a `var text = ...` bumps the counter
    // for subsequent text var allocations.
    let text_var_start: usize = hoisted_snippets
        .iter()
        .filter(|s| snippet_declares_text(s))
        .count();

    // Tree-mode templates: build `\$.from_tree(<array>, 1)` if the fragment
    // is a pure-static multi-root tree and the compile option is enabled.
    if options.fragments == FragmentsMode::Tree {
        if let Some(tree) = try_tree_template(&root.fragment) {
            let root_count = count_top_level_elements(&root.fragment);
            program_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("root"),
                    Some(b::call(
                        b::member(b::id("$"), b::id("from_tree"), false, false),
                        vec![tree, b::literal_num(1.0)],
                    )),
                )],
            ));
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("fragment"),
                    Some(b::call(b::id("root"), vec![])),
                )],
            ));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("next"), false, false),
                vec![b::literal_num(root_count as f64)],
            )));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::id("fragment")],
            )));
            return finalize_program(program_body, fn_body, component_name, options);
        }
    }

    // Try the multi-root static template pattern (e.g. `<p>...</p> <Component .../>`).
    if let Some((tpl_html, fn_stmts)) = try_multi_root_static(&root.fragment, &constants, text_var_start) {
        program_body.push(b::declaration(
            "var",
            vec![b::declarator(
                b::id("root"),
                Some(b::call(
                    b::member(b::id("$"), b::id("from_html"), false, false),
                    vec![b::template_literal(vec![&tpl_html], vec![]), b::literal_num(1.0)],
                )),
            )],
        ));
        for s in fn_stmts {
            fn_body.push(s);
        }
        return finalize_program(program_body, fn_body, component_name, options);
    }

    let has_meaningful_html = !html.trim().is_empty();
    if has_meaningful_html {
        program_body.push(b::declaration(
            "var",
            vec![b::declarator(
                b::id("root"),
                Some(b::call(
                    b::member(b::id("$"), b::id("from_html"), false, false),
                    vec![b::template_literal(vec![&html], vec![])],
                )),
            )],
        ));

        let single_root_name = guess_single_root_var(&root.fragment.nodes);
        if let Some(top_name) = single_root_name {
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id(&top_name),
                    Some(b::call(b::id("root"), vec![])),
                )],
            ));
            // If the single root element has dynamic ExpressionTag children
            // (no static text), emit the $.child + $.reset + $.template_effect
            // pattern.
            if let Some(el) = find_single_dynamic_text_element(&root.fragment.nodes) {
                let exprs: Vec<Value> = el
                    .fragment
                    .nodes
                    .iter()
                    .filter_map(|n| match n {
                        svelte_ast::fragment::FragmentChild::ExpressionTag(t) => {
                            Some(t.expression.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if !exprs.is_empty() {
                    let child_args = if options.experimental_async {
                        vec![b::id(&top_name), b::literal_bool(true)]
                    } else {
                        vec![b::id(&top_name)]
                    };
                    fn_body.push(b::declaration(
                        "var",
                        vec![b::declarator(
                            b::id("text"),
                            Some(b::call(
                                b::member(b::id("$"), b::id("child"), false, false),
                                child_args,
                            )),
                        )],
                    ));
                    fn_body.push(b::stmt(b::call(
                        b::member(b::id("$"), b::id("reset"), false, false),
                        vec![b::id(&top_name)],
                    )));
                    if options.experimental_async {
                        fn_body.push(b::stmt(build_template_effect_set_text_async(
                            "text",
                            &exprs,
                            &async_var_last_idx,
                        )));
                    } else {
                        fn_body.push(b::stmt(build_template_effect_set_text(
                            "text", &exprs,
                        )));
                    }
                }
            }
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::id(&top_name)],
            )));
        } else {
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::call(b::id("root"), vec![])],
            )));
        }
    }

    // If the only template content is a single Component invocation, emit
    // `Foo($$anchor, props)` directly inside the body.
    if !has_meaningful_html {
        if let Some(single_comp) = find_single_component(&root.fragment.nodes) {
            let call_with_directives = build_component_call(single_comp, runes_mode);
            for stmt in call_with_directives {
                fn_body.push(stmt);
            }
        } else if let Some(svelte_el) = find_single_svelte_element(&root.fragment.nodes) {
            // `<svelte:element this={tag}>...</svelte:element>` client lowering.
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("fragment"),
                    Some(b::call(
                        b::member(b::id("$"), b::id("comment"), false, false),
                        vec![],
                    )),
                )],
            ));
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("node"),
                    Some(b::call(
                        b::member(b::id("$"), b::id("first_child"), false, false),
                        vec![b::id("fragment")],
                    )),
                )],
            ));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("element"), false, false),
                vec![b::id("node"), svelte_el.tag.clone(), b::literal_bool(false)],
            )));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::id("fragment")],
            )));
        } else if let Some(blk) = find_single_each_block(&root.fragment.nodes) {
            if options.experimental_async && expression_uses_await(&blk.expression) {
                for stmt in build_async_each_block_client(blk) {
                    fn_body.push(stmt);
                }
            } else {
                // Single `{#each}` block at root.
                let (prog_extras, fn_stmts) = build_each_block_client(blk);
                for s in prog_extras {
                    program_body.push(s);
                }
                for stmt in fn_stmts {
                    fn_body.push(stmt);
                }
            }
        } else if let Some(blk) = find_single_if_block(&root.fragment.nodes) {
            if options.experimental_async && expression_uses_await(&blk.test) {
                for stmt in build_async_if_block_client(blk) {
                    fn_body.push(stmt);
                }
            } else {
                for stmt in build_if_block_client(blk) {
                    fn_body.push(stmt);
                }
            }
        } else if options.experimental_async {
            if let Some(expr) = find_single_root_expression_tag(&root.fragment.nodes) {
                // Sole-text template body in async mode: $.next() + $.text() +
                // template_effect with deps.
                fn_body.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("next"), false, false),
                    vec![],
                )));
                fn_body.push(b::declaration(
                    "var",
                    vec![b::declarator(
                        b::id("text"),
                        Some(b::call(
                            b::member(b::id("$"), b::id("text"), false, false),
                            vec![],
                        )),
                    )],
                ));
                fn_body.push(b::stmt(build_template_effect_set_text_async(
                    "text",
                    &[expr],
                    &async_var_last_idx,
                )));
                fn_body.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("append"), false, false),
                    vec![b::id("$$anchor"), b::id("text")],
                )));
            }
        }
    }

    let delegated_events = collect_delegated_event_names(&fn_body);

    // In runes mode, certain features need `$.push($$props, true)` /
    // `$.pop()` wrapping (mirrors upstream's `should_inject_context`).
    let needs_push_pop = runes_mode && body_needs_context(&fn_body);
    if needs_push_pop {
        fn_body.insert(
            0,
            b::stmt(b::call(
                b::member(b::id("$"), b::id("push"), false, false),
                vec![b::id("$$props"), b::literal_bool(true)],
            )),
        );
        fn_body.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("pop"), false, false),
            vec![],
        )));
    }

    // Detect $$props usage in body (via fn_body's identifiers) so we know
    // whether to include the parameter.
    let uses_props = body_uses_identifier(&fn_body, "$$props");
    let mut params = vec![b::id("$$anchor")];
    if uses_props {
        params.push(b::id("$$props"));
    }
    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        b::block(fn_body),
        false,
    );

    let _ = delegated_events.clone();
    if options.hmr {
        // HMR wrapper: declare component as `function`, wrap with $.hmr,
        // accept module updates, export at the end.
        program_body.push(component_fn);
        program_body.push(serde_json::json!({
            "type": "IfStatement",
            "test": {
                "type": "MemberExpression",
                "object": {
                    "type": "MetaProperty",
                    "meta": { "type": "Identifier", "name": "import" },
                    "property": { "type": "Identifier", "name": "meta" }
                },
                "property": { "type": "Identifier", "name": "hot" },
                "computed": false,
                "optional": false
            },
            "consequent": b::block(vec![
                b::stmt(b::assignment("=",
                    b::id(component_name),
                    b::call(
                        b::member(b::id("$"), b::id("hmr"), false, false),
                        vec![b::id(component_name)],
                    ),
                )),
                b::stmt(b::call(
                    b::member(
                        b::member(
                            b::member(
                                b::id("import"),
                                b::id("meta"),
                                false, false,
                            ),
                            b::id("hot"),
                            false, false,
                        ),
                        b::id("accept"),
                        false, false,
                    ),
                    vec![b::arrow(
                        vec![b::id("module")],
                        b::block(vec![
                            b::stmt(b::call(
                                b::member(
                                    b::member(
                                        b::id(component_name),
                                        b::member(b::id("$"), b::id("HMR"), false, false),
                                        true, false,
                                    ),
                                    b::id("update"),
                                    false, false,
                                ),
                                vec![b::member(b::id("module"), b::id("default"), false, false)],
                            )),
                        ]),
                        false,
                    )],
                )),
            ]),
            "alternate": serde_json::Value::Null
        }));
        program_body.push(b::export_default(b::id(component_name)));
    } else {
        program_body.push(b::export_default(component_fn));
    }

    if !delegated_events.is_empty() {
        let mut event_names: Vec<String> = delegated_events.into_iter().collect();
        event_names.sort();
        program_body.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("delegate"), false, false),
            vec![b::array(
                event_names.iter().map(|s| b::literal_str(s)).collect(),
            )],
        )));
    }

    b::program(program_body)
}

fn collect_delegated_event_names(stmts: &[Value]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(v: &Value, out: &mut std::collections::HashSet<String>) {
        match v {
            Value::Array(arr) => arr.iter().for_each(|x| walk(x, out)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("CallExpression") {
                    if let Some(callee) = obj.get("callee") {
                        let is_dot_delegated = callee
                            .get("type")
                            .and_then(|v| v.as_str())
                            == Some("MemberExpression")
                            && callee
                                .get("object")
                                .and_then(|o| o.get("name"))
                                .and_then(|v| v.as_str())
                                == Some("$")
                            && callee
                                .get("property")
                                .and_then(|p| p.get("name"))
                                .and_then(|v| v.as_str())
                                == Some("delegated");
                        if is_dot_delegated {
                            if let Some(args) = obj.get("arguments").and_then(|v| v.as_array()) {
                                if let Some(first) = args.first() {
                                    if first.get("type").and_then(|v| v.as_str()) == Some("Literal")
                                    {
                                        if let Some(name) =
                                            first.get("value").and_then(|v| v.as_str())
                                        {
                                            out.insert(name.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                obj.values().for_each(|x| walk(x, out));
            }
            _ => {}
        }
    }
    stmts.iter().for_each(|s| walk(s, &mut out));
    out
}

/// Assemble the final Program from program-level statements + function body,
/// applying the HMR wrapper when needed. Used by early-return paths.
fn finalize_program(
    mut program_body: Vec<Value>,
    mut fn_body: Vec<Value>,
    component_name: &str,
    options: &ClientOptions,
) -> Value {
    // Apply runes-mode $.push/$.pop wrapping if needed. (Runes-mode is
    // inferred from the body: presence of $.get/$.set or $$props use that
    // requires context tracking.)
    let runes_mode = body_has_runes(&fn_body);
    let needs_push_pop = runes_mode && body_needs_context(&fn_body);
    if needs_push_pop {
        fn_body.insert(
            0,
            b::stmt(b::call(
                b::member(b::id("$"), b::id("push"), false, false),
                vec![b::id("$$props"), b::literal_bool(true)],
            )),
        );
        fn_body.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("pop"), false, false),
            vec![],
        )));
    }
    let uses_props = body_uses_identifier(&fn_body, "$$props");
    let mut params = vec![b::id("$$anchor")];
    if uses_props {
        params.push(b::id("$$props"));
    }
    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        b::block(fn_body),
        false,
    );
    if options.hmr {
        program_body.push(component_fn);
        program_body.push(serde_json::json!({
            "type": "IfStatement",
            "test": {
                "type": "MemberExpression",
                "object": {
                    "type": "MetaProperty",
                    "meta": { "type": "Identifier", "name": "import" },
                    "property": { "type": "Identifier", "name": "meta" }
                },
                "property": { "type": "Identifier", "name": "hot" },
                "computed": false,
                "optional": false
            },
            "consequent": b::block(vec![
                b::stmt(b::assignment("=",
                    b::id(component_name),
                    b::call(
                        b::member(b::id("$"), b::id("hmr"), false, false),
                        vec![b::id(component_name)],
                    ),
                )),
                b::stmt(b::call(
                    b::member(
                        b::member(b::member(b::id("import"), b::id("meta"), false, false), b::id("hot"), false, false),
                        b::id("accept"),
                        false, false,
                    ),
                    vec![b::arrow(
                        vec![b::id("module")],
                        b::block(vec![
                            b::stmt(b::call(
                                b::member(
                                    b::member(
                                        b::id(component_name),
                                        b::member(b::id("$"), b::id("HMR"), false, false),
                                        true, false,
                                    ),
                                    b::id("update"),
                                    false, false,
                                ),
                                vec![b::member(b::id("module"), b::id("default"), false, false)],
                            )),
                        ]),
                        false,
                    )],
                )),
            ]),
            "alternate": serde_json::Value::Null
        }));
        program_body.push(b::export_default(b::id(component_name)));
    } else {
        program_body.push(b::export_default(component_fn));
    }
    let delegated_events = collect_delegated_event_names(&program_body);
    if !delegated_events.is_empty() {
        let mut event_names: Vec<String> = delegated_events.into_iter().collect();
        event_names.sort();
        program_body.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("delegate"), false, false),
            vec![b::array(
                event_names.iter().map(|s| b::literal_str(s)).collect(),
            )],
        )));
    }
    b::program(program_body)
}

/// Try to lower the fragment as a multi-root static template (multiple
/// top-level elements / Components, all with statically-known content).
/// Returns the template HTML + function body statements.
fn try_multi_root_static(
    fragment: &svelte_ast::Fragment,
    constants: &std::collections::HashMap<String, String>,
    text_var_start: usize,
) -> Option<(String, Vec<Value>)> {
    use svelte_ast::fragment::FragmentChild;
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};

    // Collect top-level Element/Component nodes, plus optional non-whitespace
    // Text + ExpressionTag nodes AFTER the last element (the "trailing dynamic
    // text" slot). Comments and whitespace text BETWEEN roots are separators.
    let mut roots: Vec<&FragmentChild> = Vec::new();
    let mut trailing_text_parts: Vec<DynamicPart> = Vec::new();
    let mut pending_ws: Vec<DynamicPart> = Vec::new();
    let mut seen_element_or_component = false;
    let mut has_real_trailing = false;
    for n in &fragment.nodes {
        match n {
            FragmentChild::Comment(_) => continue,
            FragmentChild::RegularElement(_)
            | FragmentChild::Component(_)
            | FragmentChild::AwaitBlock(_) => {
                if has_real_trailing {
                    // Real trailing content already started — can't go back to
                    // adding a root.
                    return None;
                }
                pending_ws.clear();
                roots.push(n);
                seen_element_or_component = true;
            }
            FragmentChild::Text(t) => {
                if !seen_element_or_component {
                    if t.data.trim().is_empty() {
                        continue;
                    }
                    return None;
                }
                let collapsed = collapse_ws(&t.data);
                if collapsed.is_empty() {
                    continue;
                }
                if has_real_trailing {
                    trailing_text_parts.push(DynamicPart::Static(collapsed));
                } else if collapsed.trim().is_empty() {
                    pending_ws.push(DynamicPart::Static(collapsed));
                } else {
                    trailing_text_parts.extend(pending_ws.drain(..));
                    trailing_text_parts.push(DynamicPart::Static(collapsed));
                    has_real_trailing = true;
                }
            }
            FragmentChild::ExpressionTag(tag) => {
                if !seen_element_or_component {
                    return None;
                }
                trailing_text_parts.extend(pending_ws.drain(..));
                if let Some(s) = constant_folded_literal_with(&tag.expression, constants) {
                    trailing_text_parts.push(DynamicPart::Static(s));
                } else {
                    trailing_text_parts.push(DynamicPart::Expr(tag.expression.clone()));
                }
                has_real_trailing = true;
            }
            _ => return None,
        }
    }
    let has_trailing_text = trailing_text_parts
        .iter()
        .any(|p| matches!(p, DynamicPart::Expr(_)));
    if roots.len() < 2 && !has_trailing_text {
        return None;
    }
    if roots.is_empty() {
        return None;
    }

    // Build template HTML: each element rendered with statically-known content;
    // each component as `<!>`. Single space between roots.
    let mut html = String::new();
    let mut slots: Vec<MultiRootSlot> = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            html.push(' ');
        }
        match r {
            FragmentChild::RegularElement(el) => {
                // Categorize attributes.
                let mut static_attrs: Vec<(String, String)> = Vec::new();
                let mut bind_value: Option<&Value> = None;
                let mut events: Vec<(String, Value)> = Vec::new();
                let mut dyn_attrs: Vec<(String, Value)> = Vec::new();
                for a in &el.attributes {
                    match a {
                        ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                            if is_event_attribute(name) {
                                if let AttributeValue::Single(tag) = value {
                                    events.push((name.clone(), tag.expression.clone()));
                                }
                                continue;
                            }
                            match value {
                                AttributeValue::Empty(true) => {
                                    static_attrs.push((name.clone(), String::new()));
                                }
                                AttributeValue::Single(tag) => {
                                    dyn_attrs.push((name.clone(), tag.expression.clone()));
                                }
                                AttributeValue::Many(parts) => {
                                    let mut text = String::new();
                                    let mut all_text = true;
                                    for p in parts {
                                        match p {
                                            AttributeValuePart::Text(t) => text.push_str(&t.data),
                                            _ => {
                                                all_text = false;
                                                break;
                                            }
                                        }
                                    }
                                    if !all_text {
                                        return None;
                                    }
                                    static_attrs.push((name.clone(), text));
                                }
                                AttributeValue::Empty(false) => {}
                            }
                        }
                        ElementAttribute::BindDirective(bd) if bd.name == "value" => {
                            bind_value = Some(&bd.expression);
                        }
                        _ => return None,
                    }
                }

                // Two passes: first try to fold to a static string. If any
                // expression isn't foldable+pure, fall back to Dynamic.
                let inner = trim_body_edges(&el.fragment.nodes);
                let mut static_text = String::new();
                let mut runtime_expr: Option<Value> = None;
                let mut needs_dynamic = false;
                let mut has_any_expression = false;
                for (i, n) in inner.iter().enumerate() {
                    let last = i == inner.len() - 1;
                    match n {
                        FragmentChild::Text(t) => {
                            let mut data = collapse_ws(&t.data);
                            if i == 0 {
                                data = data.trim_start().to_string();
                            }
                            if last {
                                data = data.trim_end().to_string();
                            }
                            static_text.push_str(&data);
                        }
                        FragmentChild::ExpressionTag(tag) => {
                            has_any_expression = true;
                            if let Some(s) =
                                constant_folded_literal_with(&tag.expression, constants)
                            {
                                static_text.push_str(&s);
                            } else if is_pure_expression(&tag.expression)
                                && runtime_expr.is_none()
                                && static_text.is_empty()
                                && i == inner.len() - 1
                            {
                                runtime_expr = Some(tag.expression.clone());
                                break;
                            } else {
                                needs_dynamic = true;
                                break;
                            }
                        }
                        _ => return None,
                    }
                }

                // Build the dynamic parts if needed.
                let dynamic_parts: Option<Vec<DynamicPart>> = if needs_dynamic {
                    let mut parts: Vec<DynamicPart> = Vec::new();
                    for (i, n) in inner.iter().enumerate() {
                        let last = i == inner.len() - 1;
                        match n {
                            FragmentChild::Text(t) => {
                                let mut data = collapse_ws(&t.data);
                                if i == 0 {
                                    data = data.trim_start().to_string();
                                }
                                if last {
                                    data = data.trim_end().to_string();
                                }
                                if !data.is_empty() {
                                    parts.push(DynamicPart::Static(data));
                                }
                            }
                            FragmentChild::ExpressionTag(tag) => {
                                if let Some(s) =
                                    constant_folded_literal_with(&tag.expression, constants)
                                {
                                    parts.push(DynamicPart::Static(s));
                                } else {
                                    parts.push(DynamicPart::Expr(tag.expression.clone()));
                                }
                            }
                            _ => return None,
                        }
                    }
                    Some(parts)
                } else {
                    None
                };

                html.push('<');
                html.push_str(&el.name);
                for (n, v) in &static_attrs {
                    if v.is_empty() {
                        html.push(' ');
                        html.push_str(n);
                    } else {
                        html.push_str(&format!(" {n}=\"{}\"", html_escape_attr(v)));
                    }
                }
                let is_void = matches!(
                    el.name.as_str(),
                    "area" | "base" | "br" | "col" | "embed" | "hr" | "img"
                    | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr"
                );
                if is_void {
                    html.push_str("/>");
                } else {
                    html.push('>');
                    if needs_dynamic {
                        // Element has dynamic content — use a single-space
                        // placeholder so the runtime $.child() finds a Text
                        // node it can update.
                        html.push(' ');
                    } else if !has_any_expression {
                        // Pure static text content.
                        html.push_str(&static_text);
                    }
                    html.push_str(&format!("</{}>", el.name));
                }
                let content = if let Some(parts) = dynamic_parts {
                    MultiRootContent::Dynamic(parts)
                } else if let Some(expr) = runtime_expr {
                    MultiRootContent::PureNonReactive(expr)
                } else if has_any_expression {
                    MultiRootContent::ConstFolded(static_text.clone())
                } else {
                    MultiRootContent::Static
                };
                slots.push(MultiRootSlot::Element {
                    name: el.name.clone(),
                    content,
                    static_text,
                    is_input: el.name == "input",
                    dyn_attrs,
                    events,
                    bind_value: bind_value.cloned(),
                });
            }
            FragmentChild::Component(c) => {
                html.push_str("<!>");
                slots.push(MultiRootSlot::Component(c));
            }
            FragmentChild::AwaitBlock(b) => {
                html.push_str("<!>");
                slots.push(MultiRootSlot::AwaitBlock(b));
            }
            _ => return None,
        }
    }
    // Trailing dynamic text — emit single-space placeholder. The template_effect
    // call is appended after the main per-slot loop.
    if has_trailing_text {
        html.push(' ');
    }

    // Build function body. var fragment = root(); then traversal + content.
    let mut out: Vec<Value> = Vec::new();
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("fragment"),
            Some(b::call(b::id("root"), vec![])),
        )],
    ));

    let mut local_names: Vec<String> = Vec::with_capacity(slots.len());
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // Track text_N variable for each Dynamic slot (or None for non-Dynamic).
    let mut text_vars: Vec<Option<String>> = Vec::with_capacity(slots.len());
    let mut text_counter: usize = text_var_start;
    for (idx, slot) in slots.iter().enumerate() {
        let base = match slot {
            MultiRootSlot::Element { name, .. } => name.clone(),
            MultiRootSlot::Component(_) | MultiRootSlot::AwaitBlock(_) => "node".to_string(),
        };
        let count = counts.entry(base.clone()).or_insert(0);
        let local = if *count == 0 {
            base.clone()
        } else {
            format!("{base}_{}", count)
        };
        *count += 1;
        local_names.push(local.clone());

        if idx == 0 {
            out.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id(&local),
                    Some(b::call(
                        b::member(b::id("$"), b::id("first_child"), false, false),
                        vec![b::id("fragment")],
                    )),
                )],
            ));
        } else {
            let prev = &local_names[idx - 1];
            out.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id(&local),
                    Some(b::call(
                        b::member(b::id("$"), b::id("sibling"), false, false),
                        vec![b::id(prev), b::literal_num(2.0)],
                    )),
                )],
            ));
        }

        // Apply content for elements with non-static content.
        let mut this_text_var: Option<String> = None;
        if let MultiRootSlot::Element {
            content, is_input, ..
        } = slot
        {
            match content {
                MultiRootContent::PureNonReactive(expr) => {
                    out.push(b::stmt(b::assignment(
                        "=",
                        b::member(b::id(&local), b::id("textContent"), false, false),
                        expr.clone(),
                    )));
                }
                MultiRootContent::ConstFolded(text) => {
                    out.push(b::stmt(b::assignment(
                        "=",
                        b::member(b::id(&local), b::id("textContent"), false, false),
                        b::literal_str(text),
                    )));
                }
                MultiRootContent::Dynamic(_) => {
                    let var_name = if text_counter == 0 {
                        "text".to_string()
                    } else {
                        format!("text_{}", text_counter)
                    };
                    text_counter += 1;
                    this_text_var = Some(var_name.clone());
                    out.push(b::declaration(
                        "var",
                        vec![b::declarator(
                            b::id(&var_name),
                            Some(b::call(
                                b::member(b::id("$"), b::id("child"), false, false),
                                vec![b::id(&local)],
                            )),
                        )],
                    ));
                    out.push(b::stmt(b::call(
                        b::member(b::id("$"), b::id("reset"), false, false),
                        vec![b::id(&local)],
                    )));
                }
                MultiRootContent::Static => {}
            }
            if *is_input {
                out.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("remove_input_defaults"), false, false),
                    vec![b::id(&local)],
                )));
            }
        }
        // Component invocations go inline right after their var decl —
        // matching upstream's ordering.
        if let MultiRootSlot::Component(c) = slot {
            out.push(b::stmt(build_multiroot_component_call(c, &local)?));
        }
        // {#await ...} blocks: inline `$.await(node, () => $.get(promise),
        // null, ($$anchor, X) => { ... })` invocation.
        if let MultiRootSlot::AwaitBlock(b) = slot {
            out.push(b::stmt(build_multiroot_await_call(b, &local)));
        }
        text_vars.push(this_text_var);
    }

    // Trailing dynamic text slot: emit `var text_N = $.sibling(<last_local>);`
    // and queue its template_effect for post-decl phase.
    let trailing_text_var: Option<String> = if has_trailing_text {
        let prev = local_names.last().cloned().unwrap();
        let var_name = if text_counter == 0 {
            "text".to_string()
        } else {
            format!("text_{}", text_counter)
        };
        out.push(b::declaration(
            "var",
            vec![b::declarator(
                b::id(&var_name),
                Some(b::call(
                    b::member(b::id("$"), b::id("sibling"), false, false),
                    vec![b::id(&prev)],
                )),
            )],
        ));
        Some(var_name)
    } else {
        None
    };

    // After all declarations, emit template_effect for Dynamic slots.
    // When there are multiple text-slot effects, bundle them into one
    // template_effect with a block body so they share the same reactive scope.
    let mut all_text_effects: Vec<(String, Vec<DynamicPart>)> = Vec::new();
    for (idx, slot) in slots.iter().enumerate() {
        if let MultiRootSlot::Element { content, .. } = slot {
            if let MultiRootContent::Dynamic(parts) = content {
                if let Some(text_var) = &text_vars[idx] {
                    all_text_effects.push((text_var.clone(), parts.clone()));
                }
            }
        }
    }
    if let Some(text_var) = &trailing_text_var {
        all_text_effects.push((text_var.clone(), trailing_text_parts.clone()));
    }
    match all_text_effects.len() {
        0 => {}
        1 => {
            let (tv, parts) = &all_text_effects[0];
            out.push(b::stmt(build_template_effect_dynamic(tv, parts)));
        }
        _ => {
            // Bundle multiple set_text calls into one effect block.
            let mut stmts: Vec<Value> = Vec::new();
            for (tv, parts) in &all_text_effects {
                stmts.push(b::stmt(build_set_text_template(tv, parts)));
            }
            out.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("template_effect"), false, false),
                vec![b::arrow(vec![], b::block(stmts), false)],
            )));
        }
    }
    for (idx, slot) in slots.iter().enumerate() {
        if let MultiRootSlot::Element {
            dyn_attrs,
            events,
            bind_value,
            ..
        } = slot
        {
            let local = &local_names[idx];
            for (attr_name, expr) in dyn_attrs {
                out.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("set_attribute"), false, false),
                    vec![b::id(local), b::literal_str(attr_name), expr.clone()],
                )));
            }
            if let Some(bv) = bind_value {
                // $.bind_value(node, () => $.get(name), ($$value) => $.set(name, $$value))
                // The expression `bv` is the binding identifier; apply state rewrite to
                // get $.get(name); the setter is constructed from the same name.
                let getter = b::arrow(vec![], rewrite_for_bind_get(bv), false);
                let setter = build_bind_setter(bv);
                out.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("bind_value"), false, false),
                    vec![b::id(local), getter, setter],
                )));
            }
            for (event_name, handler) in events {
                let stripped = event_name.strip_prefix("on").unwrap_or(event_name);
                out.push(b::stmt(b::call(
                    b::member(b::id("$"), b::id("delegated"), false, false),
                    vec![b::literal_str(stripped), b::id(local), handler.clone()],
                )));
            }
        }
    }

    // (Component invocations are emitted inline in the per-slot loop above.)
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("fragment")],
    )));

    Some((html, out))
}

#[derive(Debug)]
enum MultiRootSlot<'a> {
    Element {
        name: String,
        content: MultiRootContent,
        static_text: String,
        is_input: bool,
        dyn_attrs: Vec<(String, Value)>,
        events: Vec<(String, Value)>,
        bind_value: Option<Value>,
    },
    Component(&'a svelte_ast::elements::Component),
    AwaitBlock(&'a svelte_ast::blocks::AwaitBlock),
}

#[derive(Debug)]
enum MultiRootContent {
    Static,
    PureNonReactive(Value),
    ConstFolded(String),
    /// Dynamic content — the element's children mix Text and ExpressionTags
    /// (or one expression that isn't const-foldable). Emit:
    ///   `var text_N = $.child(node); $.reset(node);` after the node decl,
    ///   then `$.template_effect(() => $.set_text(text_N, \`...\`))` after
    ///   all declarations.
    Dynamic(Vec<DynamicPart>),
}

#[derive(Debug, Clone)]
enum DynamicPart {
    Static(String),
    Expr(Value),
}

/// Getter body for `bind:value` — the expression is already state-rewritten
/// (e.g. `$.get(str)` for state `str`). Return a clone as-is.
fn rewrite_for_bind_get(expr: &Value) -> Value {
    expr.clone()
}

/// Setter body for `bind:value`: `($$value) => $.set(name, $$value)` if the
/// rewritten expression is `$.get(name)`, otherwise fallback to direct
/// assignment `(($$value) => expr = $$value)`.
fn build_bind_setter(expr: &Value) -> Value {
    if let Some(name) = extract_state_name(expr) {
        let set_call = b::call(
            b::member(b::id("$"), b::id("set"), false, false),
            vec![b::id(&name), b::id("$$value")],
        );
        return b::arrow(vec![b::id("$$value")], set_call, false);
    }
    let assign = b::assignment("=", expr.clone(), b::id("$$value"));
    b::arrow(vec![b::id("$$value")], assign, false)
}

/// If `expr` is `$.get(<Identifier>)`, return the identifier name.
fn extract_state_name(expr: &Value) -> Option<String> {
    if expr.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return None;
    }
    let callee = expr.get("callee")?;
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return None;
    }
    let obj = callee.get("object")?;
    let prop = callee.get("property")?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("Identifier")
        || obj.get("name").and_then(|v| v.as_str()) != Some("$")
    {
        return None;
    }
    if prop.get("type").and_then(|v| v.as_str()) != Some("Identifier")
        || prop.get("name").and_then(|v| v.as_str()) != Some("get")
    {
        return None;
    }
    let args = expr.get("arguments")?.as_array()?;
    if args.len() != 1 {
        return None;
    }
    let arg = &args[0];
    if arg.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
        return None;
    }
    arg.get("name").and_then(|v| v.as_str()).map(String::from)
}

/// Pure expressions whose evaluation is constant across renders. Approximated
/// as: literal | top-level non-reactive identifier (e.g. `location.href`) |
/// constant-foldable call.
fn is_pure_expression(expr: &Value) -> bool {
    let ty = expr.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "Literal" => true,
        "MemberExpression" => {
            // e.g. `location.href` — assume pure for object.identifier patterns
            // where the object is a known global. Hard to do without scope.
            // Heuristic: if the root object is `location`/`window`/`document`,
            // treat as non-reactive.
            let mut cur = expr;
            loop {
                match cur.get("type").and_then(|v| v.as_str()) {
                    Some("MemberExpression") => cur = cur.get("object").unwrap_or(&Value::Null),
                    Some("Identifier") => {
                        let n = cur.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        return matches!(
                            n,
                            "location" | "window" | "document" | "navigator" | "globalThis"
                        );
                    }
                    _ => return false,
                }
            }
        }
        _ => false,
    }
}

/// Walk a Fragment and apply state-access rewriting to every embedded
/// expression (ExpressionTag, attribute values, BindDirective expressions,
/// block tests/iterators, etc.). Mirrors what upstream's walker does as it
/// transitions from script-scope to template-scope.
fn rewrite_fragment_state_refs(
    f: &mut svelte_ast::Fragment,
    state_names: &std::collections::HashSet<String>,
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    use svelte_ast::fragment::FragmentChild;
    for node in f.nodes.iter_mut() {
        match node {
            FragmentChild::ExpressionTag(t) => {
                rewrite::rewrite_expression(&mut t.expression, state_names);
            }
            FragmentChild::HtmlTag(t) => {
                rewrite::rewrite_expression(&mut t.expression, state_names);
            }
            FragmentChild::ConstTag(t) => {
                rewrite::rewrite_expression(&mut t.declaration, state_names);
            }
            FragmentChild::RenderTag(t) => {
                rewrite::rewrite_expression(&mut t.expression, state_names);
            }
            FragmentChild::IfBlock(b) => {
                rewrite::rewrite_expression(&mut b.test, state_names);
                rewrite_fragment_state_refs(&mut b.consequent, state_names);
                if let Some(alt) = b.alternate.as_mut() {
                    rewrite_fragment_state_refs(alt, state_names);
                }
            }
            FragmentChild::EachBlock(b) => {
                rewrite::rewrite_expression(&mut b.expression, state_names);
                rewrite_fragment_state_refs(&mut b.body, state_names);
                if let Some(fb) = b.fallback.as_mut() {
                    rewrite_fragment_state_refs(fb, state_names);
                }
            }
            FragmentChild::KeyBlock(b) => {
                rewrite::rewrite_expression(&mut b.expression, state_names);
                rewrite_fragment_state_refs(&mut b.fragment, state_names);
            }
            FragmentChild::AwaitBlock(b) => {
                rewrite::rewrite_expression(&mut b.expression, state_names);
                if let Some(f) = b.pending.as_mut() {
                    rewrite_fragment_state_refs(f, state_names);
                }
                if let Some(f) = b.then.as_mut() {
                    rewrite_fragment_state_refs(f, state_names);
                }
                if let Some(f) = b.catch_.as_mut() {
                    rewrite_fragment_state_refs(f, state_names);
                }
            }
            FragmentChild::SnippetBlock(b) => {
                rewrite_fragment_state_refs(&mut b.body, state_names);
            }
            FragmentChild::RegularElement(el) => {
                rewrite_attrs_state_refs(&mut el.attributes, state_names);
                rewrite_fragment_state_refs(&mut el.fragment, state_names);
            }
            FragmentChild::Component(c) => {
                rewrite_attrs_state_refs(&mut c.attributes, state_names);
                rewrite_fragment_state_refs(&mut c.fragment, state_names);
            }
            FragmentChild::SvelteElement(el) => {
                rewrite::rewrite_expression(&mut el.tag, state_names);
                rewrite_attrs_state_refs(&mut el.attributes, state_names);
                rewrite_fragment_state_refs(&mut el.fragment, state_names);
            }
            FragmentChild::SvelteHead(el) => rewrite_fragment_state_refs(&mut el.fragment, state_names),
            FragmentChild::SvelteFragment(el) => rewrite_fragment_state_refs(&mut el.fragment, state_names),
            FragmentChild::TitleElement(el) => rewrite_fragment_state_refs(&mut el.fragment, state_names),
            _ => {}
        }
    }
}

fn rewrite_attrs_state_refs(
    attrs: &mut Vec<svelte_ast::ElementAttribute>,
    state_names: &std::collections::HashSet<String>,
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in attrs.iter_mut() {
        match a {
            ElementAttribute::Attribute(attr) => match &mut attr.value {
                AttributeValue::Single(tag) => {
                    rewrite::rewrite_expression(&mut tag.expression, state_names);
                }
                AttributeValue::Many(parts) => {
                    for p in parts.iter_mut() {
                        if let AttributeValuePart::ExpressionTag(t) = p {
                            rewrite::rewrite_expression(&mut t.expression, state_names);
                        }
                    }
                }
                AttributeValue::Empty(_) => {}
            },
            ElementAttribute::BindDirective(bd) => {
                rewrite::rewrite_expression(&mut bd.expression, state_names);
            }
            ElementAttribute::SpreadAttribute(sa) => {
                rewrite::rewrite_expression(&mut sa.expression, state_names);
            }
            _ => {}
        }
    }
}

fn import_side_effect(source: &str) -> Value {
    serde_json::json!({
        "type": "ImportDeclaration",
        "specifiers": [],
        "source": b::literal_str(source)
    })
}

/// Detect whether the component uses runes. Includes the explicit
/// `<svelte:options runes />` opt-in and any rune call site.
fn uses_runes(root: &Root) -> bool {
    if let Some(opts) = &root.options {
        if opts.runes == Some(true) {
            return true;
        }
    }
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    for rune in [
        "\"$state\"",
        "\"$state.raw\"",
        "\"$derived\"",
        "\"$derived.by\"",
        "\"$effect\"",
        "\"$effect.pre\"",
        "\"$effect.root\"",
        "\"$props\"",
        "\"$props.id\"",
        "\"$bindable\"",
        "\"$inspect\"",
        "\"$inspect.trace\"",
        "\"$host\"",
    ] {
        if json.contains(rune) {
            return true;
        }
    }
    false
}

fn guess_single_root_var(nodes: &[svelte_ast::fragment::FragmentChild]) -> Option<String> {
    use svelte_ast::fragment::FragmentChild;
    let mut element: Option<&str> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::RegularElement(el) => {
                if element.is_some() {
                    return None;
                }
                element = Some(el.name.as_str());
            }
            _ => return None,
        }
    }
    element.map(|s| s.to_string())
}

/// When the only non-whitespace content of a fragment is a single Component
/// invocation, return it. Used to emit a direct `Foo($$anchor, props)` call
/// without a template literal.
/// Walk a list of statements looking for any Identifier reference matching `name`.
/// Walk `node` and wrap every `Identifier { name: target_name }` (in
/// non-member-property, non-declaration position) into `$.get(target_name)`.
fn wrap_identifier_with_get(node: &mut Value, target_name: &str) {
    fn walk(node: &mut Value, target_name: &str, is_member_property: bool) {
        if let Some(obj) = node.as_object_mut() {
            let ty = obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Skip declarations and function params; we only rewrite reads.
            if matches!(
                ty.as_str(),
                "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration"
            ) {
                return;
            }
            if ty == "Identifier" && !is_member_property {
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name == target_name {
                    *node = serde_json::json!({
                        "type": "CallExpression",
                        "callee": {
                            "type": "MemberExpression",
                            "object": { "type": "Identifier", "name": "$" },
                            "property": { "type": "Identifier", "name": "get" },
                            "computed": false,
                            "optional": false
                        },
                        "arguments": [{ "type": "Identifier", "name": target_name }],
                        "optional": false
                    });
                    return;
                }
            }
            if ty == "MemberExpression" {
                if let Some(o) = obj.get_mut("object") {
                    walk(o, target_name, false);
                }
                let computed = obj
                    .get("computed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(p) = obj.get_mut("property") {
                    walk(p, target_name, !computed);
                }
                return;
            }
            for (_, v) in obj.iter_mut() {
                walk(v, target_name, false);
            }
        } else if let Some(arr) = node.as_array_mut() {
            for v in arr {
                walk(v, target_name, false);
            }
        }
    }
    walk(node, target_name, false);
}

/// True if any top-level statement in `body` is a `let/var/const X = await Y`
/// declaration. Used to gate the async script transform.
fn body_has_top_level_await(body: &[Value]) -> bool {
    for stmt in body {
        if stmt.get("type").and_then(|v| v.as_str()) != Some("VariableDeclaration") {
            continue;
        }
        let decls = match stmt.get("declarations").and_then(|v| v.as_array()) {
            Some(d) => d,
            None => continue,
        };
        for d in decls {
            if let Some(init) = d.get("init") {
                if init.get("type").and_then(|v| v.as_str()) == Some("AwaitExpression") {
                    return true;
                }
            }
        }
    }
    false
}

/// If the fragment's only non-whitespace top-level node is a single
/// ExpressionTag, return that expression. Used for the async sole-text
/// template lowering.
fn find_single_root_expression_tag(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<Value> {
    use svelte_ast::fragment::FragmentChild;
    let mut expr: Option<Value> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::ExpressionTag(t) => {
                if expr.is_some() {
                    return None;
                }
                expr = Some(t.expression.clone());
            }
            _ => return None,
        }
    }
    expr
}

/// True if `expr` contains a top-level AwaitExpression (anywhere in its tree).
fn expression_uses_await(expr: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("AwaitExpression") {
                    return true;
                }
                // Don't recurse into function bodies — await inside a nested fn
                // doesn't count as "this expression awaits".
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(
                    ty,
                    "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration"
                ) {
                    return false;
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(expr)
}

/// Lower a single root `{#each await EXPR as item}...{/each}` block to the
/// async each pattern using \$.async + \$.each.
fn build_async_each_block_client(blk: &svelte_ast::blocks::EachBlock) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("fragment"),
            Some(b::call(
                b::member(b::id("$"), b::id("comment"), false, false),
                vec![],
            )),
        )],
    ));
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("node"),
            Some(b::call(
                b::member(b::id("$"), b::id("first_child"), false, false),
                vec![b::id("fragment")],
            )),
        )],
    ));

    // Unwrap top-level await.
    let collection_expr = match blk.expression.get("type").and_then(|v| v.as_str()) {
        Some("AwaitExpression") => blk
            .expression
            .get("argument")
            .cloned()
            .unwrap_or_else(|| blk.expression.clone()),
        _ => blk.expression.clone(),
    };
    let promise_thunk = b::arrow(vec![], collection_expr, false);

    // Item parameter (e.g. `item` from `{#each ... as item}`).
    let item_param = blk
        .context
        .clone()
        .unwrap_or_else(|| b::id("$$item"));
    let item_name = item_param
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("$$item")
        .to_string();

    // Inner each-iteration body: $.next(); var text = $.text(); $.template_effect(...);
    // $.append($$anchor, text);
    let mut counter: usize = 0;
    let iter_body_stmts = build_async_each_iter_body(&blk.body, &item_name, &mut counter);
    let iter_arrow = b::arrow(
        vec![b::id("$$anchor"), item_param],
        b::block(iter_body_stmts),
        false,
    );

    // Optional fallback (the `{:else}` clause).
    let fallback_arrow = blk.fallback.as_ref().map(|fb| {
        let body = build_async_each_iter_body(fb, "", &mut counter);
        b::arrow(vec![b::id("$$anchor")], b::block(body), false)
    });

    // $.each(node, FLAGS, () => $.get($$collection), $.index, iter_arrow [, fallback_arrow])
    // Flag 17 (= 16 | 1) for no-fallback; 16 for has-fallback.
    let flag_val = if fallback_arrow.is_some() { 16.0 } else { 17.0 };
    let mut each_args: Vec<Value> = vec![
        b::id("node"),
        b::literal_num(flag_val),
        b::arrow(
            vec![],
            b::call(
                b::member(b::id("$"), b::id("get"), false, false),
                vec![b::id("$$collection")],
            ),
            false,
        ),
        b::member(b::id("$"), b::id("index"), false, false),
        iter_arrow,
    ];
    if let Some(fb) = fallback_arrow {
        each_args.push(fb);
    }
    let each_call = b::call(
        b::member(b::id("$"), b::id("each"), false, false),
        each_args,
    );

    // Outer $.async wrapper.
    let async_callback = b::arrow(
        vec![b::id("node"), b::id("$$collection")],
        b::block(vec![b::stmt(each_call)]),
        false,
    );
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("async"), false, false),
        vec![
            b::id("node"),
            b::array(vec![]),
            b::array(vec![promise_thunk]),
            async_callback,
        ],
    )));

    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("fragment")],
    )));

    out
}

/// Build the body of one each-iteration in async mode. Supports a single
/// ExpressionTag (awaited or sync) and emits the $.text + template_effect
/// pattern, with $.next() before the text declaration.
fn build_async_each_iter_body(
    fragment: &svelte_ast::Fragment,
    item_name: &str,
    counter: &mut usize,
) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    let mut expr: Option<Value> = None;
    for n in &fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::ExpressionTag(t) => {
                if expr.is_some() {
                    return Vec::new();
                }
                let raw = match t.expression.get("type").and_then(|v| v.as_str()) {
                    Some("AwaitExpression") => t
                        .expression
                        .get("argument")
                        .cloned()
                        .unwrap_or_else(|| t.expression.clone()),
                    _ => t.expression.clone(),
                };
                expr = Some(raw);
            }
            _ => return Vec::new(),
        }
    }
    let Some(mut e) = expr else {
        return Vec::new();
    };
    // Wrap references to the iteration item in $.get(...) since it's a
    // reactive binding in async-each mode.
    wrap_identifier_with_get(&mut e, item_name);
    let var_name = if *counter == 0 {
        "text".to_string()
    } else {
        format!("text_{}", *counter)
    };
    *counter += 1;
    let mut stmts: Vec<Value> = Vec::new();
    stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("next"), false, false),
        vec![],
    )));
    stmts.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id(&var_name),
            Some(b::call(
                b::member(b::id("$"), b::id("text"), false, false),
                vec![],
            )),
        )],
    ));
    let void0 = serde_json::json!({
        "type": "UnaryExpression",
        "operator": "void",
        "prefix": true,
        "argument": { "type": "Literal", "value": 0, "raw": "0" }
    });
    let inner_arrow = b::arrow(
        vec![b::id("$0")],
        b::call(
            b::member(b::id("$"), b::id("set_text"), false, false),
            vec![b::id(&var_name), b::id("$0")],
        ),
        false,
    );
    let deps_array = b::array(vec![b::arrow(vec![], e, false)]);
    stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("template_effect"), false, false),
        vec![inner_arrow, void0, deps_array],
    )));
    stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id(&var_name)],
    )));
    stmts
}

/// Lower a single root `{#if await EXPR}{:else}{/if}` block to the async if
/// pattern: \$.async(...) wrapping with consequent/alternate arrows.
fn build_async_if_block_client(blk: &svelte_ast::blocks::IfBlock) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("fragment"),
            Some(b::call(
                b::member(b::id("$"), b::id("comment"), false, false),
                vec![],
            )),
        )],
    ));
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("node"),
            Some(b::call(
                b::member(b::id("$"), b::id("first_child"), false, false),
                vec![b::id("fragment")],
            )),
        )],
    ));

    // Unwrap top-level await: `await X` → X.
    let test_expr = match blk.test.get("type").and_then(|v| v.as_str()) {
        Some("AwaitExpression") => blk
            .test
            .get("argument")
            .cloned()
            .unwrap_or_else(|| blk.test.clone()),
        _ => blk.test.clone(),
    };
    let promise_thunk = b::arrow(vec![], test_expr, false);

    // Build consequent / alternate as standalone arrow functions whose bodies
    // are lowered fragments. Each body's text node gets a unique name from a
    // shared counter so they don't collide.
    let mut counter: usize = 0;
    let consequent_body = build_async_branch_body(&blk.consequent, &mut counter);
    let consequent_arrow = b::arrow(
        vec![b::id("$$anchor")],
        b::block(consequent_body),
        false,
    );

    let has_alternate = blk.alternate.is_some();
    let alternate_arrow = blk.alternate.as_ref().map(|alt| {
        let alt_body = build_async_branch_body(alt, &mut counter);
        b::arrow(vec![b::id("$$anchor")], b::block(alt_body), false)
    });

    let mut inner_stmts: Vec<Value> = Vec::new();
    inner_stmts.push(b::declaration(
        "var",
        vec![b::declarator(b::id("consequent"), Some(consequent_arrow))],
    ));
    if let Some(alt) = alternate_arrow {
        inner_stmts.push(b::declaration(
            "var",
            vec![b::declarator(b::id("alternate"), Some(alt))],
        ));
    }

    // $.if(node, ($$render) => { if ($.get($$condition)) $$render(consequent); else $$render(alternate, -1); })
    let if_stmt_body = if has_alternate {
        serde_json::json!({
            "type": "IfStatement",
            "test": {
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "Identifier", "name": "$" },
                    "property": { "type": "Identifier", "name": "get" },
                    "computed": false,
                    "optional": false
                },
                "arguments": [{ "type": "Identifier", "name": "$$condition" }],
                "optional": false
            },
            "consequent": b::stmt(b::call(
                b::id("$$render"),
                vec![b::id("consequent")]
            )),
            "alternate": b::stmt(b::call(
                b::id("$$render"),
                vec![b::id("alternate"), b::literal_num(-1.0)]
            ))
        })
    } else {
        serde_json::json!({
            "type": "IfStatement",
            "test": {
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "Identifier", "name": "$" },
                    "property": { "type": "Identifier", "name": "get" },
                    "computed": false,
                    "optional": false
                },
                "arguments": [{ "type": "Identifier", "name": "$$condition" }],
                "optional": false
            },
            "consequent": b::stmt(b::call(
                b::id("$$render"),
                vec![b::id("consequent")]
            )),
            "alternate": Value::Null
        })
    };
    inner_stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("if"), false, false),
        vec![
            b::id("node"),
            b::arrow(vec![b::id("$$render")], b::block(vec![if_stmt_body]), false),
        ],
    )));

    let outer_arrow = b::arrow(
        vec![b::id("node"), b::id("$$condition")],
        b::block(inner_stmts),
        false,
    );
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("async"), false, false),
        vec![
            b::id("node"),
            b::array(vec![]),
            b::array(vec![promise_thunk]),
            outer_arrow,
        ],
    )));

    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("fragment")],
    )));

    out
}

/// Build the body of an async if-branch (consequent or alternate). For now
/// only supports a fragment whose trimmed body is exactly one ExpressionTag
/// (with an awaited expression). Other shapes return an empty body.
fn build_async_branch_body(
    fragment: &svelte_ast::Fragment,
    counter: &mut usize,
) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    // Find the single non-whitespace ExpressionTag in the fragment.
    let mut expr: Option<Value> = None;
    for n in &fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::ExpressionTag(t) => {
                if expr.is_some() {
                    return Vec::new();
                }
                let raw = match t.expression.get("type").and_then(|v| v.as_str()) {
                    Some("AwaitExpression") => t
                        .expression
                        .get("argument")
                        .cloned()
                        .unwrap_or_else(|| t.expression.clone()),
                    _ => t.expression.clone(),
                };
                expr = Some(raw);
            }
            _ => return Vec::new(),
        }
    }
    let Some(e) = expr else {
        return Vec::new();
    };
    let var_name = if *counter == 0 {
        "text".to_string()
    } else {
        format!("text_{}", *counter)
    };
    *counter += 1;

    let mut stmts: Vec<Value> = Vec::new();
    stmts.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id(&var_name),
            Some(b::call(
                b::member(b::id("$"), b::id("text"), false, false),
                vec![],
            )),
        )],
    ));
    // $.template_effect(($0) => $.set_text(text, $0), void 0, [() => expr])
    let void0 = serde_json::json!({
        "type": "UnaryExpression",
        "operator": "void",
        "prefix": true,
        "argument": { "type": "Literal", "value": 0, "raw": "0" }
    });
    let inner_arrow = b::arrow(
        vec![b::id("$0")],
        b::call(
            b::member(b::id("$"), b::id("set_text"), false, false),
            vec![b::id(&var_name), b::id("$0")],
        ),
        false,
    );
    let deps_array = b::array(vec![b::arrow(vec![], e, false)]);
    stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("template_effect"), false, false),
        vec![inner_arrow, void0, deps_array],
    )));
    stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id(&var_name)],
    )));
    stmts
}

/// Transform the script body for `experimental.async` mode. Pulls top-level
/// `let X = await Y;` and `\$.inspect(...)` statements into a `\$.run([...])`
/// invocation. Returns `(transformed_body, var_last_idx)` where
/// `var_last_idx[name]` is the last index in $$promises that touched `name`.
fn transform_async_script(
    body: Vec<Value>,
) -> (Vec<Value>, std::collections::HashMap<String, usize>) {
    // Classify each statement as one of: AsyncDecl (let X = await Y),
    // SyncDecl (let X = sync init), Inspect ($.inspect(...)), or Other.
    // Consecutive SyncDecl + Inspect statements group into ONE \$.run callback
    // (to preserve sync-tick observable ordering — upstream's rule).
    enum Kind {
        AsyncDecl(String, Value), // X = await Y
        SyncDecl(String, Value),  // X = sync expr
        Inspect(Vec<String>),     // names read by $.inspect
        Other,
    }
    let mut items: Vec<(Kind, Value)> = Vec::new();
    for stmt in body {
        let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "VariableDeclaration" => {
                let decls = stmt
                    .get("declarations")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if decls.len() == 1 {
                    let d = &decls[0];
                    let name = d
                        .get("id")
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let init = d.get("init").cloned();
                    if let (Some(name), Some(init)) = (name, init) {
                        let is_await =
                            init.get("type").and_then(|v| v.as_str()) == Some("AwaitExpression");
                        if is_await {
                            items.push((Kind::AsyncDecl(name, init), stmt));
                        } else {
                            items.push((Kind::SyncDecl(name, init), stmt));
                        }
                        continue;
                    }
                }
                items.push((Kind::Other, stmt));
            }
            "ExpressionStatement" => {
                let expr = stmt.get("expression");
                if let Some(e) = expr {
                    if is_dollar_inspect_call(e) {
                        items.push((Kind::Inspect(collect_identifier_names(e)), stmt));
                        continue;
                    }
                }
                items.push((Kind::Other, stmt));
            }
            _ => items.push((Kind::Other, stmt)),
        }
    }

    let mut var_names: Vec<String> = Vec::new();
    let mut run_callbacks: Vec<Value> = Vec::new();
    let mut other_stmts: Vec<Value> = Vec::new();
    let mut var_last_idx: std::collections::HashMap<String, usize> = Default::default();
    let mut i = 0;
    while i < items.len() {
        match &items[i].0 {
            Kind::AsyncDecl(name, init) => {
                var_names.push(name.clone());
                let assign = b::assignment("=", b::id(name), init.clone());
                let arrow = b::arrow(vec![], assign, true);
                let idx = run_callbacks.len();
                run_callbacks.push(arrow);
                var_last_idx.insert(name.clone(), idx);
                i += 1;
            }
            Kind::SyncDecl(_, _) | Kind::Inspect(_) => {
                // Group consecutive SyncDecl / Inspect items into one callback.
                let mut group_stmts: Vec<Value> = Vec::new();
                let mut group_names: Vec<String> = Vec::new();
                while i < items.len() {
                    match &items[i].0 {
                        Kind::SyncDecl(name, init) => {
                            var_names.push(name.clone());
                            group_stmts.push(b::stmt(b::assignment(
                                "=",
                                b::id(name),
                                init.clone(),
                            )));
                            group_names.push(name.clone());
                            i += 1;
                        }
                        Kind::Inspect(reads) => {
                            group_names.extend(reads.clone());
                            i += 1;
                        }
                        _ => break,
                    }
                }
                let idx = run_callbacks.len();
                // If the group is solely an Inspect (no sync decls), emit
                // `() => void 0`. Otherwise emit `() => { ...stmts }`.
                let arrow_body = if group_stmts.is_empty() {
                    serde_json::json!({
                        "type": "UnaryExpression",
                        "operator": "void",
                        "prefix": true,
                        "argument": { "type": "Literal", "value": 0, "raw": "0" }
                    })
                } else if group_stmts.len() == 1 {
                    // Single statement → expression body of arrow.
                    let s = group_stmts.into_iter().next().unwrap();
                    s.get("expression").cloned().unwrap_or(s)
                } else {
                    b::block(group_stmts)
                };
                run_callbacks.push(b::arrow(vec![], arrow_body, false));
                for n in group_names {
                    var_last_idx.insert(n, idx);
                }
            }
            Kind::Other => {
                other_stmts.push(items[i].1.clone());
                i += 1;
            }
        }
    }

    let mut combined: Vec<Value> = Vec::new();
    if !var_names.is_empty() {
        let declarators: Vec<Value> = var_names
            .iter()
            .map(|n| b::declarator(b::id(n), None))
            .collect();
        combined.push(b::declaration("var", declarators));
    }
    if !run_callbacks.is_empty() {
        combined.push(b::declaration(
            "var",
            vec![b::declarator(
                b::id("$$promises"),
                Some(b::call(
                    b::member(b::id("$"), b::id("run"), false, false),
                    vec![b::array(run_callbacks)],
                )),
            )],
        ));
    }
    combined.extend(other_stmts);
    (combined, var_last_idx)
}

/// True if `v` is a `\$.inspect(...)` call.
fn is_dollar_inspect_call(v: &Value) -> bool {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return false;
    }
    let Some(callee) = v.get("callee") else {
        return false;
    };
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return false;
    }
    let obj = callee
        .get("object")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str());
    let prop = callee
        .get("property")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str());
    obj == Some("$") && prop == Some("inspect")
}

/// Walk a node collecting top-level Identifier names referenced (excludes
/// member-expression properties).
fn collect_identifier_names(node: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>, is_member_property: bool) {
        match v {
            Value::Array(arr) => arr.iter().for_each(|x| walk(x, out, false)),
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if ty == "Identifier" && !is_member_property {
                    if let Some(n) = obj.get("name").and_then(|x| x.as_str()) {
                        if !out.contains(&n.to_string()) {
                            out.push(n.to_string());
                        }
                    }
                }
                if ty == "MemberExpression" {
                    if let Some(o) = obj.get("object") {
                        walk(o, out, false);
                    }
                    let computed = obj
                        .get("computed")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if let Some(p) = obj.get("property") {
                        walk(p, out, !computed);
                    }
                    return;
                }
                for (_, v) in obj.iter() {
                    walk(v, out, false);
                }
            }
            _ => {}
        }
    }
    walk(node, &mut out, false);
    out
}

/// True if a hoisted snippet declaration contains a `var text = ...`
/// declaration (used to bump the trailing-text counter in the main component).
fn snippet_declares_text(stmt: &Value) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    let id_name = obj
                        .get("id")
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str());
                    if id_name == Some("text") {
                        return true;
                    }
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(stmt)
}

/// Extract top-level `{#snippet name(...)}...{/snippet}` blocks from the
/// fragment in-place. Each becomes
/// `const name = ($$anchor, ...params) => { ... };` at program scope.
fn extract_top_level_snippets_client(f: &mut svelte_ast::Fragment) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    let mut hoisted: Vec<Value> = Vec::new();
    let nodes = std::mem::take(&mut f.nodes);
    for node in nodes {
        if let FragmentChild::SnippetBlock(blk) = &node {
            let name = blk
                .expression
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("$$snippet")
                .to_string();
            let params: Vec<Value> = std::iter::once(b::id("$$anchor"))
                .chain(blk.parameters.iter().cloned())
                .collect();
            let body = build_snippet_body_client(&blk.body);
            let arrow = b::arrow(params, b::block(body), false);
            hoisted.push(b::declaration(
                "const",
                vec![b::declarator(b::id(&name), Some(arrow))],
            ));
        } else {
            f.nodes.push(node);
        }
    }
    hoisted
}

/// Build the body of a snippet's arrow function. For a simple static-text-only
/// snippet the body is:
///   $.next();
///   var text = $.text('TEXT');
///   $.append($$anchor, text);
fn build_snippet_body_client(body_frag: &svelte_ast::Fragment) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    // Concatenate static text-only children. For now only support the simple
    // text-only snippet case; everything else is a TODO.
    let mut text = String::new();
    for n in &body_frag.nodes {
        match n {
            FragmentChild::Text(t) => text.push_str(&t.data),
            _ => return Vec::new(),
        }
    }
    let text = text.trim().to_string();
    let mut out: Vec<Value> = Vec::new();
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("next"), false, false),
        vec![],
    )));
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("text"),
            Some(b::call(
                b::member(b::id("$"), b::id("text"), false, false),
                vec![b::literal_str(&text)],
            )),
        )],
    ));
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("text")],
    )));
    out
}

/// Walk a fragment collecting Identifier names that appear as the LHS of an
/// AssignmentExpression or the argument of an UpdateExpression anywhere in
/// the template's embedded expressions. Used to defeat the
/// never-reassigned-state unwrap when reassignment lives outside the script.
fn collect_template_reassignments(
    fragment: &svelte_ast::Fragment,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    walk_fragment_expressions(fragment, &mut |expr| {
        scan_reassignments(expr, &mut out);
    });
    out
}

fn scan_reassignments(node: &Value, out: &mut std::collections::HashSet<String>) {
    match node {
        Value::Array(arr) => {
            for v in arr {
                scan_reassignments(v, out);
            }
        }
        Value::Object(obj) => {
            let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "AssignmentExpression" => {
                    if let Some(left) = obj.get("left") {
                        if left.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                            if let Some(name) = left.get("name").and_then(|v| v.as_str()) {
                                out.insert(name.to_string());
                            }
                        }
                    }
                }
                "UpdateExpression" => {
                    if let Some(arg) = obj.get("argument") {
                        if arg.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                            if let Some(name) = arg.get("name").and_then(|v| v.as_str()) {
                                out.insert(name.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
            for (_, v) in obj.iter() {
                scan_reassignments(v, out);
            }
        }
        _ => {}
    }
}

fn walk_fragment_expressions(
    f: &svelte_ast::Fragment,
    visit: &mut dyn FnMut(&Value),
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    use svelte_ast::fragment::FragmentChild;
    for node in &f.nodes {
        match node {
            FragmentChild::ExpressionTag(t) => visit(&t.expression),
            FragmentChild::HtmlTag(t) => visit(&t.expression),
            FragmentChild::RenderTag(t) => visit(&t.expression),
            FragmentChild::ConstTag(t) => visit(&t.declaration),
            FragmentChild::IfBlock(b) => {
                visit(&b.test);
                walk_fragment_expressions(&b.consequent, visit);
                if let Some(alt) = b.alternate.as_ref() {
                    walk_fragment_expressions(alt, visit);
                }
            }
            FragmentChild::EachBlock(b) => {
                visit(&b.expression);
                walk_fragment_expressions(&b.body, visit);
                if let Some(fb) = b.fallback.as_ref() {
                    walk_fragment_expressions(fb, visit);
                }
            }
            FragmentChild::KeyBlock(b) => {
                visit(&b.expression);
                walk_fragment_expressions(&b.fragment, visit);
            }
            FragmentChild::AwaitBlock(b) => {
                visit(&b.expression);
                if let Some(f) = b.pending.as_ref() {
                    walk_fragment_expressions(f, visit);
                }
                if let Some(f) = b.then.as_ref() {
                    walk_fragment_expressions(f, visit);
                }
                if let Some(f) = b.catch_.as_ref() {
                    walk_fragment_expressions(f, visit);
                }
            }
            FragmentChild::SnippetBlock(b) => {
                walk_fragment_expressions(&b.body, visit);
            }
            FragmentChild::RegularElement(el) => {
                for a in &el.attributes {
                    visit_attribute_exprs(a, visit);
                }
                walk_fragment_expressions(&el.fragment, visit);
            }
            FragmentChild::Component(c) => {
                for a in &c.attributes {
                    visit_attribute_exprs(a, visit);
                }
                walk_fragment_expressions(&c.fragment, visit);
            }
            FragmentChild::SvelteElement(el) => {
                visit(&el.tag);
                for a in &el.attributes {
                    visit_attribute_exprs(a, visit);
                }
                walk_fragment_expressions(&el.fragment, visit);
            }
            FragmentChild::SvelteHead(el) => walk_fragment_expressions(&el.fragment, visit),
            FragmentChild::SvelteFragment(el) => walk_fragment_expressions(&el.fragment, visit),
            FragmentChild::TitleElement(el) => walk_fragment_expressions(&el.fragment, visit),
            _ => {}
        }
    }
}

fn visit_attribute_exprs(
    a: &svelte_ast::attributes::ElementAttribute,
    visit: &mut dyn FnMut(&Value),
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    match a {
        ElementAttribute::Attribute(attr) => match &attr.value {
            AttributeValue::Single(t) => visit(&t.expression),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        visit(&t.expression);
                    }
                }
            }
            AttributeValue::Empty(_) => {}
        },
        ElementAttribute::BindDirective(bd) => {
            // bind:X={target} causes the target to be reassigned from the
            // outside — synthesize a fake AssignmentExpression for the visitor
            // so the reassignment-collection picks it up.
            let synthetic = serde_json::json!({
                "type": "AssignmentExpression",
                "operator": "=",
                "left": bd.expression.clone(),
                "right": { "type": "Identifier", "name": "$$bind_synthetic" }
            });
            visit(&synthetic);
        }
        ElementAttribute::SpreadAttribute(sa) => visit(&sa.expression),
        ElementAttribute::OnDirective(od) => {
            if let Some(e) = od.expression.as_ref() {
                visit(e);
            }
        }
        ElementAttribute::UseDirective(ud) => {
            if let Some(e) = ud.expression.as_ref() {
                visit(e);
            }
        }
        ElementAttribute::TransitionDirective(td) => {
            if let Some(e) = td.expression.as_ref() {
                visit(e);
            }
        }
        ElementAttribute::AnimateDirective(ad) => {
            if let Some(e) = ad.expression.as_ref() {
                visit(e);
            }
        }
        ElementAttribute::ClassDirective(cd) => visit(&cd.expression),
        ElementAttribute::StyleDirective(sd) => match &sd.value {
            AttributeValue::Single(t) => visit(&t.expression),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        visit(&t.expression);
                    }
                }
            }
            AttributeValue::Empty(_) => {}
        },
        ElementAttribute::LetDirective(ld) => {
            if let Some(e) = ld.expression.as_ref() {
                visit(e);
            }
        }
        ElementAttribute::AttachTag(t) => visit(&t.expression),
    }
}

fn body_uses_identifier(stmts: &[Value], name: &str) -> bool {
    fn walk(v: &Value, name: &str) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(|x| walk(x, name)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("Identifier")
                    && obj.get("name").and_then(|v| v.as_str()) == Some(name)
                {
                    return true;
                }
                obj.values().any(|v| walk(v, name))
            }
            _ => false,
        }
    }
    stmts.iter().any(|s| walk(s, name))
}

/// Heuristic for "runes mode" applied to a fn_body. We check for any post-
/// rewrite signal: `$.state`, `$.derived`, `$.props`, `$.prop`, `$.rest_props`,
/// `$.user_effect`, `$.user_pre_effect`, `$.inspect`. Conservative — false
/// positives in shared sub-expressions don't matter since we only use this to
/// gate $.push/$.pop.
fn body_has_runes(stmts: &[Value]) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("MemberExpression") {
                    let obj_name = obj
                        .get("object")
                        .and_then(|o| o.get("name"))
                        .and_then(|v| v.as_str());
                    let prop_name = obj
                        .get("property")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str());
                    if obj_name == Some("$")
                        && matches!(
                            prop_name,
                            Some("state")
                                | Some("derived")
                                | Some("props")
                                | Some("prop")
                                | Some("rest_props")
                                | Some("user_effect")
                                | Some("user_pre_effect")
                                | Some("inspect")
                                | Some("get")
                                | Some("set")
                        )
                    {
                        return true;
                    }
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    stmts.iter().any(walk)
}

/// Conservative port of upstream's `needs_context` analysis. Emit $.push/$.pop
/// when the body has: a class declaration, a `new` expression, `this.X`
/// member access, direct `$$props.X` member access, `$.user_effect` /
/// `$.user_pre_effect` / `$.inspect`, or `$.run` (async).
fn body_needs_context(stmts: &[Value]) -> bool {
    fn walk(v: &Value) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "ClassDeclaration" | "ClassExpression" | "NewExpression" => return true,
                    "MemberExpression" => {
                        let object = obj.get("object").cloned().unwrap_or(Value::Null);
                        if object.get("type").and_then(|v| v.as_str()) == Some("ThisExpression") {
                            return true;
                        }
                        let obj_name = object.get("name").and_then(|v| v.as_str());
                        if obj_name == Some("$$props") {
                            return true;
                        }
                        let prop_name = obj
                            .get("property")
                            .and_then(|p| p.get("name"))
                            .and_then(|v| v.as_str());
                        if obj_name == Some("$")
                            && matches!(prop_name, Some("user_effect") | Some("user_pre_effect"))
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    stmts.iter().any(walk)
}

fn find_single_each_block(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::blocks::EachBlock> {
    use svelte_ast::fragment::FragmentChild;
    let mut blk: Option<&svelte_ast::blocks::EachBlock> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::EachBlock(b) => {
                if blk.is_some() {
                    return None;
                }
                blk = Some(b);
            }
            _ => return None,
        }
    }
    blk
}

fn find_single_if_block(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::blocks::IfBlock> {
    use svelte_ast::fragment::FragmentChild;
    let mut blk: Option<&svelte_ast::blocks::IfBlock> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::IfBlock(b) => {
                if blk.is_some() {
                    return None;
                }
                blk = Some(b);
            }
            _ => return None,
        }
    }
    blk
}

/// Build the client lowering for a top-level `{#each}` block. Returns
/// (program-level extras, function-body statements).
fn build_each_block_client(
    blk: &svelte_ast::blocks::EachBlock,
) -> (Vec<Value>, Vec<Value>) {
    use svelte_ast::fragment::FragmentChild;
    let mut program_extras: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("fragment"),
            Some(b::call(
                b::member(b::id("$"), b::id("comment"), false, false),
                vec![],
            )),
        )],
    ));
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("node"),
            Some(b::call(
                b::member(b::id("$"), b::id("first_child"), false, false),
                vec![b::id("fragment")],
            )),
        )],
    ));

    // Build the each callback. The callback params depend on whether the
    // user provided a context binding and/or an index. When an index is
    // supplied without context, upstream uses `$$item` as a placeholder.
    let mut params: Vec<Value> = vec![b::id("$$anchor")];
    match (&blk.context, &blk.index) {
        (Some(ctx), Some(idx)) => {
            params.push(ctx.clone());
            params.push(b::id(idx));
        }
        (Some(ctx), None) => {
            params.push(ctx.clone());
        }
        (None, Some(idx)) => {
            params.push(b::id("$$item"));
            params.push(b::id(idx));
        }
        (None, None) => {}
    }

    // Build the body. Currently supports two patterns:
    //   (1) text/expression-only (emit $.text() + $.template_effect)
    //   (2) single RegularElement with text content (emit var X = root_1();
    //       X.textContent = ...)
    let mut body_stmts: Vec<Value> = Vec::new();
    let body_trimmed = trim_body_edges(&blk.body.nodes);
    if let Some(single_el) = find_single_text_only_element(&body_trimmed) {
        return build_each_with_element_body(blk, single_el, params);
    }
    body_stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("next"), false, false),
        vec![],
    )));

    // Trim whitespace-only Text nodes at fragment edges (matches upstream's
    // clean_nodes pass).
    let mut body_nodes: Vec<&FragmentChild> = blk.body.nodes.iter().collect();
    while body_nodes
        .first()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        body_nodes.remove(0);
    }
    while body_nodes
        .last()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        body_nodes.pop();
    }

    // Gather quasis + expressions for `$.set_text(text, \`...\`)`. For the
    // first/last text nodes, also trim leading/trailing whitespace of the
    // text content itself (matches `text.data.trim()` upstream behavior).
    let mut quasis: Vec<String> = vec![String::new()];
    let mut expressions: Vec<Value> = Vec::new();
    let mut is_purely_static = true;
    let last_idx = body_nodes.len().saturating_sub(1);
    for (i, n) in body_nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let mut data = collapse_ws(&t.data);
                if i == 0 {
                    data = data.trim_start().to_string();
                }
                if i == last_idx {
                    data = data.trim_end().to_string();
                }
                quasis.last_mut().unwrap().push_str(&data);
            }
            FragmentChild::ExpressionTag(tag) => {
                if let Some(s) = constant_folded_literal(&tag.expression) {
                    quasis.last_mut().unwrap().push_str(&s);
                } else {
                    expressions.push(tag.expression.clone());
                    quasis.push(String::new());
                    is_purely_static = false;
                }
            }
            _ => return (Vec::new(), Vec::new()),
        }
    }

    body_stmts.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("text"),
            Some(b::call(
                b::member(b::id("$"), b::id("text"), false, false),
                vec![],
            )),
        )],
    ));

    if is_purely_static {
        // Static text — emit `text.nodeValue = '...'` (one-time assignment).
        let joined: String = quasis.join("");
        body_stmts.push(b::stmt(b::assignment(
            "=",
            b::member(b::id("text"), b::id("nodeValue"), false, false),
            b::literal_str(&joined),
        )));
    } else {
        // Build the template literal `${e0 ?? ''}${e1 ?? ''}...`.
        let mut tpl_quasis: Vec<String> = vec![quasis[0].clone()];
        let mut tpl_exprs: Vec<Value> = Vec::new();
        for (i, expr) in expressions.iter().enumerate() {
            tpl_exprs.push(b::logical("??", expr.clone(), b::literal_str("")));
            tpl_quasis.push(quasis[i + 1].clone());
        }
        let quasi_refs: Vec<&str> = tpl_quasis.iter().map(|s| s.as_str()).collect();
        let tpl = b::template_literal(quasi_refs, tpl_exprs);
        body_stmts.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("template_effect"), false, false),
            vec![b::arrow(
                vec![],
                b::call(
                    b::member(b::id("$"), b::id("set_text"), false, false),
                    vec![b::id("text"), tpl],
                ),
                false,
            )],
        )));
    }

    body_stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("text")],
    )));

    let collection_thunk = b::arrow(vec![], blk.expression.clone(), false);
    let each_flags = b::literal_num(0.0);
    let index_kind = b::member(b::id("$"), b::id("index"), false, false);
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("each"), false, false),
        vec![
            b::id("node"),
            each_flags,
            collection_thunk,
            index_kind,
            b::arrow(params, b::block(body_stmts), false),
        ],
    )));
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("fragment")],
    )));
    (program_extras, out)
}

fn trim_body_edges(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Vec<&svelte_ast::fragment::FragmentChild> {
    use svelte_ast::fragment::FragmentChild;
    let mut out: Vec<&FragmentChild> = nodes.iter().collect();
    while out
        .first()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        out.remove(0);
    }
    while out
        .last()
        .map(|n| matches!(n, FragmentChild::Text(t) if t.data.trim().is_empty()))
        .unwrap_or(false)
    {
        out.pop();
    }
    out
}

/// If a body contains a single RegularElement whose children are only Text/
/// ExpressionTag, return it. Used to emit the "var p = root_1(); p.textContent
/// = …" pattern.
fn find_single_text_only_element<'a>(
    nodes: &[&'a svelte_ast::fragment::FragmentChild],
) -> Option<&'a svelte_ast::elements::RegularElement> {
    use svelte_ast::fragment::FragmentChild;
    let mut el: Option<&svelte_ast::elements::RegularElement> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::RegularElement(e) => {
                if el.is_some() {
                    return None;
                }
                // Body must be text/expr only
                for c in &e.fragment.nodes {
                    if !matches!(c, FragmentChild::Text(_) | FragmentChild::ExpressionTag(_)) {
                        return None;
                    }
                }
                // Attributes are allowed; they get serialized inline or as
                // runtime $.set_attribute calls. Event handlers become
                // $.delegated calls.
                el = Some(e);
            }
            _ => return None,
        }
    }
    el
}

/// Build the each-block lowering when the body is a single RegularElement
/// with text/expr content. Emits a separate program-level template var.
fn build_each_with_element_body(
    blk: &svelte_ast::blocks::EachBlock,
    el: &svelte_ast::elements::RegularElement,
    params: Vec<Value>,
) -> (Vec<Value>, Vec<Value>) {
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
    use svelte_ast::fragment::FragmentChild;
    let mut program_extras: Vec<Value> = Vec::new();

    // Separate attributes: static text-only → embed in HTML; dynamic → assign
    // at runtime via $.set_attribute; events → $.delegated.
    let mut static_attrs: Vec<(String, String)> = Vec::new();
    let mut dyn_attrs: Vec<(String, Value)> = Vec::new();
    let mut events: Vec<(String, Value)> = Vec::new();
    for attr in &el.attributes {
        match attr {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                if is_event_attribute(name) {
                    if let AttributeValue::Single(tag) = value {
                        events.push((name.clone(), tag.expression.clone()));
                    }
                    continue;
                }
                match value {
                    AttributeValue::Empty(true) => {
                        static_attrs.push((name.clone(), String::new()));
                    }
                    AttributeValue::Single(tag) => {
                        dyn_attrs.push((name.clone(), tag.expression.clone()));
                    }
                    AttributeValue::Many(parts) => {
                        let mut text = String::new();
                        let mut all_text = true;
                        for p in parts {
                            match p {
                                AttributeValuePart::Text(t) => text.push_str(&t.data),
                                _ => {
                                    all_text = false;
                                    break;
                                }
                            }
                        }
                        if all_text {
                            static_attrs.push((name.clone(), text));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Inner element HTML for the template var.
    let mut inner_html = format!("<{}", el.name);
    for (n, v) in &static_attrs {
        if v.is_empty() {
            inner_html.push(' ');
            inner_html.push_str(n);
        } else {
            inner_html.push_str(&format!(" {n}=\"{}\"", html_escape_attr(v)));
        }
    }
    inner_html.push('>');
    // Inner text content (static-only — if there's dynamic content, leave empty).
    let inner_nodes: Vec<&FragmentChild> = el.fragment.nodes.iter().collect();
    let last_idx = inner_nodes.len().saturating_sub(1);
    let mut body_has_expression = false;
    let mut body_quasis: Vec<String> = vec![String::new()];
    let mut body_exprs: Vec<Value> = Vec::new();
    for (i, n) in inner_nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let mut data = collapse_ws(&t.data);
                if i == 0 {
                    data = data.trim_start().to_string();
                }
                if i == last_idx {
                    data = data.trim_end().to_string();
                }
                body_quasis.last_mut().unwrap().push_str(&data);
            }
            FragmentChild::ExpressionTag(tag) => {
                body_has_expression = true;
                if let Some(s) = constant_folded_literal(&tag.expression) {
                    body_quasis.last_mut().unwrap().push_str(&s);
                } else {
                    body_exprs.push(tag.expression.clone());
                    body_quasis.push(String::new());
                }
            }
            _ => unreachable!(),
        }
    }
    if !body_has_expression {
        // Static text — embed directly.
        inner_html.push_str(&body_quasis[0]);
    }
    inner_html.push_str(&format!("</{}>", el.name));
    program_extras.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("root_1"),
            Some(b::call(
                b::member(b::id("$"), b::id("from_html"), false, false),
                vec![b::template_literal(vec![&inner_html], vec![])],
            )),
        )],
    ));

    // Build body content.
    let local_name = el.name.clone();
    let mut body_stmts: Vec<Value> = Vec::new();
    body_stmts.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id(&local_name),
            Some(b::call(b::id("root_1"), vec![])),
        )],
    ));

    // Dynamic attributes
    for (name, expr) in &dyn_attrs {
        body_stmts.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("set_attribute"), false, false),
            vec![b::id(&local_name), b::literal_str(name), expr.clone()],
        )));
    }

    // Event handlers (delegated)
    for (name, expr) in &events {
        let event_name = name.strip_prefix("on").unwrap_or(name);
        body_stmts.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("delegated"), false, false),
            vec![b::literal_str(event_name), b::id(&local_name), expr.clone()],
        )));
    }

    // Dynamic textContent (only when text content had expressions)
    if body_has_expression {
        let quasi_refs: Vec<&str> = body_quasis.iter().map(|s| s.as_str()).collect();
        let tpl = b::template_literal(quasi_refs, body_exprs);
        body_stmts.push(b::stmt(b::assignment(
            "=",
            b::member(b::id(&local_name), b::id("textContent"), false, false),
            tpl,
        )));
    }

    body_stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id(&local_name)],
    )));

    // The outer fn-body for the each-block scaffolding.
    let mut out: Vec<Value> = Vec::new();
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("fragment"),
            Some(b::call(
                b::member(b::id("$"), b::id("comment"), false, false),
                vec![],
            )),
        )],
    ));
    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("node"),
            Some(b::call(
                b::member(b::id("$"), b::id("first_child"), false, false),
                vec![b::id("fragment")],
            )),
        )],
    ));
    let collection_thunk = b::arrow(vec![], blk.expression.clone(), false);
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("each"), false, false),
        vec![
            b::id("node"),
            b::literal_num(0.0),
            collection_thunk,
            b::member(b::id("$"), b::id("index"), false, false),
            b::arrow(params, b::block(body_stmts), false),
        ],
    )));
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("fragment")],
    )));
    (program_extras, out)
}

fn build_if_block_client(blk: &svelte_ast::blocks::IfBlock) -> Vec<Value> {
    let _ = blk;
    // Stub — full if-block client lowering needs `$.if(node, condition,
    // consequent, alternate)` with proper anchor management. Not yet ported.
    Vec::new()
}

fn is_event_attribute(name: &str) -> bool {
    if !name.starts_with("on") || name.len() < 3 {
        return false;
    }
    let next = name.as_bytes()[2];
    !next.is_ascii_uppercase() && next != b'-'
}

fn html_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Collect identifier → string-literal-value resolutions from a script body.
/// Only includes `let X = 'literal'` / `let X = number-literal` style bindings
/// AND post-rewrite never-reassigned-state unwraps where init became a literal.
/// Names that are reassigned anywhere are excluded.
fn collect_constant_bindings(
    body: &[Value],
    reassigned: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    fn walk(
        node: &Value,
        out: &mut std::collections::HashMap<String, String>,
        reassigned: &std::collections::HashSet<String>,
    ) {
        match node {
            Value::Array(arr) => arr.iter().for_each(|v| walk(v, out, reassigned)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    let name_opt = obj
                        .get("id")
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(name) = name_opt {
                        if !reassigned.contains(&name) {
                            if let Some(init) = obj.get("init") {
                                if let Some(s) = constant_folded_literal(init) {
                                    out.insert(name, s);
                                }
                            }
                        }
                    }
                }
                for (_, v) in obj.iter() {
                    walk(v, out, reassigned);
                }
            }
            _ => {}
        }
    }
    for s in body {
        walk(s, &mut out, reassigned);
    }
    out
}

/// Like `constant_folded_literal` but also resolves Identifier references via
/// a `constants` map.
fn constant_folded_literal_with(
    expr: &Value,
    constants: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let ty = expr.get("type").and_then(|v| v.as_str())?;
    match ty {
        "Identifier" => {
            let name = expr.get("name").and_then(|v| v.as_str())?;
            constants.get(name).cloned()
        }
        "Literal" => constant_folded_literal(expr),
        "LogicalExpression" => {
            let op = expr.get("operator").and_then(|v| v.as_str())?;
            if op != "??" {
                return None;
            }
            let left = expr.get("left")?;
            let right = expr.get("right")?;
            let left_ty = left.get("type").and_then(|v| v.as_str())?;
            if left_ty == "Literal" {
                let val = left.get("value")?;
                if val.is_null() {
                    return constant_folded_literal_with(right, constants);
                }
            }
            if let Some(s) = constant_folded_literal_with(left, constants) {
                return Some(s);
            }
            constant_folded_literal_with(right, constants)
        }
        "TemplateLiteral" => {
            // `tag` or `${expr}` strings.
            let quasis = expr.get("quasis").and_then(|v| v.as_array())?;
            let exprs = expr.get("expressions").and_then(|v| v.as_array())?;
            let mut out = String::new();
            for (i, q) in quasis.iter().enumerate() {
                let cooked = q
                    .get("value")
                    .and_then(|v| v.get("cooked"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.push_str(cooked);
                if i < exprs.len() {
                    let part = constant_folded_literal_with(&exprs[i], constants)?;
                    out.push_str(&part);
                }
            }
            Some(out)
        }
        _ => constant_folded_literal(expr),
    }
}

fn constant_folded_literal(expr: &Value) -> Option<String> {
    let ty = expr.get("type").and_then(|v| v.as_str())?;
    match ty {
        "Literal" => {
            let val = expr.get("value")?;
            match val {
                Value::String(s) => Some(s.clone()),
                Value::Null => Some(String::new()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            }
        }
        "LogicalExpression" => {
            // Nullish coalescing constant folding.
            //   `null ?? r` → r
            //   `<non-null literal> ?? r` → lhs
            //   `lhs ?? rhs` recursively folded.
            let op = expr.get("operator").and_then(|v| v.as_str())?;
            if op != "??" {
                return None;
            }
            let left = expr.get("left")?;
            let right = expr.get("right")?;
            // Check if left is a null literal.
            let left_ty = left.get("type").and_then(|v| v.as_str())?;
            if left_ty == "Literal" {
                let val = left.get("value")?;
                if val.is_null() {
                    return constant_folded_literal(right);
                }
            }
            // Try folding left; if it succeeds (and isn't "null"), use it.
            if let Some(s) = constant_folded_literal(left) {
                return Some(s);
            }
            constant_folded_literal(right)
        }
        "CallExpression" => {
            // Math.max / Math.min / Math.abs / Math.floor / Math.ceil / Math.round
            let callee = expr.get("callee")?;
            if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
                return None;
            }
            let obj = callee
                .get("object")
                .and_then(|o| o.get("name"))
                .and_then(|v| v.as_str());
            if obj != Some("Math") {
                return None;
            }
            let prop = callee
                .get("property")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())?;
            let args = expr.get("arguments")?.as_array()?;
            let nums: Vec<f64> = args
                .iter()
                .map(|a| {
                    if let Some(s) = constant_folded_literal(a) {
                        s.parse::<f64>().ok()
                    } else {
                        None
                    }
                })
                .collect::<Option<Vec<_>>>()?;
            let r = match prop {
                "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
                "abs" if nums.len() == 1 => nums[0].abs(),
                "floor" if nums.len() == 1 => nums[0].floor(),
                "ceil" if nums.len() == 1 => nums[0].ceil(),
                "round" if nums.len() == 1 => nums[0].round(),
                _ => return None,
            };
            if !r.is_finite() {
                return None;
            }
            if r.fract() == 0.0 {
                Some((r as i64).to_string())
            } else {
                Some(r.to_string())
            }
        }
        _ => None,
    }
}

/// Find a single root RegularElement whose direct children are only
/// ExpressionTag nodes (no Text, no nested blocks). Used to detect the
/// `<p>{expr1}{expr2}...</p>` pattern lowered with $.child / $.reset /
/// $.template_effect.
fn find_single_dynamic_text_element(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::elements::RegularElement> {
    use svelte_ast::fragment::FragmentChild;
    let mut found: Option<&svelte_ast::elements::RegularElement> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::RegularElement(e) => {
                if found.is_some() {
                    return None;
                }
                let mut has_expr = false;
                let mut has_other = false;
                for c in &e.fragment.nodes {
                    match c {
                        FragmentChild::Text(t) if t.data.trim().is_empty() => {}
                        FragmentChild::ExpressionTag(_) => {
                            has_expr = true;
                        }
                        _ => {
                            has_other = true;
                        }
                    }
                }
                if !has_expr || has_other {
                    return None;
                }
                found = Some(e);
            }
            _ => return None,
        }
    }
    found
}

/// Build the tree representation for `\$.from_tree(...)`. Returns the JSON
/// ArrayExpression (or None if the fragment has dynamic content).
fn try_tree_template(fragment: &svelte_ast::Fragment) -> Option<Value> {
    use svelte_ast::fragment::FragmentChild;
    let nodes = fragment_trim_ws(&fragment.nodes);
    let mut elements: Vec<Value> = Vec::new();
    let mut first = true;
    for n in nodes {
        if let FragmentChild::Text(t) = n {
            if t.data.trim().is_empty() {
                continue;
            }
        }
        if !first {
            elements.push(b::literal_str(" "));
        }
        first = false;
        let tree_node = tree_node_from_fragment_child(n)?;
        elements.push(tree_node);
    }
    Some(b::array(elements))
}

/// Recursively build a tree node for a fragment child. Returns None for
/// dynamic content (ExpressionTag, blocks, components, ...).
fn tree_node_from_fragment_child(n: &svelte_ast::fragment::FragmentChild) -> Option<Value> {
    use svelte_ast::fragment::FragmentChild;
    match n {
        FragmentChild::Text(t) => Some(b::literal_str(&collapse_ws(&t.data))),
        FragmentChild::RegularElement(el) => {
            let mut arr: Vec<Value> = Vec::new();
            arr.push(b::literal_str(&el.name));
            // Attributes object or null.
            let attrs = build_tree_attrs(el)?;
            arr.push(attrs);
            // Children.
            let mut first = true;
            let kids = fragment_trim_ws(&el.fragment.nodes);
            for c in kids {
                if let FragmentChild::Text(t) = c {
                    if t.data.trim().is_empty() {
                        continue;
                    }
                }
                if !first {
                    // Insert separator only between consecutive element children.
                    if matches!(c, FragmentChild::RegularElement(_)) {
                        arr.push(b::literal_str(" "));
                    }
                }
                first = false;
                arr.push(tree_node_from_fragment_child(c)?);
            }
            Some(b::array(arr))
        }
        _ => None,
    }
}

fn fragment_trim_ws(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> &[svelte_ast::fragment::FragmentChild] {
    use svelte_ast::fragment::FragmentChild;
    let mut start = 0;
    let mut end = nodes.len();
    while start < end {
        if let FragmentChild::Text(t) = &nodes[start] {
            if t.data.trim().is_empty() {
                start += 1;
                continue;
            }
        }
        break;
    }
    while end > start {
        if let FragmentChild::Text(t) = &nodes[end - 1] {
            if t.data.trim().is_empty() {
                end -= 1;
                continue;
            }
        }
        break;
    }
    &nodes[start..end]
}

fn build_tree_attrs(el: &svelte_ast::elements::RegularElement) -> Option<Value> {
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
    let mut props: Vec<Value> = Vec::new();
    for a in &el.attributes {
        match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                let v = match value {
                    AttributeValue::Empty(true) => b::literal_str(""),
                    AttributeValue::Empty(false) => b::literal_str(""),
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(parts) => {
                        let mut text = String::new();
                        let mut all_text = true;
                        for p in parts {
                            match p {
                                AttributeValuePart::Text(t) => text.push_str(&t.data),
                                _ => {
                                    all_text = false;
                                    break;
                                }
                            }
                        }
                        if !all_text {
                            return None;
                        }
                        b::literal_str(&text)
                    }
                };
                props.push(b::init(name, v));
            }
            _ => return None,
        }
    }
    if props.is_empty() {
        Some(b::literal_null())
    } else {
        Some(b::object(props))
    }
}

fn count_top_level_elements(fragment: &svelte_ast::Fragment) -> usize {
    use svelte_ast::fragment::FragmentChild;
    let mut count = 0;
    for n in &fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::RegularElement(_) | FragmentChild::Component(_) => count += 1,
            _ => {}
        }
    }
    count
}

/// Build `$.await(local, () => promise, pending?, ($$anchor, X) => { ... })`.
/// For now only the simplest form is supported: empty body fragments.
fn build_multiroot_await_call(b: &svelte_ast::blocks::AwaitBlock, local: &str) -> Value {
    let promise_thunk = b::arrow(vec![], b.expression.clone(), false);
    let pending = if let Some(_pending) = b.pending.as_ref() {
        // TODO: lower pending fragment as a callback.
        b::literal_null()
    } else {
        b::literal_null()
    };
    let then_param = b
        .value
        .as_ref()
        .map(|v| {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                vec![b::id("$$anchor"), b::id(name)]
            } else {
                vec![b::id("$$anchor"), v.clone()]
            }
        })
        .unwrap_or_else(|| vec![b::id("$$anchor")]);
    let then_callback = if b.then.is_some() {
        b::arrow(then_param, b::block(vec![]), false)
    } else {
        b::literal_null()
    };
    b::call(
        b::member(b::id("$"), b::id("await"), false, false),
        vec![b::id(local), promise_thunk, pending, then_callback],
    )
}

/// Build a multi-root Component invocation: `Name(local, { props..., get/set })`.
fn build_multiroot_component_call(
    c: &svelte_ast::elements::Component,
    local: &str,
) -> Option<Value> {
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
    let mut props: Vec<Value> = Vec::new();
    for a in &c.attributes {
        match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                let value_expr = match value {
                    AttributeValue::Empty(_) => b::literal_bool(true),
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(parts) => {
                        let mut s = String::new();
                        let mut all_text = true;
                        for p in parts {
                            match p {
                                AttributeValuePart::Text(t) => s.push_str(&t.data),
                                _ => {
                                    all_text = false;
                                    break;
                                }
                            }
                        }
                        if !all_text {
                            return None;
                        }
                        b::literal_str(&s)
                    }
                };
                props.push(b::init(name, value_expr));
            }
            ElementAttribute::BindDirective(bd) if bd.name != "this" => {
                let getter = serde_json::json!({
                    "type": "Property",
                    "kind": "get",
                    "key": { "type": "Identifier", "name": bd.name.clone() },
                    "value": {
                        "type": "FunctionExpression",
                        "async": false,
                        "generator": false,
                        "id": null,
                        "params": [],
                        "body": {
                            "type": "BlockStatement",
                            "body": [{
                                "type": "ReturnStatement",
                                "argument": bd.expression.clone()
                            }]
                        }
                    },
                    "computed": false,
                    "method": false,
                    "shorthand": false
                });
                let setter_call = if let Some(name) = extract_state_name(&bd.expression) {
                    b::call(
                        b::member(b::id("$"), b::id("set"), false, false),
                        vec![b::id(&name), b::id("$$value"), b::literal_bool(true)],
                    )
                } else {
                    b::assignment("=", bd.expression.clone(), b::id("$$value"))
                };
                let setter = serde_json::json!({
                    "type": "Property",
                    "kind": "set",
                    "key": { "type": "Identifier", "name": bd.name.clone() },
                    "value": {
                        "type": "FunctionExpression",
                        "async": false,
                        "generator": false,
                        "id": null,
                        "params": [{ "type": "Identifier", "name": "$$value" }],
                        "body": {
                            "type": "BlockStatement",
                            "body": [{
                                "type": "ExpressionStatement",
                                "expression": setter_call
                            }]
                        }
                    },
                    "computed": false,
                    "method": false,
                    "shorthand": false
                });
                props.push(getter);
                props.push(setter);
            }
            _ => {}
        }
    }
    Some(b::call(b::id(&c.name), vec![b::id(local), b::object(props)]))
}

/// Async-mode variant of `build_template_effect_set_text`. Produces:
///   \$.template_effect(\$0 => \$.set_text(text_var, \$0), void 0, void 0, [\$\$promises[N]])
/// for a single non-foldable expression, where N is the last \$\$promises
/// index that touched any var referenced by the expression.
fn build_template_effect_set_text_async(
    text_var: &str,
    exprs: &[Value],
    var_last_idx: &std::collections::HashMap<String, usize>,
) -> Value {
    // Pick out the LAST $$promises index referenced by any of the expressions.
    let mut max_idx: Option<usize> = None;
    for e in exprs {
        for n in collect_identifier_names(e) {
            if let Some(&i) = var_last_idx.get(&n) {
                max_idx = Some(max_idx.map_or(i, |m| m.max(i)));
            }
        }
    }

    // For a single-expression, emit `$.set_text(text, expr)` — no template
    // literal wrap, no nullish coalesce.
    if exprs.len() == 1 {
        let arrow = b::arrow(
            vec![],
            b::call(
                b::member(b::id("$"), b::id("set_text"), false, false),
                vec![b::id(text_var), exprs[0].clone()],
            ),
            false,
        );
        let void0 = serde_json::json!({
            "type": "UnaryExpression",
            "operator": "void",
            "prefix": true,
            "argument": { "type": "Literal", "value": 0, "raw": "0" }
        });
        let deps = if let Some(idx) = max_idx {
            b::array(vec![serde_json::json!({
                "type": "MemberExpression",
                "object": { "type": "Identifier", "name": "$$promises" },
                "property": { "type": "Literal", "value": idx, "raw": idx.to_string() },
                "computed": true,
                "optional": false
            })])
        } else {
            b::array(vec![])
        };
        return b::call(
            b::member(b::id("$"), b::id("template_effect"), false, false),
            vec![arrow, void0.clone(), void0, deps],
        );
    }
    // Multi-expression async — fall back to the sync builder (rare case).
    build_template_effect_set_text(text_var, exprs)
}

/// Build just the `$.set_text(text_var, \`...\`)` call (no template_effect
/// wrap). Used when bundling multiple set_text calls into one effect.
fn build_set_text_template(text_var: &str, parts: &[DynamicPart]) -> Value {
    let mut quasis: Vec<String> = Vec::new();
    let mut exprs: Vec<Value> = Vec::new();
    let mut buf = String::new();
    for p in parts {
        match p {
            DynamicPart::Static(s) => buf.push_str(s),
            DynamicPart::Expr(e) => {
                quasis.push(std::mem::take(&mut buf));
                exprs.push(serde_json::json!({
                    "type": "LogicalExpression",
                    "operator": "??",
                    "left": e.clone(),
                    "right": { "type": "Literal", "value": "", "raw": "''" }
                }));
            }
        }
    }
    quasis.push(buf);
    let static_parts: Vec<&str> = quasis.iter().map(|s| s.as_str()).collect();
    let tpl = b::template_literal(static_parts, exprs);
    b::call(
        b::member(b::id("$"), b::id("set_text"), false, false),
        vec![b::id(text_var), tpl],
    )
}

/// Build a `$.template_effect(() => $.set_text(text_var, `..${expr ?? ''}..`))` call
/// from a sequence of `DynamicPart`s (mixing static text and expressions).
fn build_template_effect_dynamic(text_var: &str, parts: &[DynamicPart]) -> Value {
    // Merge consecutive static parts and produce the alternating
    // quasi/expression form for a template literal.
    let mut quasis: Vec<String> = Vec::new();
    let mut exprs: Vec<Value> = Vec::new();
    let mut buf = String::new();
    for p in parts {
        match p {
            DynamicPart::Static(s) => buf.push_str(s),
            DynamicPart::Expr(e) => {
                quasis.push(std::mem::take(&mut buf));
                exprs.push(serde_json::json!({
                    "type": "LogicalExpression",
                    "operator": "??",
                    "left": e.clone(),
                    "right": { "type": "Literal", "value": "", "raw": "''" }
                }));
            }
        }
    }
    quasis.push(buf);
    let static_parts: Vec<&str> = quasis.iter().map(|s| s.as_str()).collect();
    let tpl = b::template_literal(static_parts, exprs);
    b::call(
        b::member(b::id("$"), b::id("template_effect"), false, false),
        vec![b::arrow(
            vec![],
            b::call(
                b::member(b::id("$"), b::id("set_text"), false, false),
                vec![b::id(text_var), tpl],
            ),
            false,
        )],
    )
}

/// Build a `$.template_effect(($0,...$N) => $.set_text(<text_var>, \`${$0 ?? ''}...\`), [() => expr0, ...])`
/// call. When N == 1, emit the inline form `$.template_effect(() => $.set_text(text, expr ?? ''))`
/// (no array, no params).
fn build_template_effect_set_text(text_var: &str, exprs: &[Value]) -> Value {
    if exprs.len() == 1 {
        // Inline form: `$.template_effect(() => $.set_text(text, `${expr ?? ''}`))`
        // BUT upstream uses `\`${expr ?? ''}\`` only when there's a text-around it.
        // For pure single-expression case it just passes the expression.
        // Hmm — to be safe, mirror the format used in nullish-coallescence-omittance:
        // `() => $.set_text(text, \`Count is ${$.get(count) ?? ''}\`)`. For a pure single-expr
        // we'll emit `() => $.set_text(text, \`${expr ?? ''}\`)`.
        let expr = &exprs[0];
        let coalesce = serde_json::json!({
            "type": "LogicalExpression",
            "operator": "??",
            "left": expr,
            "right": { "type": "Literal", "value": "", "raw": "''" }
        });
        let tpl = b::template_literal(vec!["", ""], vec![coalesce]);
        return b::call(
            b::member(b::id("$"), b::id("template_effect"), false, false),
            vec![b::arrow(
                vec![],
                b::call(
                    b::member(b::id("$"), b::id("set_text"), false, false),
                    vec![b::id(text_var), tpl],
                ),
                false,
            )],
        );
    }
    // N-form: `$.template_effect(($0, $1, ...) => $.set_text(text, \`${$0 ?? ''}${$1 ?? ''}...\`), [() => expr0, ...])`
    let params: Vec<Value> = (0..exprs.len()).map(|i| b::id(&format!("${i}"))).collect();
    let parts: Vec<Value> = (0..exprs.len())
        .map(|i| {
            serde_json::json!({
                "type": "LogicalExpression",
                "operator": "??",
                "left": { "type": "Identifier", "name": format!("${i}") },
                "right": { "type": "Literal", "value": "", "raw": "''" }
            })
        })
        .collect();
    // template_literal expects Vec<&str> for the static parts; produce them.
    let static_parts: Vec<&str> = (0..exprs.len() + 1).map(|_| "").collect();
    let tpl = b::template_literal(static_parts, parts);
    let arrow_fn = b::arrow(
        params,
        b::call(
            b::member(b::id("$"), b::id("set_text"), false, false),
            vec![b::id(text_var), tpl],
        ),
        false,
    );
    let thunks: Vec<Value> = exprs
        .iter()
        .map(|e| b::arrow(vec![], e.clone(), false))
        .collect();
    b::call(
        b::member(b::id("$"), b::id("template_effect"), false, false),
        vec![arrow_fn, b::array(thunks)],
    )
}

fn find_single_svelte_element(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::elements::SvelteElement> {
    use svelte_ast::fragment::FragmentChild;
    let mut el: Option<&svelte_ast::elements::SvelteElement> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::SvelteElement(e) => {
                if el.is_some() {
                    return None;
                }
                el = Some(e);
            }
            _ => return None,
        }
    }
    el
}

fn find_single_component(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::elements::Component> {
    use svelte_ast::fragment::FragmentChild;
    let mut comp: Option<&svelte_ast::elements::Component> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::Component(c) => {
                if comp.is_some() {
                    return None;
                }
                comp = Some(c);
            }
            _ => return None,
        }
    }
    comp
}

/// Build the call `Foo($$anchor, { ...props })` for a Component, plus any
/// `bind:` directive wrappers (e.g. `$.bind_this(Foo(...), setter, getter)`).
/// Returns the statement list to add to the function body.
fn build_component_call(
    c: &svelte_ast::elements::Component,
    runes_mode: bool,
) -> Vec<Value> {
    use svelte_ast::attributes::{Attribute, AttributeValue, ElementAttribute};
    use svelte_ast::fragment::FragmentChild;

    let mut props: Vec<Value> = Vec::new();
    let mut bind_this: Option<&Value> = None;
    for a in &c.attributes {
        match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                let value_expr = match value {
                    AttributeValue::Empty(_) => b::literal_bool(true),
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(_) => b::literal_str(""),
                };
                props.push(b::init(name, value_expr));
            }
            ElementAttribute::BindDirective(bd) if bd.name == "this" => {
                bind_this = Some(&bd.expression);
            }
            _ => {}
        }
    }
    // Non-runes (legacy) mode adds `$$legacy: true` to props.
    if !runes_mode {
        props.push(b::init("$$legacy", b::literal_bool(true)));
    }

    // Build children callback when the fragment has non-whitespace content.
    let body_trimmed = trim_body_edges(&c.fragment.nodes);
    let has_children = !body_trimmed.is_empty();
    if has_children {
        // Build children body. Currently supports text/expression-only.
        if let Some(children_body) = build_children_body(&body_trimmed) {
            props.push(b::init(
                "children",
                b::arrow(
                    vec![b::id("$$anchor"), b::id("$$slotProps")],
                    b::block(children_body),
                    false,
                ),
            ));
            props.push(b::init(
                "$$slots",
                b::object(vec![b::init("default", b::literal_bool(true))]),
            ));
        }
    }

    let component_call = b::call(b::id(&c.name), vec![b::id("$$anchor"), b::object(props)]);

    if let Some(expr) = bind_this {
        let setter = b::arrow(
            vec![b::id("$$value")],
            b::assignment("=", expr.clone(), b::id("$$value")),
            false,
        );
        let getter = b::arrow(vec![], expr.clone(), false);
        return vec![b::stmt(b::call(
            b::member(b::id("$"), b::id("bind_this"), false, false),
            vec![component_call, setter, getter],
        ))];
    }

    let _ = (has_children, FragmentChild::Text);
    vec![b::stmt(component_call)]
}

/// Build the body of a children-callback for a Component's slot fragment.
/// Currently supports text/expression-only bodies (the most common case for
/// simple slot patterns). Returns None for shapes we don't yet handle.
fn build_children_body(
    nodes: &[&svelte_ast::fragment::FragmentChild],
) -> Option<Vec<Value>> {
    use svelte_ast::fragment::FragmentChild;
    let mut out: Vec<Value> = Vec::new();
    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("next"), false, false),
        vec![],
    )));

    // Build quasis + expressions for set_text.
    let mut quasis: Vec<String> = vec![String::new()];
    let mut expressions: Vec<Value> = Vec::new();
    let mut has_expr = false;
    let last = nodes.len().saturating_sub(1);
    for (i, n) in nodes.iter().enumerate() {
        match n {
            FragmentChild::Text(t) => {
                let mut data = collapse_ws(&t.data);
                if i == 0 {
                    data = data.trim_start().to_string();
                }
                if i == last {
                    data = data.trim_end().to_string();
                }
                quasis.last_mut().unwrap().push_str(&data);
            }
            FragmentChild::ExpressionTag(tag) => {
                if let Some(s) = constant_folded_literal(&tag.expression) {
                    quasis.last_mut().unwrap().push_str(&s);
                } else {
                    expressions.push(tag.expression.clone());
                    quasis.push(String::new());
                    has_expr = true;
                }
            }
            _ => return None,
        }
    }

    out.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id("text"),
            Some(b::call(
                b::member(b::id("$"), b::id("text"), false, false),
                vec![],
            )),
        )],
    ));

    if has_expr {
        // Build `\`${e0 ?? ''}${e1 ?? ''}...\`` template.
        let mut tpl_quasis: Vec<String> = vec![quasis[0].clone()];
        let mut tpl_exprs: Vec<Value> = Vec::new();
        for (i, expr) in expressions.iter().enumerate() {
            tpl_exprs.push(b::logical("??", expr.clone(), b::literal_str("")));
            tpl_quasis.push(quasis[i + 1].clone());
        }
        let qrefs: Vec<&str> = tpl_quasis.iter().map(|s| s.as_str()).collect();
        let tpl = b::template_literal(qrefs, tpl_exprs);
        out.push(b::stmt(b::call(
            b::member(b::id("$"), b::id("template_effect"), false, false),
            vec![b::arrow(
                vec![],
                b::call(
                    b::member(b::id("$"), b::id("set_text"), false, false),
                    vec![b::id("text"), tpl],
                ),
                false,
            )],
        )));
    } else {
        let joined: String = quasis.join("");
        out.push(b::stmt(b::assignment(
            "=",
            b::member(b::id("text"), b::id("nodeValue"), false, false),
            b::literal_str(&joined),
        )));
    }

    out.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id("text")],
    )));
    Some(out)
}
