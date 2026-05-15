//! Port of `esrap@2.2.4` — estree-shaped JSON → JavaScript string + sourcemap.
//!
//! Ported from `node_modules/esrap@2.2.4/src/{index,context}.js` and
//! `node_modules/esrap@2.2.4/src/languages/ts/index.js`.
//!
//! Output is byte-equivalent to upstream esrap, as gated by
//! `packages/svelte/tests/snapshot/samples/*/_expected/{client,server}/*.svelte.js`.
//!
//! Input AST is `serde_json::Value` in the same acorn-shaped wire format that
//! `svelte_parse::oxc_bridge` produces and that the transform crates consume.

#![forbid(unsafe_code)]

pub mod comments;
pub mod context;
pub mod print;
pub mod visitors;

pub use context::{Command, CommentState, Context, PrintOptions};
pub use print::{print, PrintResult};
pub use visitors::default_visitors;
