//! Phase 3 client transform.
//!
//! Mirrors `packages/svelte/src/compiler/phases/3-transform/client/`.
//!
//! Current scope: minimal pipeline producing byte-equal output for the
//! simplest snapshot fixtures (`hello-world`, etc.). The real 53-visitor
//! port lands incrementally driven by failing snapshots.

#![forbid(unsafe_code)]

use serde_json::Value;
use svelte_ast::root::Root;
use svelte_transform_shared::builders as b;

pub mod template;

pub use template::serialize_static_html;

/// Transform a parsed `Root` into a client-side ESTree `Program`.
///
/// The shape mirrors upstream's `client_component` (`transform-client.js`):
///   import 'svelte/internal/disclose-version';
///   import 'svelte/internal/flags/legacy';
///   import * as $ from 'svelte/internal/client';
///
///   var root = $.from_html(`<HTML>`);
///
///   export default function Name($$anchor) {
///     var node = root();
///     $.append($$anchor, node);
///   }
pub fn client_component(root: &Root, component_name: &str) -> Value {
    let html = template::serialize_static_html(&root.fragment);

    let mut program_body: Vec<Value> = Vec::new();
    program_body.push(import_side_effect("svelte/internal/disclose-version"));
    program_body.push(import_side_effect("svelte/internal/flags/legacy"));
    program_body.push(b::import_all("$", "svelte/internal/client"));

    // Hoisted instance imports
    if let Some(instance) = &root.instance {
        let body = instance
            .content
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

    if !html.is_empty() {
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
                vec![b::declarator(b::id(&top_name), Some(b::call(b::id("root"), vec![])))],
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

    program_body.push(b::export_default(b::function_declaration(
        b::id(component_name),
        vec![b::id("$$anchor")],
        b::block(fn_body),
        false,
    )));

    b::program(program_body)
}

fn import_side_effect(source: &str) -> Value {
    serde_json::json!({
        "type": "ImportDeclaration",
        "specifiers": [],
        "source": b::literal_str(source)
    })
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
