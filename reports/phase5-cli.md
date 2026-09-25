# Phase 5 — CLI Matrix QA Report

Binary: `target/release/hard` · harness: `qa/cli_matrix/` (hermetic)

## Matrix result

| metric | value |
|---|---|
| lanes (torture cases) | 46 |
| passed | 46 |
| findings | 0 |
| pass rate | 100.0% |

Runner output (verbatim): `cases: 46  pass: 46  findings: 0`

## Subcommand surface exercised

`` `add` `bench` `build` `docs` `doctor` `fmt` `frobnicate` `help` `new` `run` `test`

## Notes

- Harness is hermetic: fresh sandbox per lane; byte-identical across reruns.
- Exit-code contract verified: rc0 success, rc1 runtime/file errors,
  rc2 usage errors, rc124 timeout (run server staying healthy = PASS).
