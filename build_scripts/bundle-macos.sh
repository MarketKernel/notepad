#!/bin/sh
# Builds target/release/Notepad.app (drag it into /Applications if you like).
# --universal builds target/universal-apple-darwin/release/Notepad.app, which
# runs on both Apple Silicon and Intel Macs
# (needs: rustup target add aarch64-apple-darwin x86_64-apple-darwin).
set -e
cd "$(dirname "$0")/.."
# The [package] version, i.e. the first `version = "..."` line in Cargo.toml.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || { echo "No version in Cargo.toml" >&2; exit 1; }
if [ "$1" = "--universal" ]; then
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  OUT=target/universal-apple-darwin/release
  mkdir -p "$OUT"
  lipo -create -output "$OUT/notepad" \
    target/aarch64-apple-darwin/release/notepad \
    target/x86_64-apple-darwin/release/notepad
else
  cargo build --release
  OUT=target/release
fi
APP=$OUT/Notepad.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$OUT/notepad" "$APP/Contents/MacOS/notepad"
# App icon from assets/icon.png (redraw it with build_scripts/make_icon.py).
mkdir -p "$APP/Contents/Resources"
ICONSET=$(mktemp -d)/Notepad.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Notepad.icns"
rm -rf "$(dirname "$ICONSET")"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Notepad</string>
  <key>CFBundleDisplayName</key><string>Notepad</string>
  <key>CFBundleIdentifier</key><string>local.notepad.rs</string>
  <key>CFBundleExecutable</key><string>notepad</string>
  <key>CFBundleIconFile</key><string>Notepad</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <!-- Puts Notepad into Finder's "Open With" for text (and any other) files
       without making it the default app; Get Info → "Change All…" does that. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Document</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.text</string>
        <string>public.data</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST
echo "Built $APP ($VERSION)"
