//! Phase 3 server transform.
//!
//! Mirrors `packages/svelte/src/compiler/phases/3-transform/server/`.
//!
//! Current scope: lower a template fragment to `$$renderer.push(\`...\`)`
//! statements interleaved with control-flow lowered from `{#if}` / `{#each}`.
//! Real visitor coverage — components, snippets, await/key blocks, dynamic
//! tags, all directives, the full script-body integration — is being filled
//! in driven by failing snapshot fixtures.

#![forbid(unsafe_code)]

use serde_json::Value;
use svelte_ast::root::Root;
use svelte_transform_shared::builders as b;

pub mod rewrite;
pub mod template;

pub use template::{ops_to_statements, TemplateChunks, TemplateOp};

/// Transform an analyzed `Root` into an acorn-shaped JSON `Program` ready for
/// `svelte_codegen_js::print()`.
pub fn server_component(root: &Root, component_name: &str) -> Value {
    // Collect derived bindings up front so the template-expression rewriter
    // can call them as thunks.
    let mut rewritten_instance: Option<Value> = None;
    let mut derived_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(instance) = &root.instance {
        let rewritten = rewrite::rewrite_program(instance.content.clone());
        derived_names = rewrite::collect_derived_names(&rewritten);
        rewritten_instance = Some(rewritten);
    }

    // Hoist top-level `{#snippet}` blocks out of the fragment — upstream emits
    // them as sibling `function NAME($$renderer, ...) { ... }` declarations
    // before `export default function Component(...)`.
    let mut root_with_rewritten_template: Root = root.clone();
    if !derived_names.is_empty() {
        rewrite_fragment_derived_refs(&mut root_with_rewritten_template.fragment, &derived_names);
    }
    let hoisted_snippets = extract_top_level_snippets(&mut root_with_rewritten_template.fragment);
    let template_ops = template::lower_fragment_trimmed(&root_with_rewritten_template.fragment);
    let mut function_body: Vec<Value> = Vec::new();

    // Instance script body — runs each render. Strips imports/exports (hoisted).
    if let Some(rewritten) = &rewritten_instance {
        let (hoisted, body) = split_script_body(rewritten);
        function_body.extend(body);
        let _ = hoisted;
    }

    function_body.extend(template::ops_to_statements(template_ops));

    let needs_context = component_needs_context(&root);
    let needs_props = uses_props(&root) || needs_context;
    let mut params = vec![b::id("$$renderer")];
    if needs_props {
        params.push(b::id("$$props"));
    }

    // When the component needs context (class fields with $state, full
    // `let props = $props()` rebind without destructuring, etc.), wrap the
    // body in `$$renderer.component(($$renderer) => { ... })`. Mirrors
    // upstream's `should_inject_context` path in `transform-server.js:259-269`.
    let body_value = if needs_context {
        // Rewrite `let X = $$props` (which came from `let X = $props()` via
        // the rune erasure) to the destructured form
        // `let { $$slots, $$events, ...X } = $$props;`.
        let function_body = rewrite_full_props_rebind(function_body);
        let wrapped = b::block(vec![b::stmt(b::call(
            b::member(b::id("$$renderer"), b::id("component"), false, false),
            vec![b::arrow(
                vec![b::id("$$renderer")],
                b::block(function_body),
                false,
            )],
        ))]);
        wrapped
    } else {
        b::block(function_body)
    };

    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        body_value,
        false,
    );

    // Module script body — runs once. All of its statements are hoisted to the
    // top of the Program (after the `$` import).
    let mut program_body: Vec<Value> = Vec::new();
    if uses_async(root) {
        // `import 'svelte/internal/flags/async';` as a side-effect — matches
        // upstream's `if (options.experimental.async)` injection in
        // transform-server.js:388-390.
        program_body.push(serde_json::json!({
            "type": "ImportDeclaration",
            "specifiers": [],
            "source": b::literal_str("svelte/internal/flags/async")
        }));
    }
    program_body.push(b::import_all("$", "svelte/internal/server"));

    if let Some(module_script) = &root.module {
        program_body.extend(extract_program_body(&module_script.content));
    }
    if let Some(rewritten) = &rewritten_instance {
        let (hoisted, _body) = split_script_body(rewritten);
        program_body.extend(hoisted);
    }

    // Hoisted snippet function declarations go between the imports and the
    // export-default component function.
    for snippet in hoisted_snippets {
        program_body.push(snippet);
    }

    program_body.push(b::export_default(component_fn));
    b::program(program_body)
}

