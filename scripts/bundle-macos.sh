#!/usr/bin/env bash
# Assemble Daouchi.app and a drag-to-Applications Daouchi-<version>-<arch>.dmg (the Tauri CLI is not required).
#   scripts/bundle-macos.sh [debug|release]      (default: release)
# Output: $CARGO_TARGET_DIR/bundle/macos/{Daouchi.app,Daouchi-<version>-<arch>.dmg}  (never inside the repo).
# Signing: ad-hoc by default (runs on the Mac that built it; other Macs need a right-click > Open the first time).
#   CODESIGN_IDENTITY="Developer ID Application: Name (TEAMID)" scripts/bundle-macos.sh   signs for distribution
#   (notarization steps: docs/DESKTOP_PACKAGING.md).
# ONNX Runtime: $ORT_DYLIB_PATH (default ~/.daouchi/lib/libonnxruntime.dylib) is copied to
# Contents/Frameworks/ under its resolved file name (for example libonnxruntime.1.22.1.dylib), plus a
# libonnxruntime.dylib symlink, which is the name the runtime probes.
set -euo pipefail
PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/daouchi-target}"
ORT_SRC="${ORT_DYLIB_PATH:-$HOME/.daouchi/lib/libonnxruntime.dylib}"

# Resolve symlinks (the library is often libonnxruntime.dylib -> libonnxruntime.<version>.dylib).
resolve() {
  local p="$1" t
  while [ -L "$p" ]; do
    t="$(readlink "$p")"
    case "$t" in /*) p="$t" ;; *) p="$(dirname "$p")/$t" ;; esac
  done
  echo "$(cd "$(dirname "$p")" && pwd -P)/$(basename "$p")"
}
ORT_REAL="$(resolve "$ORT_SRC")"
[ -f "$ORT_REAL" ] || { echo "ONNX Runtime dylib not found: $ORT_SRC" >&2; exit 1; }
ORT_NAME="$(basename "$ORT_REAL")"   # e.g. libonnxruntime.1.22.1.dylib

cd "$ROOT"
if [ "$PROFILE" = release ]; then
  cargo build --release -p daouchi-desktop --features custom-protocol
else
  cargo build -p daouchi-desktop
fi
BIN="$CARGO_TARGET_DIR/$PROFILE/daouchi-desktop"

APP="$CARGO_TARGET_DIR/bundle/macos/Daouchi.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Daouchi"
cp "$ORT_REAL" "$APP/Contents/Frameworks/$ORT_NAME"
ln -s "$ORT_NAME" "$APP/Contents/Frameworks/libonnxruntime.dylib"
cp "$ROOT/crates/daouchi-desktop/icons/icon.icns" "$APP/Contents/Resources/icon.icns"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/daouchi-desktop/Cargo.toml" | head -1)"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Daouchi</string>
  <key>CFBundleDisplayName</key><string>Daouchi</string>
  <key>CFBundleExecutable</key><string>Daouchi</string>
  <key>CFBundleIdentifier</key><string>com.daouchi.app</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
# Sign inside-out: the bundled ONNX Runtime library first, then the app. Apple Silicon refuses unsigned code.
SIGN_ID="${CODESIGN_IDENTITY:--}"
SIGN_OPTS=(--force --sign "$SIGN_ID")
[ "$SIGN_ID" != "-" ] && SIGN_OPTS+=(--options runtime --timestamp)
codesign "${SIGN_OPTS[@]}" "$APP/Contents/Frameworks/$ORT_NAME"
codesign "${SIGN_OPTS[@]}" "$APP"
codesign --verify --deep --strict "$APP"

# Disk image with an Applications shortcut for drag-install.
ARCH="$(uname -m)"
DMG="$CARGO_TARGET_DIR/bundle/macos/Daouchi-${VERSION}-${ARCH}.dmg"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "Daouchi" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"
[ "$SIGN_ID" != "-" ] && codesign --force --sign "$SIGN_ID" --timestamp "$DMG"

echo "Built: $APP"
echo "Disk image: $DMG"
[ "$SIGN_ID" = "-" ] && echo "Ad-hoc signed only. Distribution signing and notarization: docs/DESKTOP_PACKAGING.md"
