# Router Performance — ms1.6 trie-based dispatch

Measured on-disk with `qa/benchmark/micro/router_bench` (`-O3 -flto -march=native`,
released flags). Same binary reproduces the pre-ms1.6 linear matcher for a
direct before/after. Latency = nanoseconds per `Server::match`, best-of loop,
200k reps per cell. Machine noisy; these are hot-loop (unloaded) numbers.

## Design

- **Static fast path**: open-addressing table (FNV-1a, load ≤ 0.7, linear
  probing) keyed by method id + full path → **O(1)**. Method string is reduced
  to a `uint8_t` once (cached method lookup).
- **Trie**: literal edges (sorted vector + binary search), one `:param` child,
  one `*wildcard` child per node, terminals holding method-specific routes.
  Walk is **O(depth)**, bounded backtracking with precedence
  `literal > param > wildcard` (`/users/me` beats `/users/:id`).
- **Zero-copy params**: captured values are `string_view`s into the request
  path; names are views into the route pattern.
- **Zero heap allocation in dispatch**: tables built at registration time;
  `match()` performs no allocation (verified: 0 allocs / 100k dispatches).

## Lookup latency (ns) vs registered routes

| routes | static | param | wildcard | 404 miss | old static | old param | old miss | speedup (miss) |
|---|---|---|---|---|---|---|---|---|
| 12 | 28 | 69 | 80 | 50 | 123 | 162 | 223 | 4.5× |
| 99 | 24 | 73 | 82 | 52 | 810 | 916 | 1 959 | 38× |
| 501 | 24 | 79 | 90 | 81 | 4 357 | 4 959 | 8 948 | 111× |
| 999 | 23 | 97 | 97 | 67 | 8 288 | 8 855 | 18 344 | 273× |
| 5 001 | 23 | 87 | 97 | 69 | 39 730 | 43 420 | 85 447 | 1 238× |

Static lookup is constant (23–28 ns) from 12 to 5 001 routes; param and
wildcard stays in the low tens of ns. The old matcher's cost grew linearly
(reaching ~40–85 µs at 5 001 routes), so dispatch is now effectively
**flat** for pure-static traffic and ~0.4 µs at the extreme on dynamic paths.

## Allocation count

| workload | allocations / 100k dispatches |
|---|---|
| 100k mixed static+param+wildcard dispatches (steady state) | **0** |

## Equivalent lookup throughput

Flat static O(1): 23 ns ≈ **43 M matches/s** at 5 001 routes. Param hit:
87 ns ≈ 11.5 M matches/s; wildcard 97 ns ≈ 10.3 M matches/s.

## Memory

Route table growth (process RSS, includes ~3.8 MB baseline):

- 12 routes: 3 788 KB (static table at build time: ~0 KB extra)
- 999 routes: 4 384 KB
- 5 001 routes: 5 600 KB → ≈ 0.36 KB per route, ~1.8 MB for 5 k routes.

## New routing features (previously unsupported)

- `GET /files/*path` wildcard catch-all — zero-copy capture of the remainder.
- Route precedence: literal > param > wildcard; method-specific routes
  (`GET /users/me` vs `POST /users/me`) dispatch independently.
- Nested prefix groups (`/api/v1/orders/:oid/items`) share trie nodes.

## Functional verification

Regressions 009 (static), 010 (param), 011 (wildcard), 012 (priority),
013 (groups) all pass against a live server. The trie insert path had an
initial segfault from a dangling `RouterNode&` after `emplace_back` realloc
(code fixture 010/011 catch it).