/// Remove top-level `{#snippet name(...)}{/snippet}` blocks from the fragment
/// and lower each to a `function name($$renderer, ...params) { ... }`
/// declaration. Mirrors upstream's snippet hoisting in
/// `transform-server.js` (handles the `uses_component_bindings` path's
/// snippet collection).
fn extract_top_level_snippets(f: &mut svelte_ast::Fragment) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    let mut hoisted: Vec<Value> = Vec::new();
    let nodes = std::mem::take(&mut f.nodes);
    for node in nodes {
        if let FragmentChild::SnippetBlock(blk) = &node {
            // Snippet bodies always get a leading `<!---->` marker (regardless
            // of whether the body has dynamic content), so upstream's runtime
            // can locate the snippet's start in the parent template.
            let ops = template::lower_fragment_trimmed(&blk.body);
            let body = template::prepend_marker_and_to_statements(ops);
            let name = blk
                .expression
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("$$snippet")
                .to_string();
            let params: Vec<Value> = std::iter::once(b::id("$$renderer"))
                .chain(blk.parameters.iter().cloned())
                .collect();
            hoisted.push(b::function_declaration(b::id(&name), params, b::block(body), false));
        } else {
            f.nodes.push(node);
        }
    }
    hoisted
}

/// Whether the component needs a `$$props` parameter. Returns true if the
/// instance script contains a `$props()` call or `$$props` identifier
/// reference. Mirrors upstream's `should_inject_props` check
/// (`transform-server.js:311-318`).
fn uses_props(root: &Root) -> bool {
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    json.contains("\"$props\"") || json.contains("\"$$props\"")
}

/// Decide whether the component needs the `$$renderer.component(...)` wrapper.
/// Activates when:
/// - The instance script has `let X = $props()` (full-rebind, no destructuring).
/// - A class contains `$state`/`$derived` fields.
/// Mirrors upstream's `analysis.needs_context` set in `phases/2-analyze/index.js`.
fn component_needs_context(root: &Root) -> bool {
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    if (json.contains("\"ClassDeclaration\"") || json.contains("\"ClassExpression\""))
        && json.contains("\"$state\"")
    {
        return true;
    }
    has_full_props_rebind(&instance.content)
}

