#!/usr/bin/env bash
# 043 cache-miss: an edit invalidates the parse entry and forces a native
# recompile; the new C++ reflects the change.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-043-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > main.hard <<'EOF'
bring http
app @3032

GET "/" :: { <- 1 }
EOF

"$HARD" build main.hard >/dev/null 2>&1 || { echo "043-cache-miss: cold build failed"; exit 1; }

cat > main.hard <<'EOF'
bring http
app @3032

GET "/" :: { <- 2 }
EOF
out=$("$HARD" build main.hard 2>&1)
ec=$?
[ "$ec" -eq 0 ] || { echo "043-cache-miss: rebuild failed (exit=$ec)"; exit 1; }
echo "$out" | grep -q "0 hit, 1 miss, 0 skipped, 1 compiled" || {
    echo "043-cache-miss: summary mismatch: $out"; exit 1
}
grep -q 'Val::int_(2)' .hard/main.cpp || {
    echo "043-cache-miss: cpp did not reflect the new literal"; exit 1
}
echo "043-cache-miss: ok (edit -> 1 miss + recompile)"
exit 0