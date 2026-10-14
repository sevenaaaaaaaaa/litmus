#!/usr/bin/env bash
# Build a macOS DMG that ships the Gatekeeper helper next to the app.
#
# Layout: <App>.app, Applications -> /Applications, 打不开？双击我.command, 安装说明.txt
#
# Usage: packaging/macos/make-dmg.sh <path/to/App.app> <out.dmg> [volume-name]
# Used by scripts/daily-release.sh (local) and .github/workflows/desktop.yml (CI).
set -euo pipefail

APP_SRC="${1:?usage: make-dmg.sh <App.app> <out.dmg> [volume-name]}"
OUT="${2:?usage: make-dmg.sh <App.app> <out.dmg> [volume-name]}"
APP_SRC="${APP_SRC%/}"
APP_BASENAME="$(basename "$APP_SRC")"
APP_NAME="${APP_BASENAME%.app}"
VOLNAME="${3:-$APP_NAME}"
HERE="$(cd "$(dirname "$0")" && pwd)"

[[ -d "$APP_SRC" && "$APP_BASENAME" == *.app ]] || { echo "make-dmg: not an .app bundle: $APP_SRC" >&2; exit 1; }

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/make-dmg.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

ditto "$APP_SRC" "$STAGE/$APP_BASENAME"

# Unsigned / linker-signed bundles fail `codesign --verify` and show up as "damaged"
# even after quarantine removal; give them a consistent ad-hoc signature.
if ! codesign --verify --deep "$STAGE/$APP_BASENAME" >/dev/null 2>&1; then
  echo "make-dmg: bundle signature invalid, applying ad-hoc signature"
  codesign --force --deep -s - "$STAGE/$APP_BASENAME"
fi

ln -s /Applications "$STAGE/Applications"

HELPER="$STAGE/打不开？双击我.command"
sed "s/@APP_NAME@/${APP_NAME}/g" "$HERE/open-helper.command" > "$HELPER"
chmod 755 "$HELPER"
sed "s/@APP_NAME@/${APP_NAME}/g" "$HERE/install-readme.txt" > "$STAGE/安装说明.txt"
chmod 644 "$STAGE/安装说明.txt"

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
hdiutil create -quiet -volname "$VOLNAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT"
echo "make-dmg: wrote $OUT ($(du -h "$OUT" | cut -f1))"
