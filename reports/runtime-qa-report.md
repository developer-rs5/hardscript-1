# Cloud runtime QA report (M6.10)

Produced by `qa/run-runtime-qa.sh` on 2026-09-27 at `c787fa4` (plus the QA
commit itself), on 12 CPUs (AMD Ryzen 7 7445HS), 15253 MB RAM,
`g++ (Debian 15.3.0-2) 15.3.0`. 53 gates, all passing; any failure fails the
harness with a non-zero exit.

## Gates

| suite | result |
|---|---|
| `cargo test --workspace` | pass (276 compiler tests, all crates) |
| `tests/run-regressions.sh` | 57/57 pass |
| `tests/integration.sh` | 21 pass, 0 fail |
| `qa/phase3_snap.sh` | 16/16 pass (byte-identical codegen) |
| `qa/cli_matrix` | 72/72 lanes pass |
| `qa/run-runtime-tests.sh` | 33/33 suites pass (8 C++ fixtures, 8 gen, 8 fmt round trips, 9 diagnostics) |
| `qa/run-orm-tests.sh` | 24/24 suites pass (M5.3, unchanged) |
| `qa/run-framework-tests.sh` | 40/40 gates pass (M5.3, unchanged) |
| ASan + UBSan, all 8 runtime fixtures | clean |
| TSan, all 8 runtime fixtures | clean, no races reported |
| LSan, all 8 runtime fixtures | clean |
| `docs/errors.md` matches `hard errors --markdown` | pass |
| `docs/RUNTIME.md` present, examples compile | pass |

## Minimums

A test belongs to exactly one area: C++ fixtures by file, compiler unit tests
by name, generated expectations and diagnostics by fixture, CLI lanes by id.

| area | count | minimum | composition |
|---|---|---|---|
| cache | 109 | 80 | cache fixture 93, gen 14, CLI lanes 2 |
| queue | 117 | 95 | queue fixture 108, gen 7, CLI lanes 2 |
| scheduler | 96 | 75 | sched fixture 86, gen 10 |
| session | 92 | 70 | session fixture 83, gen 7, CLI lanes 2 |
| rate limiter | 67 | 55 | ratelimit fixture 63, gen 2, CLI lanes 2 |
| email | 128 | 100 | email fixture 118, gen 7, CLI lanes 3 |
| metrics and health | 118 | 95 | metrics fixture 107, gen 8, CLI lanes 3 |
| cluster, locks, idempotency | 173 | 140 | cluster fixture 162, gen 8, CLI lanes 3 |
| compiler, M6 surface | 25 | 20 | parser declarations and named options, formatter round trips |
| **total** | **925** | **710** | 820 of them in the eight C++ fixtures |

## Sanitizers

Each of `cache`, `queue`, `sched`, `session`, `ratelimit`, `email`, `metrics`
and `cluster` was built with `-fsanitize=address,undefined`, with
`-fsanitize=thread` and with `-fsanitize=leak`, and run to green. The socket
fixtures (`email` against a mock SMTP server, `cluster` against a mock HTTP
peer and the PostgreSQL wire server) run their threads under all three. A run
killed by the sandbox with no output at all is retried once; a real finding
reproduces every time.

## What the gate found

Four defects this milestone inherited or introduced, all fixed here and none of
them theoretical:

- **The formatter destroyed the M6 language.** `hard fmt` printed `cache
  users ttl 10m` back as `cache.declare("users", 600)`, `queue Send(u, delay =
  60)` as a raw call, `limit 100 requests / minute` as a desugared
  `ratelimit.check`, and `every 1h { .. }` lost its body while the paired
  `__sched_N` surfaced as a stray `calc`. All four now round trip, and eight new
  `runtime-fmt-*` suites format every fixture and rebuild the result, so it
  cannot regress silently.
- **A peer call could read past the socket.** The HTTP client appended its
  receive buffer as a C string, so a reply could pick up stack bytes after the
  last byte the peer sent. `append(buf, n)` now, and a short body is a
  truncated message rather than a half-parsed one.
- **A client dying of SIGPIPE.** The email and cluster clients wrote to their
  sockets without `MSG_NOSIGNAL`, so an SMTP or peer server that hung up
  mid-message killed the process instead of failing the send. The same bug made
  the ORM's mock PostgreSQL server a coin flip under ASan (it dropped clients on
  purpose); the framework gate caught it as an intermittent failure.
- **A rate limiter that scaled down.** Its allowed and denied totals were one
  process-wide atomic, and the benchmark found a twelve-core lane slower than
  one thread. The counters moved onto the shards a check already locks.

## Determinism

- `phase3_snap`: every codegen case rebuilds byte-identically.
- `runtime-fmt-*`: formatting is idempotent and the formatted source builds.
- Generated-code expectations (`*.exp`) are exact substrings of the compiler's
  own output, so a codegen change cannot quietly alter the emitted C++.
