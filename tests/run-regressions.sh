#!/usr/bin/env bash
# Regression suite for bugs found during the v0.1 QA torture test.
#
# Each pair of files in tests/regression/ describes one reproducible bug:
#   NNN-description.hard   -> minimal repro program
#   NNN-description.exp    -> expected behavior:
#       ok                    build must succeed (exit 0)
#         GET /path = BODY    then run the server and assert the route body
#       err\nMSG:<substr>     build must fail (exit 1) with this message text
#       esc                   build must succeed; the rest of the file is the
#                             exact escape-analysis report printed by
#                             `hard build` with HARD_ESCAPE_REPORT=1
#       cpp                   .cpp fixture compiled against the runtime
#                             headers; the rest is its exact expected stdout
#
# Exit 0 if every regression passes, non-zero otherwise.
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
DIR="$(cd "$(dirname "$0")" && pwd)/regression"
FAILED=0
TOTAL=0
PORT=3031

for f in "$DIR"/*.hard; do
    base="${f%.hard}"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "regression: missing $exp"; FAILED=$((FAILED+1)); continue; }

    lines=()
    while IFS= read -r l; do lines+=("$l"); done < "$exp"
    kind="${lines[0]}"
    TOTAL=$((TOTAL+1))

    # clean previous build artifacts
    rm -rf /tmp/hs-reg-$$
    TMP=/tmp/hs-reg-$$
    mkdir -p "$TMP"

    if [ "$kind" = "err" ]; then
        msg="${lines[1]#MSG:}"
        cp "$f" "$TMP/prog.hard"
        errout=$(cd "$TMP" && "$HARD" build prog.hard 2>&1)
        ec=$?
        if [ "$ec" -eq 1 ] && printf '%s' "$errout" | grep -qF "$msg"; then
            echo "regression: PASS ${base##*/} (rejected: $msg)"
        else
            echo "regression: FAIL ${base##*/} (exit=$ec, wanted err '$msg')"
            FAILED=$((FAILED+1))
        fi
        rm -rf "$TMP"
        continue
    fi

    # ws case: build, run, then run the websocket scenario checker
    if [ "$kind" = "ws" ]; then
        cp "$f" "$TMP/prog.hard"
        if ! (cd "$TMP" && "$HARD" build prog.hard) >/dev/null 2>&1; then
            echo "regression: FAIL ${base##*/} (build failed, wanted ok)"
            FAILED=$((FAILED+1))
            rm -rf "$TMP"
            continue
        fi
        "$TMP/.hard/prog" >/dev/null 2>&1 &
        PID=$!
        sleep 0.6
        if python3 "$DIR/ws_check.py" "$PORT" >/tmp/hs-ws-check.out 2>&1; then
            echo "regression: PASS ${base##*/} (ws scenario: $(cat /tmp/hs-ws-check.out))"
        else
            echo "regression: FAIL ${base##*/} (ws scenario: $(cat /tmp/hs-ws-check.out))"
            FAILED=$((FAILED+1))
        fi
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null
        srv_rc=$?
        if [ "$srv_rc" -ne 0 ]; then
            echo "regression: FAIL ${base##*/} (server exit rc=$srv_rc, sanitizer finding?)"
            FAILED=$((FAILED+1))
        fi
        rm -rf "$TMP"
        continue
    fi

    # esc case: escape-analysis report mode. First exp line is "esc";
    # the rest is the exact expected report (one line per classified local
    # binding plus the summary line), emitted by `hard build` under
    # HARD_ESCAPE_REPORT=1. The build must succeed (g++ clean) too.
    if [ "$kind" = "esc" ]; then
        cp "$f" "$TMP/prog.hard"
        out=$(cd "$TMP" && HARD_ESCAPE_REPORT=1 "$HARD" build prog.hard 2>&1)
        ec=$?
        if [ "$ec" -ne 0 ]; then
            echo "regression: FAIL ${base##*/} (build failed: $(echo "$out" | head -1))"
            FAILED=$((FAILED+1))
            rm -rf "$TMP"
            continue
        fi
        want=$(printf '%s\n' "${lines[@]:1}")
        if [ "$out" = "$want" ]; then
            echo "regression: PASS ${base##*/} ($(echo "$out" | tr '\n' ' '))"
        else
            echo "regression: FAIL ${base##*/} (report mismatch: got '$(echo "$out" | tr '\n' ' ')', want '$(echo "$want" | tr '\n' ' ')')"
            FAILED=$((FAILED+1))
        fi
        rm -rf "$TMP"
        continue
    fi

    # hir case: lower to HIR and compare the pretty printer output to the
    # expected snapshot (the rest of the file). Deterministic: lowering does
    # not depend on any external state, so the HIR text must be byte-identical.
    if [ "$kind" = "hir" ]; then
        cp "$f" "$TMP/prog.hard"
        out=$(cd "$TMP" && "$HARD" hir prog.hard 2>&1)
        ec=$?
        if [ "$ec" -ne 0 ]; then
            echo "regression: FAIL ${base##*/} (hir failed: $(echo "$out" | head -1))"
            FAILED=$((FAILED+1))
            rm -rf "$TMP"
            continue
        fi
        want=$(printf '%s\n' "${lines[@]:1}")
        if [ "$out" = "$want" ]; then
            echo "regression: PASS ${base##*/} ($(echo "$out" | head -1 | tr -d '\n'))"
        else
            echo "regression: FAIL ${base##*/} (hir mismatch)"
            FAILED=$((FAILED+1))
        fi
        rm -rf "$TMP"
        continue
    fi

    # opt case: optimize to a fixpoint and compare the full report (before/
    # after trees + per-pass stats). Deterministic, so byte-identical output.
    if [ "$kind" = "opt" ]; then
        cp "$f" "$TMP/prog.hard"
        out=$(cd "$TMP" && "$HARD" opt prog.hard 2>&1)
        ec=$?
        if [ "$ec" -ne 0 ]; then
            echo "regression: FAIL ${base##*/} (opt failed: $(echo "$out" | head -1))"
            FAILED=$((FAILED+1))
            rm -rf "$TMP"
            continue
        fi
        want=$(printf '%s\n' "${lines[@]:1}")
        if [ "$out" = "$want" ]; then
            echo "regression: PASS ${base##*/} ($(echo "$out" | sed -n 3p))"
        else
            echo "regression: FAIL ${base##*/} (opt mismatch)"
            FAILED=$((FAILED+1))
        fi
        rm -rf "$TMP"
        continue
    fi

    # ok case: build, run, assert routes
    cp "$f" "$TMP/prog.hard"
    if ! (cd "$TMP" && "$HARD" build prog.hard) >/dev/null 2>&1; then
        echo "regression: FAIL ${base##*/} (build failed, wanted ok)"
        FAILED=$((FAILED+1))
        rm -rf "$TMP"
        continue
    fi
    bin="$TMP/.hard/prog"
    if [ ! -x "$bin" ]; then
        echo "regression: FAIL ${base##*/} (binary missing)"
        FAILED=$((FAILED+1))
        rm -rf "$TMP"
        continue
    fi
    # make sure the test port is not held by a stale server
    pkill -f 'hs-reg-[0-9]*/.hard/prog' 2>/dev/null || true
    sleep 0.3
    "$bin" >/dev/null 2>&1 &
    PID=$!
    sleep 0.6
    for spec in "${lines[@]:1}"; do
        verb="${spec%% *}"                      # GET
        path="${spec#${verb} }"                 # /path
        path="${path%% = *}"
        want="${spec#* = }"
        got=""
        for _ in $(seq 1 20); do
            got=$(curl -s -m 2 -X "$verb" "http://127.0.0.1:$PORT$path" 2>/dev/null)
            [ -n "$got" ] && break
            sleep 0.3
        done
        if [ "$got" = "$want" ]; then
            echo "regression: PASS ${base##*/} $verb $path => $got"
        else
            echo "regression: FAIL ${base##*/} $verb $path => got '$got', want '$want'"
            FAILED=$((FAILED+1))
        fi
    done
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null
    srv_rc=$?
    if [ "$srv_rc" -ne 0 ]; then
        echo "regression: FAIL ${base##*/} (server exit rc=$srv_rc, sanitizer finding?)"
        FAILED=$((FAILED+1))
    fi
    rm -rf "$TMP"
