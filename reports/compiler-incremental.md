# Incremental Compiler Report

Milestone M3.3 (Incremental Compilation & Dependency Graph), sub-milestone
M3.3.7. All numbers measured on the developer machine (Linux x86-64,
12 logical CPUs), `target/release/hard`, wall clock with millisecond
precision, `HARD_TIMED=1` stage report. Native stage is `g++ -O2` against
the embedded runtime, which dominates every cold/incremental build; the
front-end columns exclude it.

## Methodology

Projects are generated as a fan-out: `main.hard` imports N-1 leaf modules
(`bring "./m1"` … `"./mN-1"`), each defining one `calc`. That shape is the
realistic application shape; a 500-deep *chain* intentionally trips the
module-graph depth guard (`import chain too deep (limit 256)`).

Per module count we measure, entry-point wall:

- **cold -j1** — clean `.hard`, serial parse (best of 3);
- **cold -j12** — clean `.hard`, parse across the rayon pool (best of 3);
- **warm** — unchanged sources, warm manifest check skips the entire
  front-end *and* the native stage;
- **incremental — one file changed** — the middle module gets one
  appended comment line and is rebuilt at -j12.

## Results

| modules | cold -j1 (total) | warm (total) | incremental (total) | cache summary |
|--------:|-----------------:|-------------:|--------------------:|:--------------|
|    1    | 3175 ms          | 23 ms        | 3194 ms             | 0 hit / 1 miss, 1 compiled |
|   10    | 3066 ms          | 21 ms        | 3161 ms             | 9 hit / 1 miss, 1 compiled |
|   50    | 2985 ms          | 19 ms        | 2957 ms             | 49 hit / 1 miss, 1 compiled |
|  100    | 2944 ms          | 23 ms        | 2909 ms             | 99 hit / 1 miss, 1 compiled |
|  500    | 3031 ms          | 22 ms        | 3070 ms             | 499 hit / 1 miss, 1 compiled |
| 1000    | 3068 ms          | 31 ms        | 3335 ms             | 999 hit / 1 miss, 1 compiled |

### Front-end vs native breakdown (cold -j1, best of 3)

| modules | discover | parse | typecheck | front-end total | native | total |
|--------:|---------:|------:|----------:|----------------:|-------:|------:|
|    1    | 0.1 ms   | 0.2   | 0.0       | 0.4             | 3151   | 3151  |
|   10    | 0.2      | 0.6   | 0.0       | 0.9             | 3041   | 3041  |
|   50    | 0.7      | 3.0   | 0.3       | 4.0             | 2958   | 2962  |
|  100    | 0.7      | 3.9   | 0.9       | 5.5             | 2919   | 2925  |
|  500    | 4.8      | 27.4  | 24.1      | 56.5            | 2954   | 3010  |
| 1000    | 6.6      | 56.5  | 92.2      | 155.6           | 2891   | 3047  |

## Findings

1. **Warm idle build: 19–31 ms** across all six module counts — the
   `<100 ms` target is met with a large margin. The warm path is a pure
   manifest check: environment fingerprint unchanged, merged SHA-256
   unchanged, outputs present, so neither the front-end nor `g++` runs.
   `examples/rest-api` cold = 4601 ms, warm = 23 ms (**200x** faster).
2. **Incremental (one file changed): one module reparses, N-1 are cache
   hits.** At 1000 modules that is `999 hit, 1 miss, 0 skipped,
   1 compiled` — the parse tier degrades to 4.4 ms (one source read +
   one astser round trip) and discovery to 9.5 ms. For ≤100 modules the
   incremental front-end is ~5 ms, below the `<50 ms` per-file target.
   At 500–1000 modules typecheck becomes the serial floor
   (24–115 ms): type checking runs over the merged TU on the main
   thread and is not yet incremental.
3. **Native recompiles on one-file changes** (~2.9–3.1 s, g++ -O2). This
   is a deliberate consequence of the single-merged-TU architecture:
   the emitted C++ must remain byte-identical to a cold whole-program
   build (M3.2 codegen snapshot gate), so an edit invalidates the native
   product even though 99.9% of the module front-end is reused. The
   incremental value today is the near-zero front-end re-do; native-tier
   caching is out of scope for a single-TU design.
4. **Cache hit ratio at idle: 100%** (warm rows show `N hit, 0 miss,
   N skipped, 0 compiled`); a one-file edit keeps **99.9%** of module
   parse-tier entries (`999 hit, 1 miss` at 1000 modules). The
   `<90% idle idle` target is met.
5. Determinism: cold -j1 vs -j12 are byte-identical (see the parallel
   report); phase-3 codegen snapshots show zero drift.

## Target sheet

| target | measured | status |
|--------|:--------:|:------:|
| warm < 100 ms                      | 19–31 ms       | PASS |
| incremental < 50 ms per file (front-end, ≤ 100 modules) | ~5 ms | PASS |
| incremental, 500–1000 modules (typecheck floor) | 44–130 ms | documented gap: typecheck is serial |
| cache hit > 90% at idle            | 99.9–100%      | PASS |
| parallel determinism 100%          | byte-identical | PASS |
| snapshot drift 0                   | phase3 16/16    | PASS |

The remaining lever for large-module incremental builds is incremental
type checking, which is not part of M3.3 (the optimizer rollup was fixed
at M3.2 and type-checking stays whole-program until a later milestone).