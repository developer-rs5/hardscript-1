#!/usr/bin/env bash
# 046 cycle-detection: circular `bring`s are rejected with a graph diagnostic
# that names the cycle, even though the incremental cache exists.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-046-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > a.hard <<'EOF'
bring "./b"
EOF
cat > b.hard <<'EOF'
bring "./a"
EOF
cat > main.hard <<'EOF'
bring "./a"
EOF

out=$("$HARD" build main.hard 2>&1)
ec=$?
[ "$ec" -eq 1 ] || { echo "046-cycle-detection: expected failure, exit=$ec"; exit 1; }
echo "$out" | grep -q "import cycle detected" || { echo "046-cycle-detection: no cycle message: $out"; exit 1; }
echo "$out" | grep -q "a.hard" || { echo "046-cycle-detection: cycle path missing a.hard"; exit 1; }
echo "046-cycle-detection: ok (cycle rejected via module graph)"
exit 0