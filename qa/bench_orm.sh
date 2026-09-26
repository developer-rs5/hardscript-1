#!/usr/bin/env bash
# ORM benchmark suite (M5.3.8).
#
# Builds three bench programs from the runtime headers and runs them:
#   qa/bench/orm_sqlite.cpp   operation lanes against in-memory SQLite
#   qa/bench/orm_pg.cpp       the same lanes against the scripted mock server
#   qa/bench/orm_migrate.cpp  migration parse/apply/rollback/status/seed
#
# Each program prints `RESULT <workload> <backend> <ops> <seconds>` lines.
# This script runs every program REPS times, takes the median per workload,
# and writes the three measured reports:
#   reports/orm-performance.md
#   reports/db-performance.md
#   reports/migration-performance.md
#
# No number in those files comes from anywhere but a run below: a missing
# RESULT line fails the script instead of leaving a blank cell.
#
# Run: qa/bench_orm.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPS="${REPS:-3}"
CXX="${CXX:-g++}"

TMP=$(mktemp -d /tmp/hs-ormbench-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

echo "bench: building (-O2)"
for prog in orm_sqlite orm_pg orm_migrate; do
    if ! $CXX -std=c++17 -O2 -pthread -Werror -I "$ROOT/runtime" -I "$ROOT/qa/orm" -I "$ROOT/qa/bench" \
        "$ROOT/qa/bench/$prog.cpp" -o "$TMP/$prog" -ldl 2>"$TMP/$prog.err"; then
        echo "bench: FAIL building $prog"
        head -20 "$TMP/$prog.err"
        exit 1
    fi
done

echo "bench: running ($REPS reps each)"
: > "$TMP/results.tsv"
for rep in $(seq 1 "$REPS"); do
    for prog in orm_sqlite orm_pg orm_migrate; do
        if ! timeout 600 "$TMP/$prog" >"$TMP/$prog.$rep.out" 2>"$TMP/$prog.$rep.err"; then
            echo "bench: FAIL running $prog (rep $rep)"
            tail -5 "$TMP/$prog.$rep.err"
            exit 1
        fi
        grep -E "^(RESULT|RSS|HEAP) " "$TMP/$prog.$rep.out" >> "$TMP/results.tsv" || {
            echo "bench: FAIL no RESULT lines from $prog (rep $rep)"
            exit 1
        }
    done
done

exec python3 - "$TMP/results.tsv" "$ROOT" "$REPS" <<'PY'
import subprocess
import sys
from collections import defaultdict

tsv, root, reps = sys.argv[1], sys.argv[2], int(sys.argv[3])

# workload -> backend -> list of (ops, seconds); rss/heap per backend (max)
times = defaultdict(lambda: defaultdict(list))
metric = defaultdict(list)
for line in open(tsv):
    p = line.split()
    if p[0] == "RESULT" and len(p) == 5:
        _, wl, be, ops, sec = p
        times[wl][be].append((int(ops), float(sec)))
    elif p[0] in ("RSS", "HEAP") and len(p) == 3:
        metric[(p[0], p[1])].append(int(p[2]))


def median(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2]


def cell(wl, be):
    """(ops_per_sec, mean_usec_per_op) at the median run."""
    runs = times[wl][be]
    if not runs:
        raise SystemExit(f"bench: missing RESULT for {wl} {be}")
    ops, sec = runs[0][0], median([s for _, s in runs])
    if any(o != ops for o, _ in runs):
        raise SystemExit(f"bench: op count moved for {wl} {be}")
    return ops / sec, sec / ops * 1e6


def fmt_rate(x):
    return f"{x:,.0f}" if x < 1_000_000 else f"{x / 1_000_000:.2f}M"


def machine():
    def sh(cmd):
        try:
            return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10).stdout.strip()
        except Exception:
            return "unknown"

    cpu = sh("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2-")
    mem = sh("free -m | awk '/Mem:/ {print $2}'")
    nproc = sh("nproc")
    gxx = sh("g++ --version | head -1")
    sqlite = sh("python3 -c 'import sqlite3; print(sqlite3.sqlite_version)'")
    rev = sh(f"git -C {root} rev-parse --short HEAD")
    return cpu.strip(), mem, nproc, gxx, sqlite, rev


cpu, mem, nproc, gxx, sqlite_v, rev = machine()

import datetime

stamp = datetime.datetime.now().strftime("%Y-%m-%d")
OP_ORDER = ["startup", "create", "update", "find", "find_many", "join_has_many", "join_belongs_to",
            "tx_commit", "tx_savepoint", "tx_rollback", "delete"]
BATCH_ORDER = ["batch_create_100", "batch_create_1000", "batch_create_10000",
               "batch_find_10000", "batch_update_10000", "batch_delete_10000"]
MIG_ORDER = ["migrate_parse", "migrate_up_1tbl", "migrate_up_5tbl", "migrate_up_20tbl",
             "migrate_status", "migrate_down", "seed_1000"]

head = f"""# ORM performance ({stamp})

Measured with `qa/bench_orm.sh` at `{rev}` on {nproc} CPUs ({cpu}), {mem} MB RAM,
`{gxx}`, SQLite {sqlite_v}. Median of {reps} runs; every cell below comes from a
run, and the script fails instead of printing a number it did not measure.

Method: in-memory SQLite (CPU and library, not disk) and the scripted mock
PostgreSQL server on loopback (client framing plus a socket round trip, no
query execution — reported as `pg-mock`). Bench programs are built `-O2` from
the same headers the tests compile. `startup` opens a connection and runs DDL,
amortized over 20 iterations including the one-time library load.
"""

