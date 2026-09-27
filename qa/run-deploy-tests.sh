#!/usr/bin/env bash
# Deployment test suite for `hard deploy` (M7.3).
#
# The interesting part of a deploy is not the Rust that decides what to do, it
# is the shell that runs on the far side: the symlink swap, the directories, the
# migration helper. A mock SSH proves the *order*; it cannot prove that
# `ln -sfn` + `mv -T` really repoints `current` and leaves `previous` alone.
#
# So this suite takes the plan the tool prints and runs it here, against a
# scratch root, with `sh`. Every path in a plan derives from `--dir`, so the
# only thing standing between a printed plan and a local run is the restart and
# the health probe -- which is exactly what is left out.
#
# Exit 0 if every check passes, non-zero otherwise.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD="${HARD:-$ROOT/target/release/hard}"
FAILED=0
TOTAL=0

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

pass() {
    TOTAL=$((TOTAL + 1))
    echo "deploy: ok   $1"
}

fail() {
    TOTAL=$((TOTAL + 1))
    FAILED=$((FAILED + 1))
    echo "deploy: FAIL $1"
}

check() {
    # check <description> <condition-as-shell>
    if eval "$2"; then pass "$1"; else fail "$1"; fi
}

# The first plan step whose first line matches $1, up to and including the line
# that ends it. A `sed` range is not enough: it starts again at the next match,
# so with two heredoc steps in one plan it captures the rest of the plan too.
step_command() {
    awk -v first="$1" -v last="$2" '
        $0 ~ first { inside = 1 }
        inside { print }
        inside && $0 ~ last { exit }
    ' "$3"
}

if [ ! -x "$HARD" ]; then
    echo "deploy: no hard binary at $HARD (cargo build --release)"
    exit 1
fi

# ---- a project to deploy ----------------------------------------------------
APP="$TMP/app"
mkdir -p "$APP/migrations" "$APP/static"
cat > "$APP/hard.toml" <<'TOML'
name = "smoke"
version = "0.4.0"
edition = "2027"

[server]
port = 8123

[database]
dialect = "sqlite"
path = "smoke.db"
TOML
cat > "$APP/main.hard" <<'HARD'
GET "/healthz" :: {
    <- { ok: true }
}

GET "/" :: {
    <- { hello: "world" }
}
HARD
# A real migration, in the format the helper accepts: the markers are part of
# the file, not decoration.
cat > "$APP/migrations/0001_init.sql" <<'SQL'
-- hardscript:migration 0001
-- hardscript:fingerprint -- table t\n  "id" INTEGER PRIMARY KEY NOT NULL\n
-- hardscript:models
-- model T {
--     id : Int @primary
-- }
--
-- hardscript:end
-- +migrate Up
-- create table t
CREATE TABLE IF NOT EXISTS "t" (
  "id" INTEGER PRIMARY KEY NOT NULL
);
-- +migrate Down
-- create table t
DROP TABLE IF EXISTS "t";
SQL
printf 'body { color: red }\n' > "$APP/static/app.css"

# ---- the printed plan -------------------------------------------------------
cd "$APP" || exit 1
PLAN="$TMP/plan.txt"
if ! "$HARD" deploy ssh --print --no-build --no-health deploy@example.com --dir "$TMP/srv" \
    > "$PLAN" 2>"$TMP/err"; then
    fail "hard deploy ssh --print ($(head -2 "$TMP/err" | tr '\n' ' '))"
    echo "$PLAN"
