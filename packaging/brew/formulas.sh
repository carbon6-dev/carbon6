#!/usr/bin/env bash
# Writes the Homebrew formulas for c6h, c6w, c6m and c6 from <name>.rb, for
# the architectures whose files build.sh left in <dir>, downloading from
# <url base>: file://<dir> for a local tap, the GitHub release for a release.
#
#   formulas.sh <version> <dir> <url base> <formula dir>
#
# The version goes into the download URLs, which Homebrew escapes and curl
# (for file://) does not unescape, so it is kept to digits and dots.
set -euo pipefail

version=$1 dir=$2 base=$3 formulas=$4
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=packaging/brew/arch.sh
. "$here/arch.sh"
mkdir -p "$formulas"
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

for pkg in c6h c6w c6m c6; do
    sources='' bottles='' arches=()
    for arch in arm64 x86_64; do
        tarball=$dir/$pkg-$version-darwin-$arch.tar.gz
        [ -f "$tarball" ] || continue
        tag=$(bottle_tag "$arch")
        bottle=$dir/$pkg-$version.$tag.bottle.tar.gz
        arches+=("$arch")
        sources+="
  $(on_arch "$arch") do
    url \"$base/$(basename "$tarball")\"
    sha256 \"$(sha "$tarball")\"
  end"
        bottles+="    sha256 cellar: :any_skip_relocation, $tag: \"$(sha "$bottle")\"
"
    done
    [ ${#arches[@]} -gt 0 ] || { echo "no $pkg files for $version in $dir" >&2; exit 1; }
    # Built for one architecture only: say so, rather than fail on the other.
    depends=''
    [ ${#arches[@]} -eq 1 ] && depends="  depends_on arch: :${arches[0]}"

    while IFS= read -r line; do
        case $line in
            @SOURCES@) printf '%s\n' "$sources" ;;
            @BOTTLES@) printf '%s' "$bottles" ;;
            @DEPENDS_ON_ARCH@) [ -z "$depends" ] || printf '%s\n' "$depends" ;;
            *) line=${line//@VERSION@/$version}; printf '%s\n' "${line//@ROOT_URL@/$base}" ;;
        esac
    done <"$here/$pkg.rb" >"$formulas/$pkg.rb"
done
ls -l "$formulas"
