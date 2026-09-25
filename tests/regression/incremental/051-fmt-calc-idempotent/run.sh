#!/usr/bin/env bash
# 051 fmt-calc-idempotent: `hard fmt` must keep `calc name(...)` function
# syntax (not rewrite it to the unparsable `calc fn name(...)`), and two
# consecutive formatting passes must be byte-identical.
set -u
HARD="${HARD:?HARD not set}"
TMP=$(mktemp -d /tmp/hs-inc-051-XXXXXX)
trap 'rm -rf "$TMP"' EXIT
cd "$TMP" || exit 1

printf 'calc add(Int a, Int b) => Int {\n    <- a + b\n}\n\ncalc greet(Str name) => Str {\n    <- "hi " + name\n}\n' > main.hard

"$HARD" fmt >/dev/null 2>&1 || { echo "051-fmt-calc-idempotent: first fmt failed"; exit 1; }
grep -q 'calc fn' main.hard && { echo "051-fmt-calc-idempotent: fmt emitted 'calc fn'"; exit 1; }
grep -q 'calc add(Int a, Int b)' main.hard || { echo "051-fmt-calc-idempotent: calc signature altered"; exit 1; }

cp main.hard first.hard
"$HARD" fmt >/dev/null 2>&1 || { echo "051-fmt-calc-idempotent: second fmt failed"; exit 1; }
cmp -s main.hard first.hard || { echo "051-fmt-calc-idempotent: fmt is not idempotent"; exit 1; }

"$HARD" fmt --check >/dev/null 2>&1 || { echo "051-fmt-calc-idempotent: fmt --check fails after formatting"; exit 1; }
"$HARD" build main.hard >/dev/null 2>&1 || { echo "051-fmt-calc-idempotent: formatted file does not build"; exit 1; }
echo "051-fmt-calc-idempotent: ok (calc functions survive fmt, idempotent)"
exit 0