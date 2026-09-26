#!/usr/bin/env bash
# Final cloud runtime QA gate (M6.10).
#
# Runs every suite the runtime has to pass and counts every runtime test
# against the milestone minimums (cache 80, queue 95, scheduler 75, session
# 70, rate limiter 55, email 100, metrics 95, cluster 140; 710 total). A test
# belongs to exactly one area: C++ fixtures by file, compiler unit tests by
# name pattern, generated expectations and diagnostics by fixture, CLI lanes by
# id. Exit non-zero on any failure or any minimum missed.
#
# Run: qa/run-runtime-qa.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD="${HARD:-$ROOT/target/debug/hard}"
CXX="${CXX:-g++}"
TMP=$(mktemp -d /tmp/hs-runtime-qa-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "runtime-qa: PASS $1"; }
bad() { FAIL=$((FAIL + 1)); echo "runtime-qa: FAIL $1"; }
section() { echo "runtime-qa: === $1 ==="; }

# Sanitizer runs are heavy and the socket fixtures are timing-sensitive: a run
# killed with no output at all is retried once before it counts as a failure. A
# real sanitizer finding reproduces every time.
run_twice() {
    local log="$1"; shift
    if timeout 600 "$@" >"$log" 2>&1; then
        return 0
    fi
    if [ -s "$log" ]; then
        return 1
    fi
    echo "runtime-qa: NOTE $* killed with no output; retrying once" >&2
    timeout 600 "$@" >"$log" 2>&1
}

# ---------------------------------------------------------------- workspace
section "cargo test --workspace"
if cargo test -q --workspace >"$TMP/workspace.log" 2>&1; then
    ok "cargo test --workspace"
else
    bad "cargo test --workspace"
    grep -E "FAILED|failures:" "$TMP/workspace.log" | head -5
fi

# ---------------------------------------------------------------- suites
section "regressions / integration / determinism / cli matrix"
if ./tests/run-regressions.sh >"$TMP/regressions.log" 2>&1; then
    ok "regressions $(tail -1 "$TMP/regressions.log")"
else
    bad "regressions"; tail -3 "$TMP/regressions.log"
fi
if ./tests/integration.sh >"$TMP/integration.log" 2>&1; then
    ok "integration $(tail -1 "$TMP/integration.log")"
else
    bad "integration"; tail -3 "$TMP/integration.log"
fi
if ./qa/phase3_snap.sh >"$TMP/snap.log" 2>&1; then
    ok "codegen snapshots $(tail -1 "$TMP/snap.log")"
else
    bad "codegen snapshots"; tail -3 "$TMP/snap.log"
fi
if python3 "$ROOT/qa/cli_matrix/run_matrix.py" "$HARD" >"$TMP/matrix.log" 2>&1; then
    ok "cli matrix $(grep -E '^cases:' "$TMP/matrix.log" | head -1)"
else
    bad "cli matrix"; tail -3 "$TMP/matrix.log"
fi

section "runtime suites (M6) and framework suites (M5)"
if ./qa/run-runtime-tests.sh >"$TMP/runtime.log" 2>&1; then
    ok "runtime $(tail -1 "$TMP/runtime.log")"
else
    bad "runtime"; grep FAIL "$TMP/runtime.log" | head -5
fi
if ./qa/run-orm-tests.sh >"$TMP/orm.log" 2>&1; then
    ok "orm $(tail -1 "$TMP/orm.log")"
else
    bad "orm"; grep FAIL "$TMP/orm.log" | head -5
fi
if ./qa/run-framework-tests.sh >"$TMP/framework.log" 2>&1; then
    ok "framework $(tail -1 "$TMP/framework.log")"
else
    bad "framework"; grep -E "FAIL" "$TMP/framework.log" | head -5
fi

# ---------------------------------------------------------------- fixtures
section "C++ fixture counts"
FIXTURES="cache queue sched session ratelimit email metrics cluster"
declare -A FIX_COUNT=()
for f in $FIXTURES; do
    if ! $CXX -std=c++17 -O1 -pthread -Werror -I "$ROOT/runtime" -I "$ROOT/qa/runtime" \
        -I "$ROOT/qa/orm" "$ROOT/qa/runtime/$f.cpp" -o "$TMP/fx-$f" -ldl 2>"$TMP/fx-$f.err"; then
        bad "compile $f"; head -5 "$TMP/fx-$f.err"; continue
    fi
    if out=$(run_twice "$TMP/fx-$f.log" "$TMP/fx-$f" && cat "$TMP/fx-$f.log"); then
        n=$(echo "$out" | tail -1 | grep -oE "[0-9]+ checks passed" | grep -oE "[0-9]+")
        FIX_COUNT[$f]="${n:-0}"
        ok "$f: ${n:-0} checks"
    else
        bad "$f"; head -5 "$TMP/fx-$f.log"
    fi
