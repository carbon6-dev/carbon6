#!/usr/bin/env bash
# Publishes release <version> from <dir> (the Release workflow's artifacts:
# .debs, tarballs and bottles): writes the Homebrew formulas pointing at the
# GitHub release, SHA256SUMS and the notes (CHANGELOG.md's section), creates
# the GitHub release, and pushes the formulas to the tap repository.
#
#   publish.sh <version> <dir>
#
# Needs `gh` signed in (GH_TOKEN) and GITHUB_REPOSITORY (owner/name). The tap
# is pushed with TAP_TOKEN (contents: write on TAP_REPOSITORY, default
# <owner>/homebrew-carbon6); without it the formulas are only attached to the
# release. Safe to re-run: missing uploads are added, an unchanged tap is left.
set -euo pipefail

version=$1 dir=$2
repo=${GITHUB_REPOSITORY:?owner/name of the repository}
tag=v$version
tap=${TAP_REPOSITORY:-${repo%%/*}/homebrew-carbon6}

packaging/brew/formulas.sh "$version" "$dir" "https://github.com/$repo/releases/download/$tag" "$dir/Formula"
cp "$dir"/Formula/*.rb "$dir/"
(cd "$dir" && shasum -a 256 -- *.deb *.tar.gz *.rb >SHA256SUMS)
awk -v head="## $version " 'index($0, head) == 1 { on = 1; next } on && /^## / { exit } on' CHANGELOG.md | sed '/./,$!d' >"$dir/notes.md"
[ -s "$dir/notes.md" ] || { echo "no section for $version in CHANGELOG.md" >&2; exit 1; }

assets=("$dir"/*.deb "$dir"/*.tar.gz "$dir"/*.rb "$dir/SHA256SUMS")
if gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
    gh release upload "$tag" --repo "$repo" --clobber "${assets[@]}"
else
    gh release create "$tag" --repo "$repo" --verify-tag --title "carbon6 $version" --notes-file "$dir/notes.md" "${assets[@]}"
fi

if [ -z "${TAP_TOKEN:-}" ]; then
    echo "::warning::TAP_TOKEN is not set: the formulas are on the release but not in $tap"
    exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone -q "https://x-access-token:$TAP_TOKEN@github.com/$tap.git" "$work/tap"
mkdir -p "$work/tap/Formula"
cp "$dir"/Formula/*.rb "$work/tap/Formula/"
git -C "$work/tap" add Formula
if git -C "$work/tap" diff --cached --quiet; then
    echo "$tap already has $version"
else
    git -C "$work/tap" -c user.name="github-actions[bot]" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
        commit -q -m "c6h, c6w and c6 $version"
    git -C "$work/tap" push -q origin HEAD
    echo "Pushed the formulas to $tap"
fi
