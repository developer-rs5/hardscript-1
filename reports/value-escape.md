# ms2.5 — Escape Analysis Foundation

Compiler-only milestone: **no runtime or codegen output changes**. Every local
binding in a function-like scope is classified into one of three buckets and
surfaced through a report, so a later milestone can stack-allocate only the
provably-safe ones.

## What was delivered

A new pass `compiler/src/escape.rs`, wired into the front-end pipeline and
exposed as `hs_compiler::escape_report(&src, path)` plus a CLI report mode:

    HARD_ESCAPE_REPORT=1 hard build app.hard

prints one line per classified binding and a `summary:` line, instead of the
normal `built ...` banner. The generated C++ is byte-identical with or without
the report (phase3 snapshots were untouched — 16/16, zero refreshes).

### Classes

| class | rule | meaning |
|---|---|---|
| `stack` | never used in an escaping position, declares + reads only | safe candidate for future stack allocation |
| `escaping` | returned, stored into a list/object, passed to a call (callee or argument), sent in an HTTP payload, launched in a `race` task, or rebound | must stay frame/heap-owned |
| `immutable` | a `const` binding declared exactly once | safe to share / inline |

The analysis is deliberately conservative: it only moves *down* to `stack`, and
a binding involved in any containing position is marked escaping. Positions
that merely *read* a value (arithmetic operands, comparisons, `match`
scrutinee/patterns, loop ranges, index bases, member reads) never escape.

### Scopes

Routes (`route GET /`), socket events (`ws /chat message`), `calc` functions
(`func name`), middleware (`middleware name`), tests (`test name`), and
top-level `<-`/`::=` bindings (`top`). Loop variables are tracked as stack
candidates too. Function parameters are not reported (they arrive escaped).

## Verification

- **5 Rust unit tests** (`cargo test -p hs-compiler`) covering: scalar reads
  stay stack, containers withdraw into escaping, race tasks escape, returned
  bindings escape (even `const` — conservative), member reads keep the base
  stack while the derived value escapes.
- **Regressions 033–035** (new `esc` kind in the harness) pin exact report
  output: 033 basic classification, 034 container withdrawal, 035 multi-scope
  (func + middleware + route). Build must also be g++-clean.
- Full regression 35/35, integration 21/21, phase3 snapshots 16/16, fmt 5/5,
  docs writes API.md, release bench smoke (3.41 s build, 149648 B,
  `{"ok":true}`), ASan/UBSan/LSan clean (regressions under sanitizers +
  rest-api clean-exit probe).

## Notes for the request-path milestone

- The phase-3 snapshot gate stayed byte-identical because the pass is
  analysis-only. Wiring it into codegen decisions is ms2.6 work.
- Pre-existing codegen gap surfaced while designing the race fixture: a `race`
  arm that closes over a *local* generates a lambda without a capture list
  (`[]() { return keep; }` fails to compile). The escape analysis is the
  correct place to later emit the capture list — the referenced locals are
  already enumerated in `in_race`/escaping sets.