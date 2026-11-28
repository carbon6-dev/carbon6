#!/usr/bin/env bash
# Local-only safeguard: the patterns and matching content never leave this machine.
set -euo pipefail

usage() {
    echo 'usage: check-private-terms.sh --staged | --range A..B' >&2
    exit 2
}
mode=${1:-}
case "$mode" in
    --staged) [ "$#" -eq 1 ] || usage ;;
    --range) [ "$#" -eq 2 ] && [ -n "$2" ] || usage ;;
    *) usage ;;
esac

terms=${C6_PRIVATE_TERMS:-${HOME}/.config/c6/private-terms}
if [ ! -f "$terms" ]; then
    echo 'private-terms: no local term file; skipping check.' >&2
    exit 0
fi
case "$terms" in
    /*) ;;
    *) terms="$PWD/$terms" ;;
esac

cd "$(git rev-parse --show-toplevel)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
sed '/^[[:space:]]*#/d; /^[[:space:]]*$/d' "$terms" > "$tmp/patterns"
[ -s "$tmp/patterns" ] || exit 0

fail=0
# Keep grep's matching content in a private temporary file; only locations print.
find_hits() {
    local status=0
    LC_ALL=C grep -n -i -E -f "$tmp/patterns" "$1" > "$tmp/hits" || status=$?
    if [ "$status" -gt 1 ]; then
        echo 'private-terms: could not check the local patterns.' >&2
        exit 2
    fi
    if [ "$status" -eq 0 ]; then fail=1; fi
}

scan_diff() {
    local file=$1
    # With --no-renames there is only one path per patch. Ignore deleted lines:
    # removing a leak must be allowed. Map additions back to their new line numbers.
    awk -v map="$tmp/lines" '
        /^@@ / { split($3, location, ","); line = substr(location[1], 2); hunk = 1; next }
        !hunk { next }
        /^\+/ { print substr($0, 2); print line > map; line++; next }
        /^ / { line++ }
    ' "$tmp/patch" > "$tmp/added"
    [ -s "$tmp/added" ] || return 0
    find_hits "$tmp/added"
    if [ -s "$tmp/hits" ]; then
        awk -F: -v file="$file" '
            NR == FNR { hits[$1] = 1; next }
            FNR in hits { printf "%s:%s: private term found\n", file, $0 }
        ' "$tmp/hits" "$tmp/lines" >&2
    fi
}

paths=(. ':!*.lock' ':!crates/c6-ui/vendor')
if [ "$mode" = --staged ]; then
    git diff --cached --no-renames --name-only -z -- "${paths[@]}" > "$tmp/files"
    while IFS= read -r -d '' file; do
        git diff --cached --no-color --no-ext-diff --no-textconv --no-renames --unified=0 -- ":(literal)$file" > "$tmp/patch"
        scan_diff "$file"
    done < "$tmp/files"
else
    # Inspect every commit, not just the net diff: adding then removing a secret
    # still publishes it. First-parent merge diffs include merge resolutions.
    git rev-list --reverse "$2" > "$tmp/commits"
    while IFS= read -r commit; do
        git show -s --no-show-signature --format=%B "$commit" > "$tmp/message"
        find_hits "$tmp/message"
        if [ -s "$tmp/hits" ]; then
            awk -F: -v commit="$commit" '{ printf "commit %s:%s: private term found\n", commit, $1 }' "$tmp/hits" >&2
        fi
        git diff-tree --root --no-commit-id --no-renames --diff-merges=first-parent -r --name-only -z "$commit" -- "${paths[@]}" > "$tmp/files"
        while IFS= read -r -d '' file; do
            git show --format= --root --diff-merges=first-parent --no-color --no-ext-diff --no-textconv --no-renames --unified=0 "$commit" -- ":(literal)$file" > "$tmp/patch"
            scan_diff "$file"
        done < "$tmp/files"
    done < "$tmp/commits"
fi
exit "$fail"
