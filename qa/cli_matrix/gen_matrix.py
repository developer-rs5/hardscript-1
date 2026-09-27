#!/usr/bin/env python3
"""Phase 5 — HardScript CLI exit-code matrix (generator).

Materializes one sandbox per case under qa/cli_matrix/cases/<id>/ and writes
matrix.tsv (id, argv, expected_rc, setup, note). Fixtures are read from
qa/cli_matrix/fixtures/ at generation time so the generator itself carries
no byte-critical program text (single source of truth for the golden bytes).

Run:  python3 gen_matrix.py
"""

import shutil, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "qa" / "cli_matrix" / "cases"
TSV = ROOT / "qa" / "cli_matrix" / "matrix.tsv"
FIX = Path(__file__).resolve().parent / "fixtures"

OK, ERR, USAGE = 0, 1, 2

F = lambda n: (FIX / n).read_text()
MAIN_OK = F("main_ok.hard")
MAIN_UNFMT = F("main_unfmt.hard")
MAIN_SYNERR = F("main_synerr.hard")
MAIN_TESTFAIL = F("main_testfail.hard")
TOML = F("hard.toml")


def write_project(d, source):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(source)
    (d / "hard.toml").write_text(TOML)


def setup_ok(d):
    write_project(d, MAIN_OK)


def setup_unfmt(d):
    write_project(d, MAIN_UNFMT)


def setup_synerr(d):
    write_project(d, MAIN_SYNERR)


def setup_testfail(d):
    write_project(d, MAIN_TESTFAIL)


def setup_missing(d):
    d.mkdir(parents=True, exist_ok=True)


def setup_nested(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "sub").mkdir(exist_ok=True)
    write_project(d / "sub", MAIN_OK)
    (d / "main.hard").write_text(MAIN_OK)
    (d / "hard.toml").write_text(TOML)


def setup_sub(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "sub").mkdir(exist_ok=True)
    write_project(d / "sub", MAIN_OK)


def setup_deep3(d):
    p = d / "a" / "b" / "c"
    p.mkdir(parents=True, exist_ok=True)
    write_project(p, MAIN_OK)


def setup_exists(d):
    d.mkdir(parents=True, exist_ok=True)
    write_project(d, MAIN_OK)
    (d / "proj").mkdir(exist_ok=True)


def setup_no_toml(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_OK)


def setup_readonly(d):
    d.mkdir(parents=True, exist_ok=True)
    write_project(d, MAIN_OK)
    for p in sorted(d.rglob("*"), reverse=True):
        p.chmod(0o555)


MAIN_MODELS = F("main_models.hard")
MAIN_CACHE = F("main_cache.hard")
MAIN_QUEUE = F("main_queue.hard")
MAIN_SCHED = F("main_sched.hard")
MAIN_SESSION = F("main_session.hard")
MAIN_LIMIT = F("main_limit.hard")
MAIN_EMAIL = F("main_email.hard")
MAIN_METRICS = F("main_metrics.hard")
MAIN_CLUSTER = F("main_cluster.hard")
MAIN_DOCKER = F("main_docker.hard")
MAIN_DEPLOY = F("main_deploy.hard")
DB_TOML = """schema = 1
name = "hsdb"
version = "0.1.0"

[database]
dialect = "sqlite"
path = "app.db"
"""


def setup_db(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_MODELS)
    (d / "hard.toml").write_text(DB_TOML)


def setup_cache(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_CACHE)
    (d / "hard.toml").write_text(DB_TOML)


def setup_queue(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_QUEUE)
    (d / "hard.toml").write_text(DB_TOML)


def setup_sched(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_SCHED)
    (d / "hard.toml").write_text(DB_TOML)


def setup_session(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_SESSION)
    (d / "hard.toml").write_text(DB_TOML)


def setup_limit(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_LIMIT)
    (d / "hard.toml").write_text(DB_TOML)


def setup_email(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_EMAIL)
    (d / "hard.toml").write_text(DB_TOML)


