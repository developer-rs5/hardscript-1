#!/usr/bin/env bash
# HardScript v0.1 integration smoke tests.
#
# Verifies the full developer workflow end-to-end:
#   build -> run -> curl -> test -> fmt --check -> docs -> bench
#
# Usage:   tests/integration.sh
# Output:  one line per check; non-zero exit on the first failure.

set -u
cd "$(dirname "$0")/.."

ROOT="$(pwd)"
HARD="$ROOT/target/debug/hard"

PASS=0
FAIL=0
fail() {
    echo "FAIL $1"
    FAIL=$((FAIL + 1))
}
ok() {
    echo "ok   $1"
    PASS=$((PASS + 1))
}
check() {
    local name="$1"
    shift
    if "$@" >/dev/null 2>&1; then ok "$name"; else fail "$name"; fi
}

echo "== cargo build"
check "cargo build" cargo build

for d in hello-world rest-api chat auth postgres; do
    check "build examples/$d" bash -c "cd '$ROOT/examples/$d' && timeout 90 '$HARD' build main.hard"
done

check "hello-world test" bash -c "cd '$ROOT/examples/hello-world' && timeout 40 '$HARD' test"
check "rest-api test" bash -c "cd '$ROOT/examples/rest-api' && timeout 40 '$HARD' test"
check "chat test" bash -c "cd '$ROOT/examples/chat' && timeout 40 '$HARD' test"
check "auth test" bash -c "cd '$ROOT/examples/auth' && timeout 40 '$HARD' test"

for d in hello-world rest-api chat auth postgres; do
    check "fmt --check examples/$d" bash -c "cd '$ROOT/examples/$d' && timeout 30 '$HARD' fmt --check"
done

check "docs examples/rest-api" bash -c "cd '$ROOT/examples/rest-api' && timeout 30 '$HARD' docs"
check "rest-api API.md generated" test -f "$ROOT/examples/rest-api/API.md"
check "bench examples/rest-api" bash -c "cd '$ROOT/examples/rest-api' && timeout 60 '$HARD' bench"

echo "== live server checks (rest-api on :8080)"
(cd "$ROOT/examples/rest-api" && timeout 15 ./.hard/main >/tmp/hs_srv.log 2>&1) &
SRV=$!
sleep 1
GET=$(curl -s -m 5 http://localhost:8080/users)
if [ -n "$GET" ]; then ok "GET /users returns body"; else fail "GET /users returns body"; fi
case "$GET" in
    *\[*\]*) ok "GET /users is a JSON list" ;;
    *) fail "GET /users is a JSON list" ;;
esac
POST=$(curl -s -m 5 -X POST http://localhost:8080/users -d '{"name":"linus"}')
case "$POST" in
    *201*) ok "POST /users reflects status 201" ;;
    *) fail "POST /users reflects status 201" ;;
esac
kill "$SRV" 2>/dev/null
wait "$SRV" 2>/dev/null

echo
echo "integration: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
exit $?