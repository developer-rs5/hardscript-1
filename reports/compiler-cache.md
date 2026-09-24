# Compiler Cache Report

Milestone M3.3, sub-milestones M3.3.2 (build cache), M3.3.3 (build
manifest), M3.3.6 (cache diagnostics). Describes the on-disk layout,
the invalidation contract, observed sizes, and the recovery behavior.

## Layout

Every project gets a hidden state directory at its root:

```
<project>/.hard/
  program.cpp        emitted C++ (merged TU)
  program            native binary
  build.json         build manifest (schema hard-build/v1)
  cache/
    env.json         environment fingerprint
    entries/<key>/   one directory per module per source revision
      meta.json      JSON descriptor
      items.bin      binary AST (astser)
```

Keys are `sha256("parse|v1|<rel>|<source_sha>|<env_fp>")` where `rel` is
the module path relative to the project root, `source_sha` is the
SHA-256 of the file contents, and `env_fp` the environment fingerprint
(compiler/runtime/platform/flags). A module edit therefore simply *misses*;
nothing is invalidated in place except by appending a new entry.

`build.json` (`hard-build/v1`) carries: `env_fp`, `merged_hash` (SHA-256
over the topological `(rel | source_sha)` pairs plus the environment), the
`modules` array in merge order (each with `rel`, `deps`, `status`
hit/miss, `items_bytes`), cache counters (`hits/misses/skipped/compiled`),
per-stage `timings_ms`, and `native_skipped`.

## Invalidation contract

- **Source edit** → that module's `source_sha` changes → parse key
  changes → one miss; everything else stays hit.
- **Environment change** (compiler, runtime headers, platform, flags) →
  `env_fp` changes → whole cache key space changes (natural, since the
  emitted artifacts are environment-sensitive).
- **Merge change** (import set, module set) → `merged_hash` changes →
  full non-warm rebuild, as checked before trusting any cached entry.
- **`jobs` does not invalidate** — it never changes output bytes.
- **Warm check = manifest agreement + outputs present**: `env_fp` and
  `merged_hash` match the current sources and `.hard/program.cpp` +
  `.hard/program` exist → skip the front-end and `g++` entirely.
- A warm check that passes is the only thing that can *skip* the native
  stage; any non-warm rebuild always re-runs `g++`, preserving
  byte-identity with a cold build (M3.2 snapshot gate).

## Observed sizes (benchmark projects)

| modules | cache entries | cache size | bytes / module |
|--------:|--------------:|-----------:|---------------:|
|    1    |    2 |     0.5 KB | 271 B  |
|   10    |   11 |     2.9 KB | 271 B  |
|   50    |   51 |    13.6 KB | 273 B  |
|  100    |  101 |    26.9 KB | 273 B  |
|  500    |  501 |   135 KB   | 277 B  |
| 1000    | 1001 |   271 KB   | 277 B  |

Two entries at N=1: the untouched `main.hard` key plus the edited revision
created by the incremental step. Per-module cost stabilizes at ~277 B
(astser binary AST + descriptor) regardless of scale.

`examples/rest-api` under `hard doctor`:

```
cache entries: 1
cache size: 1.6 KB
cache hits: 1
cache misses: 0
cache skipped: 1
cache compiled: 0
last build: warm (native skipped)
```

## Recovery

Cache reads are corruption-tolerant. `m3.3.2` wrote astser with a content
hash so a truncated or flipped `items.bin` fails load, and a bad blob is
simply degraded to a miss and rewritten on the rebuild. Regression fixture
050 overwrites a live entry with garbage, removes the binary, and asserts
the next build still completes with `1 miss / 1 compiled` and a correct
route. `env.json`/`build.json` parse failures take the same cold path.

Regression fixture 049 asserts the manifest contract end to end: schema,
64-hex `env_fp`/`merged_hash`, module list in merge order with dep edges,
and every `timings_ms` stage present.

## Diagnostics

`hard doctor` reports compiler/runtime versions, cache entry count, cache
size (human-readable), and the last build's hits/misses/skipped/compiled
plus warm-state — all either measured live from the store or read from
`build.json`; no numbers are fabricated or extrapolated.

## Guardrail

A 500-deep import chain reports `import chain too deep (limit 256)`
instead of overflowing; the cache is never consulted for a graph that
cannot legally merge.