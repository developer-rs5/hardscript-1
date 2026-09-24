#!/usr/bin/env bash
# 049 manifest-fields: .hard/build.json carries the schema, env + merged
# hashes, module entries with deps, cache counters and stage timings.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-049-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
cat > lib.hard <<'EOF'
calc lv() => Int { <- 7 }
EOF
cat > main.hard <<'EOF'
bring "./lib"
bring http
app @3032

GET "/" :: { <- lv() }
EOF

"$HARD" build main.hard >/dev/null 2>&1 || { echo "049-manifest-fields: build failed"; exit 1; }

python3 - "$PWD" <<'PYEOF' || { echo "049-manifest-fields: manifest assert failed"; exit 1; }
import json, sys, os
m = json.load(open(os.path.join(sys.argv[1], ".hard/build.json")))
assert m["schema"] == "hard-build/v1", m["schema"]
assert len(m["env_fp"]) == 64
assert len(m["merged_hash"]) == 64
assert m["root"] == "main.hard"
assert m["cpp"].endswith(".hard/main.cpp")
assert [x["rel"] for x in m["modules"]] == ["lib.hard", "main.hard"]
depmap = {x["rel"]: x["deps"] for x in m["modules"]}
assert depmap["main.hard"] == ["lib.hard"], depmap
assert all(x["status"] in ("hit", "miss") for x in m["modules"])
assert m["cache"]["misses"] == 2 and m["cache"]["compiled"] == 1, m["cache"]
t = m["timings_ms"]
for k in ("discover", "parse", "merge", "optimize", "typecheck", "codegen", "native", "total"):
    assert k in t, k
assert t["total"] > 0
PYEOF
echo "049-manifest-fields: ok (schema, hashes, deps, counts, timings present)"
exit 0