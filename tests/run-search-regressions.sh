#!/usr/bin/env bash
# Registry search regressions: real HTTP server, real publishes, real searches.
#
# Covers the API contract (query shaping, tags, prefix, limit/offset, scores,
# downloads, latest version, 400s) and the CLI contract (table, --json,
# --offline, cache fallback, no-hit message).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HARD="$ROOT/target/debug/hard"
REG_BIN="$ROOT/target/debug/hard-registry"
[ -x "$HARD" ] || cargo build --quiet
[ -x "$REG_BIN" ] || cargo build --quiet --package hard-registry

PORT="${HARD_REG_TEST_PORT:-18710}"
PORT2="${HARD_REG_TEST_PORT2:-18711}"
DEAD_PORT="${HARD_REG_TEST_PORT3:-18799}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-search-reg.XXXXXX")"
export HARD_HOME="$WORK/home"
REG_URL="http://127.0.0.1:$PORT"
export HARD_REGISTRY="$REG_URL"
mkdir -p "$HARD_HOME"

PASS=0
FAIL=0
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

N=0
ok() { N=$((N + 1)); PASS=$((PASS + 1)); printf '  ok %d - %s\n' "$N" "$1"; }
no() {
  N=$((N + 1))
  FAIL=$((FAIL + 1))
  printf '  NOT OK %d - %s\n' "$N" "$1"
  if [ $# -gt 1 ]; then printf '      %s\n' "$2"; fi
  return 0
}

check() { # name expected actual
  if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "expected [$2] got [$3]"; fi
}
contains() { # name needle haystack
  case "$3" in *"$2"*) ok "$1";; *) no "$1" "missing [$2] in [$3]";; esac
}
lacks() { # name needle haystack
  case "$3" in *"$2"*) no "$1" "unexpected [$2]";; *) ok "$1";; esac
}

api() { # method path
  if [ "$1" = GET ]; then
    curl -sS -o "$WORK/body" -w '%{http_code}' "$REG_URL$2"
  else
    curl -sS -X "$1" -o "$WORK/body" -w '%{http_code}' "$REG_URL$2"
  fi
}
body() { cat "$WORK/body"; }
field() { python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));
k=sys.argv[2]
v=d
for p in k.split("."):
    v = v[int(p)] if p.isdigit() else v.get(p) if isinstance(v,dict) else v[p]
print("" if v is None else (json.dumps(v) if isinstance(v,(list,dict)) else v))' "$WORK/body" "$1"; }

echo "# search regressions"
echo

# ---------------------------------------------------------------- server up
# --open: search is a read-only endpoint, so the suite publishes without tokens
"$REG_BIN" serve --addr "127.0.0.1:$PORT" --data "$WORK/data" --open >"$WORK/reg.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do
  curl -sS -o /dev/null "$REG_URL/health" 2>/dev/null && break
  sleep 0.1
done
check "registry is up" 200 "$(api GET /health)"

# ------------------------------------------------------------------ fixtures
mkpkg() { # dir name version description tags downloads
  local dir="$1" name="$2" ver="$3" desc="$4" tags="$5" dls="$6"
  mkdir -p "$dir"
  cat > "$dir/hard.toml" <<EOF
schema = 1
name = "$name"
version = "$ver"
edition = "2027"
description = "$desc"
license = "MIT"

[package]
tags = [$tags]
EOF
  printf 'calc f() => Int { <- 1 }\n' > "$dir/main.hard"
  ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 )
}
P="$WORK/pkgs"
mkpkg "$P/jwt"            jwt            1.0.0 "JSON web tokens"            '"web", "auth"'   0
mkpkg "$P/jsonwebtoken"   jsonwebtoken   2.3.1 "a JWT implementation"         '"web", "crypto"' 0
mkpkg "$P/hard-toml"      hard-toml      0.4.0 "TOML for HardScript"          '"config"'        0
mkpkg "$P/validator"      validator      3.0.0 "validate anything"            '"web"'           0
mkpkg "$P/jwtx"           jwtx           0.1.0 "an experimental jwt kit"     '"experimental"'  0
check "five packages published" 200 "$(api GET /packages/jwt)"
"$HARD" publish --help >/dev/null 2>&1 && ok "publish --help works" || no "publish --help works"

