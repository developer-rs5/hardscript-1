# Cloud runtime performance, v0.7 (2026-09-27)

Measured with `qa/bench_runtime.sh` at `b84fba9` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`. Median of 3 runs; every cell below comes from a run, and the
script fails instead of printing a number it did not measure.

Every number here is one lane from the four component reports, which are
where the method and the per-lane detail live:
`cache-performance.md`, `queue-performance.md`, `scheduler-performance.md`.
Lanes are in-process, single thread unless the lane says otherwise.

## Targets

| target | measured | required | verdict |
|---|---|---|---|
| cache, mixed operations | 9.83M ops/s | 100.0k ops/s | met |
| rate limiter, checks | 19.77M ops/s | 100.0k ops/s | met |
| scheduler, next-fire decision | 58ns | 5us | met |

## Subsystems

| lane | ops/s | mean us/op |
|---|---|---|
| `cache` mixed | 9.83M | 102ns |
| `queue` enqueue+poll+complete | 3.16M | 316ns |
| `scheduler` next daily fire | 17.32M | 58ns |
| `session` start+drain | 234.6k | 4.3us |
| `session` verify a signed cookie | 308.8k | 3.2us |
| `ratelimit` token bucket check | 19.77M | 51ns |
| `ratelimit` sliding window check | 18.58M | 54ns |
| `metrics` record a counter | 25.32M | 40ns |
| `metrics` scrape to Prometheus text | 53.9k | 18.5us |
| `lock` acquire+release | 14.85M | 67ns |
| `idem` claim a key | 2.92M | 342ns |
| `cluster` owner of a key, 5 nodes | 5.74M | 174ns |

## Concurrency

| lane | ops/s across every core |
|---|---|
| `cache` mixed | 18.92M |
| `ratelimit` checks | 8.10M |

A metrics scrape of a running process walks every series it holds; this one
had recorded a few hundred, and produced 12,641 bytes of Prometheus
text per scrape.

## What limits concurrency

The rate limiter is the one lane that does not scale with cores: every check
resolves its limiter by id through one registry lock, so the multi-threaded
lane measures that lock rather than the buckets. A benchmark that hid this
would be a benchmark that lied -- a sharded counter was the easy half of the
fix and is in; the registry lookup is the other half, and the honest number is
the table above. Resolving a limiter once at startup (a handle, the way
`cache x ttl T` resolves a name) is the change that would remove it.

The cache scales because its buckets are sharded and its counters are per
shard; 12 threads against 64 shards collide far less than 12 threads against
one lock.
