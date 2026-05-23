# Svelte Coding Agent Guide

This guide is for AI coding agents working in the Svelte monorepo.

**Important:** Read and follow [`CONTRIBUTING.md`](./CONTRIBUTING.md) as well - it contains essential information about testing, code structure, and contribution guidelines that applies here.

## Quick Reference

If asked to do a performance investigation, use the `performance-investigation` skill.

## Cursor Cloud specific instructions

This repo is on the `rs` branch, which adds a Rust reimplementation of the Svelte compiler under `rs/svelte/`. The branch is a superset of `main`.

### Project structure

- **JS Svelte package**: `packages/svelte/` — the original JS compiler and runtime (v5.55.7)
- **Rust compiler**: `rs/svelte/` — 20-crate Cargo workspace reimplementing the compiler in Rust
- **Playground**: `playgrounds/sandbox/` — Vite dev sandbox (optional)

### Key commands

| Task | Command | Notes |
|---|---|---|
| Install JS deps | `pnpm install` | Required even for Rust-only work (test harness uses Node) |
| Build JS Svelte | `cd packages/svelte && pnpm build` | Needed before running `pnpm test` or playground |
| JS lint | `pnpm lint` | ESLint + Prettier. 3 pre-existing errors from `rs/*.mjs` files not in tsconfig |
| JS tests | `pnpm test` | Vitest. 32/33 suites pass; `runtime-browser` needs `pnpm playwright install chromium` |
| Rust check | `cd rs/svelte && cargo check --workspace` | Fast type-check |
| Rust build | `cd rs/svelte && cargo build --workspace` | Debug build of all 20 crates |
| Rust release binary | `cd rs/svelte && cargo build --release -p svelte_compiler --bin svelte-rs` | Produces `target/release/svelte-rs` |
| Rust tests | `cd rs/svelte && cargo test -p svelte_parse -p svelte_compiler -p svelte_magic_string` | Some crates (`svelte_analyze`, `svelte_migrate`, `svelte_codegen_js`) have pre-existing test failures |
| Rust clippy | `cd rs/svelte && cargo clippy --workspace` | Passes with warnings only |
| Compile a component | `target/release/svelte-rs <file.svelte> [--ssr]` | Client mode by default, `--ssr` for server |
| Differential tests | `cd rs/svelte && cargo run -p svelte_test_harness -- all-parser-modern` | Compares Rust vs JS parser output |

### Gotchas

- The `rust-toolchain.toml` in `rs/svelte/` pins the stable channel and requests `wasm32-unknown-unknown` target. Running any `cargo` command from that directory auto-installs the correct toolchain.
- esbuild's postinstall script is blocked by pnpm's build script policy. The binary still works — `pnpm build` in `packages/svelte` succeeds without manual intervention.
- The Rust `svelte_codegen_js` crate has a test-only compile error (`missing field type_annotation`) that does not affect the library build. Run tests with `--exclude svelte_codegen_js` to skip it.
- `svelte_analyze` and `svelte_migrate` have a few pre-existing test assertion failures unrelated to environment setup.
- Codegen hot path: `rs/svelte/crates/svelte_codegen_js/src/typed.rs`. After codegen changes, compare CLI output to a baseline to ensure byte-identical JS.
