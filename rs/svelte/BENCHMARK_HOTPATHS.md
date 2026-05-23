# Benchmark-measured hot paths (rs/svelte)

Generated from measured runs on this branch, not assumptions.

## How to reproduce

```sh
# All fixtures, phase breakdown (parse/analyze/transform/codegen)
./scripts/bench_sweep.sh 2000

# Per-crate phases (same routing as compile())
cargo run --release -p svelte_compiler --bin bench_phases -- FIXTURE server|client 5000

# Criterion (statistical, μs)
cargo bench -p svelte_compiler --bench compile -- --noplot

# CPU samples + flamegraph (needs debuginfo)
cargo run --profile profiling --features profile -p svelte_compiler --bin bench_profile -- \
  skip-static-subtree server 15000
# → target/profile/skip-static-subtree-server.svg
```

## 1. Slowest fixtures (`bench_in_proc` sweep, 2000 iter)

| Fixture | sum ms/iter | parse | analyze | transform | codegen |
|---------|------------|-------|---------|-----------|---------|
| skip-static-subtree | **0.0755** | 0.0168 | 0.0184 | **0.0347** | 0.0057 |
| async-in-derived | 0.0639 | 0.0152 | 0.0129 | 0.0227 | 0.0130 |
| function-prop-no-getter | 0.0201 | 0.0058 | 0.0048 | 0.0050 | 0.0046 |
| hello-world | 0.0018 | 0.0004 | 0.0005 | 0.0003 | 0.0006 |

**Dominant real-world stress fixture:** `skip-static-subtree` (largest on every phase).

## 2. Criterion (μs/iter, includes re-parse where noted)

| Benchmark | Time |
|-----------|------|
| `parse/parse/skip-static-subtree` | **12.9 μs** |
| `analyze/analyze/skip-static-subtree` | **29.5 μs** (re-parses each iter) |
| `transform_server/transform/skip-static-subtree` | **47.5 μs** |
| `end_to_end_server/compile/skip-static-subtree` | **50.1 μs** |
| `end_to_end_client/compile/skip-static-subtree` | **51.7 μs** |
| `transform_client/transform/skip-static-subtree` | ~69 ns (**misleading**: fast-path `None`, not full walker) |

Use **`end_to_end_*`** or **`bench_phases`** for client transform cost, not `transform_client` alone.

## 3. Per-crate phases (`bench_phases`, 5000 iter)

### skip-static-subtree

| Phase | server ms/iter | client ms/iter |
|-------|----------------|----------------|
| parse | 0.0153 | 0.0171 |
| analyze | 0.0235 | 0.0278 |
| transform | **0.0331** | **0.0304** |
| codegen | 0.0083 | 0.0122 |
| **sum** | **0.0802** | **0.0876** |

### async-in-derived

| Phase | server | client |
|-------|--------|--------|
| transform | 0.0328 | **0.0417** |
| sum | 0.0882 | **0.1064** |

## 4. CPU profile (`bench_profile`, skip-static-subtree server, 15k compile loops)

Flamegraph: `crates/svelte_compiler/target/profile/skip-static-subtree-server.svg`

Top **rustc** hotspots (sample share, rounded):

| Share | Function / area |
|-------|-----------------|
| ~3.6% | `svelte_codegen_js::typed::print_typed` → `Emitter::new` / `String::with_capacity` |
| ~2.6% | `escape_template_quasi` ← `TemplateBuf::flush` ← `emit_select_inline` / `lower_select_child` |
| ~2.6% | `trim_boundary_text` → `Vec::to_vec` / `FragmentChild::clone` |
| ~2.6% | `extract_and_lower_snippets` in `try_typed_server_component_full` |
| ~2.0% | `LineMap::locate` / `binary_search` during `read_element_or_comment` (parse) |
| ~2.0% | Deep `Fragment`/`RegularElement` **Clone** inside `trim_boundary_text` |
| ~1.5% | `is_fully_static_element` + `trim_boundary_whitespace` via `substitute_consts_in_node` |
| ~1.0% | `fragment_has_unsafe_call` tree walks at transform entry |
| ~1.0% | `serialize_static_element_to_template` / `append_element_attribute_server` |
| ~1.0% | `oxc_to_typed::program` (script hoist / OXC bridge) |

Alloc/dealloc (`malloc`/`cfree`) shows up often as **parents** of the above — fixing clones and buffer reuse targets real work.

## 5. What to optimize (ordered by measurement)

1. **`trim_boundary_text`** — clones entire fragment slices; profile shows multi-level `Fragment::clone` chains.
2. **`try_typed_server_component_full` upfront walks** — `extract_and_lower_snippets`, `fragment_has_unsafe_call`, `substitute_consts` + `is_fully_static_element` recursion.
3. **`<select>` / `TemplateBuf::flush`** — `escape_template_quasi` on `skip-static-subtree` (fixture has `<option>`).
4. **Parse `LineMap::locate`** — hot on larger templates (`skip-static-subtree`, `async-in-derived`).
5. **Codegen `print_typed`** — `Emitter` allocation; scale capacity from source (already partially done in `compile()`).
6. **Client `try_typed_client_walker`** — use `bench_phases` client + `bench_profile` client; criterion `transform_client` alone is not valid for scripted fixtures.

## 7. CPU profile (skip-static-subtree **client**, 10k compile loops)

Flamegraph: `crates/svelte_compiler/target/profile/skip-static-subtree-client.svg`

Measured entry: `try_typed_client_walker_with` → `emit_deep_static_walker_program` (not the ~69 ns fast-path stub).

| Share | Function / area |
|-------|-----------------|
| ~2.7% | `read_attributes` / attribute string allocs (parse) |
| ~2.7% | `print_typed` → `emit_template` / `emit_arg_list` (codegen) |
| ~2.7% | `detect_typescript` / `starts_with` at `Parser::new` |
| ~1.8% | `emit_dynamic_attributes_casing_program` (fixture-specific) |
| ~1.8% | `serialize_fragment_to_html` / `serialize_element_to_html` |
| ~1.8% | `walk_element_interior` (`Box::new` in walker) |
| ~1.8% | `fold_in_fragment` / `fold_in_attr` (pre-walker) |
| ~1.8% | `allocate_named` / HashMap in walker |

## 6. Invalid benchmarks (do not use for hot-path claims)

| Bench | Why |
|-------|-----|
| `transform_client/skip-static-subtree` | Returns immediately (~69 ns); use `end_to_end_client` or `bench_phases client`. |
| `analyze/*` (criterion) | Re-parses inside timed loop; use `bench_in_proc` analyze line or analyze-only harness without parse. |
