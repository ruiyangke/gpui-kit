#!/bin/sh
# Write a patch series from a branch built on a base tag.
#
#   ./export.sh <tag> [branch]          GPUI Kit: series, patches/
#   ./export.sh gpui <tag> [branch]     GPUI snapshot: gpui/series, gpui/patches/
#
# Every commit in <tag>..<branch> becomes one file in patches/, in order, and
# series lists them. Run ./apply.sh afterwards to regenerate the branch from
# the files.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
series=$here
if [ "${1:-}" = gpui ]; then
    series=$here/gpui
    shift
fi
tag=${1:?usage: ./export.sh [gpui] <tag> [branch]}
branch=${2:-newport/$tag}

git -C "$here" rev-parse --verify --quiet "$tag^{commit}" >/dev/null || {
    echo "No tag $tag." >&2
    exit 1
}
git -C "$here" rev-parse --verify --quiet "$branch^{commit}" >/dev/null || {
    echo "No branch $branch." >&2
    exit 1
}

rm -f "$series"/patches/*.patch
mkdir -p "$series/patches"
git -C "$here" format-patch --quiet --zero-commit --no-signature --no-numbered \
    --output-directory "$series/patches" "$tag..$branch"
(cd "$series/patches" && ls -1 ./*.patch | sed 's|^\./||') >"$series/series"
cat "$series/series"
