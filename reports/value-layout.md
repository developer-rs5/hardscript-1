# Value Engine Layout — ms2.1 (tagged union)

Measured against `runtime/hs_runtime_value.hpp` with `g++ -O1 -std=c++17` on this
machine; identical numbers under `-O3 -flto -march=native`. Locked in by
`tests/regression/023-value-layout.cpp` (exact stdout, so any layout drift
fails the suite).

## Kind set

`enum class ValueKind : uint8_t { Nil, Bool, Int, Float, String, Array, Object, Function, Bytes };`

| kind | storage | heap |
|---|---|---|
| Nil | inline tag | never |
| Bool | inline `bool` | never |
| Int | inline `int64_t` | never |
| Float | inline `double` | never |
| String | `char[14]` SSO, `std::string*`, or non-owning view | long strings only |
| Array | `ValueVec` box (4 inline slots, spill→`std::vector`) | box + spill |
| Object | `ValueObj` box (4 inline slots, spill→`std::vector`) | box + spill |
| Function | `void*` fn ptr + `const char*` pooled name | none (name interned once in `ConstPool`) |
| Bytes | non-owning `{const char*, size_t}` view | never |

## Layout (measured)

```
sizeof(Value)  = 24
alignof(Value) = 8
sso            = 14
kinds          = 9
array_inline   = 4
object_inline  = 4
polymorphic    = 0      (no virtuals)
standard_layout = yes
```

`Value` is a plain tagged union — no `std::variant`, no `std::any`, no virtuals,
no RTTI. `value_layout_report()` exposes the one-line summary so a generated
report or the `hard` CLI can embed it without re-deriving the C++ type.

## Allocation behavior (measured in 019–022)

- 200 000 Int constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Bool constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Float constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Nil constructs/destroys → 0 allocations, 0 bytes.

## Function kind

`Value::fn(void*, name)` interns `name` into the `ConstPool` (stable address,
deduped), stores the pointer and pooled name inline. Copies share the same
pooled name. `Function` is not JSON-serializable: `value_to_json` renders
`null` (size 4) and the `Val` bridge returns `Val::nil()` so code paths that
iterate a mixed collection never observe a dangling payload.

## Design decisions

- `str_view`/`bytes` are non-owning views into request buffers / the pool; the
  owning path is only `str()` past `kSso`. This is what keeps the read path on
  the HTTP hot line allocation-free.
- Array/object bodies live behind a heap box so `Value` stays shallow-copyable
  at any nesting depth; the 4-slot inline array is the "small array" case the
  spec asks M2.3 to optimize iteration for.
- SSO is 14 today. M2.2 will raise it to 22–24 by widening the layout (see
  roadmap); every number above is pinned by 023, so that change re-pins it
  deliberately, never silently.