#!/usr/bin/env bash
# Synthetic fixtures only; never reads a maintainer's real pattern file.
set -euo pipefail
scripts=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# Do not inherit signing, templates, hooks, or diff settings from the developer.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export C6_PRIVATE_TERMS="$tmp/terms"
printf '  # ignored-comment\n\nSENSITIVE-FIXTURE\n' > "$C6_PRIVATE_TERMS"
git init -q "$tmp/repo"
cd "$tmp/repo"
git config user.name 'Test Maintainer'
git config user.email 'maintainer@example.invalid'
git config commit.gpgsign false
git config core.hooksPath "$tmp/no-hooks"
printf 'clean\n' > 'sample file.txt'
git add .
git commit -qm 'Initial fixture'
base=$(git rev-parse HEAD)

expect_status() {
    local expected=$1 status=0
    shift
    "$@" > "$tmp/output" 2>&1 || status=$?
    if [ "$status" -ne "$expected" ]; then
        printf 'private-terms test: expected exit %s, got %s\n' "$expected" "$status" >&2
        cat "$tmp/output" >&2
        exit 1
    fi
}
check() { bash "$scripts/check-private-terms.sh" "$@"; }

# Added lines are case-insensitive, redacted, and reported with actual new lines.
printf 'clean\nsensitive-fixture\n' > 'sample file.txt'
git add .
expect_status 1 check --staged
grep -q '^sample file.txt:2: private term found$' "$tmp/output"
if grep -qi 'sensitive-fixture' "$tmp/output"; then
    echo 'private-terms test: matching content was printed' >&2
    exit 1
fi

printf 'clean\nignored-comment\n' > 'sample file.txt'
git add .
expect_status 0 check --staged
# Unstaged changes are not part of the check.
printf 'sensitive-fixture\n' >> 'sample file.txt'
expect_status 0 check --staged
git checkout -- 'sample file.txt'

# Exclusions apply both to staged changes and commit ranges.
mkdir -p crates/c6-ui/vendor
printf 'sensitive-fixture\n' > fixture.lock
printf 'sensitive-fixture\n' > crates/c6-ui/vendor/fixture.js
git add .
expect_status 0 check --staged
git commit -qm 'Clean fixture and excluded files'
expect_status 0 check --range "$base..HEAD"

printf 'sensitive-fixture\n' >> 'sample file.txt'
git add .
git commit -qm 'Add synthetic fixture'
dirty=$(git rev-parse HEAD)
printf 'clean\n' > 'sample file.txt'
git add .
expect_status 0 check --staged
git commit -qm 'Remove synthetic fixture'
# A clean final tree does not hide an intermediate commit's addition.
expect_status 1 check --range "$base..HEAD"
expect_status 0 check --range "$dirty..HEAD"

git commit --allow-empty -qm 'Message with sensitive-fixture'
expect_status 1 check --range 'HEAD^..HEAD'
grep -q '^commit .*:1: private term found$' "$tmp/output"
if grep -qi 'sensitive-fixture' "$tmp/output"; then
    echo 'private-terms test: matching message was printed' >&2
    exit 1
fi

# Missing and comment-only term files are successful no-ops.
expect_status 0 env C6_PRIVATE_TERMS="$tmp/missing" bash "$scripts/check-private-terms.sh" --staged
grep -q 'no local term file' "$tmp/output"
printf '# comment only\n  \n' > "$tmp/empty-terms"
expect_status 0 env C6_PRIVATE_TERMS="$tmp/empty-terms" bash "$scripts/check-private-terms.sh" --staged
mkdir -p "$tmp/home/.config/c6"
cp "$C6_PRIVATE_TERMS" "$tmp/home/.config/c6/private-terms"
expect_status 1 env -u C6_PRIVATE_TERMS HOME="$tmp/home" bash "$scripts/check-private-terms.sh" --range 'HEAD^..HEAD'

# A root commit and merge-only resolution must also be checked.
expect_status 1 check --range "$dirty"
git checkout -qb merge-left "$base"
printf 'left\n' > conflict.txt
git add .
git commit -qm 'Left side'
left=$(git rev-parse HEAD)
git checkout -qb merge-right "$base"
printf 'right\n' > conflict.txt
git add .
git commit -qm 'Right side'
if git merge --no-edit merge-left > "$tmp/merge-output" 2>&1; then
    echo 'private-terms test: expected a merge conflict' >&2
    exit 1
fi
printf 'sensitive-fixture\n' > conflict.txt
git add .
git commit -qm 'Resolve both sides'
expect_status 1 check --range 'HEAD^!'
grep -q '^conflict.txt:1: private term found$' "$tmp/output"

# Hook protocol: existing branch, new branch, and deletion. The hook resolves
# the guard relative to its checkout, not relative to its installation path.
mkdir -p scripts
cp "$scripts/check-private-terms.sh" scripts/
tip=$(git rev-parse HEAD)
zeros=0000000000000000000000000000000000000000
printf 'refs/heads/topic %s refs/heads/topic %s\n' "$tip" "$left" > "$tmp/push"
expect_status 1 bash "$scripts/hooks/pre-push" origin < "$tmp/push"
git update-ref refs/remotes/origin/main "$left"
printf 'refs/heads/topic %s refs/heads/topic %s\n' "$tip" "$zeros" > "$tmp/push"
expect_status 1 bash "$scripts/hooks/pre-push" origin < "$tmp/push"
printf 'refs/heads/topic %s refs/heads/topic %s\n' "$left" "$zeros" > "$tmp/push"
expect_status 0 bash "$scripts/hooks/pre-push" origin < "$tmp/push"
printf '(delete) %s refs/heads/topic %s\n' "$zeros" "$tip" > "$tmp/push"
expect_status 0 bash "$scripts/hooks/pre-push" origin < "$tmp/push"

echo 'private-terms: shell checks passed.'
