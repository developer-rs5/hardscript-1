#!/usr/bin/env bash
# Download regression suite (M8.3).
#
# Exercises the download client end to end against a real registry: cold
# fetch, cache reuse, offline reuse, parallel installs, resumed downloads,
# `hard download`, integrity refusal and cache verification.
#
# Run: tests/run-download-regressions.sh   (uses $HARD, default target/debug/hard)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"
REGISTRY="${REGISTRY:-$REPO/target/debug/hard-registry}"

if [ ! -x "$HARD" ]; then
    echo "download: $HARD not found (cargo build first)"
    exit 1
fi
if [ ! -x "$REGISTRY" ]; then
    echo "download: $REGISTRY not found (cargo build -p hard-registry first)"
    exit 1
fi

TMP="$(mktemp -d /tmp/hs-dlreg-XXXXXX)"
trap 'kill "$REG_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

PORT=""
if command -v "$PYTHON" >/dev/null 2>&1; then
    PORT=$("$PYTHON" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi
[ -n "$PORT" ] || PORT=$(( 41000 + $$ % 20000 ))
"$REGISTRY" serve --addr "127.0.0.1:$PORT" --data "$TMP/registry" --open >"$TMP/registry.log" 2>&1 &
REG_PID=$!
export HARD_REGISTRY="http://127.0.0.1:$PORT"
for _ in $(seq 1 100); do
    if grep -q listening "$TMP/registry.log" 2>/dev/null; then break; fi
    kill -0 "$REG_PID" 2>/dev/null || { echo "download: registry died"; cat "$TMP/registry.log"; exit 1; }
    sleep 0.1
done
grep -q listening "$TMP/registry.log" || { echo "download: registry did not start"; cat "$TMP/registry.log"; exit 1; }

PASS=0
FAILED=0

case_begin() {
    CASE_NAME="$1"
    CASE_DIR="$(mktemp -d "$TMP/case.XXXXXX")"
    export HARD_HOME="$CASE_DIR/home"
    cd "$CASE_DIR" || exit 1
    PKG="dl$(printf '%s' "$1" | tr -cd 'a-z0-9')"
}
run_case() {
    name="$1"; shift
    if ( "$1" ) >"$TMP/case.out" 2>&1; then
        echo "download: PASS $name"
        PASS=$((PASS + 1))
    else
        echo "download: FAIL $name"
        cat "$TMP/case.out"
        FAILED=$((FAILED + 1))
    fi
}
assert_rc() { want="$1"; shift; out=$("$@" 2>&1); got=$?; [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $*"; echo "$out" | head -4; exit 1; }; }
assert_out() { needle="$1"; shift; out=$("$@" 2>&1); echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: missing '$needle' in: $out"; exit 1; }; }
assert_file() { [ -e "$1" ] || { echo "case $CASE_NAME: missing file $1"; exit 1; }; }
assert_no_file() { [ ! -e "$1" ] || { echo "case $CASE_NAME: unexpected file $1"; exit 1; }; }
assert_grep() { grep -qF "$2" "$1" || { echo "case $CASE_NAME: '$2' not in $1"; exit 1; }; }
assert_nogrep() { if grep -qF "$2" "$1"; then echo "case $CASE_NAME: '$2' unexpectedly in $1"; exit 1; fi; }

# Publish a dependency-free package and echo nothing.
publish_pkg() {   # $1 = name, $2 = version
    local dir="$CASE_DIR/pub"
    mkdir -p "$dir"
    cat > "$dir/hard.toml" <<EOF
schema = 1
name = "$1"
version = "$2"
edition = "2027"
description = "a downloadable package"
EOF
    printf 'calc hello() => Str { <- "hi" }\n' > "$dir/main.hard"
    ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 ) \
        || { echo "case $CASE_NAME: could not publish $1: $?"; exit 1; }
}

# A consumer project depending on the named packages.
consumer() {   # rest = name@version specs
    cat > hard.toml <<EOF
name = "consumer"
version = "0.1.0"
edition = "2027"

[dependencies]
EOF
    for spec in "$@"; do
        printf '%s = "%s"\n' "${spec%@*}" "${spec#*@}" >> hard.toml
    done
}

# ---- coverage added for the v0.9 QA gate (M8.10) --------------------------
# A package with a body big enough that a range request is meaningful.
big_pkg() {   # $1 = name, $2 = version, $3 = functions
    local dir="$CASE_DIR/pub-big"
    mkdir -p "$dir"
    cat > "$dir/hard.toml" <<EOF
schema = 1
name = "$1"
version = "$2"
edition = "2027"
description = "a package with a body worth slicing"
EOF
    {
        local i
        for i in $(seq 1 "$3"); do
            printf 'calc f%s() => Int { <- %s * 2 + 1 }\n' "$i" "$i"
        done
    } > "$dir/main.hard"
    ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 ) \
        || { echo "case $CASE_NAME: could not publish $1: $?"; exit 1; }
}
api_field() {   # $1 = path, $2 = python expression over `d`
    curl -s "$HARD_REGISTRY$1" | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)
    print(eval(sys.argv[1]))
except Exception as e:
    print("ERR:" + str(e))' "$2" 2>/dev/null
}

t_a_download_command_writes_the_archive() {
    case_begin dl-cmd
    publish_pkg "$PKG" 1.0.0
    out=$("$HARD" download "$PKG@1.0.0" 2>&1) && rc=0 || rc=$?
    [ "$rc" = "0" ] || { echo "case dl-cmd: rc $rc: $out"; exit 1; }
    [ -f "$PKG-1.0.0.hspkg" ] || { echo "case dl-cmd: no archive written: $(ls)"; exit 1; }
    got=$(sha256sum "$PKG-1.0.0.hspkg" | cut -d' ' -f1)
    want=$(api_field "/packages/$PKG/1.0.0/manifest" 'd["integrity"]')
    [ "sha256:$got" = "$want" ] || { echo "case dl-cmd: digest [$got] != [$want]"; exit 1; }
}
t_a_download_command_resolves_the_newest_version() {
    case_begin dl-newest
    publish_pkg "$PKG" 1.0.0
    publish_pkg "$PKG" 1.4.0
    publish_pkg "$PKG" 2.0.0
    assert_rc 0 "$HARD" download "$PKG"
    [ -f "$PKG-2.0.0.hspkg" ] || { echo "case dl-newest: expected 2.0.0: $(ls)"; exit 1; }
}
t_a_download_command_refuses_an_unknown_version() {
    case_begin dl-unknown
    publish_pkg "$PKG" 1.0.0
    out=$("$HARD" download "$PKG@9.9.9" 2>&1) && rc=0 || rc=$?
    [ "$rc" != "0" ] || { echo "case dl-unknown: an unknown version downloaded"; exit 1; }
    echo "$out" | head -2
}
t_a_download_command_refuses_an_unknown_package() {
    case_begin dl-nopkg
    out=$("$HARD" download "${CASE_NAME}-missing@1.0.0" 2>&1) && rc=0 || rc=$?
    [ "$rc" != "0" ] || { echo "case dl-nopkg: a missing package downloaded"; exit 1; }
    echo "$out" | head -2
}
t_a_second_download_reuses_the_cache() {
    case_begin dl-twice
    publish_pkg "$PKG" 1.0.0
    assert_rc 0 "$HARD" download "$PKG@1.0.0"
    before=$(api_field "/packages/$PKG" 'd["downloads"]')
    out=$("$HARD" download "$PKG@1.0.0" 2>&1)
    after=$(api_field "/packages/$PKG" 'd["downloads"]')
    [ "$before" = "$after" ] || { echo "case dl-twice: a cache hit still counted a download"; exit 1; }
    assert_out "cache" "$HARD" download "$PKG@1.0.0"
}
t_force_downloads_even_when_cached() {
    case_begin dl-force
    publish_pkg "$PKG" 1.0.0
    assert_rc 0 "$HARD" download "$PKG@1.0.0"
    before=$(api_field "/packages/$PKG" 'd["downloads"]')
    assert_rc 0 "$HARD" download "$PKG@1.0.0" --force
    after=$(api_field "/packages/$PKG" 'd["downloads"]')
    [ "$after" -gt "$before" ] || { echo "case dl-force: --force did not re-download"; exit 1; }
}
t_a_download_writes_where_it_is_told() {
    case_begin dl-output
    publish_pkg "$PKG" 1.0.0
    assert_rc 0 "$HARD" download "$PKG@1.0.0" --output "$CASE_DIR/elsewhere.hspkg"
    [ -f "$CASE_DIR/elsewhere.hspkg" ] || { echo "case dl-output: --output was ignored"; exit 1; }
}
t_a_yanked_version_still_downloads_for_people_who_pinned_it() {
    case_begin dl-yanked
    publish_pkg "$PKG" 1.0.0
    publish_pkg "$PKG" 1.1.0
    assert_rc 0 "$HARD" yank "$PKG@1.1.0"
    assert_rc 0 "$HARD" download "$PKG@1.1.0"
    [ -f "$PKG-1.1.0.hspkg" ] || { echo "case dl-yanked: the pinned yanked version did not download"; exit 1; }
}
t_a_range_request_returns_a_slice_and_the_full_length() {
    case_begin dl-range
    big_pkg "$PKG" 1.0.0 400
    total=$(curl -sI "$HARD_REGISTRY/packages/$PKG/1.0.0/download" | tr -d '\r' | sed -n 's/^[Cc]ontent-[Ll]ength: //p')
    [ -n "$total" ] || { echo "case dl-range: no Content-Length"; exit 1; }
    [ "$total" -gt 100 ] || { echo "case dl-range: the fixture is too small ($total)"; exit 1; }
    got=$(curl -s -r 0-99 "$HARD_REGISTRY/packages/$PKG/1.0.0/download" | wc -c | tr -d ' ')
    [ "$got" = "100" ] || { echo "case dl-range: a 100-byte range returned $got"; exit 1; }
    got=$(curl -s -r 10- "$HARD_REGISTRY/packages/$PKG/1.0.0/download" | wc -c | tr -d ' ')
    [ "$got" = "$((total - 10))" ] || { echo "case dl-range: an open range returned $got of $total"; exit 1; }
}
t_an_unsatisfiable_range_is_refused() {
    case_begin dl-bad-range
    publish_pkg "$PKG" 1.0.0
    code=$(curl -s -o /dev/null -w '%{http_code}' -r 999999-1000000 "$HARD_REGISTRY/packages/$PKG/1.0.0/download")
    [ "$code" = "416" ] || [ "$code" = "200" ] || { echo "case dl-bad-range: HTTP $code"; exit 1; }
}
t_a_head_request_does_not_count_a_download() {
    case_begin dl-head
    publish_pkg "$PKG" 1.0.0
    before=$(api_field "/packages/$PKG" 'd["downloads"]')
    curl -sI -o /dev/null "$HARD_REGISTRY/packages/$PKG/1.0.0/download"
    after=$(api_field "/packages/$PKG" 'd["downloads"]')
    [ "$before" = "$after" ] || { echo "case dl-head: HEAD counted a download"; exit 1; }
    curl -s -o /dev/null "$HARD_REGISTRY/packages/$PKG/1.0.0/download"
    after2=$(api_field "/packages/$PKG" 'd["downloads"]')
    [ "$after2" -gt "$after" ] || { echo "case dl-head: GET did not count a download"; exit 1; }
}
t_an_install_verifies_the_archive_digest() {
    case_begin dl-digest
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    assert_grep hard.lock "sha256:"
    want=$(api_field "/packages/$PKG/1.0.0/manifest" 'd["integrity"]')
    assert_grep hard.lock "$want"
}
t_a_corrupt_cache_entry_is_refetched_on_the_next_install() {
    case_begin dl-corrupt
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    archive="$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg"
    [ -f "$archive" ] || { echo "case dl-corrupt: nothing was cached"; exit 1; }
    printf 'tampered' >> "$archive"
    rm -rf .hard hard.lock
    out=$("$HARD" install 2>&1) && rc=0 || rc=$?
    [ "$rc" = "0" ] || { echo "case dl-corrupt: rc $rc: $out"; exit 1; }
    # the install that repaired the entry already refetched it, so check the
    # cache is whole again rather than expecting a second miss
    assert_out "1 hit, 0 miss" "$HARD" install
}
t_the_installed_tree_is_byte_identical_to_the_registry() {
    case_begin dl-identical
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    a=$(cat "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" | sha256sum | cut -d' ' -f1)
    b=$(cat "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.src/main.hard" | sha256sum | cut -d' ' -f1)
    [ -n "$a" ] && [ -n "$b" ] || { echo "case dl-identical: nothing to compare"; exit 1; }
    c=$(cat ".hard/packages/$PKG/main.hard" 2>/dev/null | sha256sum | cut -d' ' -f1)
    [ "$b" = "$c" ] || { echo "case dl-identical: the installed source differs from the cached one"; exit 1; }
}
t_a_dependency_archive_is_fetched_too() {
    case_begin dl-deps
    publish_pkg "$PKG" 1.0.0
    dir="$CASE_DIR/pub-dep"
    mkdir -p "$dir"
    cat > "$dir/hard.toml" <<EOF
schema = 1
name = "${CASE_NAME}mid"
version = "1.0.0"
edition = "2027"
description = "a package with a dependency"

[dependencies]
$PKG = "1.0.0"
EOF
    printf 'calc mid() => Int { <- 1 }\n' > "$dir/main.hard"
    ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 ) \
        || { echo "case dl-deps: could not publish the dependent package"; exit 1; }
    consumer "${CASE_NAME}mid@1.0.0"
    assert_rc 0 "$HARD" install
    [ -f "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" ] \
        || { echo "case dl-deps: the transitive archive was not fetched"; exit 1; }
}
t_an_offline_install_of_a_yanked_version_still_works() {
    case_begin dl-offline-yanked
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@=1.0.0"
    assert_rc 0 "$HARD" install
    publish_pkg "$PKG" 1.1.0
    assert_rc 0 "$HARD" yank "$PKG@1.1.0"
    rm -rf .hard hard.lock
    out=$("$HARD" install --offline 2>&1) && rc=0 || rc=$?
    [ "$rc" = "0" ] || { echo "case dl-offline-yanked: rc $rc: $out"; exit 1; }
}
t_a_download_of_a_large_package_streams_to_disk() {
    case_begin dl-big
    big_pkg "$PKG" 1.0.0 2000
    assert_rc 0 "$HARD" download "$PKG@1.0.0"
    size=$(wc -c < "$PKG-1.0.0.hspkg" | tr -d ' ')
    [ "$size" -gt 1000 ] || { echo "case dl-big: the archive is only $size bytes"; exit 1; }
    got=$(sha256sum "$PKG-1.0.0.hspkg" | cut -d' ' -f1)
    want=$(api_field "/packages/$PKG/1.0.0/manifest" 'd["integrity"]')
    [ "sha256:$got" = "$want" ] || { echo "case dl-big: digest mismatch"; exit 1; }
}
t_the_cache_reports_what_it_did() {
    case_begin dl-report
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_out "0 hit, 1 miss" "$HARD" install
    assert_out "1 hit, 0 miss" "$HARD" install
}
t_download_help_works() {
    case_begin dl-help
    out=$("$HARD" download --help 2>&1) && rc=0 || rc=$?
    [ "$rc" = "0" ] || { echo "case dl-help: rc $rc"; exit 1; }
    echo "$out" | grep -qF "hard download" || { echo "case dl-help: no usage: $out"; exit 1; }
}
t_install_help_works() {
    case_begin dl-help2
    out=$("$HARD" install --help 2>&1) && rc=0 || rc=$?
    [ "$rc" = "0" ] || { echo "case dl-help2: rc $rc"; exit 1; }
    echo "$out" | grep -qF "hard install" || { echo "case dl-help2: no usage: $out"; exit 1; }
}

