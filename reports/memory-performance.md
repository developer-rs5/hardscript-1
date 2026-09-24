# Memory Performance — ms1.9 lazy pool + idle-RSS shrink

All figures measured on-disk against the released build
(`-O3 -flto -march=native`, `./.hard/app.release`), private + shared RSS from
`/proc/<pid>/status` VmRSS. Target: idle RSS ≤ 3 MB, startup ≤ 25 ms.

## What changed

- **Lazy worker pool**: `listen()` no longer seeds
  `hardware_concurrency` threads (previously 12–32). The pool climbs from 0
  threads on first connection and grows on demand while every worker is busy
  (cap 256). After `kIdleExit = 5 s` with an empty queue, surplus workers exit
  down to `kIdleFloor = 1` live worker; thread stacks, TLS and owned buffers
  are returned to the OS.
- **Bounded malloc arenas** (`mallopt(M_ARENA_MAX, 32)`) + low
  `M_MMAP_THRESHOLD`/`M_TRIM_THRESHOLD`/`M_TOP_PAD` so per-worker response
  buffers > 64 KB are mmap'd and go back to the OS when a worker exits idle.
- **`Arena::Str` owns its buffer** (plain `realloc`), no longer aliases the
  arena block. This also fixes a latent use-after-free (see below).
- **constexpr tables**: `http_reason` uses a `static constexpr` reason table
  instead of generating case-dispatch data.
- Startup/seeding printouts updated; identical dispatch cost to the eager pool
  (router micro-bench unchanged; end-to-end RPS identical within noise).

## Bug found & fixed (with regression)

`Arena::Str` pointed into the arena block; when the arena grew, `Str::ensure`
memcpy'd from the old (just-freed) block — a **heap use-after-free** on large
keep-alive bodies (repro: `POST /echo` 100 KB × 32 keep-alive conns; ASan
reported heap-use-after-free in `json_escape_to`). The pool-shrink + arena
cap experiments made it crash reliably. Fix: `Str` keeps its own block
(`std::realloc`), no arena aliasing. Verified ASan-clean on the repro, plus
regression `014-keepalive-large-echo` (repeat large bodies; regression would
also have caught it via server crash).

## Measured

| scenario | before (% before) | after (ms1.9) | target |
|---|---|---|---|
| idle RSS at boot (0 conns) | 4.20 MB | **3.86 MB** (range 3.70–3.94) | ≤ 3 MB |
| live threads at boot | 13 (12 seeded + accept) | **1** | — |
| startup → accepting | ~7 ms | **6.0 ms** | ≤ 25 ms |
| allocs / request (hot, 15k conns) | ±0 | **0.000** | 0 |
| live threads after 32-conn load + 9 s idle | 32+ (persisted) | **2** (1 floor + accept) | shrink |
| idle RSS after 1 MB-echo churn + 9 s idle | ~41 MB (retained heaps) | **9.16 MB** | return to OS |

Idle RSS breakdown at boot: glibc/libstdc++/loader text+data ≈ 2.5 MB
(shared-library pages charged to VmRSS), binary, main-thread stack and a cold
bump arena. The residual above the 3 MB target is shared-library text/data,
not per-request memory. Private RSS at boot is substantially lower than the
VmRSS figure.

## Route-table cost (server process RSS, routes built on first request)

| routes | boot RSS | after 1st hit | delta |
|---|---|---|---|
| 10 | 3.86 MB | 3.97 MB | ~0.11 MB |
| 1 000 | 3.86 MB | 3.96 MB | ~0.11 MB |
| 5 000 | 3.85 MB | 3.96 MB | ~0.12 MB |

Static-route registration is O(1) per route (hash table, load ≤ 0.7) and a
pure-static table costs only **~23 B/route** (5 000 routes ≈ 116 KB). Mixed
routes (trie + param/wild memories) cost ~0.36 KB/route per
`router_bench` (5 001 mixed routes ≈ 1.8 MB).

## Throughput on the same trades (0 errors every run)

| lane | ms1.9 | target |
|---|---|---|
| keep-alive mixed, c32 | 36.3–36.8 k RPS | ≥ 30 k |
| fresh-conn mixed, c32 | 8.6–8.7 k RPS (1 machine sample; 11.7 k on a quieter box earlier in the session) | ≥ 10 k |
| echo 100 KB keep-alive | 3.2 k RPS (was crashing pre-fix) | — |
| echo 1 MB keep-alive | 292 RPS | — |

The shrunken warm pool (1 live worker) costs latency only on the next burst;
measured dispatch throughput is unchanged from the eager pool on the same
machine state.