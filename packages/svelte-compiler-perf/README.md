# svelte-compiler-perf

Isolated sandbox for **compile-time performance experiments** backported from
[`rs/svelte`](../../rs/svelte/). The production compiler lives in
[`packages/svelte/src/compiler`](../svelte/src/compiler); proven wins are
cherry-picked there as small PRs.

## Setup

```sh
pnpm install   # from repo root
```

## Benchmarks

```sh
cd packages/svelte-compiler-perf

# End-to-end compile (sandbox vs upstream svelte/compiler)
pnpm bench -- --iter 1000 --mode client

# Per-phase breakdown (parse / analyze / transform / codegen / e2e)
pnpm bench:phases -- --fixture skip-static-subtree --iter 2000

# CPU profile (writes isolate-*.cpuprofile in cwd)
node --cpu-prof bench/bench-phases.mjs --fixture skip-static-subtree --phase e2e --iter 5000

# Refresh BENCHMARK_BASELINE.md
pnpm bench:baseline
```

## Sync from upstream compiler

```sh
rsync -a --delete ../svelte/src/compiler/ src/
# Re-apply sandbox-only perf modules under src/phases/2-analyze/
```

## Upstream PR checklist

1. Minimal diff against `packages/svelte/src/compiler`
2. Snapshot byte-equal on default fixture set (`pnpm bench -- --check`)
3. Phase bench delta recorded in `BENCHMARK_BASELINE.md`
4. `pnpm test` (compiler snapshot suites)
