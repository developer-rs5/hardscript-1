#!/usr/bin/env bash
# Package-manager regression suite (M4.0).
#
# Exercises `hard add/install/remove/update/list/outdated/cache/workspace/
# search/report` against a local mock .hspkg registry
# (tests/support/mock_registry.py + tests/fixtures/pm-registry.json), over
# plain http://127.0.0.1 so no network or curl is needed.
#
# Coverage: download+lock, transitive deps, lock determinism (no redownload),
# integrity mismatch, conflicts, offline/frozen modes, cache verify/clean,
# workspace discovery/execution, search, reports, env overrides.
#
# Run: tests/run-package-regressions.sh   (uses $HARD, default target/debug/hard)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
DIR="$(cd "$(dirname "$0")" && pwd)"

PYTHON="${PYTHON:-python3}"
PORT=$( "$PYTHON" - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)

TMP="$(mktemp -d /tmp/hs-pmreg-XXXXXX)"
READY="$TMP/ready.log"
REQLOG="$TMP/requests.log"
CASELOG="$TMP/case.out"
trap 'kill "$REG_PID" "$REG2_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

# ---- launch the mock registries -----------------------------------------
# REG2 serves an extended registry ("late" fixture) so outdated works: a new
# patch of hello is published after the first install.
"$PYTHON" "$DIR/support/mock_registry.py" "$DIR/fixtures/pm-registry.json" "$PORT" --logfile "$REQLOG" >"$READY" 2>&1 &
REG_PID=$!
PORT2=$( "$PYTHON" - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)
"$PYTHON" "$DIR/support/mock_registry.py" "$DIR/fixtures/pm-registry-late.json" "$PORT2" >"$TMP/ready2.log" 2>&1 &
REG2_PID=$!
for _ in $(seq 1 100); do
    grep -q READY "$READY" 2>/dev/null && break
    kill -0 "$REG_PID" 2>/dev/null || { echo "pm: mock registry died"; cat "$READY"; exit 1; }
    sleep 0.1
done
for _ in $(seq 1 100); do
    grep -q READY "$TMP/ready2.log" 2>/dev/null && break
    kill -0 "$REG2_PID" 2>/dev/null || { echo "pm: late mock registry died"; cat "$TMP/ready2.log"; exit 1; }
    sleep 0.1
done
grep -q READY "$READY" || { echo "pm: mock registry did not start"; exit 1; }
grep -q READY "$TMP/ready2.log" || { echo "pm: late mock registry did not start"; exit 1; }

PASS=0
FAILED=0

