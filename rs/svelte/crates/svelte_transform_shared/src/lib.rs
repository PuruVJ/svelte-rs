//! Shared transform utilities (used by both client and server transforms).
//!
//! Builders construct typed `svelte_js_ast` nodes — no `serde_json::Value`.

#![forbid(unsafe_code)]

pub mod builders_typed;
pub mod compile_bump;
pub mod template_meta;
pub mod template_slab;
