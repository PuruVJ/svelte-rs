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
    if !runes_mode {
        program_body.push(import_side_effect("svelte/internal/flags/legacy"));
    }
    program_body.push(b::import_all("$", "svelte/internal/client"));

    // Hoisted instance imports + state-name collection (for template-side
    // expression rewriting).
    let (rewritten_instance, state_names): (Option<Value>, std::collections::HashSet<String>) =
        match root.instance.as_ref() {
            Some(i) => {
                let (prog, names) = rewrite::rewrite_program_with_state(i.content.clone());
                (Some(prog), names)
            }
            None => (None, Default::default()),
        };

    // Rewrite template-embedded expressions to use $.get/$.set for state.
    let mut root_owned: Root = root.clone();
    if !state_names.is_empty() {
        rewrite_fragment_state_refs(&mut root_owned.fragment, &state_names);
    }
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

    let mut fn_body: Vec<Value> = Vec::new();

    // Instance-script non-import statements appear inside the component fn.
    if let Some(rewritten) = &rewritten_instance {
        let body = rewritten
            .get("body")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for stmt in body {
            let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(ty, "ImportDeclaration") {
                continue;
            }
            fn_body.push(stmt);
        }
    }

    // Try the multi-root static template pattern (e.g. `<p>...</p> <Component .../>`).
    if let Some((tpl_html, fn_stmts)) = try_multi_root_static(&root.fragment) {
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
            // Single `{#each}` block at root.
            let (prog_extras, fn_stmts) = build_each_block_client(blk);
            for s in prog_extras {
                program_body.push(s);
            }
            for stmt in fn_stmts {
                fn_body.push(stmt);
            }
        } else if let Some(blk) = find_single_if_block(&root.fragment.nodes) {
            for stmt in build_if_block_client(blk) {
                fn_body.push(stmt);
            }
        }
    }

    let delegated_events = collect_delegated_event_names(&fn_body);

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
    fn_body: Vec<Value>,
    component_name: &str,
    options: &ClientOptions,
) -> Value {
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
    b::program(program_body)
}

/// Try to lower the fragment as a multi-root static template (multiple
/// top-level elements / Components, all with statically-known content).
/// Returns the template HTML + function body statements.
fn try_multi_root_static(
    fragment: &svelte_ast::Fragment,
) -> Option<(String, Vec<Value>)> {
    use svelte_ast::fragment::FragmentChild;
    use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};

    // Collect top-level nodes, filtering out whitespace-only text and HTML
    // comments.
    let mut roots: Vec<&FragmentChild> = Vec::new();
    for n in &fragment.nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::RegularElement(_) | FragmentChild::Component(_) => roots.push(n),
            _ => return None,
        }
    }
    if roots.len() < 2 {
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
                if !el.attributes.is_empty() {
                    return None;
                }
                let mut text = String::new();
                let mut has_expression = false;
                let mut runtime_expr: Option<Value> = None;
                let inner = trim_body_edges(&el.fragment.nodes);
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
                            text.push_str(&data);
                        }
                        FragmentChild::ExpressionTag(tag) => {
                            has_expression = true;
                            if let Some(s) = constant_folded_literal(&tag.expression) {
                                text.push_str(&s);
                            } else if is_pure_expression(&tag.expression) {
                                runtime_expr = Some(tag.expression.clone());
                                break;
                            } else {
                                return None;
                            }
                        }
                        _ => return None,
                    }
                }
                html.push('<');
                html.push_str(&el.name);
                html.push('>');
                // Embed text in HTML only when there were NO expression tags
                // (pure static Text content).
                if !has_expression {
                    html.push_str(&text);
                }
                html.push_str("</");
                html.push_str(&el.name);
                html.push('>');
                let content = if let Some(expr) = runtime_expr {
                    MultiRootContent::PureNonReactive(expr)
                } else if has_expression {
                    // Const-folded literal — assign as string at runtime.
                    MultiRootContent::ConstFolded(text.clone())
                } else {
                    MultiRootContent::Static
                };
                slots.push(MultiRootSlot::Element {
                    name: el.name.clone(),
                    content,
                    static_text: text,
                });
            }
            FragmentChild::Component(c) => {
                html.push_str("<!>");
                slots.push(MultiRootSlot::Component(c));
            }
            _ => return None,
        }
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
    for (idx, slot) in slots.iter().enumerate() {
        let base = match slot {
            MultiRootSlot::Element { name, .. } => name.clone(),
            MultiRootSlot::Component(_) => "node".to_string(),
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
        if let MultiRootSlot::Element { content, .. } = slot {
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
                MultiRootContent::Static => {}
            }
        }
    }

    // Now emit Component invocations.
    for (idx, slot) in slots.iter().enumerate() {
        if let MultiRootSlot::Component(c) = slot {
            let local = &local_names[idx];
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
                    _ => {}
                }
            }
            out.push(b::stmt(b::call(
                b::id(&c.name),
                vec![b::id(local), b::object(props)],
            )));
        }
    }

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
    },
    Component(&'a svelte_ast::elements::Component),
}

#[derive(Debug)]
enum MultiRootContent {
    Static,
    PureNonReactive(Value),
    ConstFolded(String),
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
