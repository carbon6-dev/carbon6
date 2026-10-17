#!/usr/bin/env bash
# Prints the CHANGELOG.md section for a release: one bullet per commit since
# the previous release tag (all commits for the first), release commits left out.
#
#   notes.sh [<version>]     (default: "Unreleased")
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
printf '## %s (%s)\n\n' "${1:-Unreleased}" "$(date +%Y-%m-%d)"
# For updates (docs/updates.md §2): MANUAL="steps" makes the release one a
# person applies; UPGRADE_FROM=x.y.z is the oldest version that updates to
# it on its own. The publish job reads them back from these lines.
if [ -n "${MANUAL:-}" ]; then printf 'Manual: %s\n\n' "$MANUAL"; fi
if [ -n "${UPGRADE_FROM:-}" ]; then printf 'Upgrade from: %s\n\n' "$UPGRADE_FROM"; fi
git log --no-merges --format='- %s' ${last:+"$last..HEAD"} | grep -v '^- Release [0-9]' || true
