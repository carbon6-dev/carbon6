#!/usr/bin/env bash
# Publishes the built website (make site) to the repository GitHub Pages
# serves carbon6.dev from: its branch becomes exactly the site, in one commit
# that names the carbon6 commit the site was built from. An unchanged site
# pushes nothing.
#
#   site/publish.sh <site dir>
#
# SITE_REPOSITORY (owner/name, default carbon6-dev/carbon6.dev) and SITE_BRANCH
# (default main) name where. With SITE_TOKEN (contents: write on that
# repository) it pushes over HTTPS, as the Site workflow does; without it, with
# your own SSH key.
set -euo pipefail

dir=${1:?usage: publish.sh <site dir>}
repo=${SITE_REPOSITORY:-carbon6-dev/carbon6.dev}
branch=${SITE_BRANCH:-main}
[ -f "$dir/index.html" ] || { echo "$dir has no index.html: run make site first" >&2; exit 1; }
src=$(cd "$dir" && pwd)
built_from=$(git rev-parse --short HEAD)

if [ -n "${SITE_TOKEN:-}" ]; then
    url="https://x-access-token:$SITE_TOKEN@github.com/$repo.git"
elif [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::warning::SITE_TOKEN is not set: the site is built but not published to $repo"
    exit 0
else
    url="git@github.com:$repo.git"
fi
ident=()
if [ -n "${GITHUB_ACTIONS:-}" ]; then
    ident=(-c user.name="github-actions[bot]" -c user.email="41898282+github-actions[bot]@users.noreply.github.com")
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone -q --depth 1 --no-single-branch "$url" "$work/pages" 2>&1 | grep -v 'empty repository' >&2 || true
[ -d "$work/pages/.git" ] || { echo "could not clone $repo" >&2; exit 1; }
cd "$work/pages"
if git rev-parse -q --verify "refs/remotes/origin/$branch" >/dev/null; then
    git checkout -q -B "$branch" "origin/$branch"
else
    git checkout -q --orphan "$branch"
fi

# Everything but .git is replaced, so pages gone from the site go here too.
find . -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
cp -R "$src"/. .
git add -A
if git diff --cached --quiet; then
    echo "$repo already has this site"
    exit 0
fi
git ${ident[@]+"${ident[@]}"} commit -q -m "Site from carbon6 $built_from"
git push -q origin "HEAD:$branch"
echo "Published the site from $built_from to $repo ($branch)"
