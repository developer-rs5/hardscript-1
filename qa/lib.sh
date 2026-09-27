#!/usr/bin/env bash
# Shared helpers for the registry benchmark scripts.
#
# The rules this harness follows, so the numbers in reports/ mean something:
#
# - Every number is measured. Nothing is estimated, interpolated or typed in
#   by hand; the scripts write `metric<TAB>value<TAB>unit` rows to
#   reports/raw/<name>.tsv and the reports are rendered from those files.
# - Latency is wall-clock around the real client, including process start and
#   the HTTP round trip, because that is what a user waits for.
# - Warm and cold are measured separately and never averaged together: a cache
#   hit is a different operation from a download.
# - The machine is described in the report. A latency without knowing the CPU is
#   not a result, it is a rumour.
#
# Sourced, not executed.

set -euo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$QA_ROOT/.." && pwd)"
RAW_DIR="$REPO_ROOT/reports/raw"
BENCH_BIN_DIR="${BENCH_BIN_DIR:-$REPO_ROOT/target/release}"
HARD_BIN="$BENCH_BIN_DIR/hard"
REG_BIN="$BENCH_BIN_DIR/hard-registry"

# --------------------------------------------------------------------------
# binaries

# Benchmarks run against the release binaries: a debug build measures `cargo`'s
# absence of optimisation, not the package manager.
ensure_release_binaries() {
  if [ ! -x "$HARD_BIN" ] || [ ! -x "$REG_BIN" ]; then
    echo "# building release binaries (first run only)" >&2
    ( cd "$REPO_ROOT" && cargo build --release --quiet && cargo build --release --quiet --package hard-registry )
  fi
  [ -x "$HARD_BIN" ] || { echo "missing $HARD_BIN" >&2; exit 1; }
  [ -x "$REG_BIN" ] || { echo "missing $REG_BIN" >&2; exit 1; }
}

# --------------------------------------------------------------------------
# raw result files

# raw_file <name> -> path, truncated
# bench_work_out <path> - where loops should redirect command output
bench_work_out() {
  WORK_OUT="$1"
}

raw_file() {
  local path="$RAW_DIR/$1.tsv"
  mkdir -p "$RAW_DIR"
  : > "$path"
  printf '%s' "$path"
}

# record_stats <raw> <prefix> <samples-file> [unit]
#
# One call records the whole distribution, so a report never has to print
# "not measured" for a column the benchmark simply forgot to capture.
record_stats() {
  local raw="$1" prefix="$2" file="$3" u="${4:-us}"
  local line
  line=$(stats "$file")
  record "$raw" "${prefix}_mean"  "$(echo "$line" | awk '{print $2}')" "$u"
  record "$raw" "${prefix}_p50"   "$(echo "$line" | awk '{print $3}')" "$u"
  record "$raw" "${prefix}_p95"   "$(echo "$line" | awk '{print $4}')" "$u"
  record "$raw" "${prefix}_min"   "$(echo "$line" | awk '{print $5}')" "$u"
  record "$raw" "${prefix}_max"   "$(echo "$line" | awk '{print $6}')" "$u"
  record "$raw" "${prefix}_count" "$(echo "$line" | awk '{print $1}')" samples
}

# record <file> <metric> <value> [unit]
record() {
  printf '%s\t%s\t%s\n' "$2" "$3" "${4:-ms}" >> "$1"
}

# --------------------------------------------------------------------------
# timing

now_ns() { date +%s%N; }

# elapsed_ms <start_ns> -> integer milliseconds
elapsed_ms() {
  echo $(( ( $(now_ns) - $1 ) / 1000000 ))
}

# elapsed_us <start_ns> -> integer microseconds
#
# Sub-millisecond work is the norm for a package manager talking to a local
# registry, so the primary unit here is the microsecond; reporting whole
# milliseconds would round most samples to the cost of starting the process.
elapsed_us() {
  echo $(( ( $(now_ns) - $1 ) / 1000 ))
}

# --------------------------------------------------------------------------
# statistics (awk over a file of numbers, one per line)

