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

    // Hoisted instance imports
    let rewritten_instance = root.instance.as_ref().map(|i| rewrite::rewrite_program(i.content.clone()));

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

    b::program(program_body)
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
                // No attributes (for now)
                if !e.attributes.is_empty() {
                    return None;
                }
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
    use svelte_ast::fragment::FragmentChild;
    let mut program_extras: Vec<Value> = Vec::new();

    // Inner element HTML for the template var.
    let inner_html = format!("<{name}></{name}>", name = el.name);
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

    // Build body content: var p = root_1(); p.textContent = `template literal`;
    // $.append($$anchor, p);
    let local_name = el.name.clone();
    let mut body_stmts: Vec<Value> = Vec::new();
    body_stmts.push(b::declaration(
        "var",
        vec![b::declarator(
            b::id(&local_name),
            Some(b::call(b::id("root_1"), vec![])),
        )],
    ));

    // Build template literal from inner text/expressions.
    let mut quasis: Vec<String> = vec![String::new()];
    let mut expressions: Vec<Value> = Vec::new();
    let mut is_purely_static = true;
    let inner_nodes: Vec<&FragmentChild> = el.fragment.nodes.iter().collect();
    let last = inner_nodes.len().saturating_sub(1);
    for (i, n) in inner_nodes.iter().enumerate() {
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
                    is_purely_static = false;
                }
            }
            _ => unreachable!(),
        }
    }
    let quasi_refs: Vec<&str> = quasis.iter().map(|s| s.as_str()).collect();
    let tpl = b::template_literal(quasi_refs, expressions);
    body_stmts.push(b::stmt(b::assignment(
        "=",
        b::member(b::id(&local_name), b::id("textContent"), false, false),
        tpl,
    )));
    body_stmts.push(b::stmt(b::call(
        b::member(b::id("$"), b::id("append"), false, false),
        vec![b::id("$$anchor"), b::id(&local_name)],
    )));
    let _ = is_purely_static;

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
    if ty != "Literal" {
        return None;
    }
    let val = expr.get("value")?;
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Null => Some(String::new()),
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

    let component_call = b::call(b::id(&c.name), vec![b::id("$$anchor"), b::object(props)]);

    if let Some(expr) = bind_this {
        // $.bind_this(call, ($$value) => target = $$value, () => target)
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

    vec![b::stmt(component_call)]
}
