#!/bin/sh
# Builds dist/Transly-<version>.dmg (universal arm64 + Intel). Run on macOS from the repo root.
set -e
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/Transly.app
rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist/transly.iconset dist/dmg

for t in aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build --release --target "$t"
done
lipo -create -output "$APP/Contents/MacOS/transly" \
  target/aarch64-apple-darwin/release/transly target/x86_64-apple-darwin/release/transly

for s in 16 32 128 256 512; do
  sips -z $s $s packaging/icon.png --out dist/transly.iconset/icon_${s}x${s}.png >/dev/null
  sips -z $((s * 2)) $((s * 2)) packaging/icon.png --out dist/transly.iconset/icon_${s}x${s}@2x.png >/dev/null
done
iconutil -c icns dist/transly.iconset -o "$APP/Contents/Resources/transly.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Transly</string>
  <key>CFBundleDisplayName</key><string>Transly</string>
  <key>CFBundleIdentifier</key><string>dev.transly</string>
  <key>CFBundleExecutable</key><string>transly</string>
  <key>CFBundleIconFile</key><string>transly</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST

# ad-hoc signature: arm64 refuses to run completely unsigned code (a real certificate needs a paid Apple account)
codesign --force --deep --sign - "$APP"

cp -R "$APP" dist/dmg/
ln -s /Applications dist/dmg/Applications
for i in 1 2 3; do # hdiutil occasionally fails with "Resource busy" on CI
  hdiutil create -volname Transly -srcfolder dist/dmg -ov -format UDZO "dist/Transly-$VER.dmg" && break
  sleep 5
done
ls -la dist/*.dmg
