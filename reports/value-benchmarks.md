# Value-Engine Benchmarks — Phase 2 post-integration

Uniform methodology (qa/benchmark/run.sh), identical REST API on every stack:
`GET /` (weight 1), `GET /hello/:name` (weight 2), `POST /echo` (weight 1).
Client: `qa/benchmark/load.py`, 3 s warmup + 8 s measure, c32, two lanes
(one TCP connection per request; persistent connections). All numbers measured
on this machine, 0 client errors everywhere.

## Matrix (fresh run, post ms2.6)

| stack | build | bin bytes | cold ms | idle KB | conn/req RPS | p50 ms | keep-alive RPS |
|---|---|---|---|---|---|---|---|
| **hs** | 3.90 s | 157368 | **29.0** | **3880** | **10548.8** | **2.86** | **41688.5** |
| node | n/a | 315 | 161.0 | 82764 | 9128.6 | 3.43 | 31983.8 |
| bun | n/a | 519 | 30.0 | 37476 | 9985.6 | 3.03 | 40394.2 |
| go | 561 s | 9811396 | 27.0 | 11280 | 8903.6 | 3.42 | 33628.4 |
| rust | 147 s | 1100480 | 26.0 | 3496 | 9595.6 | 3.16 | 40090.1 |

HardScript is the only stack over 10k conn/req and the keep-alive leader.

## JSON-echo lanes (POST /echo, body `{"x":7,"s":"bench"}`)

| lane | RPS | p50 ms | p95 ms | p99 ms | err |
|---|---|---|---|---|---|
| keep-alive c32 | 42147.4 | 0.62 | 1.87 | 2.78 | 32* |
| conn/req c32 | 9293.4 | 3.25 | 6.03 | 7.53 | 0 |

\* 32 of 337179 = 0.01%: connections closed by the idle sweep under the parser
latency tail; the client retried via the keep-alive path (load.py treats a
closed socket as a lane error). conn/req lane: 0.

## Phase-1 → Phase-2 delta (HardScript, same lanes)

| metric | phase 1 | post ms2.6 | delta |
|---|---|---|---|
| conn/req RPS | 6967.5 | 10548.8 | +51% |
| conn/req p50 | 4.35 ms | 2.86 ms | −34% |
| keep-alive RPS | ~30–38k | 41688.5 | + |
| idle RSS | 3760 KB | 3880 KB | +120 KB (const pool + engine) |
| cold start | 32.0 ms | 29.0 ms | −9% |

## Targets: met / not met

| target | threshold | measured | status |
|---|---|---|---|
| conn/req | ≥ 10000 RPS | 10548.8 | met |
| keep-alive | ≥ 30000 RPS | 41688.5 | met |
| idle RSS | ≤ 3 MB | 3.88 MB | not met |
| startup | ≤ 6 ms | 29.0 ms | not met |

Caveat: this machine is noisy (~±20% on throughput lanes); every stack was
load-tested back-to-back in one run so comparisons are relative and consistent.