#!/usr/bin/env bash
# Signature and trust-store regressions.
#
# Real registry, real publishes, real Ed25519 verification, real tampering:
# every check here either signs something, edits something that was signed, or
# asks the CLI to make a trust decision.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HARD="$ROOT/target/debug/hard"
REG_BIN="$ROOT/target/debug/hard-registry"
cargo build --quiet
cargo build --quiet --package hard-registry

PORT="${HARD_SIG_PORT:-18810}"
PORT2="${HARD_SIG_PORT2:-18811}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/hard-sig.XXXXXX")"
export HARD_HOME="$WORK/home"
export HARD_REGISTRY="http://127.0.0.1:$PORT"
REG_URL="$HARD_REGISTRY"
KEYS="$HARD_HOME/trusted-keys.toml"
mkdir -p "$HARD_HOME"

N=0; PASS=0; FAIL=0; SERVER_PID=""; SERVER2_PID=""
cleanup() {
  for pid in "$SERVER_PID" "$SERVER2_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
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
contains() { case "$3" in *"$2"*) ok "$1";; *) no "$1" "missing [$2] in [$(printf '%.120s' "$3")]";; esac; }
lacks() { case "$3" in *"$2"*) no "$1" "unexpected [$2]";; *) ok "$1";; esac; }

api() { curl -sS -o "$WORK/body" -w '%{http_code}' "$REG_URL$1"; }
body() { cat "$WORK/body"; }
jq_() { python3 -c 'import json,sys
d=json.load(open(sys.argv[1]))
for p in sys.argv[2].split("."):
    d = d[int(p)] if p.isdigit() else (d.get(p) if isinstance(d,dict) else None)
    if d is None: break
print("" if d is None else (json.dumps(d) if isinstance(d,(list,dict)) else d))' "$WORK/body" "$1"; }

# publish a package: mkpkg <dir> <name> <version> [tags]
mkpkg() {
  local dir="$1" name="$2" ver="$3" tags="${4:-}"
  mkdir -p "$dir"
  {
    printf 'schema = 1\nname = "%s"\nversion = "%s"\nedition = "2027"\n' "$name" "$ver"
    printf 'description = "the %s package"\nlicense = "MIT"\n' "$name"
    [ -n "$tags" ] && printf '\n[package]\ntags = [%s]\n' "$tags"
  } > "$dir/hard.toml"
  printf 'calc f() => Int { <- 1 }\n' > "$dir/main.hard"
  # a failed publish is a failed fixture, and `set -e` should say so
  ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 ) || {
    echo "fixture $name@$ver did not publish" >&2
    exit 1
  }
}

P="$WORK/pkgs"
echo "# signature regressions"
echo

# A stale server on the port would silently answer for us and make every
# publish a duplicate, so refuse to start rather than report nonsense.
for port in "$PORT" "$PORT2"; do
  if curl -sS -m 1 -o /dev/null "http://127.0.0.1:$port/health" 2>/dev/null; then
    echo "error: something is already listening on 127.0.0.1:$port" >&2
    echo "       stop it, or set HARD_SIG_PORT / HARD_SIG_PORT2" >&2
    exit 1
  fi
done

# ------------------------------------------------------------------ server
"$REG_BIN" serve --addr "127.0.0.1:$PORT" --data "$WORK/data" --open >"$WORK/reg.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do curl -sS -o /dev/null "$REG_URL/health" 2>/dev/null && break; sleep 0.1; done
check "registry is up" 200 "$(api /health)"

# ------------------------------------------------------------------- /keys
check "GET /keys is 200" 200 "$(api /keys)"
check "the key is ed25519" ed25519 "$(jq_ algorithm)"
check "the payload format is hs-signature/1" hs-signature/1 "$(jq_ payload_format)"
check "the key id is k: + 16 hex" 1 "$(python3 -c 'import json,re;d=json.load(open("'"$WORK"'/body"));print(1 if re.fullmatch(r"k:[0-9a-f]{16}", d["key_id"]) else 0)')"
check "the public key is 64 hex characters" 1 "$(python3 -c 'import json,re;d=json.load(open("'"$WORK"'/body"));print(1 if re.fullmatch(r"[0-9a-f]{64}", d["public_key"]) else 0)')"
check "a test key is flagged as such" True "$(jq_ test_key)"
check "the key id matches the public key" 1 "$("$HARD" keys add --key "$(jq_ public_key)" >/dev/null 2>&1; python3 -c "
import re
key=open('$KEYS').read()
m=re.search(r'id = \"(k:[0-9a-f]+)\"', key)
print(1 if m and m.group(1) == 'k:' + open('/dev/stdin').read().strip()[:16] else 0)" <<< "$(jq_ public_key)")"
rm -f "$KEYS"

