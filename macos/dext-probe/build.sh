#!/bin/sh
# Construit FlirProbeLoader.app (avec le driver FlirProbe.dext intégré).
# Usage : sh build.sh            (signature locale ad hoc ; SIP désactivé requis)
set -e
cd "$(dirname "$0")"
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
SDK=$(xcrun --sdk driverkit --show-sdk-path)
FW="$SDK/System/DriverKit/System/Library/Frameworks"
OUT=build
rm -rf "$OUT"; mkdir -p "$OUT/gen"

xcrun --sdk driverkit iig --def FlirProbe.iig --header "$OUT/gen/FlirProbe.h" --impl "$OUT/gen/FlirProbe.iig.cpp" \
  -- -isysroot "$SDK" -x c++ -std=gnu++20 -D__IIG=1 -iframework "$FW"

CXXFLAGS="-target arm64e-apple-driverkit21.0 -isysroot $SDK -iframework $FW -I$OUT/gen -std=gnu++20 -O2 -fno-exceptions -Wall"
xcrun --sdk driverkit clang++ $CXXFLAGS -c FlirProbe.cpp -o "$OUT/FlirProbe.o"
xcrun --sdk driverkit clang++ $CXXFLAGS -c "$OUT/gen/FlirProbe.iig.cpp" -o "$OUT/FlirProbe.iig.o"

DEXT="$OUT/org.flironepc.FlirProbe.dext"
mkdir -p "$DEXT"
xcrun --sdk driverkit clang++ -target arm64e-apple-driverkit21.0 -isysroot "$SDK" -F "$FW" \
  -framework DriverKit -framework USBDriverKit \
  "$OUT/FlirProbe.o" "$OUT/FlirProbe.iig.o" -o "$DEXT/org.flironepc.FlirProbe"
cp Info-dext.plist "$DEXT/Info.plist"
codesign --force --sign - --entitlements dext.entitlements --timestamp=none "$DEXT"

APP="$OUT/FlirProbeLoader.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Library/SystemExtensions"
xcrun --sdk macosx swiftc -O Loader.swift -o "$APP/Contents/MacOS/FlirProbeLoader"
cp Info-app.plist "$APP/Contents/Info.plist"
cp -R "$DEXT" "$APP/Contents/Library/SystemExtensions/"
codesign --force --sign - --entitlements app.entitlements --timestamp=none "$APP"
echo "OK : $APP"
