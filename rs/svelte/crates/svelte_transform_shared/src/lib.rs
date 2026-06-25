//! Shared transform utilities (used by both client and server transforms).
//!
//! Builders construct typed `svelte_js_ast` nodes — no `serde_json::Value`.

#![forbid(unsafe_code)]

pub mod builders_typed;
