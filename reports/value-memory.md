# Value-Engine Memory Model — Phase 2 (ms2.1–ms2.6)

Measured model of what a value costs in the engine, on the request path, and
as a whole process. All quantities from the operator-new counter fixtures and
`VmRSS` probes; no estimates.

## Value layout (fixtures 023/027)

| unit | bytes | notes |
|---|---|---|
| `Value` tagged union | 32 | 4 B header + 24 B SSO payload (ms2.2) |
| `Value` SSO string ≤ 24 B | 0 alloc | inline |
| `Value` heap string ≥ 25 B | 2 allocs | 32 B `std::string` + `size+1` buffer |
| `ValueVec` box (array ≤ 4 inline elems) | 1 alloc, 160 B | 5th elem → fallback vector alloc |
| `ValueObj` box (object ≤ 4 inline pairs) | 1 alloc, 288 B | 5th pair → fallback vector alloc |

## Typical request body (fixture 036)

`{"a":12345,"b":"toot-toot horn!","c":[1,2]}` parses to exactly **2 allocs,
448 B**: one object box (288 B) + one array box (160 B). All strings and keys
inline; numbers are inline tagged values.

## Request-cache structure (fixture 037)

One cached engine `Value` per `Request` (lazy, memoized). Repeated
`json_value()` calls return a copy of the cache box (1 alloc) instead of a new
parse. Empty body is a `{}` object box with zero keys.

## Router/Dispatch (fixture 038)

| resource | cost |
|---|---|
| static-dispatch params | 0 allocs |
| 2-param dispatch params | 2 red-black tree nodes (~128 B) |
| const-pool interning during dispatch | 0 |

## Process memory (ms2.6, `VmRSS`)

| state | RSS |
|---|---|
| fresh boot, idle (post cold-start probe) | 3.88 MB |
| after sustained concurrency-32 heat | ~6.5 MB |

The warm idle figure grows because the worker pool is lazy and grow-on-demand:
workers (and their stacks/arenas) are only materialized under load. The
value-engine additions (universal const-pool table, engine boxes) add ~120 KB
to the fresh idle footprint over phase 1 (3.76 MB → 3.88 MB).