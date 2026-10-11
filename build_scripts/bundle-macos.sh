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
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Notepad</string>
  <key>CFBundleDisplayName</key><string>Notepad</string>
  <key>CFBundleIdentifier</key><string>local.notepad.rs</string>
  <key>CFBundleExecutable</key><string>notepad</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST
echo "Built $APP ($VERSION)"