# ------------------------------------------------------- publish + signature
mkpkg "$P/jwt" jwt 1.0.0
mkpkg "$P/jwt2" jwt 1.1.0
mkpkg "$P/other" otherpkg 0.1.0
check "the publish was accepted" 200 "$(api /packages/jwt)"

check "the signature endpoint is 200" 200 "$(api /packages/jwt/1.0.0/signature)"
check "the signature is base64" 1 "$(python3 -c 'import json,base64,re
d=json.load(open("'"$WORK"'/body"))
s=d["signature"]
print(1 if re.fullmatch(r"[A-Za-z0-9+/]+={0,2}", s) and len(base64.b64decode(s))==64 else 0)')"
check "the record names the key" 1 "$(python3 -c 'import json;d=json.load(open("'"$WORK"'/body"));print(1 if d["key_id"].startswith("k:") else 0)')"
check "the record carries the public key" 1 "$(python3 -c 'import json;d=json.load(open("'"$WORK"'/body"));print(1 if len(d["public_key"])==64 else 0)')"
check "the record carries the integrity" 1 "$(python3 -c 'import json;d=json.load(open("'"$WORK"'/body"));print(1 if d["integrity"].startswith("sha256:") else 0)')"
check "the record carries the fingerprint" 1 "$(python3 -c 'import json;d=json.load(open("'"$WORK"'/body"));print(1 if d["fingerprint"].startswith("sha256:") else 0)')"
check "the payload is the canonical text" 1 "$(python3 -c 'import json
d=json.load(open("'"$WORK"'/body"))
want="hs-signature/1\nname jwt\nversion 1.0.0\nintegrity %s\nfingerprint %s\n" % (d["integrity"], d["fingerprint"])
print(1 if d["payload"]==want else 0)')"
check "the payload has no timestamp or count" 1 "$(python3 -c 'import json
d=json.load(open("'"$WORK"'/body"))
print(1 if not any(w in d["payload"] for w in ("published_at","downloads","202","http")) else 0)')"
check "the payload is 5 lines" 5 "$(python3 -c 'import json;print(len(json.load(open("'"$WORK"'/body"))["payload"].strip().split(chr(10))))')"
check "the signature covers this version only" 1 "$(api /packages/jwt/1.1.0/signature >/dev/null; python3 -c '
import json
a=json.loads(open("'"$WORK"'/body").read())
print(1 if a["version"]=="1.1.0" and a["payload"].count("version 1.1.0")==1 else 0)')"
check "an unknown version has no signature" 404 "$(api /packages/jwt/9.9.9/signature)"
check "an unknown package has no signature" 404 "$(api /packages/nope/1.0.0/signature)"

# two versions must not share a signature
a=$(curl -sS "$REG_URL/packages/jwt/1.0.0/signature" | python3 -c 'import json,sys;print(json.load(sys.stdin)["signature"])')
b=$(curl -sS "$REG_URL/packages/jwt/1.1.0/signature" | python3 -c 'import json,sys;print(json.load(sys.stdin)["signature"])')
check "two versions have different signatures" 1 "$([ "$a" != "$b" ] && echo 1 || echo 0)"
check "publishing is deterministic in the payload" 1 "$([ "$(curl -sS "$REG_URL/packages/jwt/1.0.0/signature" | python3 -c 'import json,sys;print(json.load(sys.stdin)["payload"])')" = "$(curl -sS "$REG_URL/packages/jwt/1.0.0/signature" | python3 -c 'import json,sys;print(json.load(sys.stdin)["payload"])')" ] && echo 1 || echo 0)"

