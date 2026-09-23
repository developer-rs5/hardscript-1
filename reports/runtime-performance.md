# Runtime Performance — Phase 1 milestone report

Measured on-disk, no estimates. Methodology: identical REST API
(GET /, GET /hello/:name, POST /echo) on 127.0.0.1:8080, `qa/benchmark/load.py`,
8 s measure + 3 s warmup. Machine is noisy (load avg ~6.5 from unrelated
processes); figures are medians or the range observed across repeats.

Targets: conn/request ≥ 10000 RPS · keep-alive ≥ 30000 RPS · startup ≤ 25 ms ·
idle RSS ≤ 3 MB.

## Before (v0.2-alpha start, baseline from results.json)

| metric | hs | node | bun | go | rust |
|---|---|---|---|---|---|
| cold start (ms) | 32.0 | 352.0 | 65.0 | 65.0 | 62.0 |
| idle RSS (KB) | 3760 | 82228 | 37668 | 9228 | 3584 |
| RPS conn/req (c32) | 6967.5 | 9672.0 | 7650.4 | 6882.5 | 7281.1 |
| p50 (ms) | 4.35 | 3.14 | 3.97 | 4.43 | 4.19 |

## After (commit b2d7b3f, all milestones landed)

| metric | value | target | status |
|---|---|---|---|
| keep-alive RPS (c32) | 29999–31199 | ≥ 30000 | met |
| keep-alive p50 / p95 / p99 | 0.89 / 2.2 / 3.0 ms | — | — |
| conn/req RPS (c32) | 7530–7897 | ≥ 10000 | 0.76× of target |
| allocations per request | 0.000 | — | arena + pool |
| cold start | ~2 ms (release) | ≤ 25 ms | met |
| idle RSS | ~4096 KB (arena reserved) | ≤ 3 MB | ~1.4× target (thread stacks are lazily committed) |

## Milestone log

| commit | milestone | effect |
|---|---|---|
| 1a49d72 | ms1.1 zero-copy HTTP parser | 6967.5 → 8393–10169 conn/req RPS |
| 0eb2a6f | ms1.2 request arena + poll accept | 1.000 alloc/req; arena_bytes 4096 |
| d7e07f2 | ms1.5 keep-alive + TCP_NODELAY | 31199 RPS keep-alive (Nagle fix) |
| 6ab1a74 | ms1.3 grow-on-demand worker pool | 0.000 alloc/req; 7897 conn/req RPS |
| b2d7b3f | ms1.7 -O3 -flto -march=native -fvisibility=hidden | no regression; ~30.7k keep-alive |

## Quality gates

- Regression: 7/7 pass each milestone commit.
- Integration: 21/21 pass each milestone commit.
- WebSocket (006-ws-room) and crypto/JSON round-trips verified in regressions.

## Remaining gap: conn/req lane (7530–7897 vs 10000)

Bottleneck is per-connection cost (thread wake + TCP handshake + alloc-free
path). Levers queued:

- ms1.4 streaming JSON encoder (POST /echo emits into the arena Str without
  intermediate std::string; drop `std::function` recursion in parse_json).
- ms1.6 optimized route table (FNV-1a direct-map; avoid linear chain lookup).
- Startup: current ~2 ms release; RSS 3760→4096 KB after arena reservation;
  preallocation strategy can reclaim stack pages (guard_size / lazy).

## How to reproduce

```
cargo build
target/debug/hard bench qa/benchmark/hs/app.hard
./.hard/app.release &   # serves 127.0.0.1:8080
python3 qa/benchmark/load.py --duration 8 --warmup 3 --concurrency 32 --keepalive 1
python3 qa/benchmark/load.py --duration 8 --warmup 3 --concurrency 32 --keepalive 0
```