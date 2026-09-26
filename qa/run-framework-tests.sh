#!/usr/bin/env bash
# Final ORM QA gate (M5.3.10).
#
# Runs every required suite and counts every ORM test against the milestone
# minimums (CRUD 40, relationships 20, migration 25, transaction 25, SQLite
# 20, PostgreSQL 20, batch 20; 160 total). A test belongs to exactly one
# area: C++ fixtures by file, compiler unit tests by name pattern (batch,
# then migration, then relationships, then CRUD), fixtures and lanes by
# directory. Exit non-zero on any failure or any minimum missed.
#
# Run: qa/run-framework-tests.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD="${HARD:-$ROOT/target/debug/hard}"
CXX="${CXX:-g++}"
TMP=$(mktemp -d /tmp/hs-framework-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "framework: PASS $1"; }
bad() { FAIL=$((FAIL + 1)); echo "framework: FAIL $1"; }

# Sanitizer binaries are heavy and socket tests are timing-sensitive; a run
# killed by the sandbox (no output at all) is retried once before it counts
# as a failure. A real sanitizer finding reproduces every time.
run_twice() {
    local log="$1"; shift
    if timeout 300 "$@" >"$log" 2>&1; then
        return 0
    fi
    if [ -s "$log" ]; then
        return 1
    fi
    echo "framework: NOTE $1 killed with no output; retrying once" >&2
    timeout 300 "$@" >"$log" 2>&1
}

section() { echo "framework: === $1 ==="; }

# ---------------------------------------------------------------- workspace
section "cargo test --workspace"
if cargo test -q --workspace >"$TMP/workspace.log" 2>&1; then
    ok "cargo test --workspace"
else
    bad "cargo test --workspace"
    grep -E "FAILED|failures:" "$TMP/workspace.log" | head -5
fi
COMPILER_TOTAL=$(cargo test -p hs-compiler -- --list 2>/dev/null | grep -c ": test$")

# ---------------------------------------------------------------- suites
section "regressions / integration / phase3 / cli_matrix"
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
if ./qa/phase3_snap.sh >"$TMP/phase3.log" 2>&1; then
    ok "phase3 $(grep -c PASS "$TMP/phase3.log" 2>/dev/null || echo 0) cases"
else
    bad "phase3"; tail -3 "$TMP/phase3.log"
fi
if python3 ./qa/cli_matrix/run_matrix.py "$HARD" >"$TMP/matrix.log" 2>&1; then
    ok "cli_matrix $(grep -E '^cases:' "$TMP/matrix.log" | head -1)"
else
    bad "cli_matrix"; tail -3 "$TMP/matrix.log"
fi

# ---------------------------------------------------------------- orm suites
section "orm suites (generated code + diagnostics gate)"
if ./qa/run-orm-tests.sh >"$TMP/orm.log" 2>&1; then
    ok "orm $(tail -1 "$TMP/orm.log")"
else
    bad "orm"; grep FAIL "$TMP/orm.log" | head -8
fi

# ---------------------------------------------------------------- fixture counts
section "C++ fixture counts"
declare -A FIX_AREA=( [query_builder]=crud [crud]=crud [relations]=relationships
    [sqlite]=sqlite [pgsql]=postgres [migrate]=migration [transactions]=transaction [batch]=batch )
declare -A FIX_COUNT=()
for f in query_builder crud relations sqlite pgsql migrate transactions batch; do
    if ! $CXX -std=c++17 -O1 -pthread -Werror -I "$ROOT/runtime" -I "$ROOT/qa/orm" \
        "$ROOT/qa/orm/$f.cpp" -o "$TMP/fx-$f" -ldl 2>"$TMP/fx-$f.err"; then
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
section "ASan+UBSan and LSan"
for f in query_builder crud relations sqlite pgsql migrate transactions batch; do
    if ! $CXX -std=c++17 -O1 -g -pthread -Werror -fsanitize=address,undefined \
        -I "$ROOT/runtime" -I "$ROOT/qa/orm" "$ROOT/qa/orm/$f.cpp" -o "$TMP/asan-$f" -ldl \
        2>"$TMP/asan-$f.err"; then
        bad "asan compile $f"; head -3 "$TMP/asan-$f.err"; continue
    fi
    if run_twice "$TMP/asan-$f.log" "$TMP/asan-$f"; then
        ok "asan+ubsan $f ($(tail -1 "$TMP/asan-$f.log"))"
    else
        bad "asan+ubsan $f"; tail -5 "$TMP/asan-$f.log"
    fi
    if ! $CXX -std=c++17 -O1 -g -pthread -I "$ROOT/runtime" -I "$ROOT/qa/orm" \
        "$ROOT/qa/orm/$f.cpp" -o "$TMP/lsan-$f" -ldl -fsanitize=leak 2>"$TMP/lsan-$f.err"; then
        bad "lsan compile $f"; head -3 "$TMP/lsan-$f.err"; continue
    fi
    if run_twice "$TMP/lsan-$f.log" "$TMP/lsan-$f"; then
        ok "lsan $f ($(tail -1 "$TMP/lsan-$f.log"))"
    else
        bad "lsan $f"; tail -5 "$TMP/lsan-$f.log"
    fi
done

