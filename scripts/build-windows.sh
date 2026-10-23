#!/usr/bin/env bash
# Cross-builds the Windows portable zip from macOS/Linux:
#   build/MoliTerm-<version>-x86_64-windows.zip  (MoliTerm.exe + `portable` marker)
# Needs: rustup target add x86_64-pc-windows-gnu; mingw-w64 (brew install mingw-w64).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-gnu
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ZIP="build/MoliTerm-${VERSION}-x86_64-windows.zip"

export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
cargo build --release --locked -p moliterm --target "$TARGET"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/MoliTerm"
cp "target/$TARGET/release/moliterm.exe" "$STAGE/MoliTerm/MoliTerm.exe"
: > "$STAGE/MoliTerm/portable"
mkdir -p build
rm -f "$ZIP"
(cd "$STAGE" && zip -qr - MoliTerm) > "$ZIP"

echo "binary: $(du -h "$STAGE/MoliTerm/MoliTerm.exe" | cut -f1)"
echo "built:  $ZIP"
