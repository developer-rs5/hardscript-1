#!/usr/bin/env bash
# ORM test suite for the native ORM (M5.3).
#
#   qa/orm/*.cpp   self-asserting C++ fixtures compiled against runtime/
#   qa/orm/*.hard  HardScript programs the suite builds with the real compiler
#   qa/orm/*.exp   the generated code those programs must produce
#   qa/orm/diag_*.hard  programs that must fail, with diag_*.exp listing the
#                       error codes the programmer has to be shown
#
# Every phase adds to this runner; the milestone is done when the whole gate
# passes. Nothing here needs a live database: the fixtures use a recording
# fake backend, and the SQLite and PostgreSQL phases add their own fixtures.
#
# Exit 0 if every check passes, non-zero otherwise.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/qa/orm"
HARD="${HARD:-$ROOT/target/debug/hard}"
FAILED=0
TOTAL=0

CXX="${CXX:-g++}"
CXXFLAGS="-std=c++17 -O1 -pthread -Werror"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# ---- C++ fixtures -----------------------------------------------------------
for f in "$DIR"/*.cpp; do
    [ -e "$f" ] || continue
    name="orm-$(basename "${f%.cpp}")"
    TOTAL=$((TOTAL + 1))

    if ! $CXX $CXXFLAGS -I "$ROOT/runtime" -I "$DIR" "$f" -o "$TMP/$name" 2>"$TMP/err"; then
        echo "orm: FAIL $name (compile: $(head -3 "$TMP/err" | tr '\n' ' '))"
        FAILED=$((FAILED + 1))
        continue
    fi
    if out=$("$TMP/$name" 2>&1); then
        echo "orm: PASS $name ($out)"
    else
        echo "orm: FAIL $name"
        echo "$out" | sed 's/^/    /'
        FAILED=$((FAILED + 1))
    fi
done

# ---- generated code --------------------------------------------------------
# The compiler's output is part of the contract, so the SQL and the C++ it
# emits are checked against the expectations in the .exp files rather than
# left to review.
for f in "$DIR"/*.hard; do
    [ -e "$f" ] || continue
    # The diag_ fixtures are expected to fail; they have their own loop.
    case "$(basename "$f")" in diag_*) continue ;; esac
    base="${f%.hard}"
    base_name="$(basename "$base")"
    name="orm-gen-$base_name"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "orm: FAIL $name (missing $exp)"; FAILED=$((FAILED + 1)); continue; }
    TOTAL=$((TOTAL + 1))

    if ! "$HARD" build "$f" >"$TMP/build.log" 2>&1; then
        echo "orm: FAIL $name (build: $(grep -m1 -E 'error|HS[0-9]{4}' "$TMP/build.log"))"
        FAILED=$((FAILED + 1))
        continue
    fi
    # The build writes `<name>.cpp` (or `main.cpp`) into the .hard directory
    # beside the source; take whichever it produced.
    gen=""
    for cand in "$(dirname "$f")/.hard/$base_name.cpp" "$(dirname "$f")/.hard/main.cpp"; do
        [ -f "$cand" ] && { gen="$cand"; break; }
    done
    if [ -z "$gen" ]; then
        echo "orm: FAIL $name (no generated C++ in $(dirname "$f")/.hard/)"
        FAILED=$((FAILED + 1))
        continue
    fi

    missing=0
    while IFS= read -r want; do
        [ -n "$want" ] || continue
        if ! grep -qF -- "$want" "$gen"; then
            echo "orm: FAIL $name (generated code lacks: $want)"
            missing=$((missing + 1))
        fi
    done < "$exp"

    if [ "$missing" -gt 0 ]; then
        FAILED=$((FAILED + 1))
    else
        echo "orm: PASS $name ($(grep -c . "$exp") expectations in $(basename "$gen"))"
    fi
done

# ---- diagnostics -----------------------------------------------------------
# A typed query is only useful if the error is a typed one. `where(emial == ..)`
# has to report the unknown column rather than an undefined variable, so these
# fixtures pin the code the programmer is shown.
for f in "$DIR"/diag_*.hard; do
    [ -e "$f" ] || continue
    base="${f%.hard}"
    name="orm-diag-$(basename "${f%.hard}")"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "orm: FAIL $name (missing $exp)"; FAILED=$((FAILED + 1)); continue; }
    TOTAL=$((TOTAL + 1))

    # One code per line; the build must fail and mention every one of them.
    want="$(grep -c . "$exp")"
    log="$("$HARD" build "$f" 2>&1)"
    if [ $? -eq 0 ]; then
        echo "orm: FAIL $name (built; expected $want diagnostic(s))"
        FAILED=$((FAILED + 1))
        continue
    fi
    missing=0
    while IFS= read -r code; do
        [ -n "$code" ] || continue
        grep -q "$code" <<<"$log" || { echo "orm: FAIL $name (no $code in the output)"; missing=$((missing + 1)); }
    done < "$exp"
    if [ "$missing" -gt 0 ]; then
        FAILED=$((FAILED + 1))
    else
        echo "orm: PASS $name ($want diagnostic(s))"
    fi
done

# ---- summary ---------------------------------------------------------------
if [ "$FAILED" -gt 0 ]; then
    echo "orm: $FAILED of $TOTAL suites failed"
    exit 1
fi
echo "orm: all $TOTAL suites passed"