# ---- helpers ------------------------------------------------------------
case_begin() {   # $1 = case name
    CASE_NAME="$1"
    sandbox
}
sandbox() {
    CASE_DIR="$(mktemp -d "$TMP/case.XXXXXX")"
    export HARD_HOME="$CASE_DIR/home"
    export HARD_REGISTRY="http://127.0.0.1:$PORT"
    export HARD_BIN="$HARD"
    unset HARD_VERBOSE HARD_REGISTRY_TIMEOUT HARD_REGISTRY_RETRIES
    cd "$CASE_DIR" || exit 1
    : > "$REQLOG"
}
manifest() {   # $1 = dependency spec lines, e.g. 'hello = "^1.0.0"'
    cat > hard.toml <<EOF
name = "pmr"
version = "0.1.0"
edition = "2027"

[dependencies]
$1
EOF
}
member_manifest() {   # $1 = member name; writes hard.toml in current dir
    printf 'name = "%s"\nversion = "0.1.0"\nedition = "2027"\n\n[dependencies]\n' "$1" > hard.toml
}
member_main() {   # $1 = port
    cat > main.hard <<EOF
bring http

app @$1

GET "/" :: { <- { ok: true } }

test "t" {
    res <- GET "/"
    expect res.status == 200
}
EOF
}
run_case() {
    name="$1"; shift
    if ( "$1" ) >"$CASELOG" 2>&1; then
        echo "pm: PASS $name"
        PASS=$((PASS+1))
    else
        echo "pm: FAIL $name"
        cat "$CASELOG"
        FAILED=$((FAILED+1))
    fi
}
assert_rc() {  # $1=want rc, rest = command; runs in CASE_DIR
    want="$1"; shift
    "$@" >/dev/null 2>&1
    got=$?
    [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $*"; exit 1; }
}
assert_out() {  # $1 = needle, rest = command (output captured)
    needle="$1"; shift
    out=$("$@" 2>&1)
    echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: output missing '$needle': $out"; exit 1; }
}
assert_file() {
    [ -e "$1" ] || { echo "case $CASE_NAME: missing file $1"; exit 1; }
}
assert_no_file() {
    [ ! -e "$1" ] || { echo "case $CASE_NAME: unexpected file $1"; exit 1; }
}
assert_grep() {  # $1 = file, $2 = needle
    grep -qF "$2" "$1" || { echo "case $CASE_NAME: '$2' not in $1"; exit 1; }
}
assert_nogrep() {  # $1 = file, $2 = needle
    if grep -qF "$2" "$1"; then echo "case $CASE_NAME: '$2' unexpectedly in $1"; exit 1; fi
}
assert_one_download() {  # $1 = package, $2 = version path segment
    c=$(grep -c "^GET /packages/$1/$2 " "$REQLOG" 2>/dev/null || true)
    [ -z "$c" ] && c=0
    [ "$c" -le 1 ] || { echo "case $CASE_NAME: $1/$2 downloaded $c times"; exit 1; }
}

# ---- scenarios ----------------------------------------------------------
t_add_downloads_and_writes_lock() {
    case_begin add-downloads-and-locks
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_file hard.lock
    assert_grep hard.lock '[package.hello]'
    assert_file .hard/packages/hello/main.hard
}
t_add_via_cli() {
    case_begin add-via-cli
    assert_rc 0 "$HARD" add hello
    assert_file hard.toml
    assert_grep hard.toml 'hello = "^1.0"'
    assert_file hard.lock
}
t_lock_is_deterministic() {
    case_begin lock-deterministic
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/lock.1"
    assert_rc 0 "$HARD" install
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case lock-deterministic: hard.lock changed"; exit 1; }
    assert_one_download hello 1.2.0
}
t_transitive_deps() {
    case_begin transitive
    manifest 'greet = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock '[package.greet]'
    assert_grep hard.lock '[package.hello]'
    assert_file .hard/packages/hello/main.hard
}
t_newest_satisfying_wins() {
    case_begin newest-satisfying
    manifest 'hello = "*"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock 'version = "2.0.0"'
}
t_caret_excludes_major() {
    case_begin caret-excludes-major
    manifest 'lib = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock '[package.lib]'
    assert_grep hard.lock 'version = "1.9.0"'
    assert_nogrep hard.lock 'version = "2.'
}
t_integrity_mismatch_rejected() {
    case_begin integrity-mismatch
    manifest 'broken = "^1.0.0"'
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case integrity-mismatch: rc $got want 1"; exit 1; }
    echo "$out" | grep -qi 'integrity mismatch' || { echo "case integrity-mismatch: $out"; exit 1; }
}
t_unsatisfiable_conflict() {
    case_begin unsatisfiable
    manifest $'app = "^1.0.0"\nlib = "^1.0.0"'
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case unsatisfiable: rc $got want 1"; exit 1; }
    echo "$out" | grep -q 'lib' || { echo "case unsatisfiable: $out"; exit 1; }
}
t_unknown_package_fails() {
    case_begin unknown-package
    manifest 'nosuch = "^1.0.0"'
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case unknown-package: rc $got want 1"; exit 1; }
    echo "$out" | grep -q 'nosuch' || { echo "case unknown-package: $out"; exit 1; }
}
t_offline_cold_cache_fails() {
    case_begin offline-cold
    manifest 'hello = "^1.0.0"'
    assert_rc 1 "$HARD" install --offline
}
t_offline_warm_cache_succeeds() {
    case_begin offline-warm
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    rm -rf .hard/packages
    cp hard.lock "$CASE_DIR/lock.1"
    assert_rc 0 "$HARD" install --offline
    assert_file hard.lock
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case offline-warm: lock changed"; exit 1; }
}
t_frozen_matches_lock() {
    case_begin frozen
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/lock.1"
    rm -rf .hard/packages
    # frozen rewrites the same lock without touching the registry
    assert_rc 0 "$HARD" install --frozen
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case frozen: lock changed"; exit 1; }
}
t_frozen_cold_fails() {
    case_begin frozen-cold
    manifest 'hello = "^1.0.0"'
    assert_rc 1 "$HARD" install --frozen
}
t_remove_drops_and_relocks() {
    case_begin remove
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_rc 0 "$HARD" remove hello
    assert_nogrep hard.toml 'hello'
    assert_nogrep hard.lock 'hello'
}
t_update_resolves_newest() {
    case_begin update
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock 'version = "1.2.0"'
    manifest 'hello = "^2.0.0"'
    assert_rc 0 "$HARD" update
    assert_grep hard.lock 'version = "2.0.0"'
}
t_list_shows_locked() {
    case_begin list
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    out=$("$HARD" list 2>&1)
    echo "$out" | grep -q '1.2.0' || { echo "case list: no version in: $out"; exit 1; }
    echo "$out" | grep -q 'hello' || { echo "case list: no hello in: $out"; exit 1; }
}
t_outdated_reports() {
    case_begin outdated
    # install against the base registry (locks hello 1.2.0)...
    export HARD_REGISTRY="http://127.0.0.1:$PORT"
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock 'version = "1.2.0"'
    # ...then ask the "late" registry (which now publishes hello 1.3.0).
    export HARD_REGISTRY="http://127.0.0.1:$PORT2"
    out=$("$HARD" outdated 2>&1)
    echo "$out" | grep -q 'hello: locked 1.2.0, latest matching 1.3.0' \
        || { echo "case outdated: $out"; exit 1; }
}
t_exact_pin() {
    case_begin exact-pin
    manifest 'hello = "=1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock 'version = "1.0.0"'
}
t_add_specific_version() {
    case_begin add-specific-version
    assert_rc 0 "$HARD" add 'hello@=1.0.0'
    assert_grep hard.toml 'hello = "=1.0.0"'
    assert_grep hard.lock 'version = "1.0.0"'
}
t_transitive_linked_after_reinstall() {
    case_begin transitive-linked
    manifest 'greet = "^1.0.0"'
    assert_rc 0 "$HARD" install
    rm -rf .hard/packages
    assert_rc 0 "$HARD" install
    assert_file .hard/packages/hello/main.hard
    assert_file .hard/packages/greet/main.hard
    assert_grep hard.lock '[package.hello]'
    assert_grep hard.lock '[package.greet]'
}
t_dev_dependency_resolved() {
    case_begin dev-deps
    rm -f hard.toml
    cat > hard.toml <<EOF
name = "pmr"
version = "0.1.0"
edition = "2027"

[dev-dependencies]
devlib = "^1.0.0"

[dependencies]
EOF
    assert_rc 0 "$HARD" install
    assert_grep hard.lock '[package.devlib]'
}
t_no_deps_install_ok() {
    case_begin no-deps
    manifest ''
    assert_rc 0 "$HARD" install
    assert_file hard.lock
    assert_nogrep hard.lock '[package.'
}
t_cache_home_respected() {
    case_begin cache-home
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_file "$HARD_HOME/cache/packages/hello/hello-1.2.0.hspkg"
}
t_cache_info() {
    case_begin cache-info
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_out 'packages: 1' "$HARD" cache info
}
t_cache_verify_detects_corruption() {
    case_begin cache-verify
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    : > "$HARD_HOME/cache/packages/hello/hello-1.2.0.hspkg"
    assert_rc 1 "$HARD" cache verify
}
t_cache_clean() {
    case_begin cache-clean
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_rc 0 "$HARD" cache clean
    assert_no_file "$HARD_HOME/cache"
}
t_workspace_list() {
    case_begin workspace-list
    mkdir -p a1 a2
    for m in a1 a2; do
        (cd "$m" && member_manifest "$m" && member_main 3110)
    done
    member_manifest wsroot
    # rebuild with workspace at top level (before the [dependencies] table)
    printf 'name = "wsroot"\nversion = "0.1.0"\nedition = "2027"\nworkspace = ["a1", "a2"]\n\n[dependencies]\n' > hard.toml
    out=$("$HARD" workspace list 2>&1)
    echo "$out" | grep -q 'a1' || { echo "case workspace-list: no a1: $out"; exit 1; }
    echo "$out" | grep -q 'a2' || { echo "case workspace-list: no a2: $out"; exit 1; }
    echo "$out" | grep -q '2 members' || { echo "case workspace-list: member count: $out"; exit 1; }
}
t_workspace_test() {
    case_begin workspace-test
    mkdir -p a1
    (cd a1 && member_manifest a1 && member_main 3111)
    member_manifest wsroot
    printf 'name = "wsroot"\nversion = "0.1.0"\nedition = "2027"\nworkspace = ["a1"]\n\n[dependencies]\n' > hard.toml
    out=$("$HARD" workspace test 2>&1)
    echo "$out" | grep -q 'ok' || { echo "case workspace-test: no ok: $out"; exit 1; }
}
t_workspace_not_root_fails() {
    case_begin workspace-not-root
    manifest ''
    out=$("$HARD" workspace test 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case workspace-not-root: rc $got want 1"; exit 1; }
    echo "$out" | grep -q 'not in a workspace' || { echo "case workspace-not-root: $out"; exit 1; }
}
t_search() {
    case_begin search
    assert_out 'hello' "$HARD" search hello
}
t_report_written() {
    case_begin report
    assert_rc 0 "$HARD" report
    assert_file reports/package-manager.md
    assert_grep reports/package-manager.md 'packages'
}
t_dead_registry_fails() {
    case_begin dead-registry
    manifest 'hello = "^1.0.0"'
    export HARD_REGISTRY="http://127.0.0.1:1"
    assert_rc 1 "$HARD" install
}
t_lockfile_schema() {
    case_begin lockfile-schema
    manifest 'hello = "^1.0.0"'
    assert_rc 0 "$HARD" install
    assert_grep hard.lock 'schema = "hard-lock/v1"'
}
t_add_via_cli_requires_name() {
    case_begin add-no-name
    assert_rc 2 "$HARD" add
}
t_init_writes_manifest() {
    case_begin init
    assert_rc 0 "$HARD" init
    assert_file hard.toml
    assert_file main.hard
    assert_grep hard.toml 'name = "'
}

# ---- run ----------------------------------------------------------------
run_case add-downloads-and-locks t_add_downloads_and_writes_lock
run_case add-via-cli t_add_via_cli
run_case add-no-name t_add_via_cli_requires_name
run_case init-writes-manifest t_init_writes_manifest
run_case lock-deterministic t_lock_is_deterministic
run_case transitive t_transitive_deps
run_case newest-satisfying t_newest_satisfying_wins
run_case caret-excludes-major t_caret_excludes_major
run_case integrity-mismatch t_integrity_mismatch_rejected
run_case unsatisfiable t_unsatisfiable_conflict
run_case unknown-package t_unknown_package_fails
run_case offline-cold t_offline_cold_cache_fails
run_case offline-warm t_offline_warm_cache_succeeds
run_case frozen t_frozen_matches_lock
run_case frozen-cold t_frozen_cold_fails
run_case remove t_remove_drops_and_relocks
run_case update t_update_resolves_newest
run_case list t_list_shows_locked
run_case outdated t_outdated_reports
run_case exact-pin t_exact_pin
run_case add-specific-version t_add_specific_version
run_case transitive-linked t_transitive_linked_after_reinstall
run_case dev-deps t_dev_dependency_resolved
run_case no-deps t_no_deps_install_ok
run_case cache-home t_cache_home_respected
run_case cache-info t_cache_info
run_case cache-verify t_cache_verify_detects_corruption
run_case cache-clean t_cache_clean
run_case workspace-list t_workspace_list
run_case workspace-test t_workspace_test
run_case workspace-not-root t_workspace_not_root_fails
run_case search t_search
run_case report t_report_written
run_case dead-registry t_dead_registry_fails
run_case lockfile-schema t_lockfile_schema

echo "pm: $((PASS-FAILED))/$PASS suite passed"
[ "$FAILED" -eq 0 ] || exit 1
exit 0