#!/usr/bin/env bash
# Cuts a release: checks, sets the workspace version, adds the release's
# section to CHANGELOG.md, commits "Release <version>", tags v<version> and
# pushes both, which starts the Release workflow (.github/workflows/release.yml)
# that tests, builds, smoke-tests and publishes it.
#
#   release.sh patch|minor|major|<x.y.z>
#
# It asks before pushing (YES=1 skips the question). Not pushed, the commit
# and tag stay local; `git tag -d v<version> && git reset --hard HEAD~1` undoes them.
set -euo pipefail

want=${1:-patch}
remote=${RELEASE_REMOTE:-origin}
cd "$(git rev-parse --show-toplevel)"
die() { echo "release: $*" >&2; exit 1; }

# Checks.
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "uncommitted changes"
git fetch -q "$remote" main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse "$remote/main")" ] || die "main is not at $remote/main: pull or push first"

current=$(sed -n '/^\[workspace.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' Cargo.toml)
IFS=. read -r major minor patch <<<"$current"
case $want in
    major) version=$((major + 1)).0.0 ;;
    minor) version=$major.$((minor + 1)).0 ;;
    patch) version=$major.$minor.$((patch + 1)) ;;
    [0-9]*.[0-9]*.[0-9]*) version=$want ;;
    *) die "unknown version or bump: $want (patch, minor, major or x.y.z)" ;;
esac
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$version is not x.y.z"
[ "$(printf '%s\n%s\n' "$current" "$version" | sort -V | tail -1)" = "$version" ] && [ "$version" != "$current" ] \
    || die "$version is not newer than $current"
! git rev-parse -q --verify "refs/tags/v$version" >/dev/null || die "tag v$version exists"

# The release commit. Until it is tagged, a failure puts everything back.
echo "Releasing $version (from $current)"
start=$(git rev-parse HEAD)
had_changelog=$([ -f CHANGELOG.md ] && echo 1 || true)
undo() {
    echo "release: failed; nothing was released" >&2
    git tag -d "v$version" >/dev/null 2>&1 || true
    git reset -q --hard "$start"
    rm -f CHANGELOG.md.new
    [ -n "$had_changelog" ] || rm -f CHANGELOG.md
}
trap undo ERR
sed -i.bak '/^\[workspace.package\]/,/^\[/ s/^version = ".*"/version = "'"$version"'"/' Cargo.toml && rm Cargo.toml.bak
cargo update --workspace --offline --quiet
notes=$(packaging/release/notes.sh "$version")
{
    echo "# Changelog"
    echo
    echo "$notes"
    if [ -f CHANGELOG.md ]; then echo; sed '1{/^# Changelog$/d;}' CHANGELOG.md | sed '/./,$!d'; fi
} >CHANGELOG.md.new && mv CHANGELOG.md.new CHANGELOG.md
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -q -m "Release $version"
git tag -a "v$version" -m "carbon6 $version"
trap - ERR
git --no-pager log --oneline -1
echo "CHANGELOG.md: $(grep -c '^- ' <<<"$notes") changes since $current"

# Pushing starts the Release workflow.
if [ "${YES:-}" != 1 ]; then
    read -r -p "Push main and v$version to $remote (starts the Release workflow)? [y/N] " answer
    case $answer in
        y | Y | yes) ;;
        *) echo "Not pushed. Push later with: git push --atomic $remote main v$version"; exit 0 ;;
    esac
fi
git push --atomic "$remote" main "v$version"
url=$(git remote get-url "$remote" | sed -E 's#^git@github.com:#https://github.com/#; s#\.git$##')
echo "Release workflow: $url/actions/workflows/release.yml"
