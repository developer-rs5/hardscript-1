#!/usr/bin/env bash
# Diagnostics V2 snapshot suite (M3.4.7).
#
# For every case in qa/diag_cases/<NN>-<name>/:
#   1. copy the case into a fresh temp project root (so paths stay `main.hard`)
#   2. run `hard <invoke>` with NO_COLOR=1, capturing stderr only
#   3. compare stderr byte-for-byte against qa/diag_snapshots/<name>.txt;
#      missing snapshots are recorded as goldens (report "RECORD")
#
# Usage:
#   qa/diag_snap.sh            verify mode (enforce goldens)
#   qa/diag_snap.sh record     re-record every golden from current output
#
# Every case is curated to be deterministic: stderr only, no g++/HS0502
# (native output embeds build paths), fixed cwd and module names.
set -u
HARD="${HARD:-target/release/hard}"
HARD="$(cd "$(dirname "$HARD")" 2>/dev/null && pwd)/$(basename "$HARD")"
QA="$(cd "$(dirname "$0")" && pwd)"
CASES="$QA/diag_cases"
SNAPS="$QA/diag_snapshots"
MODE="${1:-verify}"
mkdir -p "$SNAPS"

FAILED=0
TOTAL=0
FRESH=0
RECORDED=0

if [ "$MODE" = "record" ]; then
    rm -f "$SNAPS"/*.txt
fi

for dir in "$CASES"/*/*/; do
    name="$(basename "$dir")"
    has_invoke=0
    [ -f "$dir/invoke" ] && has_invoke=1
    [ "$has_invoke" -eq 0 ] && [ ! -f "$dir/main.hard" ] && continue
    TOTAL=$((TOTAL+1))
    TMP="/tmp/hs-diag-$name"
    rm -rf "$TMP"; mkdir -p "$TMP"
    cp -r "$dir/." "$TMP/proj/"
    invoke=build
    [ -f "$TMP/proj/invoke" ] && invoke="$(cat "$TMP/proj/invoke")"
    (cd "$TMP/proj" && NO_COLOR=1 "$HARD" $invoke >/dev/null 2>out.stderr)
    if [ -f "$SNAPS/$name.txt" ]; then
        if cmp -s "$TMP/proj/out.stderr" "$SNAPS/$name.txt"; then
            FRESH=$((FRESH+1))
        else
            echo "diagsnap: DIFF  $name"
            FAILED=$((FAILED+1))
        fi
    else
        cp "$TMP/proj/out.stderr" "$SNAPS/$name.txt"
        echo "diagsnap: RECORD $name"
        RECORDED=$((RECORDED+1))
    fi
done

echo "diagsnap: ${TOTAL} cases, ${FRESH} match, ${RECORDED} recorded, ${FAILED} differ"
if [ "$FAILED" -ne 0 ]; then
    echo "rerun with \`qa/diag_snap.sh record\` after reviewing the diffs"
    exit 1
fi
exit 0