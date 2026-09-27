#!/usr/bin/env bash
# 047 multi-module-build: a project split across modules with nested imports
# builds into a single working binary (deps merged first, model shared once).
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-047-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
mkdir -p models
cat > models/user.hard <<'EOF'
model User = users [
    id => Int #id,
    name => Str,
]
EOF
cat > handlers.hard <<'EOF'
bring "./models/user"
calc greeting() => Str { <- "hi" }
EOF
cat > main.hard <<'EOF'
bring "./handlers"
bring http
app @3032

GET "/" :: { <- { greeting: greeting() } }
EOF

out=$("$HARD" build main.hard 2>&1)
[ "$?" -eq 0 ] || { echo "047-multi-module-build: build failed: $out"; exit 1; }
found=$(find .hard/cache/entries -name meta.json 2>/dev/null | wc -l)
[ "$found" -ge 3 ] || { echo "047-multi-module-build: expected >=3 cache entries, got $found"; exit 1; }
nmods=$(python3 -c 'import json;print(len(json.load(open(".hard/build.json"))["modules"]))')
[ "$nmods" = "3" ] || { echo "047-multi-module-build: expected 3 modules, got $nmods"; exit 1; }
( env -u PORT ./.hard/main >/dev/null 2>&1 & echo $! > pid )
sleep 0.8
got=$(curl -s -m 2 "http://127.0.0.1:3032/" 2>/dev/null)
# Wait for the port, not the pid: `kill` returns as soon as the signal is
# delivered, and a backgrounded child stays visible to `kill -0` as a zombie
# until the shell reaps it, which never happens here. The next scenario binds
# the same port, and a suite that fails on its neighbour's leftovers is worse
# than no suite.
# The port this scenario's own source pins; the suite's $PORT is a default,
# not what these programs listen on.
SCENARIO_PORT=$(sed -n 's/.*app @\([0-9]\+\).*/\1/p' main.hard | head -1)
: "${SCENARIO_PORT:=$PORT}"

port_free() {
    python3 - "$SCENARIO_PORT" <<'EOF'
import pathlib, sys
want = int(sys.argv[1])
for f in ("/proc/net/tcp", "/proc/net/tcp6"):
    try:
        rows = pathlib.Path(f).read_text().splitlines()[1:]
    except OSError:
        continue
    for row in rows:
        parts = row.split()
        if len(parts) < 4 or parts[3] != "0A":  # 0A = LISTEN
            continue
        if int(parts[1].split(":")[1], 16) == want:
            sys.exit(1)
sys.exit(0)
EOF
}
kill "$(cat pid)" 2>/dev/null
for _ in $(seq 1 60); do
    port_free && break
    sleep 0.1
done
[ "$got" = '{"greeting":"hi"}' ] || { echo "047-multi-module-build: got '$got'"; exit 1; }
echo "047-multi-module-build: ok (3 modules, nested import, correct route)"
exit 0