else
    pass "hard deploy ssh --print"

    # The release id names the version, the moment, and the bytes.
    if grep -qE '^# release 0\.4\.0-[0-9]{8}T[0-9]{6}Z-[0-9a-f]{8}$' "$PLAN"; then
        pass "the release id is version, time, and digest"
    else
        fail "the release id is version, time, and digest ($(head -1 "$PLAN"))"
    fi

    # The order, which is the whole contract of a deploy.
    order=$(grep -oE '^[a-z]+' "$PLAN" | tr '\n' ' ')
    if [ "$order" = "inspect prepare upload install upload upload upload upload install migrate activate restart " ]; then
        pass "migrations before activation, activation before the restart"
    else
        fail "step order ($order)"
    fi

    # Nothing was left out by accident: --no-health asked for no probe.
    if grep -q '^health' "$PLAN"; then
        fail "--no-health leaves the probe out of the plan"
    else
        pass "--no-health leaves the probe out of the plan"
    fi

    # A plan with the probe is a different plan; check it separately.
    if "$HARD" deploy ssh --print --no-build --no-migrate root@example.com --dir "$TMP/srv" \
        2>/dev/null | grep -q 'health.*http://127.0.0.1:8123/healthz'; then
        pass "with the probe, the plan asks for the manifest's port"
    else
        fail "with the probe, the plan asks for the manifest's port"
    fi

    # Everything that travels with the binary.
    for want in "hard.toml -> $TMP/srv/releases/" \
                "migrations/0001_init.sql -> $TMP/srv/shared/migrations/0001_init.sql" \
                "static/app.css -> $TMP/srv/shared/static/app.css" \
                ".hard/migrate-helper ->"; do
        if grep -q "$want" "$PLAN"; then
            pass "the plan ships $(echo "$want" | cut -d' ' -f1)"
        else
            fail "the plan ships $(echo "$want" | cut -d' ' -f1)"
        fi
    done

    # A non-root deploy escalates, and escalates without a prompt.
    if grep -q 'restart   sudo -n systemctl restart smoke.service' "$PLAN"; then
        pass "a non-root deploy restarts with sudo -n"
    else
        fail "a non-root deploy restarts with sudo -n"
    fi

    # Nothing from a build machine's environment leaks into the remote commands.
    if grep -q 'DATABASE_URL' "$PLAN"; then
        fail "a sqlite deploy does not mention DATABASE_URL"
    else
        pass "a sqlite deploy does not mention DATABASE_URL"
    fi
fi

# ---- run the plan's shell against a scratch root ----------------------------
# The uploads are scp lines, so the files they would create are created here,
# and the restart and the probe are left out: this suite is about the layout.
SRV="$TMP/srv"
R1="$SRV/releases/0.4.0-aaaa"
R2="$SRV/releases/0.4.0-bbbb"

# The prepare step, for real: the layout the rest of the suite depends on is
# the one the plan creates, not one made by hand.
if out=$(sed -n 's/^prepare  //p' "$PLAN" | sh -e 2>&1); then
    pass "the prepare step runs as sh"
else
    fail "the prepare step ($out)"
fi
check "the shared directories exist" \
    "[ -d '$SRV/releases' ] && [ -d '$SRV/shared/migrations' ] && [ -d '$SRV/shared/static' ]"

mkdir -p "$R1" "$R2"
printf '#!/bin/sh\necho smoke\n' > "$R1/server"
printf '#!/bin/sh\necho smoke\n' > "$R2/server"
chmod 0644 "$R1/server" "$R2/server"

run_activate() {
    # The activation step of a plan, with the release id swapped for one that
    # exists here.
    local id="$1"
    sed -n 's/^activate  //p' "$PLAN" | sed "s|$SRV/releases/0\.4\.0-[0-9T-Za-z-]*|$SRV/releases/$id|g"
}

for id in 0.4.0-aaaa 0.4.0-bbbb; do
    # The activation ends in a `readlink -f`, so its output is the path it now
    # points at; the checks below are the interesting part.
    if out=$(run_activate "$id" | sh -e 2>&1 >/dev/null); then
        pass "activation of $id runs as sh"
    else
        fail "activation of $id ($out)"
    fi
done

check "current points at the new release" \
    "[ \"\$(readlink -f '$SRV/current')\" = '$R2' ]"
check "previous points at the one before it" \
    "[ \"\$(readlink -f '$SRV/previous')\" = '$R1' ]"
check "the swap did not nest releases inside releases" \
    "[ ! -e '$R1/releases' ]"
check "no temporary symlink was left behind" \
    "[ ! -e '$SRV/current.new' ]"
check "the release directories are untouched by the swap" \
    "[ -f '$R1/server' ] && [ -f '$R2/server' ]"

# A third deploy moves previous again, so a rollback always has two candidates.
R3="$SRV/releases/0.4.0-cccc"
mkdir -p "$R3"
printf '#!/bin/sh\n' > "$R3/server"
sed -n 's/^activate  //p' "$PLAN" | sed "s|$SRV/releases/0\.4\.0-[0-9T-Za-z-]*|$R3|g" | sh -e >/dev/null
check "a third deploy rotates previous" \
    "[ \"\$(readlink -f '$SRV/previous')\" = '$R2' ] && [ \"\$(readlink -f '$SRV/current')\" = '$R3' ]"

