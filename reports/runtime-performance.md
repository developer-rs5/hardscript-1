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

## M1.4 measured (streaming JSON encoder, commit *next*)

| lane | value | note |
|---|---|---|
| keep-alive mixed (c32) | 39554–39766 RPS (0 err) | prior ~30k |
| conn/req mixed (c32) | 9565–9719 RPS (0 err) | prior 7530–7897 |
| echo-only keep-alive | 58170 RPS | 18-byte payload |
| echo-only 1 KB payload | 53634 RPS | response ~1.04 KB JSON, 0 err after warmup blip |
| echo-only 10 KB payload | 35935 RPS | response ~10.0 KB JSON, 0 err |
| echo-only 100 KB payload | 5386 RPS | response ~100 KB JSON, 0 err, CL exact (10017 = body) |
| JSON correctness | regression 008 | escapes, int64, float, nesting — exact output |

## Milestone log

| commit | milestone | effect |
|---|---|---|
| 1a49d72 | ms1.1 zero-copy HTTP parser | 6967.5 → 8393–10169 conn/req RPS |
| 0eb2a6f | ms1.2 request arena + poll accept | 1.000 alloc/req; arena_bytes 4096 |
| d7e07f2 | ms1.5 keep-alive + TCP_NODELAY | 31199 RPS keep-alive (Nagle fix) |
| 6ab1a74 | ms1.3 grow-on-demand worker pool | 0.000 alloc/req; 7897 conn/req RPS |
| b2d7b3f | ms1.7 -O3 -flto -march=native -fvisibility=hidden | no regression; ~30.7k keep-alive |
| c8d265b | ms1.8 phase-1 runtime report | baseline + milestone log written |
| 844db3e | ms1.4 streaming JSON encoder | JSON bodies stream into the arena Str (single send, no temp std::string / ostringstream / per-byte snprintf); keep-alive ~30k → 39.7k RPS |
| 04d40de | ms1.6 trie-based router | static O(1) (23 ns at 5k routes), param/wild O(depth), wildcard support, 0 alloc/100k dispatch; linear matcher was 40–85 µs at 5k routes |
| 7371fbd | ms1.9 lazy pool + memory | idle RSS 4.20 → 3.86 MB (1 thread at boot), pool shrink to floor after 5 s idle, big buffers return to OS on worker exit; fixed Arena::Str use-after-free on large keep-alive bodies |
| *next* | ms2.0 value engine foundation | additive `Value` (tagged union, SSO ≤ 14, inline 4-slot arrays, non-owning views) + immutable `ConstPool`; `http_reason` strings now intern once. Not on the request path — `hs::Val` stays the default. No perf delta expected; see reports/value-foundation.md |
| *next* | stress lab (ms2.0) | flat to 10k routes (30–38k RPS keep-alive @ c512, 0 err); concurrency up to 1024 no collapse; 1 MB echo 0 err; WS: 5000 clients hold + join, 3.4k msg/s broadcast round-trips @p50 0.68 ms, 0 err. See reports/http-stress.md, reports/websocket-performance.md |

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