//! AST → Svelte source code.
//!
//! Mirrors `packages/svelte/src/compiler/print/`. Used by tools that want
//! to round-trip a parsed component (formatters, codemods, devtools).
//!
//! The output is *valid* Svelte but formatting may differ from the input.

#![forbid(unsafe_code)]

mod printer;

pub use printer::{print, PrintOptions, PrintResult};
