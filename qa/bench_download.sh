#!/usr/bin/env bash
# Download and install latency, warm cache versus cold, plus the cache hit rate.
#
# Cold and warm are different operations and are never mixed: a cold install
# pays for HTTP and disk writes, a warm one pays for digest checks. The hit rate
# is the client's own number, parsed out of its output rather than recomputed.
set -euo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=qa/lib.sh
. "$QA_ROOT/lib.sh"

PORT="${BENCH_PORT:-19120}"
PACKAGES="${BENCH_DOWNLOAD_PACKAGES:-25}"
ITERATIONS="${BENCH_DOWNLOAD_ITERATIONS:-12}"
LINES="${BENCH_DOWNLOAD_LINES:-400}"
URL="http://127.0.0.1:$PORT"

ensure_release_binaries
bench_record_machine
bench_require_free_port "$PORT"

RAW="$(raw_file download)"
trap bench_cleanup EXIT

BENCH_WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-bench-download.XXXXXX")"
bench_work_out "$BENCH_WORK/cmd.out"
export HARD_REGISTRY="$URL"
bench_registry_start "$PORT" "$BENCH_WORK/data"

echo "# download benchmark"
echo "# $PACKAGES packages x $LINES source lines"

# ---- publish the corpus once ------------------------------------------------
export HARD_HOME="$BENCH_WORK/seed-home"
mkdir -p "$HARD_HOME"
for i in $(seq 1 "$PACKAGES"); do
  dir="$BENCH_WORK/src/dep-$i"
  bench_package "$dir" "bench-dep-$i" "1.0.0" "$LINES"
  bench_publish "$dir" "$URL"
done

# a second version of every fifth package, so the resolver has a choice to make
for i in $(seq 5 5 "$PACKAGES"); do
  dir="$BENCH_WORK/src/dep-$i-v2"
  bench_package "$dir" "bench-dep-$i" "1.1.0" "$LINES"
  bench_publish "$dir" "$URL"
done

# ---- a project that depends on all of them ---------------------------------
PROJECT="$BENCH_WORK/project"
mkdir -p "$PROJECT"
{
  printf 'schema = 1\nname = "bench-app"\nversion = "0.1.0"\nedition = "2027"\n'
  printf '\n[dependencies]\n'
  for i in $(seq 1 "$PACKAGES"); do
    printf 'bench-dep-%s = "1"\n' "$i"
  done
} > "$PROJECT/hard.toml"
# The manifest above is the fixture; writing it again through bench_package
# would quietly drop every dependency and benchmark an empty project.
printf 'calc f() => Int { <- 1 }\n' > "$PROJECT/main.hard"

# ---- cold: empty cache, every package fetched ------------------------------
COLD="$BENCH_WORK/cold.txt"
COLD_HOME="$BENCH_WORK/cold-home"
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$COLD_HOME" "$PROJECT/.hard" "$PROJECT/hard.lock"
  mkdir -p "$COLD_HOME"
  start=$(now_ns)
  ( cd "$PROJECT" && HARD_HOME="$COLD_HOME" "$HARD_BIN" install >"$WORK_OUT" 2>&1 )
  echo "$(elapsed_us "$start")" >> "$COLD"
  cp "$WORK_OUT" "$BENCH_WORK/cold-last.txt"
done

# ---- warm: the cache already has everything --------------------------------
WARM="$BENCH_WORK/warm.txt"
WARM_HOME="$BENCH_WORK/warm-home"
rm -rf "$WARM_HOME" "$PROJECT/.hard" "$PROJECT/hard.lock"
mkdir -p "$WARM_HOME"
( cd "$PROJECT" && HARD_HOME="$WARM_HOME" "$HARD_BIN" install >/dev/null 2>&1 )
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard" "$PROJECT/hard.lock"
  start=$(now_ns)
  ( cd "$PROJECT" && HARD_HOME="$WARM_HOME" "$HARD_BIN" install >"$WORK_OUT" 2>&1 )
  echo "$(elapsed_us "$start")" >> "$WARM"
  cp "$WORK_OUT" "$BENCH_WORK/warm-last.txt"
