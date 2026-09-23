#!/usr/bin/env bash
# Asan/UBSan build wrapper for `hard build`.
#
# Usage:     SAN_BASE_HARD=/abs/path/to/hard HARD=/abs/path/this/script tests/run-regressions.sh
#
# Delegates to the real `hard build`, then recompiles the generated .cpp in
# .hard/ with AddressSanitizer + UndefinedBehaviorSanitizer and overwrites the
# binary, so whichever harness invokes `hard build` subsequently runs a
# sanitizer instrumented server. Non-build invocations pass straight through.
set -u
BASE="${SAN_BASE_HARD:?SAN_BASE_HARD must point at the real hard binary}"
"$BASE" "$@"
rc=$?
if [ "$rc" -eq 0 ] && [ "${1:-}" = "build" ]; then
    tgt="${2:-main.hard}"
    stem="${tgt%.hard}"
    cpp=".hard/${stem}.cpp"
    bin=".hard/${stem}"
    if [ -f "$cpp" ] && [ -f "$bin" ]; then
        g++ -std=c++17 -pthread -I .hard \
            -fsanitize=address,undefined -fno-sanitize-recover=all -g -O1 \
            -fno-omit-frame-pointer "$cpp" -o "$bin" 2>/tmp/san-hard.err || { rc=2; }
    fi
fi
exit $rc