# -------------------------------------------------------------- trust store
out=$("$HARD" keys list)
contains "an empty store says so" "no keys recorded" "$out"
check "the store file does not exist yet" 0 "$([ -f "$KEYS" ] && echo 1 || echo 0)"

out=$("$HARD" keys list --json)
check "an empty store is [] in JSON" "[]" "$out"

out=$("$HARD" keys add "$REG_URL")
contains "keys add records the key" "recorded key k:" "$out"
check "the store file now exists" 1 "$([ -f "$KEYS" ] && echo 1 || echo 0)"
check "a recorded key is not pinned" 0 "$(grep -c '^trusted = true' "$KEYS" || true)"
check "the store records one key" 1 "$(grep -c '^\[\[key\]\]' "$KEYS")"
check "the store records the public key" 1 "$(grep -c '^public_key = "[0-9a-f]\{64\}"' "$KEYS")"

KEY_ID=$(grep -o 'k:[0-9a-f]*' "$KEYS" | head -1)
PUB=$(grep '^public_key = ' "$KEYS" | head -1 | sed 's/public_key = "//; s/"//')

out=$("$HARD" keys list)
contains "keys list shows the key" "$KEY_ID" "$out"
contains "an unpinned key is marked seen" "seen" "$out"
contains "keys list explains the strict consequence" "nothing will pass" "$out"

out=$("$HARD" keys trust "$KEY_ID")
contains "keys trust pins the key" "trusted $KEY_ID" "$out"
check "the store now says trusted" 1 "$(grep -c '^trusted = true' "$KEYS")"
check "keys list reports 1 of 1" 1 "$("$HARD" keys list | grep -c '1 of 1 trusted')"

out=$("$HARD" keys trust k:doesnotexist 2>&1) && rc=0 || rc=$?
check "trusting an unknown key fails" 1 "$rc"
contains "and says why" "add it first" "$out"

out=$("$HARD" keys add --key "$PUB" --trust --label "manual")
contains "a hand-added key works" "updated key" "$out"
check "the label is stored" 1 "$(grep -c 'label = "manual"' "$KEYS")"
check "adding the same key does not duplicate it" 1 "$(grep -c '^\[\[key\]\]' "$KEYS")"

out=$("$HARD" keys add --key nothex 2>&1) && rc=0 || rc=$?
check "an invalid key is refused" 1 "$rc"
contains "and says why" "not a valid ed25519 public key" "$out"

out=$("$HARD" keys add --key 00 2>&1) && rc=0 || rc=$?
check "a short key is refused" 1 "$rc"

out=$("$HARD" keys add "$REG_URL" --for jwt --trust)
check "scoping an already general key does not narrow it" 0 "$(grep -c 'packages = ' "$KEYS")"

out=$("$HARD" keys export)
contains "export is TOML" "[[key]]" "$out"
check "export is the same document" 1 "$([ "$out" = "$(cat "$KEYS")" ] && echo 1 || echo 0)"
out=$("$HARD" keys export --json)
check "export --json is JSON" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(1 if isinstance(d,list) else 0)')"
check "export --json has every key" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')"

out=$("$HARD" keys bogus 2>&1) && rc=0 || rc=$?
check "an unknown subcommand exits 2" 2 "$rc"
contains "and prints usage" "usage: hard keys" "$out"

# ----------------------------------------------------------- hard verify
out=$("$HARD" verify jwt@1.0.0)
contains "a pinned key verifies" "verified" "$out"
contains "the outcome names the key" "$KEY_ID" "$out"
check "verify exits 0 when it passes" 0 "$("$HARD" verify jwt@1.0.0 >/dev/null 2>&1; echo $?)"

out=$("$HARD" verify jwt --json)
check "verify --json is JSON" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(1 if json.load(sys.stdin)["verified"] else 0)')"
check "verify --json reports the key" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(1 if json.load(sys.stdin)["key_id"].startswith("k:") else 0)')"
check "verify --json reports the payload hash" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(1 if json.load(sys.stdin)["payload_hash"].startswith("sha256:") else 0)')"

out=$("$HARD" verify otherpkg@0.1.0)
contains "another package verifies too" "verified" "$out"
out=$("$HARD" verify otherpkg)
contains "a bare name resolves to the latest version" "verified" "$out"

