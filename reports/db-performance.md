# ORM performance (2026-09-26)

Measured with `qa/bench_orm.sh` at `4f80c94` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`, SQLite 3.53.4. Median of 3 runs; every cell below comes from a
run, and the script fails instead of printing a number it did not measure.

Method: in-memory SQLite (CPU and library, not disk) and the scripted mock
PostgreSQL server on loopback (client framing plus a socket round trip, no
query execution — reported as `pg-mock`). Bench programs are built `-O2` from
the same headers the tests compile. `startup` opens a connection and runs DDL,
amortized over 20 iterations including the one-time library load.

## Backend comparison (ops/s)

| workload | sqlite | pg-mock | sqlite / pg-mock |
|---|---|---|---|
| `startup` | 10,858 | 7,128 | 1.5x |
| `create` | 323,520 | 17,858 | 18.1x |
| `update` | 396,432 | 20,305 | 19.5x |
| `find` | 321,750 | 17,227 | 18.7x |
| `find_many` | 948,317 | 866,551 | 1.1x |
| `join_has_many` | 19,412 | 14,956 | 1.3x |
| `join_belongs_to` | 277,778 | 16,588 | 16.7x |
| `tx_commit` | 1.66M | 10,600 | 157.0x |
| `tx_savepoint` | 638,570 | 5,612 | 113.8x |
| `tx_rollback` | 218,723 | 7,006 | 31.2x |
| `delete` | 530,926 | 23,040 | 23.0x |

The gap is the wire: every PostgreSQL statement is five framed messages
(Parse, Bind, Describe, Execute, Sync) plus a socket round trip, against
SQLite's function call into the same process. Both backends speak TCP_NODELAY
on loopback; without it the 40ms delayed-ACK timer owns every number. The
mock executes nothing, so these are client-stack costs — a floor, not a
ceiling, for a real server.

## Process footprint (peak RSS KB, heap bytes held at end of run)

| backend | peak RSS KB | heap held B |
|---|---|---|
| sqlite | 40800 | 343376 |
| pg-mock | 6696 | 1323008 |

Heap held is glibc `mallinfo2` `uordblks` at exit: everything the bench
process kept, which in these programs is the last working set. Per-operation
allocation behavior of values is covered by the value-engine reports; the ORM
layer adds no per-row allocation beyond the rows themselves.