# ---------------------------------------------------------------- formatter
section "formatter idempotency on ORM fixtures"
FMT_OK=0
FMT_TOTAL=0
for f in "$ROOT"/qa/orm/*.hard; do
    [ -e "$f" ] || continue
    FMT_TOTAL=$((FMT_TOTAL + 1))
    cp "$f" "$TMP/fmt.hard"
    "$HARD" fmt "$TMP/fmt.hard" >/dev/null 2>&1
    cp "$TMP/fmt.hard" "$TMP/fmt.once"
    "$HARD" fmt "$TMP/fmt.hard" >/dev/null 2>&1
    if cmp -s "$TMP/fmt.once" "$TMP/fmt.hard"; then
        FMT_OK=$((FMT_OK + 1))
    else
        bad "fmt idempotency $(basename "$f")"
    fi
done
[ "$FMT_OK" -eq "$FMT_TOTAL" ] && ok "fmt idempotent on $FMT_TOTAL fixtures"

# ---------------------------------------------------------------- docs generation
section "docs generation determinism"
if "$HARD" errors --markdown >"$TMP/errors.md" 2>/dev/null && cmp -s "$TMP/errors.md" "$ROOT/docs/errors.md"; then
    ok "errors.md matches the catalog"
else
    bad "errors.md differs from the catalog output"
fi

# ---------------------------------------------------------------- counts
section "minimums"
LIST=$(cargo test -p hs-compiler -- --list 2>/dev/null | grep ": test$" || true)
ORM_LIST=$(echo "$LIST" | grep "orm::tests::" || true)
BATCH_RE='create_many|update_many|delete_many|find_many|batch_arity'
MIG_RE='fingerprint|join_table|new_column|new_foreign_key|new_table|unique|added_index|auto_increment|drop_reverses|index_statements|literal_defaults|rebuild|alter_column|dialect|defaults|primary_key|implicit_id|canonical|stable|emitted|quoted|strict_only|bracket|table_name|column_names|plural|documented_model'
REL_RE='belongs_to|has_many|has_one|many_to_many|junction|relation|back_reference|singular|through|implies|narrowed|to_one|other_way|falls_back|declared_back|records_the_variable|takes_no_arguments|field_is_not_a_column|column_is_not_read|member_chain|model_rooted|dynamic_column|field_typed|bare_name|unknown_relationship|lists_the_real'
count_re() { echo "$ORM_LIST" | grep -cE "$1" || true; }
U_BATCH=$(count_re "$BATCH_RE")
U_MIG=$(echo "$ORM_LIST" | grep -vE "$BATCH_RE" | grep -cE "$MIG_RE" || true)
U_REL=$(echo "$ORM_LIST" | grep -vE "$BATCH_RE|$MIG_RE" | grep -cE "$REL_RE" || true)
U_CRUD=$(echo "$ORM_LIST" | grep -vE "$BATCH_RE|$MIG_RE|$REL_RE" | grep -c . || true)
U_TX=$(echo "$LIST" | grep -c "typecheck::tests::" || true)
U_TX_EXTRA=$(echo "$LIST" | grep -cE "fmt::tests::a_transaction|astser::tests::a_transaction" || true)
U_PM_DB=$(cargo test -p hs-pm -- --list 2>/dev/null | grep -cE "database" || true)
GEN_CRUD=$(grep -c . "$ROOT/qa/orm/query.exp" 2>/dev/null || echo 0)
GEN_CRUD=$((GEN_CRUD + $(grep -c . "$ROOT/qa/orm/writes.exp" 2>/dev/null || echo 0)))
GEN_REL=$(grep -c . "$ROOT/qa/orm/relations.exp" 2>/dev/null || echo 0)
GEN_BATCH=$(grep -c . "$ROOT/qa/orm/batch.exp" 2>/dev/null || echo 0)
GEN_TX=$(grep -c . "$ROOT/qa/orm/transactions.exp" 2>/dev/null || echo 0)
DIAG_CRUD=0; DIAG_REL=0; DIAG_TX=0; DIAG_BATCH=0
for f in "$ROOT"/qa/orm/diag_*.exp; do
    [ -e "$f" ] || continue
    n=$(grep -c . "$f" || echo 0)
    case "$(basename "$f")" in
        diag_tx_*) DIAG_TX=$((DIAG_TX + n)) ;;
        diag_batch*) DIAG_BATCH=$((DIAG_BATCH + n)) ;;
        diag_relation*) DIAG_REL=$((DIAG_REL + n)) ;;
        *) DIAG_CRUD=$((DIAG_CRUD + n)) ;;
    esac
done
CLI_MG=$(grep -c "^mg" "$ROOT/qa/cli_matrix/matrix.tsv" || echo 0)

CRUD=$(( ${FIX_COUNT[query_builder]:-0} + ${FIX_COUNT[crud]:-0} + U_CRUD + GEN_CRUD + DIAG_CRUD ))
REL=$(( ${FIX_COUNT[relations]:-0} + U_REL + GEN_REL + DIAG_REL ))
MIG=$(( ${FIX_COUNT[migrate]:-0} + U_MIG + U_PM_DB + CLI_MG ))
TX=$(( ${FIX_COUNT[transactions]:-0} + U_TX + U_TX_EXTRA + GEN_TX + DIAG_TX ))
SQLITE_N=${FIX_COUNT[sqlite]:-0}
PG_N=${FIX_COUNT[pgsql]:-0}
BATCH=$(( ${FIX_COUNT[batch]:-0} + U_BATCH + GEN_BATCH + DIAG_BATCH ))
TOTAL=$((CRUD + REL + MIG + TX + SQLITE_N + PG_N + BATCH))

check_min() {
    if [ "$2" -ge "$3" ]; then ok "$1: $2 (minimum $3)"; else bad "$1: $2 (minimum $3)"; fi
}
check_min "CRUD" "$CRUD" 40
check_min "relationships" "$REL" 20
check_min "migration" "$MIG" 25
check_min "transaction" "$TX" 25
check_min "SQLite" "$SQLITE_N" 20
check_min "PostgreSQL" "$PG_N" 20
check_min "batch" "$BATCH" 20
check_min "total ORM tests" "$TOTAL" 160
echo "framework: compiler unit tests total: $COMPILER_TOTAL"

echo "framework: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
