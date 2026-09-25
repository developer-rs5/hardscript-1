#!/usr/bin/env bash
# 045 import-change: adding an import changes the module graph and the merged
# hash, so the build rebuilds with the new dependency set.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-045-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > main.hard <<'EOF'
bring http
app @3032

GET "/" :: { <- 1 }
EOF

"$HARD" build main.hard >/dev/null 2>&1 || { echo "045-import-change: cold build failed"; exit 1; }
n1=$(python3 -c 'import json;print(len(json.load(open(".hard/build.json"))["modules"]))')
[ "$n1" = "1" ] || { echo "045-import-change: expected 1 module before import, got $n1"; exit 1; }

# introduce a dependency: graph must grow, merged hash must change, rebuild.
cat > lib.hard <<'EOF'
calc one() => Int { <- 2 }
EOF
cat > main.hard <<'EOF'
bring "./lib"
bring http
app @3032

GET "/" :: { <- one() }
EOF
out=$("$HARD" build main.hard 2>&1)
[ "$?" -eq 0 ] || { echo "045-import-change: rebuild failed"; exit 1; }
echo "$out" | grep -q "0 hit, 2 miss, 0 skipped, 1 compiled" || {
    echo "045-import-change: summary mismatch: $out"; exit 1
}
n2=$(python3 -c 'import json;print(len(json.load(open(".hard/build.json"))["modules"]))')
[ "$n2" = "2" ] || { echo "045-import-change: expected 2 modules after import, got $n2"; exit 1; }
deps=$(python3 -c 'import json;m=json.load(open(".hard/build.json"))["modules"];print(m[1]["deps"])')
[ "$deps" = "['lib.hard']" ] || { echo "045-import-change: bad dep edge: $deps"; exit 1; }
( ./.hard/main >/dev/null 2>&1 & echo $! > pid )
sleep 0.7
got=$(curl -s -m 2 "http://127.0.0.1:3032/" 2>/dev/null)
kill "$(cat pid)" 2>/dev/null
[ "$got" = "2" ] || { echo "045-import-change: got '$got', want 2"; exit 1; }
echo "045-import-change: ok (graph 1 -> 2 modules, dep edge + route correct)"
exit 0