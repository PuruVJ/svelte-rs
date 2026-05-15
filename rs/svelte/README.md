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

**Phase 1** — in progress.
- `svelte_diagnostics` ✅ 270 diagnostic functions (187 errors + 83 warnings)
  generated at build time from `packages/svelte/messages/**/*.md` (the same
  source the upstream `scripts/process-messages/index.js` consumes). 13-case
  differential test asserts byte-equal message strings vs. the JS `errors.js`.
- `svelte_ast` 🟡 core node shapes only (`Root`, `Fragment`, `Text`,
  `Comment`, `Script`, `JsComment`). Serde wire format matches what
  `parse(source, { modern: true })` emits after `to_public_ast` cleans
  internal metadata. Remaining work: ~40 more node variants
  (tags, elements, blocks, directives, attributes, CSS subtree, ESTree subtree).

Run `cargo test --workspace` to see all green.

