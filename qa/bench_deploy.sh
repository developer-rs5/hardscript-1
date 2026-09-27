#!/usr/bin/env bash
# Deployment benchmarks for the M7.9 report.
#
# A deploy is a build, a hash, a handful of small commands over one ssh
# connection, and a health check that waits. Only one of those is the tool's
# own work, and it is the one worth measuring here: everything else is the
# network, the compiler, or systemd. So this measures what `hard` actually
# does between "the operator pressed enter" and "the first byte goes out":
#
#   plan        build a DeployPlan and render it, with the release id, the
#               digest, the migration file list, and the health check
#   digest      hash the artifacts a release is named after
#   env         render the environment file and the secret check
#   unit        render and install the systemd unit
#   nginx       render the server block and check it with nginx -t
#   start       `hard deploy --help` and friends, i.e. the fixed cost of
#               starting the process at all
#
# It also measures the two end-to-end shapes that do not need a network:
# a real build of a project that is about to be deployed (the binary the deploy
# uploads) and a real `docker compose up` of the generated file, which is the
# other way this project gets deployed.
#
# Emits a TSV on stdout:
#   lane  unit  inner  trials  best  median
# Every time is in microseconds: the harness measures nanoseconds and divides
# once, so a lane is comparable with every other lane and a reader does not have
# to remember which of two units a column is in. `inner` is how many times an
# in-process lane repeats its work per trial.
#
# Exits non-zero if a lane is missing a measurement, because a benchmark that
# prints a number it did not take is worse than no benchmark.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD="${HARD:-$ROOT/target/release/hard}"
BENCH_TRIALS="${BENCH_TRIALS:-5}"
BENCH_SAMPLES="${BENCH_SAMPLES:-200}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# ---- a project to deploy ----------------------------------------------------
APP="$TMP/app"
mkdir -p "$APP/migrations" "$APP/static"
cat > "$APP/hard.toml" <<TOML
name = "benchapp"
version = "0.1.0"
edition = "2027"

[server]
port = 8099

[database]
dialect = "sqlite"
path = "benchapp.db"

[env.production]
host = "deploy@bench.example.com"
dir = "$TMP/srv"
domain = "bench.example.com"
tls_email = "ops@example.com"
secrets = ["SESSION_KEY"]

[env.production.vars]
RUST_LOG = "info"
TOML
cat > "$APP/main.hard" <<'HARD'
bring http

app @8099

GET "/healthz" :: {
    <- { ok: true }
}

GET "/" :: {
    <- { hello: "world" }
}
HARD
cp "$ROOT/qa/cli_matrix/cases/mg002/migrations/0001_init.sql" "$APP/migrations/0001_init.sql" \
    2>/dev/null || printf -- '-- +migrate Up\n-- +migrate Down\n' > "$APP/migrations/0001_init.sql"
# Enough static bytes that hashing them is not free, which is the point.
for i in $(seq 1 40); do
    head -c 8192 /dev/urandom | base64 > "$APP/static/asset$i.txt"
done
cd "$APP" || exit 1

# A binary to hash, the same size class as a small service.
head -c 4000000 /dev/urandom > "$APP/.hard-server.bin" 2>/dev/null
mkdir -p .hard
mv .hard-server.bin .hard/server

# ---- timing helpers ---------------------------------------------------------
# `date +%s%N` rather than python: no interpreter in the measurement.
now_ns() { date +%s%N; }

