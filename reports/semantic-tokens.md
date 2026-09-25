# Semantic Tokens Report

## Scope

Semantic highlighting in `hs-lsp`: legend coverage, corpus-wide emission rate, and
token request latency.

## How It Was Measured

`qa/lsp/semantic_scan.py` initializes the release binary with `rootUri` set to
`qa/corpus`, waits for the background index, then walks every `.hard` file in the
corpus. For each file it sends `textDocument/didOpen`, requests
`textDocument/semanticTokens/full`, decodes the LSP delta encoding, and sends
`textDocument/didClose` so each file is measured in a realistic single-open-file
workspace.

Reproduce with:

```
python3 qa/lsp/semantic_scan.py --binary target/release/hs-lsp
```

Raw results, including a per-file row for all 309 files: `qa/lsp/semantic-tokens.json`.

## Coverage

| Metric | Value |
| --- | --- |
| Files scanned | 309 |
| Bytes scanned | 1,210,946 |
| Lines scanned | 38,458 |
| Tokens emitted | 179,090 |
| Legend token types | 14 |
| Legend token modifiers | 0 |
| Distinct token types observed | 14 of 14 |
| Workspace symbols indexed during the scan | 5,349 |

Every token type in the advertised legend is produced by the corpus; no legend
entry is dead and no emitted token falls outside the legend.

## Token Type Distribution

| Token type | Count | Share |
| --- | --- | --- |
| `operator` | 106,797 | 59.6% |
| `number` | 55,430 | 31.0% |
| `variable` | 5,534 | 3.1% |
| `string` | 5,302 | 3.0% |
| `method` | 5,219 | 2.9% |
| `keyword` | 385 | 0.2% |
| `function` | 154 | 0.1% |
| `parameter` | 117 | 0.1% |
| `decorator` | 51 | 0.0% |
| `module` | 47 | 0.0% |
| `property` | 31 | 0.0% |
| `type` | 15 | 0.0% |
| `const` | 5 | 0.0% |
| `comment` | 3 | 0.0% |

The distribution matches the language: the stress fixtures are dominated by
operator and number tokens, while declarations, imports, decorators, and comments
are rare across the corpus.

## Classification

Classification is resolution-aware rather than purely syntactic, and it is what the
counts above measure:

- `function` and `method` are assigned only when the identifier resolves to a
  `calc` function or a method, either in the current file or through the
  background index. Unresolved calls stay `variable` so that a call the server
  cannot resolve is not mislabelled.
- `type` and `property` are assigned from resolved model and field declarations,
  including fields reached through the background index.
- `module` is assigned to `bring` statements for both built-in modules and local
  `./path` imports.
- `parameter`, `const`, and `decorator` come from parameter declarations, `const`
  declarations, and `@decorator` usages.
- `comment` ranges come from the comment lexer pass and are merged with the
  classified ranges before encoding, so they are always emitted.

## Latency

`textDocument/semanticTokens/full` round trip over the same 309 files
(`qa/lsp/perf.json`):

| Statistic | Milliseconds |
| --- | --- |
| min | 0.110 |
| p50 | 0.222 |
| p90 | 0.520 |
| p95 | 0.740 |
| max | 33.395 |
| total | 151.965 |

The 33.395 ms maximum is `p1_stress/123-huge-list.hard`, whose longest line is
338,899 characters; the request still returns in tens of milliseconds after the
`TextIndex` checkpoint work described in `reports/lsp-performance.md`.

The full open-and-close round trip including diagnostics, measured by
`qa/lsp/semantic_scan.py` over the same 309 files, has a 23.04 ms median and a
6,980.6 ms maximum, the maximum being
`p1_stress/121-large-5000-routes.hard` where the frozen compiler typechecker, not
token computation, dominates the request.

## Position Encoding

All measurements use `general.positionEncodings: ["utf-16"]`, and the server
declares UTF-16 support. Token ranges are encoded in UTF-16 code units through
`TextIndex`, which indexes UTF-16 checkpoints per line, so multi-byte source
positions survive the delta encoding on the same corpus that contains a
200,011-character single-string fixture.

## Determinism

`backend_semantic_tokens_are_encoded` and the `semantic_tokens` case in
`qa/run-lsp-tests.sh` assert encoded output for a fixed document, and the 112
LSP regression tests pass, so the legend and encoding are pinned independently of
these measurements.
