# Small String Optimization — ms2.2

Goal (spec): SSO threshold 22–24 bytes, UTF-8 safe, measured on 8/16/24/32/64/128
byte strings (allocations + RSS). Implemented and locked in during ms2.2.

## Threshold

`Value::kSso = 24`, filling the widened 32-byte tagged union (header 4 bytes +
24-byte payload). Regression `tests/regression/023-value-layout.cpp` pins
`sizeof=32 alignof=8 sso=24`.

## Per-string allocation behavior (fixtures)

Measured per instance with the operator-new counter harness
(`tests/regression/024-sso-small.cpp`):

| bytes | storage | allocs | bytes allocated |
|---|---|---|---|
| 0 | SSO | 0 | 0 |
| 1 | SSO | 0 | 0 |
| 8 | SSO | 0 | 0 |
| 16 | SSO | 0 | 0 |
| 23 | SSO | 0 | 0 |
| 24 | SSO | 0 | 0 |
| 25 | heap | 2 | 58 |
| 26 | heap | 2 | 59 |
| 32 | heap | 2 | 65 |
| 64 | heap | 2 | 97 |
| 128 | heap | 2 | 161 |
| 512 | heap | 2 | 545 |
| 1024 | heap | 2 | 1057 |

Heap strings are exactly 2 allocations: the 32-byte `std::string` object plus a
`size+1` buffer (libstdc++). Copying an SSO string allocates nothing; a `str("")`
is a valid zero-length inline view (the pre-ms2.2 `own_` bool made this read
indeterminate union memory).

## UTF-8 safety (fixture 026)

SSO copies raw bytes and never interprets them, so a codepoint is never split
mid-sequence on the inline path. Fixture 026 proves it for 3-byte (U+20AC) and
4-byte (U+1F980) sequences at the 24/25-byte boundary, and for a codepoint
straddling the boundary (23 ASCII + `€` = 26 bytes → heap, intact). Non-ASCII
bytes pass through JSON verbatim.

## Live-memory probe

`qa/benchmark/micro/string_sso_probe.cpp` (200 000 live values per size,
operator-new counter + `VmRSS`). `rss_growth_kb` is relative to the already
reserved value vector, so small values show 0; large values pay the exact
buffers.

```
size=8    n=200000  allocs=0      bytes=0        allocs_per_value=0.00 bytes_per_value=0.00 rss_growth_kb=0
size=16   n=200000  allocs=0      bytes=0        allocs_per_value=0.00 bytes_per_value=0.00 rss_growth_kb=0
size=24   n=200000  allocs=0      bytes=0        allocs_per_value=0.00 bytes_per_value=0.00 rss_growth_kb=0
size=32   n=200000  allocs=400000 bytes=13000000 allocs_per_value=2.00 bytes_per_value=65.00 rss_growth_kb=18748
size=64   n=200000  allocs=400000 bytes=19400000 allocs_per_value=2.00 bytes_per_value=97.00 rss_growth_kb=24868
size=128  n=200000  allocs=400000 bytes=32200000 allocs_per_value=2.00 bytes_per_value=161.00 rss_growth_kb=31124
```

## Notes

- The 0-allocation guarantee for 8/16/24-byte strings makes typical route
  response strings ("ok", names, IDs, headers) allocation-free on the read path
  once M2.6 wires the engine into HTTP.
- RSS numbers are page-granular and process-global; the allocation counts are
  the deterministic signal.
- This supersedes the ms2.0 `value_probe.cpp` §1 (sso14/sso15 lanes); kept as a
  historical snapshot in `reports/value-foundation.md`.