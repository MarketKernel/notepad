#!/bin/sh
# Builds target/release/Notepad.app (drag it into /Applications if you like).
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP=target/release/Notepad.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/notepad "$APP/Contents/MacOS/notepad"
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
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict>
</plist>
PLIST
echo "Built $APP"
