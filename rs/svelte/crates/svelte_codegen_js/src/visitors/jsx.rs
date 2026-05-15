//! JSX visitors. Stubs — Svelte does not emit JSX, but keeping the surface
//! shaped so the visitor table parity with esrap stays clean. Filled in only
//! if a future fixture needs them.

use serde_json::Value;

use crate::context::Context;

pub fn jsx_unimplemented(node: &Value, _ctx: &mut Context) {
    panic!(
        "svelte_codegen_js: JSX node `{}` not yet implemented",
        node.get("type").and_then(|v| v.as_str()).unwrap_or("?")
    );
}
