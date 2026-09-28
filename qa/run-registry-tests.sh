#!/usr/bin/env bash
# The v0.9 registry QA gate.
#
# Runs every registry suite, counts the tests per area, and fails if an area
# falls below its floor. The counts come from each suite's own output, so this
# script cannot claim coverage the suites did not produce.
#
#   qa/run-registry-tests.sh              # everything
#   BENCH_QA_SKIP_SANITIZERS=1 qa/run-registry-tests.sh
#   BENCH_QA_AREAS="signatures mirrors" qa/run-registry-tests.sh
set -uo pipefail

QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$QA_ROOT/.." && pwd)"
cd "$REPO"

LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hard-qa.XXXXXX")"
OUT="$LOG_DIR/qa-report.txt"
: > "$OUT"

AREAS="${BENCH_QA_AREAS:-publish download auth search resolver signatures mirrors cache}"
WANT_UNIT=1
[ "${BENCH_QA_SKIP_UNIT:-0}" = "1" ] && WANT_UNIT=0
WANT_SANITIZERS=1
[ "${BENCH_QA_SKIP_SANITIZERS:-0}" = "1" ] && WANT_SANITIZERS=0

# area -> suite script
suite_for() {
    case "$1" in
        publish)    echo "tests/run-publish-regressions.sh" ;;
        download)   echo "tests/run-download-regressions.sh" ;;
        auth)       echo "tests/run-auth-regressions.sh" ;;
        search)     echo "tests/run-search-regressions.sh" ;;
        resolver)   echo "tests/run-resolver-regressions.sh" ;;
        signatures) echo "tests/run-signature-regressions.sh" ;;
        mirrors)    echo "tests/run-mirror-regressions.sh" ;;
        cache)      echo "tests/run-cache-regressions.sh" ;;
        package)    echo "tests/run-package-regressions.sh" ;;
        core)       echo "tests/run-regressions.sh" ;;
        *)          echo "" ;;
    esac
}

# The number of checks a suite reported, read from its own summary line.
count_of() {
    local log="$1" pattern="$2"
    python3 - "$log" "$pattern" <<'PY'
import re, sys
text = open(sys.argv[1], errors="replace").read()
matches = re.findall(sys.argv[2], text, re.M)
print(len(matches))
PY
}

have_area() { case " $AREAS " in *" $1 "*) return 0;; *) return 1;; esac; }

say() { printf '%s\n' "$*" | tee -a "$OUT"; }

FAILED_AREAS=""
TOTAL_CHECKS=0
declare -A AREA_CHECKS=()
declare -A AREA_ELAPSED=()
declare -A AREA_LOG=()

say "# v0.9-alpha registry QA"
say
say "run at $(date -u '+%Y-%m-%d %H:%M:%S UTC') on $(uname -sr)"
say
say "Each suite runs on its own ports with its own throwaway registry and"
say "HARD_HOME, so nothing here depends on state left by a previous suite."
say

# --------------------------------------------------------------- cargo tests
if [ "$WANT_UNIT" = "1" ]; then
    say "## unit and integration tests"
    say
    if cargo test --workspace >"$LOG_DIR/cargo.log" 2>&1; then
        passed=$(grep -oE "^test result: ok\. [0-9]+" "$LOG_DIR/cargo.log" | grep -oE "[0-9]+$" | paste -sd+ | bc)
        say "cargo test --workspace: **$passed passed, 0 failed**"
    else
        say "cargo test --workspace: **FAILED** (see below)"
        grep -E "^(error|test result: FAILED|---- .* stdout)" "$LOG_DIR/cargo.log" | head -20 | tee -a "$OUT"
        FAILED_AREAS="$FAILED_AREAS unit"
    fi
    say
    AREA_CHECKS[unit]=$passed
    TOTAL_CHECKS=$((TOTAL_CHECKS + passed))
    AREA_LOG[unit]="$LOG_DIR/cargo.log"
fi

# ------------------------------------------------------------------- suites
say "## registry suites"
say
say "| area | suite | checks | minimum | result |"
say "| --- | --- | ---: | ---: | --- |"

minimum_for() {
    case "$1" in
        publish)    echo 40 ;;
        download)   echo 40 ;;
        auth)       echo 30 ;;
        search)     echo 30 ;;
        resolver)   echo 40 ;;
        signatures) echo 70 ;;
        mirrors)    echo 60 ;;
        cache)      echo 30 ;;
        package)    echo 1 ;;
        core)       echo 1 ;;
        *)          echo 1 ;;
    esac
}