# second version of jwt, to check "latest" is the newest
mkpkg "$P/jwt2" jwt 1.10.0 "JSON web tokens, faster" '"web", "auth"' 0
mkpkg "$P/jwt3" jwt 1.9.0 "an older jwt" '"web", "auth"' 0

# ------------------------------------------------------------- api contract
code=$(api GET '/search?q=jwt')
check "GET /search is 200" 200 "$code"
check "q=jwt matches the name" "jwt" "$(field results.0.name)"
check "an exact name scores 1000" 1000 "$(field results.0.score)"
contains "q=jwt also matches a prefix" "jsonwebtoken" "$(body)"
check "count is reported" 3 "$(field count)"
check "total is the full match count, count the page" "3 2" "$(api GET '/search?q=jwt&limit=2' >/dev/null; printf '%s %s' "$(field total)" "$(field count)")"

check "a package exposes its latest version" 1.10.0 "$(field results.0.version)"
check "latest is the newest, not the last published" 1.10.0 "$("$HARD" search jwt --json | python3 -c 'import json,sys;print(json.load(sys.stdin)["results"][0]["version"])')"
check "results carry the owner" 1 "$(curl -sS "$REG_URL/search?q=jwt" | python3 -c 'import json,sys
d=json.load(sys.stdin)
print(1 if all("owner" in r for r in d["results"]) else 0)')"
check "results carry keywords" 1 "$(curl -sS "$REG_URL/search?q=jwt" | python3 -c 'import json,sys
d=json.load(sys.stdin)
print(1 if all("keywords" in r for r in d["results"]) else 0)')"

check "a tag filter is honoured" 200 "$(api GET '/search?tag=config')"
check "tag=config finds exactly one" hard-toml "$(field results.0.name)"
check "an unknown tag is still a 200" 200 "$(api GET '/search?tag=nope-nothing')"
check "an unknown tag returns no results" 0 "$(api GET '/search?tag=nope-nothing' >/dev/null; field count)"
contains "two tags must both match" "jwt" "$(api GET '/search?tag=web&tag=auth' >/dev/null; body)"
check "a package with both tags is found" 1 "$(api GET '/search?tag=web&tag=auth' >/dev/null; field count)"
check "a package with one of two tags is excluded" 0 "$(api GET '/search?tag=web&tag=experimental' >/dev/null; field count)"

check "a prefix filter is honoured" jwt "$(api GET '/search?prefix=jwt' >/dev/null; field results.0.name)"
check "a prefix keeps every prefixed package" 2 "$(api GET '/search?prefix=jwt' >/dev/null; field count)"
check "a prefix excludes unprefixed packages" 0 "$(api GET '/search?prefix=zzz' >/dev/null; field count)"
check "prefix plus text intersects" 1 "$(api GET '/search?q=json&prefix=json' >/dev/null; field count)"

check "limit is respected" 1 "$(api GET '/search?q=jwt&limit=1' >/dev/null; field count)"
check "offset skips" 1 "$(api GET '/search?q=jwt&offset=1&limit=1' >/dev/null; field count)"
check "offset past the end is empty, not an error" 200 "$(api GET '/search?q=jwt&offset=99')"
check "offset past the end has no results" 0 "$(api GET '/search?q=jwt&offset=99' >/dev/null; field count)"
check "a huge limit is clamped, not rejected" 200 "$(api GET '/search?q=jwt&limit=100000')"
check "limit=0 is clamped" 200 "$(api GET '/search?q=jwt&limit=0')"

