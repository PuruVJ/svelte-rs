//! Typed-AST JS codegen.
//!
//! Consumes `svelte_js_ast::Program` and emits a JS source string + decoded
//! sourcemap segments. Layout decisions (line wrapping, comment placement,
//! parenthesization) match esrap (`node_modules/esrap@2.2.4`).
//!
//! Output is byte-equivalent to upstream esrap, as gated by the snapshot
//! suite in `packages/svelte/tests/snapshot/samples/*/_expected/{client,server}/*.svelte.js`.

#![forbid(unsafe_code)]

pub mod typed;

pub use typed::{
    print_expression_str, print_pattern_str, print_statements_str, print_typed,
    LineMap, Segment, TypedComment, TypedCommentKind, TypedPrintOptions,
    TypedPrintResult,
};
