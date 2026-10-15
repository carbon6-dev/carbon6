#!/usr/bin/env bash
# Packages c6h, c6w, c6m and c6 for Homebrew on this Mac's architecture. For
# each: a bottle (the installed keg, <name>/<version>/bin/<name>), a source
# tarball with just the binary (used only where the bottle can't be poured),
# and a formula from <name>.rb pointing at both by file:// URL. Bottles are
# poured without Homebrew's build-from-source checks (no Xcode or CLT needed).
#
#   build.sh <dir with c6h, c6w, c6m and c6> <version> <output dir>
#
# The version goes into file:// URLs, which Homebrew escapes and curl does
# not unescape, so keep it to digits and dots (e.g. 0.1.0.20261015011445).
#
# Writes <name>-<version>-darwin-<arch>.tar.gz, <name>-<version>.<tag>.bottle.tar.gz
# and Formula/<name>.rb into <output dir>.
set -euo pipefail

bin=$1 version=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
arch=$(uname -m)
# Big Sur (macOS 11) is the oldest macOS Rust targets; Homebrew pours a
# bottle on that release and every later one.
case $arch in
    arm64) tag=arm64_big_sur ;;
    x86_64) tag=big_sur ;;
    *) echo "unsupported architecture $arch" >&2; exit 1 ;;
esac
mkdir -p "$out/Formula"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# No AppleDouble (._*) files in the tarballs.
export COPYFILE_DISABLE=1

for pkg in c6h c6w c6m c6; do
    tarball=$out/$pkg-$version-darwin-$arch.tar.gz
    tar -czf "$tarball" -C "$bin" "$pkg"

    install -d "$work/$pkg/$version/bin"
    install -m 0755 "$bin/$pkg" "$work/$pkg/$version/bin/$pkg"
    # The name Homebrew asks root_url for.
    bottle=$out/$pkg-$version.$tag.bottle.tar.gz
    tar -czf "$bottle" -C "$work" "$pkg"

    sed -e "s|@URL@|file://$tarball|" -e "s|@VERSION@|$version|" -e "s|@ARCH@|$arch|" \
        -e "s|@SHA256@|$(shasum -a 256 "$tarball" | cut -d' ' -f1)|" \
        -e "s|@ROOT_URL@|file://$out|" -e "s|@BOTTLE_TAG@|$tag|" \
        -e "s|@BOTTLE_SHA256@|$(shasum -a 256 "$bottle" | cut -d' ' -f1)|" \
        "$here/$pkg.rb" >"$out/Formula/$pkg.rb"
done
ls -l "$out" "$out/Formula"
