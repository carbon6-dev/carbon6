#!/usr/bin/env bash
# Packages c6h, c6w and c6 for Homebrew on this Mac's architecture: for
# each, a bottle (the installed keg, <name>/<version>/bin/<name>) and a plain
# tarball of the binary (the formula's source, used only where a bottle can't
# be poured). formulas.sh then writes the formulas for whichever
# architectures' files are in the directory.
#
#   build.sh <dir with c6h, c6w and c6> <version> <output dir>
#
# Writes <name>-<version>-darwin-<arch>.tar.gz and
# <name>-<version>.<bottle tag>.bottle.tar.gz into <output dir>.
set -euo pipefail

bin=$1 version=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=packaging/brew/arch.sh
. "$here/arch.sh"
arch=$(uname -m)
tag=$(bottle_tag "$arch")
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# No AppleDouble (._*) files in the tarballs.
export COPYFILE_DISABLE=1

for pkg in c6h c6w c6; do
    tar -czf "$out/$pkg-$version-darwin-$arch.tar.gz" -C "$bin" "$pkg"
    install -d "$work/$pkg/$version/bin"
    install -m 0755 "$bin/$pkg" "$work/$pkg/$version/bin/$pkg"
    tar -czf "$out/$pkg-$version.$tag.bottle.tar.gz" -C "$work" "$pkg"
done
ls -l "$out"
