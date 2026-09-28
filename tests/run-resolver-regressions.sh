#!/usr/bin/env bash
# Dependency resolution V2 regression suite (M8.4).
#
# Exercises the resolver through the CLI against a live registry: newest
# satisfying wins, caret ranges, transitive ordering, conflicts, backtracking
# over a graph a greedy pass cannot solve, dev-dependency handling, cycle
# detection, lockfile stability and lockfile preference.
#
# Run: tests/run-resolver-regressions.sh  (uses $HARD, default target/debug/hard)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"
REGISTRY="${REGISTRY:-$REPO/target/debug/hard-registry}"

if [ ! -x "$HARD" ]; then
    echo "resolver: $HARD not found (cargo build first)"
    exit 1
fi
if [ ! -x "$REGISTRY" ]; then
    echo "resolver: $REGISTRY not found (cargo build -p hard-registry first)"
    exit 1
fi

TMP="$(mktemp -d /tmp/hs-resreg-XXXXXX)"
trap 'kill "$REG_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

PORT=""
if command -v "$PYTHON" >/dev/null 2>&1; then
    PORT=$("$PYTHON" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi
[ -n "$PORT" ] || PORT=$(( 42000 + $$ % 20000 ))
"$REGISTRY" serve --addr "127.0.0.1:$PORT" --data "$TMP/registry" --open >"$TMP/registry.log" 2>&1 &
REG_PID=$!
export HARD_REGISTRY="http://127.0.0.1:$PORT"
for _ in $(seq 1 100); do
    if grep -q listening "$TMP/registry.log" 2>/dev/null; then break; fi
    kill -0 "$REG_PID" 2>/dev/null || { echo "resolver: registry died"; cat "$TMP/registry.log"; exit 1; }
    sleep 0.1
done
grep -q listening "$TMP/registry.log" || { echo "resolver: registry did not start"; cat "$TMP/registry.log"; exit 1; }

PASS=0
FAILED=0

case_begin() {
    CASE_NAME="$1"
    CASE_DIR="$(mktemp -d "$TMP/case.XXXXXX")"
    export HARD_HOME="$CASE_DIR/home"
    cd "$CASE_DIR" || exit 1
    # a per-case prefix keeps names unique in the shared registry
    PFX="r$(printf '%s' "$1" | tr -cd 'a-z0-9')"
}
run_case() {
    name="$1"; shift
    if ( "$1" ) >"$TMP/case.out" 2>&1; then
        echo "resolver: PASS $name"
        PASS=$((PASS + 1))
    else
        echo "resolver: FAIL $name"
        cat "$TMP/case.out"
        FAILED=$((FAILED + 1))
    fi
}
assert_rc() { want="$1"; shift; out=$("$@" 2>&1); got=$?; [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $*"; echo "$out" | head -5; exit 1; }; }
assert_out() { needle="$1"; shift; out=$("$@" 2>&1); echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: missing '$needle' in: $out"; exit 1; }; }
assert_nogrep() { if echo "$1" | grep -qF "$2"; then echo "case $CASE_NAME: '$2' unexpectedly in: $1"; exit 1; fi; }
assert_grep() { grep -qF "$2" "$1" || { echo "case $CASE_NAME: '$2' not in $1"; exit 1; }; }
assert_nogrep_file() { if grep -qF "$2" "$1"; then echo "case $CASE_NAME: '$2' unexpectedly in $1"; exit 1; fi; }

# Publish a package. $1 name, $2 version, $3 = "normal" body, $4 = optional
# dependency lines (name@req, optionally kind:name@req).
pub() {   # $1 = name, $2 = version, rest = dependency specs
    local name="$1" version="$2"
    shift 2
    local dir="$CASE_DIR/pub-$name-$version"
    mkdir -p "$dir"
    {
        echo "schema = 1"
        echo "name = \"$name\""
        echo "version = \"$version\""
        echo "edition = \"2027\""
        if [ "$#" -gt 0 ]; then
            local normal=() dev=()
            local spec
            for spec in "$@"; do
                case "$spec" in
                    dev:*) dev+=("${spec#dev:}") ;;
                    *) normal+=("$spec") ;;
                esac
            done
            if [ "${#normal[@]}" -gt 0 ]; then
                echo
                echo "[dependencies]"
                for spec in "${normal[@]}"; do
                    echo "${spec%@*} = \"${spec#*@}\""
                done
            fi
            if [ "${#dev[@]}" -gt 0 ]; then
                echo
                echo "[dev-dependencies]"
                for spec in "${dev[@]}"; do
                    echo "${spec%@*} = \"${spec#*@}\""
                done
            fi
        fi
    } > "$dir/hard.toml"
    printf 'calc %s() => Int { <- 1 }\n' "$name" > "$dir/main.hard"
    ( cd "$dir" && "$HARD" publish >/dev/null 2>&1 ) \
        || { echo "case $CASE_NAME: could not publish $name@$version"; exit 1; }
}

# A consumer project. Each stdin line is `name@req` and becomes
# `name = "req"` under [dependencies].
consumer() {
    {
        echo "name = \"consumer\""
        echo "version = \"0.1.0\""
        echo "edition = \"2027\""
        echo
        echo "[dependencies]"
        while IFS= read -r spec; do
            [ -n "$spec" ] || continue
            printf '%s = "%s"\n' "${spec%@*}" "${spec#*@}"
        done
    } > hard.toml
}

locked() {   # $1 = package, $2 = version to find
    sed -n "/^\[package\.$1\]$/,/^$/p" hard.lock | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1
}

# ---- version ranges and convergence (M8.10 coverage) -----------------------
t_range_gte_is_honoured() {
    case_begin r-gte
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.5.0
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@>=1.2.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "2.0.0" ] || { echo "case r-gte: $(locked "${PFX}a")"; exit 1; }
}
t_range_upper_bound_is_honoured() {
    case_begin r-lt
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.9.9
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@>=1.0.0 <2.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.9.9" ] || { echo "case r-lt: $(locked "${PFX}a")"; exit 1; }
}
t_range_exact_pin_rejects_a_newer_version() {
    case_begin r-exact
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.0.1
    echo "${PFX}a@=1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.0.0" ] || { echo "case r-exact: $(locked "${PFX}a")"; exit 1; }
}
t_range_tilde_takes_the_newest_patch_of_that_minor() {
    case_begin r-tilde
    pub "${PFX}a" 1.2.0
    pub "${PFX}a" 1.2.7
    pub "${PFX}a" 1.9.0
    echo "${PFX}a@~1.2.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.2.7" ] || { echo "case r-tilde: $(locked "${PFX}a")"; exit 1; }
}
t_range_tilde_does_not_leave_the_minor() {
    case_begin r-tilde-bound
    pub "${PFX}a" 1.2.0
    pub "${PFX}a" 1.9.0
    echo "${PFX}a@~1.2.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.2.0" ] || { echo "case r-tilde-bound: $(locked "${PFX}a")"; exit 1; }
}
t_range_tilde_major_is_bounded() {
    case_begin r-tilde-major
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@~2" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "2.0.0" ] || { echo "case r-tilde-major: $(locked "${PFX}a")"; exit 1; }
}
t_range_or_picks_the_newest_matching() {
    case_begin r-or
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 3.0.0
    echo "${PFX}a@^2.0.0 || ^3.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "3.0.0" ] || { echo "case r-or: $(locked "${PFX}a")"; exit 1; }
}
t_caret_keeps_zero_minor_packages_below_one() {
    case_begin r-caret-zero
    pub "${PFX}a" 0.1.0
    pub "${PFX}a" 0.2.0
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@^0.1.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "0.1.0" ] || { echo "case r-caret-zero: $(locked "${PFX}a")"; exit 1; }
}
t_a_prerelease_is_only_taken_when_asked_for() {
    case_begin r-pre
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 2.0.0-rc.1
    echo "${PFX}a@*" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.0.0" ] || { echo "case r-pre: $(locked "${PFX}a")"; exit 1; }
}
t_a_prerelease_requirement_selects_the_prerelease() {
    case_begin r-pre-req
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" "2.0.0-rc.1"
    echo "${PFX}a@=2.0.0-rc.1" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "2.0.0-rc.1" ] || { echo "case r-pre-req: $(locked "${PFX}a")"; exit 1; }
}
t_two_requirements_on_one_package_converge() {
    case_begin r-converge
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.6.0
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.5.0"
    pub "${PFX}c" 1.0.0 "${PFX}a@^1.0.0"
    printf '%s\n%s\n' "${PFX}b@1" "${PFX}c@1" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.6.0" ] || { echo "case r-converge: $(locked "${PFX}a")"; exit 1; }
}
t_impossible_convergence_is_reported() {
    case_begin r-conflict
    pub "${PFX}a" 1.0.0
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.0.0"
    pub "${PFX}c" 1.0.0 "${PFX}a@^2.0.0"
    pub "${PFX}a" 2.0.0
    printf '%s\n%s\n' "${PFX}b@1" "${PFX}c@1" | consumer
    assert_rc 1 "$HARD" install
}
t_a_missing_version_is_named_in_the_error() {
    case_begin r-missing-version
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@^3.0.0" | consumer
    out=$("$HARD" install 2>&1) && rc=0 || rc=$?
    [ "$rc" = "1" ] || { echo "case r-missing-version: rc $rc"; exit 1; }
    echo "$out" | grep -qF "${PFX}a" || { echo "case r-missing-version: error does not name the package: $out"; exit 1; }
}
t_a_wildcard_with_no_versions_fails_cleanly() {
    case_begin r-no-versions
    echo "${PFX}nothing@*" | consumer
    out=$("$HARD" install 2>&1) && rc=0 || rc=$?
    [ "$rc" = "1" ] || { echo "case r-no-versions: rc $rc"; exit 1; }
    echo "$out" | grep -qi "cannot resolve" || { echo "case r-no-versions: unhelpful error: $out"; exit 1; }
}
t_a_yanked_version_is_not_chosen() {
    case_begin r-yanked
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.1.0
    assert_rc 0 "$HARD" yank "${PFX}a@1.1.0"
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.0.0" ] || { echo "case r-yanked: $(locked "${PFX}a")"; exit 1; }
}
t_an_exact_pin_on_a_yanked_version_still_fails_loudly() {
    case_begin r-yanked-exact
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.1.0
    assert_rc 0 "$HARD" yank "${PFX}a@1.1.0"
    echo "${PFX}a@=1.1.0" | consumer
    assert_rc 1 "$HARD" install
}
t_three_level_chain_orders_dependencies_first() {
    case_begin r-chain3
    pub "${PFX}c" 1.0.0 "${PFX}b@^1.0.0"
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.0.0"
    pub "${PFX}a" 1.0.0
    echo "${PFX}c@1" | consumer
    assert_rc 0 "$HARD" install
    order=$(grep '^\[package\.' hard.lock | sed 's/^\[package\.//; s/\]$//')
    a=$(printf '%s\n' "$order" | grep -n "^${PFX}a$" | cut -d: -f1)
    b=$(printf '%s\n' "$order" | grep -n "^${PFX}b$" | cut -d: -f1)
    c=$(printf '%s\n' "$order" | grep -n "^${PFX}c$" | cut -d: -f1)
    [ "$a" -lt "$b" ] && [ "$b" -lt "$c" ] || { echo "case r-chain3: order was $order"; exit 1; }
}
t_a_shared_transitive_is_installed_once() {
    case_begin r-shared
    pub "${PFX}shared" 1.0.0
    pub "${PFX}left" 1.0.0 "${PFX}shared@^1.0.0"
    pub "${PFX}right" 1.0.0 "${PFX}shared@^1.0.0"
    printf '%s\n%s\n' "${PFX}left@1" "${PFX}right@1" | consumer
    assert_rc 0 "$HARD" install
    [ "$(grep -c "^\[package\.${PFX}shared\]$" hard.lock)" = "1" ] \
        || { echo "case r-shared: shared appears more than once"; exit 1; }
}
t_a_consumer_dev_dependency_is_installed() {
    case_begin r-consumer-dev
    pub "${PFX}a" 1.0.0
    pub "${PFX}testkit" 1.0.0
    {
        echo "name = \"consumer\""
        echo "version = \"0.1.0\""
        echo "edition = \"2027\""
        echo
        echo "[dependencies]"
        echo "${PFX}a = \"1\""
        echo
        echo "[dev-dependencies]"
        echo "${PFX}testkit = \"1\""
    } > hard.toml
    assert_rc 0 "$HARD" install
    [ -n "$(locked "${PFX}a")" ] || { echo "case r-consumer-dev: the dependency is missing"; exit 1; }
    [ -n "$(locked "${PFX}testkit")" ] || { echo "case r-consumer-dev: the dev-dependency is missing"; exit 1; }
}
t_a_workspace_root_installs_registry_dependencies() {
    case_begin r-workspace
    pub "${PFX}a" 1.0.0
    mkdir -p "$CASE_DIR/member"
    printf 'schema = 1\nname = "%s"\nversion = "0.1.0"\nedition = "2027"\n' "${PFX}member" > "$CASE_DIR/member/hard.toml"
    printf 'calc m() => Int { <- 1 }\n' > "$CASE_DIR/member/main.hard"
    {
        echo "schema = 1"
        echo "name = \"root\""
        echo "version = \"0.1.0\""
        echo "edition = \"2027\""
        echo "workspace = [\"member\"]"
        echo
        echo "[dependencies]"
        echo "${PFX}a = \"1\""
    } > hard.toml
    assert_rc 0 "$HARD" install
    grep -q "^\[package\.${PFX}a\]$" hard.lock || { echo "case r-workspace: the registry dependency is missing"; exit 1; }
    out=$("$HARD" workspace list 2>&1) && rc=0 || rc=$?
    assert_out "member" "$HARD" workspace list
}
t_a_deleted_lockfile_is_regenerated_identically() {
    case_begin r-regen
    pub "${PFX}a" 1.0.0
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.0.0"
    printf '%s\n' "${PFX}b@1" | consumer
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/first.lock"
    rm -f hard.lock
    assert_rc 0 "$HARD" install
    assert_grep "$CASE_DIR/first.lock" "[package.${PFX}a]"
    cmp -s hard.lock "$CASE_DIR/first.lock" || { echo "case r-regen: regenerated lock differs"; exit 1; }
}
t_an_offline_install_uses_the_locked_versions() {
    case_begin r-offline
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.5.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.5.0" ] || { echo "case r-offline: $(locked "${PFX}a")"; exit 1; }
    pub "${PFX}a" 2.0.0
    assert_rc 0 "$HARD" install --offline
    [ "$(locked "${PFX}a")" = "1.5.0" ] || { echo "case r-offline: offline install moved the lock"; exit 1; }
}
t_a_corrupt_lockfile_is_not_trusted() {
    case_begin r-corrupt
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@1" | consumer
    assert_rc 0 "$HARD" install
    printf 'schema = "hard-lock/v1"\nthis is not toml [\n' > hard.lock
    assert_rc 0 "$HARD" install
    grep -q "^\[package\.${PFX}a\]$" hard.lock || { echo "case r-corrupt: lock was not rebuilt"; exit 1; }
}
t_a_manifest_without_a_schema_still_resolves() {
    case_begin r-no-schema
    pub "${PFX}a" 1.0.0
    {
        echo "name = \"consumer\""
        echo "version = \"0.1.0\""
        echo
        echo "[dependencies]"
        echo "${PFX}a = \"1\""
    } > hard.toml
    assert_rc 0 "$HARD" install
    grep -q "^\[package\.${PFX}a\]$" hard.lock || { echo "case r-no-schema: nothing was locked"; exit 1; }
}
t_installing_twice_installs_nothing_new() {
    case_begin r-idempotent
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@1" | consumer
    assert_rc 0 "$HARD" install
    out=$("$HARD" install 2>&1)
    assert_nogrep "$out" "installed ${PFX}a"
}
t_outdated_is_empty_for_the_newest_version() {
    case_begin r-up-to-date
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@1" | consumer
    assert_rc 0 "$HARD" install
    out=$("$HARD" outdated 2>&1) && rc=0 || rc=$?
    assert_nogrep "$out" "${PFX}a"
}