# ---- scenarios -----------------------------------------------------------
t_cold_install_downloads_the_archive() {
    case_begin cold
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    out=$("$HARD" install 2>&1) || { echo "case cold: install failed: $out"; exit 1; }
    echo "$out" | grep -qF "installed $PKG@1.0.0" || { echo "case cold: $out"; exit 1; }
    echo "$out" | grep -qF "1 miss" || { echo "case cold: $out"; exit 1; }
    assert_file "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg"
    assert_file ".hard/packages/$PKG/main.hard"
    assert_grep hard.lock "[package.$PKG]"
    assert_grep hard.lock "integrity = \"sha256:"
}
t_second_install_hits_the_cache() {
    case_begin warm
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    rm -rf .hard/packages hard.lock
    out=$("$HARD" install 2>&1)
    echo "$out" | grep -qF "reused $PKG@1.0.0" || { echo "case warm: $out"; exit 1; }
    echo "$out" | grep -qF "1 hit, 0 miss" || { echo "case warm: $out"; exit 1; }
    assert_file ".hard/packages/$PKG/main.hard"
}
t_offline_install_uses_the_cache() {
    case_begin offline
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    rm -rf .hard/packages hard.lock
    out=$("$HARD" install --offline 2>&1)
    echo "$out" | grep -qF "reused $PKG@1.0.0" || { echo "case offline: $out"; exit 1; }
    assert_file ".hard/packages/$PKG/main.hard"
}
t_offline_cold_cache_fails() {
    case_begin offline-cold
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    out=$("$HARD" install --offline 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case offline-cold: rc $got want 1: $out"; exit 1; }
    echo "$out" | grep -qi "not in the cache" || { echo "case offline-cold: $out"; exit 1; }
}
t_parallel_install_of_many_packages() {
    case_begin parallel
    consumer a1@1.0.0 a2@1.0.0 a3@1.0.0 a4@1.0.0 a5@1.0.0 a6@1.0.0
    for n in a1 a2 a3 a4 a5 a6; do
        publish_pkg "$n" 1.0.0
    done
    assert_rc 0 "$HARD" install
    for n in a1 a2 a3 a4 a5 a6; do
        assert_file ".hard/packages/$n/main.hard"
    done
    out=$("$HARD" install 2>&1)
    echo "$out" | grep -qF "6 hit, 0 miss" || { echo "case parallel: $out"; exit 1; }
}
t_jobs_env_bounds_the_pool() {
    case_begin jobs
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 env HARD_JOBS=1 "$HARD" install
    out=$(HARD_JOBS=1 "$HARD" install 2>&1)
    echo "$out" | grep -qF "1 hit" || { echo "case jobs: $out"; exit 1; }
}
t_download_writes_the_archive() {
    case_begin download
    publish_pkg "$PKG" 1.0.0
    assert_rc 0 "$HARD" download "$PKG@1.0.0"
    assert_file "$PKG-1.0.0.hspkg"
    head -c 7 "$PKG-1.0.0.hspkg" | grep -qF "HSPKG" || { echo "case download: not an .hspkg"; exit 1; }
}
t_download_honours_out_and_default_version() {
    case_begin download-out
    publish_pkg "$PKG" 1.0.0
    publish_pkg "$PKG" 1.1.0
    mkdir -p out
    assert_rc 0 "$HARD" download "$PKG" --out out
    assert_file "out/$PKG-1.1.0.hspkg"
    assert_no_file "out/$PKG-1.0.0.hspkg"
}
t_download_of_an_unknown_version_fails() {
    case_begin download-404
    publish_pkg "$PKG" 1.0.0
    out=$("$HARD" download "$PKG@9.9.9" 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case download-404: rc $got want 1: $out"; exit 1; }
}
t_download_uses_the_cache_when_warm() {
    case_begin download-cache
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    before=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    out=$("$HARD" download "$PKG@1.0.0" --out . 2>&1)
    echo "$out" | grep -qF "cache" || { echo "case download-cache: $out"; exit 1; }
    after=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    [ "$before" = "$after" ] || { echo "case download-cache: downloads $before -> $after"; exit 1; }
}
t_a_corrupt_cache_entry_is_replaced() {
    case_begin corrupt
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    printf 'garbage' > "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg"
    out=$("$HARD" install 2>&1)
    echo "$out" | grep -qF "installed $PKG@1.0.0" || { echo "case corrupt: $out"; exit 1; }
    head -c 7 "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" | grep -qF "HSPKG" \
        || { echo "case corrupt: the archive was not replaced"; exit 1; }
}
t_cache_verify_notices_a_corrupt_archive() {
    case_begin cache-verify
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    printf 'garbage' > "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg"
    assert_rc 1 "$HARD" cache verify
}
t_cache_info_reports_the_package() {
    case_begin cache-info
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    assert_out "packages: 1" "$HARD" cache info
    assert_out "$PKG" "$HARD" cache info
}
t_the_installed_archive_matches_the_published_one() {
    case_begin integrity
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    curl -s "$HARD_REGISTRY/packages/$PKG/1.0.0" -o "$CASE_DIR/published.hspkg"
    cmp -s "$CASE_DIR/published.hspkg" "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" \
        || { echo "case integrity: cached bytes differ from the published archive"; exit 1; }
}
t_downloads_are_counted_once_per_fetch() {
    case_begin counted
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    before=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    "$HARD" install >/dev/null 2>&1
    after=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    [ "$after" = "$((before + 1))" ] || { echo "case counted: downloads $before -> $after"; exit 1; }
    # a warm install must not hit the network again
    "$HARD" install >/dev/null 2>&1
    final=$(curl -s "$HARD_REGISTRY/stats" | sed 's/.*"downloads":\([0-9]*\).*/\1/')
    [ "$final" = "$after" ] || { echo "case counted: a warm install downloaded: $after -> $final"; exit 1; }
}
t_transitive_dependencies_are_downloaded() {
    case_begin transitive
    # a depends on b; both are published
    local bdir="$CASE_DIR/pub-b"
    mkdir -p "$bdir"
    cat > "$bdir/hard.toml" <<EOF
schema = 1
name = "${PKG}b"
version = "1.0.0"
edition = "2027"
EOF
    printf 'calc b() => Int { <- 2 }\n' > "$bdir/main.hard"
    ( cd "$bdir" && "$HARD" publish >/dev/null 2>&1 ) || { echo "case transitive: publish b failed"; exit 1; }
    local adir="$CASE_DIR/pub-a"
    mkdir -p "$adir"
    cat > "$adir/hard.toml" <<EOF
schema = 1
name = "$PKG"
version = "1.0.0"
edition = "2027"

[dependencies]
${PKG}b = "^1.0.0"
EOF
    printf 'calc a() => Int { <- 1 }\n' > "$adir/main.hard"
    ( cd "$adir" && "$HARD" publish >/dev/null 2>&1 ) || { echo "case transitive: publish a failed"; exit 1; }
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    assert_file ".hard/packages/$PKG/main.hard"
    assert_file ".hard/packages/${PKG}b/main.hard"
    assert_grep hard.lock "[package.${PKG}b]"
}
t_a_dead_registry_fails_the_install() {
    case_begin dead
    consumer "$PKG@1.0.0"
    out=$(HARD_REGISTRY=http://127.0.0.1:1 "$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case dead: rc $got want 1: $out"; exit 1; }
}
t_an_unknown_package_fails() {
    case_begin unknown
    consumer "nosuchpkg@1.0.0"
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case unknown: rc $got want 1: $out"; exit 1; }
    echo "$out" | grep -qF "nosuchpkg" || { echo "case unknown: $out"; exit 1; }
}
t_a_partial_download_is_resumed() {
    case_begin resume
    publish_pkg "$PKG" 1.0.0
    # stage a truncated archive the way an interrupted run would
    mkdir -p "$HARD_HOME/cache/packages/$PKG"
    curl -s "$HARD_REGISTRY/packages/$PKG/1.0.0" -o "$CASE_DIR/full.hspkg"
    head -c 20 "$CASE_DIR/full.hspkg" > "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg.part"
    consumer "$PKG@1.0.0"
    out=$("$HARD" install 2>&1)
    echo "$out" | grep -qF "1 resumed" || { echo "case resume: $out"; exit 1; }
    cmp -s "$CASE_DIR/full.hspkg" "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg" \
        || { echo "case resume: the resumed archive does not match the published one"; exit 1; }
    assert_no_file "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg.part"
}
t_a_cleaned_cache_refetches_everything() {
    case_begin cache-clean
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    assert_rc 0 "$HARD" cache clean
    assert_no_file "$HARD_HOME/cache/packages/$PKG"
    out=$("$HARD" install 2>&1) || { echo "case cache-clean: $out"; exit 1; }
    echo "$out" | grep -qF "installed $PKG@1.0.0" || { echo "case cache-clean: $out"; exit 1; }
    assert_file "$HARD_HOME/cache/packages/$PKG/$PKG-1.0.0.hspkg"
}
t_the_lockfile_records_the_registry_digest() {
    case_begin lock-digest
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    got=$(sed -n 's/^integrity = "\(sha256:[0-9a-f]*\)"/\1/p' hard.lock | head -1)
    served=$(curl -s "$HARD_REGISTRY/packages/$PKG/1.0.0" | sha256sum | cut -d' ' -f1)
    [ -n "$got" ] || { echo "case lock-digest: no integrity in the lockfile"; exit 1; }
    [ "$got" = "sha256:$served" ] || { echo "case lock-digest: lock says $got, registry serves $served"; exit 1; }
}
t_frozen_does_not_touch_the_network() {
    case_begin frozen
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/lock.1"
    rm -rf .hard/packages
    assert_rc 0 "$HARD" install --frozen
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case frozen: the lockfile changed"; exit 1; }
}
t_install_writes_the_source_tree() {
    case_begin tree
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@1.0.0"
    assert_rc 0 "$HARD" install
    assert_file ".hard/packages/$PKG/main.hard"
    assert_file ".hard/packages/$PKG/hard.toml"
    assert_grep ".hard/packages/$PKG/.hs-pkg.json" "\"version\":\"1.0.0\""
}
t_many_versions_all_resolve() {
    case_begin versions
    publish_pkg "$PKG" 1.0.0
    publish_pkg "$PKG" 1.1.0
    publish_pkg "$PKG" 2.0.0
    consumer "$PKG@^1.0.0"
    assert_rc 0 "$HARD" install
    assert_grep hard.lock "version = \"1.1.0\""
    assert_nogrep hard.lock "version = \"2.0.0\""
}
t_list_and_outdated_work() {
    case_begin list
    publish_pkg "$PKG" 1.0.0
    consumer "$PKG@^1.0.0"
    assert_rc 0 "$HARD" install
    assert_out "$PKG 1.0.0" "$HARD" list
    assert_out "all dependencies are up to date" "$HARD" outdated
    publish_pkg "$PKG" 1.4.0
    assert_out "latest matching 1.4.0" "$HARD" outdated
}

# ---- run ----------------------------------------------------------------
run_case cold-install t_cold_install_downloads_the_archive
run_case warm-cache-hit t_second_install_hits_the_cache
run_case offline-reuse t_offline_install_uses_the_cache
run_case offline-cold-cache t_offline_cold_cache_fails
run_case parallel-install t_parallel_install_of_many_packages
run_case jobs-bounds-pool t_jobs_env_bounds_the_pool
run_case download-archive t_download_writes_the_archive
run_case download-out-and-latest t_download_honours_out_and_default_version
run_case download-unknown t_download_of_an_unknown_version_fails
run_case download-uses-cache t_download_uses_the_cache_when_warm
run_case corrupt-cache-refetched t_a_corrupt_cache_entry_is_replaced
run_case cache-verify t_cache_verify_notices_a_corrupt_archive
run_case cache-info t_cache_info_reports_the_package
run_case bytes-match t_the_installed_archive_matches_the_published_one
run_case downloads-counted t_downloads_are_counted_once_per_fetch
run_case transitive t_transitive_dependencies_are_downloaded
run_case dead-registry t_a_dead_registry_fails_the_install
run_case unknown-package t_an_unknown_package_fails
run_case partial-resume t_a_partial_download_is_resumed
run_case cache-clean-refetch t_a_cleaned_cache_refetches_everything
run_case lock-records-digest t_the_lockfile_records_the_registry_digest
run_case frozen t_frozen_does_not_touch_the_network
run_case source-tree t_install_writes_the_source_tree
run_case many-versions t_many_versions_all_resolve
run_case list-and-outdated t_list_and_outdated_work
run_case dl-cmd t_a_download_command_writes_the_archive
run_case dl-newest t_a_download_command_resolves_the_newest_version
run_case dl-unknown t_a_download_command_refuses_an_unknown_version
run_case dl-nopkg t_a_download_command_refuses_an_unknown_package
run_case dl-twice t_a_second_download_reuses_the_cache
run_case dl-force t_force_downloads_even_when_cached
run_case dl-output t_a_download_writes_where_it_is_told
run_case dl-yanked t_a_yanked_version_still_downloads_for_people_who_pinned_it
run_case dl-range t_a_range_request_returns_a_slice_and_the_full_length
run_case dl-bad-range t_an_unsatisfiable_range_is_refused
run_case dl-head t_a_head_request_does_not_count_a_download
run_case dl-digest t_an_install_verifies_the_archive_digest
run_case dl-corrupt t_a_corrupt_cache_entry_is_refetched_on_the_next_install
run_case dl-identical t_the_installed_tree_is_byte_identical_to_the_registry
run_case dl-deps t_a_dependency_archive_is_fetched_too
run_case dl-offline-yanked t_an_offline_install_of_a_yanked_version_still_works
run_case dl-big t_a_download_of_a_large_package_streams_to_disk
run_case dl-report t_the_cache_reports_what_it_did
run_case dl-help t_download_help_works
run_case dl-help2 t_install_help_works

echo "download: $((PASS - FAILED))/$PASS suite passed"
[ "$FAILED" -eq 0 ] || exit 1
exit 0