out=$("$HARD" verify nope@1.0.0 2>&1) && rc=0 || rc=$?
check "an unknown package fails" 1 "$rc"
contains "and says it is unsigned" "no signature available" "$out"

out=$("$HARD" verify 2>&1) && rc=0 || rc=$?
check "verify with no target exits 2" 2 "$rc"
contains "and prints usage" "usage: hard verify" "$out"

out=$("$HARD" verify jwt@1.0.0 --verify=loose 2>&1) && rc=0 || rc=$?
check "an unknown mode exits 2" 2 "$rc"
contains "and lists the modes" "expected strict, warn or off" "$out"

# --------------------------------------------------- local archive + sidecar
DL="$WORK/dl"; mkdir -p "$DL"
check "the archive downloads" 200 "$(curl -sS -o "$DL/jwt-1.0.0.hspkg" -w '%{http_code}' "$REG_URL/packages/jwt/1.0.0/download")"
curl -sS "$REG_URL/packages/jwt/1.0.0/signature" -o "$DL/jwt-1.0.0.hspkg.sig"
out=$("$HARD" verify "$DL/jwt-1.0.0.hspkg")
contains "a signed archive verifies offline" "verified" "$out"
out=$("$HARD" verify "$DL/jwt-1.0.0.hspkg" --json)
check "the archive digest matches the signature" 1 "$(printf '%s' "$out" | python3 -c 'import json,sys;print(1 if json.load(sys.stdin)["verified"] else 0)')"

cp "$DL/jwt-1.0.0.hspkg" "$WORK/unsigned.hspkg"
out=$("$HARD" verify "$WORK/unsigned.hspkg" 2>&1) && rc=0 || rc=$?
check "an archive with no sidecar is not verified" 1 "$rc"
contains "it says unsigned" "unsigned" "$out"
contains "and still reports the digest" "sha256:" "$out"

printf 'this is not an archive' > "$WORK/junk.hspkg"
out=$("$HARD" verify "$WORK/junk.hspkg" 2>&1) && rc=0 || rc=$?
check "a non-archive is rejected" 1 "$rc"
contains "and says it is not an archive" "not a .hspkg archive" "$out"

out=$("$HARD" verify "$WORK/missing.hspkg" 2>&1) && rc=0 || rc=$?
check "a missing file is rejected" 1 "$rc"
contains "and says it cannot be read" "cannot read" "$out"

