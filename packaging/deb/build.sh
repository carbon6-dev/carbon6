#!/usr/bin/env bash
# Builds the c6h, c6w and c6m .deb packages with their systemd units, and c6
# (the CLI). Runs on Debian/Ubuntu with dpkg-dev and binutils (the docker-deb
# build stage).
#
#   build.sh <dir with c6h, c6w, c6m and c6> <version> <output dir>
#
# Each package is its binary, <pkg>/root (other files as installed, if any)
# and <pkg>/control, whose
# @VERSION@, @ARCH@, @MAINTAINER@, @SIZE@ and @SHLIBS@ are filled in here, and
# its maintainer scripts and conffiles. MAINTAINER defaults to "carbon6".
set -euo pipefail

bin=$1 version=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
arch=$(dpkg --print-architecture)
maintainer=${MAINTAINER:-carbon6 <carbon6@localhost>}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

for pkg in c6h c6w c6m c6; do
    src=$here/$pkg
    root=$work/$pkg
    mkdir -p "$root/DEBIAN"
    [ -d "$src/root" ] && cp -R "$src/root/." "$root/"
    find "$root" -type d -exec chmod 0755 {} +
    find "$root" -type f -exec chmod 0644 {} +
    install -D -m 0755 "$bin/$pkg" "$root/usr/bin/$pkg"
    strip --strip-debug "$root/usr/bin/$pkg"

    for f in postinst prerm postrm; do
        [ -f "$src/$f" ] && install -m 0755 "$src/$f" "$root/DEBIAN/$f"
    done
    [ -f "$src/conffiles" ] && install -m 0644 "$src/conffiles" "$root/DEBIAN/conffiles"

    # Shared-library dependencies (libc6 and libgcc-s1, with the minimum
    # versions the binary needs), as dpkg-shlibdeps finds them. Its warning
    # about the usr-merge diversions of the dynamic loader is noise.
    mkdir -p "$work/shlibs/debian" && touch "$work/shlibs/debian/control"
    shlibs=$(cd "$work/shlibs" && dpkg-shlibdeps -O "$root/usr/bin/$pkg" 2> >(grep -v 'diversion' >&2) | sed -n 's/^shlibs:Depends=//p')
    size=$(du -sk --exclude=DEBIAN "$root" | cut -f1)
    sed -e "s|@VERSION@|$version|" -e "s|@ARCH@|$arch|" -e "s|@MAINTAINER@|$maintainer|" \
        -e "s|@SIZE@|$size|" -e "s|@SHLIBS@|$shlibs|" "$src/control" >"$root/DEBIAN/control"

    dpkg-deb -Zxz --root-owner-group --build "$root" "$out/${pkg}_${version}_${arch}.deb"
done
ls -l "$out"
