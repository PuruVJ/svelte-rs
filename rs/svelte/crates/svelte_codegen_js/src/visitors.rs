//! Per-node-type visitors. Ported from
//! `node_modules/esrap@2.2.4/src/languages/ts/index.js`.
//!
//! Each visitor consumes an acorn-shaped JSON node and emits commands into the
//! supplied [`Context`]. The expected node shapes mirror upstream's TSESTree
//! types as produced by `acorn-typescript`. Field-by-field parity with esrap
//! is required — the snapshot fixtures check byte equality.

mod helpers;
mod literals;
mod identifiers;
mod expressions;
mod statements;
mod declarations;
mod patterns;
mod classes;
mod modules;
mod jsx;
mod typescript;
mod programs;

pub use programs::default_visitors;
