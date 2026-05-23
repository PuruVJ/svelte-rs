#!/usr/bin/env bash
# Run bench_in_proc on every snapshot fixture and print ranked phase times.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BENCH="$ROOT/rs/svelte/target/release/bench_in_proc"
FIXTURES="$ROOT/packages/svelte/tests/snapshot/samples"
ITER="${1:-2000}"

if [[ ! -x "$BENCH" ]]; then
  echo "building bench_in_proc..." >&2
  (cd "$ROOT/rs/svelte" && cargo build --release -p svelte_compiler --bin bench_in_proc)
fi

TMP=$(mktemp)
for dir in "$FIXTURES"/*/; do
  name=$(basename "$dir")
  f="$dir/index.svelte"
  [[ -f "$f" ]] || continue
  out=$("$BENCH" "$f" "$ITER" 2>/dev/null) || continue
  parse=$(echo "$out" | awk '/parse:/ {print $2}')
  analyze=$(echo "$out" | awk '/analyze:/ {print $2}')
  transform=$(echo "$out" | awk '/transform:/ {print $2}')
  codegen=$(echo "$out" | awk '/codegen:/ {print $2}')
  sum=$(echo "$out" | awk '/sum:/ {print $2}')
  echo -e "${name}\t${parse}\t${analyze}\t${transform}\t${codegen}\t${sum}"
done > "$TMP"

echo "=== bench_in_proc sweep (iter=$ITER, ms/iter) ==="
printf "%-42s %10s %10s %10s %10s %10s\n" "fixture" "parse" "analyze" "transform" "codegen" "sum"
sort -t$'\t' -k6 -nr "$TMP" | while IFS=$'\t' read -r n p a t c s; do
  printf "%-42s %10s %10s %10s %10s %10s\n" "$n" "$p" "$a" "$t" "$c" "$s"
done

echo ""
echo "=== slowest transform (top 10) ==="
sort -t$'\t' -k4 -nr "$TMP" | head -10 | while IFS=$'\t' read -r n p a t c s; do
  printf "%-42s transform=%s sum=%s\n" "$n" "$t" "$s"
done

echo ""
echo "=== slowest parse (top 10) ==="
sort -t$'\t' -k2 -nr "$TMP" | head -10 | while IFS=$'\t' read -r n p a t c s; do
  printf "%-42s parse=%s sum=%s\n" "$n" "$p" "$s"
done

echo ""
echo "=== slowest analyze (top 10) ==="
sort -t$'\t' -k3 -nr "$TMP" | head -10 | while IFS=$'\t' read -r n p a t c s; do
  printf "%-42s analyze=%s sum=%s\n" "$n" "$p" "$s"
done

rm -f "$TMP"
