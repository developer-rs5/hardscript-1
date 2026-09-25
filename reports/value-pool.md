# Immutable Constant Pool — ms2.4

The `ConstPool` (ms2.0: strings only) is now a universal immutable pool: every
constant the engine can dedup at boot lands once and lives forever on a stable
address. The pooled set forms a compile-time table exposed as `table_json()`.

## Pooled kinds (dedup = single slot)

| kind | intern helper | storage |
|---|---|---|
| String | `intern_str` / `intern` (legacy char*) | owned NUL-terminated buffer + pooled `str_view` |
| Int | `intern_i64` | inline in the pooled `Value` |
| Float | `intern_f64` | inline in the pooled `Value` |
| Bool | `intern_bool` | inline in the pooled `Value` |
| empty Array | `intern_empty_array` | one `ValueVec` box |
| empty Object | `intern_empty_object` | one `ValueObj` box |
| non-empty containers / Bytes / Function | `intern_value` (deep copy) | no dedup |

`intern_value` dispatches by value kind onto the same slots, so callers can
pool generically without knowing the kind. Fingerprints are kind-tagged 64-bit
hashes (FNV-1a for strings, `mix(int64/double bits)`) in a
`unordered_map<uint64_t, size_t>`.

## Stable-address design

Pooled entries live in two `std::deque`s (`owned_` strings, `vals_` Values),
whose references are stable across later appends — the exact reason the ms2.0
`std::vector`-of-pointers was replaced. `freeze()` seals the pool; unlike
ms2.0 (which dropped the dedup map), the index is kept so post-freeze
re-interns still return the same slot and never duplicate.

## Measured (fixtures 030–032)

- Dedup: repeat intern of any pooled string/int/float/bool/empty container is
  zero allocations (100k repeats each: `allocs=0 bytes=0`), through both the
  typed helpers and `intern_value`.
- Non-empty containers are deep-copied, never shared (fixture 030).
- Table snapshot exact and byte-stable across `freeze()` (fixture 031):
  `["method","path",200,204,0.5,true,false,[],{},-7]`.
- Pool accounting: `count_items()` (all kinds), `size()` (legacy string
  count), `bytes()` (logical payload: `32+len` string content, container
  boxes), `table_json()`.
- The legacy `intern()` -> `const char*` contract (NUL-terminated, stable,
  dedup, order-preserving `cstr`/`get` ids) is unchanged: fixture 017 still
  passes verbatim.

## Notes

- Pooled strings are stored once and referenced by view, so `str_view` into
  the pool is the request-path allocation-free idiom (ms2.6 will use it for
  reason strings and header literals).
- `freeze()` keeps entries immutable; the pool is intended to be populated at
  boot (server warm-up) and sealed before the accept loop starts.