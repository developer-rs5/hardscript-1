# Search performance

Measured by `qa/bench_search.sh` against a local registry on 2026-09-27 22:04:35 UTC.

Every number below was recorded by the benchmark that produced it and
read back out of `reports/raw/`. Nothing here is estimated, and a metric
that was not measured is printed as `not measured` rather than filled in.

## Machine

| property | value |
| --- | --- |
| cpu_model | AMD Ryzen 7 7445HS w/ Radeon 740M Graphics |
| cpu_count | 12 |
| memory_kb | 15619132 |
| kernel | Linux 7.1.5+kali-amd64 |
| os | Kali GNU/Linux Rolling |
| filesystem | ext4 |
| rustc | rustc 1.97.1 (8bab26f4f 2026-07-14) |
| build_profile | release |

## What was measured

`hard search` wall-clock, per query shape, at three index sizes. One query
measures one cache line, so each size is asked four ways: an exact name, a
prefix, a fuzzy name one edit away, and a miss that returns nothing.

`direct_http` is the same request made with `curl`, included because it is
*slower* than the client: the client speaks HTTP over a socket directly,
while `curl` pays for a process launch every time. It is a useful reminder
that a benchmark which shells out measures the shell-out.

## Latency by index size and query shape

| query | 50 packages | 200 packages | 500 packages |
| --- | ---: | ---: | ---: |
| exact name | 6.00 ms | 8.94 ms | 11.96 ms |
| prefix | 6.36 ms | 8.77 ms | 12.16 ms |
| fuzzy (edit distance 1) | 5.44 ms | 6.81 ms | 9.68 ms |
| tag filter | 5.29 ms | 8.06 ms | 13.18 ms |
| miss | 4.87 ms | 6.27 ms | 8.98 ms |
| exact name, `--json` | 6.23 ms | 8.46 ms | 12.78 ms |

## p95 and index growth

| query | 50 packages (p95) | 200 packages (p95) | 500 packages (p95) |
| --- | ---: | ---: | ---: |
| exact name | 6.91 ms | 12.32 ms | 13.59 ms |
| prefix | 7.36 ms | 10.70 ms | 14.45 ms |
| fuzzy (edit distance 1) | 7.85 ms | 8.18 ms | 10.47 ms |
| tag filter | 6.85 ms | 9.66 ms | 14.73 ms |
| miss | 6.10 ms | 7.62 ms | 11.57 ms |
| exact name, `--json` | 7.65 ms | 11.15 ms | 14.85 ms |

## Client versus curl

| path | p50 | p95 | mean |
| --- | ---: | ---: | ---: |
| `hard search` (socket) | 11.96 ms | 13.59 ms | 12.04 ms |
| `curl` (process per request) | 18.02 ms | 22.40 ms | 18.20 ms |

| property | value |
| --- | ---: |
| samples per measurement | 20 |
| largest index measured | 500 packages |


