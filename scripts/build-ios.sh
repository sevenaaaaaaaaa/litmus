#!/usr/bin/env bash
# Build an unsigned device IPA for signing with the user's own installation tool.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# iOS targets are installed in rustup, not in Homebrew's standalone Rust toolchain.
export PATH="$HOME/.cargo/bin:$PATH"
for LLVM_BIN in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin; do
  if [[ -x "$LLVM_BIN/llvm-objcopy" ]]; then export PATH="$PATH:$LLVM_BIN"; fi
done
VER="$(python3 - "$ROOT/Cargo.toml" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
print(re.search(r'\[workspace.package\][^\[]*?^version\s*=\s*"([^"]+)"', text, re.S | re.M)[1])
PY
)"
OUT="$ROOT/target/ios"
cd "$ROOT/desktop"
cargo tauri ios build --no-sign --archive-only --ci --config "{\"version\":\"$VER\"}"
ARCHIVE="$ROOT/desktop/gen/apple/build/litmus-desktop_iOS.xcarchive"
APP="$ARCHIVE/Products/Applications/Litmus.app"
[[ -d "$APP" ]] || { echo "Archive has no Litmus.app: $ARCHIVE" >&2; exit 1; }
python3 - "$APP/Info.plist" <<'PY'
import plistlib, sys
with open(sys.argv[1], 'rb') as f:
    info = plistlib.load(f)
assert info.get('CFBundleSupportedPlatforms') == ['iPhoneOS'], 'Expected a device build, not a simulator app'
assert info.get('CFBundleIdentifier') == 'com.nownexts.litmus', 'Unexpected application identifier'
PY
mkdir -p "$OUT"
STAGE="$(mktemp -d "$OUT/package.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/Payload"
ditto "$APP" "$STAGE/Payload/Litmus.app"
IPA="$OUT/Litmus-${VER}-ios-arm64-unsigned.ipa"
(cd "$STAGE" && /usr/bin/zip -qry "$STAGE/Litmus.ipa" Payload)
/usr/bin/unzip -tq "$STAGE/Litmus.ipa"
mv -f "$STAGE/Litmus.ipa" "$IPA"
(cd "$OUT" && shasum -a 256 "$(basename "$IPA")") > "$IPA.sha256"
echo "IPA: $IPA"
