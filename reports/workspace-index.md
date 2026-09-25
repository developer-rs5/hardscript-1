# Workspace Index Report

## Scope

Background workspace indexing in `hs-lsp`: cold index time, index scaling, symbol
query latency, cross-file navigation, and incremental root changes.

## How It Was Measured

`qa/lsp/index_scan.py` drives the release binary over stdio JSON-RPC.

- `measure_scaling` writes synthetic workspaces of 25, 100, and 400 HardScript
  files, each declaring a model, a `calc`, and a route, then initializes a server
  whose root is that workspace and polls `workspace/symbol` with an empty query
  until the first non-empty result. The elapsed time is the cold index time.
- `measure_multimodule` writes a three-file project that uses real import syntax
  (`bring "./models/user"`, `bring "./handlers"`) and measures definition, hover,
  references, and symbol queries across the import boundary, then adds a second
  workspace root with `workspace/didChangeWorkspaceFolders` and re-measures.

Reproduce with:

```
python3 qa/lsp/index_scan.py --binary target/release/hs-lsp
```

Raw results: `qa/lsp/index-scan.json`.

## Cold Index Scaling

| Files | Bytes | `initialize` | Cold index ready | Workspace symbols | First query |
| --- | --- | --- | --- | --- | --- |
| 25 | 4,325 | 0.557 ms | 21.182 ms | 100 | 0.983 ms |
| 100 | 17,450 | 2.822 ms | 13.839 ms | 400 | 13.822 ms |
| 400 | 71,450 | 2.116 ms | 53.765 ms | 1,600 | 53.755 ms |

Indexing is linear in file count: 400 files index in 53.765 ms, roughly 0.13 ms per
file, and the 309-file `qa/corpus` indexes 5,349 symbols in 241.533 ms
(`qa/lsp/perf.json`). `initialize` never waits for the walk; the server registers
roots during `initialize` and indexes on `initialized` and on workspace-folder
changes.

## Symbol Query Latency

Against the indexed multi-module project, on a warm index:

| Query | Latency | Symbols |
| --- | --- | --- |
| `greeting` | 0.111 ms | 1 |
| `User` | 0.103 ms | 1 |
| `item` | 0.108 ms | 0 |
| `zzz` | 0.101 ms | 0 |

Against the 309-file corpus, the same query path returns 5,349 symbols with an
empty query in 0.5 ms once the index is ready.

## Cross-File Navigation

Project under test (`qa/lsp/index_scan.py` fixtures):

```
models/user.hard   model User = users [ id => Int #id, name => Str ]
handlers.hard      bring "./models/user"  +  calc greeting() => Str
main.hard          bring "./handlers"  +  bring http  +  GET "/" calling greeting()
```

| Request | Result | Latency |
| --- | --- | --- |
| `textDocument/definition` on `greeting()` in `main.hard` | `handlers.hard` line 2 (never opened by the client) | 0.230 ms |
| `textDocument/hover` on the same call | markdown documentation returned | 0.100 ms |
| `textDocument/references` on the same call | 2 locations: the declaration in `handlers.hard` and the call site in `main.hard` | 0.112 ms |
| `workspace/symbol` `greeting` | 1 symbol from the indexed closed file | 0.074 ms |

Navigation resolves through the background index, so targets in files the user
never opened are reachable. The same fixture is covered deterministically by
`backend_definition_crosses_file_boundary`,
`backend_references_cross_files_and_skip_object_keys`,
`backend_rename_edits_both_files`, and
`backend_references_reports_declaration_only_when_requested` in
`lsp/tests/regression.rs`.

## Incremental Root Changes

`workspace/didChangeWorkspaceFolders` adding a second root re-indexed the new root
and reported 6 symbols in 0.170 ms. Roots are kept sorted and de-duplicated, so
re-adding a known root is a no-op.

## Diagnostics Reachability

`Backend::diagnostics` follows `bring "./path"` imports to build the reachable
document set, merges those programs, and runs `check_with` plus `warn::analyze`
once per distinct reachable set. Parse errors in any reachable file suppress
merged diagnostics for that set, matching compiler behaviour, while per-file parse
diagnostics are still published. The multi-module fixture publishes 1 diagnostic
for `main.hard` on open.

## Limitations

- Symbols are indexed for closed files; occurrences are also indexed, so
  references and rename work across files, but a rename that would edit a file with
  parse errors is refused.
- The index is per server process and is rebuilt on restart. There is no
  persistence to disk, so a cold start pays the full walk shown above.
- The synthetic scaling fixtures declare identical shapes per file; real projects
  with many fields per model will produce more symbols per file and scale by symbol
  count rather than file count.
