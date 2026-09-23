#!/usr/bin/env bash
# Regression suite for bugs found during the v0.1 QA torture test.
#
# Each pair of files in tests/regression/ describes one reproducible bug:
#   NNN-description.hard   -> minimal repro program
#   NNN-description.exp    -> expected behavior:
#       ok                    build must succeed (exit 0)
#         GET /path = BODY    then run the server and assert the route body
#       err\nMSG:<substr>     build must fail (exit 1) with this message text
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
        wait "$PID" 2>/dev/null || true
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
    wait "$PID" 2>/dev/null || true
    rm -rf "$TMP"
done

echo "regression: $((TOTAL-FAILED))/$TOTAL passed"
[ "$FAILED" -eq 0 ] || exit 1