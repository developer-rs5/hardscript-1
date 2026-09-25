# Request-Path Value Engine — ms2.6

Goal (spec): the runtime request path carries the engine value. `hs::Val`
stays as a compatibility adapter — zero public API change. Measured: per-parse
allocations, body memoization, router params, throughput before/after wiring.

## Engine JSON parse (fixture 036)

`parse_json_value` (recursive-descent parser with a plain struct cursor, no
`std::function` heap) inside `hs_runtime_value.hpp`.

| input | allocs | bytes |
|---|---|---|
| `12345` (bare int) | 0 | 0 |
| `"toot-toot horn!"` (15 B, ≤ SSO) | 0 | 0 |
| `{"x":1}` (1-key object) | 1 | 288 (box) |
| `[1,2]` (2-elem array) | 1 | 160 (box) |
| `{"a":12345,"b":"toot-toot horn!","c":[1,2]}` | 2 | 448 (obj box 288 + arr box 160) |

Keys ≤ 15 B and string values ≤ 24 B ride inline storage; numeric scalars are
inline tagged values never allocated. Byte parity with the legacy `parse_json`
and with `val_from_value(parse_json_value(...)).to_json()` is asserted by the
fixture.

## Body memoization (fixture 037)

`Request::json_value()` parses the request body once, lazily, into a cached
engine `Value`; `Request::json()` is the legacy `Val` adapter over that cache.

| call | allocs | bytes | meaning |
|---|---|---|---|
| first `json_value()` (`{"x":1,"y":2}`) | 2 | 576 | parse obj box + returned copy box |
| repeated `json_value()` | 1 | 288 | copy of cached box — no re-parse |
| empty body `json()` / `json_value()` | — | — | `{}` object, matches legacy contract |

## Router params (fixture 038)

The `Request::params` map is now `std::map<std::string_view, std::string_view>`:
keys alias the route pattern (`process` lifetime), values alias the request
path (valid for the duration of dispatch; handlers copy with
`std::string(value)`). Static routes dispatch with zero allocations;
one-param routes cost exactly one red-black node (was a copied `std::string`
key + a copied value per param).

| route kind | allocs/dispatch |
|---|---|
| static (`/health`) | 0 |
| 2-param (`/users/:id/photos/:photo`) | 2 |
| const-pool interning during dispatch | 0 (delta) |

## Throughput after wiring (qa/benchmark, 8 s @ c32, mixed 1 root : 2 hello : 1 echo)

| lane | HardScript | node | bun | go | rust |
|---|---|---|---|---|---|
| conn/req RPS | **10548.8** | 9128.6 | 9985.6 | 8903.6 | 9595.6 |
| conn/req p50 | **2.86 ms** | 3.43 | 3.03 | 3.42 | 3.16 |
| keep-alive RPS | **41688.5** | 31983.8 | 40394.2 | 33628.4 | 40090.1 |
| keep-alive p50 | **0.64 ms** | — | — | — | — |
| errors | 0 | 0 | 0 | 0 | 0 |

The POST /echo lane carries a real JSON body (`{"x":7,"s":"bench"}`), so the
engine parse runs inside every echo request in the mix. Isolated JSON-echo
lanes: keep-alive **42147.4 RPS** (p50 0.62 ms), conn/req **9293.4 RPS**
(p50 3.25 ms), 0 client errors.

## Phase-2 targets (measured, post-integration)

| target | threshold | measured | status |
|---|---|---|---|
| conn/req | ≥ 10000 RPS | 10548.8 | met |
| keep-alive | ≥ 30000 RPS | 41688.5 | met |
| idle RSS | ≤ 3 MB | 3.88 MB fresh boot | not met (close) |
| startup | ≤ 6 ms | 29.0 ms median-of-5 | not met |

Machine is noisy (±20% on throughput lanes); the cross-stack comparisons and
the 0-error invariant are the reliable signal. Warmup lane intentional: the
reported `cold_ms` includes process spawn + readiness probe.