# svelte-rs

Rust + WebAssembly port of the Svelte compiler. Lives alongside `packages/svelte/`,
which remains the production JS compiler and the source of truth for behavior.

## Ground rule

The JS sources under `packages/svelte/src/compiler/` are the **only** source of truth.
Behavior, error messages, AST shapes, and sourcemap output are derived from those
files, not from prior knowledge or external docs. The `probe/` directory holds a
Node-side bridge that runs the JS compiler against fixture inputs — the
`svelte_test_harness` binary diffs Rust output against this golden master.

## Layout

```
rs/svelte/
  Cargo.toml             workspace manifest
  rust-toolchain.toml    pinned stable + wasm32 target
  crates/                one crate per pipeline stage / ported npm dep
  probe/run.mjs          Node bridge that runs the JS compiler for diffing
```

## Working with the workspace

```sh
# Build everything (cache deps on first run)
cd rs/svelte && cargo check --workspace

# Run a fixture through the JS compiler via the probe (sanity check plumbing)
cargo run -p svelte_test_harness -- parse packages/svelte/tests/parser-modern/samples/comment-before-script

# Per-crate tests
cargo test -p svelte_ast
```

## Status

**Phase 0** — done. Workspace scaffolded (18 crates), all stubs compile, the
`svelte_test_harness` binary shells out to `probe/run.mjs` which runs the
upstream JS compiler against any fixture under `packages/svelte/tests/`.

**Phase 1** — done.
- `svelte_diagnostics` — 270 diagnostic functions (187 errors + 83 warnings)
  generated at build time from `packages/svelte/messages/**/*.md` (the same
  source the upstream `scripts/process-messages/index.js` consumes). A 13-case
  differential test asserts byte-equal message strings vs. the JS `errors.js`.
- `svelte_ast` — full template AST: Root, Fragment, Text, Comment, Script,
  JsComment, all 6 tag kinds, all 14 element kinds (Component, RegularElement,
  SlotElement, TitleElement, plus svelte:body/boundary/component/document/
  element/fragment/head/options/self/window), all 5 block kinds, all 8
  directive kinds, Attribute, SpreadAttribute. **All 24 parser-modern
  fixtures roundtrip cleanly** through serde — Rust deserializes the live
  JS parser output and re-serializes byte-equivalent JSON.
- `svelte_compiler` facade — `CompileOptions`, `ModuleCompileOptions`,
  `ParseOptions`, including `experimental.async` (Svelte 5.36+). JSON
  shape matches the upstream camelCase contract.
- `svelte_test_harness` — runs both Rust and JS in one shot and prints
  a unified diff. `cargo run -p svelte_test_harness -- all-parser-modern`
  exercises the full parser-modern suite.

**Phase 2** — in progress.
- 2a/2b: `svelte_parse` scaffold + utility helpers (`is_whitespace`, BOM
  strip, cursor advance, `LineMap` for line/column), text reader, HTML
  comment reader.
- 2c: Element parsing — `RegularElement`, attributes (bare, quoted-string,
  unquoted), nested fragments, void elements, self-closing tags. `name_loc`
  with line/column/character via `LineMap`. `{...}` blocks inside opening
  tags are skipped (placeholder until 2d's real mustache parsing).
- 2d-2g pending: mustache tag parsing (`{expr}`, `{#if}`, etc.) + OXC
  integration for JS expressions + `<script>` + `<style>`.

Run `cargo test --workspace` to see all green (45 tests).
Run `cargo run -q -p svelte_test_harness -- all-parser-modern` to see the
parser-modern suite status — currently 0 match / 18 diverge / 6 error
(every fixture exercises a feature still pending; diff plumbing is fully
working, so once 2d-2g land, fixtures will go green in waves).

