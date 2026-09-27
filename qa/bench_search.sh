#!/usr/bin/env bash
# Search latency, by query shape and by index size.
#
# A search benchmark that only measures one query measures one cache line. The
# corpus is published in three sizes and each size is asked four ways — exact
# name, prefix, fuzzy (edit distance), and a miss — because those hit different
# parts of the ranking and the cache behaves differently for each.
set -euo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=qa/lib.sh
. "$QA_ROOT/lib.sh"

PORT="${BENCH_PORT:-19130}"
SIZES="${BENCH_SEARCH_SIZES:-50 200 500}"
ITERATIONS="${BENCH_SEARCH_ITERATIONS:-20}"
LINES="${BENCH_SEARCH_LINES:-40}"
URL="http://127.0.0.1:$PORT"

ensure_release_binaries
bench_record_machine
bench_require_free_port "$PORT"

RAW="$(raw_file search)"
trap bench_cleanup EXIT

BENCH_WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-bench-search.XXXXXX")"
export HARD_REGISTRY="$URL"
bench_registry_start "$PORT" "$BENCH_WORK/data"

# A home whose registry comes from the environment, since search does not need a
# project. Trust is pinned so verification never adds noise to a search.
export HARD_HOME="$BENCH_WORK/home"
mkdir -p "$HARD_HOME"
"$HARD_BIN" keys add "$URL" --trust --allow-test-key >/dev/null 2>&1 || true

echo "# search benchmark"
echo "# index sizes: $SIZES"

MAX=0
for size in $SIZES; do [ "$size" -gt "$MAX" ] && MAX="$size"; done

# ---- publish the largest corpus once, and remember where each size ends ----
publish_corpus() {
  local target="$1" i
  for i in $(seq 1 "$target"); do
    [ -f "$BENCH_WORK/published-$i" ] && continue
    dir="$BENCH_WORK/src/pkg-$i"
    bench_package "$dir" "searchable-$i" "1.0.0" "$LINES"
    {
      printf '\n[package]\ntags = ["even"]\n'
    } >> "$dir/hard.toml"
    if [ $((i % 2)) -eq 0 ]; then
      printf 'tags = ["even", "web"]\n' >> "$dir/hard.toml"
    else
      printf 'tags = ["even", "cli"]\n' >> "$dir/hard.toml"
    fi
    bench_publish "$dir" "$URL" || return 1
    touch "$BENCH_WORK/published-$i"
  done
}

# measure <label> <query-args...>
measure() {
  local label="$1"
  shift
  local file="$BENCH_WORK/search-$label.txt"
  local i start
  : > "$file"
  for i in $(seq 1 "$ITERATIONS"); do
    start=$(now_ns)
    ( cd "$BENCH_WORK" && "$HARD_BIN" search "$@" >/dev/null 2>&1 ) || true
    echo "$(elapsed_us "$start")" >> "$file"
  done
  printf '  %-22s p50=%sus p95=%us\n' "$label" \
    "$(stats "$file" | awk '{print $3}')" "$(stats "$file" | awk '{print $4}')"
  record_stats "$RAW" "search_${label}" "$file"
}

for size in $SIZES; do
  echo "# index size $size"
  publish_corpus "$size"
  PUBLISHED=$(curl -sS "$URL/stats" | python3 -c 'import json,sys;print(json.load(sys.stdin)["packages"])')
  record "$RAW" "index_size_$size" "$PUBLISHED" count
  measure "exact_${size}"      "searchable-$((size / 2))"
  measure "prefix_${size}"     "searchable-$((size / 10))"
  measure "fuzzy_${size}"      "searchble-$((size / 2))"
  measure "tag_${size}"        --tag web
  measure "miss_${size}"       "zzz-no-such-package-xyz"
  measure "json_${size}"       "searchable-$((size / 2))" --json
done

# ---- the server alone, for comparison --------------------------------------
DIRECT="$BENCH_WORK/direct.txt"
BIGGEST=$MAX
for i in $(seq 1 "$ITERATIONS"); do
  start=$(now_ns)
  curl -sS -o /dev/null "$URL/search?q=searchable-$((BIGGEST / 2))&limit=20"
  echo "$(elapsed_us "$start")" >> "$DIRECT"
done
record_stats "$RAW" search_direct "$DIRECT"
record "$RAW" "search_max_index" "$MAX" count
printf '  %-22s p50=%sus p95=%sus\n' "direct_http" \
  "$(stats "$DIRECT" | awk '{print $3}')" "$(stats "$DIRECT" | awk '{print $4}')"

echo
echo "raw -> $RAW"
