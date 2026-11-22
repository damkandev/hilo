#!/usr/bin/env bash
# Builds hilo.app (the app plus the hilo CLI inside it) and a .dmg to drag it to Applications.
#
#   app/bundle.sh                     # ad-hoc signed: Gatekeeper asks once ("Open Anyway")
#   SIGN_ID="Developer ID Application: …" app/bundle.sh   # signed for distribution (then notarize)
#
# The app builds from outside the repo's cargo config: that config swaps crates.io for ../vendor
# (the CLI's offline build), and gpui's dependencies are not vendored.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=$ROOT/app/dist
APP=$OUT/hilo.app

cargo build --release --offline --locked
(cd / && cargo build --release --locked --manifest-path "$ROOT/app/Cargo.toml")

rm -rf "$OUT" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp app/target/release/hilo-app target/release/hilo "$APP/Contents/MacOS/"

# the icon, from assets/icon.svg (Quick Look rasterizes SVG), on Apple's grid: the 824px tile
# centered in 1024 with a soft shadow, so it sits the size of the other apps' icons
ICON=$(mktemp -d)
{ echo '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024"><filter id="s"><feGaussianBlur stdDeviation="12"/></filter><rect x="100" y="112" width="824" height="824" rx="181" opacity=".3" filter="url(#s)"/>'
  sed 's/<svg /<svg x="100" y="100" width="824" height="824" /' assets/icon.svg
  echo '</svg>'; } > "$ICON/icon.svg"
qlmanage -t -s 1024 -o "$ICON" "$ICON/icon.svg" >/dev/null 2>&1
mkdir "$ICON/hilo.iconset"
for s in 16 32 128 256 512; do
    sips -z $s $s "$ICON/icon.svg.png" --out "$ICON/hilo.iconset/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$ICON/icon.svg.png" --out "$ICON/hilo.iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICON/hilo.iconset" -o "$APP/Contents/Resources/hilo.icns"
rm -rf "$ICON"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>hilo</string>
  <key>CFBundleDisplayName</key><string>hilo</string>
  <key>CFBundleIdentifier</key><string>dev.hilo.app</string>
  <key>CFBundleExecutable</key><string>hilo-app</string>
  <key>CFBundleIconFile</key><string>hilo</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF

if [ -n "${SIGN_ID:-}" ]; then
    codesign --force --options runtime --timestamp -s "$SIGN_ID" "$APP/Contents/MacOS/hilo" "$APP/Contents/MacOS/hilo-app"
    codesign --force --options runtime --timestamp -s "$SIGN_ID" "$APP"
else
    codesign --force -s - "$APP/Contents/MacOS/hilo" "$APP/Contents/MacOS/hilo-app" && codesign --force -s - "$APP"
fi

# the .dmg: a brushed-metal window with the app, an arrow and Applications (app/dmg.py), laid out
# by dmgbuild (pinned, hash-checked, in a throwaway venv: Python 3.11 or later)
VENV=$(mktemp -d)
"${PYTHON:-python3}" -m venv "$VENV"
"$VENV/bin/pip" install -q --disable-pip-version-check --require-hashes --only-binary :all: -r app/dmg-requirements.txt
# a stable name, so https://github.com/damkandev/hilo/releases/latest/download/hilo-macos.dmg always works
rm -f "$OUT/hilo-macos.dmg"
"$VENV/bin/dmgbuild" -s app/dmg.py -D app="$APP" -D icon="$APP/Contents/Resources/hilo.icns" -D bg="$ROOT/app/assets/dmg-background.tiff" hilo "$OUT/hilo-macos.dmg" >/dev/null
rm -rf "$VENV"
[ -n "${SIGN_ID:-}" ] && codesign -s "$SIGN_ID" "$OUT/hilo-macos.dmg"
echo "$APP"
echo "$OUT/hilo-macos.dmg"
