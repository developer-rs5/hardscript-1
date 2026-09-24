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
( ./.hard/main >/dev/null 2>&1 & echo $! > pid )
sleep 0.7
got=$(curl -s -m 2 "http://127.0.0.1:3032/" 2>/dev/null)
kill "$(cat pid)" 2>/dev/null
[ "$got" = "1" ] || { echo "050-corruption: got '$got'"; exit 1; }
echo "050-cache-corruption-recovery: ok (corrupt entry -> miss, build recovers)"
exit 0