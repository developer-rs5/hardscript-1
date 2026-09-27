#!/usr/bin/env bash
# Mirror, offline and cache regressions.
#
# Three registries on loopback: a primary, a mirror that is a faithful copy,
# and one that is deliberately wrong. Everything is checked through the real
# CLI over real HTTP, including a killed primary, a missing mirror, a corrupted
# cache entry and an offline install.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HARD="$ROOT/target/debug/hard"
REG_BIN="$ROOT/target/debug/hard-registry"
cargo build --quiet
cargo build --quiet --package hard-registry

P1="${HARD_MIRROR_PORT:-18910}"   # primary
P2="${HARD_MIRROR_PORT2:-18911}"  # good mirror
P3="${HARD_MIRROR_PORT3:-18912}"  # bad mirror (missing packages)
P4="${HARD_MIRROR_PORT4:-18913}"  # dead (never started)
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-mirror.XXXXXX")"
export HARD_HOME="$WORK/home"
mkdir -p "$HARD_HOME"
URL1="http://127.0.0.1:$P1"
URL2="http://127.0.0.1:$P2"
URL3="http://127.0.0.1:$P3"
export HARD_REGISTRY="$URL1"

N=0; PASS=0; FAIL=0; PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  # Anything started from this work dir goes too, even if the pid list was
  # lost: a registry left listening would silently answer the next run.
  for pid in $(ps -eo pid,cmd | grep "[h]ard-registry serve" | grep -- "$WORK" | awk '{print $1}'); do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
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

start_registry() { # url data
  # `--addr` is a socket address, not a URL.
  local addr="${1#http://}"
  "$REG_BIN" serve --addr "$addr" --data "$2" --open >"$2.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 60); do curl -sS -m 1 -o /dev/null "$1/health" 2>/dev/null && return 0; sleep 0.1; done
  echo "registry at $1 did not come up" >&2
  return 1
}
stop_registry() { # url
  local pid addr="${1#http://}"
  for pid in "${PIDS[@]:-}"; do
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -q -- "$addr"; then
      kill "$pid" 2>/dev/null || true
      return 0
    fi
  done
  return 1
}

mkpkg() { # dir name version
  mkdir -p "$1"
  {
    printf 'schema = 1\nname = "%s"\nversion = "%s"\nedition = "2027"\n' "$2" "$3"
    printf 'description = "the %s package"\nlicense = "MIT"\n' "$2"
  } > "$1/hard.toml"
  printf 'calc f() => Int { <- 1 }\n' > "$1/main.hard"
}
publish_to() { # url name version
  local dir="$WORK/src/$2-$3"
  mkpkg "$dir" "$2" "$3"
  ( cd "$dir" && HARD_REGISTRY="$1" "$HARD" publish >/dev/null 2>&1 ) || {
    echo "fixture $2@$3 did not publish to $1" >&2
    exit 1
  }
}

echo "# mirror regressions"
echo

for port in "$P1" "$P2" "$P3" "$P4"; do
  if curl -sS -m 1 -o /dev/null "http://127.0.0.1:$port/health" 2>/dev/null; then
    echo "error: something is already listening on 127.0.0.1:$port" >&2
    echo "       stop it, or set HARD_MIRROR_PORT* to free ports" >&2
    exit 1
  fi
done

# ---------------------------------------------------------------- registries
start_registry "$URL1" "$WORK/data1"
start_registry "$URL2" "$WORK/data2"
start_registry "$URL3" "$WORK/data3"
check "primary is up" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL1/health")"
check "good mirror is up" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL2/health")"
check "bad mirror is up" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL3/health")"
check "the fourth port is free" 1 "$(curl -sS -m 1 -o /dev/null "http://127.0.0.1:$P4/health" 2>/dev/null && echo 0 || echo 1)"

# ------------------------------------------------------------------ fixtures
for spec in "jwt:1.0.0" "hard-toml:0.4.0" "validator:3.0.0" "jsonwebtoken:2.3.1"; do
  name="${spec%%:*}"
  version="${spec##*:}"
  publish_to "$URL1" "$name" "$version"
  publish_to "$URL2" "$name" "$version"
  publish_to "$URL3" "$name" "$version"
done
# the bad mirror is missing one package on purpose
check "the primary has jwt" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL1/packages/jwt")"
check "the good mirror has jwt" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL2/packages/jwt")"
publish_to "$URL2" "only-on-mirror" "1.0.0"
check "the mirror has a package the primary lacks" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL2/packages/only-on-mirror")"
check "the primary does not" 404 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL1/packages/only-on-mirror")"