done

# cpp kind: self-asserting C++ fixtures compiled directly against the runtime
# headers (used for the ms2.0 value-engine foundation). First exp line is
# "cpp"; the rest is the exact expected stdout (one line per entry).
ROOT="$(cd "$(dirname "$0")" && pwd)/.."
for f in "$DIR"/*.cpp; do
    [ -e "$f" ] || continue
    base="${f%.cpp}"
    exp="$base.exp"
    [ -f "$exp" ] || { echo "regression: missing $exp"; FAILED=$((FAILED+1)); continue; }

    lines=()
    while IFS= read -r l; do lines+=("$l"); done < "$exp"
    TOTAL=$((TOTAL+1))

    TMP=/tmp/hs-reg-cpp-$$
    rm -rf "$TMP"
    mkdir -p "$TMP"

    if ! g++ -std=c++17 -O1 -pthread -I "$ROOT/runtime" "$f" -o "$TMP/reg" 2>"$TMP/build.err"; then
        echo "regression: FAIL ${base##*/} (cpp compile failed: $(head -1 "$TMP/build.err"))"
        FAILED=$((FAILED+1))
        rm -rf "$TMP"
        continue
    fi

    if ! "$TMP/reg" >"$TMP/out" 2>&1; then
        echo "regression: FAIL ${base##*/} (assertion failed: $(head -1 "$TMP/out"))"
        FAILED=$((FAILED+1))
        rm -rf "$TMP"
        continue
    fi

    want=$(printf '%s\n' "${lines[@]:1}")
    got=$(cat "$TMP/out")
    if [ "$got" = "$want" ]; then
        echo "regression: PASS ${base##*/} ($(echo "$got" | tr '\n' ' '))"
    else
        echo "regression: FAIL ${base##*/} (stdout mismatch: got '$got', want '$want')"
        FAILED=$((FAILED+1))
    fi
    rm -rf "$TMP"
done

echo "regression: $((TOTAL-FAILED))/$TOTAL passed"
[ "$FAILED" -eq 0 ] || exit 1