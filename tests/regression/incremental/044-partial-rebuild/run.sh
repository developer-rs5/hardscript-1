#!/usr/bin/env bash
# 044 partial-rebuild: with several modules, editing one reparses only that
# module (all others keep cache hits) yet the merged program still rebuilds.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-044-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > a.hard <<'EOF'
calc av() => Int { <- 1 }
EOF
cat > b.hard <<'EOF'
calc bv() => Int { <- av() + 10 }
EOF
cat > main.hard <<'EOF'
bring "./a"
bring "./b"
bring http
app @3032

GET "/" :: { <- { a: av(), b: bv() } }
EOF

"$HARD" build main.hard >/dev/null 2>&1 || { echo "044-partial-rebuild: cold build failed"; exit 1; }

cat > b.hard <<'EOF'
calc bv() => Int { <- av() + 20 }
EOF
out=$("$HARD" build main.hard 2>&1)
ec=$?
[ "$ec" -eq 0 ] || { echo "044-partial-rebuild: rebuild failed (exit=$ec)"; exit 1; }
echo "$out" | grep -q "2 hit, 1 miss, 0 skipped, 1 compiled" || {
    echo "044-partial-rebuild: expected 2 hit/1 miss, got: $out"; exit 1
}
# and the binary still answers correctly
( ./.hard/main >/dev/null 2>&1 & echo $! > pid )
sleep 0.7
got=$(curl -s -m 2 "http://127.0.0.1:3032/" 2>/dev/null)
kill "$(cat pid)" 2>/dev/null
[ "$(echo "$got" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["b"])' 2>/dev/null)" = "21" ] || {
    echo "044-partial-rebuild: route wrong after partial rebuild: $got"; exit 1
}
echo "044-partial-rebuild: ok (2 hits, 1 miss, merged result correct)"
exit 0