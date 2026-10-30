#!/usr/bin/env bash
# Publishes an edge build (docs/updates.md, the edge channel): the packages
# CI built for a green commit on main that isn't a release, as the rolling
# pre-release `edge`, which every such commit replaces. Machines on the edge
# channel update to it; `releases/latest` never names a pre-release, so
# everyone else is untouched. Its release.json is signed like a release's.
#
#   edge.sh <version> <dir>      (<version>: `make print-version`)
#
# Needs `gh` signed in (GH_TOKEN), GITHUB_REPOSITORY and GITHUB_SHA, and
# RELEASE_SIGNING_KEY (without it the build stays unpublished: an unsigned
# edge build would be one nothing updates to).
set -euo pipefail

version=$1 dir=$2
repo=${GITHUB_REPOSITORY:?owner/name of the repository}
sha=${GITHUB_SHA:?the commit}
[ -n "${RELEASE_SIGNING_KEY:-}" ] || { echo "::warning::RELEASE_SIGNING_KEY is not set: no edge build"; exit 0; }

# Its notes: what main has had since the last release.
last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)
notes=$(git log --no-merges --format='- %s' ${last:+"$last..$sha"} | grep -v '^- Release [0-9]' || true)
NOTES=${notes:-"- $(git log -1 --format=%s "$sha")"} packaging/release/manifest.sh "$version" "$dir"
[ -f "$dir/release.json.sig" ] || { echo "release.json is unsigned: no edge build" >&2; exit 1; }
printf 'carbon6 %s, built from %s on main.\n\nSince %s:\n\n%s\n' "$version" "${sha:0:7}" "${last:-the start}" "$notes" >"$dir/edge-notes.md"

# The one edge release, replaced: the new one is published first (under
# `edge-next`), so a publish that fails leaves the old one in place; then
# the old one and its tag go, and the new one takes the tag `edge`, which
# is where machines look. Between the two, for a moment, there is none:
# a machine that looks then finds no update that time.
gh release delete edge-next --repo "$repo" --cleanup-tag --yes 2>/dev/null || true
gh release create edge-next --repo "$repo" --prerelease --latest=false --target "$sha" --title "carbon6 edge ($version)" \
    --notes-file "$dir/edge-notes.md" "$dir"/*.deb "$dir"/*.tar.gz "$dir/release.json" "$dir/release.json.sig"
if gh release view edge --repo "$repo" >/dev/null 2>&1; then
    gh release delete edge --repo "$repo" --cleanup-tag --yes
fi
gh release edit edge-next --repo "$repo" --tag edge --target "$sha"
gh api -X DELETE "repos/$repo/git/refs/tags/edge-next" >/dev/null 2>&1 || true
echo "Published edge $version"