# ---- scenarios -----------------------------------------------------------
t_newest_satisfying_wins() {
    case_begin newest
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.4.0
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.4.0" ] || { echo "case newest: locked $(locked "${PFX}a")"; exit 1; }
}
t_caret_excludes_the_next_major() {
    case_begin caret
    pub "${PFX}a" 1.9.0
    pub "${PFX}a" 2.1.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.9.0" ] || { echo "case caret: $(locked "${PFX}a")"; exit 1; }
}
t_transitive_packages_are_ordered_first() {
    case_begin order
    pub "${PFX}base" 1.0.0
    pub "${PFX}mid" 1.0.0 "${PFX}base@^1.0.0"
    pub "${PFX}top" 1.0.0 "${PFX}mid@^1.0.0"
    echo "${PFX}top@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    # the lockfile lists a package before the packages that depend on it
    line_base=$(grep -n "package.${PFX}base" hard.lock | head -1 | cut -d: -f1)
    line_top=$(grep -n "package.${PFX}top" hard.lock | head -1 | cut -d: -f1)
    [ -n "$line_base" ] && [ -n "$line_top" ] || { echo "case order: missing lock entries"; exit 1; }
    [ "$line_base" -lt "$line_top" ] || { echo "case order: base is not locked before top"; exit 1; }
    assert_grep hard.lock "dependencies = [\"${PFX}base\"]"
}
t_a_conflict_is_reported_with_versions() {
    case_begin conflict
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 2.0.0
    { echo "${PFX}a@^1.0.0"; echo "${PFX}a@^2.0.0"; } | consumer
    # the same name twice in one table is a manifest error, so use two names
    pub "${PFX}b" 1.0.0 "${PFX}a@^2.0.0"
    cat > hard.toml <<EOF
name = "consumer"
version = "0.1.0"
edition = "2027"

[dependencies]
${PFX}b = "^1.0.0"
${PFX}a = "^1.0.0"
EOF
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case conflict: rc $got want 1: $out"; exit 1; }
    echo "$out" | grep -qF "conflict" || { echo "case conflict: no conflict reported: $out"; exit 1; }
    echo "$out" | grep -qF "available versions" || { echo "case conflict: no versions listed: $out"; exit 1; }
}
t_the_solver_backs_off_to_a_working_version() {
    case_begin backtrack
    # mid 2.4.0 (the newest) requires leaf ^9.0.0, which nothing satisfies, so
    # the solver has to give up on it and take mid 2.0.0
    pub "${PFX}leaf" 1.0.0
    pub "${PFX}leaf" 2.0.0
    pub "${PFX}mid" 2.0.0 "${PFX}leaf@^1.0.0"
    pub "${PFX}mid" 2.4.0 "${PFX}leaf@^9.0.0"
    pub "${PFX}top" 1.0.0 "${PFX}mid@^2.0.0"
    echo "${PFX}top@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}mid")" = "2.0.0" ] || { echo "case backtrack: mid locked $(locked "${PFX}mid")"; exit 1; }
    [ "$(locked "${PFX}leaf")" = "1.0.0" ] || { echo "case backtrack: leaf locked $(locked "${PFX}leaf")"; exit 1; }
}
t_a_root_requirement_wins_over_the_newest() {
    case_begin root-wins
    pub "${PFX}leaf" 0.3.1
    pub "${PFX}leaf" 0.4.0
    pub "${PFX}leaf" 0.5.0
    pub "${PFX}mid" 2.0.0
    pub "${PFX}mid" 2.1.0 "${PFX}leaf@^0.3.0"
    pub "${PFX}top" 1.0.0 "${PFX}mid@^2.0.0"
    { echo "${PFX}top@^1.0.0"; echo "${PFX}leaf@^0.4.0"; } | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}leaf")" = "0.4.0" ] || { echo "case root-wins: leaf locked $(locked "${PFX}leaf")"; exit 1; }
}
t_a_transitive_dev_dependency_is_not_installed() {
    case_begin dev-edge
    # mid needs a dev-only harness that is never published: a consumer must
    # not care, and must not fail trying to resolve it
    pub "${PFX}mid" 1.0.0 "dev:${PFX}harness@^1.0.0"
    echo "${PFX}mid@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    assert_nogrep_file hard.lock "${PFX}harness"
    [ -d ".hard/packages/${PFX}harness" ] && { echo "case dev-edge: the dev dependency was installed"; exit 1; }
    return 0
}
t_a_root_dev_dependency_is_installed() {
    case_begin dev-root
    pub "${PFX}mid" 1.0.0
    pub "${PFX}tools" 1.0.0
    cat > hard.toml <<EOF
name = "consumer"
version = "0.1.0"
edition = "2027"

[dependencies]
${PFX}mid = "^1.0.0"

[dev-dependencies]
${PFX}tools = "^1.0.0"
EOF
    assert_rc 0 "$HARD" install
    assert_grep hard.lock "[package.${PFX}tools]"
}
t_a_runtime_cycle_is_reported() {
    case_begin cycle
    pub "${PFX}a" 1.0.0 "${PFX}b@^1.0.0"
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.0.0"
    echo "${PFX}a@^1.0.0" | consumer
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case cycle: rc $got want 1: $out"; exit 1; }
    echo "$out" | grep -qi "circular" || { echo "case cycle: $out"; exit 1; }
}
t_an_unknown_package_names_the_requester() {
    case_begin unknown
    echo "${PFX}ghost@^1.0.0" | consumer
    out=$("$HARD" install 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case unknown: rc $got want 1: $out"; exit 1; }
    echo "$out" | grep -qF "${PFX}ghost" || { echo "case unknown: $out"; exit 1; }
}
t_the_lockfile_is_stable_across_installs() {
    case_begin stable
    pub "${PFX}a" 1.0.0
    pub "${PFX}b" 1.0.0 "${PFX}a@^1.0.0"
    { echo "${PFX}b@^1.0.0"; } | consumer
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/lock.1"
    assert_rc 0 "$HARD" install
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case stable: the lockfile changed"; exit 1; }
}
t_adding_a_dependency_keeps_locked_versions() {
    case_begin preference
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.5.0
    pub "${PFX}b" 1.0.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    before=$(locked "${PFX}a")
    {
        echo "${PFX}a@^1.0.0"
        echo "${PFX}b@^1.0.0"
    } | consumer
    assert_rc 0 "$HARD" install
    after=$(locked "${PFX}a")
    [ "$before" = "$after" ] || { echo "case preference: $before -> $after after adding a dependency"; exit 1; }
}
t_an_exact_pin_is_honoured() {
    case_begin exact
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.5.0
    echo "${PFX}a@=1.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.0.0" ] || { echo "case exact: $(locked "${PFX}a")"; exit 1; }
}
t_a_wildcard_takes_the_newest() {
    case_begin wildcard
    pub "${PFX}a" 0.1.0
    pub "${PFX}a" 0.9.0
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@*" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.0.0" ] || { echo "case wildcard: $(locked "${PFX}a")"; exit 1; }
}
t_a_comparator_set_is_honoured() {
    case_begin comparators
    pub "${PFX}a" 1.0.0
    pub "${PFX}a" 1.5.0
    pub "${PFX}a" 2.0.0
    echo "${PFX}a@>=1.2.0,<2.0.0" | consumer
    assert_rc 0 "$HARD" install
    [ "$(locked "${PFX}a")" = "1.5.0" ] || { echo "case comparators: $(locked "${PFX}a")"; exit 1; }
}
t_outdated_reports_a_newer_matching_version() {
    case_begin outdated
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    assert_out "all dependencies are up to date" "$HARD" outdated
    pub "${PFX}a" 1.3.0
    assert_out "latest matching 1.3.0" "$HARD" outdated
    assert_rc 0 "$HARD" update
    [ "$(locked "${PFX}a")" = "1.3.0" ] || { echo "case outdated: $(locked "${PFX}a") after update"; exit 1; }
}
t_a_frozen_install_never_re_resolves() {
    case_begin frozen
    pub "${PFX}a" 1.0.0
    echo "${PFX}a@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    cp hard.lock "$CASE_DIR/lock.1"
    pub "${PFX}a" 1.9.0
    assert_rc 0 "$HARD" install --frozen
    cmp -s hard.lock "$CASE_DIR/lock.1" || { echo "case frozen: the lockfile changed"; exit 1; }
}
t_a_deep_chain_resolves_in_order() {
    case_begin chain
    pub "${PFX}l1" 1.0.0
    pub "${PFX}l2" 1.0.0 "${PFX}l1@^1.0.0"
    pub "${PFX}l3" 1.0.0 "${PFX}l2@^1.0.0"
    pub "${PFX}l4" 1.0.0 "${PFX}l3@^1.0.0"
    echo "${PFX}l4@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    for p in l1 l2 l3 l4; do
        assert_grep hard.lock "[package.${PFX}$p]"
    done
    first=$(grep -n "^\[package\.${PFX}l1\]" hard.lock | cut -d: -f1)
    last=$(grep -n "^\[package\.${PFX}l4\]" hard.lock | cut -d: -f1)
    [ "$first" -lt "$last" ] || { echo "case chain: the chain is not in dependency order"; exit 1; }
}
t_a_diamond_resolves_once() {
    case_begin diamond
    pub "${PFX}base" 1.0.0
    pub "${PFX}left" 1.0.0 "${PFX}base@^1.0.0"
    pub "${PFX}right" 1.0.0 "${PFX}base@^1.0.0"
    pub "${PFX}top" 1.0.0 "${PFX}left@^1.0.0" "${PFX}right@^1.0.0"
    echo "${PFX}top@^1.0.0" | consumer
    assert_rc 0 "$HARD" install
    n=$(grep -c "^\[package\.${PFX}base\]" hard.lock)
    [ "$n" = "1" ] || { echo "case diamond: base locked $n times"; exit 1; }
    assert_grep hard.lock "dependencies = [\"${PFX}base\"]"
}
t_an_empty_project_resolves() {
    case_begin empty
    cat > hard.toml <<'EOF'
name = "consumer"
version = "0.1.0"
edition = "2027"

[dependencies]
EOF
    assert_rc 0 "$HARD" install
    assert_nogrep_file hard.lock "[package."
}

