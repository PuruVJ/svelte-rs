//! Port of `esrap` — estree → JS string + sourcemap.
//!
//! Output must be byte-equivalent to esrap's output as recorded in
//! `packages/svelte/tests/snapshot/samples/*/_expected/{client,server}/*.svelte.js`.

#![forbid(unsafe_code)]
