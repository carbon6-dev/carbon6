#!/usr/bin/env bash
# Rejects the patterns that make tests fail for reasons other than what
# they check (docs/testing.md). A line can opt out with a comment saying
# why: `// test-lint: allow (<reason>)`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0
# Hits (file:line:text) not allowed on their line or the line above. (No
# `case` inside `$(…)`: macOS's bash 3.2 can't parse it.)
unallowed() {
    while IFS=: read -r file line rest; do
        if printf '%s' "$rest" | grep -q 'test-lint: allow'; then continue; fi
        if [ "$line" -gt 1 ] && sed -n "$((line - 1))p" "$file" | grep -q 'test-lint: allow'; then continue; fi
        printf '%s:%s:%s\n' "$file" "$line" "$rest"
    done
}
check() {
    local what=$1 pattern=$2; shift 2
    local hits
    # Allowed on the line itself, or by a comment on the line above.
    hits=$(git grep -nE "$pattern" -- "$@" | unallowed || true)
    if [ -n "$hits" ]; then
        printf 'test-lint: %s (docs/testing.md):\n%s\n\n' "$what" "$hits" >&2
        fail=1
    fi
}

tests=('crates/*/tests/**/*.rs' 'bins/*/tests/**/*.rs' 'crates/*/tests/*.rs' 'bins/*/tests/*.rs')
# Tests that share a process: integration binaries with many tests, and
# every crate's unit tests.
shared=('crates/*/tests/it/*.rs' 'crates/*/src/**/*.rs' 'bins/*/src/*.rs')

check "wait with c6_testkit::eventually, not a copy of it" 'fn (eventually|until)<' "${tests[@]}" 'crates/*/src/**/*.rs' ':!crates/c6-testkit'
check "a fixed timeout fails a slow machine: wait with c6_testkit::within" 'tokio::time::timeout\(' "${tests[@]}"
check "a fixed deadline fails a slow machine: use c6_testkit::wait::deadline()" 'Instant::now\(\) \+ (std::time::)?Duration::from_(secs|millis)' "${tests[@]}"
check "tests in a shared process must not change its environment: set it on what they test" 'std::env::(set_var|remove_var)\(' "${shared[@]}"
check "a port picked and released can be taken by another test: use c6_testkit::port()" 'fn free_port' "${tests[@]}"

# A code block in a doc comment is a doctest unless it names another
# language: CI's nextest never runs doctests (and the workspace has none), so
# one would sit there untested. Tag examples `text`, `sh`, `json`, `toml`…
doctests=$(git ls-files 'crates/*.rs' 'bins/*.rs' | xargs awk '
    FNR == 1 { inside = 0 }
    /^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]*```/ {
        tag = $0
        sub(/^[[:space:]]*(\/\/\/|\/\/!)[[:space:]]*```/, "", tag)
        gsub(/[[:space:]]/, "", tag)
        if (!inside) {
            inside = 1
            if (tag == "" || tag ~ /(^|,)(rust|ignore|no_run|should_panic|compile_fail|edition[0-9]+)(,|$)/)
                print FILENAME ":" FNR ":" $0
        } else {
            inside = 0
        }
    }' | unallowed || true)
if [ -n "$doctests" ]; then
    printf 'test-lint: %s (docs/testing.md):\n%s\n\n' "a code block in a doc comment is a doctest, which CI never runs: tag it with its language (text, sh, json…)" "$doctests" >&2
    fail=1
fi

[ $fail = 0 ] && echo "test-lint: ok"
exit $fail
