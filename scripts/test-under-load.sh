#!/usr/bin/env bash
# Runs the test suite while every CPU core is kept busy by other work, the
# way a shared CI runner can be (docs/testing.md). A test that fails here
# and not otherwise waits on time instead of on what it checks: a bug.
#
#   test-under-load.sh [runs (default 1)] [test arguments…]
#
# NEXTEST=1 runs the suite with cargo nextest (every test its own process)
# instead of cargo test.
set -euo pipefail
runs=${1:-1}
shift || true
cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)
if [ "${NEXTEST:-}" = 1 ]; then
    run() { cargo nextest run --locked --workspace --no-fail-fast "$@"; }
    cargo nextest run --locked --workspace --no-run "$@" >/dev/null
else
    run() { cargo test --locked "$@"; }
    cargo test --locked --no-run "$@" >/dev/null
fi
burners=()
cleanup() { [ ${#burners[@]} -eq 0 ] || kill "${burners[@]}" 2>/dev/null || true; }
trap cleanup EXIT
for _ in $(seq "$cores"); do
    (while :; do :; done) &
    burners+=($!)
done
echo "test-under-load: $cores cores busy, $runs run(s)"
for r in $(seq "$runs"); do
    echo "test-under-load: run $r"
    run "$@"
done
