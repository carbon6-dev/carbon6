#!/usr/bin/env bash
# Runs the test suite while every CPU core is kept busy by other work, the
# way a shared CI runner can be (docs/testing.md). A test that fails here
# and not otherwise waits on time instead of on what it checks: a bug.
#
#   test-under-load.sh [runs (default 1)] [cargo test arguments…]
set -euo pipefail
runs=${1:-1}
shift || true
cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu)
cargo test --locked --no-run "$@" >/dev/null
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
    cargo test --locked "$@"
done