/// `let X = $props()` (no destructuring) — walks the parsed program.
fn has_full_props_rebind(program: &Value) -> bool {
    fn walk(node: &Value) -> bool {
        match node {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    let id_is_identifier = obj
                        .get("id")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("Identifier");
                    let init = obj.get("init");
                    let init_is_props = init
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("CallExpression")
                        && init
                            .and_then(|v| v.get("callee"))
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            == Some("$props");
                    if id_is_identifier && init_is_props {
                        return true;
                    }
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(program)
}

/// Rewrite `let X = $$props;` (the result of `let X = $props()` after rune
/// erasure) into `let { $$slots, $$events, ...X } = $$props;` so the context
/// wrapper can pull off slots/events. Mirrors upstream's full-rebind path.
fn rewrite_full_props_rebind(body: Vec<Value>) -> Vec<Value> {
    body.into_iter()
        .map(|stmt| {
            if stmt.get("type").and_then(|v| v.as_str()) != Some("VariableDeclaration") {
                return stmt;
            }
            let mut stmt = stmt;
            if let Some(decls) = stmt.get_mut("declarations").and_then(|v| v.as_array_mut()) {
                for d in decls.iter_mut() {
                    let is_init_props = d
                        .get("init")
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                        == Some("$$props");
                    let id_is_ident = d
                        .get("id")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("Identifier");
                    if is_init_props && id_is_ident {
                        let name = d
                            .get("id")
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("props")
                            .to_string();
                        d["id"] = serde_json::json!({
                            "type": "ObjectPattern",
                            "properties": [
                                {
                                    "type": "Property",
                                    "kind": "init",
                                    "key": { "type": "Identifier", "name": "$$slots" },
                                    "value": { "type": "Identifier", "name": "$$slots" },
                                    "computed": false,
                                    "shorthand": true,
                                    "method": false
                                },
                                {
                                    "type": "Property",
                                    "kind": "init",
                                    "key": { "type": "Identifier", "name": "$$events" },
                                    "value": { "type": "Identifier", "name": "$$events" },
                                    "computed": false,
                                    "shorthand": true,
                                    "method": false
                                },
                                {
                                    "type": "RestElement",
                                    "argument": { "type": "Identifier", "name": name }
                                }
                            ]
                        });
                    }
                }
            }
            stmt
        })
        .collect()
}

/// Whether the component uses `experimental.async` features.
fn uses_async(root: &Root) -> bool {
    use svelte_ast::fragment::{Fragment, FragmentChild};
    fn json_has_await(v: &Value) -> bool {
        v.to_string().contains("\"AwaitExpression\"")
    }
    fn fragment_uses_async(f: &Fragment) -> bool {
        for n in &f.nodes {
            match n {
                FragmentChild::ExpressionTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::HtmlTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::ConstTag(t) if json_has_await(&t.declaration) => return true,
                FragmentChild::RenderTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::RegularElement(el) => {
                    if fragment_uses_async(&el.fragment) {
                        return true;
                    }
                }
                FragmentChild::IfBlock(b) => {
                    if json_has_await(&b.test) || fragment_uses_async(&b.consequent) {
                        return true;
                    }
                    if let Some(alt) = &b.alternate {
                        if fragment_uses_async(alt) {
                            return true;
                        }
                    }
                }
                FragmentChild::EachBlock(b) => {
                    if json_has_await(&b.expression) || fragment_uses_async(&b.body) {
                        return true;
                    }
                }
                FragmentChild::KeyBlock(b) => {
                    if json_has_await(&b.expression) || fragment_uses_async(&b.fragment) {
                        return true;
                    }
                }
                FragmentChild::Component(c) => {
                    if fragment_uses_async(&c.fragment) {
                        return true;
                    }
                }
                FragmentChild::SvelteElement(el) => {
                    if fragment_uses_async(&el.fragment) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
    if fragment_uses_async(&root.fragment) {
        return true;
    }
    if let Some(instance) = &root.instance {
        // Top-level await in instance script — must be experimental.async.
        if instance_has_top_level_await(&instance.content) {
            return true;
        }
    }
    false
}

/// Walk only top-level statements of `program.body[]` (not into nested
/// FunctionDeclaration / ArrowFunctionExpression / FunctionExpression bodies)
/// to detect an await that requires `experimental.async`.
fn instance_has_top_level_await(program: &Value) -> bool {
    fn walk(node: &Value) -> bool {
        match node {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ty == "AwaitExpression" {
                    return true;
                }
                if matches!(
                    ty,
                    "FunctionDeclaration" | "FunctionExpression" | "ArrowFunctionExpression"
                ) {
                    return false;
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(program)
}

/// Walk a `Fragment` rewriting every embedded expression so identifiers in
/// `deriveds` become `name()` calls. Used for server's `$derived` getter
/// invocation pattern.
fn rewrite_fragment_derived_refs(
    f: &mut svelte_ast::Fragment,
    deriveds: &std::collections::HashSet<String>,
) {
    use svelte_ast::fragment::FragmentChild;
    for node in f.nodes.iter_mut() {
        match node {
            FragmentChild::ExpressionTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::HtmlTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::ConstTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.declaration, deriveds);
            }
            FragmentChild::RenderTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::IfBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.test, deriveds);
                rewrite_fragment_derived_refs(&mut b.consequent, deriveds);
                if let Some(alt) = b.alternate.as_mut() {
                    rewrite_fragment_derived_refs(alt, deriveds);
                }
            }
            FragmentChild::EachBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                rewrite_fragment_derived_refs(&mut b.body, deriveds);
                if let Some(fb) = b.fallback.as_mut() {
                    rewrite_fragment_derived_refs(fb, deriveds);
                }
            }
            FragmentChild::KeyBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                rewrite_fragment_derived_refs(&mut b.fragment, deriveds);
            }
            FragmentChild::AwaitBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                if let Some(f) = b.pending.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
                if let Some(f) = b.then.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
                if let Some(f) = b.catch_.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
            }
            FragmentChild::SnippetBlock(b) => {
                rewrite_fragment_derived_refs(&mut b.body, deriveds);
            }
            FragmentChild::RegularElement(el) => {
                rewrite_attrs_derived_refs(&mut el.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut el.fragment, deriveds);
            }
            FragmentChild::Component(c) => {
                rewrite_attrs_derived_refs(&mut c.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut c.fragment, deriveds);
            }
            FragmentChild::SvelteElement(el) => {
                rewrite::rewrite_derived_refs(&mut el.tag, deriveds);
                rewrite_attrs_derived_refs(&mut el.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut el.fragment, deriveds);
            }
            FragmentChild::SvelteHead(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            FragmentChild::SvelteFragment(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            FragmentChild::TitleElement(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            _ => {}
        }
    }
}

fn rewrite_attrs_derived_refs(
    attrs: &mut Vec<svelte_ast::ElementAttribute>,
    deriveds: &std::collections::HashSet<String>,
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in attrs.iter_mut() {
        match a {
            ElementAttribute::Attribute(attr) => match &mut attr.value {
                AttributeValue::Single(tag) => {
                    rewrite::rewrite_derived_refs(&mut tag.expression, deriveds);
                }
                AttributeValue::Many(parts) => {
                    for p in parts.iter_mut() {
                        if let AttributeValuePart::ExpressionTag(t) = p {
                            rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
                        }
                    }
                }
                AttributeValue::Empty(_) => {}
            },
            ElementAttribute::BindDirective(bd) => {
                rewrite::rewrite_derived_refs(&mut bd.expression, deriveds);
            }
            ElementAttribute::SpreadAttribute(sa) => {
                rewrite::rewrite_derived_refs(&mut sa.expression, deriveds);
            }
            _ => {}
        }
    }
}

/// Pull `body[]` out of a parsed ESTree Program JSON value.
fn extract_program_body(program: &Value) -> Vec<Value> {
    program
        .get("body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Split the instance `<script>` content into (hoisted top-level decls, body
/// stmts that run each render). Imports are hoisted; everything else stays.
fn split_script_body(program: &Value) -> (Vec<Value>, Vec<Value>) {
    let body = extract_program_body(program);
    let mut hoisted: Vec<Value> = Vec::new();
    let mut keep: Vec<Value> = Vec::new();
    for stmt in body {
        let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "ImportDeclaration" => hoisted.push(stmt),
            "ExportNamedDeclaration" => {
                // `export const x = ...` → the inner declaration stays in the
                // function body; the `export` wrapper is dropped. (For now.)
                if let Some(decl) = stmt.get("declaration") {
                    if !decl.is_null() {
                        keep.push(decl.clone());
                    }
                }
            }
            _ => keep.push(stmt),
        }
    }
    (hoisted, keep)
}
