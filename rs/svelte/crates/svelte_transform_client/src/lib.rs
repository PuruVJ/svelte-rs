//! Client-side typed transform.
//!
//! Greenfield rewrite: every output is `svelte_js_ast::Program`, no
//! `serde_json::Value` anywhere. Coverage grows fixture-by-fixture from the
//! simplest static templates outward. Shapes that aren't yet handled return
//! `None` from the entry points; `svelte_compiler::compile` surfaces that as
//! a `typed_client_unsupported` diagnostic.

#![forbid(unsafe_code)]

mod typed_fast;

pub use typed_fast::try_typed_client;

use svelte_ast::fragment::FragmentChild;
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

/// Compile options threaded through from `svelte_compiler::CompileOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub filename: Option<String>,
    pub dev: bool,
    pub hmr: bool,
    pub experimental_async: bool,
    pub fragments: FragmentsMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FragmentsMode {
    #[default]
    Html,
    Tree,
}

/// Second-tier typed entry point: shapes too complex for `try_typed_client`
/// (single static root with no script/css) but still supported.
///
/// Currently handles: "instance script with imports only + empty template".
pub fn try_typed_client_component(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() {
        return None;
    }
    // Skip when typed_fast can already handle it.
    if root.instance.is_none() && root.module.is_none() && !fragment_is_empty(&root.fragment) {
        return None;
    }
    let fragment_empty = fragment_is_empty(&root.fragment);

    // Extract script imports + non-import statements.
    let (script_imports, script_body) = match root.instance.as_ref() {
        Some(s) => partition_imports(&s.content.body)?,
        None => (Vec::new(), Vec::new()),
    };
    // Module script not yet supported in this entry point.
    if root.module.is_some() {
        return None;
    }
    // Body that goes inside the function (anything except module-level imports).
    if !script_body.is_empty() {
        return None; // script contains non-import statements — needs more work
    }
    // Template must be empty for this entry point.
    if !fragment_empty {
        return None;
    }

    let mut top: Vec<Statement> = Vec::with_capacity(4 + script_imports.len());
    top.push(t::import_side_effect("svelte/internal/disclose-version"));
    top.push(t::import_side_effect("svelte/internal/flags/legacy"));
    top.push(t::import_namespace("$", "svelte/internal/client"));
    top.extend(script_imports);
    top.push(t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        Vec::new(),
    ));
    Some(t::program(top))
}

fn fragment_is_empty(f: &svelte_ast::fragment::Fragment) -> bool {
    f.nodes.iter().all(|n| match n {
        FragmentChild::Text(t) => t.data.trim().is_empty(),
        _ => false,
    })
}

/// Split a Program body into (top-level imports, everything else).
/// Returns None if any non-import statement appears between imports
/// (we'd need to preserve ordering more carefully).
fn partition_imports(body: &[Statement]) -> Option<(Vec<Statement>, Vec<Statement>)> {
    let mut imports = Vec::new();
    let mut rest = Vec::new();
    let mut saw_non_import = false;
    for s in body {
        match s {
            Statement::Import(_) => {
                if saw_non_import {
                    // Interleaved imports — bail for now.
                    return None;
                }
                imports.push(s.clone());
            }
            _ => {
                saw_non_import = true;
                rest.push(s.clone());
            }
        }
    }
    Some((imports, rest))
}
