# Cache performance (2026-09-27)

Measured with `qa/bench_runtime.sh` at `b84fba9` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`. Median of 3 runs; every cell below comes from a run, and the
script fails instead of printing a number it did not measure.

Method: the in-process cache, one thread unless a lane says otherwise. The
working set is 10,000 keys so hits spread over all 64 shards, and a miss lane
probes keys that were never written, so the sharded-miss path is measured
rather than assumed. `expired_read` reads entries that are already past their
TTL: the lazy path drops them on the way out.

## Single-thread lanes (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
| `get_hit` | 8.62M | 116ns |
| `get_miss` | 16.22M | 62ns |
| `set` | 10.26M | 97ns |
| `incr` | 25.79M | 39ns |
| `exists` | 12.24M | 82ns |
| `expired_read` | 31.68M | 32ns |

## Concurrency (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
| `mixed` on 1 thread | 9.83M | 102ns |
| `mixed` on 12 threads | 18.92M | 53ns |

The target for this milestone was 100,000 mixed ops/s. One thread does
9.83M; every core together does 18.92M.

## Memory

| measure | value |
|---|---|
| live entries after 200,000 inserts | 200,000 |
| inserts/s while filling | 1.49M |
| peak RSS | 176.0 MB |
| heap held with all 200,000 live | 116.5 MB |
| heap per entry | 611 bytes |

Measured while the entries were still live, not after the cache was destroyed:
the heap a filled cache costs is the number a box gets sized by, and an empty
process says nothing. The peak RSS also covers the fill itself.