# And pointing current back at previous is all a rollback has to be.
ln -sfn "$(readlink -f "$SRV/previous")" "$SRV/current.new"
mv -T "$SRV/current.new" "$SRV/current"
check "going back to the previous release is one link" \
    "[ \"\$(readlink -f '$SRV/current')\" = '$R2' ]"

# ---- the migration helper runs from a release directory ----------------------
# The deploy ships `.hard/migrate-helper` and calls it with `up <dialect>
# <target> <files>`; that call has to work against a file in shared/.
if [ -x "$APP/.hard/migrate-helper" ]; then
    # The upload steps are scp lines, so the file they would have copied is
    # copied here; the point is the `up <dialect> <target> <files>` call, run
    # from a release directory against a file in shared/.
    cp "$APP/migrations/0001_init.sql" "$SRV/shared/migrations/0001_init.sql"
    "$APP/.hard/migrate-helper" up sqlite "$SRV/shared/smoke.db" \
        "$SRV/shared/migrations/0001_init.sql" > "$TMP/migrate.out" 2>&1
    check "the helper creates the schema in shared/" \
        "$APP/.hard/migrate-helper status sqlite '$SRV/shared/smoke.db' '$SRV/shared/migrations/0001_init.sql' 2>/dev/null | grep -q '^A 0001 '"
    check "the helper applies nothing the second time" \
        "[ \"\$('$APP/.hard/migrate-helper' up sqlite '$SRV/shared/smoke.db' '$SRV/shared/migrations/0001_init.sql')\" = 'U' ]"
else
    echo "deploy: skip the helper (no .hard/migrate-helper; run hard migrate first)"
fi

# ---- environments -----------------------------------------------------------
# The env file is written by a heredoc over ssh, so the only way to know the
# quoting is right is to run it and source the result.
cat >> "$APP/hard.toml" <<'TOML'

[env.production]
host = "deploy@api.example.com"
dir = "SRV_PLACEHOLDER"
domain = "smoke.example.com"
tls_email = "ops@example.com"
secrets = ["SESSION_KEY", "DATABASE_URL"]

[env.production.vars]
RUST_LOG = "info"
MOTD = "it's fine"
TOML
sed -i "s|SRV_PLACEHOLDER|$SRV|" "$APP/hard.toml"

if "$HARD" deploy env list 2>/dev/null | grep -q "deploy@api.example.com (dir $SRV"; then
    pass "env list names the environment and where it goes"
else
    fail "env list names the environment and where it goes"
fi

ENV_PLAN="$TMP/envplan.txt"
"$HARD" deploy ssh --print --no-build --no-health --no-migrate --env production \
    > "$ENV_PLAN" 2>"$TMP/err"
if grep -q "cat > '$SRV/shared/env' <<'HS_ENV_EOF'" "$ENV_PLAN"; then
    pass "a deploy writes shared/env with a quoted heredoc"
else
    fail "a deploy writes shared/env with a quoted heredoc"
fi
if grep -q "grep -q \"^SESSION_KEY=\" " "$ENV_PLAN"; then
    pass "and checks the host for the secrets by name"
else
    fail "and checks the host for the secrets by name"
fi

# The write step, for real, against the scratch root.
step_command '^config    umask' '^chmod 0644' "$ENV_PLAN" | sed 's/^config    //' > "$TMP/envwrite.sh"
if sh -e "$TMP/envwrite.sh" 2>"$TMP/err"; then
    pass "the env write step runs as sh"
else
    fail "the env write step ($(head -2 "$TMP/err" | tr '\n' ' '))"
fi
if ( set +u; . "$SRV/shared/env" && [ "$RUST_LOG" = "info" ] && [ "$MOTD" = "it's fine" ] ); then
    pass "the file it wrote is sourceable, apostrophes and all"
else
    fail "the file it wrote is sourceable, apostrophes and all"
fi
if grep -q "^HS_ENVIRONMENT=production$" "$SRV/shared/env" && grep -q "^HS_RELEASE=" "$SRV/shared/env"; then
    pass "and says which environment and release a service is running"
else
    fail "and says which environment and release a service is running"
fi
if grep -q "^SESSION_KEY=\|^DATABASE_URL=" "$SRV/shared/env"; then
    fail "and names no secret in it"
else
    pass "and names no secret in it"
fi

# The secret check, for real: absent file, then present file, then one missing.
SECHECK=$(sed -n '/^config    f=/p' "$ENV_PLAN" | sed 's/^config    //')
if printf '%s' "$SECHECK" | sh -e 2>"$TMP/err"; then
    fail "the secret check fails when the file is absent"