# stats <file> -> "n mean p50 p95 min max"
stats() {
  sort -n "$1" | awk '
    { v[NR] = $1; sum += $1 }
    END {
      if (NR == 0) { print "0 0 0 0 0 0"; exit }
      n = NR
      mean = sum / n
      p50 = v[int((n + 1) * 0.5)]
      p95 = v[int((n * 0.95) + 0.999)]
      if (p95 > n) p95 = v[n]
      printf "%d %.2f %d %d %d %d\n", n, mean, p50, p95, v[1], v[n]
    }'
}

# --------------------------------------------------------------------------
# a throwaway registry

BENCH_PIDS=()
BENCH_WORK=""

# bench_registry_start <port> <data-dir> [extra args...]
bench_registry_start() {
  local port="$1" data="$2"
  shift 2
  "$REG_BIN" serve --addr "127.0.0.1:$port" --data "$data" --open "$@" >"$data.log" 2>&1 &
  BENCH_PIDS+=($!)
  local i
  for i in $(seq 1 100); do
    if curl -sS -m 1 -o /dev/null "http://127.0.0.1:$port/health" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "bench registry on $port did not start" >&2
  cat "$data.log" >&2 || true
  return 1
}

# bench_registry_stop_matching <port>
bench_registry_stop_matching() {
  local port="$1" pid
  for pid in "${BENCH_PIDS[@]:-}"; do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q -- "127.0.0.1:$port"; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 0
    fi
  done
  return 1
}

bench_cleanup() {
  local pid
  for pid in "${BENCH_PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  for pid in $(ps -eo pid,cmd | grep "[h]ard-registry serve" | grep -- "$BENCH_WORK" | awk '{print $1}'); do
    kill "$pid" 2>/dev/null || true
  done
  [ -n "$BENCH_WORK" ] && rm -rf "$BENCH_WORK"
  return 0
}

# bench_require_free_port <port>
bench_require_free_port() {
  if curl -sS -m 1 -o /dev/null "http://127.0.0.1:$1/health" 2>/dev/null; then
    echo "error: something is already listening on 127.0.0.1:$1" >&2
    echo "       stop it, or set the BENCH_PORT* variables" >&2
    exit 1
  fi
}

# --------------------------------------------------------------------------
# fixtures

# bench_package <dir> <name> <version> <source-lines> [deps-toml]
bench_package() {
  local dir="$1" name="$2" version="$3" lines="${4:-20}" deps="${5:-}"
  mkdir -p "$dir"
  {
    printf 'schema = 1\nname = "%s"\nversion = "%s"\nedition = "2027"\n' "$name" "$version"
    printf 'description = "benchmark fixture %s"\nlicense = "MIT"\n' "$name"
    printf '\n[package]\ntags = ["bench"]\n'
    [ -n "$deps" ] && printf '\n%s\n' "$deps"
  } > "$dir/hard.toml"
  local i
  : > "$dir/main.hard"
  for i in $(seq 1 "$lines"); do
    printf 'calc f%s() => Int { <- %s }\n' "$i" "$i" >> "$dir/main.hard"
  done
}

# bench_publish <registry-url> <dir>
bench_publish() {
  ( cd "$1" && HARD_REGISTRY="$2" "$HARD_BIN" publish >/dev/null 2>&1 )
}

# --------------------------------------------------------------------------
# the machine, described once

bench_machine_file() {
  "$RAW_DIR/machine.tsv"
}

bench_record_machine() {
  local out="$RAW_DIR/machine.tsv"
  mkdir -p "$RAW_DIR"
  {
    printf 'cpu_model\t%s\n' "$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ *//')"
    printf 'cpu_count\t%s\n' "$(nproc)"
    printf 'memory_kb\t%s\n' "$(awk '/MemTotal/ {print $2}' /proc/meminfo)"
    printf 'kernel\t%s\n' "$(uname -sr)"
    printf 'os\t%s\n' "$(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" || uname -s)"
    printf 'filesystem\t%s\n' "$(df -T "$REPO_ROOT" | awk 'NR==2 {print $2}')"
    printf 'rustc\t%s\n' "$(rustc --version 2>/dev/null || echo unknown)"
    printf 'build_profile\trelease\n'
  } > "$out"
}

# bench_header <title> <script>
bench_header() {
  echo "# $1"
  echo
  echo "measured by \`qa/$2\` on $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
  echo
}
