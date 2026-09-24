#!/usr/bin/env bash
# Incremental/parallel build benchmarks for the M3.3 report.
#
# For each module count in BENCH_NS (default 1 10 50 100 500 1000) it generates
# a chain project (main.hard imports m1..mN-1, m_k imports m_{k+1} and adds a
# calc) and measures, entry-point wall-clock with ms precision:
#   cold -j1, cold -j{JOBS}, warm, and incremental (touch the middle module).
# HARD_TIMED stage lines are captured for the -j1 and -j{JOBS} cold builds so
# the front-end parse stage (the parallelizable part) can be compared.
#
# Emits a TSV on stdout:
#   N  serial_ms  parallel_ms  warm_ms  incremental_ms  hits  misses  skipped
#   compiled  cache_entries  cache_size_bytes
# plus per-run "bench-stage" lines: N jobs stage=..ms parse=..ms merge=..ms
#   opt=..ms typecheck=..ms codegen=..ms native=..ms total=..ms
set -u
HARD="${HARD:-target/release/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
ROOT="${BENCH_ROOT:-/tmp/hs-bench}"
NS="${BENCH_NS:-1 10 50 100 500 1000}"
JOBS="${BENCH_JOBS:-12}"
TRIALS="${BENCH_TRIALS:-1}"

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

gen() {
    local n="$1" d="$2"
    rm -rf "$d"; mkdir -p "$d"
    local i
    for i in $(seq 1 $((n-1))); do
        printf 'calc v%d() => Int { <- %02d }\n' "$i" "$i" > "$d/m$i.hard"
    done
    if [ "$n" -eq 1 ]; then
        printf 'bring http\napp @3032\nGET "/" :: { <- 1 }\n' > "$d/main.hard"
    else
        # fanout: main imports every module directly (depth 1, width N); a
        # deep import chain would intentionally trip the 256-level guard.
        { for i in $(seq 1 $((n-1))); do echo "bring \"./m$i\""; done
          printf 'bring http\napp @3032\nGET "/" :: { <- v1() + v%d() }\n' "$((n-1))"; } > "$d/main.hard"
    fi
}

build() { # args: jobs -> wall ms + cache summary on stdout, stage lines to batch
    local jobs="$1"
    local start end wall
    start=$(now_ms)
    out=$(cd "$ROOT" && HARD_TIMED=1 "$HARD" build -j "$jobs" main.hard 2>&1)
    end=$(now_ms)
    wall=$((end-start))
    echo "$out"
    echo "$out" | grep -oE 'cache: .*' | sed 's/^/bench-cache /'
    echo "$out" | grep -oE 'time: .*' | sed 's/^/bench-stage /'
    echo "$wall"
}

printf 'N\tserial_ms\tparallel_ms\twarm_ms\tincremental_ms\thits\tmisses\tskipped\tcompiled\tentries\tcache_bytes\n'
PARENT="$ROOT"
for n in $NS; do
    ROOTP="$PARENT/n$n"
    gen "$n" "$ROOTP"
    ROOT="$ROOTP"

    # serial cold (best of TRIALS to damp noise on the small projects)
best=""
    for _ in $(seq 1 "$TRIALS"); do
        rm -rf "$ROOT/.hard"
        out=$(build 1)
        w=$(echo "$out" | grep '^[0-9][0-9]*$' | head -1)
        if [ -z "$best" ] || [ "$w" -lt "$best" ]; then best="$w"; fi
        echo "$out" | grep '^bench-stage' | sed "s/^/[$n serial] /"
    done
    serial="$best"

    # parallel cold
    best=""
    for _ in $(seq 1 "$TRIALS"); do
        rm -rf "$ROOT/.hard"
        out=$(build "$JOBS")
        w=$(echo "$out" | grep '^[0-9][0-9]*$' | head -1)
        if [ -z "$best" ] || [ "$w" -lt "$best" ]; then best="$w"; fi
        echo "$out" | grep '^bench-stage' | sed "s/^/[$n parallel] /"
        echo "$out" | grep '^bench-cache' | sed "s/^/[$n parallel] /"
    done
    parallel="$best"

    # warm (uses the parallel-built cache)
    rm -rf "$ROOT/.hard"
    build "$JOBS" >/dev/null 2>&1                # establish cache
    out=$(build "$JOBS")
    warm=$(echo "$out" | grep '^[0-9][0-9]*$' | head -1)
    echo "$out" | grep '^bench-cache' | sed "s/^/[$n warm] /"

    # incremental: touch the middle module, rebuild
    mid=$(( n/2 ))
    if [ "$n" -eq 1 ]; then mid=1; echo "// touch" >> "$ROOT/main.hard"; fi
    [ "$n" -ne 1 ] && echo "// touch" >> "$ROOT/m$mid.hard"
    out=$(build "$JOBS")
    incr=$(echo "$out" | grep '^[0-9][0-9]*$' | head -1)
    echo "$out" | grep '^bench-cache' | sed "s/^/[$n incremental] /"
    hits=$(echo "$out" | grep -oE '[0-9]+ hit' | grep -oE '[0-9]+' | head -1)
    misses=$(echo "$out" | grep -oE '[0-9]+ miss' | grep -oE '[0-9]+' | head -1)
    skipped=$(echo "$out" | grep -oE '[0-9]+ skipped' | grep -oE '[0-9]+' | head -1)
    compiled=$(echo "$out" | grep -oE '[0-9]+ compiled' | grep -oE '[0-9]+' | head -1)

    entries=$(python3 -c '
import json,os
c=0;s=0
for path,_,files in os.walk("'"$ROOT"'/.hard/cache/entries"):
    for f in files:
        if f=="meta.json":
            c+=1
            s+=os.path.getsize(os.path.join(path,f))
            if os.path.exists(os.path.join(path,"items.bin")):
                s+=os.path.getsize(os.path.join(path,"items.bin"))
print(f"{c} {s}")')
    ecount="${entries%% *}"; esize="${entries##* }"

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$n" "$serial" "$parallel" "$warm" "$incr" \
        "$hits" "$misses" "$skipped" "$compiled" "$ecount" "$esize"
done