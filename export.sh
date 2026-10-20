#!/bin/sh
# Write this series from a branch built on an upstream tag.
#
#   ./export.sh <tag> [branch]
#
# Every commit in <tag>..<branch> becomes one file in patches/, in order, and
# series lists them. Run ./apply.sh afterwards to regenerate the branch from
# the files.
set -eu

tag=${1:?usage: ./export.sh <tag> [branch]}
branch=${2:-newport/$tag}
here=$(cd "$(dirname "$0")" && pwd)

git -C "$here" rev-parse --verify --quiet "$tag^{commit}" >/dev/null || {
    echo "No tag $tag." >&2
    exit 1
}
git -C "$here" rev-parse --verify --quiet "$branch^{commit}" >/dev/null || {
    echo "No branch $branch." >&2
    exit 1
}

rm -f "$here"/patches/*.patch
mkdir -p "$here/patches"
git -C "$here" format-patch --quiet --zero-commit --no-signature --no-numbered \
    --output-directory "$here/patches" "$tag..$branch"
(cd "$here/patches" && ls -1 ./*.patch | sed 's|^\./||') >"$here/series"
cat "$here/series"
