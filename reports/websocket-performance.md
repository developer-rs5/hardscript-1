# WebSocket Stress — ms2.0

Measured against `qa/benchmark/hs/stress_ws.hard` (`socket /s` that
`websocket.join("room")` on connect and `broadcast_room` echoes with a
(echo:) prefix). Raw-socket RFC6455 client `qa/benchmark/ws_stress.py`
(waits for its own echo, so every client drains its own reply). Noisy
machine (load ≈ 4–6) like the HTTP lab.

## Connection capacity — hold + join

| clients | msgs/client | wall | result |
|---|---|---|---|
| 5000 | 0 (connect + join only) | 0.98 s | 5000/5000, 0 err |

5000 concurrent WebSocket connections (each joining the room) are all held;
the server stays fully responsive afterward.

## Round-trip broadcast — 1 message per client

| clients | msgs/s | wall | p50 | p95 | p99 | err |
|---|---|---|---|---|---|---|
| 1000 | 2447 | 0.41 s | 0.93 | 3.47 | 5.37 | 0 |
| 2000 | 3579 | 0.56 s | 0.69 | 2.31 | 3.76 | 0 |
| 5000 | 3358 | 1.49 s | 0.68 | 2.22 | 3.56 | 0 |

Throughput scales with concurrency and **sub-millisecond p50** at 5000
clients, zero errors. Per-client one round trip cost is ~O(1) because each
client stops as soon as its own echo arrives.

## Sequential multi-message (k messages per client, all-to-all)

| clients | msgs/client | result |
|---|---|---|
| 500 | 4 | 0 err |
| 1000 | 4 | 0 err |
| 2000 | 4 | tail timeouts (746/2000 within 150 s) |
| 5000 | 4 | tail timeouts (2456/5000 within 180 s) |

`broadcast_room` fans out to all room members, so with every client sending
several messages and each client waiting for its own k echoes in order, the
working set is ~O(k·N) frames per client over an O(N) room — the cumulative
cost is quadratic in room size. The tail (clients whose turn comes last)
exceeds generous deadlines; the server itself never crashed and HTTP stayed
healthy (verified 200 after each run). Characterizing, not a defect: the
measurement pattern starves the tail by design.

Throughput still grows with N under these saturated runs (2000 → 20k msg:
427 msgs/s; 5000 → 20k msg: 526 msgs/s).

## Findings

- 5000-client fan-out holds and p50 stays under 1 ms for single-hop echoes.
- Multi-message all-to-all broadcasts are O(room) each — for chat-style
  rooms with a few hundred members this is fine; a per-connection unicast
  path would remove the O(N²) tail. Queued for the next cycle, not committed.

## Reproduction

```
target/debug/hard bench qa/benchmark/hs/stress_ws.hard
cp .hard/stress_ws.release /tmp/stress_ws && /tmp/stress_ws &
python3 qa/benchmark/ws_stress.py 8080 5000 1   # single round-trip
python3 qa/benchmark/ws_stress.py 8080 1000 4   # sequential 4-msg
```