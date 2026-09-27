#!/usr/bin/env bash
# The registry as a whole: resolution, install, health, and what mirror fallback
# actually costs when the default registry disappears.
#
# This is the umbrella benchmark. It measures the operations the other three
# scripts do not isolate — dependency resolution on a deep graph, health-check
# latency, and the fallback path — and then re-runs the publish, download and
# search benchmarks so one command produces every number in reports/.
set -euo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=qa/lib.sh
. "$QA_ROOT/lib.sh"

PRIMARY_PORT="${BENCH_PORT:-19140}"
MIRROR_PORT="${BENCH_PORT2:-19141}"
PACKAGES="${BENCH_REGISTRY_PACKAGES:-40}"
WIDTH="${BENCH_REGISTRY_WIDTH:-8}"      # dependency-graph width
DEPTH="${BENCH_REGISTRY_DEPTH:-5}"      # how many levels deep
ITERATIONS="${BENCH_REGISTRY_ITERATIONS:-10}"
LINES="${BENCH_REGISTRY_LINES:-40}"
PRIMARY_URL="http://127.0.0.1:$PRIMARY_PORT"
MIRROR_URL="http://127.0.0.1:$MIRROR_PORT"

ensure_release_binaries
bench_record_machine
bench_require_free_port "$PRIMARY_PORT"
bench_require_free_port "$MIRROR_PORT"

RAW="$(raw_file registry)"
trap bench_cleanup EXIT

BENCH_WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-bench-registry.XXXXXX")"
export HARD_REGISTRY="$PRIMARY_URL"
bench_registry_start "$PRIMARY_PORT" "$BENCH_WORK/primary"
bench_registry_start "$MIRROR_PORT" "$BENCH_WORK/mirror"

echo "# registry benchmark"
echo "# $PACKAGES packages, graph ${WIDTH}x${DEPTH}, $ITERATIONS iterations"

export HARD_HOME="$BENCH_WORK/home"
mkdir -p "$HARD_HOME"
"$HARD_BIN" keys add "$PRIMARY_URL" --trust --allow-test-key >/dev/null 2>&1 || true

# ---- a dependency graph, published to both registries ----------------------
GRAPH="$BENCH_WORK/src"
publish_one() { # url name version deps
  local url="$1" name="$2" version="$3" deps="$4"
  local dir="$GRAPH/$name-$version"
  bench_package "$dir" "$name" "$version" "$LINES"
  [ -n "$deps" ] && printf '\n[dependencies]\n%s\n' "$deps" >> "$dir/hard.toml"
  bench_publish "$dir" "$url"
}

echo "# publishing the corpus to both registries"
for level in $(seq 1 "$DEPTH"); do
  for w in $(seq 1 "$WIDTH"); do
    idx=$(( (level - 1) * WIDTH + w ))
    name="graph-l$level-$w"
    deps=""
    if [ "$level" -gt 1 ]; then
      for d in $(seq 1 2); do
        deps="$deps graph-l$((level - 1))-$(( (w + d - 1) % WIDTH + 1 )) = \"1\"\n"
      done
    fi
    publish_one "$PRIMARY_URL" "$name" "1.0.0" "$(printf "$deps")"
    publish_one "$MIRROR_URL" "$name" "1.0.0" "$(printf "$deps")"
  done
done
# plus a flat set, for a graph with no sharing
for i in $(seq 1 "$PACKAGES"); do
  publish_one "$PRIMARY_URL" "flat-$i" "1.0.0" ""
  publish_one "$MIRROR_URL" "flat-$i" "1.0.0" ""
done

# ---- resolution latency: a deep graph, resolved against a warm cache -------
PROJECT="$BENCH_WORK/project"
mkdir -p "$PROJECT"
{
  printf 'schema = 1\nname = "bench-root"\nversion = "0.1.0"\nedition = "2027"\n'
  printf '\n[dependencies]\n'
  for i in $(seq 1 "$PACKAGES"); do printf 'flat-%s = "1"\n' "$i"; done
  for w in $(seq 1 "$WIDTH"); do printf 'graph-l%s-%s = "1"\n' "$DEPTH" "$w"; done
} > "$PROJECT/hard.toml"
printf 'calc f() => Int { <- 1 }\n' > "$PROJECT/main.hard"

# warm the cache and the lockfile once
( cd "$PROJECT" && "$HARD_BIN" install >/dev/null 2>&1 )

RESOLVE="$BENCH_WORK/resolve.txt"
for i in $(seq 1 "$ITERATIONS"); do
  rm -f "$PROJECT/hard.lock"
  start=$(now_ns)
  ( cd "$PROJECT" && "$HARD_BIN" install --offline >/dev/null 2>&1 )
  echo "$(elapsed_us "$start")" >> "$RESOLVE"
done

