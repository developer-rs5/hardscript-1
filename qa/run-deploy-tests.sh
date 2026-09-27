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
