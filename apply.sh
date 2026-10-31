#!/bin/sh
# Generate newport/<tag> from a base tag and a patch series.
#
#   ./apply.sh <tag> [branch]          GPUI Kit: series, patches/
#   ./apply.sh gpui <tag> [branch]     GPUI snapshot: gpui/series, gpui/patches/
#
# Each patch becomes one commit with the patch's author, date and message.
# The committer is set to the author, so the same series on the same tag
# always produces the same commit IDs. Nothing is checked out: the commits
# are built on a temporary index, and only the branch ref is written.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
series=$here
if [ "${1:-}" = gpui ]; then
    series=$here/gpui
    shift
fi
tag=${1:?usage: ./apply.sh [gpui] <tag> [branch]}
branch=${2:-newport/$tag}

if git -C "$here" worktree list --porcelain | grep -qx "branch refs/heads/$branch"; then
    echo "$branch is checked out in a worktree; switch it away first." >&2
    exit 1
fi

parent=$(git -C "$here" rev-parse --verify --quiet "$tag^{commit}") || {
    echo "No tag $tag. Fetch it first: git fetch upstream --tags" >&2
    exit 1
}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
GIT_INDEX_FILE="$scratch/index"
export GIT_INDEX_FILE
git -C "$here" read-tree "$parent"

field() {
    sed -n "s/^$1: //p" "$scratch/info"
}

while read -r patch; do
    case $patch in '' | '#'*) continue ;; esac
    git -C "$here" mailinfo "$scratch/message" "$scratch/diff" \
        <"$series/patches/$patch" >"$scratch/info"
    if ! git -C "$here" apply --cached "$scratch/diff"; then
        echo "$patch does not apply to $tag." >&2
        echo "Fix it on a branch, then run ./export.sh (see README.md)." >&2
        exit 1
    fi
    tree=$(git -C "$here" write-tree)
    name=$(field Author)
    email=$(field Email)
    date=$(field Date)
    parent=$(
        {
            field Subject
            if [ -s "$scratch/message" ]; then
                echo
                cat "$scratch/message"
            fi
        } | GIT_AUTHOR_NAME=$name GIT_AUTHOR_EMAIL=$email GIT_AUTHOR_DATE=$date \
            GIT_COMMITTER_NAME=$name GIT_COMMITTER_EMAIL=$email GIT_COMMITTER_DATE=$date \
            git -C "$here" commit-tree "$tree" -p "$parent"
    )
    echo "$(git -C "$here" rev-parse --short "$parent") $patch"
done <"$series/series"

git -C "$here" update-ref "refs/heads/$branch" "$parent"
echo "$branch -> $(git -C "$here" rev-parse --short "$parent")"
