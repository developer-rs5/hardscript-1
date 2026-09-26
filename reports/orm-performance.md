# ORM performance (2026-09-26)

Measured with `qa/bench_orm.sh` at `4f80c94` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`, SQLite 3.53.4. Median of 3 runs; every cell below comes from a
run, and the script fails instead of printing a number it did not measure.

Method: in-memory SQLite (CPU and library, not disk) and the scripted mock
PostgreSQL server on loopback (client framing plus a socket round trip, no
query execution — reported as `pg-mock`). Bench programs are built `-O2` from
the same headers the tests compile. `startup` opens a connection and runs DDL,
amortized over 20 iterations including the one-time library load.

## Single-row operations (ops/s, mean us/op)

| workload | sqlite ops/s | sqlite us/op | pg-mock ops/s | pg-mock us/op |
|---|---|---|---|---|
| `startup` | 10,858 | 92.1 | 7,128 | 140.3 |
| `create` | 323,520 | 3.1 | 17,858 | 56.0 |
| `update` | 396,432 | 2.5 | 20,305 | 49.3 |
| `find` | 321,750 | 3.1 | 17,227 | 58.0 |
| `find_many` | 948,317 | 1.1 | 866,551 | 1.2 |
| `join_has_many` | 19,412 | 51.5 | 14,956 | 66.9 |
| `join_belongs_to` | 277,778 | 3.6 | 16,588 | 60.3 |
| `tx_commit` | 1.66M | 0.6 | 10,600 | 94.3 |
| `tx_savepoint` | 638,570 | 1.6 | 5,612 | 178.2 |
| `tx_rollback` | 218,723 | 4.6 | 7,006 | 142.7 |
| `delete` | 530,926 | 1.9 | 23,040 | 43.4 |

`find_many` reads 200 keys per call; `join_has_many` reads one user's posts
with no index on the foreign key, so each call scans the table — the honest
cost of an unindexed join, and the reason migrations exist. `tx_commit` is an
empty guarded block; `tx_savepoint` nests two; `tx_rollback` inserts a row and
throws it away.

## Batches (rows/s, mean us/row, SQLite)

| workload | rows/s | us/row |
|---|---|---|
| `batch_create_100` | 386,100 | 2.6 |
| `batch_create_1000` | 352,609 | 2.8 |
| `batch_create_10000` | 351,173 | 2.8 |
| `batch_find_10000` | 545,613 | 1.8 |
| `batch_update_10000` | 557,476 | 1.8 |
| `batch_delete_10000` | 2.41M | 0.4 |

## Batches (rows/s, pg-mock)

| workload | rows/s | us/row |
|---|---|---|
| `batch_create_100` | 18,484 | 54.1 |
| `batch_create_1000` | 20,589 | 48.6 |

A batch is one transaction: ten thousand rows land or none do. `create_many`
inserts row by row through the same path as `create`; `update_many` shares one
prepared statement per column shape; `delete_many` and `find_many` cut long id
lists into chunks under the backend's variable cap.