# ---- reports/orm-performance.md ----
op_rows = []
for wl in OP_ORDER:
    r_s, u_s = cell(wl, "sqlite")
    r_p, u_p = cell(wl, "pg-mock")
    op_rows.append(f"| `{wl}` | {fmt_rate(r_s)} | {u_s:,.1f} | {fmt_rate(r_p)} | {u_p:,.1f} |")
batch_rows = []
for wl in BATCH_ORDER:
    r_s, u_s = cell(wl, "sqlite")
    batch_rows.append(f"| `{wl}` | {fmt_rate(r_s)} | {u_s:,.1f} |")
# pg-mock batch lanes exist for 100/1000 only; the 10k lanes are SQLite.
pg_batch_rows = []
for wl in ["batch_create_100", "batch_create_1000"]:
    r_p, u_p = cell(wl, "pg-mock")
    pg_batch_rows.append(f"| `{wl}` | {fmt_rate(r_p)} | {u_p:,.1f} |")

orm_md = head + """
## Single-row operations (ops/s, mean us/op)

| workload | sqlite ops/s | sqlite us/op | pg-mock ops/s | pg-mock us/op |
|---|---|---|---|---|
""" + "\n".join(op_rows) + """

`find_many` reads 200 keys per call; `join_has_many` reads one user's posts
with no index on the foreign key, so each call scans the table — the honest
cost of an unindexed join, and the reason migrations exist. `tx_commit` is an
empty guarded block; `tx_savepoint` nests two; `tx_rollback` inserts a row and
throws it away.

## Batches (rows/s, mean us/row, SQLite)

| workload | rows/s | us/row |
|---|---|---|
""" + "\n".join(batch_rows) + """

## Batches (rows/s, pg-mock)

| workload | rows/s | us/row |
|---|---|---|
""" + "\n".join(pg_batch_rows) + """

A batch is one transaction: ten thousand rows land or none do. `create_many`
inserts row by row through the same path as `create`; `update_many` shares one
prepared statement per column shape; `delete_many` and `find_many` cut long id
lists into chunks under the backend's variable cap.
"""
open(f"{root}/reports/orm-performance.md", "w").write(orm_md)

# ---- reports/db-performance.md ----
ratio_rows = []
for wl in OP_ORDER:
    r_s, _ = cell(wl, "sqlite")
    r_p, _ = cell(wl, "pg-mock")
    ratio_rows.append(f"| `{wl}` | {fmt_rate(r_s)} | {fmt_rate(r_p)} | {r_s / r_p:.1f}x |")
rss_s = max(metric[("RSS", "sqlite")])
rss_p = max(metric[("RSS", "pg-mock")])
heap_s = max(metric[("HEAP", "sqlite")])
heap_p = max(metric[("HEAP", "pg-mock")])

db_md = head + """
## Backend comparison (ops/s)

| workload | sqlite | pg-mock | sqlite / pg-mock |
|---|---|---|---|
""" + "\n".join(ratio_rows) + f"""

The gap is the wire: every PostgreSQL statement is five framed messages
(Parse, Bind, Describe, Execute, Sync) plus a socket round trip, against
SQLite's function call into the same process. Both backends speak TCP_NODELAY
on loopback; without it the 40ms delayed-ACK timer owns every number. The
mock executes nothing, so these are client-stack costs — a floor, not a
ceiling, for a real server.

## Process footprint (peak RSS KB, heap bytes held at end of run)

| backend | peak RSS KB | heap held B |
|---|---|---|
| sqlite | {rss_s} | {heap_s} |
| pg-mock | {rss_p} | {heap_p} |

Heap held is glibc `mallinfo2` `uordblks` at exit: everything the bench
process kept, which in these programs is the last working set. Per-operation
allocation behavior of values is covered by the value-engine reports; the ORM
layer adds no per-row allocation beyond the rows themselves.
"""
open(f"{root}/reports/db-performance.md", "w").write(db_md)

# ---- reports/migration-performance.md ----
mig_rows = []
for wl in MIG_ORDER:
    r_s, u_s = cell(wl, "sqlite")
    mig_rows.append(f"| `{wl}` | {fmt_rate(r_s)} | {u_s:,.1f} |")
_, u_1 = cell("migrate_up_1tbl", "sqlite")
_, u_5 = cell("migrate_up_5tbl", "sqlite")
_, u_20 = cell("migrate_up_20tbl", "sqlite")

mig_md = head + """
## Migrations and seeds, SQLite (ops/s, mean us/op)

| workload | ops/s | us/op |
|---|---|---|
""" + "\n".join(mig_rows) + """

`migrate_parse` parses a 3-table migration file fifty times: pure CPU, no
database. `migrate_up_Ntbl` applies a fresh N-table migration (tables plus
indexes plus a foreign key) on a new in-memory database, ten times each, after
a warmup that takes the one-time library load off the first lane.
`migrate_status` reads history fifty times; `migrate_down` applies then rolls
back ten times; `seed_1000` inserts a parent plus a thousand child rows in one
transaction.

Apply cost grows with the migration: ~%.2fms for one table, ~%.2fms for five,
~%.2fms for twenty. Parsing is microseconds; status is a single indexed read.
""" % (u_1 / 1000, u_5 / 1000, u_20 / 1000)
open(f"{root}/reports/migration-performance.md", "w").write(mig_md)
print("bench: wrote reports/orm-performance.md reports/db-performance.md reports/migration-performance.md")
PY
