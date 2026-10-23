#!/usr/bin/env bash
# Builds build/moliterm_<version>_amd64.deb (x86_64).
# On Linux x86_64 it builds natively (needs: rustup stable, pkg-config,
# libwayland-dev, libxkbcommon-dev, dpkg-deb). Elsewhere (the dev Mac) it runs
# itself inside a linux/amd64 Docker container, reusing the local crate cache
# so no network is needed for crates.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DEB="build/moliterm_${VERSION}_amd64.deb"

if [ "$(uname -s)-$(uname -m)" != "Linux-x86_64" ]; then
  exec docker run --rm --platform linux/amd64 \
    -v "$PWD":/src -w /src \
    -v "${CARGO_HOME:-$HOME/.cargo}/registry":/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/target/linux-amd64 -e CARGO_BUILD_JOBS=4 \
    rust:1-bookworm bash -c '
      apt-get update -qq && apt-get install -y -qq pkg-config libwayland-dev libxkbcommon-dev >/dev/null &&
      scripts/build-linux.sh'
fi

TARGET_DIR=${CARGO_TARGET_DIR:-target}
cargo build --release --locked --offline -p moliterm

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
install -Dm755 "$TARGET_DIR/release/moliterm" "$STAGE/usr/bin/moliterm"
install -Dm644 assets/icons/moliterm-256.png "$STAGE/usr/share/icons/hicolor/256x256/apps/moliterm.png"
mkdir -p "$STAGE/usr/share/applications" "$STAGE/DEBIAN"
cat > "$STAGE/usr/share/applications/moliterm.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=MoliTerm
Comment=SSH, SFTP, RDP, Telnet and serial client
Exec=moliterm
Icon=moliterm
Terminal=false
Categories=Network;RemoteAccess;System;TerminalEmulator;
StartupWMClass=MoliTerm
EOF
SIZE=$(du -sk "$STAGE/usr" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: moliterm
Version: ${VERSION}
Architecture: amd64
Maintainer: MoliTerm
Installed-Size: ${SIZE}
Depends: libc6 (>= 2.31), libxkbcommon0, libwayland-client0
Recommends: libvulkan1 | libgl1
Section: net
Priority: optional
Description: Remote access client (SSH, SFTP, RDP, Telnet, serial)
 Tabs, splits, multi-exec, tunnels, an encrypted vault and an SFTP panel.
EOF
mkdir -p build
dpkg-deb --root-owner-group --build "$STAGE" "$DEB" >/dev/null
echo "binary: $(du -h "$STAGE/usr/bin/moliterm" | cut -f1)"
echo "built:  $DEB"
