# Svelte Coding Agent Guide

This guide is for AI coding agents working in the Svelte monorepo.

**Important:** Read and follow [`CONTRIBUTING.md`](./CONTRIBUTING.md) as well - it contains essential information about testing, code structure, and contribution guidelines that applies here.

## Quick Reference

If asked to do a performance investigation, use the `performance-investigation` skill.

## Cursor Cloud specific instructions

The Rust Svelte compiler lives under `rs/svelte/` (Cargo workspace). From repo root:

| Task | Command |
|------|---------|
| Check codegen crate | `cd rs/svelte && cargo check -p svelte_codegen_js` |
| Build CLI | `cd rs/svelte && cargo build --release --bin svelte-rs` |
| Compile a sample | `rs/svelte/target/release/svelte-rs packages/svelte/tests/snapshot/samples/hello-world/index.svelte` |

Codegen hot path: `rs/svelte/crates/svelte_codegen_js/src/typed.rs`. After codegen changes, compare CLI output to a saved baseline to ensure byte-identical JS.