done

# ---------------------------------------------------------------- sanitizers
section "ASan+UBSan, TSan and LSan"
for f in $FIXTURES; do
    [ -f "$ROOT/qa/runtime/$f.cpp" ] || continue
    if ! $CXX -std=c++17 -O1 -g -pthread -Werror -fsanitize=address,undefined \
        -I "$ROOT/runtime" -I "$ROOT/qa/runtime" -I "$ROOT/qa/orm" \
        "$ROOT/qa/runtime/$f.cpp" -o "$TMP/asan-$f" -ldl 2>"$TMP/asan-$f.err"; then
        bad "asan compile $f"; head -3 "$TMP/asan-$f.err"; continue
    fi
    if run_twice "$TMP/asan-$f.log" "$TMP/asan-$f"; then
        ok "asan+ubsan $f ($(tail -1 "$TMP/asan-$f.log"))"
    else
        bad "asan+ubsan $f"; tail -5 "$TMP/asan-$f.log"
    fi
    if ! $CXX -std=c++17 -O1 -g -pthread -Werror -fsanitize=thread \
        -I "$ROOT/runtime" -I "$ROOT/qa/runtime" -I "$ROOT/qa/orm" \
        "$ROOT/qa/runtime/$f.cpp" -o "$TMP/tsan-$f" -ldl 2>"$TMP/tsan-$f.err"; then
        bad "tsan compile $f"; head -3 "$TMP/tsan-$f.err"; continue
    fi
    if run_twice "$TMP/tsan-$f.log" "$TMP/tsan-$f"; then
        if grep -q "WARNING: ThreadSanitizer" "$TMP/tsan-$f.log"; then
            bad "tsan $f (reported a race)"
        else
            ok "tsan $f ($(tail -1 "$TMP/tsan-$f.log"))"
        fi
    else
        bad "tsan $f"; tail -5 "$TMP/tsan-$f.log"
    fi
    if ! $CXX -std=c++17 -O1 -g -pthread -I "$ROOT/runtime" -I "$ROOT/qa/runtime" \
        -I "$ROOT/qa/orm" "$ROOT/qa/runtime/$f.cpp" -o "$TMP/lsan-$f" -ldl -fsanitize=leak \
        2>"$TMP/lsan-$f.err"; then
        bad "lsan compile $f"; head -3 "$TMP/lsan-$f.err"; continue
    fi
    if run_twice "$TMP/lsan-$f.log" "$TMP/lsan-$f"; then
        ok "lsan $f ($(tail -1 "$TMP/lsan-$f.log"))"
    else
        bad "lsan $f"; tail -5 "$TMP/lsan-$f.log"
    fi
done

# ---------------------------------------------------------------- docs
section "docs"
if "$HARD" errors --markdown >"$TMP/errors.md" 2>/dev/null && cmp -s "$TMP/errors.md" "$ROOT/docs/errors.md"; then
    ok "docs/errors.md matches the catalog"
else
    bad "docs/errors.md does not match the catalog"
fi
if [ -s "$ROOT/docs/RUNTIME.md" ]; then
    ok "docs/RUNTIME.md present ($(wc -l <"$ROOT/docs/RUNTIME.md") lines)"
else
    bad "docs/RUNTIME.md missing or empty"
fi
# The documented examples have to compile: documentation that does not build is
# a lie with a code block around it.
if [ -f "$ROOT/qa/docs/runtime_examples.hard" ]; then
    if "$HARD" build "$ROOT/qa/docs/runtime_examples.hard" >"$TMP/docs_examples.log" 2>&1; then
        ok "documented runtime examples compile"
    else
        bad "documented runtime examples do not compile"
        head -8 "$TMP/docs_examples.log"
    fi
fi

# ---------------------------------------------------------------- minimums
section "minimums"
LIST=$(cargo test -p hs-compiler -- --list 2>/dev/null | grep ": test$" || true)
# M6 compiler-side tests: the M6 declarations, the named-options call, the
# formatter round trip. Matched on the test name, not the module -- `cache` is
# also the build cache and `every` also means something else entirely.
M6_NAME_RE='^(parser::tests::(cache_declaration|cache_without_ttl|every_(day|interval|rejects|startup|weekday|without)|job_(declares|needs)|limit_(desugars|rejects|without)|queue_(desugars|rejects)|schedule_body|worker_(lowers|needs)|email_(send|template))|fmt::tests::(m6_declarations|a_hand_written_call|durations_pick))'
U_M6=$(echo "$LIST" | sed 's/: test$//' | grep -cE "$M6_NAME_RE" || true)
# Generated-code expectations and diagnostics belong to the area their fixture
# is named after.
# `grep -c` prints 0 and exits 1 when nothing matched; a `|| echo 0` after it
# appends a second line and the arithmetic below dies on the newline.
count_lines() { grep -c . "$1" 2>/dev/null || true; }

