# Queue performance (2026-09-27)

Measured with `qa/bench_runtime.sh` at `b84fba9` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`. Median of 3 runs; every cell below comes from a run, and the
script fails instead of printing a number it did not measure.

Method: the memory backend directly (a program pays for the round trip, not for
a worker thread), plus SQLite in memory for the durable path. The clock is
frozen, so a delayed job becomes ready when the lane says so rather than when
the wall clock gets there. `fail_to_dlq` drives a job through all five
attempts into the dead-letter queue, moving the clock to each backoff.

`enqueue_poll_complete` is the number a request handler pays: enqueue, claim,
complete. `depth` is the queue's own counter on an empty queue, which is what a
health check asks.

## Memory backend (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
| `enqueue` | 1.95M | 512ns |
| `enqueue_payload_1kb` | 852.3k | 1.2us |
| `poll_priority` | 3.30M | 303ns |
| `enqueue_poll_complete` | 3.16M | 316ns |
| `fail_to_dlq` | 561.5k | 1.8us |
| `depth` | 189.39M | 5ns |

## SQLite (ops/s, mean us/op)

| lane | ops/s | mean us/op |
|---|---|---|
| `enqueue_poll_complete` | 40.8k | 24.5us |

`recover_after_restart` is one operation, not a rate: it claims a job, never
completes it, closes the database and reopens it, and takes 282.0us.
The memory backend cannot do that at all, which is the whole reason the SQL
one exists.

## Workers

| lane | jobs/s | mean us/job |
|---|---|---|
| `worker_drain` on 12 threads | 3.79M | 264ns |

A worker pool is configured by a program (`worker Name { .. }` with a
concurrency), and this lane runs the runtime's own pool on every core.

Peak RSS for the program: 174.6 MB (the enqueue lanes hold half a million
jobs before draining them).
