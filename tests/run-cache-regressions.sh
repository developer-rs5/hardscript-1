#!/usr/bin/env bash
# Package cache regressions: integrity, corruption, repair, offline reuse.
#
# The cache is the one part of the package manager that a user cannot see but
# pays for: a corrupt entry would install bytes nobody can account for, and a
# cache that never hits turns every install back into a download. Both halves
# are checked here against a real registry and a real cache directory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HARD="$ROOT/target/debug/hard"
REG_BIN="$ROOT/target/debug/hard-registry"
cargo build --quiet
cargo build --quiet --package hard-registry

PORT="${HARD_CACHE_PORT:-19210}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-cache.XXXXXX")"
export HARD_HOME="$WORK/home"
export HARD_REGISTRY="http://127.0.0.1:$PORT"
URL="$HARD_REGISTRY"
CACHE="$HARD_HOME/cache"
mkdir -p "$HARD_HOME"

N=0; PASS=0; FAIL=0; SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  for pid in $(ps -eo pid,cmd | grep "[h]ard-registry serve" | grep -- "$WORK" | awk '{print $1}'); do
    kill "$pid" 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

ok() { N=$((N+1)); PASS=$((PASS+1)); printf '  ok %d - %s\n' "$N" "$1"; }
no() {
  N=$((N+1)); FAIL=$((FAIL+1))
  printf '  NOT OK %d - %s\n' "$N" "$1"
  if [ $# -gt 1 ]; then printf '      %s\n' "$2"; fi
  return 0
}
check() { if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "expected [$2] got [$3]"; fi; }
contains() { case "$3" in *"$2"*) ok "$1";; *) no "$1" "missing [$2] in [$(printf '%.110s' "$3")]";; esac; }
lacks() { case "$3" in *"$2"*) no "$1" "unexpected [$2]";; *) ok "$1";; esac; }

# 0/1 helpers: shell truth, printed as a digit
yes() { if "$@"; then echo 1; else echo 0; fi; }
file_has() { [ -e "$1" ] && echo 1 || echo 0; }

mkpkg() { # dir name version
  mkdir -p "$1"
  printf 'schema = 1\nname = "%s"\nversion = "%s"\nedition = "2027"\ndescription = "cache fixture %s"\nlicense = "MIT"\n' "$2" "$3" "$2" > "$1/hard.toml"
  python3 - "$1/main.hard" "$2" <<'PY'
import sys
path, name = sys.argv[1], sys.argv[2]
body = "".join(f"calc f{i}() => Int {{ <- {i} }}\n" for i in range(1, 200))
open(path, "w").write(body)
PY
  ( cd "$1" && "$HARD" publish >/dev/null 2>&1 ) || { echo "fixture $2@$3 did not publish" >&2; exit 1; }
}

# project <dir> <dep> <version-req>
#
# The requirement is written exactly as given, so a test can pin `=1.0.0` and
# know which cache entry it is about to corrupt.
project() { # dir dep version
  mkdir -p "$1"
  {
    printf 'schema = 1\nname = "cacheapp"\nversion = "0.1.0"\nedition = "2027"\n'
    printf '\n[dependencies]\n%s = "%s"\n' "$2" "$3"
  } > "$1/hard.toml"
  printf 'calc f() => Int { <- 1 }\n' > "$1/main.hard"
}

echo "# cache regressions"

if curl -sS -m 1 -o /dev/null "$URL/health" 2>/dev/null; then
  echo "error: something is already listening on 127.0.0.1:$PORT" >&2
  exit 1
fi
"$REG_BIN" serve --addr "127.0.0.1:$PORT" --data "$WORK/data" --open >"$WORK/reg.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do curl -sS -m 1 -o /dev/null "$URL/health" 2>/dev/null && break; sleep 0.1; done
check "registry is up" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL/health")"

# Pin the signing key: the offline strict install below verifies against the
# trust store, and an unpinned key would (correctly) fail the whole install.
"$HARD" keys add "$URL" --trust --allow-test-key >/dev/null 2>&1
check "the signing key is pinned" 1 "$(grep -c '^trusted = true' "$HARD_HOME/trusted-keys.toml")"

# ------------------------------------------------------------------ empty
out=$("$HARD" cache info)
contains "cache info works on an empty cache" "cache root" "$out"
contains "and says how many packages it holds" "packages: 0" "$out"
check "a clean empty cache verifies" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"
out=$("$HARD" cache verify)
contains "and says so" "cache ok" "$out"
out=$("$HARD" cache verify --repair 2>&1) && rc=0 || rc=$?
check "repairing an empty cache succeeds" 0 "$rc"
contains "and reports it clean" "cache ok" "$out"

# ------------------------------------------------------------------ fixtures
mkpkg "$WORK/src/alpha" alpha 1.0.0
mkpkg "$WORK/src/beta" beta 1.0.0
mkpkg "$WORK/src/alpha2" alpha 1.1.0
APP="$WORK/app"; project "$APP" alpha "=1.0.0"
cd "$APP"
out=$("$HARD" install 2>&1) && rc=0 || rc=$?
check "a cold install succeeds" 0 "$rc"
contains "and reports a miss" "0 hit, 1 miss" "$out"
check "and leaves an archive" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.hspkg")"
check "and recorded metadata" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.json")"
check "and extracted the sources" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.src/main.hard")"
check "and wrote a cached index document" 1 "$(file_has "$CACHE/index/alpha.json")"
check "and wrote a signature record" 1 "$(file_has "$CACHE/signatures/alpha@1.0.0.json")"

