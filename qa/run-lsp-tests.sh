#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root_dir"

cargo build -p hs-lsp

server_binary=${HARD_LSP_BIN:-target/debug/hs-lsp}
if [ ! -x "$server_binary" ]; then
    echo "qa/run-lsp-tests.sh: missing server binary $server_binary" >&2
    exit 1
fi

python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' || {
    echo "qa/run-lsp-tests.sh: python3 >= 3.8 is required" >&2
    exit 1
}

python3 qa/lsp/run.py --binary "$server_binary" --output "${LSP_QA_RESULTS:-qa/lsp/results.json}"