# tamper: flip a byte in the archive, keep the signature
cp "$DL/jwt-1.0.0.hspkg" "$DL/tampered.hspkg"
cp "$DL/jwt-1.0.0.hspkg.sig" "$DL/tampered.hspkg.sig"
python3 - "$DL/tampered.hspkg" <<'PY'
import sys
p = sys.argv[1]
b = bytearray(open(p, 'rb').read())
b[len(b) // 2] ^= 0xFF
open(p, 'wb').write(bytes(b))
PY
out=$("$HARD" verify "$DL/tampered.hspkg" 2>&1) && rc=0 || rc=$?
check "a tampered archive fails" 1 "$rc"
contains "with an integrity mismatch" "integrity-mismatch" "$out"

# a valid archive with a sidecar whose signature is for a different package
cp "$DL/jwt-1.0.0.hspkg" "$DL/swapped.hspkg"
cp "$DL/jwt-1.0.0.hspkg.sig" "$DL/swapped.hspkg.sig"
python3 - "$DL/swapped.hspkg.sig" "$REG_URL" <<'PYSWAP'
import json, sys, urllib.request
out, url = sys.argv[1], sys.argv[2]
mine = json.load(open(out))
other = json.load(urllib.request.urlopen(url + "/packages/otherpkg/0.1.0/signature"))
mine["signature"] = other["signature"]
json.dump(mine, open(out, "w"))
PYSWAP
out=$("$HARD" verify "$DL/swapped.hspkg" 2>&1) && rc=0 || rc=$?
check "a signature borrowed from another package fails" 1 "$rc"
contains "as a bad signature" "bad-signature" "$out"

# an unsigned record
cp "$DL/jwt-1.0.0.hspkg" "$DL/nosig.hspkg"
python3 - "$DL/nosig.hspkg.sig" "$REG_URL" <<'PY'
import json, sys, urllib.request
out, url = sys.argv[1], sys.argv[2]
d = json.load(urllib.request.urlopen(url + "/packages/jwt/1.0.0/signature"))
d["signature"] = ""
json.dump(d, open(out, "w"))
PY
out=$("$HARD" verify "$DL/nosig.hspkg" 2>&1) && rc=0 || rc=$?
check "an empty signature is unsigned" 1 "$rc"
contains "and says so" "unsigned" "$out"

# a record with a signature that is not a signature
cp "$DL/jwt-1.0.0.hspkg" "$DL/garbage.hspkg"
python3 - "$DL/garbage.hspkg.sig" "$REG_URL" <<'PY'
import json, sys, urllib.request
out, url = sys.argv[1], sys.argv[2]
d = json.load(urllib.request.urlopen(url + "/packages/jwt/1.0.0/signature"))
d["signature"] = "bm90LWEtc2lnbmF0dXJl"
json.dump(d, open(out, "w"))
PY
out=$("$HARD" verify "$DL/garbage.hspkg" 2>&1) && rc=0 || rc=$?
check "a garbage signature is a bad signature" 1 "$rc"
contains "and says so" "bad-signature" "$out"

# a record whose key id does not match its public key
cp "$DL/jwt-1.0.0.hspkg" "$DL/keyid.hspkg"
python3 - "$DL/keyid.hspkg.sig" "$REG_URL" <<'PY'
import json, sys, urllib.request
out, url = sys.argv[1], sys.argv[2]
d = json.load(urllib.request.urlopen(url + "/packages/jwt/1.0.0/signature"))
d["key_id"] = "k:0123456789abcdef"
json.dump(d, open(out, "w"))
PY
out=$("$HARD" verify "$DL/keyid.hspkg" 2>&1) && rc=0 || rc=$?
check "a mismatched key id is refused" 1 "$rc"
contains "and explains" "does not match the public key" "$out"

# a record with an algorithm we do not implement
cp "$DL/jwt-1.0.0.hspkg" "$DL/algo.hspkg"
python3 - "$DL/algo.hspkg.sig" "$REG_URL" <<'PY'
import json, sys, urllib.request
out, url = sys.argv[1], sys.argv[2]
d = json.load(urllib.request.urlopen(url + "/packages/jwt/1.0.0/signature"))
d["algorithm"] = "rsa-pss"
json.dump(d, open(out, "w"))
PY
out=$("$HARD" verify "$DL/algo.hspkg" 2>&1) && rc=0 || rc=$?
check "an unknown algorithm is refused" 1 "$rc"
contains "and names it" "rsa-pss" "$out"

# a sidecar that is not JSON
cp "$DL/jwt-1.0.0.hspkg" "$DL/broken.hspkg"
echo "not json" > "$DL/broken.hspkg.sig"
out=$("$HARD" verify "$DL/broken.hspkg" 2>&1) && rc=0 || rc=$?
check "a broken sidecar is rejected" 1 "$rc"
contains "and says it is not JSON" "not valid JSON" "$out"

# an unpinned key makes a valid signature untrusted
cp "$KEYS" "$WORK/keys.bak"
python3 - "$KEYS" <<'PY'
import re, sys
p = sys.argv[1]
s = open(p).read().replace("trusted = true", "trusted = false")
open(p, "w").write(s)
PY
out=$("$HARD" verify jwt@1.0.0 2>&1) && rc=0 || rc=$?
check "an unpinned key is untrusted" 1 "$rc"
contains "and says how to pin it" "hard keys trust" "$out"
out=$("$HARD" verify jwt@1.0.0 --verify=off 2>&1) && rc=0 || rc=$?
check "--verify=off skips the check" 0 "$rc"
contains "and says it skipped" "skipped" "$out"
cp "$WORK/keys.bak" "$KEYS"

# a key pinned for a different package does not cover this one
cp "$KEYS" "$WORK/keys.bak2"
PUBKEY=$(grep '^public_key = ' "$KEYS" | head -1 | sed 's/public_key = "//; s/"//')
KEYID=$(grep -o 'k:[0-9a-f]*' "$KEYS" | head -1)
printf '[[key]]\nid = "%s"\npublic_key = "%s"\ntrusted = true\npackages = ["something-else"]\n' "$KEYID" "$PUBKEY" > "$KEYS"
out=$("$HARD" verify jwt@1.0.0 2>&1) && rc=0 || rc=$?
check "a key scoped to another package does not count" 1 "$rc"
contains "and is reported as untrusted" "untrusted-key" "$out"
cp "$WORK/keys.bak2" "$KEYS"

# ---------------------------------------------------------------- installs
APP="$WORK/app"; mkdir -p "$APP"
cat > "$APP/hard.toml" <<'EOF'
schema = 1
name = "sigapp"
version = "0.1.0"
edition = "2027"

[dependencies]
jwt = "1"
EOF
cd "$APP"
out=$("$HARD" install --verify=strict 2>&1)
contains "a strict install of a pinned package succeeds" "verified 1 package signature" "$out"
check "the lockfile records the signature" 1 "$(grep -c '^signature = ' hard.lock)"
check "the lockfile records the key id" 1 "$(grep -c "^key_id = \"$KEY_ID\"" hard.lock)"
check "the lockfile records the fingerprint" 1 "$(grep -c '^fingerprint = "sha256:' hard.lock)"
check "the lockfile records verified = true" 1 "$(grep -c '^verified = true' hard.lock)"
check "the lockfile schema is unchanged" 1 "$(grep -c 'schema = "hard-lock/v1"' hard.lock)"
check "the lockfile is reusable" 0 "$("$HARD" install --verify=strict --frozen >/dev/null 2>&1; echo $?)"

