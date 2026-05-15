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

    // Lower the template, then rewrite derived references in every embedded
    // expression so `{counter.count}` becomes `${$.escape(counter().count)}`
    // when `counter` is a derived binding.
    let mut root_with_rewritten_template: Root = root.clone();
    if !derived_names.is_empty() {
        rewrite_fragment_derived_refs(&mut root_with_rewritten_template.fragment, &derived_names);
    }
    let template_ops = template::lower_fragment_trimmed(&root_with_rewritten_template.fragment);
    let mut function_body: Vec<Value> = Vec::new();

    // Instance script body — runs each render. Strips imports/exports (hoisted).
    if let Some(rewritten) = &rewritten_instance {
        let (hoisted, body) = split_script_body(rewritten);
        function_body.extend(body);
        let _ = hoisted;
    }

    function_body.extend(template::ops_to_statements(template_ops));

    let needs_props = uses_props(&root);
    let mut params = vec![b::id("$$renderer")];
    if needs_props {
        params.push(b::id("$$props"));
    }

    let component_block = b::block(function_body);
    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        component_block,
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

    program_body.push(b::export_default(component_fn));
    b::program(program_body)
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

/// Whether the component uses `experimental.async` features. This isn't a
/// pure source-code check — upstream gates it on `compileOptions.experimental.async`
/// which the per-fixture `_config.js` controls. As a heuristic when we don't
/// have that config available, treat top-level await (in script or template
/// tags) as a strong signal. `{#await}` blocks alone don't trigger this —
/// they work in non-async mode too.
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
