#!/usr/bin/env bash
# Quick regression check for perf loop (ms/iter, lower is better).
set -euo pipefail
BIN="/workspace/rs/svelte/target/release/bench_phases"
FIXTURES=(skip-static-subtree async-in-derived each-string-template hello-world)
ITER="${1:-5000}"
for fx in "${FIXTURES[@]}"; do
  for mode in server client; do
    out=$("$BIN" "$fx" "$mode" "$ITER" 2>/dev/null) || { echo "$fx $mode FAIL"; continue; }
    sum=$(echo "$out" | awk '/sum/ {print $2}')
    transform=$(echo "$out" | awk '/transform/ {print $2}')
    echo "$fx $mode sum=$sum transform=$transform"
  done
done
