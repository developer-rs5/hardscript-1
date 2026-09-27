# Package download performance

Measured by `qa/bench_download.sh` against a local registry on 2026-09-27 22:04:35 UTC.

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

`hard install` for a project depending on a corpus of published packages,
timed end to end. Warm and cold are different operations and are never mixed:
a cold install pays for HTTP and disk writes, a warm one pays for digest
checks and resolution.

- **cold** — empty cache, every package fetched.
- **warm** — cache already populated; no network at all.
- **`--frozen`** — trust the lockfile: resolution and link only.
- **`--offline`** — the cache is the only registry there is.
- **archive GET** — one archive over the wire with `curl`, no client in the way.

## Latency

| operation | p50 | p95 | mean | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| install (cold cache) | 40.42 ms | 49.30 ms | 41.01 ms | 31.98 ms | 49.30 ms |
| install (warm cache) | 38.78 ms | 43.57 ms | 39.26 ms | 33.99 ms | 43.57 ms |
| install `--frozen` | 11.43 ms | 13.82 ms | 11.29 ms | 9.79 ms | 13.82 ms |
| install `--offline` | 10.66 ms | 14.09 ms | 11.19 ms | 8.91 ms | 14.09 ms |
| archive GET | 9.42 ms | 10.05 ms | 9.23 ms | 7.85 ms | 10.05 ms |

## Cache behaviour

The hit rate and byte counts are the client's own numbers, parsed out of its
output rather than recomputed here.

| property | cold | warm |
| --- | ---: | ---: |
| cache hit rate | 0% | 100% |
| bytes over the network | 300082 | 0 |
| requests | 25 | 0 |

| property | value |
| --- | ---: |
| cache size on disk | 655913 bytes |
| packages in the corpus | 25 |
| source lines per package | 400 |
| samples per measurement | 12 |