done

# ---- warm and frozen: resolution and link only, no network -----------------
FROZEN="$BENCH_WORK/frozen.txt"
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard"
  start=$(now_ns)
  ( cd "$PROJECT" && HARD_HOME="$WARM_HOME" "$HARD_BIN" install --frozen >"$WORK_OUT" 2>&1 )
  echo "$(elapsed_us "$start")" >> "$FROZEN"
done

# ---- offline: the cache is the only registry ------------------------------
OFFLINE="$BENCH_WORK/offline.txt"
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard" "$PROJECT/hard.lock"
  start=$(now_ns)
  ( cd "$PROJECT" && HARD_HOME="$WARM_HOME" "$HARD_BIN" install --offline >"$WORK_OUT" 2>&1 )
  echo "$(elapsed_us "$start")" >> "$OFFLINE"
done

# ---- one archive, over the wire, with no client in the way -----------------
GET="$BENCH_WORK/get.txt"
for i in $(seq 1 "$ITERATIONS"); do
  start=$(now_ns)
  curl -sS -o /dev/null "$URL/packages/bench-dep-1/1.0.0/download"
  echo "$(elapsed_us "$start")" >> "$GET"
done

# ---- what the client says it did -------------------------------------------
COLD_LINE=$(grep -o 'cache: .*' "$BENCH_WORK/cold-last.txt" | tail -1 || true)
WARM_LINE=$(grep -o 'cache: .*' "$BENCH_WORK/warm-last.txt" | tail -1 || true)
COLD_HIT=$(printf '%s' "$COLD_LINE" | grep -o '[0-9]*% hit rate' | grep -o '[0-9]*' || echo "")
WARM_HIT=$(printf '%s' "$WARM_LINE" | grep -o '[0-9]*% hit rate' | grep -o '[0-9]*' || echo "")
COLD_BYTES=$(printf '%s' "$COLD_LINE" | sed -n 's/.*(\([0-9]*\) bytes over the network.*/\1/p')
COLD_REQ=$(printf '%s' "$COLD_LINE" | sed -n 's/.*, \([0-9]*\) requests.*/\1/p')
WARM_BYTES=$(printf '%s' "$WARM_LINE" | sed -n 's/.*(\([0-9]*\) bytes over the network.*/\1/p')
WARM_REQ=$(printf '%s' "$WARM_LINE" | sed -n 's/.*, \([0-9]*\) requests.*/\1/p')
CACHE_SIZE=$(du -sb "$WARM_HOME/cache" 2>/dev/null | awk '{print $1}' || echo 0)

record_stats "$RAW" install_cold "$COLD"
record_stats "$RAW" install_warm "$WARM"
record_stats "$RAW" install_frozen "$FROZEN"
record_stats "$RAW" install_offline "$OFFLINE"
record_stats "$RAW" archive_get "$GET"
record "$RAW" "cache_hit_rate_cold" "${COLD_HIT:-0}" percent
record "$RAW" "cache_hit_rate_warm" "${WARM_HIT:-0}" percent
record "$RAW" "bytes_over_network_cold" "${COLD_BYTES:-0}" bytes
record "$RAW" "bytes_over_network_warm" "${WARM_BYTES:-0}" bytes
record "$RAW" "requests_cold" "${COLD_REQ:-0}" count
record "$RAW" "requests_warm" "${WARM_REQ:-0}" count
record "$RAW" "cache_size_bytes" "$CACHE_SIZE" bytes
record "$RAW" "package_count" "$PACKAGES" count
record "$RAW" "source_lines" "$LINES" lines

echo
echo "install cold    $(stats "$COLD")"
echo "install warm    $(stats "$WARM")"
echo "install frozen  $(stats "$FROZEN")"
echo "install offline $(stats "$OFFLINE")"
echo "archive GET     $(stats "$GET")"
echo "cache: cold=[$COLD_HIT%] warm=[$WARM_HIT%] size=${CACHE_SIZE}B"
echo "raw -> $RAW"