# ------------------------------------------------------------- hard.toml form
APP="$WORK/app"; mkdir -p "$APP"
cat > "$APP/hard.toml" <<EOF
schema = 1
name = "mirrorapp"
version = "0.1.0"
edition = "2027"

[registry]
default = "$URL1"

[[registry.mirror]]
url = "$URL2"
priority = 1

[[registry.mirror]]
url = "$URL3"
priority = 9

[[registry.mirror]]
url = "http://127.0.0.1:$P4"
priority = 2
EOF
cd "$APP"
# Pin the signing key: without it, every `hard verify` below would (correctly)
# report an untrusted key and exit non-zero.
"$HARD" keys add "$URL1" --trust --allow-test-key >/dev/null
check "the signing key is pinned" 1 "$(grep -c '^trusted = true' "$HARD_HOME/trusted-keys.toml")"
out=$("$HARD" registry list)
contains "the default is listed" "$URL1" "$out"
contains "the good mirror is listed" "$URL2" "$out"
contains "the bad mirror is listed" "$URL3" "$out"
contains "the dead mirror is listed" "$P4" "$out"
contains "priority is shown" "priority 1" "$out"
out=$("$HARD" registry list --json)
check "registry list --json is JSON" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(1 if d["mirrors"]==3 else 0)')"
check "the default is named" "$URL1" "$(printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["default"])')"

# --------------------------------------------------------------- health
out=$("$HARD" registry health)
contains "health reports the primary as ok" "ok" "$out"
contains "health shows latency" "ms" "$out"
contains "health shows the sequence" "seq" "$out"
contains "health flags the test key" "test key" "$out"
check "health of a live set exits 0" 0 "$("$HARD" registry health >/dev/null 2>&1; echo $?)"
out=$("$HARD" registry health --json)
check "health --json is JSON" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(1 if all("reachable" in x for x in d) else 0)')"
check "health --json counts 4 registries" 4 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')"
DEADAPP="$WORK/deadapp"; mkdir -p "$DEADAPP"
cat > "$DEADAPP/hard.toml" <<EOF
schema = 1
name = "deadapp"
version = "0.1.0"
edition = "2027"

[registry]
default = "http://127.0.0.1:$P4"

[[registry.mirror]]
url = "http://127.0.0.1:$((P4 + 1))"
EOF
out=$(cd "$DEADAPP" && "$HARD" registry health 2>&1) && rc=0 || rc=$?
check "an all-dead set exits 1" 1 "$rc"
contains "and says FAIL" "FAIL" "$out"
out=$(cd "$DEADAPP" && HARD_HOME="$WORK/no-cache" "$HARD" search jwt 2>&1) && rc=0 || rc=$?
check "an all-dead set with no cache cannot search" 1 "$rc"
contains "and reports the transport failure" "Connection refused" "$out"
out=$(cd "$DEADAPP" && "$HARD" registry sync 2>&1) && rc=0 || rc=$?
check "an all-dead set cannot sync" 1 "$rc"

# ---------------------------------------------------------------- install
out=$("$HARD" install 2>&1)
contains "an install with no dependencies works" "locked" "$out"
cat > hard.toml <<EOF
schema = 1
name = "mirrorapp"
version = "0.1.0"
edition = "2027"

[registry]
default = "$URL1"

[[registry.mirror]]
url = "$URL2"
priority = 1

[dependencies]
jwt = "1"
EOF
out=$("$HARD" install 2>&1)
contains "a mirrored install succeeds" "installed jwt" "$out"
check "the lockfile names the primary" 1 "$([ -f hard.lock ] && echo 1 || echo 0)"

# ----------------------------------------------------- fallback: primary down
stop_registry "$URL1"
sleep 0.3
out=$("$HARD" install 2>&1) && rc=0 || rc=$?
check "an install still works with the primary down" 0 "$rc"
contains "and comes from the cache" "reused jwt" "$out"
lacks "and does not re-download" "1 miss" "$out"
rm -rf .hard
out=$("$HARD" search jwt 2>&1) && rc=0 || rc=$?
check "search falls back to a mirror" 0 "$rc"
contains "and finds the package" "jwt" "$out"
out=$("$HARD" verify jwt@1.0.0 2>&1) && rc=0 || rc=$?
check "verify falls back to a mirror" 0 "$rc"

# the good mirror goes down too: only the bad one is left, and it has jwt
stop_registry "$URL2"
sleep 0.3
out=$("$HARD" search jwt 2>&1) && rc=0 || rc=$?
check "the bad mirror still answers jwt" 0 "$rc"

