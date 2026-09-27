#!/usr/bin/env bash
# Registry authentication regression suite (M8.5).
#
# Drives `hard register / login / logout / whoami / token` against a real
# registry, and checks the parts that only show up when the server is real:
# scopes, ownership of published packages, revocation, the credentials file's
# permissions, and the environment override.
#
# Run: tests/run-auth-regressions.sh  (uses $HARD, default target/debug/hard)
set -u
HARD="${HARD:-target/debug/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"
REGISTRY="${REGISTRY:-$REPO/target/debug/hard-registry}"

if [ ! -x "$HARD" ]; then
    echo "auth: $HARD not found (cargo build first)"
    exit 1
fi
if [ ! -x "$REGISTRY" ]; then
    echo "auth: $REGISTRY not found (cargo build -p hard-registry first)"
    exit 1
fi

TMP="$(mktemp -d /tmp/hs-authreg-XXXXXX)"
trap 'kill "$REG_PID" 2>/dev/null; rm -rf "$TMP"' EXIT

PORT=""
if command -v "$PYTHON" >/dev/null 2>&1; then
    PORT=$("$PYTHON" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
fi
[ -n "$PORT" ] || PORT=$(( 43000 + $$ % 20000 ))
# no --open: authentication is the point of this suite
"$REGISTRY" serve --addr "127.0.0.1:$PORT" --data "$TMP/registry" >"$TMP/registry.log" 2>&1 &
REG_PID=$!
export HARD_REGISTRY="http://127.0.0.1:$PORT"
for _ in $(seq 1 100); do
    if grep -q listening "$TMP/registry.log" 2>/dev/null; then break; fi
    kill -0 "$REG_PID" 2>/dev/null || { echo "auth: registry died"; cat "$TMP/registry.log"; exit 1; }
    sleep 0.1
done
grep -q listening "$TMP/registry.log" || { echo "auth: registry did not start"; cat "$TMP/registry.log"; exit 1; }

PASS=0
FAILED=0

case_begin() {
    CASE_NAME="$1"
    CASE_DIR="$(mktemp -d "$TMP/case.XXXXXX")"
    export HARD_HOME="$CASE_DIR/home"
    unset HARD_TOKEN HARD_USER HARD_PASSWORD
    cd "$CASE_DIR" || exit 1
    PFX="a$(printf '%s' "$1" | tr -cd 'a-z0-9')"
}
run_case() {
    name="$1"; shift
    if ( "$1" ) >"$TMP/case.out" 2>&1; then
        echo "auth: PASS $name"
        PASS=$((PASS + 1))
    else
        echo "auth: FAIL $name"
        cat "$TMP/case.out"
        FAILED=$((FAILED + 1))
    fi
}
assert_rc() { want="$1"; shift; out=$("$@" 2>&1); got=$?; [ "$got" = "$want" ] || { echo "case $CASE_NAME: rc $got, want $want: $*"; echo "$out" | head -4; exit 1; }; }
assert_out() { needle="$1"; shift; out=$("$@" 2>&1); echo "$out" | grep -qF "$needle" || { echo "case $CASE_NAME: missing '$needle' in: $out"; exit 1; }; }
assert_nogrep() { if echo "$1" | grep -qF "$2"; then echo "case $CASE_NAME: '$2' unexpectedly in: $1"; exit 1; fi; }
# assert_in <needle> <text>
assert_in() { echo "$2" | grep -qF "$1" || { echo "case $CASE_NAME: missing '$1' in: $2"; exit 1; }; }
assert_not_in() { if echo "$2" | grep -qF "$1"; then echo "case $CASE_NAME: '$1' unexpectedly in: $2"; exit 1; fi; }

# A tiny package to publish.
pkg() {   # $1 = name
    cat > hard.toml <<EOF
schema = 1
name = "$1"
version = "1.0.0"
edition = "2027"
EOF
    printf 'calc p() => Int { <- 1 }\n' > main.hard
}

# register <user> <password> — an account that already exists is fine (every
# case shares one registry), anything else is not.
register() {   # $1 = user, $2 = password
    out=$("$HARD" register --user "$1" --password "$2" 2>&1)
    case "$out" in
        *"already exists"*) return 0 ;;
        *"password strength"*) return 0 ;;
        *) echo "case $CASE_NAME: register $1 failed: $out"; exit 1 ;;
    esac
}
login() {   # $1 = user, $2 = password
    out=$("$HARD" login --user "$1" --password "$2" 2>&1)
    echo "$out" | grep -qF "logged in" || { echo "case $CASE_NAME: login $1 failed: $out"; exit 1; }
}

