# ORM performance (2026-09-26)

Measured with `qa/bench_orm.sh` at `4f80c94` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`, SQLite 3.53.4. Median of 3 runs; every cell below comes from a
run, and the script fails instead of printing a number it did not measure.

Method: in-memory SQLite (CPU and library, not disk) and the scripted mock
PostgreSQL server on loopback (client framing plus a socket round trip, no
query execution — reported as `pg-mock`). Bench programs are built `-O2` from
the same headers the tests compile. `startup` opens a connection and runs DDL,
amortized over 20 iterations including the one-time library load.

## Migrations and seeds, SQLite (ops/s, mean us/op)

| workload | ops/s | us/op |
|---|---|---|
| `migrate_parse` | 63,452 | 15.8 |
| `migrate_up_1tbl` | 6,369 | 157.0 |
| `migrate_up_5tbl` | 3,860 | 259.1 |
| `migrate_up_20tbl` | 1,334 | 749.5 |
| `migrate_status` | 33,761 | 29.6 |
| `migrate_down` | 2,566 | 389.7 |
| `seed_1000` | 366,667 | 2.7 |

`migrate_parse` parses a 3-table migration file fifty times: pure CPU, no
database. `migrate_up_Ntbl` applies a fresh N-table migration (tables plus
indexes plus a foreign key) on a new in-memory database, ten times each, after
a warmup that takes the one-time library load off the first lane.
`migrate_status` reads history fifty times; `migrate_down` applies then rolls
back ten times; `seed_1000` inserts a parent plus a thousand child rows in one
transaction.

Apply cost grows with the migration: ~0.16ms for one table, ~0.26ms for five,
~0.75ms for twenty. Parsing is microseconds; status is a single indexed read.
