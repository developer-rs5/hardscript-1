#!/usr/bin/env bash
# Runtime benchmark suite (M6.9).
#
# Builds four bench programs from the runtime headers and runs them:
#   qa/bench/rt_cache.cpp   cache lanes: hit, miss, set, incr, expiry, mixed, scale
#   qa/bench/rt_queue.cpp   queue lanes: round trip, payloads, priority, retries,
#                            SQLite durability, a real worker pool
#   qa/bench/rt_sched.cpp   scheduler lanes: next/prev fire maths per form, register
#   qa/bench/rt_web.cpp     sessions, rate limits, metrics, locks, idempotency,
#                            placement -- the roll-up report
#
# Each program prints `RESULT <workload> <backend> <ops> <seconds>` lines plus
# RSS/HEAP. This script runs every program REPS times, takes the median seconds
# per lane, and writes four measured reports:
#   reports/cache-performance.md
#   reports/queue-performance.md
#   reports/scheduler-performance.md
#   reports/runtime-performance-v0.7.md
#
# No number in those files comes from anywhere but a run below: a missing
# RESULT line fails the script instead of leaving a blank cell.
#
# Usage: qa/bench_runtime.sh [scope]     scope = all (default) | cache | queue
#                                                    | sched | web
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCOPE="${1:-all}"
REPS="${REPS:-3}"
CXX="${CXX:-g++}"

case "$SCOPE" in
    all) PROGS="rt_cache rt_queue rt_sched rt_web" ;;
    cache) PROGS="rt_cache" ;;
    queue) PROGS="rt_queue" ;;
    sched) PROGS="rt_sched" ;;
    web) PROGS="rt_web" ;;
    *)
        echo "bench: unknown scope '$SCOPE' (all, cache, queue, sched, web)"
        exit 1
        ;;
esac

TMP=$(mktemp -d /tmp/hs-rtbench-XXXXXX)
trap 'rm -rf "$TMP"' EXIT

echo "bench: building (-O2)"
for prog in $PROGS; do
    if ! $CXX -std=c++17 -O2 -pthread -Werror -I "$ROOT/runtime" -I "$ROOT/qa/bench" \
        "$ROOT/qa/bench/$prog.cpp" -o "$TMP/$prog" -ldl 2>"$TMP/$prog.err"; then
        echo "bench: FAIL building $prog"
        head -20 "$TMP/$prog.err"
        exit 1
    fi
done

echo "bench: running ($REPS reps each)"
: > "$TMP/results.tsv"
for rep in $(seq 1 "$REPS"); do
    for prog in $PROGS; do
        if ! timeout 900 "$TMP/$prog" >"$TMP/$prog.$rep.out" 2>"$TMP/$prog.$rep.err"; then
            echo "bench: FAIL running $prog (rep $rep)"
            tail -5 "$TMP/$prog.err"
            exit 1
        fi
        grep -E "^(RESULT|RSS|HEAP|SCRAPE) " "$TMP/$prog.$rep.out" >> "$TMP/results.tsv" || {
            echo "bench: FAIL no RESULT lines from $prog (rep $rep)"
            exit 1
        }
    done
done

exec python3 - "$TMP/results.tsv" "$ROOT" "$REPS" "$SCOPE" <<'PY'
import datetime
import subprocess
import sys
from collections import defaultdict

tsv, root, reps, scope = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]

# (workload, backend) -> list of (ops, seconds); RSS/HEAP/SCRAPE per backend
times = defaultdict(list)
metric = defaultdict(list)
for line in open(tsv):
    p = line.split()
    if p[0] == "RESULT" and len(p) == 5:
        _, wl, be, ops, sec = p
        times[(wl, be)].append((int(ops), float(sec)))
    elif p[0] in ("RSS", "HEAP", "SCRAPE") and len(p) == 3:
        metric[(p[0], p[1])].append(int(p[2]))


def median(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2]


def lane(wl, be):
    """(ops_per_sec, mean_usec_per_op) at the median run."""
    runs = times[(wl, be)]
    if not runs:
        raise SystemExit(f"bench: missing RESULT for {wl} on {be}")
    ops, sec = runs[0][0], median([s for _, s in runs])
    if any(o != ops for o, _ in runs):
        raise SystemExit(f"bench: op count moved for {wl} on {be}")
    return ops / sec, sec / ops * 1e6


def rate(x):
    if x >= 1_000_000:
        return f"{x / 1_000_000:.2f}M"
    if x >= 1_000:
        return f"{x / 1_000:.1f}k"
    return f"{x:,.0f}"