def setup_metrics(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_METRICS)
    (d / "hard.toml").write_text(DB_TOML)


def setup_docker(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_DOCKER)
    (d / "hard.toml").write_text(DB_TOML)


ENVS_TOML = DB_TOML + """
[env.production]
host = "deploy@app.example.com"
dir = "/srv/hsdb"
port = 8080
health_path = "/live"
secrets = ["DATABASE_URL"]

[env.production.vars]
RUST_LOG = "info"

[env.broken]
dir = "/srv/hsdb; rm -rf /"
"""

MIGRATION_SQL = """-- hardscript:migration 0001
-- hardscript:fingerprint -- table t\\n  "id" INTEGER PRIMARY KEY NOT NULL\\n
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
"""


def setup_deploy(d):
    # A project with everything a deploy ships: a health route to probe, a
    # migration, and a static file.
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_DEPLOY)
    (d / "hard.toml").write_text(ENVS_TOML)
    (d / "migrations").mkdir(exist_ok=True)
    (d / "migrations" / "0001_init.sql").write_text(MIGRATION_SQL)
    (d / "static").mkdir(exist_ok=True)
    (d / "static" / "app.css").write_text("body { color: red }\n")


def setup_noenv(d):
    # A project with no environments at all: the tools have to say what to add.
    setup_deploy(d)
    (d / "hard.toml").write_text(DB_TOML)


def setup_badenv(d):
    # An environment with a root that would be a shell problem on the host.
    setup_deploy(d)
    (d / "hard.toml").write_text(ENVS_TOML.replace(
        'dir = "/srv/hsdb"\nport = 8080', 'dir = "/srv/hsdb; rm -rf /"\nport = 8080'))


def setup_cluster(d):
    d.mkdir(parents=True, exist_ok=True)
    (d / "main.hard").write_text(MAIN_CLUSTER)
    (d / "hard.toml").write_text(DB_TOML)


SETUPS = {
    "ok": setup_ok, "unfmt": setup_unfmt, "synerr": setup_synerr,
    "testfail": setup_testfail, "missing": setup_missing,
    "nested": setup_nested, "sub": setup_sub, "deep3": setup_deep3,
    "exists": setup_exists, "no_toml": setup_no_toml, "ro": setup_readonly,
    "db": setup_db, "cache": setup_cache, "queue": setup_queue, "sched": setup_sched,
    "session": setup_session, "limit": setup_limit, "email": setup_email, "metrics": setup_metrics, "cluster": setup_cluster, "docker": setup_docker,
    "deploy": setup_deploy, "noenv": setup_noenv, "badenv": setup_badenv,
}

