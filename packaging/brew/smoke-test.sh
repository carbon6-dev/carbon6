#!/usr/bin/env bash
# Tries the Homebrew formulas in <formula dir> (from formulas.sh, with
# file:// URLs): installs c6h, c6w, c6m and c6 from a scratch tap, starts the
# head with `brew services`, attaches a worker and the memex (run as their
# services run them, with scratch state), checks c6 lists them online and that
# SIGTERM stops them cleanly, then uninstalls everything it installed.
#
#   smoke-test.sh <formula dir>
#
# It installs into this Mac's Homebrew, so it refuses to run where any of the
# four is already installed or something listens on the head's ports.
set -euo pipefail

formulas=$(cd "$1" && pwd)
tap=carbon6/smoke
prefix=$(brew --prefix)
export PATH="$prefix/bin:$PATH"
state=$(mktemp -d)
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_ENV_HINTS=1

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
fail() { echo "FAILED: $1" >&2; tail -n 30 "$prefix/var/log/c6h.log" >&2 2>/dev/null || true; exit 1; }
# Waits up to 60s for a command to succeed.
until_ok() {
    for _ in $(seq 120); do eval "$1" >/dev/null 2>&1 && return 0; sleep 0.5; done
    fail "$2"
}

for f in c6h c6w c6m c6; do
    ! brew list --versions "$f" >/dev/null || { echo "$f is already installed; uninstall it to run this test" >&2; exit 1; }
done
! nc -z 127.0.0.1 7700 2>/dev/null || { echo "something listens on :7700; stop it to run this test" >&2; exit 1; }

cleanup() {
    kill "${worker:-}" "${memex:-}" 2>/dev/null || true
    brew services stop "$tap/c6h" >/dev/null 2>&1 || true
    brew uninstall --force c6h c6w c6m c6 >/dev/null 2>&1 || true
    brew untap "$tap" >/dev/null 2>&1 || true
    rm -rf "$prefix/var/c6h" "$prefix/etc/c6h" "$prefix"/var/log/c6[hwm].log "$state"
}
trap cleanup EXIT

step "Installing from $tap"
brew tap-new --no-git "$tap" >/dev/null
cp "$formulas"/*.rb "$(brew --repo "$tap")/Formula/"
brew install "$tap/c6h" "$tap/c6w" "$tap/c6m" "$tap/c6"
for f in c6h c6w c6m c6; do "$prefix/bin/$f" --version; done
grep -q "^data_dir = \"$prefix/var/c6h\"" "$prefix/etc/c6h/c6h.toml" || fail "the head's config was not written"

step "Head (brew services)"
brew services start "$tap/c6h"
until_ok 'curl -fsS -o /dev/null http://127.0.0.1:7700/' "the head's UI does not answer on :7700"
config=$prefix/etc/c6h/c6h.toml
c6h --config "$config" login-link | grep -q '^http' || fail "login-link"

step "Worker and memex, as their services run them"
token() { c6h --config "$config" enroll-token --name "$1" | awk '{print $3}'; }
c6w --state-dir "$state/w" enroll "$(token smoke-worker)"
c6m --state-dir "$state/m" enroll "$(token smoke-memex)"
NO_COLOR=1 /bin/zsh -lc "exec $prefix/opt/c6w/bin/c6w --state-dir '$state/w' run" >"$state/w.log" 2>&1 &
worker=$!
NO_COLOR=1 "$prefix/opt/c6m/bin/c6m" --state-dir "$state/m" run >"$state/m.log" 2>&1 &
memex=$!
until_ok "grep -q 'attached to head' '$state/w.log' && grep -q 'attached to head' '$state/m.log'" "the worker and memex did not attach"

step "CLI"
C6_URL=http://127.0.0.1:7700 C6_TOKEN=$(c6h --config "$config" api-token smoke) c6 workers | tee "$state/workers"
grep -qE 'online +smoke-worker' "$state/workers" || fail "c6 workers does not list the worker online"

step "Clean stop (SIGTERM)"
for pid in "$worker" "$memex"; do
    kill -TERM "$pid"
    status=0; wait "$pid" || status=$?
    [ "$status" -eq 0 ] || fail "a SIGTERM'd process exited with $status"
done
worker='' memex=''
brew services stop "$tap/c6h"
until_ok '! nc -z 127.0.0.1 7700' "the head did not stop"

printf '\n\033[1;32mPASS\033[0m: c6h, c6w, c6m and c6 install from bottles and run\n'
