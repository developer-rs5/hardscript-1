#!/usr/bin/env bash
# Phase 4.5 / Milestone 1 — Memory safety audit.
#
# Builds the runtime and all generated apps under AddressSanitizer,
# UndefinedBehaviorSanitizer and (via detect_leaks) LeakSanitizer, then runs:
#   A. the full Phase 4 runtime torture matrix (1000-req concurrency, 500 WS
#      clients, 1 MB body, crypto, jwt, fs) against the sanitized app,
#   B. the entire regression suite against sanitized binaries,
#   C. the integration suite (build/fmt/docs/bench + live probes),
#   D. targeted clean-exit LSan probes on the example apps.
#
# Exits non-zero if anything is reported. Sanitizer findings abort the run via
# halt_on_error so a FAIL here is a genuine finding (leaks, UAF, double-free, UB).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
HARD_DBG="$ROOT/target/debug/hard"
HARD_REL="$ROOT/target/release/hard"
SUPP="$ROOT/qa/phase45/lsan.supp"
SAN_FLAGS="-fsanitize=address,undefined -fno-sanitize-recover=all -g -O1 -fno-omit-frame-pointer"
export ASAN_OPTIONS="detect_leaks=1:halt_on_error=1:abort_on_error=1"
export LSAN_OPTIONS="suppressions=$SUPP:exitcode=23"
export UBSAN_OPTIONS="halt_on_error=1:print_stacktrace=1"

pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  ok   %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  FAIL %s\n' "$1"; }
check_nz() { [ "$2" -eq 0 ] && ok "$1" || bad "$1"; }
free_port() {
    local P pid
    for _ in $(seq 1 10); do
        pid=$(ss -ltnp 2>/dev/null | grep ":$1 " | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
        [ -z "$pid" ] && return 0
        kill -TERM "$pid" 2>/dev/null
        sleep 0.5
    done
    bad "port $1 still held after 5s"
}

echo "== M1: rebuild hard so the runtime headers are embedded =="
cargo build 2>&1 | tail -1 | sed 's/^/   debug: /'
cargo build --release 2>&1 | tail -1 | sed 's/^/  release: /'

echo "== A. Phase 4 matrix under ASan/UBSan/LSan =="
PH4_SANITIZE=1 bash qa/phase4_runtime.sh
[ $? -eq 0 ] && ok "phase4 matrix under sanitizers" || bad "phase4 matrix under sanitizers"

echo "== B. regression suite under sanitizers =="
SAN_BASE_HARD="$HARD_DBG" HARD="$ROOT/qa/phase45/san-hard.sh" bash tests/run-regressions.sh
[ $? -eq 0 ] && ok "regression suite under sanitizers" || bad "regression suite under sanitizers"

echo "== C. integration suite (normal build path) =="
bash tests/integration.sh
[ $? -eq 0 ] && ok "integration suite" || bad "integration suite"

echo "== D. clean-exit LSan probes (rest-api / chat / auth) =="
build_san() { # example-name
  local d="$1"
  (cd "examples/$d" && rm -rf .hard && timeout 90 "$HARD_DBG" build main.hard >/dev/null 2>&1)
  g++ -std=c++17 -pthread -I "examples/$d/.hard" $SAN_FLAGS \
      "examples/$d/.hard/main.cpp" -o "examples/$d/.hard/main.san"
}
probe_done() { # name rc http findings(logfile)
  if [ "$2" -eq 0 ] && [ "$3" != "000" ]; then
      if grep -qi 'LeakSanitizer: detected memory leaks\|ERROR: AddressSanitizer\|runtime error:\|DEADLYSIGNAL' "$4"; then
          bad "examples/$1 probe (sanitizer finding): $(grep -iE 'LeakSanitizer|AddressSanitizer|runtime error|DEADLYSIGNAL' "$4" | head -1)"
      else
          ok "examples/$1 probe rc=0 http=$3, no sanitizer findings"
      fi
  else
      bad "examples/$1 probe rc=$2 http=$3"
  fi
}

build_san rest-api
free_port 8080
bash -c 'cd '"$ROOT"'/examples/rest-api && exec ./.hard/main.san >/tmp/m1-rest.log 2>&1' &
SRV=$!; sleep 1.5
if ! grep -q listening /tmp/m1-rest.log 2>/dev/null; then bad "rest-api did not bind (log: $(head -1 /tmp/m1-rest.log))"; kill -TERM "$SRV" 2>/dev/null; else
code=$(curl -s -o /dev/null -m 4 -w '%{http_code}' http://127.0.0.1:8080/users 2>/dev/null)
kill -TERM "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; rc=$?
probe_done rest-api "$rc" "$code" /tmp/m1-rest.log
fi

build_san auth
free_port 9090
bash -c 'cd '"$ROOT"'/examples/auth && exec ./.hard/main.san >/tmp/m1-auth.log 2>&1' &
SRV=$!; sleep 1.5
if ! grep -q listening /tmp/m1-auth.log 2>/dev/null; then bad "auth did not bind (log: $(head -1 /tmp/m1-auth.log))"; kill -TERM "$SRV" 2>/dev/null; else
code=$(curl -s -o /dev/null -m 4 -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{"name":"probe","key":"x"}' http://127.0.0.1:9090/register 2>/dev/null)
kill -TERM "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; rc=$?
probe_done auth "$rc" "$code" /tmp/m1-auth.log
fi

build_san chat
free_port 9090
bash -c 'cd '"$ROOT"'/examples/chat && exec ./.hard/main.san >/tmp/m1-chat.log 2>&1' &
SRV=$!; sleep 1.5
if ! grep -q listening /tmp/m1-chat.log 2>/dev/null; then bad "chat did not bind (log: $(head -1 /tmp/m1-chat.log))"; kill -TERM "$SRV" 2>/dev/null; else
python3 - "$SRV" <<'PYEOF' >/tmp/m1-ws.out 2>&1
import os,base64,socket,sys,time
port=9090
s=socket.create_connection(("127.0.0.1",port),timeout=5)
key=base64.b64encode(os.urandom(16)).decode()
s.sendall(("GET /chat HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n\r\n"%key).encode())
r=s.recv(4096)
ok = b"101" in r
if ok:
    payload=b"ping"; mask=bytes(os.urandom(4))
    masked=bytes(b ^ mask[i%4] for i,b in enumerate(payload))
    s.sendall(bytes([0x81,0x80|len(payload)])+mask+masked)
    s.settimeout(3); buf=b""
    try:
        for _ in range(3):
            d=s.recv(4096)
            if not d: break
            buf+=d
            if b"pong" in buf: break
    except socket.timeout:
        pass
    print("ws101=%s pong=%s" % (ok, b"pong" in buf))
    sys.exit(0 if b"pong" in buf else 1)
print("ws101=false"); sys.exit(1)
PYEOF
code=$?; [ "$code" -eq 0 ] && code=200 || code=000
kill -TERM "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; rc=$?
probe_done chat "$rc" "$code" /tmp/m1-chat.log
grep -qiE 'runtime error|ERROR: AddressSanitizer' /tmp/m1-chat.log && bad "chat captured sanitizer finding" || ok "chat ws heartbeat ran clean"
fi

echo "== M1 summary: $pass passed, $fail failed =="
[ "$fail" -eq 0 ]