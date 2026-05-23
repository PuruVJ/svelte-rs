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