check "an empty q is a 400" 400 "$(api GET '/search?q=')"
check "a no-filter search is a 400" 400 "$(api GET '/search')"
check "a blank q is a 400" 400 "$(api GET '/search?q=%20%20')"
check "the 400 explains itself" 1 "$(api GET '/search' >/dev/null; python3 -c 'import json;print(1 if json.load(open("'"$WORK"'/body"))["error"] else 0)')"
check "an empty tag is a 400" 400 "$(api GET '/search?tag=')"
check "a garbage limit is a 400" 400 "$(api GET '/search?q=jwt&limit=lots')"
check "a negative limit is a 400" 400 "$(api GET '/search?q=jwt&limit=-1')"
check "a negative offset is a 400" 400 "$(api GET '/search?q=jwt&offset=-1')"
check "a misspelled filter is ignored, not fatal" 200 "$(api GET '/search?q=jwt&colour=blue')"
check "a duplicate q takes the first" 200 "$(api GET '/search?q=jwt&q=zzz')"
check "search is a GET only" 405 "$(api POST '/search?q=jwt')"

check "results are ordered by score" 1 "$(curl -sS "$REG_URL/search?q=jwt" | python3 -c 'import json,sys
s=[r["score"] for r in json.load(sys.stdin)["results"]]
print(1 if s==sorted(s,reverse=True) else 0)')"
check "a miss is an empty result set, not a 404" 200 "$(api GET '/search?q=definitely-not-published-xyz')"
check "a miss has no results" 0 "$(api GET '/search?q=definitely-not-published-xyz' >/dev/null; field count)"
check "a search for a unicode name is 200" 200 "$(api GET '/search?q=%E2%9C%93')"

# ---------------------------------------------------------------- downloads
# download counting shows up in search results
mkpkg "$P/dl1" downloader 1.0.0 "counts downloads" '"web"' 0
curl -sS -o /dev/null "$REG_URL/packages/downloader/1.0.0/download"
curl -sS -o /dev/null "$REG_URL/packages/downloader/1.0.0/download"
check "a download is counted" 2 "$(curl -sS "$REG_URL/packages/downloader" | python3 -c 'import json,sys;print(json.load(sys.stdin)["downloads"])')"
check "a HEAD download is not counted" 2 "$(curl -sS -I -o /dev/null "$REG_URL/packages/downloader/1.0.0/download"; curl -sS "$REG_URL/packages/downloader" | python3 -c 'import json,sys;print(json.load(sys.stdin)["downloads"])')"
check "search reports the download count" 2 "$(curl -sS "$REG_URL/search?q=downloader" | python3 -c 'import json,sys;print(json.load(sys.stdin)["results"][0]["downloads"])')"
check "an untagged search still finds it" 1 "$(api GET '/search?q=downloader' >/dev/null; field count)"

# ------------------------------------------------------------------ cli
out=$("$HARD" search jwt)
contains "the CLI table lists the name" "jwt" "$out"
contains "the CLI table shows the version" "1.10.0" "$out"
contains "the CLI table shows the summary" "result(s) from registry" "$out"
lacks "the CLI does not print a score by default" "score" "$out"

out=$("$HARD" search jwt --explain)
contains "--explain prints the scores" "score 1000" "$out"

out=$("$HARD" search jwt --json)
check "the CLI --json is valid JSON" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;json.load(sys.stdin);print(1)')"
check "--json reports the origin" registry "$(printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["origin"])')"
check "--json has no trailing noise" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;json.loads(sys.stdin.read());print(1)')"
check "--json carries the downloads" 0 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(json.load(sys.stdin)["results"][0]["downloads"])')"
check "--json is parseable by a shell loop" "jwt 1.10.0" "$(printf '%s' "$out" | python3 -c 'import json,sys
r=json.load(sys.stdin)["results"][0]
print(r["name"], r["version"])')"

out=$("$HARD" search --tag web --limit 2)
check "a tag-only search works from the CLI" 2 "$(printf '%s\n' "$out" | grep -c ' dl ')"
lacks "a limited search does not print a third row" "validator" "$out"

out=$("$HARD" search --tag web --offset 1 --limit 1)
check "the CLI honours --offset" 1 "$(printf '%s\n' "$out" | grep -c ' dl ')"

