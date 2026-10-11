#!/bin/sh
# Construit « FLIR One PC.app » (macOS) dans target/macos, avec son icône.
# Usage : scripts/macos-app.sh [--install]   (--install copie dans /Applications)
set -e
cd "$(dirname "$0")/.."

# Un SDK plus récent que le linker (ex. SDK 27 avec les Command Line Tools 26)
# fait échouer l'édition de liens (« tapi error: unknown architecture ») :
# on garde le SDK par défaut s'il fonctionne, sinon on prend le plus récent qui marche.
if [ -z "$SDKROOT" ]; then
  TEST_C=$(mktemp -t sdktest).c
  echo 'int main(void){return 0;}' > "$TEST_C"
  if ! cc "$TEST_C" -o "$TEST_C.out" 2>/dev/null; then
    SDKS_DIR="$(xcode-select -p)/SDKs"
    [ -d "$SDKS_DIR" ] || SDKS_DIR="$(xcode-select -p)/Platforms/MacOSX.platform/Developer/SDKs"
    for sdk in $(ls -d "$SDKS_DIR"/MacOSX[0-9]*.sdk 2>/dev/null | sort -rV); do
      if SDKROOT="$sdk" cc "$TEST_C" -o "$TEST_C.out" 2>/dev/null; then
        export SDKROOT="$sdk"
        echo "SDK utilisé : $SDKROOT"
        break
      fi
    done
  fi
  rm -f "$TEST_C" "$TEST_C.out"
fi

cargo build --release -p flir-viewer

APP="target/macos/FLIR One PC.app"
ICON_SRC="crates/flir-viewer/assets/icon.png"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/flir-viewer "$APP/Contents/MacOS/flir-viewer"

# Icône : toutes les tailles demandées par macOS, à partir de l'icône 256 px.
ICONSET="target/macos/icon.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for s in 16 32 64 128 256; do
  sips -z $s $s "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
done
cp "$ICONSET/icon_32x32.png" "$ICONSET/icon_16x16@2x.png"
cp "$ICONSET/icon_64x64.png" "$ICONSET/icon_32x32@2x.png"
cp "$ICONSET/icon_256x256.png" "$ICONSET/icon_128x128@2x.png"
rm "$ICONSET/icon_64x64.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>FLIR One PC</string>
  <key>CFBundleDisplayName</key><string>FLIR One PC</string>
  <key>CFBundleIdentifier</key><string>org.flir-one-pc.viewer</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>flir-viewer</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDevelopmentRegion</key><string>fr</string>
</dict>
</plist>
PLIST

# Signature locale (ad hoc) : suffisante pour lancer l'app sur ce Mac.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

echo "Application créée : $APP"
if [ "$1" = "--install" ]; then
  rm -rf "/Applications/FLIR One PC.app"
  cp -R "$APP" /Applications/
  echo "Installée dans /Applications"
fi
