# Registry performance

Measured by `qa/bench_registry.sh` against a local registry on 2026-09-27 22:04:35 UTC.

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

The registry as a whole: dependency resolution on a real graph, health checks,
and what mirror fallback costs when the default registry disappears.

- **resolve** — `hard install --offline` against a warm cache with no lockfile,
  so the measurement is resolution rather than downloading.
- **`--frozen`** — the lockfile is trusted: what a CI build pays when nothing
  changed.
- **health** — `GET /health` with `curl`.
- **mirror fallback** — the same install and search with the default registry
  killed and a live mirror configured.

## Resolution and install

| operation | p50 | p95 | mean | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| resolve (warm cache, no lockfile) | 19.65 ms | 24.75 ms | 21.07 ms | 17.76 ms | 24.75 ms |
| install `--frozen` | 21.49 ms | 24.38 ms | 20.76 ms | 17.14 ms | 24.38 ms |
| health check | 8.76 ms | 10.23 ms | 8.93 ms | 7.85 ms | 10.23 ms |

## Mirror fallback

The primary is killed between the two rows, so the second one includes
discovering that it is gone.

| operation | p50 | p95 |
| --- | ---: | ---: |
| install, primary up | 4.17 ms | |
| install, primary down (mirror answers) | 4.03 ms | 5.11 ms |
| search, primary down (mirror answers) | 6.03 ms | |

Fallback succeeded: **1** (1 = every install completed with the default registry killed).

### A finding from this benchmark

The first run of this benchmark measured a 600 ms fallback search. Almost all of
it was the client sleeping through its retry backoff (200 ms, then 400 ms) against
a host that was refusing connections. Nothing is going to start listening in 200
ms, and with a mirror configured the right move is to fail over immediately.
Refusals and unresolvable names are now treated as definitive and skip the backoff;
timeouts and resets still retry. That is the difference between the 600 ms above and
the 6.03 ms measured now.

## Graph shape

| property | value |
| --- | ---: |
| packages in the registry | 80 |
| graph width | 8 |
| graph depth | 5 |
| samples per measurement | 10 |

## The other three benchmarks

Run by `qa/bench_registry.sh` unless `BENCH_SKIP_SUB=1`. Full detail lives in
each report; this is the summary.

| benchmark | p50 | p95 |
| --- | ---: | ---: |
| publish (cold) | 5.43 ms | 6.75 ms |
| install (cold cache) | 40.42 ms | 49.30 ms |
| install (warm cache) | 38.78 ms | 43.57 ms |
| search (largest index) | 11.96 ms | 13.59 ms |

## Reports

- [publish](package-publish-performance.md)
- [download](package-download-performance.md)
- [search](search-performance.md)


