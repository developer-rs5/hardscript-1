#!/usr/bin/env bash
# 042 cache-hit: the second build of an unchanged project reuses the cache and
# skips the native stage; C++ output stays byte-identical.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-042-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > main.hard <<'EOF'
bring http
app @3032

GET "/" :: { <- { hello: "world" } }
EOF

if ! "$HARD" build main.hard >/dev/null 2>&1; then
    echo "042-cache-hit: cold build failed"; exit 1
fi
cp .hard/main.cpp p1.cpp
out2=$("$HARD" build main.hard 2>&1)
ec2=$?
if [ "$ec2" -ne 0 ]; then
    echo "042-cache-hit: warm build failed (exit=$ec2)"; exit 1
fi
echo "$out2" | grep -q "1 hit, 0 miss, 1 skipped, 0 compiled" || {
    echo "042-cache-hit: summary mismatch: $out2"; exit 1
}
cmp -s p1.cpp .hard/main.cpp || { echo "042-cache-hit: cpp changed"; exit 1; }
[ -f .hard/build.json ] || { echo "042-cache-hit: manifest missing"; exit 1; }
echo "042-cache-hit: ok (warm build reused cache, cpp identical)"
exit 0