# ---- run ----------------------------------------------------------------
run_case newest-satisfying t_newest_satisfying_wins
run_case caret-excludes-major t_caret_excludes_the_next_major
run_case transitive-order t_transitive_packages_are_ordered_first
run_case conflict-reported t_a_conflict_is_reported_with_versions
run_case backtracking t_the_solver_backs_off_to_a_working_version
run_case root-requirement-wins t_a_root_requirement_wins_over_the_newest
run_case transitive-dev-edge-skipped t_a_transitive_dev_dependency_is_not_installed
run_case root-dev-edge-kept t_a_root_dev_dependency_is_installed
run_case runtime-cycle t_a_runtime_cycle_is_reported
run_case unknown-package t_an_unknown_package_names_the_requester
run_case lockfile-stable t_the_lockfile_is_stable_across_installs
run_case lockfile-preference t_adding_a_dependency_keeps_locked_versions
run_case exact-pin t_an_exact_pin_is_honoured
run_case wildcard-newest t_a_wildcard_takes_the_newest
run_case comparator-set t_a_comparator_set_is_honoured
run_case outdated-and-update t_outdated_reports_a_newer_matching_version
run_case frozen-never-resolves t_a_frozen_install_never_re_resolves
run_case deep-chain t_a_deep_chain_resolves_in_order
run_case diamond t_a_diamond_resolves_once
run_case empty-project t_an_empty_project_resolves
run_case caret-zero t_caret_keeps_zero_minor_packages_below_one
run_case r-gte t_range_gte_is_honoured
run_case r-lt t_range_upper_bound_is_honoured
run_case r-exact t_range_exact_pin_rejects_a_newer_version
run_case r-tilde t_range_tilde_takes_the_newest_patch_of_that_minor
run_case r-tilde-bound t_range_tilde_does_not_leave_the_minor
run_case r-tilde-major t_range_tilde_major_is_bounded
run_case r-or t_range_or_picks_the_newest_matching
run_case r-pre t_a_prerelease_is_only_taken_when_asked_for
run_case r-pre-req t_a_prerelease_requirement_selects_the_prerelease
run_case r-converge t_two_requirements_on_one_package_converge
run_case r-conflict t_impossible_convergence_is_reported
run_case r-missing-version t_a_missing_version_is_named_in_the_error
run_case r-no-versions t_a_wildcard_with_no_versions_fails_cleanly
run_case r-yanked t_a_yanked_version_is_not_chosen
run_case r-yanked-exact t_an_exact_pin_on_a_yanked_version_still_fails_loudly
run_case r-chain3 t_three_level_chain_orders_dependencies_first
run_case r-shared t_a_shared_transitive_is_installed_once
run_case r-consumer-dev t_a_consumer_dev_dependency_is_installed
run_case r-workspace t_a_workspace_root_installs_registry_dependencies
run_case r-regen t_a_deleted_lockfile_is_regenerated_identically
run_case r-offline t_an_offline_install_uses_the_locked_versions
run_case r-corrupt t_a_corrupt_lockfile_is_not_trusted
run_case r-no-schema t_a_manifest_without_a_schema_still_resolves
run_case r-idempotent t_installing_twice_installs_nothing_new
run_case r-up-to-date t_outdated_is_empty_for_the_newest_version

echo "resolver: $((PASS - FAILED))/$PASS suite passed"
[ "$FAILED" -eq 0 ] || exit 1
exit 0
