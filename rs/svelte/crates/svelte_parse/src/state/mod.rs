//! State-machine handlers — one module per top-level state in the upstream
//! parser (`packages/svelte/src/compiler/phases/1-parse/state/`).

pub mod comment;
pub mod element;
pub mod text;