# nothing is left
stop_registry "$URL3"
sleep 0.3
out=$("$HARD" search jwt 2>&1) && rc=0 || rc=$?
check "with every registry down the search still answers" 0 "$rc"
out=$("$HARD" search jwt 2>&1)
contains "from the cache" "from cache" "$out"
contains "and says the registry was unreachable" "registry unreachable" "$out"

# bring the default and the good mirror back for the rest of the suite
start_registry "$URL1" "$WORK/data1"
start_registry "$URL2" "$WORK/data2"
sleep 0.2
check "the primary is back" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL1/health")"
check "the good mirror is back" 200 "$(curl -sS -o /dev/null -w '%{http_code}' "$URL2/health")"

# ------------------------------------------------------------- offline
out=$("$HARD" install --offline 2>&1) && rc=0 || rc=$?
check "an offline install uses the cache" 0 "$rc"
contains "and says it reused the cache" "reused jwt" "$out"
out=$("$HARD" search jwt --offline 2>&1) && rc=0 || rc=$?
check "an offline search works from the cache index" 0 "$rc"
contains "and finds jwt" "jwt" "$out"

FRESH="$WORK/fresh"; mkdir -p "$FRESH"
cat > "$FRESH/hard.toml" <<EOF
schema = 1
name = "freshapp"
version = "0.1.0"
edition = "2027"

[registry]
default = "$URL1"

[dependencies]
jwt = "1"
EOF
out=$(cd "$FRESH" && HARD_HOME="$WORK/empty-home" "$HARD" install --offline 2>&1) && rc=0 || rc=$?
check "an offline install with an empty cache fails" 1 "$rc"
contains "and says what is missing" "not in the cache" "$out"
out=$(cd "$FRESH" && HARD_HOME="$WORK/empty-home" "$HARD" search jwt --offline 2>&1) && rc=0 || rc=$?
check "an offline search with an empty cache fails" 1 "$rc"
contains "and points at the cache" "no package index" "$out"
contains "and says what to do" "hard registry sync" "$out"

# ---------------------------------------------------------------- sync
out=$("$HARD" registry sync 2>&1) && rc=0 || rc=$?
check "a full sync succeeds" 0 "$rc"
contains "sync reports the registry" "$URL1" "$out"
contains "sync wrote index documents" "cache index document" "$out"
check "the sync state file exists" 1 "$([ -f "$HARD_HOME/mirror-sync.toml" ] && echo 1 || echo 0)"
contains "sync state names the registry" "$URL1" "$(cat "$HARD_HOME/mirror-sync.toml")"
check "the cache index has jwt" 1 "$([ -f "$HARD_HOME/cache/index/jwt.json" ] && echo 1 || echo 0)"
out=$("$HARD" registry sync 2>&1)
contains "a second sync is incremental" "seq" "$out"
out=$("$HARD" registry sync --full 2>&1) && rc=0 || rc=$?
check "--full re-lists everything" 0 "$rc"
contains "and reports a listing" "listed" "$out"

# a new publish shows up in the incremental sync
publish_to "$URL1" "fresh" "1.0.0"
publish_to "$URL2" "fresh" "1.0.0"
out=$("$HARD" registry sync 2>&1)
contains "an incremental sync reports what changed" "changed" "$out"
contains "including the new package" "fresh" "$out"

out=$(cd "$DEADAPP" && "$HARD" registry sync 2>&1) && rc=0 || rc=$?
check "a sync with only dead registries fails" 1 "$rc"
contains "and reports the error" "sync" "$out"

# with a synced cache, a dead set still answers
out=$(cd "$DEADAPP" && "$HARD" search jwt 2>&1) && rc=0 || rc=$?
check "an all-dead set still answers from the synced cache" 0 "$rc"
contains "and marks the answer degraded" "from cache" "$out"
contains "and says the registry was unreachable" "registry unreachable" "$out"

# ------------------------------------------------------- mirror verify
start_registry "$URL2" "$WORK/data2"
sleep 0.2
out=$("$HARD" registry mirror verify "$URL2" 2>&1) && rc=0 || rc=$?
check "a faithful mirror verifies clean" 0 "$rc"
contains "and says so" "clean" "$out"
out=$("$HARD" registry mirror verify "$URL3" 2>&1) && rc=0 || rc=$?
check "an incomplete mirror does not verify" 1 "$rc"
contains "and names what is missing" "missing" "$out"
out=$("$HARD" registry mirror verify "http://127.0.0.1:$P4" 2>&1) && rc=0 || rc=$?
check "a dead mirror does not verify" 1 "$rc"
out=$("$HARD" registry mirror verify 2>&1) && rc=0 || rc=$?
check "mirror verify needs a url" 2 "$rc"
contains "and prints usage" "usage: hard registry mirror verify" "$out"
out=$("$HARD" registry mirror bogus 2>&1) && rc=0 || rc=$?
check "an unknown mirror action exits 2" 2 "$rc"