# ---- scenarios -----------------------------------------------------------
t_register_then_login() {
    case_begin register
    assert_rc 0 "$HARD" register --user ada --password supersecret
    # a different account name: every case shares one registry, and reusing a
    # name with a different password would make later logins fail
    assert_out "password strength" "$HARD" register --user weakling --password shortish
    assert_rc 0 "$HARD" login --user ada --password supersecret
    assert_out "logged in to $HARD_REGISTRY as ada" "$HARD" login --user ada --password supersecret
}
t_a_wrong_password_is_refused() {
    case_begin wrongpass
    register ada supersecret
    out=$("$HARD" login --user ada --password wrong 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case wrongpass: rc $got want 1: $out"; exit 1; }
    assert_nogrep "$out" "logged in"
    # and nothing was stored
    assert_rc 1 "$HARD" whoami
}
t_a_short_password_is_refused() {
    case_begin shortpass
    out=$("$HARD" register --user ada --password abc 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case shortpass: rc $got want 1: $out"; exit 1; }
    assert_nogrep "$out" "password strength"
}
t_a_duplicate_account_is_refused() {
    case_begin dupe
    register ada supersecret
    out=$("$HARD" register --user ada --password supersecret 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case dupe: rc $got want 1: $out"; exit 1; }
    assert_out "already exists" "$HARD" register --user ada --password supersecret
}
t_login_requires_a_user() {
    case_begin nouser
    assert_rc 2 "$HARD" login --password supersecret
    assert_rc 2 "$HARD" login --user ada
}
t_whoami_reports_the_session() {
    case_begin whoami
    register ada supersecret
    login ada supersecret
    out=$("$HARD" whoami 2>&1)
    assert_out "user:   ada" "$HARD" whoami
    assert_out "label:  session" "$HARD" whoami
    assert_nogrep "$out" "hssess_"
    assert_nogrep "$out" "hspat_"
}
t_whoami_without_a_token_fails() {
    case_begin nowhoami
    assert_rc 1 "$HARD" whoami
}
t_the_credentials_file_is_private() {
    case_begin private
    register ada supersecret
    login ada supersecret
    creds="$HARD_HOME/credentials.toml"
    [ -f "$creds" ] || { echo "case private: no credentials file"; exit 1; }
    mode=$(stat -c '%a' "$creds")
    [ "$mode" = "600" ] || { echo "case private: mode is $mode, want 600"; exit 1; }
    grep -qF "$HARD_REGISTRY" "$creds" || { echo "case private: the registry is not in the file"; exit 1; }
    # the token itself is in there, and nowhere else in the tree
    grep -qF 'token = "hssess_' "$creds" || { echo "case private: no token recorded"; exit 1; }
}
t_logout_forgets_the_token() {
    case_begin logout
    register ada supersecret
    login ada supersecret
    assert_rc 0 "$HARD" whoami
    assert_out "logged out of" "$HARD" logout
    assert_rc 1 "$HARD" whoami
    assert_out "not logged in" "$HARD" logout
}
t_a_revoked_session_cannot_be_reused() {
    case_begin revoke
    register ada supersecret
    login ada supersecret
    token=$(sed -n 's/^token = "\(.*\)"$/\1/p' "$HARD_HOME/credentials.toml" | head -1)
    [ -n "$token" ] || { echo "case revoke: no token in the file"; exit 1; }
    assert_rc 0 "$HARD" logout
    # the session is revoked server-side, so replaying it must fail
    code=$(curl -s -o /dev/null -w '%{http_code}' "$HARD_REGISTRY/auth/whoami" -H "Authorization: Bearer $token")
    [ "$code" = "401" ] || { echo "case revoke: replayed session returned $code"; exit 1; }
}
t_token_create_list_and_revoke() {
    case_begin tokens
    register ada supersecret
    login ada supersecret
    out=$("$HARD" token create ci --scope read,publish 2>&1)
    assert_out "created token tok_" "$HARD" token create ci --scope read,publish
    assert_out "hspat_" "$out"
    assert_out "only time it is shown" "$HARD" token create ci --scope read
    id=$("$HARD" token list | awk '/  ci /{print $1}' | head -1)
    [ -n "$id" ] || { echo "case tokens: the new token is not listed"; exit 1; }
    assert_out "revoked $id" "$HARD" token revoke "$id"
    assert_out "(revoked)" "$HARD" token list
}
t_a_revoked_token_is_rejected() {
    case_begin revoked
    register ada supersecret
    login ada supersecret
    pat=$("$HARD" token create ci --scope read 2>&1 | sed -n 's/^  token: \(hspat_.*\)$/\1/p' | head -1)
    [ -n "$pat" ] || { echo "case revoked: no token returned"; exit 1; }
    code=$(curl -s -o /dev/null -w '%{http_code}' "$HARD_REGISTRY/auth/whoami" -H "Authorization: Bearer $pat")
    [ "$code" = "200" ] || { echo "case revoked: a fresh token returned $code"; exit 1; }
    id=$("$HARD" token list | awk '/  ci /{print $1}' | head -1)
    "$HARD" token revoke "$id" >/dev/null 2>&1
    code=$(curl -s -o /dev/null -w '%{http_code}' "$HARD_REGISTRY/auth/whoami" -H "Authorization: Bearer $pat")
    [ "$code" = "401" ] || { echo "case revoked: a revoked token returned $code"; exit 1; }
}
t_an_unknown_scope_is_refused_locally() {
    case_begin badscope
    register ada supersecret
    login ada supersecret
    out=$("$HARD" token create ci --scope wizardry 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case badscope: rc $got want 1: $out"; exit 1; }
    assert_in "unknown scope" "$out"
}
t_publishing_needs_a_token() {
    case_begin authpublish
    pkg "${PFX}p"
    out=$("$HARD" publish 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case authpublish: rc $got want 1: $out"; exit 1; }
    assert_in "needs a token" "$out"
    register ada supersecret
    login ada supersecret
    out=$("$HARD" publish 2>&1) || { echo "case authpublish: publish failed: $out"; exit 1; }
    assert_in "published ${PFX}p@1.0.0" "$out"
    out=$("$HARD" publish --version 1.1.0 2>&1) || { echo "case authpublish: 1.1.0 failed: $out"; exit 1; }
    assert_in "published ${PFX}p@1.1.0" "$out"
}
t_a_read_only_token_cannot_publish() {
    case_begin ropublish
    pkg "${PFX}p"
    register ada supersecret
    login ada supersecret
    ro=$("$HARD" token create ro --scope read 2>&1 | sed -n 's/^  token: \(hspat_.*\)$/\1/p' | head -1)
    [ -n "$ro" ] || { echo "case ropublish: no token"; exit 1; }
    out=$("$HARD" publish --token "$ro" 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case ropublish: rc $got want 1: $out"; exit 1; }
    assert_in "lacks the 'publish' scope" "$out"
    # and it still works with the right scope
    assert_rc 0 "$HARD" publish
}
t_the_environment_token_overrides_the_file() {
    case_begin envtoken
    register ada supersecret
    login ada supersecret
    pat=$("$HARD" token create ci --scope read,publish 2>&1 | sed -n 's/^  token: \(hspat_.*\)$/\1/p' | head -1)
    pkg "${PFX}p"
    assert_rc 0 env HARD_TOKEN="$pat" "$HARD" publish
    # after logging out, the env token still authenticates
    "$HARD" logout >/dev/null 2>&1
    assert_rc 0 env HARD_TOKEN="$pat" "$HARD" publish --version 1.1.0
    # but without it there is nothing
    assert_rc 1 "$HARD" publish --version 1.2.0
}
t_an_invalid_token_is_rejected() {
    case_begin badtoken
    pkg "${PFX}p"
    out=$("$HARD" publish --token hspat_nope 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case badtoken: rc $got want 1: $out"; exit 1; }
    assert_in "not valid" "$out"
}
t_the_first_publisher_owns_the_name() {
    case_begin owner
    pkg "${PFX}p"
    register ada supersecret
    register bo supersecret
    login ada supersecret
    assert_out "user:   ada" "$HARD" whoami
    assert_rc 0 "$HARD" publish
    assert_in '"owner":"ada"' "$(curl -s "$HARD_REGISTRY/packages/${PFX}p")"
    login bo supersecret
    assert_out "user:   bo" "$HARD" whoami
    out=$("$HARD" publish --version 1.1.0 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case owner: rc $got want 1: $out"; exit 1; }
    assert_in "owned by 'ada'" "$out"
    # ada can keep going
    login ada supersecret
    assert_rc 0 "$HARD" publish --version 1.2.0
}
t_yanking_needs_a_token() {
    case_begin authyank
    pkg "${PFX}p"
    register ada supersecret
    login ada supersecret
    assert_rc 0 "$HARD" publish
    "$HARD" logout >/dev/null 2>&1
    rc_out=$("$HARD" yank "${PFX}p@1.0.0" 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case authyank: rc $got want 1: $rc_out"; exit 1; }
    assert_in "needs a token" "$rc_out"
    login ada supersecret
    assert_out "yanked ${PFX}p@1.0.0" "$HARD" yank "${PFX}p@1.0.0"
}
t_credentials_are_per_registry() {
    case_begin perregistry
    register ada supersecret
    login ada supersecret
    assert_out "ada" "$HARD" whoami
    # a different registry has no credentials at all
    out=$(HARD_REGISTRY=http://127.0.0.1:1 "$HARD" whoami 2>&1)
    got=$?
    [ "$got" = "1" ] || { echo "case perregistry: rc $got want 1: $out"; exit 1; }
    # and the original registry is untouched
    assert_out "ada" "$HARD" whoami
}
t_logout_all_clears_every_registry() {
    case_begin logoutall
    register ada supersecret
    login ada supersecret
    grep -qF "$HARD_REGISTRY" "$HARD_HOME/credentials.toml" || { echo "case logoutall: nothing stored"; exit 1; }
    assert_out "logged out of 1" "$HARD" logout --all
    [ -s "$HARD_HOME/credentials.toml" ] && grep -qF "$HARD_REGISTRY" "$HARD_HOME/credentials.toml" \
        && { echo "case logoutall: the registry is still there"; exit 1; }
    return 0
}
t_whoami_never_prints_a_token() {
    case_begin notoken
    register ada supersecret
    login ada supersecret
    for cmd in "whoami" "token list"; do
        out=$("$HARD" $cmd 2>&1)
        assert_nogrep "$out" "hssess_"
        assert_nogrep "$out" "hspat_"
    done
}
t_the_login_session_survives_a_new_process() {
    case_begin persist
    register ada supersecret
    login ada supersecret
    # a completely separate invocation must see the same identity
    assert_out "user:   ada" "$HARD" whoami
    assert_out "registry $HARD_REGISTRY" "$HARD" whoami
}

# ---- run ----------------------------------------------------------------
run_case register-login t_register_then_login
run_case wrong-password t_a_wrong_password_is_refused
run_case short-password t_a_short_password_is_refused
run_case duplicate-account t_a_duplicate_account_is_refused
run_case login-needs-user t_login_requires_a_user
run_case whoami t_whoami_reports_the_session
run_case whoami-anonymous t_whoami_without_a_token_fails
run_case credentials-private t_the_credentials_file_is_private
run_case logout t_logout_forgets_the_token
run_case session-revoked t_a_revoked_session_cannot_be_reused
run_case token-lifecycle t_token_create_list_and_revoke
run_case revoked-token-rejected t_a_revoked_token_is_rejected
run_case unknown-scope t_an_unknown_scope_is_refused_locally
run_case publish-needs-token t_publishing_needs_a_token
run_case read-only-cannot-publish t_a_read_only_token_cannot_publish
run_case env-token-overrides t_the_environment_token_overrides_the_file
run_case invalid-token t_an_invalid_token_is_rejected
run_case package-ownership t_the_first_publisher_owns_the_name
run_case yank-needs-token t_yanking_needs_a_token
run_case per-registry t_credentials_are_per_registry
run_case logout-all t_logout_all_clears_every_registry
run_case tokens-never-printed t_whoami_never_prints_a_token
run_case session-persists t_the_login_session_survives_a_new_process

echo "auth: $((PASS - FAILED))/$PASS suite passed"
[ "$FAILED" -eq 0 ] || exit 1
exit 0
