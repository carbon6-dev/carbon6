#!/usr/bin/env bash
# Writes release.json for <version> from the files in <dir> and CHANGELOG.md
# (docs/updates.md §2), and signs it into release.json.sig with the Ed25519
# key in RELEASE_SIGNING_KEY (PEM). Without the key it writes release.json
# only and says so: carbon6 doesn't update itself to an unsigned release.
#
#   manifest.sh <version> <dir>
set -euo pipefail

version=$1 dir=$2
notes=$(awk -v head="## $version " 'index($0, head) == 1 { on = 1; next } on && /^## / { exit } on' CHANGELOG.md | sed '/./,$!d')
[ -n "$notes" ] || { echo "no section for $version in CHANGELOG.md" >&2; exit 1; }
protocol=$(sed -n 's/^pub const PROTOCOL_VERSION: u32 = \([0-9]*\);/\1/p' crates/c6-core/src/lib.rs)
(cd "$dir" && ls -- *.deb *.tar.gz 2>/dev/null) | while read -r f; do
    printf '%s\tsha256:%s\n' "$f" "$(shasum -a 256 -- "$dir/$f" | cut -d' ' -f1)"
done >"$dir/.assets"
NOTES=$notes VERSION=$version PROTOCOL=$protocol python3 - "$dir" <<'PY'
import json, os, re, sys, datetime
d = sys.argv[1]
notes = os.environ["NOTES"]
line = lambda k: (re.search(rf"^{k}: (.+)$", notes, re.M) or [None, None])[1]
assets = dict(l.rstrip("\n").split("\t") for l in open(f"{d}/.assets") if l.strip())
m = {
    "version": os.environ["VERSION"],
    "date": datetime.date.today().isoformat(),
    "protocol_version": int(os.environ["PROTOCOL"]),
    "upgrade_from": line("Upgrade from") or "",
    "manual": line("Manual"),
    "notes": notes,
    "assets": assets,
}
open(f"{d}/release.json", "w").write(json.dumps(m, indent=2, sort_keys=True) + "\n")
PY
rm -f "$dir/.assets"
if [ -z "${RELEASE_SIGNING_KEY:-}" ]; then
    echo "::warning::RELEASE_SIGNING_KEY is not set: release.json is unsigned, and carbon6 won't update itself to $version"
    exit 0
fi
key=$(mktemp)
trap 'rm -f "$key"' EXIT
printf '%s\n' "$RELEASE_SIGNING_KEY" >"$key"
openssl pkeyutl -sign -inkey "$key" -rawin -in "$dir/release.json" -out "$dir/release.json.sig"
echo "Signed release.json ($(grep -c '"sha256:' "$dir/release.json") files)"