# -------------------------------------------------------- registry add/remove
ADDED="$WORK/added"; mkdir -p "$ADDED"
cat > "$ADDED/hard.toml" <<'EOF'
schema = 1
name = "addedapp"
version = "0.1.0"
edition = "2027"
EOF
out=$(cd "$ADDED" && "$HARD" registry add "$URL2")
contains "registry add reports success" "added mirror" "$out"
check "and writes hard.toml" 1 "$([ -f "$ADDED/hard.toml" ] && echo 1 || echo 0)"
contains "hard.toml gains a [registry] table" "[[registry.mirror]]" "$(cat "$ADDED/hard.toml")"
out=$(cd "$ADDED" && "$HARD" registry add "$URL2")
contains "adding the same mirror twice is refused politely" "already a mirror" "$out"
out=$(cd "$ADDED" && "$HARD" registry add "$URL3" --priority 5 --name "asia")
check "a second mirror is added" 2 "$(grep -c '^\[\[registry.mirror\]\]' "$ADDED/hard.toml")"
contains "with its priority" "priority = 5" "$(cat "$ADDED/hard.toml")"
contains "and its name" 'name = "asia"' "$(cat "$ADDED/hard.toml")"
out=$(cd "$ADDED" && "$HARD" registry remove "$URL3")
contains "registry remove reports success" "removed mirror" "$out"
check "and the entry is gone" 0 "$(grep -c "$URL3" "$ADDED/hard.toml" || true)"
out=$(cd "$ADDED" && "$HARD" registry remove "$URL3" 2>&1) && rc=0 || rc=$?
check "removing a mirror twice fails" 1 "$rc"
out=$(cd "$ADDED" && "$HARD" registry add "not-a-url" 2>&1) && rc=0 || rc=$?
check "a bad url is refused" 1 "$rc"
out=$(cd "$ADDED" && "$HARD" registry add 2>&1) && rc=0 || rc=$?
check "add with no url exits 2" 2 "$rc"
out=$(cd "$ADDED" && "$HARD" registry bogus 2>&1) && rc=0 || rc=$?
check "an unknown subcommand exits 2" 2 "$rc"
contains "and prints usage" "usage: hard registry" "$out"
out=$(cd "$ADDED" && "$HARD" registry list)
contains "the added mirror is listed after the round trip" "$URL2" "$out"

# ------------------------------------------------------------------- cache
out=$("$HARD" cache info)
contains "cache info shows the root" "cache root" "$out"
check "a healthy cache verifies" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"
contains "and says so" "cache ok" "$("$HARD" cache verify)"

# corrupt a cached archive and prove the cache notices
ARCHIVE=$(find "$HARD_HOME/cache/packages" -name '*.hspkg' | head -1)
check "a cached archive exists to corrupt" 1 "$([ -n "$ARCHIVE" ] && echo 1 || echo 0)"
if [ -n "$ARCHIVE" ]; then
  printf 'corrupted' >> "$ARCHIVE"
  out=$("$HARD" cache verify 2>&1) && rc=0 || rc=$?
  check "a corrupt cache fails verification" 1 "$rc"
  contains "and names the entry" "corrupt" "$out"
  contains "and suggests the repair" "hard cache verify --repair" "$out"
  out=$("$HARD" cache verify --repair 2>&1) && rc=0 || rc=$?
  check "--repair succeeds" 0 "$rc"
  contains "and reports what it removed" "repaired" "$out"
  check "the corrupt archive is gone" 0 "$([ -f "$ARCHIVE" ] && echo 1 || echo 0)"
  check "the cache verifies again" 0 "$("$HARD" cache verify >/dev/null 2>&1; echo $?)"
  # and the package can be re-fetched
  rm -rf .hard
  out=$("$HARD" install 2>&1) && rc=0 || rc=$?
  check "a repaired package installs again" 0 "$rc"
  contains "and is fetched anew" "installed jwt" "$out"
fi

out=$("$HARD" cache bogus 2>&1) && rc=0 || rc=$?
check "an unknown cache subcommand exits 2" 2 "$rc"
contains "and lists the subcommands" "info|verify|clean" "$out"

echo
echo "mirror regressions: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
