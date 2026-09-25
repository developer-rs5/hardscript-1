#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root_dir"

cargo build --release -p hs-lsp

server_binary=${HARD_LSP_BIN:-target/release/hs-lsp}
if [ ! -x "$server_binary" ]; then
    echo "qa/run-lsp-perf.sh: missing server binary $server_binary" >&2
    exit 1
fi

python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' || {
    echo "qa/run-lsp-perf.sh: python3 >= 3.8 is required" >&2
    exit 1
}

python3 qa/lsp/semantic_scan.py --binary "$server_binary" --output "${LSP_SEMANTIC_OUTPUT:-qa/lsp/semantic-tokens.json}"
python3 qa/lsp/index_scan.py --binary "$server_binary" --output "${LSP_INDEX_OUTPUT:-qa/lsp/index-scan.json}"
python3 qa/lsp/perf_scan.py --binary "$server_binary" --output "${LSP_PERF_OUTPUT:-qa/lsp/perf.json}"