out=$("$HARD" install --verify=warn 2>&1)
contains "warn mode still verifies" "verified 1 package signature" "$out"

out=$("$HARD" install --verify=off 2>&1)
lacks "off mode prints no verification line" "verified" "$out"
check "off mode still installs" 1 "$([ -f hard.lock ] && echo 1 || echo 0)"
check "off mode records no trust" 0 "$(grep -c '^signature = ' hard.lock)"

out=$(HARD_VERIFY=strict "$HARD" install 2>&1)
contains "HARD_VERIFY=strict is honoured" "verified 1 package signature" "$out"
out=$(HARD_VERIFY=loose "$HARD" install 2>&1) && rc=0 || rc=$?
check "a bad HARD_VERIFY is fatal" 2 "$rc"

# unpinned key: warn continues, strict refuses
cp "$KEYS" "$WORK/keys.install.bak"
python3 - "$KEYS" <<'PY'
import sys
p = sys.argv[1]
open(p, "w").write(open(p).read().replace("^trusted = true", "trusted = false"))
PY
rm -rf .hard hard.lock
out=$("$HARD" install --verify=warn 2>&1) && rc=0 || rc=$?
check "warn mode installs an untrusted package" 0 "$rc"
contains "but says so" "untrusted-key" "$out"
check "and records verified = false" 1 "$(grep -c '^verified = false' hard.lock)"

rm -rf .hard hard.lock
out=$("$HARD" install --verify=strict 2>&1) && rc=0 || rc=$?
check "strict mode refuses an untrusted package" 1 "$rc"
contains "and says why" "untrusted-key" "$out"
check "and writes no lockfile" 0 "$([ -f hard.lock ] && echo 1 || echo 0)"
check "and installs nothing" 0 "$([ -d .hard/jwt ] && echo 1 || echo 0)"

# a trust store that does not exist at all
rm -f "$KEYS"; rm -rf .hard hard.lock
out=$("$HARD" install --verify=warn 2>&1) && rc=0 || rc=$?
check "a missing trust store does not crash warn mode" 0 "$rc"
contains "it reports an untrusted key" "untrusted-key" "$out"
rm -rf .hard hard.lock
out=$("$HARD" install --verify=strict 2>&1) && rc=0 || rc=$?
check "a missing trust store fails strict mode" 1 "$rc"

# ------------------------------------------------------- lockfile backwards
cp "$WORK/keys.install.bak" "$KEYS"
cat > hard.lock <<'EOF'
# hard.lock — written by an older hard
schema = "hard-lock/v1"
compiler = "0.4.0"
platform = "linux-x86_64"

