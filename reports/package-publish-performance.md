# Package publish performance

Measured by `qa/bench_publish.sh` against a local registry on 2026-09-27 22:04:35 UTC.

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

`hard publish` end to end: read `hard.toml`, build the deterministic `.hspkg`,
sign it, upload it, and let the registry validate and commit it. Wall-clock
around the real client, so process start is included — that is what a user
waits for.

- **cold** — a package name published for the first time.
- **warm** — a new version of a package the registry already holds.
- **`--dry-run`** — validate and describe, no upload: the build-and-sign half
  of a publish on its own.
- **dependency graph** — a manifest with two dependencies, so the resolver and
  dependency validation are part of the measurement.
- **server round trip** — `curl` doing metadata + archive reads, with no client
  process in the way. Compare it with the client rows to see which side the
  time is on.

## Latency

| operation | p50 | p95 | mean | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| publish (cold) | 5.43 ms | 6.75 ms | 5.38 ms | 4.48 ms | 6.75 ms |
| publish (warm) | 5.78 ms | 6.92 ms | 5.92 ms | 5.15 ms | 6.92 ms |
| publish `--dry-run` | 4.94 ms | 5.86 ms | 4.96 ms | 3.56 ms | 5.86 ms |
| publish (with dependencies) | 5.67 ms | 6.86 ms | 5.74 ms | 4.84 ms | 6.86 ms |
| server round trip (2 GETs) | 16.38 ms | 20.02 ms | 16.58 ms | 13.65 ms | 20.02 ms |

## Payload

| property | value |
| --- | ---: |
| archive size | 12000 bytes |
| source lines per package | 400 |
| samples per measurement | 40 |

## Registry state afterwards

| property | value |
| --- | ---: |
| packages | 43 |
| versions | 121 |
| downloads | 40 |


