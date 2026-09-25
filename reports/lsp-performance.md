# LSP Performance Report

## Scope

HardScript `v0.5.0` language server (`hs-lsp`) request latency over the `qa/corpus`
fixture set, measured end to end over the real stdio JSON-RPC transport with the
release binary `target/release/hs-lsp`.

Frozen compiler behaviour was not modified. Every number below comes from the
harnesses in `qa/lsp/`, which are re-runnable with `qa/run-lsp-perf.sh`.

## How It Was Measured

- `qa/lsp/perf_scan.py` opens each corpus file with `textDocument/didOpen`, waits for
  the matching `textDocument/publishDiagnostics` notification, requests
  `textDocument/semanticTokens/full`, then sends `textDocument/didClose`.
  Diagnostic latency is the didOpen to publish round trip; token latency is the
  `semanticTokens/full` request round trip.
- `qa/lsp/semantic_scan.py` measures the same round trip and decodes the returned
  delta-encoded token stream.
- `qa/lsp/index_scan.py` measures cold workspace indexing and cross-file navigation.
- Percentiles are computed over per-file samples by `qa/lsp/perf_scan.py`
  (`p50`/`p90`/`p95`/`max`); raw samples are committed in `qa/lsp/perf.json`.

Reproduce with:

```
./qa/run-lsp-perf.sh
```

## Corpus Under Test

| Metric | Value |
| --- | --- |
| Files | 309 |
| Bytes | 1,210,946 |
| Lines | 38,458 |
| Workspace symbols indexed | 5,349 |
| `initialize` round trip | 0.861 ms |
| Cold workspace index ready | 241.533 ms |

The corpus mixes valid programs, parse errors, type errors, and the
`qa/corpus/p1_stress/` fixtures (`120-large-10000-lines`, `121-large-5000-routes`,
`122-huge-single-string`, `123-huge-list`, `022-deep-open-10000`,
`024-deep-nested-expr2`).

## Results

Diagnostics round trip (`didOpen` to `publishDiagnostics`), 309 samples:

| Statistic | Milliseconds |
| --- | --- |
| min | 19.735 |
| p50 | 29.578 |
| p90 | 34.489 |
| p95 | 36.792 |
| max | 4,751.880 |
| mean | 47.157 |
| total | 14,571.423 |

`textDocument/semanticTokens/full` round trip, 309 samples:

| Statistic | Milliseconds |
| --- | --- |
| min | 0.110 |
| p50 | 0.222 |
| p90 | 0.520 |
| p95 | 0.740 |
| max | 33.395 |
| mean | 0.492 |
| total | 151.965 |

Slowest files, by diagnostics round trip:

| File | Bytes | Diagnostics | Round trip |
| --- | --- | --- | --- |
| `p1_stress/121-large-5000-routes.hard` | 152,780 | 5,000 | 4,751.9 ms |
| `p1_stress/022-deep-open-10000.hard` | 40,000 | 10,001 | 313.2 ms |
| `p1_stress/120-large-10000-lines.hard` | 447,780 | 10,000 | 221.6 ms |
| `p1_stress/123-huge-list.hard` | 338,915 | 1 | 127.3 ms |
| `p1_stress/122-huge-single-string.hard` | 200,027 | 1 | 83.4 ms |
| `p2_type/001-undef-route.hard` | 27 | 1 | 70.4 ms |
| `p1_stress/024-deep-nested-expr2.hard` | 10,002 | 1 | 53.6 ms |
| `p1_parse/001-empty.hard` | 0 | 0 | 51.0 ms |

The two 10,000-diagnostic results are the expected cost of materialising 10,000
`Diagnostic` messages, not of analysis: the analysis layer for those files
completes in 10 ms and 4 ms respectively.

## Analysis Cost of the Hot Path

The `121-large-5000-routes` outlier is dominated by the frozen compiler
typechecker, not by the server. In-process stage timings for that file on the same
machine: frontend parse 7 ms, lex 1 ms, `DocumentAnalysis::new` 37 ms,
`hs_compiler::typecheck::check_with` 4,391 ms producing 0 diagnostics, and
`hs_compiler::warn::analyze` 1 ms producing the 5,000 warnings that make up the
4,751.9 ms round trip. The server's own analysis work is under 1% of that
request, and the compiler was left untouched for this milestone.

## Quadratic Fixes Landed in This Milestone

Three superlinear costs in the analysis layer were found with in-process stage
timings and removed. All before/after numbers are `DocumentAnalysis::new` on the
release profile, same machine, same corpus files.

| Hot spot | Symptom | Fix | Before | After |
| --- | --- | --- | --- | --- |
| `TextIndex::position` / `TextIndex::offset` | Scanned the whole line per call; `123-huge-list.hard` has a 338,899 character line, and every token asked for a position | Per-line UTF-16 checkpoint index (one checkpoint per 32 characters) with binary search, so a conversion walks at most 32 characters | 79,584 ms | 40 ms |
| `DocumentAnalysis::selection_for` | Walked the whole token vector for every symbol, so 15,000 symbols in `121-large-5000-routes.hard` cost 600,000,000 token conversions | Precomputed `token_starts` plus `partition_point`, then scan only tokens inside the requested range | 4,514 ms | < 1 ms |
| `build_occurrences` / `build_semantic` | Linear symbol scans per identifier, linear occurrence scans per token | `SymbolLookup` maps (name and container, sorted selection ranges, models, local blocks), occurrence ordering by range with binary search | 10,542 ms | 14 ms |

Cumulative effect on the stress corpus: `123-huge-list.hard` analysis went from
79,584 ms to 40 ms and `121-large-5000-routes.hard` from 11,612 ms to 42 ms.
`024-deep-nested-expr2.hard` went from 123 ms to 1 ms, and its end-to-end
diagnostics round trip from 4,057 ms to 53.6 ms.

## Diagnostics Cost Per Open Document

`Backend::diagnostics` used to rebuild and re-typecheck the merged reachable
program once per open document, so publish cost grew with the number of open
files. Results are now memoised per distinct reachable document set
(`Backend::program_diagnostics`), which is behaviour-preserving: the same merged
program and the same `check_with` plus `warn::analyze` inputs produce the same
diagnostics, once instead of N times. Measured on the corpus scan, per-file
diagnostics latency stays at a 29.578 ms p50 while documents are opened and closed
one at a time, and the earlier 3.6 s per-didOpen behaviour at 120 simultaneously
open documents is gone.

## Background Indexing

Workspace indexing is deferred off the initialize handshake through
`tokio::spawn`, so `initialize` never waits for a workspace walk. The measured cold
index for the 309-file corpus is 241.533 ms, and `initialize` returns in 0.861 ms.
See `reports/workspace-index.md` for the scaling table.

## Determinism

`qa/run-lsp-tests.sh` passes 15/15 protocol cases against the debug binary with an
empty server stderr, and `cargo test --workspace` passes 112 LSP tests, including
five cross-file cases for definition, references, and rename across an import
boundary. Latency numbers above are wall-clock measurements and are expected to
variance by machine; the pass/fail gates do not depend on them.
