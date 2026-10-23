#!/usr/bin/env bash
# Builds build/MoliTerm.app (ad-hoc signed) and build/MoliTerm-<version>-arm64.dmg.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=aarch64-apple-darwin
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=build/MoliTerm.app
DMG="build/MoliTerm-${VERSION}-arm64.dmg"

cargo build --release --locked -p moliterm --target "$TARGET"

rm -rf "$APP" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$TARGET/release/moliterm" "$APP/Contents/MacOS/"
sed "s/@VERSION@/${VERSION}/g" assets/Info.plist > "$APP/Contents/Info.plist"
cp assets/icons/MoliTerm.icns "$APP/Contents/Resources/"
codesign --force --deep --sign - "$APP"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname MoliTerm -srcfolder "$STAGE" -ov -format UDZO "$DMG"

echo "binary: $(du -h "$APP/Contents/MacOS/moliterm" | cut -f1)"
echo "built:  $APP"
echo "built:  $DMG"
