# Value Engine Layout — ms2.2 (tagged union, SSO-24)

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
| String | `char[24]` SSO, `std::string*`, or non-owning view | strings > 24 bytes only |
| Array | `ValueVec` box (4 inline slots, spill→`std::vector`) | box + spill |
| Object | `ValueObj` box (4 inline slots, spill→`std::vector`) | box + spill |
| Function | `void*` fn ptr + `const char*` pooled name | none (name interned once in `ConstPool`) |
| Bytes | non-owning `{const char*, size_t}` view | never |

## Layout (measured)

```
sizeof(Value)  = 32
alignof(Value) = 8
sso            = 24
kinds          = 9
array_inline   = 4
object_inline  = 4
polymorphic    = 0      (no virtuals)
standard_layout = yes
```

Header is `ValueKind kind_ + uint8_t fl_ + uint8_t sso_n_ + uint8_t pad_` (4
bytes); the union payload is 24 bytes (`char sso[24]` sized to fill it). The
`fl_` bits select the string storage path — `kOwn` (heap `std::string`, bit 0),
`kInline` (SSO, bit 1), `kView` (non-owning view, bit 2) — so empty strings are
representable without reading indeterminate union bytes (the pre-ms2.2 `own_`
bool made `str("")` read garbage from the view slot).

`Value` is a plain tagged union — no `std::variant`, no `std::any`, no virtuals,
no RTTI. `value_layout_report()` exposes the one-line summary so a generated
report or the `hard` CLI can embed it without re-deriving the C++ type.

## Allocation behavior (measured in 019–022, 024–026)

- 200 000 Int constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Bool constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Float constructs/destroys → 0 allocations, 0 bytes.
- 200 000 Nil constructs/destroys → 0 allocations, 0 bytes.
- Strings of 0, 1, 8, 16, 23, 24 UTF-8-safe bytes → 0 allocations; a 25-byte
  string (or a multibyte sequence crossing the 24-byte boundary) → exactly
  2 allocations (32-byte `std::string` object + `size+1` buffer).
- Copying an SSO string (incl. the empty string) → 0 allocations.
- UTF-8 bytes (3-byte U+20AC, 4-byte U+1F980) are stored verbatim — SSO never
  interprets nor truncates a codepoint (026).

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
- SSO was raised 14 → 24 in ms2.2 (fixtures 024–026) and re-pinned the layout
  here: `sizeof 32, align 8, sso 24`. 023 pins every measured number, so any
  future layout drift fails the suite deliberately, never silently.