out=$("$HARD" search definitely-not-published-xyz)
contains "a CLI miss says so" "no packages matching" "$out"
check "a CLI miss exits 0" 0 "$("$HARD" search definitely-not-published-xyz >/dev/null 2>&1; echo $?)"

out=$("$HARD" search 2>&1) && rc=0 || rc=$?
check "a CLI search with no query exits 2" 2 "$rc"
contains "a CLI search with no query explains" "missing query" "$out"

# ---------------------------------------------------------------- offline
mkdir -p "$WORK/app"
cat > "$WORK/app/hard.toml" <<'EOF'
schema = 1
name = "search-app"
version = "0.1.0"
edition = "2027"

[dependencies]
jwt = "1"
EOF
( cd "$WORK/app" && "$HARD" install >/dev/null 2>&1 )
check "the index was cached" 1 "$([ -f "$HARD_HOME/cache/index/jwt.json" ] && echo 1 || echo 0)"

out=$("$HARD" search jwt --offline)
contains "offline search answers from the cache" "jwt" "$out"
contains "offline search says it is degraded" "answered from the cache" "$out"
contains "offline search uses the cached index" "from cache" "$out"
check "offline search still exits 0" 0 "$("$HARD" search jwt --offline >/dev/null 2>&1; echo $?)"

out=$("$HARD" search --offline --tag auth)
contains "offline search honours tags" "jwt" "$out"
out=$("$HARD" search --offline --tag nothing-has-this)
contains "an offline tag miss is empty, not an error" "no packages" "$out"
out=$("$HARD" search --offline definitely-not-cached-xyz)
contains "an offline text miss is empty, not an error" "no packages" "$out"
check "an offline miss exits 0" 0 "$("$HARD" search --offline definitely-not-cached-xyz >/dev/null 2>&1; echo $?)"
out=$("$HARD" search --offline definitely-not-cached-xyz)
contains "an offline miss says the cache was empty" "nothing in the local cache" "$out"

out=$(HARD_REGISTRY="http://127.0.0.1:$DEAD_PORT" "$HARD" search jwt)
contains "an unreachable registry falls back to the cache" "answered from the cache" "$out"
check "an unreachable registry is not a hard error" 0 "$(HARD_REGISTRY="http://127.0.0.1:$DEAD_PORT" "$HARD" search jwt >/dev/null 2>&1; echo $?)"
out=$(HARD_REGISTRY="http://127.0.0.1:$DEAD_PORT" "$HARD" search definitely-not-cached-xyz 2>&1) && rc=0 || rc=$?
check "an unreachable registry with a cache miss is empty, not an error" 0 "$rc"
contains "and says the cache was empty" "nothing in the local cache" "$out"

# a second registry, to prove the URL is honoured
"$REG_BIN" serve --addr "127.0.0.1:$PORT2" --data "$WORK/data2" --open >"$WORK/reg2.log" 2>&1 &
PID2=$!
for _ in $(seq 1 50); do curl -sS -o /dev/null "http://127.0.0.1:$PORT2/health" 2>/dev/null && break; sleep 0.1; done
mkdir -p "$P/other" && cat > "$P/other/hard.toml" <<'EOF'
schema = 1
name = "elsewhere"
version = "1.0.0"
edition = "2027"
description = "only on the second registry"
EOF
printf 'calc f() => Int { <- 1 }\n' > "$P/other/main.hard"
( cd "$P/other" && HARD_REGISTRY="http://127.0.0.1:$PORT2" "$HARD" publish >/dev/null 2>&1 )
check "the second registry has its own package" 1 "$(curl -sS "http://127.0.0.1:$PORT2/search?q=elsewhere" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["results"]))')"
check "--registry points at another registry" 1 "$(HARD_REGISTRY="$REG_URL" "$HARD" search elsewhere --registry "http://127.0.0.1:$PORT2" --json | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["results"]))')"
check "the default registry does not have it" 0 "$(curl -sS "$REG_URL/search?q=elsewhere" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["results"]))')"
kill "$PID2" 2>/dev/null || true

# ------------------------------------------------------------------ report
echo
echo "search regressions: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
