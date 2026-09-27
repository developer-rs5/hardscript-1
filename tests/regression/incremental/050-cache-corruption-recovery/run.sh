#!/usr/bin/env bash
# 050 cache-corruption-recovery: a corrupted cache entry degrades to a miss
# rather than failing the build, and the pristine cache is restored.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-050-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > main.hard <<'EOF'
bring http
app @3032

GET "/" :: { <- 1 }
EOF

"$HARD" build main.hard >/dev/null 2>&1 || { echo "050-corruption: cold build failed"; exit 1; }
entry=$(find .hard/cache/entries -name items.bin | head -1)
[ -n "$entry" ] || { echo "050-corruption: no cache entry"; exit 1; }
printf 'THIS IS NOT A VALID ASTSER BLOB\x00\xff' > "$entry"
rm -f .hard/main

out=$("$HARD" build main.hard 2>&1)
ec=$?
[ "$ec" -eq 0 ] || { echo "050-corruption: build failed after corrupt cache: $out"; exit 1; }
echo "$out" | grep -q "1 miss" || { echo "050-corruption: expected a miss after corruption: $out"; exit 1; }
# the binary still builds and serves
( env -u PORT ./.hard/main >/dev/null 2>&1 & echo $! > pid )
sleep 0.7
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
[ "$got" = "1" ] || { echo "050-corruption: got '$got'"; exit 1; }
echo "050-cache-corruption-recovery: ok (corrupt entry -> miss, build recovers)"
exit 0