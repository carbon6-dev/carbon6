#!/usr/bin/env bash
# The last accepted production wasm, not a guessed fixed ceiling. When a build
# shrinks, lower these two numbers in that commit. Raising them needs a reason
# in the commit message. Allow 3% for ordinary compiler/build variation.
set -euo pipefail
cd "$(dirname "$0")/.."
# Retain typed views: erase_components made ordinary live snapshots clear and
# rebuild attributes/SVG innerHTML. Accept the measured runtime/size tradeoff;
# the 8 MB / 2 MB targets below remain goals, not a reason to regress input.
# 2026-11-14: raised for the UI that landed since (126 UI commits: batch
# starts, capture and Up next, session goals, browser Back, transcript times
# and more), measured from the Linux release build.
# 2026-11-21: the release with recovery status and guarded controls measures
# 326,355 raw bytes and 1,319 Brotli bytes above their accepted baselines.
# After simplifying the banner and
# sharing JSON serialization, record the measured Linux release raw size;
# the compressed build still fits its existing allowance below.
# 2026-11-26: cache-lifetime/keep-warm settings, waiting status and cost
# breakdown, combined with quiet-turn trim controls, measure 11,528,007 raw
# bytes after sharing the existing Settings field renderers. Retain these
# controls rather than remove functionality to fit the previous allowance.
accepted_raw=11528007
# 2026-11-26: worker compile-cache settings/statistics and attributed build
# folders add guarded, explicitly confirmed cleanup controls. After sharing JSON
# decoding and repeated form/metric renderers, the measured Linux production
# build is 11,547,277 raw / 2,879,521 Brotli bytes. Retain typed editable views
# rather than trading input stability for view-erasure size savings.
# 2026-11-21: aliases add creation/switch controls and per-account transcript
# metadata. After sharing Settings renderers, the measured Linux release is
# 2,790,904 bytes Brotli (82,824 above the previous accepted build); raw WASM
# still fits its existing allowance, so retain that baseline.
# The same combined Linux release measures 2,874,984 Brotli bytes.
accepted_br=2874984
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
