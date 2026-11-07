#!/usr/bin/env bash
# Production browser matrices. No retries; keep each pass's failures/evidence.
set -euo pipefail
cd "$(dirname "$0")/.."
ulimit -n 65536
passes=${UI_BROWSER_PASSES:-1}
[[ "$passes" =~ ^[1-9][0-9]*$ ]] || { echo 'UI_BROWSER_PASSES must be a positive integer' >&2; exit 1; }
mkdir -p "${BROWSER_TEST_OUTPUT:-dist/browser-tests}"
output=$(cd "${BROWSER_TEST_OUTPUT:-dist/browser-tests}" && pwd)
for ((pass = 1; pass <= passes; pass++)); do
  for engine in ${BROWSER_ENGINES:-chromium webkit}; do
    [[ "$engine" == chromium || "$engine" == webkit ]] || { echo "Unknown engine: $engine" >&2; exit 1; }
    for suite in workspace-open motion comment-sheet phone-picker theme ring-render; do
      # LayerTree, tracing, screencasting and CPU throttling require Chromium CDP.
      if [[ "$suite" == ring-render && "$engine" != chromium ]]; then
        echo 'NOT APPLICABLE: ring-render requires Chromium CDP, not WebKit coverage'
        continue
      fi
      directory="$output/pass-$pass/$engine/$suite"
      mkdir -p "$directory"
      command=(node "crates/c6-ui/tests/$suite.cjs")
      [[ "$suite" != theme ]] || command+=(--browser)
      echo "RUN pass=$pass engine=$engine suite=$suite"
      ENGINE="$engine" SCREENSHOTS="$directory" EVIDENCE="$directory" \
        "${command[@]}" 2>&1 | tee "$directory/run.log"
      echo "PASS pass=$pass engine=$engine suite=$suite"
    done
  done
done