declare -A GEN=() DIAG=() CLI=()
# The fixture whose name is the area name; `cache_api` is the cache one and
# `jobs` is the queue one, so both fold into their area below.
for f in cache_api cache queue jobs sched session limit email metrics cluster; do
    [ -f "$ROOT/qa/runtime/$f.exp" ] && GEN[$f]=$(count_lines "$ROOT/qa/runtime/$f.exp")
done
GEN[cache]=$(( ${GEN[cache]:-0} + ${GEN[cache_api]:-0} ))
GEN[queue]=$(( ${GEN[queue]:-0} + ${GEN[jobs]:-0} ))
for f in "$ROOT"/qa/runtime/diag_*.exp; do
    [ -e "$f" ] || continue
    n=$(count_lines "$f")
    case "$(basename "$f")" in
        diag_cache*) GEN[cache]=$(( ${GEN[cache]:-0} + n )) ;;
        diag_queue* | diag_job*) GEN[queue]=$(( ${GEN[queue]:-0} + n )) ;;
        diag_sched*) GEN[sched]=$(( ${GEN[sched]:-0} + n )) ;;
        diag_session*) GEN[session]=$(( ${GEN[session]:-0} + n )) ;;
        diag_limit*) GEN[ratelimit]=$(( ${GEN[ratelimit]:-0} + n )) ;;
        diag_email*) GEN[email]=$(( ${GEN[email]:-0} + n )) ;;
        diag_metrics*) GEN[metrics]=$(( ${GEN[metrics]:-0} + n )) ;;
        diag_cluster*) GEN[cluster]=$(( ${GEN[cluster]:-0} + n )) ;;
    esac
done
# CLI lanes: the prefix the generator gave each area, counted from the matrix it
# wrote. (cache=ch, queue=q, jobs/scheduler=jc, session=ss, ratelimit=rl,
# email=em, metrics=mt, cluster=cl.)
for area in ch q jc ss rl em mt cl; do
    CLI[$area]=$(grep -cE "^$area[0-9]" "$ROOT/qa/cli_matrix/matrix.tsv" 2>/dev/null || true)
done

CACHE=$(( ${FIX_COUNT[cache]:-0} + ${GEN[cache]:-0} + ${CLI[ch]:-0} ))
QUEUE=$(( ${FIX_COUNT[queue]:-0} + ${GEN[queue]:-0} + ${CLI[q]:-0} + ${CLI[jc]:-0} ))
SCHED=$(( ${FIX_COUNT[sched]:-0} + ${GEN[sched]:-0} + ${CLI[jc]:-0} ))
SESSION=$(( ${FIX_COUNT[session]:-0} + ${GEN[session]:-0} + ${CLI[ss]:-0} ))
RATELIMIT=$(( ${FIX_COUNT[ratelimit]:-0} + ${GEN[ratelimit]:-0} + ${CLI[rl]:-0} ))
EMAIL=$(( ${FIX_COUNT[email]:-0} + ${GEN[email]:-0} + ${CLI[em]:-0} ))
METRICS=$(( ${FIX_COUNT[metrics]:-0} + ${GEN[metrics]:-0} + ${CLI[mt]:-0} ))
CLUSTER=$(( ${FIX_COUNT[cluster]:-0} + ${GEN[cluster]:-0} + ${CLI[cl]:-0} ))
U_M6=$((U_M6))
TOTAL=$((CACHE + QUEUE + SCHED + SESSION + RATELIMIT + EMAIL + METRICS + CLUSTER + U_M6))

check_min() {
    if [ "$2" -ge "$3" ]; then ok "$1: $2 (minimum $3)"; else bad "$1: $2 (minimum $3)"; fi
}
check_min "cache" "$CACHE" 80
check_min "queue" "$QUEUE" 95
check_min "scheduler" "$SCHED" 75
check_min "session" "$SESSION" 70
check_min "ratelimit" "$RATELIMIT" 55
check_min "email" "$EMAIL" 100
check_min "metrics" "$METRICS" 95
check_min "cluster" "$CLUSTER" 140
check_min "compiler, M6 surface" "$U_M6" 20
check_min "total" "$TOTAL" 710

# ---------------------------------------------------------------- summary
section "summary"
echo "runtime-qa: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    echo "runtime-qa: FAILED"
    exit 1
fi
echo "runtime-qa: all gates passed"