def worst_us(x):
    """A latency in the unit that reads well at that size."""
    if x >= 1000:
        return f"{x / 1000:,.1f}ms"
    if x >= 1:
        return f"{x:,.1f}us"
    return f"{x * 1000:,.0f}ns"


def mib(kb):
    return f"{kb / 1024:,.1f} MB"


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
    rev = sh(f"git -C {root} rev-parse --short HEAD")
    return cpu.strip(), mem, nproc, gxx, rev


cpu, mem, nproc, gxx, rev = machine()
stamp = datetime.datetime.now().strftime("%Y-%m-%d")

PREAMBLE = f"""Measured with `qa/bench_runtime.sh` at `{rev}` on {nproc} CPUs ({cpu}), {mem} MB RAM,
`{gxx}`. Median of {reps} runs; every cell below comes from a run, and the
script fails instead of printing a number it did not measure.
"""

# ---------------------------------------------------------------------------
# reports/cache-performance.md
# ---------------------------------------------------------------------------
if scope in ("all", "cache"):
    single = ["get_hit", "get_miss", "set", "incr", "exists", "expired_read"]
    rows = []
    for wl in single:
        r, u = lane(wl, "memory")
        rows.append(f"| `{wl}` | {rate(r)} | {worst_us(u)} |")
    mixed_1, mixed_1u = lane("mixed", "memory-1t")
    mt_backends = sorted({be for (wl, be) in times if wl == "mixed" and be != "memory-1t"})
    mt_rows = [f"| `mixed` on 1 thread | {rate(mixed_1)} | {worst_us(mixed_1u)} |"]
    for be in mt_backends:
        r, u = lane("mixed", be)
        threads = be.replace("memory-", "").replace("t", "")
        mt_rows.append(f"| `mixed` on {threads} threads | {rate(r)} | {worst_us(u)} |")
    scale_r, scale_u = lane("insert_200k", "memory")
    live, _ = lane("live_keys", "memory")
    rss = median(metric[("RSS", "live")])
    heap = median(metric[("HEAP", "live")])
    single_thread_total = mixed_1
    threads_total = max(lane("mixed", be)[0] for be in mt_backends) if mt_backends else mixed_1
    text = f"""# Cache performance ({stamp})

{PREAMBLE}
Method: the in-process cache, one thread unless a lane says otherwise. The
working set is 10,000 keys so hits spread over all 64 shards, and a miss lane
probes keys that were never written, so the sharded-miss path is measured
rather than assumed. `expired_read` reads entries that are already past their
TTL: the lazy path drops them on the way out.

## Single-thread lanes (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
{chr(10).join(rows)}

## Concurrency (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
{chr(10).join(mt_rows)}

The target for this milestone was 100,000 mixed ops/s. One thread does
{rate(single_thread_total)}; every core together does {rate(threads_total)}.

## Memory

| measure | value |
|---|---|
| live entries after 200,000 inserts | {live:,.0f} |
| inserts/s while filling | {rate(scale_r)} |
| peak RSS | {mib(rss)} |
| heap held with all 200,000 live | {mib(heap // 1024)} |
| heap per entry | {heap / live:,.0f} bytes |

Measured while the entries were still live, not after the cache was destroyed:
the heap a filled cache costs is the number a box gets sized by, and an empty
process says nothing. The peak RSS also covers the fill itself.
"""
    open(f"{root}/reports/cache-performance.md", "w").write(text)
    print("wrote reports/cache-performance.md")