pattern_for() {
    case "$1" in
        publish)    echo 'PASS [a-z0-9-]+' ;;
        download)   echo 'PASS [a-z0-9-]+' ;;
        auth)       echo 'PASS [a-z0-9-]+' ;;
        search)     echo '^  ok [0-9]+ - ' ;;
        resolver)   echo 'PASS [a-z0-9-]+' ;;
        signatures) echo '^  ok [0-9]+ - ' ;;
        mirrors)    echo '^  ok [0-9]+ - ' ;;
        cache)      echo '^  ok [0-9]+ - ' ;;
        package)    echo 'PASS [a-z0-9-]+' ;;
        core)       echo 'PASS [a-z0-9-]+' ;;
        *)          echo 'PASS' ;;
    esac
}

for area in $AREAS; do
    script="$(suite_for "$area")"
    if [ -z "$script" ]; then
        say "| $area | (unknown area) | 0 | - | skipped |"
        continue
    fi
    [ -f "$script" ] || { say "| $area | $script | 0 | - | missing |"; FAILED_AREAS="$FAILED_AREAS $area"; continue; }
    log="$LOG_DIR/${area}.log"
    start=$(date +%s)
    if bash "$script" >"$log" 2>&1; then
        status="pass"
    else
        status="FAIL"
        FAILED_AREAS="$FAILED_AREAS $area"
    fi
    elapsed=$(( $(date +%s) - start ))
    checks=$(count_of "$log" "$(pattern_for "$area")")
    min=$(minimum_for "$area")
    AREA_CHECKS[$area]=$checks
    AREA_ELAPSED[$area]=$elapsed
    AREA_LOG[$area]=$log
    TOTAL_CHECKS=$((TOTAL_CHECKS + checks))
    if [ "$status" = "pass" ] && [ "$checks" -lt "$min" ]; then
        status="pass but short of $min"
        FAILED_AREAS="$FAILED_AREAS $area"
    fi
    say "| $area | \`$script\` | $checks | $min | $status (${elapsed}s) |"
done

say
say "**$TOTAL_CHECKS checks in total.**"
say