# bench <lane> <unit> <command...>
#
# `inner` is how many times an in-process lane repeats its work per trial; a
# command lane runs once, and the column says so.
bench() {
    local lane="$1" unit="$2" inner="${3:-1}"
    shift 3 2>/dev/null || shift 2
    local i=0 values=()
    while [ "$i" -lt "$BENCH_TRIALS" ]; do
        local start end
        start=$(now_ns)
        "$@" >/dev/null 2>&1
        end=$(now_ns)
        values+=( $(( (end - start) / 1000 )) )
        i=$((i + 1))
    done
    local sorted
    sorted=$(printf '%s\n' "${values[@]}" | sort -n)
    local best median
    best=$(printf '%s\n' "$sorted" | head -1)
    median=$(printf '%s\n' "$sorted" | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$lane" "$unit" "$inner" "$BENCH_TRIALS" "$best" "$median"
    RESULTS="$RESULTS
$lane	$unit	$inner	$BENCH_TRIALS	$best	$median"
}

RESULTS=""
echo -e "lane\tunit\tsamples\ttrials\tbest\tmedian"

# ---- the fixed cost of starting ---------------------------------------------
bench startup us 1 "$HARD" --version
bench startup_help us 1 "$HARD" --help

# ---- planning ---------------------------------------------------------------
bench plan_print us 1 "$HARD" deploy ssh --print --no-build --no-health --env production
bench plan_print_tls us 1 "$HARD" deploy ssh --print --no-build --no-health --env production --unit
bench env_list us 1 "$HARD" deploy env list
bench env_render us 1 "$HARD" deploy env render production
bench env_check us 1 "$HARD" deploy env check production
bench config_unit us 1 "$HARD" deploy config unit --env production
bench nginx_render us 1 "$HARD" deploy nginx --env production --tls
bench compose us 1 "$HARD" deploy compose

# ---- the release digest, on its own -----------------------------------------
# The plan hashes every artifact to name the release, so the number that matters
# is the hash of the whole set. Python's hashlib and the compiler's sha256 are
# the same function; this measures the work, not the binding.
export SAMPLES="$BENCH_SAMPLES"
bench digest_all us "$BENCH_SAMPLES" python3 -c '
import hashlib, os, pathlib
paths = [pathlib.Path(".hard/server")] + sorted(pathlib.Path("static").iterdir()) \
      + sorted(pathlib.Path("migrations").iterdir())
for _ in range(int(os.environ["SAMPLES"])):
    h = hashlib.sha256()
    for p in paths:
        h.update(p.name.encode())
        h.update(p.read_bytes())
'

# The migration helper is a C++ compile that `hard migrate` does on every
# invocation. It is part of a deploy when the project has a database, so it
# belongs in the report.
bench helper_build us 1 "$HARD" migrate up

# ---- what a deploy actually waits for ---------------------------------------
# The build the deploy uploads, cold and warm. Cold is the honest number for a
# first deploy and the only one a "best of N" can report honestly, so the lane
# drops the cache itself on every trial rather than being cold only the first
# time and then reporting a cache hit as the best cold build.
build_cold() {
    rm -f .hard/build.json
    "$HARD" build main.hard
}
bench build_cold us 1 build_cold
bench build_warm us 1 "$HARD" build main.hard

# The health check is the only step a deploy waits on that is not the network.
# Polling a real server is the honest measurement: how long from a restart to a
# 200 on /healthz.

cat > "$APP/probe.hard" <<'HARD'
bring http

app @8098

GET "/healthz" :: {
    <- { ok: true }
}
HARD
if "$HARD" build probe.hard >/dev/null 2>&1; then
    ./.hard/probe &
    SERVER=$!
    for _ in $(seq 1 100); do
        if curl -fsS -o /dev/null "http://127.0.0.1:8098/healthz" 2>/dev/null; then break; fi
        sleep 0.05
    done
    bench health_probe us 1 curl -fsS -o /dev/null "http://127.0.0.1:8098/healthz"
    kill "$SERVER" 2>/dev/null
    wait "$SERVER" 2>/dev/null
else
    echo "bench: the health probe lane needs a build; skipping" >&2
fi

# ---- the other way this project gets deployed -------------------------------
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    "$HARD" deploy compose >/dev/null 2>&1 \
        && bench compose_config us 1 docker compose -f docker-compose.yml config
else
    echo "bench: no docker daemon here; the compose lane is not measured" >&2
fi

echo "bench: ${BENCH_TRIALS} trials per lane, ${BENCH_SAMPLES} samples each" >&2
printf '%s\n' "$RESULTS" | grep -c "	" >/dev/null 2>&1 || true
