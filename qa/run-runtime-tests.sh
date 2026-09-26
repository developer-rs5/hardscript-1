#!/usr/bin/env bash
# Runtime test suite for the M6 cloud runtime (cache, queue, scheduler, ...).
#
#   qa/runtime/*.cpp   self-asserting C++ fixtures compiled against runtime/
#   qa/runtime/*.hard  HardScript programs the suite builds with the real compiler
#   qa/runtime/*.exp   the generated code those programs must produce
#   qa/runtime/diag_*.hard  programs that must fail, with diag_*.exp listing
#                       the codes or messages the programmer has to be shown
#
# Every milestone adds to this runner; M6.10 gates on the whole thing.
# Exit 0 if every check passes, non-zero otherwise.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/qa/runtime"
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
    name="runtime-$(basename "${f%.cpp}")"
    TOTAL=$((TOTAL + 1))

    if ! $CXX $CXXFLAGS -I "$ROOT/runtime" -I "$DIR" "$f" -o "$TMP/$name" -ldl 2>"$TMP/err"; then
        echo "runtime: FAIL $name (compile: $(head -3 "$TMP/err" | tr '\n' ' '))"
        FAILED=$((FAILED + 1))
        continue
    fi
    if out=$("$TMP/$name" 2>&1); then
        echo "runtime: PASS $name ($out)"
    else
        echo "runtime: FAIL $name"
        echo "$out" | sed 's/^/    /'
        FAILED=$((FAILED + 1))
    fi
done

# ---- generated code --------------------------------------------------------
# The compiler's output is part of the contract, so the C++ it emits is
# checked against the expectations in the .exp files rather than left to
# review.
for f in "$DIR"/*.hard; do
    [ -e "$f" ] || continue
    # The diag_ fixtures are expected to fail; they have their own loop.
    case "$(basename "$f")" in diag_*) continue ;; esac
    base="${f%.hard}"
    base_name="$(basename "$base")"
    name="runtime-gen-$base_name"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "runtime: FAIL $name (missing $exp)"; FAILED=$((FAILED + 1)); continue; }
    TOTAL=$((TOTAL + 1))

    if ! "$HARD" build "$f" >"$TMP/build.log" 2>&1; then
        echo "runtime: FAIL $name (build: $(grep -m1 -E 'error|HS[0-9]{4}' "$TMP/build.log"))"
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
        echo "runtime: FAIL $name (no generated C++ in $(dirname "$f")/.hard/)"
        FAILED=$((FAILED + 1))
        continue
    fi
    missing=0
    while IFS= read -r want; do
        [ -n "$want" ] || continue
        if ! grep -qF -- "$want" "$gen"; then
            echo "runtime: FAIL $name (generated code lacks: $want)"
            missing=$((missing + 1))
        fi
    done < "$exp"

    if [ "$missing" -gt 0 ]; then
        FAILED=$((FAILED + 1))
    else
        echo "runtime: PASS $name ($(grep -c . "$exp") expectations in $(basename "$gen"))"
    fi
done

# ---- formatter round trip --------------------------------------------------
# The M6 declarations (cache, queue, limit, email.send) are parsed into
# ordinary calls, so the formatter is the only thing standing between a user
# and their own source rewritten as machinery. Every fixture must survive
# `fmt` and still build.
for f in "$DIR"/*.hard; do
    [ -e "$f" ] || continue
    case "$(basename "$f")" in diag_*) continue ;; esac
    base_name="$(basename "${f%.hard}")"
    name="runtime-fmt-$base_name"
    TOTAL=$((TOTAL + 1))

    rt="$TMP/fmt-$base_name"
    mkdir -p "$rt"
    cp "$f" "$rt/main.hard"
    if ! "$HARD" fmt "$rt/main.hard" >"$TMP/fmt.log" 2>&1; then
        echo "runtime: FAIL $name (fmt: $(grep -m1 -E 'error|HS[0-9]{4}' "$TMP/fmt.log"))"
        FAILED=$((FAILED + 1))
        continue
    fi
    # Idempotent: a second pass must not change the file.
    cp "$rt/main.hard" "$TMP/fmt-$base_name.once"
    "$HARD" fmt "$rt/main.hard" >/dev/null 2>&1
    if ! cmp -s "$TMP/fmt-$base_name.once" "$rt/main.hard"; then
        echo "runtime: FAIL $name (fmt is not idempotent)"
        FAILED=$((FAILED + 1))
        continue
    fi
    if ! "$HARD" build "$rt/main.hard" >"$TMP/fmtbuild.log" 2>&1; then
        echo "runtime: FAIL $name (formatted source does not build: $(grep -m1 -E 'error|HS[0-9]{4}' "$TMP/fmtbuild.log"))"
        FAILED=$((FAILED + 1))
        continue
    fi
    echo "runtime: PASS $name (formatted source still builds)"
done

# ---- diagnostics -----------------------------------------------------------
for f in "$DIR"/diag_*.hard; do
    [ -e "$f" ] || continue
    base="${f%.hard}"
    name="runtime-diag-$(basename "${f%.hard}")"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "runtime: FAIL $name (missing $exp)"; FAILED=$((FAILED + 1)); continue; }
    TOTAL=$((TOTAL + 1))

    want="$(grep -c . "$exp")"
    log="$("$HARD" build "$f" 2>&1)"
    if [ $? -eq 0 ]; then
        echo "runtime: FAIL $name (built; expected $want diagnostic(s))"
        FAILED=$((FAILED + 1))
        continue
    fi
    missing=0
    while IFS= read -r code; do
        [ -n "$code" ] || continue
        grep -q "$code" <<<"$log" || { echo "runtime: FAIL $name (no $code in the output)"; missing=$((missing + 1)); }
    done < "$exp"
    if [ "$missing" -gt 0 ]; then
        FAILED=$((FAILED + 1))
    else
        echo "runtime: PASS $name ($want diagnostic(s))"
    fi
done

# ---- summary ---------------------------------------------------------------
if [ "$FAILED" -gt 0 ]; then
    echo "runtime: $FAILED of $TOTAL suites failed"
    exit 1
fi
echo "runtime: all $TOTAL suites passed"