# ---------------------------------------------------------------- sanitizers
if [ "$WANT_SANITIZERS" = "1" ]; then
    say "## sanitizers"
    say
    if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        say "nightly is not installed, so the Rust sanitizers could not run."
        say "Install it with \`rustup toolchain install nightly --profile minimal\`"
        say "and re-run this gate. Skipped, not passed."
    else
        say "| check | how | result |"
        say "| --- | --- | --- |"
        # Rust has no -Zsanitizer=undefined: the list is address, cfi, dataflow,
        # hwaddress, memtag, safestack, shadow-call-stack and thread. Undefined
        # behaviour is covered by Miri instead, which is run below, and both
        # facts are reported rather than a "UBSan: pass" that never happened.
        run_sanitizer() {   # $1 = name, $2 = flag, $3.. = extra cargo args
            local name="$1" flag="$2"
            shift 2
            local log="$LOG_DIR/san-$name.log"
            local start elapsed passed
            start=$(date +%s)
            # A target directory per sanitizer: `-Zsanitizer` changes the ABI,
            # so artifacts from one sanitizer cannot be linked with another.
            # `--lib --bins --tests`: doc-tests are compiled without RUSTFLAGS,
            # so a sanitizer-built library cannot be linked into them and the
            # run would fail on an ABI mismatch rather than on a finding.
            if RUSTFLAGS="-Zsanitizer=$flag" \
               CARGO_TARGET_DIR="target/qa-sanitizer-$name" \
               cargo +nightly test --lib --bins --tests --target x86_64-unknown-linux-gnu "$@" \
               >"$log" 2>&1; then
                elapsed=$(( $(date +%s) - start ))
                passed=$(grep -oE "^test result: ok\. [0-9]+" "$log" | grep -oE "[0-9]+$" | paste -sd+ | bc)
                say "| $name | \`-Zsanitizer=$flag\` | pass: $passed tests in ${elapsed}s |"
                AREA_CHECKS[sanitizer-$name]=$passed
                TOTAL_CHECKS=$((TOTAL_CHECKS + passed))
            else
                elapsed=$(( $(date +%s) - start ))
                say "| $name | \`-Zsanitizer=$flag\` | **FAILED** after ${elapsed}s |"
                grep -E "AddressSanitizer|LeakSanitizer|ThreadSanitizer|runtime error|SUMMARY:|^error" "$log" \
                    | head -10 | tee -a "$OUT"
                FAILED_AREAS="$FAILED_AREAS sanitizer-$name"
                AREA_LOG[sanitizer-$name]=$log
            fi
        }
        run_sanitizer asan address --workspace
        run_sanitizer lsan leak --workspace
        # TSan needs a sanitizer-aware standard library; without -Zbuild-std the
        # ABI of prebuilt dependencies does not match.
        if rustup component list --toolchain nightly 2>/dev/null | grep -q 'rust-src (installed)'; then
            run_sanitizer tsan thread -Zbuild-std --workspace
        else
            say "| tsan | \`-Zsanitizer=thread\` | skipped: \`rust-src\` is not installed, and TSan needs \`-Zbuild-std\` |"
        fi
        # Miri is the undefined-behaviour check available in Rust.
        MIRI_LOG="$LOG_DIR/miri.log"
        if cargo +nightly miri --version >/dev/null 2>&1; then
            start=$(date +%s)
            # Miri intercepts the filesystem by default, so the runs use
            # -Zmiri-disable-isolation: these tests touch temporary files, and
            # the point of the run is memory safety, not syscall emulation.
            # The modules are the pure logic: semver ordering, the TOML parser,
            # and the signature payload and trust store.
            export MIRIFLAGS="-Zmiri-disable-isolation"
            if cargo +nightly miri test -p hs-pm semver:: >"$MIRI_LOG" 2>&1 \
               && cargo +nightly miri test -p hs-pm toml:: >>"$MIRI_LOG" 2>&1 \
               && cargo +nightly miri test -p hs-pm verify:: >>"$MIRI_LOG" 2>&1; then
                passed=$(grep -oE "^test result: ok\. [0-9]+" "$MIRI_LOG" | grep -oE "[0-9]+$" | paste -sd+ | bc)
                say "| miri (undefined behaviour) | \`cargo +nightly miri test\` on semver, toml, verify | pass: $passed tests in $(( $(date +%s) - start ))s |"
                AREA_CHECKS[miri]=$passed
                TOTAL_CHECKS=$((TOTAL_CHECKS + passed))
            else
                say "| miri (undefined behaviour) | \`cargo +nightly miri test\` | **FAILED** |"
                grep -E "error:|Undefined Behavior" "$MIRI_LOG" | head -10 | tee -a "$OUT"
                FAILED_AREAS="$FAILED_AREAS miri"
                AREA_LOG[miri]="$MIRI_LOG"
            fi
        else
            say "| miri (undefined behaviour) | \`cargo +nightly miri\` | skipped: \`rustup component add miri --toolchain nightly\` |"
        fi
        say
        say "UBSan is not a thing in Rust: \`-Zsanitizer\` has no \`undefined\` value."
        say "The Miri row above is the undefined-behaviour check, and it is reported as"
        say "Miri rather than dressed up as a UBSan pass."
        say
    fi
fi

# ------------------------------------------------------------------- verdict
say "## verdict"
say
if [ -z "$FAILED_AREAS" ]; then
    say "**PASS** — every area met its minimum and no suite failed."
else
    say "**FAIL** — the following did not pass or fell short: $FAILED_AREAS"
    for area in $FAILED_AREAS; do
        log="${AREA_LOG[$area]:-}"
        if [ -n "$log" ] && [ -f "$log" ]; then
            say
            say "### $area"
            grep -E "NOT OK|^.*: FAIL|error:|panicked at" "$log" | head -20 | tee -a "$OUT"
        fi
    done
fi
say
say "Logs: $LOG_DIR"

REPORT="${BENCH_QA_REPORT:-$REPO/reports/registry-qa-report.md}"
if [ -n "$REPORT" ]; then
    {
        echo "# Registry QA report"
        echo
        echo "Generated by \`qa/run-registry-tests.sh\`; the machine-readable form of this run"
        echo "is in \`reports/qa-run.txt\`. Re-run the gate to regenerate both."
        echo
        cat "$OUT"
    } > "$REPORT"
    echo "wrote $REPORT"
fi
cp "$OUT" "$REPO/reports/qa-run.txt" 2>/dev/null || true

if [ -n "$FAILED_AREAS" ]; then
    exit 1
fi
exit 0
