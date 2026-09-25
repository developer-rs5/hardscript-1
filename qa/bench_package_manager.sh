#!/usr/bin/env bash
# Package-manager benchmark suite (M4.0).
#
# Measures the `hard` package manager end-to-end against a local mock
# registry (tests/support/mock_registry.py): cold resolve+download, warm
# lock-reuse reinstall, offline reinstall, cache verification, metadata
# round-trips and report generation. Deterministic, no network, `python3`
# only. Writes reports/package-manager-bench.md and prints a summary.
#
# Run: qa/bench_package_manager.sh          (uses $HARD, default debug build)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"

PORT=$( "$PYTHON" - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
TMP="$(mktemp -d /tmp/hs-pmbench-XXXXXX)"
trap 'kill "$REG_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

# ---- synthetic scale fixture: 21 packages (fan-in of one big dep) --------
"$PYTHON" - "$TMP/fixture.json" <<'PY'
import json, sys
pkgs = [{"name": "s0", "versions": [{"version": "1.0.0", "dependencies": {}, "files": [["main.hard", "calc s(){<-0}\n"]]}]}]
for i in range(1, 20):
    dep = f"s{i-1}"
    pkgs.append({"name": f"s{i}", "versions": [{"version": "1.0.0", "dependencies": {dep: "*"}, "files": [["main.hard", f"calc s{i}()=>Int{{<-{i}}}\n"]]}]})
# a distinct chain for cache-size / verify benchmarks
for i in range(6):
    chain = [{"name": f"c{i}", "versions": [{"version": "1.0.0", "dependencies": {}, "files": [["main.hard", "calc c()=>Int{<-0}\n"]]}]}]
    pkgs.extend(chain)
json.dump({"packages": pkgs, "search": {"s0": [{"name": "s0", "version": "1.0.0", "description": "bench"}]}}, open(sys.argv[1], "w"))
print("fixture ready")
PY
"$PYTHON" "$ROOT/tests/support/mock_registry.py" "$TMP/fixture.json" "$PORT" >"$TMP/ready" 2>&1 &
REG_PID=$!
for _ in $(seq 1 100); do
    grep -q READY "$TMP/ready" 2>/dev/null && break
    kill -0 "$REG_PID" 2>/dev/null || { echo "bench: mock registry died"; cat "$TMP/ready"; exit 1; }
    sleep 0.1
done

export HARD_HOME="$TMP/home"
export HARD_REGISTRY="http://127.0.0.1:$PORT"
export HARD_BIN="$HARD"
cd "$TMP" || exit 1

mkdir -p app && cd app
cat > hard.toml <<'EOF'
name = "benchapp"
version = "0.1.0"
edition = "2027"

[dependencies]
s19 = "*"
EOF

bench_ms() {  # $1 = name, rest = command; echoes "NAME ms"
    local name="$1"; shift
    local start done ns ms
    start=$(date +%s%N)
    "$@" >/dev/null 2>&1
    done=$(date +%s%N)
    ns=$((done - start))
    ms=$((ns / 1000000))
    echo "$name $ms"
}
min_of() {  # already-sorted numbers on stdin -> min
    sort -n | head -1
}

measure() {  # $1 = name; $2 = iterations; rest = command
    local name="$1"; shift
    local it="$1"; shift
    local i v best=9999999
    for i in $(seq 1 "$it"); do
        v=$(bench_ms x "$@")
        v=${v##* }
        [ "$v" -lt "$best" ] && best=$v
    done
    echo "$name $best"
}

cold_clean() {
    rm -rf "$HARD_HOME" .hard/packages hard.lock
}

RUNS=5

# cold resolve+download: wipe cache + project state before every replicate
CA=$(cd "$TMP/app" && { for i in $(seq 1 "$RUNS"); do cold_clean; bench_ms x "$HARD" install; done; } | sort -n -k2 | head -1)
# warm reinstall with an unchanged lock (no registry, no downloads)
WARM=0
(cd "$TMP/app" && cold_clean && "$HARD" install >/dev/null 2>&1)
WARM=$( cd "$TMP/app" && measure "warm" $RUNS "$HARD" install )
# offline reinstall from cache
OFF=$( cd "$TMP/app" && measure "off" $RUNS "$HARD" install --offline )
# cache verify over every cached archive
VER=$( cd "$TMP/app" && measure "ver" $RUNS "$HARD" cache verify )
# metadata round-trip
SRC=$( cd "$TMP/app" && measure "src" $RUNS "$HARD" search s0 )
# report generation
RPT=$( cd "$TMP/app" && measure "rpt" $RUNS "$HARD" report )

r1="cold-resolve-download $(echo "$CA" | awk '{print $2}')"
r2="warm-reinstall-lock-reuse ${WARM##* }"
r3="offline-reinstall ${OFF##* }"
r4="cache-verify-all ${VER##* }"
r5="metadata-search ${SRC##* }"
r6="report-generation ${RPT##* }"

UNAME=$(uname -srm 2>/dev/null || true)
TS_START=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || true)

mkdir -p "$ROOT/reports"
OUT="$ROOT/reports/package-manager-bench.md"
{
    echo "# Package-manager benchmarks (M4.0)"
    echo
    echo "- host: \`$UNAME\` (linux)"
    echo "- binary: \`$HARD\`"
    echo "- registry: mock \`.hspkg\` on \`127.0.0.1:$PORT\` (21 packages, sha256 integrity)"
    echo "- machine clock: \`date +%s%N\` (wall, best of $RUNS)"
    echo
    echo "## Results (ms, lower is better)"
    echo
    echo "| Phase | ms |"
    echo "| --- | ---: |"
    for r in "$r1" "$r2" "$r3" "$r4" "$r5" "$r6"; do
        name=${r%% *}
        ms=${r##* }
        case "$name" in
            cold-resolve-download) label="resolve + download (cold cache, 21 packages)" ;;
            warm-reinstall-lock-reuse) label="reinstall with unchanged lock (no re-download)" ;;
            offline-reinstall) label="offline reinstall from cache" ;;
            cache-verify-all) label="cache verify (all archives)" ;;
            metadata-search) label="search (single metadata round-trip)" ;;
            report-generation) label="hard report (5 markdown reports)" ;;
            *) label="$name" ;;
        esac
        echo "| $label | $ms |"
    done
    echo
    echo "Notes: cold resolve exercises resolve + sharded fetch loop over the"
    echo "mock registry; the metadata search and report numbers include one"
    echo "server round-trip each. Times are wall-clock best-of-$RUNS in ms."
} > "$OUT"

# canonical M4.0 ecosystem reports (written by `hard report`)
cp -r reports/package-manager.md reports/dependency-resolver.md reports/cache-report.md reports/workspace-report.md reports/templates-report.md "$ROOT/reports/" 2>/dev/null || true

echo "bench: wrote reports/package-manager-bench.md"
for r in "$r1" "$r2" "$r3" "$r4" "$r5" "$r6"; do
    printf "  %-40s %6s ms\n" "$(echo "$r" | cut -d' ' -f1)" "$(echo "$r" | cut -d' ' -f2)"
done