[package.jwt]
version = "1.0.0"
source = "registry"
EOF
out=$("$HARD" install --verify=strict 2>&1)
contains "a lockfile without trust fields still installs" "verified 1 package signature" "$out"
check "and is rewritten with trust fields" 1 "$(grep -c '^signature = ' hard.lock)"
check "and keeps the same schema" 1 "$(grep -c 'schema = "hard-lock/v1"' hard.lock)"

cat > "$WORK/bad.lock" <<'EOF'
schema = "hard-lock/v2"
compiler = "0.4.0"
EOF
cp hard.lock "$WORK/good.lock"; cp "$WORK/bad.lock" hard.lock
out=$("$HARD" install --verify=strict 2>&1) && rc=0 || rc=$?
check "an unknown lockfile schema does not stop a verified install" 0 "$rc"
cp "$WORK/good.lock" hard.lock

# ------------------------------------------------------------ offline + cache
rm -rf .hard hard.lock
"$HARD" install --verify=strict >/dev/null 2>&1
rm -rf .hard
out=$("$HARD" install --offline --verify=strict 2>&1)
contains "an offline install re-verifies from the cached record" "verified 1 package signature" "$out"

rm -rf .hard
out=$(HARD_HOME="$WORK/home2" "$HARD" install --offline --verify=strict 2>&1) && rc=0 || rc=$?
check "offline with nothing cached fails honestly" 1 "$rc"
contains "and says what is missing" "not in the cache" "$out"

rm -rf .hard
out=$("$HARD" install --offline --verify=off 2>&1) && rc=0 || rc=$?
check "offline with verification off uses the cache" 0 "$rc"

# ------------------------------------------------------------- diagnostics
cd "$WORK"
# `hard doctor` exits non-zero on a security warning, which is the point here
out=$("$HARD" doctor 2>&1) || true
contains "doctor reports the signing key" "signing key" "$out"
contains "doctor reports the trusted key count" "trusted keys" "$out"
contains "doctor warns about a test key" "test key" "$out"
"$HARD" doctor >/dev/null 2>&1 && rc=0 || rc=$?
check "doctor exits non-zero on a security warning" 1 "$rc"
out=$("$HARD" doctor --no-registry 2>&1)
lacks "--no-registry skips the registry section" "signing key" "$out"

cp "$KEYS" "$WORK/keys.diag.bak"
python3 - "$KEYS" <<'PY'
import sys
p = sys.argv[1]
open(p, "w").write(open(p).read().replace("^trusted = true", "trusted = false"))
PY
out=$("$HARD" doctor 2>&1) || true
contains "doctor notices an unpinned key" "not pinned" "$out"
contains "doctor says strict would refuse everything" "refuse everything" "$out"
cp "$WORK/keys.diag.bak" "$KEYS"

# ------------------------------------------------------- a second registry
"$REG_BIN" serve --addr "127.0.0.1:$PORT2" --data "$WORK/data2" --open >"$WORK/reg2.log" 2>&1 &
SERVER2_PID=$!
for _ in $(seq 1 60); do curl -sS -o /dev/null "http://127.0.0.1:$PORT2/health" 2>/dev/null && break; sleep 0.1; done
mkpkg "$P/elsewhere" elsewhere 1.0.0
( cd "$P/elsewhere" && HARD_REGISTRY="http://127.0.0.1:$PORT2" "$HARD" publish >/dev/null 2>&1 )
out=$(HARD_REGISTRY="http://127.0.0.1:$PORT2" "$HARD" verify elsewhere@1.0.0 2>&1)
contains "a package from a second registry verifies" "verified" "$out"
out=$(HARD_REGISTRY="http://127.0.0.1:$PORT2" "$HARD" keys add "http://127.0.0.1:$PORT2" 2>&1)
contains "the second registry's key can be recorded" "key k:" "$out"
# both registries run the same deterministic test key, so this is one entry
check "the store does not grow for the same key" 1 "$(grep -c '^\[\[key\]\]' "$KEYS")"
out=$(HARD_REGISTRY="http://127.0.0.1:$PORT2" "$HARD" verify elsewhere@1.0.0 --verify=off 2>&1)
contains "a package from another registry can be skipped too" "skipped" "$out"

echo
echo "signature regressions: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