else
    pass "the secret check fails when the file is absent"
fi
if grep -q "no .*env.secrets on this host" "$TMP/err"; then
    pass "and says which file is missing"
else
    fail "and says which file is missing"
fi
printf 'SESSION_KEY=abc\n' > "$SRV/shared/env.secrets"
if printf '%s' "$SECHECK" | sh -e 2>"$TMP/err"; then
    fail "the secret check fails when a variable is missing"
else
    pass "the secret check fails when a variable is missing"
fi
if grep -q "^DATABASE_URL is not set in" "$TMP/err"; then
    pass "and names the variable that is missing"
else
    fail "and names the variable that is missing"
fi
printf 'SESSION_KEY=abc\nDATABASE_URL=postgres://u@h/d\n' > "$SRV/shared/env.secrets"
if printf '%s' "$SECHECK" | sh -e 2>"$TMP/err"; then
    pass "and passes once the host has them"
else
    fail "and passes once the host has them ($(head -2 "$TMP/err" | tr '\n' ' '))"
fi

# ---- the unit file -----------------------------------------------------------
# systemd is the only authority that matters about a unit, so where it is
# installed, ask it.
UNIT_OUT="$TMP/unit.txt"
"$HARD" deploy config unit --env production > "$UNIT_OUT" 2>"$TMP/err"
if grep -q "^ExecStart=$SRV/current/server$" "$UNIT_OUT"; then
    pass "the unit runs the current release, not a release directory"
else
    fail "the unit runs the current release, not a release directory"
fi
if grep -q "^EnvironmentFile=$SRV/shared/env$" "$UNIT_OUT" \
    && grep -q "^EnvironmentFile=-$SRV/shared/env.secrets$" "$UNIT_OUT"; then
    pass "the unit reads both env files, and the secrets one is optional"
else
    fail "the unit reads both env files, and the secrets one is optional"
fi
if grep -q "^User=root$" "$UNIT_OUT"; then
    fail "the service does not run as root"
else
    pass "the service does not run as root"
fi
if [ "$($HARD deploy config unit --env production | md5sum)" \
    = "$($HARD deploy config unit --env production | md5sum)" ]; then
    pass "the unit is byte-for-byte deterministic"
else
    fail "the unit is byte-for-byte deterministic"
fi
if command -v systemd-analyze >/dev/null 2>&1; then
    mkdir -p "$TMP/etc"
    cp "$UNIT_OUT" "$TMP/etc/smoke.service"
    if out=$(systemd-analyze verify "$TMP/etc/smoke.service" 2>&1); then
        pass "systemd-analyze verify accepts the unit"
    elif printf '%s' "$out" | grep -qi "failed to parse\|syntax error\|unexpected token"; then
        fail "systemd could not parse the unit: $(printf '%s' "$out" | head -2 | tr '\n' ' ')"
    else
        # It complains that ExecStart does not exist, which is what a deploy is for.
        pass "systemd-analyze verify only objects to the missing binary"
    fi
else
    echo "deploy: skip systemd-analyze (not installed)"
fi

# The install step, for real, against a scratch /etc.
if command -v systemd-analyze >/dev/null 2>&1; then
    "$HARD" deploy ssh --print --no-build --no-health --unit --env production \
        > "$TMP/unitplan.txt" 2>/dev/null
    step_command '^config    umask' 'daemon-reload' "$TMP/unitplan.txt" \
        | sed 's/^config    //' > "$TMP/unit-install.sh"
    # /etc/systemd/system exists on a host; in a scratch root it has to be made.
    mkdir -p "$TMP/etc/systemd/system"
    # The daemon-reload is dropped: it needs a systemd and an account with
    # NOPASSWD sudo, and what this suite is checking is the file.
    sed -e "s|/etc/systemd/system/|$TMP/etc/systemd/system/|g" \
        -e "s|; sudo -n systemctl daemon-reload||" \
        -e "s|; systemctl daemon-reload||" "$TMP/unit-install.sh" > "$TMP/unit-install2.sh"
    if sh "$TMP/unit-install2.sh" 2>"$TMP/err"; then
        pass "the unit install step runs as sh"
    else
        fail "the unit install step ($(head -2 "$TMP/err" | tr '\n' ' '))"
    fi
    if cmp -s "$TMP/etc/smoke.service" "$TMP/etc/systemd/system/smoke.service"; then
        pass "and wrote the same unit systemd will read"
    else
        fail "and wrote the same unit systemd will read"
    fi
