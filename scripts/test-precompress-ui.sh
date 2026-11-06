#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
sh "$root/scripts/precompress-ui.sh" "$tmp/absent"
mkdir -p "$tmp/dist/nested folder"
for ext in wasm js css html; do
    printf 'original %s content\n' "$ext" > "$tmp/dist/nested folder/test.$ext"
done
printf 'untouched' > "$tmp/dist/icon.png"
TRUNK_STAGING_DIR="$tmp/dist" sh "$root/scripts/precompress-ui.sh"
[ ! -e "$tmp/dist/icon.png.br" ]
for ext in wasm js css html; do
    file="$tmp/dist/nested folder/test.$ext"
    brotli --decompress --stdout "$file.br" | cmp - "$file"
done
printf 'changed bytes\n' > "$tmp/dist/nested folder/test.js"
sh "$root/scripts/precompress-ui.sh" "$tmp/dist"
brotli --decompress --stdout "$tmp/dist/nested folder/test.js.br" | cmp - "$tmp/dist/nested folder/test.js"
echo 'UI precompression: roundtrip, nested paths, rebuild and absent dist passed'
