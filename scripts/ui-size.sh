#!/usr/bin/env bash
# The last accepted production wasm, not a guessed fixed ceiling. When a build
# shrinks, lower these two numbers in that commit. Raising them needs a reason
# in the commit message. Allow 3% for ordinary compiler/build variation.
set -euo pipefail
cd "$(dirname "$0")/.."
# Retain typed views: erase_components made ordinary live snapshots clear and
# rebuild attributes/SVG innerHTML. Accept the measured runtime/size tradeoff;
# the 8 MB / 2 MB targets below remain goals, not a reason to regress input.
accepted_raw=10412407
accepted_br=2589638
dist=${UI_DIST:-crates/c6-ui/dist}
shopt -s nullglob
wasm=("$dist"/*_bg.wasm)
if ((${#wasm[@]} == 0)); then
    echo 'ui-size: no built UI; skipped (make ui to measure it)'
    exit 0
fi
if ((${#wasm[@]} != 1)); then
    echo "ui-size: expected one wasm in $dist; run make ui to replace stale output" >&2
    exit 1
fi
file=${wasm[0]}
if [[ ! -f "$file.br" ]]; then
    echo "ui-size: missing $file.br; run make ui to precompress the UI" >&2
    exit 1
fi
status=0
measure() {
    local label=$1 file=$2 accepted=$3 target=$4 bytes ceiling delta
    bytes=$(wc -c < "$file")
    ceiling=$(((accepted * 103 + 99) / 100))
    delta=$((bytes - accepted))
    printf 'ui-size: %s %d bytes (%+d vs accepted %d; ceiling %d; target %d)\n' \
        "$label" "$bytes" "$delta" "$accepted" "$ceiling" "$target"
    if ((bytes > ceiling)); then
        echo "ui-size: $label exceeds the accepted build + 3%; shrink it or explain a budget increase in the commit" >&2
        status=1
    fi
}
measure raw "$file" "$accepted_raw" 8000000
measure brotli "$file.br" "$accepted_br" 2000000
exit "$status"