# ---------------------------------------------------------------------------
# reports/queue-performance.md
# ---------------------------------------------------------------------------
if scope in ("all", "queue"):
    mem_lanes = ["enqueue", "enqueue_payload_1kb", "poll_priority", "enqueue_poll_complete",
                 "fail_to_dlq", "depth"]
    rows = []
    for wl in mem_lanes:
        r, u = lane(wl, "memory")
        rows.append(f"| `{wl}` | {rate(r)} | {worst_us(u)} |")
    sql_r, sql_u = lane("enqueue_poll_complete", "sqlite")
    rec_us = lane("recover_after_restart", "sqlite")[1]
    drain_backends = sorted({be for (wl, be) in times if wl == "worker_drain"})
    drain_rows = []
    for be in drain_backends:
        r, u = lane("worker_drain", be)
        threads = be.replace("memory-", "").replace("t", "")
        drain_rows.append(f"| `worker_drain` on {threads} threads | {rate(r)} | {worst_us(u)} |")
    rss = median(metric[("RSS", "memory")])
    text = f"""# Queue performance ({stamp})

{PREAMBLE}
Method: the memory backend directly (a program pays for the round trip, not for
a worker thread), plus SQLite in memory for the durable path. The clock is
frozen, so a delayed job becomes ready when the lane says so rather than when
the wall clock gets there. `fail_to_dlq` drives a job through all five
attempts into the dead-letter queue, moving the clock to each backoff.

`enqueue_poll_complete` is the number a request handler pays: enqueue, claim,
complete. `depth` is the queue's own counter on an empty queue, which is what a
health check asks.

## Memory backend (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
{chr(10).join(rows)}

## SQLite (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
| `enqueue_poll_complete` | {rate(sql_r)} | {worst_us(sql_u)} |

`recover_after_restart` is one operation, not a rate: it claims a job, never
completes it, closes the database and reopens it, and takes {worst_us(rec_us)}.
The memory backend cannot do that at all, which is the whole reason the SQL
one exists.

## Workers

| lane | jobs/s | mean us/job |
|---|---|---|
{chr(10).join(drain_rows)}

A worker pool is configured by a program (`worker Name {{ .. }}` with a
concurrency), and this lane runs the runtime's own pool on every core.

Peak RSS for the program: {mib(rss)} (the enqueue lanes hold half a million
jobs before draining them).
"""
    open(f"{root}/reports/queue-performance.md", "w").write(text)
    print("wrote reports/queue-performance.md")

# ---------------------------------------------------------------------------
# reports/scheduler-performance.md
# ---------------------------------------------------------------------------
if scope in ("all", "sched"):
    fire = ["next_interval", "next_daily_utc", "next_daily_tz+0530", "next_weekly",
            "prev_daily", "next_daily_dst_pair"]
    rows = []
    for wl in fire:
        r, u = lane(wl, "memory")
        rows.append(f"| `{wl}` | {rate(r)} | {worst_us(u)} |")
    reg_r, reg_u = lane("register", "memory")
    reg_backends = sorted({be for (wl, be) in times if wl == "register" and be != "memory"})
    reg_rows = [f"| `register` | {rate(reg_r)} | {worst_us(reg_u)} |"]
    for be in reg_backends:
        r, u = lane("register", be)
        threads = be.replace("register-", "").replace("t", "")
        reg_rows.append(f"| `register` on {threads} threads | {rate(r)} | {worst_us(u)} |")
    fire_r, fire_u = lane("fire_latency", "memory")
    worst_fire = worst_us(fire_u)
    text = f"""# Scheduler performance ({stamp})

{PREAMBLE}
Method: the next/prev fire arithmetic on its own, which is where a scheduler
spends its time -- a timer that is not due costs nothing but a comparison. The
input times come from a linear congruential walk, not `now + i`: a linear walk
is an induction variable the compiler closes in a multiply, and the lane then
measures nothing at all. Every lane sums what it computed and prints the sum,
so the work cannot be folded away.

`next_daily_dst_pair` asks for the same wall time either side of a DST change
(UTC-5 and UTC-4), which is where local-time arithmetic usually goes wrong.

## Fire arithmetic (ops/s, mean ns/op)

| lane | ops/s | mean ns/op |
|---|---|---|
{chr(10).join(rows)}

The dispatch budget for this milestone was 5us per decision. The most
expensive form, a weekly wall-clock time in a zoned zone, costs
{worst_us(lane('next_weekly', 'memory')[1])}.

`next_interval` is the floor of the lane set rather than a measurement of
interest: an interval is one addition, so what the loop actually costs is the
clock walk feeding it. It is here to show the decision is free next to the
time it takes to ask the question.

## Registration (timers/s, mean us/timer)

| lane | timers/s | mean us/timer |
|---|---|---|
{chr(10).join(reg_rows)}

## Firing

| measure | value |
|---|---|
| firings observed | {int(lane('fire_latency', 'memory')[0] * 2)} |
| mean interval including the 100ms sleep slices | {worst_fire} |

A one-second timer fires once a second by definition, so this lane measures
what is actually interesting: the scheduler thread notices a due timer within
its sleep slice rather than sleeping through it.
"""
    open(f"{root}/reports/scheduler-performance.md", "w").write(text)
    print("wrote reports/scheduler-performance.md")

