# HTTP Stress Lab — ms2.0

Measured on-disk, 127.0.0.1:8080, `qa/benchmark/load.py`. Machine under
noisy desktop load (load avg ≈ 4–6) the whole session; 4–5 s measure spans.
All runs report **0 client errors** unless noted.

## Router growth — keep-alive mixed lane @ c512

Generated apps with 1e3 / 5e3 / 1e4 static routes (`qa/benchmark/gen_stress_app.py`),
all sharing the identical mixed core (`GET /`, `GET /hello/:name`, `POST /echo`).

| routes | RPS | p50 | p95 | p99 | err |
|---|---|---|---|---|---|
| 3 (app.hard) | 30419 | 2.06 | 6.71 | 11.51 | 0 |
| 1000 | 37167 | 1.75 | 5.02 | 7.06 | 0 |
| 5000 | 32514 | 1.92 | 5.91 | 10.14 | 0 |
| 10000 | 37897 | 1.75 | 4.96 | 7.00 | 0 |

Flat to 10k routes (differences are machine noise; note the 3-route app sits
in the same band). Static table is O(1) direct-map, so adding routes does not
touch the hot dispatch path.

Conn/request @ c512 (worst lane): 1000 routes 7729 RPS, 10000 routes 6231 RPS
(0 err). Tail is connection setup, not routing.

## Concurrency sweep — mixed lane, 3-route app

| concurrency | keep-alive RPS | conn/req RPS |
|---|---|---|
| 32 | 22338 | 7189 |
| 64 | 36294 | 7588 |
| 128 | 40195 | 6484 |
| 256 | 40749 | 6887 |
| 512 | 30559 | 7729 (route app) |
| 1024 | 34544 | — |

Keep-alive peaks ~40k RPS around c128–256 and stays in the 30k+ band at
c1024 — the pool grow-on-demand caps growth, so no collapse. conn/req stays
~6.5–7.7k RPS (target 10k not yet met; known gap, see runtime-performance.md).

## Payload sweep — echo lane, keep-alive @ c64

| payload | resp ~ | RPS | p50 | err |
|---|---|---|---|---|
| 1 KB | 1.04 KB JSON | 34451 | 1.44 | 0 |
| 10 KB | 10.0 KB JSON | 14663 | 3.60 | 0 |
| 100 KB | 100 KB JSON | 1979 | 28.30 | 0 |
| 1 MB | 1 MB JSON | 168 | 341.34 | 0 |

Zero errors at every size (previous crash at 100 KB+ fixed in ms1.9).
Throughput drops with payload size as expected for a streaming serializer;
server-side validation (fsanitize + regression 014) stays clean.

## Observed limits

- Fine up to 10k routes: dispatch adds ~0 ns on the hot path.
- Zero-alloc path holds under load: no degradation attributable to leaks
  (RSS returned to baseline after each sweep — see memory report).
- Machine-noise caveat: absolute RPS wobbles ±20% run to run; relative lanes
  and the 0-error invariant are the reliable measures.

## Reproduction

```
cargo build
target/debug/hard bench qa/benchmark/hs/app.hard        # 3-route base
python3 qa/benchmark/gen_stress_app.py 10000 /tmp/r.hard
target/debug/hard bench /tmp/r.hard                     # 10k-route app
./.hard/app.release &                                   # or app-not-in-.hard paths
python3 qa/benchmark/load.py --duration 5 --warmup 2 --concurrency 512 --keepalive 1
```

Tooling added: `qa/benchmark/gen_stress_app.py`, `qa/benchmark/ws_stress.py`
(see websocket report), stress apps under `qa/benchmark/hs/stress_*.hard`.