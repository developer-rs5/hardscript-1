# Value-Engine Foundation — ms2.0 report

Additive compatibility layer for the Phase-2 `hs::Val` replacement. Nothing
here replaces the existing runtime value type; `hs::Val` remains the default
for all generated code. The new `hs::Value` engine, its JSON bridge, and the
immutable `hs::ConstPool` exist to (a) be measurable now and (b) give the
Phase-2 rewrite a reference layout for escape- and allocation-analysis.

Scope control: no language-syntax, compiler, or semantics changes. All gates
(regressions, integration, phase-3 snapshot, fmt, docs) pass with this code
present but dormant.

## What was added (all in `runtime/hs_runtime_value.hpp`)

- `enum ValueKind` + `class Value` — tagged union, 24 bytes:
  - 14-byte SSO for short strings (0 heap for ≤ 14 chars)
  - non-owning `str_view`/`bytes` views pointing into request buffers or the const pool
  - owned long strings (std::string)
  - `ValueVec`/`ValueObj` box behind a pointer → recursion-safe, copyable at any depth
  - `small_[4]` inline slots absorb the common case; a 5th element migrates
    inline slots into an enclosed std::vector (first-spill realloc chain bounded)
  - explicit copy / move / dtor; copy of an array/object deep-copies the box
- `ConstPool` — append-only string interning with stable addresses:
  - build-time `intern()` dedups via a small hash map
  - steady-state reads lock-free (interned `const char*` never moves)
  - `freeze()` clears the map; ids/pointers stay valid after shutdown of writes
- `value_size` + allocation-free `value_to_json<Sink>` (reuses the existing
  escape / int / double emitters) + `value_from_val` / `val_from_value` bridge
  that round-trips every `Val` kind including nested containers
- wired `http_reason` in `hs_runtime_http.hpp` through `const_pool().intern(...)`
  so reason strings exist exactly once in memory at boot

## Measured (qa/benchmark/micro/value_probe.cpp, operator-new counter)

Compiler: g++-15, -O2. Layouts and allocation counts (post-warmup, so glibc
bootstrap allocations are excluded):

| case | allocs | bytes | meaning |
|---|---|---|---|
| `str("…")` ≤ 14 chars | 0 | 0 | SSO — short strings never touch the heap |
| `str("…")` 15 chars | 1 | 32 | one std::string buffer |
| array of 4 scalars | 1 | 128 | one `ValueVec` box, zero per element |
| array of 9 scalars | 6 | 872 | box + enclosed-vector growth chain (≈4 reallocs) |
| one `intern()` | 6 | 209 | boot-time only: store + 3 containers growing |
| 1000 identical `intern()` | 0 | 0 | dedup: steady state returns the stored pointer |

Layouts: `sizeof(Value)=24`, `sizeof(ValueVec)=128`, `sizeof(ValueObj)=256`.

## Escape analysis (analysis only — no optimization enabled)

- Short strings already escape the heap by construction (SSO), so a Phase-2
  value scheme can treat ≤ 14-byte strings as registers without an allocation.
- Arrays/objects escape only through the single `ValueVec`/`ValueObj` box; with
  ≤ 4 members the box is the ONLY allocation. A compile-time kSmall=4 target
  means request body sizes stay allocation-free for typical JSON documents.
- After the first `intern()` of a given string, the pool is a read-only
  pointer-with-a-hashmap; on the request path `intern` for already-pooled
  strings performs zero allocations (verified 1000/1000).
- `http_reason` hot-path effect: unchanged — every status string is interned
  once at first use; later calls are map hits returning the same pointer.

## Files

- `runtime/hs_runtime_value.hpp` — engine, pool, bridge (additive block)
- `runtime/hs_runtime_http.hpp` — `http_reason` → pool
- `tests/run-regressions.sh` — new `cpp` fixture kind (compiles a fixture
  against the runtime headers, runs it, diffs stdout)
- `tests/regression/015-value-kinds.{cpp,exp}` — kinds + JSON + sizes
- `tests/regression/016-value-sso-wellbox.{cpp,exp}` — SSO edges, inline slots, spill, copy/move
- `tests/regression/017-value-const-pool.{cpp,exp}` — intern dedup, freeze-stability, v_str
- `tests/regression/018-value-bridge.{cpp,exp}` — Val ⇄ Value round-trip, http_reason
- `qa/benchmark/micro/value_probe.cpp` — measurement harness

## Bugs caught by the probe

- First `push` past `small_[4]` silently dropped the 5th element: the spill
  path pushed into the fallback vector but never migrated the inline slots
  (and `n_` stayed at 4), so `data()` kept reading `small_`. Fixed in
  `Value::push`/`Value::set` — first spill migrates slots into the vector;
  guarded by regression 016.
- Nothing else: generated-code output is unchanged (phase-3 snapshot gate is
  byte-identical), no public API changes, and `hs::Val` is untouched.