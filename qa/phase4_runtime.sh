#!/usr/bin/env bash
# Phase 4 QA: runtime torture on the generated runtime_torture.hard app.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD="$ROOT/target/release/hard"
APP="$ROOT/qa/runtime_torture.hard"
GEN="$ROOT/qa/gen_runtime_torture.py"
TMP="$ROOT/.hard"
PORT=3031
BASE="http://127.0.0.1:$PORT"
pass=0; fail=0
ok()  { pass=$((pass+1)); printf '  ok   %s\n' "$1"; }
bad() { fail=$((fail+1)); printf '  FAIL %s\n' "$1"; }
check() { # name expected actual
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 [expected=$2 got=$3]"; fi
}

echo "== Phase 4: runtime torture =="
pkill -f 'runtime_torture$' 2>/dev/null; sleep 0.3
python3 "$GEN" >/dev/null
rm -rf "$TMP"
( cd "$ROOT" && "$HARD" build "$APP"; ) >/dev/null 2>&1 || { echo "build failed"; exit 1; }

if [ "${PH4_SANITIZE:-0}" = "1" ]; then
    echo "== sanitizer recompile (ASan/UBSan) =="
    export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=1:halt_on_error=1:abort_on_error=1}"
    export UBSAN_OPTIONS="${UBSAN_OPTIONS:-halt_on_error=1:print_stacktrace=1}"
    g++ -std=c++17 -pthread -I "$ROOT/.hard" \
        -fsanitize=address,undefined -fno-sanitize-recover=all -g -O1 \
        -fno-omit-frame-pointer \
        "$ROOT/.hard/runtime_torture.cpp" -o "$ROOT/.hard/runtime_torture" || { echo "sanitize build failed"; exit 1; }
fi

"$ROOT/.hard/runtime_torture" >"$ROOT/.hard/srv.log" 2>&1 &
SRV=$!
sleep 1.5

echo "-- routes --"
check "/ root"      "root"   "$(curl -s -m3 "$BASE/")"
check "/r0"         0        "$(curl -s -m3 "$BASE/r0")"
check "/r99"        99       "$(curl -s -m3 "$BASE/r99")"
check "/u/42"       '{"id":"42"}' "$(curl -s -m3 "$BASE/u/42")"
check "/sum ab+cd"  '{"s":"abcd"}' "$(curl -s -m3 "$BASE/sum/ab/cd")"
check "unknown->404" 404     "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/nope")"
check "PUT->404"     404     "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$BASE/r0")"
check "GET/POST /echo" '{"x":7}' "$(curl -s -m3 -X POST -d '{"x":7}' "$BASE/echo")"
check "/elev x"      7        "$(curl -s -m3 -X POST -d '{"x":7}' "$BASE/elev")"

echo "-- fs roundtrip --"
FS=$(curl -s -m3 "$BASE/fs")
check "fs rd"    'hello world' "$(python3 -c "import json,sys;print(json.loads('''$FS''')['rd'])")"
check "fs sz=11" 11 "$(python3 -c "import json,sys;print(json.loads('''$FS''')['sz'])")"
check "fs ex true" True "$(python3 -c "import json,sys;print(json.loads('''$FS''')['ex'])")"
check "fs big=4194304" 4194304 "$(python3 -c "import json,sys;print(json.loads('''$FS''')['big'])")"
[ -e /tmp/hs_rt_test.txt ] && bad "fs remove left file" || ok "fs remove cleaned"

echo "-- crypto vectors --"
python3 - "$BASE" <<'PYEOF' && ok "crypto vectors" || bad "crypto vector mismatch"
import hashlib,hmac,sys,urllib.request,json
base=sys.argv[1]
def g(p): return urllib.request.urlopen(base+"/"+p, timeout=5).read().decode()
v={"sha":hashlib.sha256(b"abc").hexdigest(),"sha0":hashlib.sha256(b"").hexdigest(),
   "sha-a":hashlib.sha256(b"a").hexdigest(),"shafox":hashlib.sha256(b"The quick brown fox jumps over the lazy dog").hexdigest(),
   "shalong":hashlib.sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").hexdigest(),
   "sha1abc":hashlib.sha1(b"abc").hexdigest(),"md5abc":hashlib.md5(b"abc").hexdigest()}
for k,want in v.items():
    got=g(k)
    if got!=want:
        print("mismatch",k,got,want); raise SystemExit(1)
got=urllib.request.urlopen(base+"/hmac",timeout=5).read().hex()
want=hmac.new(b"key",b"The quick brown fox jumps over the lazy dog",hashlib.sha256).hexdigest()
if got!=want:
    print("mismatch hmac",got,want); raise SystemExit(1)
PYEOF

echo "-- jwt --"
TOK=$(curl -s -m3 "$BASE/jwt")
check "jwtv ok"  true  "$(curl -s -m3 "$BASE/jwtv?t=$TOK")"
check "jwtv tamper" false "$(curl -s -m3 "$BASE/jwtv?t=bad.header.badsig")"
check "jwt is 3 parts" 3 "$(echo "$TOK" | tr '.' '\n' | grep -c .)"

echo "-- large body (1MB) --"
python3 - "$BASE" <<'PYEOF' && ok "1MB echo roundtrip" || bad "1MB echo mismatch"
import json,sys,urllib.request,random
random.seed(7)
value="".join(chr(97+random.randrange(26)) for _ in range(1024*1024))
body=json.dumps({"s":value}).encode()
req=urllib.request.Request(sys.argv[1]+"/echo", data=body, headers={"Content-Type":"application/json"})
resp=json.loads(urllib.request.urlopen(req, timeout=30).read())
sys.exit(0 if resp.get("s")==value else 1)
PYEOF

echo "-- malformed body -> 500 --"
check "bad json ->500" 500 "$(curl -s -o /dev/null -m5 -w '%{http_code}' -X POST -d 'not json at all' "$BASE/echo")"

echo "-- concurrency 50x20 --"
python3 - "$BASE" <<'PYEOF' && ok "concurrency 50x20=1000" || bad "concurrency failed"
import sys,urllib.request,concurrent.futures
base=sys.argv[1]
def one(i):
    try:
        r=urllib.request.urlopen(base+"/r%d"%(i%100), timeout=10)
        return r.status, r.read().decode()
    except Exception as e:
        return 0, str(e)
with concurrent.futures.ThreadPoolExecutor(50) as ex:
    res=list(ex.map(one, range(1000)))
hit=sum(1 for r in res if r[0]==200)
if hit != 1000:
    print("only", hit, "of 1000 got 200"); sys.exit(1)
PYEOF

echo "-- websocket 500 clients --"
python3 "$ROOT/qa/phase4_ws.py" "$PORT" 500 && ok "ws 500 clients echo" || bad "ws echo failure"

kill $SRV 2>/dev/null
wait $SRV 2>/dev/null
srv_rc=$?
[ "$srv_rc" -eq 0 ] && ok "server clean exit (rc=0)" || bad "server exit rc=$srv_rc (sanitizer finding?)"
echo "== Phase 4: $pass passed, $fail failed =="
[ "$fail" -eq 0 ]