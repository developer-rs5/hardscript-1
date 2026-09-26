# ORM QA report (M5.3.10)

Produced by `qa/run-framework-tests.sh` on 2026-09-26 at `1311dbf` (plus the
QA commit itself), on 12 CPUs (AMD Ryzen 7 7445HS), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`, SQLite 3.53.4. Every suite below passed; any
failure fails the harness with a non-zero exit.

## Gates

| suite | result |
|---|---|
| `cargo test --workspace` | pass (251 compiler tests, 38 pm, all crates) |
| `tests/run-regressions.sh` | 57/57 pass |
| `tests/integration.sh` | 21 pass, 0 fail |
| `qa/phase3_snap.sh` | 16 cases pass (deterministic codegen) |
| `qa/cli_matrix` | 53/53 lanes pass |
| `qa/run-orm-tests.sh` | 24/24 suites pass |
| ASan + UBSan, all 8 C++ fixtures | clean |
| LSan, all 8 C++ fixtures | clean |
| formatter idempotency, 15 ORM fixtures | pass |
| `docs/errors.md` matches `hard errors --markdown` | pass |

## Minimums

A test belongs to exactly one area: C++ fixtures by file, compiler unit
tests by name (batch, then migration, then relationships, then CRUD),
generated expectations and diagnostics by fixture, CLI lanes by id.

| area | count | minimum | composition |
|---|---|---|---|
| CRUD | 188 | 40 | query_builder 65, crud 51, compiler unit 55, gen 13, diag 4 |
| relationships | 74 | 20 | relations 37, compiler unit 26, gen 7, diag 4 |
| migration | 104 | 25 | migrate 61, compiler unit 34, manifest 2, CLI lanes 7 |
| transaction | 71 | 25 | transactions 50, typecheck 12, fmt/astser 2, gen 3, diag 4 |
| SQLite | 82 | 20 | sqlite fixture, real libsqlite3 |
| PostgreSQL | 139 | 20 | pgsql fixture, mock wire server |
| batch | 59 | 20 | batch 46, compiler unit 7, gen 4, diag 2 |
| **total** | **717** | **160** | |

## Sanitizers

Each of `query_builder`, `crud`, `relations`, `sqlite`, `pgsql`, `migrate`,
`transactions`, `batch` was built with `-fsanitize=address,undefined` and
with `-fsanitize=leak` and run to green. The mock-server fixture runs its
socket threads under all three. A transient sandbox kill (empty output) is
retried once; a real finding reproduces every time.

## Determinism

- `phase3_snap`: every codegen case rebuilds byte-identically, twice.
- `hard fmt` is idempotent on all 15 ORM fixtures (format twice, compare).
- `docs/errors.md` is byte-identical to the catalog generator's output.
- The benchmark reports regenerate from runs only (`qa/bench_orm.sh` fails
  instead of printing an unmeasured number).
