# Parallel Module Compilation Report

Milestone M3.3, sub-milestone M3.3.5. `hard build [-j N|--jobs N|--jobs=N]`
parses modules concurrently on a rayon pool; the pool size defaults to the
machine's available parallelism (12 on the measurement host). Parallelism is
deliberately confined to the per-module front-end: cache entry lookup, source
read, astser round trip or lex+parse, and cache write. The merged program is
always reassembled in deterministic topological order, so output bytes do not
depend on worker count or scheduling.

## Methodology

Same fan-out projects as the incremental report (`main.hard` imports N-1
leaf modules). Each row is the best of 3 cold builds (cache wiped before
each) at `-j 1` and `-j 12`, split from the `HARD_TIMED=1` stage report.

## Results

| modules | parse -j1 | parse -j12 | parse speedup | front-end -j1 | front-end -j12 | front-end speedup |
|--------:|----------:|-----------:|--------------:|--------------:|---------------:|------------------:|
|    1    | 0.2 ms    | 0.8 ms     | x0.25 (overhead) | 0.4 ms     | 0.8 ms         | x0.5              |
|   10    | 0.6       | 0.7        | x0.86 (overhead) | 0.9        | 1.0            | x0.9              |
|   50    | 3.0       | 1.3        | x2.31         | 4.0           | 2.3            | x1.74             |
|  100    | 3.9       | 2.2        | x1.77         | 5.5           | 4.3            | x1.28             |
|  500    | 27.4      | 8.1        | x3.38         | 56.5          | 44.1           | x1.28             |
| 1000    | 56.5      | 16.6       | x3.40         | 155.6         | 131.0          | x1.19             |

(Front-end = discover + parse + merge + optimize + typecheck + codegen;
the typecheck and merge stages stay serial by design, which is why the
front-end speedup is bounded by Amdahl's law below the parse speedup.)

### Readings

1. **Parse speedup x3.4 at 500–1000 modules.** Each module load is
   independent (own source, own cache entry key), so the pool scales until
   the serial discover/typecheck work dominates.
2. **Pool overhead below ~50 modules.** A 1- or 10-module build pays
   ~0.5 ms spinning up 12 workers for ~0.6 ms of parse work —
   a sub-millisecond effect on cold builds, invisible on warm builds
   (no pool) and acceptable given the 3 s native stage. Projects that
   want the tail notched can pass `-j 1`; the default stays the machine
   parallelism for the large-workload case.
3. **Cold total wall is native-dominated** (g++ -O2 ≈ 2.9–3.1 s for every
   project size), so cold end-to-end times are near-equal between -j1 and
   -j12; the parallel gain is in the front-end that runs *before* the
   edit-free warm tier and before native. The hot incremental path
   (N-1 cache hits) is where the parallel pool does its daily work — see
   `reports/compiler-incremental.md`.

## Determinism

Parallelism never changes output bytes:

1. **Unit test** `build::tests::parallel_and_serial_are_byte_identical` —
   17-module project, `jobs=1` vs `jobs=8`: byte-identical C++ and equal
   merged hash.
2. **Unit test** `build::tests::parallel_rebuild_counts_agree_via_manifest`
   — a one-file edit at `jobs=4` records `16 hit, 1 miss`, matching the
   serial counts exactly.
3. **Regression fixture 048** (`tests/regression/incremental/`):
   21-module project, `-j 1` vs `--jobs 8` — byte-identical C++, equal
   merged hash.
4. **Measurement run** (this report): 1000-module project, `-j 1` vs
   `-j 12`, clean `.hard` — `cmp` of the emitted C++ is silent and
   `build.json.merged_hash` matches (`64-hex, equal`).

The manifest's `is_warm` deliberately excludes `jobs`: changing worker
count never invalidates a warm build, and it never changes the merged
SHA either.

## Implementation note: worker stack sizing

Rayon's default worker stack (2 MiB on Linux, 512 KiB on macOS) is smaller
than the main thread's (~8 MiB). The parser's `MAX_DEPTH` guard (256 nested
levels) is tuned to fire *before* a native overflow of the main thread
(measured overflow ~450 levels on an unoptimized build); on an un-sized
worker the process aborted with a stack overflow before the guard tripped
(deep-nesting regression 001, fixed in `f09002a`). The pool is now built
with 64 MiB worker stacks, so the guard always fires first and deep nesting
still reports `nesting too deep (limit 256)` as a clean diagnostic.

## Gate summary

| gate | result |
|------|--------|
| unit tests (incl. both parallel tests) | 65 / 65 |
| regression suite (incl. 042–050)       | 50 / 50 |
| integration                             | 21 / 21 |
| phase-3 codegen snapshots              | 16 / 16, zero refreshes |
| release build                           | 0 warnings |