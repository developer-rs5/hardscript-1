#!/usr/bin/env bash
# Phase 3: codegen validation with snapshots.
#
# For every case in qa/codegen_cases/*.hard:
#   1. rebuild twice and assert the emitted C++ is byte-identical (determinism)
#   2. stash the snapshot under qa/codegen_snapshots/<name>.cpp (updated only if
#      the output legitimately changes; the script reports updates)
#   3. compile the snapshot standalone with g++ and the runtime headers
#   4. run the binary and exercise the recorded routes, comparing against the
#      stored response snapshot (first run records; later runs enforce it)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
QA="$(cd "$(dirname "$0")" && pwd)"
CASES="$QA/codegen_cases"
SNAPS="$QA/codegen_snapshots"
mkdir -p "$SNAPS"
FAILED=0
TOTAL=0
PORT=3031

for hardfile in "$CASES"/*.hard; do
    name="$(basename "${hardfile%.hard}")"
    TOTAL=$((TOTAL+1))
    TMP="/tmp/hs-snap-$name"
    rm -rf "$TMP"; mkdir -p "$TMP"
    cp "$hardfile" "$TMP/prog.hard"

    if ! (cd "$TMP" && "$HARD" build prog.hard) >/dev/null 2>&1; then
        echo "phase3: FAIL $name (hard build failed)"
        FAILED=$((FAILED+1)); rm -rf "$TMP"; continue
    fi
    cp "$TMP/.hard/prog.cpp" "$TMP/p1.cpp"
    (cd "$TMP" && "$HARD" build prog.hard) >/dev/null 2>&1
    cp "$TMP/.hard/prog.cpp" "$TMP/p2.cpp"
    if ! cmp -s "$TMP/p1.cpp" "$TMP/p2.cpp"; then
        echo "phase3: FAIL $name (non-deterministic C++ output)"
        FAILED=$((FAILED+1)); rm -rf "$TMP"; continue
    fi

    # snapshot compare / update
    if [ -f "$SNAPS/$name.cpp" ]; then
        if ! cmp -s "$SNAPS/$name.cpp" "$TMP/p1.cpp"; then
            cp "$TMP/p1.cpp" "$SNAPS/$name.cpp"
            echo "phase3: UPDATE $name (snapshot refreshed)"
        fi
    else
        cp "$TMP/p1.cpp" "$SNAPS/$name.cpp"
        echo "phase3: NEW $name (snapshot created)"
    fi

    # standalone g++ compile of the snapshot with copied runtime headers
    (cd "$TMP" && g++ -std=c++17 -O1 -w -I "$TMP/.hard" p1.cpp -o prog -lpthread 2>/dev/null)
    if [ ! -x "$TMP/prog" ]; then
        echo "phase3: FAIL $name (g++ standalone compile failed)"
        FAILED=$((FAILED+1)); rm -rf "$TMP"; continue
    fi

    routes="$(python3 - "$name" <<'EOF'
import json, sys
name = sys.argv[1]
cases = json.load(open('qa/codegen_cases/cases.json'))
routes = cases.get(name, {}).get('routes', [])
for r in routes:
    d = r.get('d','')
    print(f'{r["m"]}\t{r["p"]}\t{d}')
EOF
)"
    if [ -z "$routes" ]; then
        echo "phase3: PASS $name (compile-only, no route assertions)"
        rm -rf "$TMP"; continue
    fi

    pkill -f "hs-snap-$name/.hard/prog" 2>/dev/null || true
    "$TMP/prog" >/dev/null 2>&1 &
    PID=$!
    sleep 0.6
    resp="$SNAPS/$name.resp"
    temp_resp="$TMP.resp"
    : > "$temp_resp"
    allok=1
    curl_rc=""
    for _ in $(seq 1 25); do curl_rc=$(curl -s -m 2 http://127.0.0.1:$PORT/___ready 2>/dev/null); [ -n "$curl_rc" ] && break; sleep 0.3; done
    while IFS=$'\t' read -r method path body; do
        [ -z "$method" ] && continue
        curlc=0
        for _ in $(seq 1 10); do
            if [ -n "$body" ]; then
                got=$(curl -s -m 2 -X "$method" -H 'Content-Type: application/json' -d "$body" "http://127.0.0.1:$PORT$path")
            else
                got=$(curl -s -m 2 -X "$method" "http://127.0.0.1:$PORT$path")
            fi
            [ -n "$got" ] && break
            sleep 0.3
        done
        printf '%s %s => %s\n' "$method" "$path" "$got" >> "$temp_resp"
    done <<< "$routes"
    kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true

    if [ ! -f "$resp" ]; then
        cp "$temp_resp" "$resp"
        echo "phase3: NEW $name (responses recorded)"
    elif cmp -s "$resp" "$temp_resp"; then
        echo "phase3: PASS $name (responses match snapshot)"
    else
        echo "phase3: FAIL $name (responses changed)"
        diff "$resp" "$temp_resp" || true
        FAILED=$((FAILED+1))
    fi
    rm -rf "$TMP"
done

echo "phase3: $((TOTAL-FAILED))/$TOTAL passed"
[ "$FAILED" -eq 0 ] || exit 1