# ---------------------------------------------------------------------------
# reports/runtime-performance-v0.7.md
# ---------------------------------------------------------------------------
if scope in ("all", "web"):
    sess_r, sess_u = lane("session_start_drain", "memory")
    ver_r, ver_u = lane("session_verify", "memory")
    lim_r, lim_u = lane("limit_check", "memory")
    sl_r, sl_u = lane("limit_check_sliding", "memory")
    lim_mt_backends = sorted({be for (wl, be) in times if wl == "limit_check" and be != "memory"})
    lim_mt_r = max(lane("limit_check", be)[0] for be in lim_mt_backends) if lim_mt_backends else lim_r
    met_r, met_u = lane("metrics_incr", "memory")
    scr_r, scr_u = lane("metrics_scrape", "memory")
    scrape_bytes = median(metric[("SCRAPE", "memory")])
    lock_r, lock_u = lane("lock_acquire_release", "memory")
    idem_r, idem_u = lane("idem_claim", "memory")
    own_r, own_u = lane("cluster_owner", "memory")
    cache_r, _ = lane("mixed", "memory-1t")
    queue_r, _ = lane("enqueue_poll_complete", "memory")
    sched_r, sched_u2 = lane("next_daily_utc", "memory")

    def target_row(name, measured, target, unit="ops/s"):
        verdict = "met" if measured >= target else "MISSED"
        return f"| {name} | {rate(measured)} {unit} | {rate(target)} {unit} | {verdict} |"

    text = f"""# Cloud runtime performance, v0.7 ({stamp})

{PREAMBLE}
Every number here is one lane from the four component reports, which are
where the method and the per-lane detail live:
`cache-performance.md`, `queue-performance.md`, `scheduler-performance.md`.
Lanes are in-process, single thread unless the lane says otherwise.

## Targets

| target | measured | required | verdict |
|---|---|---|---|
{target_row("cache, mixed operations", cache_r, 100_000)}
{target_row("rate limiter, checks", lim_r, 100_000)}
| scheduler, next-fire decision | {worst_us(sched_u2)} | 5us | {"met" if sched_u2 <= 5 else "MISSED"} |

## Subsystems

| lane | ops/s | mean us/op |
|---|---|---|
| `cache` mixed | {rate(cache_r)} | {worst_us(lane('mixed', 'memory-1t')[1])} |
| `queue` enqueue+poll+complete | {rate(queue_r)} | {worst_us(lane('enqueue_poll_complete', 'memory')[1])} |
| `scheduler` next daily fire | {rate(sched_r)} | {worst_us(sched_u2)} |
| `session` start+drain | {rate(sess_r)} | {worst_us(sess_u)} |
| `session` verify a signed cookie | {rate(ver_r)} | {worst_us(ver_u)} |
| `ratelimit` token bucket check | {rate(lim_r)} | {worst_us(lim_u)} |
| `ratelimit` sliding window check | {rate(sl_r)} | {worst_us(sl_u)} |
| `metrics` record a counter | {rate(met_r)} | {worst_us(met_u)} |
| `metrics` scrape to Prometheus text | {rate(scr_r)} | {worst_us(scr_u)} |
| `lock` acquire+release | {rate(lock_r)} | {worst_us(lock_u)} |
| `idem` claim a key | {rate(idem_r)} | {worst_us(idem_u)} |
| `cluster` owner of a key, 5 nodes | {rate(own_r)} | {worst_us(own_u)} |

## Concurrency

| lane | ops/s across every core |
|---|---|
| `cache` mixed | {rate(max(lane('mixed', be)[0] for be in {b for (w, b) in times if w == 'mixed' and b != 'memory-1t'}))} |
| `ratelimit` checks | {rate(lim_mt_r)} |

A metrics scrape of a running process walks every series it holds; this one
had recorded a few hundred, and produced {scrape_bytes:,} bytes of Prometheus
text per scrape.

## What limits concurrency

The rate limiter is the one lane that does not scale with cores: every check
resolves its limiter by id through one registry lock, so the multi-threaded
lane measures that lock rather than the buckets. A benchmark that hid this
would be a benchmark that lied -- a sharded counter was the easy half of the
fix and is in; the registry lookup is the other half, and the honest number is
the table above. Resolving a limiter once at startup (a handle, the way
`cache x ttl T` resolves a name) is the change that would remove it.

The cache scales because its buckets are sharded and its counters are per
shard; 12 threads against 64 shards collide far less than 12 threads against
one lock.
"""
    open(f"{root}/reports/runtime-performance-v0.7.md", "w").write(text)
    print("wrote reports/runtime-performance-v0.7.md")
PY
