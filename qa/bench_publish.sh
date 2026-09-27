#!/usr/bin/env bash
# Publish latency: how long `hard publish` takes, end to end, cold and warm.
#
# Measured around the real client: manifest read, deterministic .hspkg build,
# signing, upload, and the registry's write. `--dry-run` is timed separately
# because it stops before the upload and is what a pre-commit hook would run.
set -euo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=qa/lib.sh
. "$QA_ROOT/lib.sh"

PORT="${BENCH_PORT:-19110}"
ITERATIONS="${BENCH_PUBLISH_ITERATIONS:-40}"
LINES="${BENCH_PUBLISH_LINES:-400}"
URL="http://127.0.0.1:$PORT"

ensure_release_binaries
bench_record_machine
bench_require_free_port "$PORT"

RAW="$(raw_file publish)"
trap bench_cleanup EXIT

BENCH_WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-bench-publish.XXXXXX")"
export HARD_HOME="$BENCH_WORK/home"
export HARD_REGISTRY="$URL"
mkdir -p "$HARD_HOME"

bench_registry_start "$PORT" "$BENCH_WORK/data"

echo "# publish benchmark"
echo "# $ITERATIONS publishes, $LINES source lines each, release build"

# ---- cold publish: a package that has never been published -----------------
COLD="$BENCH_WORK/cold.txt"
for i in $(seq 1 "$ITERATIONS"); do
  dir="$BENCH_WORK/src/bench-cold-$i"
  bench_package "$dir" "bench-cold-$i" "1.0.0" "$LINES"
  start=$(now_ns)
  bench_publish "$dir" "$URL"
  echo "$(elapsed_us "$start")" >> "$COLD"
done

# ---- warm publish: new versions of a package the registry already knows ----
WARM="$BENCH_WORK/warm.txt"
for i in $(seq 1 "$ITERATIONS"); do
  dir="$BENCH_WORK/src/bench-warm"
  bench_package "$dir" "bench-warm" "1.$i.0" "$LINES"
  start=$(now_ns)
  bench_publish "$dir" "$URL"
  echo "$(elapsed_us "$start")" >> "$WARM"
done

# ---- dry run: validate and describe, upload nothing ------------------------
DRY="$BENCH_WORK/dry.txt"
for i in $(seq 1 "$ITERATIONS"); do
  dir="$BENCH_WORK/src/bench-dry"
  bench_package "$dir" "bench-dry" "2.0.$i" "$LINES"
  start=$(now_ns)
  ( cd "$dir" && HARD_REGISTRY="$URL" "$HARD_BIN" publish --dry-run >/dev/null 2>&1 )
  echo "$(elapsed_us "$start")" >> "$DRY"
done

# ---- a dependency graph, so the manifest work is not trivial ---------------
DEPS="$BENCH_WORK/graph.txt"
for i in $(seq 1 "$ITERATIONS"); do
  dir="$BENCH_WORK/src/bench-graph"
  bench_package "$dir" "bench-graph" "1.$i.0" "$LINES"
  {
    printf '\n[dependencies]\n'
    printf 'bench-warm = "1"\n'
    printf 'bench-cold-1 = "1"\n'
  } >> "$dir/hard.toml"
  start=$(now_ns)
  bench_publish "$dir" "$URL"
  echo "$(elapsed_us "$start")" >> "$DEPS"
done

# ---- payload size ----------------------------------------------------------
SIZE="$BENCH_WORK/size.txt"
bench_package "$BENCH_WORK/src/bench-size" "bench-size" "1.0.0" "$LINES"
( cd "$BENCH_WORK/src/bench-size" && HARD_REGISTRY="$URL" "$HARD_BIN" publish --dry-run 2>/dev/null \
  | awk '/^size/ {print $2; exit}' ) > "$SIZE" || true

# ---- server-side cost, measured without the client ------------------------
#
# `hard publish` is process start + manifest + build + sign + upload + commit.
# The pure registry costs are timed with curl, so the report can say which side
# the time is on instead of guessing. `--dry-run` already gives the client's
# build-and-sign half.
SERVER="$BENCH_WORK/server.txt"
bench_package "$BENCH_WORK/src/bench-server" "bench-server" "1.0.0" "$LINES"
bench_publish "$BENCH_WORK/src/bench-server" "$URL"
for i in $(seq 1 "$ITERATIONS"); do
  start=$(now_ns)
  curl -sS -o /dev/null "$URL/packages/bench-server"
  curl -sS -o /dev/null "$URL/packages/bench-server/1.0.0/download"
  echo "$(elapsed_us "$start")" >> "$SERVER"
done

# ---- the registry's own view ----------------------------------------------
STATS=$(curl -sS "$URL/stats" || echo '{}')

record_stats "$RAW" publish_cold "$COLD"
record_stats "$RAW" publish_warm "$WARM"
record_stats "$RAW" publish_dryrun "$DRY"
record_stats "$RAW" publish_graph "$DEPS"
record_stats "$RAW" server_roundtrip "$SERVER"
record "$RAW" "publish_archive_bytes" "$(cat "$SIZE" | tr -dc '0-9')" bytes
record "$RAW" "publish_source_lines" "$LINES" lines
record "$RAW" "server_roundtrip_p50" "$(stats "$SERVER" | awk '{print $3}')" us
record "$RAW" "server_roundtrip_mean" "$(stats "$SERVER" | awk '{print $2}')" us
record "$RAW" "server_roundtrip_p95" "$(stats "$SERVER" | awk '{print $4}')" us
record "$RAW" "registry_packages" "$(printf '%s' "$STATS" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("packages", 0))
except Exception: print(0)')" count
record "$RAW" "registry_versions" "$(printf '%s' "$STATS" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("versions", 0))
except Exception: print(0)')" count
record "$RAW" "registry_downloads" "$(printf '%s' "$STATS" | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("downloads", 0))
except Exception: print(0)')" count

echo
echo "publish cold  $(stats "$COLD")"
echo "publish warm  $(stats "$WARM")"
echo "publish dry   $(stats "$DRY")"
echo "publish graph $(stats "$DEPS")"
echo "server 2x GET $(stats "$SERVER")"
echo "raw -> $RAW"
