# Svelte Coding Agent Guide

This guide is for AI coding agents working in the Svelte monorepo.

**Important:** Read and follow [`CONTRIBUTING.md`](./CONTRIBUTING.md) as well - it contains essential information about testing, code structure, and contribution guidelines that applies here.

## Quick Reference

If asked to do a performance investigation, use the `performance-investigation` skill.

## Cursor Cloud specific instructions

The Rust Svelte compiler lives in `rs/svelte/` (not the repo root). Use that directory for all `cargo` commands.

- **Check:** `cd rs/svelte && cargo check --workspace`
- **Tests (compiler crates):** `cargo test -p svelte_parse -p svelte_compiler -p svelte_magic_string -p svelte_codegen_js -p svelte_transform_shared -p svelte_transform_server -p svelte_transform_client`
- **Release CLI:** `cargo build --release -p svelte_compiler --bin svelte-rs`
- **Snapshot fixtures:** `/workspace/packages/svelte/tests/snapshot/samples/<name>/index.svelte` (not relative `../../../../packages/...` from `rs/svelte`).

The typed JS AST (`svelte_js_ast`) uses `Cow<'static, str>` for `Identifier.name` and `StringLiteral.value`. Static identifiers go through `t::id("literal")` / `Cow::Borrowed`; dynamic names use `t::id_owned(...)` / `Cow::Owned`. When reading names in hash maps, prefer `.as_ref()` over `.as_str()` (unstable on `Cow` in this toolchain).

## Cursor Cloud specific instructions

The Rust Svelte compiler lives in [`rs/svelte/`](./rs/svelte/README.md). Use the pinned toolchain from [`rs/svelte/rust-toolchain.toml`](./rs/svelte/rust-toolchain.toml).

### Rust compiler (rs/svelte)

| Task | Command (from `rs/svelte/`) |
|------|-----------------------------|
| Check workspace | `cargo check --workspace` |
| Release binaries | `cargo build --release -p svelte_compiler --bin svelte-rs --bin bench_in_proc` |
| Compile a fixture (client) | `./target/release/svelte-rs /path/to/index.svelte` |
| Compile a fixture (SSR) | `./target/release/svelte-rs /path/to/index.svelte --ssr` |
| Per-phase profiler | `./target/release/bench_in_proc /path/to/index.svelte 20000` |

Server transform entry points (`try_typed_server_component*`) take `&mut Root` and mutate `root.fragment` in place during lowering (similar to the client transform's `walker_fold_in_fragment`). Benchmarks that reuse a parsed `Root` across iterations must re-parse or use a fresh `Root` each iteration.

Analyze tests: `cargo test -p svelte_analyze` (some scope/validator tests may fail independently of compiler output).

JS monorepo setup (if needed for snapshot fixtures under `packages/svelte/tests/`): `pnpm install` from the repo root.