# id, argv, expected_rc, setup, note
CASES = [
    # global ===================================================================
    ("g001", ["--version"], 0, "missing", "version long"),
    ("g002", ["-V"], 0, "missing", "version short"),
    ("g003", ["--help"], 0, "missing", "help long"),
    ("g004", ["-h"], 0, "missing", "help short"),
    ("g005", ["help"], 0, "missing", "help cmd"),
    ("g006", [], 0, "missing", "no args => help"),
    ("g007", ["frobnicate"], 2, "missing", "unknown cmd rc1"),
    ("g008", ["--nope"], 2, "missing", "unknown flag rc1"),
    ("g009", [], 0, "ok", "help in project"),

    # new ======================================================================
    ("n001", ["new"], 2, "missing", "new missing name"),
    ("n002", ["new", "proj"], 0, "missing", "new ok"),
    ("n003", ["new", "proj"], 2, "exists", "new existing"),
    ("n004", ["new", "proj", "extra"], 0, "missing", "new leniency extra arg"),
    ("n005", ["new", ".."], 2, "missing", "new .."),
    ("n006", ["new", ""], 2, "missing", "new empty name"),

    # build ====================================================================
    ("b001", ["build", "main.hard"], 0, "ok", "build ok"),
    ("b002", ["build", "missing.hard"], 1, "missing", "build missing file"),
    ("b003", ["build"], 1, "missing", "build no default"),
    ("b004", ["build", "main.hard"], 1, "synerr", "build syntax error"),
    ("b005", ["build", "sub/main.hard"], 0, "sub", "build nested"),
    ("b006", ["build", "main.hard", "extra"], 0, "ok", "build extra arg"),

    # run ======================================================================
    ("r001", ["run", "main.hard"], 124, "ok", "run server up (healthy)"  ),
    ("r002", ["run", "missing.hard"], 1, "missing", "run missing file"),
    ("r003", ["run", "main.hard"], 1, "synerr", "run syntax error"),
    ("r004", ["run"], 1, "missing", "run no default"),

    # test =====================================================================
    ("t001", ["test", "main.hard"], 124, "ok", "test ok"),
    ("t002", ["test", "main.hard"], 1, "testfail", "test fail"),
    ("t003", ["test", "missing.hard"], 1, "missing", "test missing"),
    ("t004", ["test", "main.hard"], 1, "synerr", "test syntax error"),
    ("t005", ["test"], 1, "missing", "test no default"),

    # fmt ======================================================================
    ("f001", ["fmt", "main.hard"], 0, "ok", "fmt canonical ok"),
    ("f002", ["fmt", "main.hard"], 0, "unfmt", "fmt reformat"),
    ("f003", ["fmt", "--check", "main.hard"], 1, "ok", "fmt --check canonical"),
    ("f004", ["fmt", "main.hard"], 1, "missing", "fmt missing"),
    ("f005", ["fmt", "-h"], 1, "missing", "fmt help"),
    ("f006", ["fmt", "--check", "main.hard"], 1, "unfmt", "fmt --check unformatted (rc1 expected? no)"),

    # docs =====================================================================
    ("d001", ["docs", "main.hard"], 0, "ok", "docs ok"),
    ("d002", ["docs"], 1, "missing", "docs no default (rc1)"),
    ("d003", ["docs", "sub/main.hard"], 0, "sub", "docs nested"),

    # add ======================================================================
    ("a001", ["add", "datetime"], 0, "ok", "add known module"),
    ("a002", ["add"], 2, "ok", "add missing name"),
    ("a003", ["add", "nopenope"], 0, "ok", "add unknown module leniency"),

    # doctor / bench ===========================================================
    ("dc001", ["doctor"], 0, "ok", "doctor ok"),
    ("dc002", ["doctor"], 0, "ro", "doctor read-only leniency"),
    ("bm001", ["bench", "main.hard"], 0, "ok", "bench ok"),
    ("bm002", ["bench", "missing.hard"], 1, "missing", "bench missing"),
    # migrations ===============================================================
    ("mg001", ["migrate", "diff"], 1, "missing", "diff with no dialect configured"),
    ("mg002", ["migrate", "diff", "--name", "init"], 0, "db", "diff writes the first migration"),
    ("mg003", ["migrate", "status"], 0, "db", "status with no migrations"),
    ("mg004", ["migrate", "up"], 0, "db", "up with nothing pending"),
    ("mg005", ["migrate", "down"], 1, "db", "down with nothing applied"),
    ("mg006", ["migrate", "diff", "--dialect", "mysql"], 1, "db", "diff with an unknown dialect"),
    ("mg007", ["seed"], 2, "db", "seed with no seed files"),
    # cache ==================================================================
    ("ch001", ["build", "main.hard"], 0, "cache", "build a cache program"),
    ("ch002", ["fmt", "main.hard"], 0, "cache", "format a cache declaration"),
    # queue ==================================================================
    ("q001", ["build", "main.hard"], 0, "queue", "build a queue program"),
    ("q002", ["fmt", "main.hard"], 0, "queue", "format job and worker declarations"),
    # scheduler ==============================================================
    ("s001", ["build", "main.hard"], 0, "sched", "build a schedule program"),
    ("s002", ["fmt", "main.hard"], 0, "sched", "format schedule declarations"),
    # session ================================================================
    ("ss001", ["build", "main.hard"], 0, "session", "build a session program"),
    ("ss002", ["fmt", "main.hard"], 0, "session", "format session calls"),
    # rate limiter ===========================================================
    ("rl001", ["build", "main.hard"], 0, "limit", "build a limit program"),
    ("rl002", ["fmt", "main.hard"], 0, "limit", "format limit declarations"),
    # email ====================================================================
    ("em001", ["build", "main.hard"], 0, "email", "build an email program"),
    ("em002", ["fmt", "main.hard"], 0, "email", "format email.send options"),
    ("em003", ["docs", "main.hard"], 0, "email", "document an email program"),
    # metrics and health =======================================================
    ("mt001", ["build", "main.hard"], 0, "metrics", "build a metrics program"),
    ("mt002", ["fmt", "main.hard"], 0, "metrics", "format metric calls"),
    ("mt003", ["docs", "main.hard"], 0, "metrics", "document a metrics program"),
    # cluster, locks, idempotency =============================================
    ("cl001", ["build", "main.hard"], 0, "cluster", "build a clustered program"),
    ("cl002", ["fmt", "main.hard"], 0, "cluster", "format cluster calls"),
    ("cl003", ["docs", "main.hard"], 0, "cluster", "document a clustered program"),
    # docker generation =======================================================
    ("dk001", ["build", "--docker", "main.hard"], 0, "docker", "generate a Dockerfile"),
    ("dk002", ["build", "--docker", "--print", "main.hard"], 0, "docker", "print a Dockerfile"),
    ("dk003", ["build", "--docker", "missing.hard"], 1, "docker", "a Dockerfile for a missing source fails"),
    # compose generation ======================================================
    ("cp001", ["deploy", "compose", "main.hard"], 0, "docker", "generate a compose file"),
    ("cp002", ["deploy", "compose", "--print", "main.hard"], 0, "docker", "print a compose file"),
    ("cp003", ["deploy"], 0, "docker", "deploy with no subcommand prints help"),
    ("cp004", ["deploy", "nonsense"], 2, "docker", "an unknown deploy subcommand is a usage error"),
    # ssh deployment =========================================================
    ("dp001", ["deploy", "ssh", "--print", "--no-build", "root@example.com"], 0, "deploy", "print a deploy plan"),
    ("dp002", ["deploy", "ssh", "--print", "--no-build"], 2, "deploy", "a deploy with no host is a usage error"),
    ("dp003", ["deploy", "ssh", "--print", "--no-build", "root@example.com", "root@other"], 2, "deploy", "a deploy to two hosts is a usage error"),
    ("dp004", ["deploy", "ssh", "--print", "--no-build", "--release-id", "../../etc", "root@example.com"], 1, "deploy", "a release id that is a path is refused"),
    ("dp005", ["deploy", "ssh", "--print", "--no-build", "--dir", "/opt/web;id", "root@example.com"], 1, "deploy", "a remote root that is a shell problem is refused"),
    ("dp006", ["deploy", "ssh", "--print", "--no-build", "--health-port", "http", "root@example.com"], 2, "deploy", "a health port that is not a number is a usage error"),
    ("dp007", ["deploy", "ssh", "--print", "--no-build", "--nonsense", "root@example.com"], 2, "deploy", "an unknown deploy flag is a usage error"),
    ("dp008", ["deploy", "ssh", "--print", "--no-build", "--no-health", "root@example.com"], 0, "deploy", "a deploy that does not verify still prints"),
    # environments ===========================================================
    ("ev001", ["deploy", "env"], 2, "deploy", "env with no subcommand is a usage error"),
    ("ev002", ["deploy", "env", "nonsense"], 2, "deploy", "an unknown env subcommand is a usage error"),
    ("ev003", ["deploy", "env", "list"], 0, "deploy", "list environments"),
    ("ev004", ["deploy", "env", "show", "production"], 0, "deploy", "show one environment"),
    ("ev005", ["deploy", "env", "show", "nope"], 1, "deploy", "an environment that does not exist fails"),
    ("ev006", ["deploy", "env", "show"], 2, "deploy", "show with no name is a usage error"),
    ("ev007", ["deploy", "env", "render", "production"], 0, "deploy", "render the env file"),
    ("ev008", ["deploy", "env", "check", "production"], 0, "deploy", "check a good environment"),
    ("ev009", ["deploy", "env", "check", "broken"], 1, "badenv", "a root that is a shell problem fails the check"),
    ("ev010", ["deploy", "env", "list"], 0, "noenv", "a project with no environments says what to add"),
    ("ev011", ["deploy", "ssh", "--print", "--no-build", "--env", "production"], 0, "deploy", "an environment can name the host"),
    ("ev012", ["deploy", "ssh", "--print", "--no-build", "--env", "nope"], 1, "deploy", "an environment that does not exist fails a deploy"),
    ("ev013", ["deploy", "ssh", "--print", "--no-build", "--env", "broken"], 1, "badenv", "a bad environment fails a deploy before it connects"),
    # production configuration =================================================
    ("pc001", ["deploy", "config"], 2, "deploy", "config with no subcommand is a usage error"),
    ("pc002", ["deploy", "config", "nonsense"], 2, "deploy", "an unknown config subcommand is a usage error"),
    ("pc003", ["deploy", "config", "unit"], 0, "deploy", "write the unit"),
    ("pc004", ["deploy", "config", "unit", "--env", "production"], 0, "deploy", "write the unit for an environment"),
    ("pc005", ["deploy", "config", "check"], 2, "deploy", "check with no host is a usage error"),
    ("pc006", ["deploy", "ssh", "--print", "--no-build", "--no-health", "--unit", "root@example.com"], 0, "deploy", "a deploy that installs the unit says so"),
    ("pc007", ["deploy", "ssh", "--print", "--no-build", "--no-health", "root@example.com"], 0, "deploy", "and one that does not, leaves it alone"),
    # logs and lifecycle ======================================================
    ("op001", ["deploy", "logs", "-n", "all", "root@example.com"], 2, "deploy", "a line count that is not a number is a usage error"),
    ("op002", ["deploy", "logs", "--nope", "root@example.com"], 2, "deploy", "an unknown logs flag is a usage error"),
    ("op003", ["deploy", "status"], 2, "deploy", "status with no host is a usage error"),
    ("op004", ["deploy", "releases"], 2, "deploy", "releases with no host is a usage error"),
    ("op005", ["deploy", "restart"], 2, "deploy", "restart with no host is a usage error"),
    ("op006", ["deploy", "logs", "--env", "nope", "root@example.com"], 1, "deploy", "an environment that does not exist fails any deploy command"),
    # rollback and prune ======================================================
    ("rb001", ["deploy", "rollback"], 2, "deploy", "rollback with no host is a usage error"),
    ("rb002", ["deploy", "rollback", "--nope", "root@example.com"], 2, "deploy", "an unknown rollback flag is a usage error"),
    ("rb003", ["deploy", "rollback", "--to", "1.0.0", "--nope", "root@example.com"], 2, "deploy", "a flag that takes a value is parsed as one"),
    ("rb004", ["deploy", "prune", "--keep", "abc", "root@example.com"], 2, "deploy", "a keep count that is not a number is a usage error"),
    ("rb005", ["deploy", "prune", "--keep"], 2, "deploy", "a keep count with no value is a usage error"),
    ("rb006", ["deploy", "prune", "--yes", "root@nonexistent.invalid"], 1, "deploy", "prune on a host it cannot reach says so"),
]

for cid, argv, rc, setup, note in CASES:
    d = OUT / cid
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True, exist_ok=True)
    SETUPS[setup](d)

with TSV.open("w") as f:
    f.write("\t".join(["id", "argv", "expected_rc", "setup", "note"]) + "\n")
    for cid, argv, rc, setup, note in CASES:
        note = note.replace("\t", " ")
        f.write("\t".join([cid, " ".join(argv), str(rc), setup, note]) + "\n")

print(f"generated {len(CASES)} cases -> cases/ and {TSV.relative_to(ROOT)}")
