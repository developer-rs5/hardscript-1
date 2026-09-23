#!/usr/bin/env bash
# Phase 4.5 / Milestone 2 — cross-stack performance benchmark.
#
# Uniform methodology for HardScript, Node(Fastify), Bun, Go, Rust(Axum):
# identical REST API (GET /, GET /hello/:name, POST /echo) on 127.0.0.1:8080.
#
# Measures per stack: build time, binary size, cold-start (median of 5),
# idle RSS, throughput + p50/p95/p99 (conn-per-request lane for everyone,
# plus a keep-alive lane for the stacks that support it).
#
# Usage: qa/benchmark/run.sh   (writes qa/benchmark/results.json)
set -u
export PATH="$HOME/opt/go/bin:$PATH:/usr/local/go/bin"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
B="http://127.0.0.1:8080"
OUT="$ROOT/qa/benchmark/results.json"
: > "$OUT"

rss() { awk '/VmRSS:/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

ready() {
  python3 - <<'PY'
import socket, sys, time
for _ in range(4000):
    try:
        s=socket.create_connection(("127.0.0.1",8080),timeout=.5)
        s.sendall(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        s.recv(16); s.close(); sys.exit(0)
    except OSError:
        time.sleep(0.01)
raise SystemExit(1)
PY
}

cold_start_ms() { # <cmd...> spawn+wait for readiness, 5 runs, median
  vals=""
  for _ in 1 2 3 4 5; do
    t0=$(date +%s%3N)
    "$@" >/dev/null 2>&1 &
    pid=$!
    ready; rc=$?
    t1=$(date +%s%3N)
    [ "$rc" -eq 0 ] && vals="$vals $((t1 - t0))"
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    sleep 0.2
  done
  echo "$vals" | tr ' ' '\n' | grep -v '^$' | sort -n | awk '{a[NR]=$1} END{print (NR%2?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2)}'
}

bench_load() { # <keepalive>
  python3 "$ROOT/qa/benchmark/load.py" --duration 8 --warmup 3 --concurrency 32 --keepalive "$1"
}

collect() { # stack size build cold rss cpr_json ka_json
  STACK="$1" SZ="$2" BUILD="$3" COLD="$4" RSS="$5" CPR="$6" KA="$7" OUT="$OUT" python3 - <<'PY'
import json, os
r = {
  "stack": os.environ["STACK"],
  "build_s": os.environ["BUILD"],
  "bin_bytes": int(os.environ["SZ"] or 0),
  "cold_ms": float(os.environ["COLD"] or 0),
  "idle_rss_kb": int(os.environ["RSS"] or 0),
  "conn_per_request": json.loads(os.environ["CPR"]),
}
ka = os.environ["KA"]
if ka:
    r["keepalive"] = json.loads(ka)
with open(os.environ["OUT"], "a") as f:
    f.write(json.dumps(r) + "\n")
PY
}

echo "== build =="
cd "$ROOT/qa/benchmark/hs"
HS_LOG=$( "$ROOT/target/release/hard" bench app.hard 2>&1 )
HS_BIN="$ROOT/qa/benchmark/hs/.hard/app.release"
[ -f "$HS_BIN" ] || { echo "hs bench build failed: $HS_LOG"; exit 1; }
HS_SIZE=$(stat -c%s "$HS_BIN")
HS_BUILD=$(echo "$HS_LOG" | grep -oE 'release build in [0-9.]+s' | grep -oE '[0-9.]+' | head -1)
HS_BUILD="${HS_BUILD:-0}"

cd "$ROOT/qa/benchmark/node"
[ -d node_modules ] || npm install --silent >/dev/null 2>&1
N_SIZE=$(stat -c%s app.js); N_BUILD="n/a"
N_BIN="node $ROOT/qa/benchmark/node/app.js"

cd "$ROOT/qa/benchmark/bun"
B_SIZE=$(stat -c%s app.bun.js); B_BUILD="n/a"

cd "$ROOT/qa/benchmark/go"
GO_T0=$(date +%s%3N)
go build -o hs_bench_go . 
GO_T1=$(date +%s%3N); GO_BUILD=$((GO_T1 - GO_T0))
GO_SIZE=$(stat -c%s hs_bench_go)
GO_BIN="$ROOT/qa/benchmark/go/hs_bench_go"

cd "$ROOT/qa/benchmark/rust"
RU_T0=$(date +%s%3N)
cargo build --release 2>/dev/null
RU_T1=$(date +%s%3N); RU_BUILD=$((RU_T1 - RU_T0))
RU_BIN="$ROOT/qa/benchmark/rust/target/release/hs-bench-rust"
RU_SIZE=$(stat -c%s "$RU_BIN")

echo "== cold start (median of 5, ms) =="
HS_COLD=$(cold_start_ms env "$HS_BIN")
N_COLD=$(cold_start_ms $N_BIN)
B_COLD=$(cold_start_ms bun "$ROOT/qa/benchmark/bun/app.bun.js")
GO_COLD=$(cold_start_ms "$GO_BIN")
RU_COLD=$(cold_start_ms "$RU_BIN")

echo "== throughput (8s warmup + 8s measure, c=32) =="
run_stack() { # name cmd keepalive_supported
  local stack="$1"; shift
  local cmd="$1"; shift
  local ka_sup="$1"
  eval "$cmd >/dev/null 2>&1 &"
  local pid=$!
  sleep 1
  local rsskb; rsskb=$(rss "$pid")
  local cpr; cpr=$(bench_load 0)
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  local ka=""
  if [ "$ka_sup" = "1" ]; then
    eval "$cmd >/dev/null 2>&1 &"
    pid=$!
    sleep 1
    ka=$(bench_load 1)
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  fi
  printf '%s|%s|%s|%s\n' "$stack" "$rsskb" "$cpr" "$ka"
}

declare -A VMETA
VMETA[hs]="$HS_SIZE|$HS_BUILD|$HS_COLD|env $HS_BIN|0"
VMETA[node]="$N_SIZE|$N_BUILD|$N_COLD|$N_BIN|1"
VMETA[bun]="$B_SIZE|n/a|$B_COLD|bun $ROOT/qa/benchmark/bun/app.bun.js|1"
VMETA[go]="$GO_SIZE|$GO_BUILD|$GO_COLD|$GO_BIN|1"
VMETA[rust]="$RU_SIZE|$RU_BUILD|$RU_COLD|$RU_BIN|1"

res=$(mktemp)
for stack in hs node bun go rust; do
  IFS='|' read -r size build cold cmd ka_sup <<< "${VMETA[$stack]}"
  line=$(run_stack "$stack" "$cmd" "$ka_sup")
  IFS='|' read -r _ rss cpr ka <<< "$line"
  collect "$stack" "$size" "$build" "$cold" "$rss" "$cpr" "$ka"
done