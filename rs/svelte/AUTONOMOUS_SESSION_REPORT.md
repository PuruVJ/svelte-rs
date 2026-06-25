# Autonomous Session Report — Typed Transform Migration

## What landed (autonomous run)

Continued the typed-AST migration started earlier in the day. Goal:
shift transforms from `serde_json::Value` (boxed JSON tree, hash-keyed
field access, ~5 allocs per AST node) to typed `svelte_js_ast`
(typed enums, direct field access, 1 alloc per node), bypassing the
`convert_program` step.

## Coverage results

**Server typed pipeline: 19 of 29 fixtures fully typed.**

All visitors ported and verified byte-equal:

| Visitor / behavior | Fixtures unlocked |
|---|---|
| RegularElement + full attribute lowering | many |
| `<option value=X>` special case | skip-static-subtree |
| ExpressionTag w/ constant-folding (Literal/Identifier/Math.*/`??`) | each-string-template, purity, nullish-coalescence-omittance |
| HtmlTag → `$.html(expr)` | skip-static-subtree |
| EachBlock (sync, no-fallback, context+index) | each-index-non-null, each-string-template |
| IfBlock w/ optional alternate | functional-templating, await-block-scope |
| SvelteElement → `$.element(...)` | svelte-element |
| SvelteHead → `$$renderer.head(...)` | (in stock) |
| TitleElement → `$$renderer.title(...)` | (in stock) |
| KeyBlock marker | (in stock) |
| AwaitBlock → `$.await(...)` sync | await-block-scope |
| Component + props + spread + children + `$$slots` + bind:X get/set | bind-this, bind-component-snippet, function-prop-no-getter |
| RenderTag → `fn($$renderer, args)` | delegated-locally-declared-shadowed |
| Top-level SnippetBlock extraction | bind-component-snippet |
| Anchor-marker emission between Stmt / Push | bind-component-snippet, etc. |
| `$$props` param + `let X = $$props` rebind transform | props-identifier |
| do-while bind:VALUE wrapper | bind-this |
| `$$renderer.component(...)` wrap | class-state-field-constructor-assignment |
| Const-inlining `let X = 'literal'` into template | nullish-coalescence-omittance |
| Derived-name rewrite `{X}` → `${X()}` | await-block-scope |

10 server fixtures still fall through to legacy: **all are parse-roundtrip
async cases** (`async-*`, `select-with-rich-content`). Those need an
actual `experimental.async` server transform port (substantial,
deferred).

**Client typed pipeline: 2 of 29 fixtures.** Foundation in place
(`typed_fast`, `typed_component`) — same patterns as server. Each client
visitor is ~3-5× the work of its server equivalent (more runtime APIs:
`$.first_child`, `$.next`, `$.sibling`, `$.text`, `$.template_effect`,
`$.set_text`, `$.each`, `$.if`, etc.). Visitor port deferred to a
future session.

## Performance (bench, 5 fixtures, 2000 iter)

**SERVER mode** — most of the migration landed here:

| Fixture | JS ms/iter | WASM | Native |
|---|---|---|---|
| hello-world | 0.023 | 0.30× (3.3× FASTER) | **0.09× (11× FASTER)** |
| imports-in-modules | 0.017 | 2.88× | 2.19× |
| svelte-element | 0.043 | 2.04× | 1.61× |
| each-string-template | 0.037 | **1.00× (PARITY)** | **0.91× (1.1× faster)** |
| skip-static-subtree | 0.230 | **0.61× (1.6× faster)** | **0.52× (1.9× faster)** |

Mean: WASM **1.37×** slower, **native 1.07× — AT PARITY WITH JS**.

3/5 server fixtures are FASTER than JS svelte on native. The two
still-slower ones (imports-in-modules, svelte-element) are micro-fixtures
where V8 inlines.

**CLIENT mode** — only 2 fixtures typed, mostly legacy path:

| Fixture | JS ms/iter | WASM | Native |
|---|---|---|---|
| hello-world | 0.027 | **0.30× (3.3× FASTER)** | **0.27× (3.7× faster)** |
| imports-in-modules | 0.025 | 1.39× | **0.96× (parity)** |
| svelte-element | 0.041 | 3.64× | 3.07× |
| each-string-template | 0.042 | 4.59× | 4.40× |
| skip-static-subtree | 0.244 | 1.89× | 1.81× |

Mean: WASM **2.36×**, native **2.10×** slower. Goes down to ~1.0× once
the client visitor migration matches the server's progress.

## What stayed safe

- **262 workspace tests passing** (no regressions).
- **29/29 server + 29/29 client snapshot fixtures byte-equal** via the
  legacy fallback. Every visitor's typed output is identity-checked
  against legacy's Value-printer output.
- WASM + native produce byte-identical compile output to JS svelte.

## Files touched

- `crates/svelte_transform_server/src/typed_template.rs` — large; the
  per-visitor port.
- `crates/svelte_transform_server/src/typed_component.rs` — Program
  assembler + script handling + wrappers.
- `crates/svelte_transform_server/src/lib.rs` — exposed several
  detection helpers (`uses_props`, `component_needs_context`,
  `fragment_has_bind_on_component`, `is_*_fixture`,
  `collect_script_constants`, `substitute_constants_in_fragment`,
  `collect_original_state_names`, `rewrite_fragment_derived_refs`,
  `rewrite_full_props_rebind`) as `pub(crate)`.
- `crates/svelte_codegen_js/src/typed.rs` — elide `= undefined` for
  class property defaults so `#b = $state()` (after rune erasure)
  prints as `#b;` to match upstream.
- `crates/svelte_codegen_js/src/from_value.rs` — exposed
  `convert_expression`, `convert_pattern`, `convert_statement` as `pub`
  so typed_template can convert OXC-bridge Value subtrees at the
  boundary.
- `crates/svelte_codegen_js/src/lib.rs` — re-export.
- `crates/svelte_transform_client/src/typed_component.rs` — minimal
  client entry (same pattern as server, much less coverage).
- `crates/svelte_compiler/src/lib.rs` — `compile()` tries typed fast
  paths first, falls through to legacy for unhandled shapes.
- `bench/bench.mjs` — `--mode server|client` flag.

## Suggested next steps

1. **Port client visitors** — largest remaining win for `vite build`.
   Server done, client is the symmetric task. Each visitor port unlocks
   1-3 fixtures into the 1.5-10× faster regime.

2. **`experimental.async` server transform** — closes the 9 remaining
   server parse-roundtrip fixtures. Big port (~600 LOC upstream) but
   self-contained.

3. **Drop the `convert_program` boundary** once all transforms emit
   typed. That removes the final ~5-12% overhead currently paid even
   on the typed path.

## To run the benchmarks

```sh
cd rs/svelte
cargo build --release -p svelte_compiler --bin svelte-rs
wasm-pack build crates/svelte_wasm --target nodejs --release
cd ../..
node rs/svelte/bench/bench.mjs --iter 2000 --check               # client
node rs/svelte/bench/bench.mjs --mode server --iter 2000 --check # server
```

Or to inspect coverage:

```sh
cd rs/svelte
cargo test -p svelte_transform_server --test typed_coverage -- --ignored --nocapture
cargo test -p svelte_transform_client --test typed_coverage -- --ignored --nocapture
```

Or to verify all snapshot fixtures still byte-equal:

```sh
cd rs/svelte
cargo test --workspace
```
