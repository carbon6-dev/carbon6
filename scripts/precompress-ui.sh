#!/bin/sh
# Trunk runs this against staging before publishing dist. Also usable by size
# checks: scripts/precompress-ui.sh [dist]. A checkout without a UI is fine.
set -eu
dist=${1:-${TRUNK_STAGING_DIR:-crates/c6-ui/dist}}
[ -d "$dist" ] || exit 0
if ! command -v brotli >/dev/null 2>&1; then
    echo 'UI precompression needs brotli (brew install brotli / apt-get install brotli)' >&2
    exit 1
fi
# Keep originals for clients without Brotli support. Rebuild sidecars even if
# they already exist: a watch rebuild or a size check must not keep stale bytes.
find "$dist" -type f \( -name '*.wasm' -o -name '*.js' -o -name '*.css' -o -name '*.html' \) \
    -exec sh -ec 'for file do brotli --quality=11 --force --no-copy-stat --output="$file.br" "$file"; done' sh {} +
