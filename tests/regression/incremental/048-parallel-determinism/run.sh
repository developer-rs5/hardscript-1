#!/usr/bin/env bash
# 048 parallel-determinism: `-j 1` and `-j 8` must produce byte-identical C++,
# identical merged hashes and identical module orders.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-048-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1
for i in $(seq 0 19); do
    printf 'calc m%02d() => Int { <- %02d }\n' "$i" "$i" > "m$i.hard"
done
{
    for i in $(seq 0 19); do echo "bring \"./m$i\""; done
    printf 'bring http\napp @3032\n\nGET "/" :: { <- { n: m00() } }\n'
} > main.hard

"$HARD" build -j 1 main.hard >/dev/null 2>&1 || { echo "048-parallel-determinism: -j1 build failed"; exit 1; }
cp .hard/main.cpp /tmp/hs-inc-048-j1.cpp
h1=$(python3 -c 'import json;print(json.load(open(".hard/build.json"))["merged_hash"])')

rm -rf .hard
"$HARD" build --jobs 8 main.hard >/dev/null 2>&1 || { echo "048-parallel-determinism: -j8 build failed"; exit 1; }
cmp -s /tmp/hs-inc-048-j1.cpp .hard/main.cpp || { echo "048-parallel-determinism: cpp differs between -j1 and -j8"; exit 1; }
h2=$(python3 -c 'import json;print(json.load(open(".hard/build.json"))["merged_hash"])')
[ "$h1" = "$h2" ] || { echo "048-parallel-determinism: merged hash differs"; exit 1; }
echo "048-parallel-determinism: ok (byte-identical regardless of jobs)"
exit 0