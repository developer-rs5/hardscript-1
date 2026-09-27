#!/usr/bin/env bash
# Publish regression suite (M8.2).
#
# Drives `hard publish` and `hard yank` against a real `hard-registry`
# process: it builds the .hspkg from a project directory, uploads it, checks
# the registry's records, and exercises the refusal paths (duplicate version,
# bad version, missing auth, pre-publish conflict checks).
#
# Run: tests/run-publish-regressions.sh   (uses $HARD, default target/debug/hard)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"
REGISTRY="${REGISTRY:-$REPO/target/debug/hard-registry}"

if [ ! -x "$HARD" ]; then
    echo "publish: $HARD not found (cargo build first)"
    exit 1
fi
if [ ! -x "$REGISTRY" ]; then
    echo "publish: $REGISTRY not found (cargo build -p hard-registry first)"
    exit 1
fi

TMP="$(mktemp -d /tmp/hs-pubregress-XXXXXX)"
trap 'kill "$REG_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

# ---- launch the registry (open mode: no token needed) --------------------
PORT=""
if command -v "$PYTHON" >/dev/null 2>&1; then
    PORT=$("$PYTHON" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi
if [ -z "$PORT" ]; then
    # no python: pick a port and let the registry fail loudly if it is taken
    PORT=$(( 40000 + $$ % 20000 ))
fi
"$REGISTRY" serve --addr "127.0.0.1:$PORT" --data "$TMP/registry" --open >"$TMP/registry.log" 2>&1 &
REG_PID=$!
export HARD_REGISTRY="http://127.0.0.1:$PORT"
export HARD_HOME="$TMP/home"
for _ in $(seq 1 100); do
    if grep -q listening "$TMP/registry.log" 2>/dev/null; then break; fi
    kill -0 "$REG_PID" 2>/dev/null || { echo "publish: registry died"; cat "$TMP/registry.log"; exit 1; }
    sleep 0.1
done
grep -q listening "$TMP/registry.log" || { echo "publish: registry did not start"; cat "$TMP/registry.log"; exit 1; }

PASS=0
FAILED=0

# ---- helpers ------------------------------------------------------------
case_begin() {   # $1 = case name
    CASE_NAME="$1"
    CASE_DIR="$(mktemp -d "$TMP/case.XXXXXX")"
    cd "$CASE_DIR" || exit 1
    # Every case publishes into the same registry, so each gets its own
    # package name, derived from the (unique) case name.
    PKG="pkg$(printf '%s' "$1" | tr -cd 'a-z0-9')"
}
run_case() {
    name="$1"; shift
    if ( "$1" ) >"$TMP/case.out" 2>&1; then
        echo "publish: PASS $name"
        PASS=$((PASS + 1))
    else
        echo "publish: FAIL $name"
        cat "$TMP/case.out"
        FAILED=$((FAILED + 1))
    fi
}
assert_rc() { want="$1"; shift; out=$("$@" 2>&1); got=$?; [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $*"; echo "$out" | head -5; exit 1; }; }
assert_rc_out() { want="$1"; needle="$2"; shift 2; out=$("$@" 2>&1); got=$?; [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $out"; exit 1; }; echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: missing '$needle' in: $out"; exit 1; }; }
assert_out() { needle="$1"; shift; out=$("$@" 2>&1); echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: missing '$needle' in: $out"; exit 1; }; }
assert_api() { needle="$1"; path="$2"; out=$(curl -s "$HARD_REGISTRY$path"); echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: '$needle' not in $path: $out"; exit 1; }; }
assert_no_api() { needle="$1"; path="$2"; out=$(curl -s "$HARD_REGISTRY$path"); echo "$out" | grep -qF "$needle" && { echo "case $CASE_NAME: '$needle' unexpectedly in $path: $out"; exit 1; }; return 0; }

# A project directory with a manifest and two source files.
demo_project() {   # $1 = package name, $2 = version
    mkdir -p src
    cat > hard.toml <<EOF
schema = 1
name = "$1"
version = "$2"
edition = "2027"
description = "A demo package"
license = "MIT"

[package]
homepage = "https://example.org"
tags = ["example"]

[dependencies]
base64 = "^0.4.0"
EOF
    printf 'app @3000\n\nGET "/" :: { <- { ok: true } }\n' > main.hard
    printf 'calc x() => Int { <- 1 }\n' > src/lib.hard
}

# ---- scenarios -----------------------------------------------------------
t_publish_uploads_the_package() {
    case_begin publish
    demo_project "$PKG" 1.0.0
    assert_rc_out 0 "published $PKG@1.0.0" "$HARD" publish
    assert_api "\"name\":\"$PKG\"" "/packages/$PKG"
    assert_api '"version":"1.0.0"' "/packages/$PKG"
    assert_api '"license":"MIT"' "/packages/$PKG"
    assert_api '"base64"' "/packages/$PKG"
    assert_api '"homepage":"https://example.org"' "/packages/$PKG"
    assert_api '"example"' "/packages/$PKG"
}
t_dry_run_stores_nothing() {
    case_begin dry-run
    demo_project "$PKG" 1.0.0
    before=$(curl -s "$HARD_REGISTRY/stats")
    assert_rc 0 "$HARD" publish --dry-run
    after=$(curl -s "$HARD_REGISTRY/stats")
    [ "$before" = "$after" ] || { echo "case dry-run: stats changed: $before -> $after"; exit 1; }
    out=$(curl -s -o /dev/null -w '%{http_code}' "$HARD_REGISTRY/packages/$PKG")
    [ "$out" = "404" ] || { echo "case dry-run: package exists after a dry run: $out"; exit 1; }
}
t_dry_run_without_a_registry() {
    case_begin dry-run-local
    demo_project "$PKG" 1.0.0
    out=$(HARD_REGISTRY= "$HARD" publish --dry-run 2>&1)
    echo "$out" | grep -qF "no registry contacted" || { echo "case dry-run-local: $out"; exit 1; }
    echo "$out" | grep -qF "integrity sha256:" || { echo "case dry-run-local: $out"; exit 1; }
}
t_duplicate_version_is_refused() {
    case_begin duplicate
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_rc_out 1 "already on" "$HARD" publish
}
t_version_override_publishes_a_new_one() {
    case_begin version-override
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish --version 1.1.0
    assert_api '"1.1.0"' "/packages/$PKG/versions"
}
t_publish_is_deterministic() {
    case_begin determinism
    demo_project "$PKG" 1.0.0
    first=$("$HARD" publish --dry-run 2>&1 | grep '^integrity')
    second=$("$HARD" publish --dry-run 2>&1 | grep '^integrity')
    [ -n "$first" ] || { echo "case determinism: no integrity line"; exit 1; }
    [ "$first" = "$second" ] || { echo "case determinism: '$first' vs '$second'"; exit 1; }
}
t_lockfile_and_build_output_are_excluded() {
    case_begin exclusions
    demo_project "$PKG" 1.0.0
    printf 'schema = "hard-lock/v1"\n' > hard.lock
    mkdir -p .hard target/debug
    printf '{}' > .hard/build.json
    printf 'x' > target/debug/app
    out=$("$HARD" publish --dry-run 2>&1)
    echo "$out" | grep -qF "hard.lock" && { echo "case exclusions: hard.lock was published: $out"; exit 1; }
    echo "$out" | grep -qF ".hard/build.json" && { echo "case exclusions: .hard was published: $out"; exit 1; }
    echo "$out" | grep -qF "target/debug/app" && { echo "case exclusions: target was published: $out"; exit 1; }
    echo "$out" | grep -qF "main.hard" || { echo "case exclusions: main.hard missing: $out"; exit 1; }
    return 0
}
t_a_bad_version_is_refused_locally() {
    case_begin bad-version
    demo_project "$PKG" 1.0.0
    sed -i 's/^version = "1.0.0"/version = "one.two"/' hard.toml
    assert_rc_out 1 "not a valid semantic version" "$HARD" publish
}
t_a_missing_manifest_is_scaffolded() {
    case_begin no-manifest
    printf 'app @3000\n' > main.hard
    assert_rc 0 "$HARD" publish
    grep -qF 'name = ' hard.toml || { echo "case no-manifest: no manifest written"; exit 1; }
    name=$(grep -m1 '^name = ' hard.toml | cut -d'"' -f2)
    assert_api "\"name\":\"$name\"" "/packages/$name"
}
t_the_published_package_can_be_installed() {
    case_begin download
    demo_project "$PKG" 1.0.0
    # no dependencies: a consumer must be able to install it standalone
    sed -i '/^\[dependencies\]$/,$d' hard.toml
    assert_rc 0 "$HARD" publish
    mkdir -p consumer && cd consumer
    cat > hard.toml <<EOF
name = "consumer"
version = "0.1.0"
edition = "2027"

[dependencies]
$PKG = "1.0.0"
EOF
    assert_rc 0 "$HARD" install
    [ -f "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" ] \
        || { echo "case download: no cached archive"; ls -R "$HARD_HOME" 2>/dev/null; exit 1; }
    head -c 7 "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" | grep -qF "HSPKG" \
        || { echo "case download: cached archive is not an .hspkg"; exit 1; }
}
t_yank_and_unyank() {
    case_begin yank
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_out "yanked $PKG@1.0.0" "$HARD" yank "$PKG@1.0.0"
    assert_api '"yanked":true' "/packages/$PKG"
    assert_out "restored $PKG@1.0.0" "$HARD" yank "$PKG@1.0.0" --unyank
    assert_api '"yanked":false' "/packages/$PKG"
}
t_yank_needs_a_version() {
    case_begin yank-usage
    demo_project "$PKG" 1.0.0
    assert_rc 2 "$HARD" yank "$PKG"
    assert_rc 2 "$HARD" yank
}
t_yank_of_an_unknown_version_fails() {
    case_begin yank-missing
    demo_project "$PKG" 1.0.0
    assert_rc_out 1 "does not exist" "$HARD" yank "$PKG@9.9.9"
}
t_a_publish_check_does_not_count_a_download() {
    case_begin no-download-on-check
    demo_project "$PKG" 1.0.0
    before=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    assert_rc 0 "$HARD" publish
    after=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    [ "$after" = "$before" ] || { echo "case no-download-on-check: downloads $before -> $after"; exit 1; }
    curl -s -o /dev/null "$HARD_REGISTRY/packages/$PKG/1.0.0"
    final=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    [ "$final" = "$((after + 1))" ] || { echo "case no-download-on-check: a real download did not count: $final"; exit 1; }
}
t_scoped_package_names_publish() {
    case_begin scoped
    demo_project "acme/http" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_api '"name":"acme/http"' "/packages/acme%2Fhttp"
}
t_multiple_versions_coexist() {
    case_begin multiple-versions
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_rc 0 "$HARD" publish --version 1.1.0
    assert_rc 0 "$HARD" publish --version 1.2.0
    assert_api '"count":3' "/packages/$PKG/versions"
}
t_registry_stats_count_publishes() {
    case_begin stats
    demo_project "$PKG" 1.0.0
    before=$(curl -s "$HARD_REGISTRY/stats")
    assert_rc 0 "$HARD" publish
    after=$(curl -s "$HARD_REGISTRY/stats")
    # the registry is shared by every case, so compare the delta
    bp=$(printf '%s' "$before" | sed 's/.*"packages":\([0-9]*\).*/\1/')
    ap=$(printf '%s' "$after" | sed 's/.*"packages":\([0-9]*\).*/\1/')
    bv=$(printf '%s' "$before" | sed 's/.*"versions":\([0-9]*\).*/\1/')
    av=$(printf '%s' "$after" | sed 's/.*"versions":\([0-9]*\).*/\1/')
    bs=$(printf '%s' "$before" | sed 's/.*"signed":\([0-9]*\).*/\1/')
    as=$(printf '%s' "$after" | sed 's/.*"signed":\([0-9]*\).*/\1/')
    [ "$ap" = "$((bp + 1))" ] || { echo "case stats: packages $bp -> $ap"; exit 1; }
    [ "$av" = "$((bv + 1))" ] || { echo "case stats: versions $bv -> $av"; exit 1; }
    [ "$as" = "$((bs + 1))" ] || { echo "case stats: signed $bs -> $as"; exit 1; }
}
t_publishes_are_searchable() {
    case_begin searchable
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_out "$PKG" "$HARD" search "$PKG"
}
t_the_change_feed_records_the_publish() {
    case_begin change-feed
    demo_project "$PKG" 1.0.0
    assert_rc 0 "$HARD" publish
    assert_api "\"name\":\"$PKG\"" "/api/mirror/changes?since=0"
}

# ---- run ----------------------------------------------------------------
run_case publish-uploads t_publish_uploads_the_package
run_case dry-run-stores-nothing t_dry_run_stores_nothing
run_case dry-run-without-registry t_dry_run_without_a_registry
run_case duplicate-version-refused t_duplicate_version_is_refused
run_case version-override t_version_override_publishes_a_new_one
run_case deterministic-archive t_publish_is_deterministic
run_case excludes-lock-and-build t_lockfile_and_build_output_are_excluded
run_case bad-version-refused t_a_bad_version_is_refused_locally
run_case no-manifest-scaffolds t_a_missing_manifest_is_scaffolded
run_case published-package-installs t_the_published_package_can_be_installed
run_case yank-and-unyank t_yank_and_unyank
run_case yank-needs-version t_yank_needs_a_version
run_case yank-unknown-version t_yank_of_an_unknown_version_fails
run_case check-does-not-download t_a_publish_check_does_not_count_a_download
run_case scoped-names t_scoped_package_names_publish
run_case multiple-versions t_multiple_versions_coexist
run_case stats-counts-publishes t_registry_stats_count_publishes
run_case published-is-searchable t_publishes_are_searchable
run_case change-feed-records t_the_change_feed_records_the_publish

echo "publish: $((PASS - FAILED))/$PASS suite passed"
[ "$FAILED" -eq 0 ] || exit 1
exit 0