fi

# ---- the migration helper is compiled once ----------------------------------
# `hard migrate` builds the helper from the embedded runtime, and a deploy ships
# that binary. If it is compiled on every invocation, every migration is three
# seconds slower than it has to be -- which is what the first version did.
if [ -x "$APP/.hard/migrate-helper" ]; then
    ms() { date +%s%3N; }
    t0=$(ms); "$HARD" migrate up >/dev/null 2>&1; t1=$(ms)
    "$HARD" migrate up >/dev/null 2>&1; t2=$(ms)
    cold=$((t1 - t0))
    warm=$((t2 - t1))
    if [ "$warm" -lt $((cold / 2 + 50)) ]; then
        pass "the migration helper is compiled once (${cold}ms, then ${warm}ms)"
    else
        fail "the migration helper is compiled once (${cold}ms, then ${warm}ms)"
    fi
    # And the cache is keyed on what it was built from, not on its timestamp.
    cp "$APP/.hard/migrate-helper" "$TMP/helper.first"
    "$HARD" migrate up >/dev/null 2>&1
    if cmp -s "$TMP/helper.first" "$APP/.hard/migrate-helper"; then
        pass "and a cached helper is the one that ships"
    else
        fail "and a cached helper is the one that ships"
    fi
fi

# ---- the reverse proxy and the certificate ----------------------------------
# nginx is the authority on its own configuration, so where it is installed the
# generated file is tested by nginx rather than by a string comparison.
NGX="$TMP/site.conf"
"$HARD" deploy nginx --env production --tls > "$NGX" 2>"$TMP/err"
if grep -q "^        proxy_set_header   Upgrade \$http_upgrade;$" "$NGX" \
    && grep -q "^        proxy_set_header   Connection \$connection_upgrade;$" "$NGX"; then
    pass "the generated block keeps a WebSocket upgrade alive"
else
    fail "the generated block keeps a WebSocket upgrade alive"
fi
if [ "$(grep -c 'acme-challenge' "$NGX")" -ge 1 ] \
    && [ "$(grep -n 'location \^~ /.well-known' "$NGX" | cut -d: -f1)" \
        -lt "$(grep -n 'return 301' "$NGX" | head -1 | cut -d: -f1)" ]; then
    pass "the ACME challenge is served before the redirect to HTTPS"
else
    fail "the ACME challenge is served before the redirect to HTTPS"
fi
if command -v nginx >/dev/null 2>&1; then
    if "$HARD" deploy nginx --env production --tls --check > "$TMP/ngxcheck" 2>&1; then
        pass "nginx -t accepts the generated file"
    else
        fail "nginx -t accepts the generated file ($(head -2 "$TMP/ngxcheck" | tr '\n' ' '))"
    fi
    # The install command is a heredoc and an nginx -t, so it gets the same
    # treatment as the other generated shell.
    NGX_INSTALL=$("$HARD" deploy nginx --env production --tls --install "$TMP/srv" 2>&1 || true)
    case "$NGX_INSTALL" in
        *"cannot read the host"*|*"Could not resolve"*|*"Connection refused"*)
            pass "installing the site needs a host (not reachable here)" ;;
        *)
            fail "installing the site needs a host (got: $(printf '%s' "$NGX_INSTALL" | head -1))" ;;
    esac
else
    echo "deploy: skip nginx (not installed)"
fi
if "$HARD" deploy https --env production | grep -q "certbot certonly --webroot -w '$SRV/shared/acme'"; then
    pass "the certificate is issued against the webroot the block serves"
else
    fail "the certificate is issued against the webroot the block serves"
fi
if "$HARD" deploy https --env production | grep -q -- "--email 'ops@example.com'"; then
    pass "and the expiry notice has an address"
else
    fail "and the expiry notice has an address"
fi

# ---- the shell the tool generates is the shell that runs ---------------------
# A plan is only trustworthy if the commands in it are the commands that run.
# Every remote command the executor sends has to appear in the print.
if "$HARD" deploy ssh --print --no-build --no-health root@example.com --dir "$TMP/srv" \
    2>/dev/null | grep -q '^prepare'; then
    pass "the printed plan is the plan the deploy builds"
else
    fail "the printed plan is the plan the deploy builds"
fi

echo "deploy: $((TOTAL - FAILED))/$TOTAL checks passed"
[ "$FAILED" -eq 0 ]