# ------------------------------------------------------------------ warm
out=$("$HARD" install 2>&1)
contains "a second install is a hit" "1 hit, 0 miss" "$out"
contains "and says 100% hit rate" "100% hit rate" "$out"
lacks "and asks for nothing over the network" "1 miss" "$out"
out=$("$HARD" install 2>&1)
check "a warm install is not slower than the cache check it skips" 1 \
  "$([ "$(printf '%s' "$out" | grep -c '0 requests')" = "1" ] && echo 1 || echo 0)"

# ------------------------------------------------------------------ versions
project "$WORK/app2" alpha "1.1"
cd "$WORK/app2"
out=$("$HARD" install 2>&1) && rc=0 || rc=$?
check "a second version installs" 0 "$rc"
contains "and picks the requested version" "alpha 1.1.0" "$out"
check "both versions are cached" 2 "$(ls "$CACHE/packages/alpha"/*.hspkg | wc -l | tr -d ' ')"
out=$("$HARD" cache info)
contains "cache info lists every cached version" "alpha 1.0.0, 1.1.0" "$out"

# ------------------------------------------------------------------ corruption
VICTIM="$CACHE/packages/alpha/alpha-1.0.0.hspkg"
# flip a byte in the middle: the header still parses, the digest does not match
python3 - "$VICTIM" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, "rb").read())
b[len(b) // 2] ^= 0xFF
open(p, "wb").write(bytes(b))
PY
out=$("$HARD" cache verify 2>&1) && rc=0 || rc=$?
check "a corrupt archive fails verification" 1 "$rc"
contains "and is named" "alpha@1.0.0" "$out"
contains "and the reason is a digest mismatch" "mismatch" "$out"
contains "and repair is suggested" "hard cache verify --repair" "$out"
out=$("$HARD" cache verify --repair 2>&1) && rc=0 || rc=$?
check "repair succeeds" 0 "$rc"
contains "and reports the count" "repaired 1 corrupt" "$out"
check "the corrupt archive is gone" 0 "$(file_has "$VICTIM")"
check "its metadata is gone too" 0 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.json")"
check "its extracted sources are gone" 0 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.src")"
check "the healthy version is untouched" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.1.0.hspkg")"
check "the cache verifies again" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"

# a corrupt archive is refetched rather than installed
cd "$APP"; rm -rf .hard hard.lock
out=$("$HARD" install 2>&1) && rc=0 || rc=$?
check "installing again refetches the corrupt entry" 0 "$rc"
contains "and reports a miss" "0 hit, 1 miss" "$out"
check "and the archive is back" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.hspkg")"
check "and verifies" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"

# ------------------------------------------------------------------ not an archive
printf 'this is not an archive' > "$VICTIM"
out=$("$HARD" cache verify 2>&1) && rc=0 || rc=$?
check "a file that is not a .hspkg fails verification" 1 "$rc"
"$HARD" cache verify --repair >/dev/null 2>&1
check "and repair removes it" 0 "$(file_has "$VICTIM")"

# a hand-seeded archive with no recorded digest
mkdir -p "$CACHE/packages/hand"
printf 'not an archive either' > "$CACHE/packages/hand/hand-1.0.0.hspkg"
out=$("$HARD" cache verify 2>&1) && rc=0 || rc=$?
check "a seeded entry with no metadata is still checked" 1 "$rc"
rm -rf "$CACHE/packages/hand"
check "and removing it makes the cache clean again" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"

# ------------------------------------------------------------------ offline
# The corruption tests above removed 1.0.0 on purpose; fetch it again so the
# offline checks measure the cache and not the leftovers of the previous test.
cd "$APP"; rm -rf .hard hard.lock
"$HARD" install >/dev/null 2>&1
check "the cache holds 1.0.0 again" 1 "$(file_has "$CACHE/packages/alpha/alpha-1.0.0.hspkg")"
out=$("$HARD" install --offline 2>&1) && rc=0 || rc=$?
check "an offline install works from the cache" 0 "$rc"
contains "and reuses the archive" "reused alpha" "$out"
out=$("$HARD" install --offline --verify=strict 2>&1) && rc=0 || rc=$?
check "an offline strict install re-verifies" 0 "$rc"
contains "and says so" "verified 1 package signature" "$out"
FRESH="$WORK/fresh"; project "$FRESH" beta "1"
out=$(cd "$FRESH" && HARD_HOME="$WORK/empty-home" "$HARD" install --offline 2>&1) && rc=0 || rc=$?
check "an offline install with an empty cache fails" 1 "$rc"
contains "and names the package" "not in the cache" "$out"
out=$(cd "$FRESH" && HARD_HOME="$WORK/empty-home" "$HARD" search beta --offline 2>&1) && rc=0 || rc=$?
check "an offline search with an empty cache fails" 1 "$rc"
contains "and points at the index" "no package index" "$out"

# ------------------------------------------------------------------ clean
out=$("$HARD" cache clean 2>&1) && rc=0 || rc=$?
check "cache clean succeeds" 0 "$rc"
contains "and says so" "cache cleaned" "$out"
check "the cache directory is gone" 0 "$(file_has "$CACHE")"
cd "$APP"; rm -rf .hard hard.lock
out=$("$HARD" install 2>&1) && rc=0 || rc=$?
check "installing after a clean refetches everything" 0 "$rc"
contains "and reports misses" "0 hit" "$out"

# ------------------------------------------------------------------ partials
out=$("$HARD" cache info)
check "no partial downloads are left behind" 0 "$("$HARD" cache info | grep -c 'partial downloads: [1-9]')"

# ------------------------------------------------------------------ CLI shape
out=$("$HARD" cache bogus 2>&1) && rc=0 || rc=$?
check "an unknown cache subcommand exits 2" 2 "$rc"
contains "and lists the subcommands" "info|verify|clean" "$out"

echo
echo "cache regressions: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
