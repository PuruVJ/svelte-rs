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
    let template_ops = template::lower_fragment(&root.fragment);
    let mut function_body: Vec<Value> = Vec::new();

    // Instance script body — runs each render. Strips imports/exports (hoisted)
    // and erases runes (`$state`/`$derived`/`$effect`/...).
    if let Some(instance) = &root.instance {
        let rewritten = rewrite::rewrite_program(instance.content.clone());
        let (hoisted, body) = split_script_body(&rewritten);
        function_body.extend(body);
        let _ = hoisted; // collected in build_program below
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
    program_body.push(b::import_all("$", "svelte/internal/server"));

    if let Some(module_script) = &root.module {
        program_body.extend(extract_program_body(&module_script.content));
    }
    if let Some(instance) = &root.instance {
        let rewritten = rewrite::rewrite_program(instance.content.clone());
        let (hoisted, _body) = split_script_body(&rewritten);
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
