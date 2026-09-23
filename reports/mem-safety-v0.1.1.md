# v0.1.1 Memory Safety Audit (Milestone 1)

- Date: 2026-09-23
- Host: linux x86_64, 12 vCPU
- Toolchain: rustc 1.97.1, g++ 15.3.0
- Sanitizers: AddressSanitizer + UndefinedBehaviorSanitizer (g++ `-fsanitize=address,undefined`),
  LeakSanitizer via `ASAN_OPTIONS=detect_leaks=1`
- Flags: `-fno-sanitize-recover=all -g -O1 -fno-omit-frame-pointer`
- Options: `ASAN_OPTIONS=detect_leaks=1:halt_on_error=1:abort_on_error=1`,
  `LSAN_OPTIONS=suppressions=qa/phase45/lsan.supp`, `UBSAN_OPTIONS=halt_on_error=1`

## What was exercised under the sanitizers

Every application was compiled to C++ with `hard build`, then recompiled with the
sanitizer flags above and executed with halt-on-error semantics (any finding aborts
the run and fails that suite).

| Suite | Coverage | Result |
| --- | --- | --- |
| Phase 4 runtime torture (sanitized binary) | 100 routes, params, 404s, fs roundtrip (4 MB file), crypto vectors vs Python, jwt sign/verify/tamper, 1 MB echo, malformed-JSON→500, 50×20 (1000) concurrent requests, 500 concurrent WebSocket clients all receiving their broadcast echo, then clean shutdown | PASS (no sanitizer findings) |
| Regression suite 001–007 (sanitized binaries) | parse-error fixtures, global scope, calc roundtrip, crypto vectors, WS room lifecycle, long-base64 decode | PASS (7/7, no findings) |
| Integration suite (21 checks; apps then probed under sanitation) | build/test/fmt/docs/bench + live GET/POST | PASS |
| Example live probes (sanitized binaries) | rest-api (GET/POST /users), auth (POST /register), chat (WS handshake + ping→pong), each terminated with SIGTERM for a clean-exit LSan run | PASS (exit rc=0, no findings) |

## Findings

### BUG 10 — signed left-shift UB in `base64_decode` (found by UBSan)

`runtime/hs_runtime_crypto.hpp:113` accumulated sextets into a **signed `int`** as
`v = (v << 6) | d` and never masked off consumed bits. Once the accumulated value
exceeded 2^25, `v << 6` overflowed → undefined behavior. UBSan halted the server:

```
runtime error: left shift of 516463837 by 6 places cannot be represented in type 'int'
```

Trigger: any sufficiently long base64 payload. In the torture app this fired on
`jwt.verify` of a valid JWT (payload segment long enough); short strings
(`bad.header.badsig`) stayed under the threshold, which is why the basic paths
were green. Inputs long enough to push `v` past 32 bits would also silently
corrupt output.

Fix: accumulate into `uint32_t` and mask to 26 bits per step
`v = ((v << 6) | d) & 0x3ffffff`. Verified byte-identical output vs. the old
decoder across lengths 0–100000 bytes (random content) and vs. the prior vectors.
Regression: `tests/regression/007-b64-long-u32` (2600 → base64 → decode → exact
echo). **No other signed-shift/overflow sites remain** (sha/md5/hmac/JWT use
`uint32_t`; JSON `\u` uses bounded `unsigned`; postgres byte-swap uses `uint32_t`).

### Release hardening — graceful shutdown (prerequisite for leak measurement)

The server had no signal handling; SIGTERM hard-killed the process, so LeakSanitizer
never got an exit hook and detached worker threads could be cut off mid-handling.
Added a minimal `g_hs_stop` flag + connection counter to `hs_runtime_http.hpp`:

- SIGINT/SIGTERM set `g_hs_stop`; the accept loop breaks out on `EINTR`/flag.
- The listener drains in-flight connections (bounded 5 s wait) and `main()` returns
  normally, so C++ destructors and the leak checker run.
- `Server::listen()` sits at the only exit point; behavior is otherwise unchanged.
- All suites now assert `server exit rc=0` after a SIGTERM (previously non-zero).

## Report

- **Leaks:** none. LSan clean on exit for every sanitized server under a suppression
  list for five intentional process-lifetime singletons (`ws_registry`, `ws_clients`,
  `ws_rooms`, `ws_client_room`, `test_registry` — function-local static maps/vectors
  never individually freed; not per-request allocations).
- **Use-after-free:** none found.
- **Double-free:** none found.
- **Undefined behavior:** 1 finding (BUG 10, above) — fixed and regression-covered.
- **Conclusion:** v0.1.1 runtime and generated server programs pass AddressSanitizer,
  UndefinedBehaviorSanitizer and LeakSanitizer across the full torture matrix,
  regression suite, integration suite and example apps.

## Re-run

```
bash qa/phase45_mem_safety.sh     # full audit (exit 0 = pass)
```