#!/usr/bin/env bash
# Prints the CHANGELOG.md section for a release: one bullet per commit since
# the previous release tag (all commits for the first), release commits left out.
#
#   notes.sh [<version>]     (default: "Unreleased")
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
printf '## %s (%s)\n\n' "${1:-Unreleased}" "$(date +%Y-%m-%d)"
git log --no-merges --format='- %s' ${last:+"$last..HEAD"} | grep -v '^- Release [0-9]' || true