# resolution with the lockfile trusted: the cheap path a CI build takes
LOCKED="$BENCH_WORK/locked.txt"
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard"
  start=$(now_ns)
  ( cd "$PROJECT" && "$HARD_BIN" install --frozen >/dev/null 2>&1 )
  echo "$(elapsed_us "$start")" >> "$LOCKED"
done

# ---- health checks ---------------------------------------------------------
HEALTH="$BENCH_WORK/health.txt"
for i in $(seq 1 "$ITERATIONS"); do
  start=$(now_ns)
  curl -sS -o /dev/null "$PRIMARY_URL/health"
  echo "$(elapsed_us "$start")" >> "$HEALTH"
done

record_stats "$RAW" resolve_warm_cache "$RESOLVE"
record_stats "$RAW" install_locked "$LOCKED"
record_stats "$RAW" health "$HEALTH"
record "$RAW" "graph_packages" "$(curl -sS "$PRIMARY_URL/stats" | python3 -c 'import json,sys;print(json.load(sys.stdin)["packages"])')" count
record "$RAW" "graph_shape_width" "$WIDTH" count
record "$RAW" "graph_shape_depth" "$DEPTH" count

# ---- mirror fallback: what it costs when the default disappears ------------
cat > "$PROJECT/hard.toml" <<EOF
schema = 1
name = "bench-root"
version = "0.1.0"
edition = "2027"

[registry]
default = "$PRIMARY_URL"

[[registry.mirror]]
url = "$MIRROR_URL"
EOF

# the same operation, primary up
DIRECT="$BENCH_WORK/direct.txt"
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard" "$PROJECT/hard.lock"
  start=$(now_ns)
  ( cd "$PROJECT" && "$HARD_BIN" install --offline >/dev/null 2>&1 )
  echo "$(elapsed_us "$start")" >> "$DIRECT"
done

bench_registry_stop_matching "$PRIMARY_PORT" || true
sleep 0.3
FALLBACK="$BENCH_WORK/fallback.txt"
FALLBACK_STATUS=1
if curl -sS -m 1 -o /dev/null "$PRIMARY_URL/health" 2>/dev/null; then
  FALLBACK_STATUS=0
fi
for i in $(seq 1 "$ITERATIONS"); do
  rm -rf "$PROJECT/.hard" "$PROJECT/hard.lock"
  start=$(now_ns)
  if ( cd "$PROJECT" && "$HARD_BIN" install >"$BENCH_WORK/fallback.out" 2>&1 ); then
    FALLBACK_STATUS=0
  elif [ -z "${BENCH_FALLBACK_DEBUG:-}" ]; then
    echo "fallback install failed:" >&2
    head -5 "$BENCH_WORK/fallback.out" >&2
  fi
  echo "$(elapsed_us "$start")" >> "$FALLBACK"
done
SEARCH_FALLBACK="$BENCH_WORK/search-fallback.txt"
for i in $(seq 1 "$ITERATIONS"); do
  start=$(now_ns)
  ( cd "$PROJECT" && "$HARD_BIN" search graph >/dev/null 2>&1 ) || true
  echo "$(elapsed_us "$start")" >> "$SEARCH_FALLBACK"
done

record_stats "$RAW" install_primary "$DIRECT"
record_stats "$RAW" install_mirror_fallback "$FALLBACK"
record_stats "$RAW" search_mirror_fallback "$SEARCH_FALLBACK"
# FALLBACK_STATUS is 0 when every attempt succeeded; record that as the plain
# boolean a reader expects.
record "$RAW" "mirror_fallback_works" "$([ "$FALLBACK_STATUS" -eq 0 ] && echo 1 || echo 0)" bool

echo
echo "resolve (warm cache) $(stats "$RESOLVE")"
echo "install --frozen     $(stats "$LOCKED")"
echo "health               $(stats "$HEALTH")"
echo "install, primary up  $(stats "$DIRECT")"
echo "install, fallback    $(stats "$FALLBACK")"
echo "search,  fallback    $(stats "$SEARCH_FALLBACK")"
echo "raw -> $RAW"

# ---- the other three, so one command produces every report -----------------
if [ "${BENCH_SKIP_SUB:-0}" != "1" ]; then
  # Stop this benchmark's registries first: the sub-benchmarks start their own,
  # and leaving four servers running would have them compete for the same CPU
  # and disk, which is not what any of these numbers is supposed to measure.
  bench_registry_stop_matching "$PRIMARY_PORT" || true
  bench_registry_stop_matching "$MIRROR_PORT" || true
  sleep 0.3
  echo
  echo "# running the publish, download and search benchmarks"
  BENCH_PORT=$((PRIMARY_PORT + 10)) "$QA_ROOT/bench_publish.sh" | tail -6
  BENCH_PORT=$((PRIMARY_PORT + 20)) "$QA_ROOT/bench_download.sh" | tail -8
  BENCH_PORT=$((PRIMARY_PORT + 30)) "$QA_ROOT/bench_search.sh" | tail -4
fi
