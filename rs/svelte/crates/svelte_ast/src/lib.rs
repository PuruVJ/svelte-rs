//! Svelte template AST.
//!
//! Typed throughout — all JS sub-trees use `svelte_js_ast` types
//! (`Expression`, `Pattern`, `Program`, `VariableDeclaration`, `Identifier`).
//! No `serde_json::Value` here.

#![forbid(unsafe_code)]

pub mod arena;
pub mod attributes;
pub mod blocks;
pub mod css;
pub mod elements;
pub mod fragment;
pub mod position;
pub mod root;
pub mod tags;

pub use arena::*;
pub use attributes::*;
pub use blocks::*;
pub use elements::*;
pub use fragment::*;
pub use position::*;
pub use root::*;
pub use tags::*;

