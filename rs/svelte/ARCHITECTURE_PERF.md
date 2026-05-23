# Compiler architecture (performance)

## Pipeline shapes

| Shape | Detection | Emitter |
|-------|-----------|---------|
| **Fully static** | `try_typed_client` | `$.from_html` + `$.append` |
| **Sparse islands** | `sparse_pipeline::try_sparse_islands_program` (early) | `emit_top_level_multi_if_program` |
| **General** | `try_typed_client_walker` | Full walker |

## Metadata (analyze-once)

`mark_template_metadata(&mut Root)` sets:

- `fragment.metadata.dynamic`
- `element.metadata.dynamic` / `element.metadata.is_static_element`

Client and server transforms read these flags before re-walking subtrees.

## Static HTML cache

After `mark_template_metadata`, client `compile()` runs `precompute_static_html_cache`
so each static element’s outer HTML is serialized once into
`element.metadata.cached_static_html`. Sparse emit reuses it instead of re-walking subtrees.

## Sparse early compile

`try_emit_sparse_islands_client_js` runs in `compile()` before the walker:

1. Fast `analyze_script_props_only` for `$props()`-only scripts
2. `try_sparse_islands_program` + `try_emit_client_program_direct`
3. Returns JS without `print_typed` or walker PRE-DETECT

Measure real pipeline: `bench_phases FIXTURE e2e 5000`.

## Direct codegen (skip `print_typed`)

`compile()` tries, in order:

1. **`try_emit_fully_static_client_js`** — static-only templates; no `Program` allocation.
2. Transform → **`try_emit_client_program_direct`** — sparse / slab `Program` shapes.
3. Fallback: `print_typed`.

Scratch buffers use **`CompileBump`** (`svelte_transform_shared::compile_bump`) per compile call.

## Entry order (client walker)

1. `analyze_script`
2. `mark_template_metadata` + `fold_fragment_with_consts`
3. **Sparse islands early return** (skips PRE-DETECT funnel)
4. Legacy PRE-DETECT + general walker

Reproduce: `cd rs/svelte && ./scripts/bench_loop.sh 5000`
