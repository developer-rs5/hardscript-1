#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root_dir"

hard_bin=${HARD_BIN:-target/release/hard}
if [ ! -x "$hard_bin" ]; then
    cargo build --release
fi

if [ ! -x "$hard_bin" ]; then
    echo "qa/run-extension-tests.sh: missing hard CLI at $hard_bin" >&2
    exit 1
fi

if ! command -v node >/dev/null 2>&1; then
    echo "qa/run-extension-tests.sh: node is required to run the debug adapter" >&2
    exit 1
fi

node --version

if [ -d vscode-hardscript/node_modules ]; then
    (cd vscode-hardscript && npm run check)
else
    echo "qa/run-extension-tests.sh: node_modules is absent, running the dependency-free checks directly"
    node vscode-hardscript/scripts/check.js
fi

python3 qa/extension/dap_smoke.py \
    --adapter vscode-hardscript/debugAdapter/hardDebugAdapter.js \
    --program examples/hello-world/main.hard \
    --hard "$hard_bin" \
    --output "${EXTENSION_QA_RESULTS:-qa/extension/results.json}"
