# Scheduler performance (2026-09-27)

Measured with `qa/bench_runtime.sh` at `b84fba9` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M Graphics), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`. Median of 3 runs; every cell below comes from a run, and the
script fails instead of printing a number it did not measure.

Method: the next/prev fire arithmetic on its own, which is where a scheduler
spends its time -- a timer that is not due costs nothing but a comparison. The
input times come from a linear congruential walk, not `now + i`: a linear walk
is an induction variable the compiler closes in a multiply, and the lane then
measures nothing at all. Every lane sums what it computed and prints the sum,
so the work cannot be folded away.

`next_daily_dst_pair` asks for the same wall time either side of a DST change
(UTC-5 and UTC-4), which is where local-time arithmetic usually goes wrong.

## Fire arithmetic (ops/s, mean ns/op)

| lane | ops/s | mean ns/op |
|---|---|---|
| `next_interval` | 795.54M | 1ns |
| `next_daily_utc` | 17.32M | 58ns |
| `next_daily_tz+0530` | 23.80M | 42ns |
| `next_weekly` | 23.12M | 43ns |
| `prev_daily` | 45.37M | 22ns |
| `next_daily_dst_pair` | 12.28M | 81ns |

The dispatch budget for this milestone was 5us per decision. The most
expensive form, a weekly wall-clock time in a zoned zone, costs
43ns.

`next_interval` is the floor of the lane set rather than a measurement of
interest: an interval is one addition, so what the loop actually costs is the
clock walk feeding it. It is here to show the decision is free next to the
time it takes to ask the question.

## Registration (timers/s, mean us/timer)

| lane | timers/s | mean us/timer |
|---|---|---|
| `register` | 1.53M | 653ns |
| `register` on 4 threads | 921.9k | 1.1us |

## Firing

| measure | value |
|---|---|
| firings observed | 1 |
| mean interval including the 100ms sleep slices | 1,000.8ms |

A one-second timer fires once a second by definition, so this lane measures
what is actually interesting: the scheduler thread notices a due timer within
its sleep slice rather than sleeping through it.
