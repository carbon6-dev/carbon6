#!/usr/bin/env bash
# No retries. Each invocation keeps its own logs, timings and failure evidence.
set -euo pipefail
cd "$(dirname "$0")/.."
ulimit -n 65536
exec node crates/c6-ui/tests/browser-runner.cjs "$@"
