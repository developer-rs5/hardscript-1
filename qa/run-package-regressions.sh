#!/usr/bin/env bash
# The documented entry point for the package-manager suite.
#
# The suite itself lives with the other regression scripts, because that is
# where it has always been and where the other suites look for it; this wrapper
# exists so `qa/run-package-regressions.sh` works as the release checklist
# says it should.
set -euo pipefail
QA_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$QA_ROOT/.." && pwd)"
exec bash "$REPO/tests/run-package-regressions.sh" "$@"
