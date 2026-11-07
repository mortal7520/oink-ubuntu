#!/bin/sh
# Modified Ubuntu X11 port of Oink, based on upstream 546f65b2135e.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export CARGO_PROFILE_RELEASE_DEBUG=0
npm ci --engine-strict=false
npm run licenses:check
npm run -w apps/oink build
cargo build --release -p oink-ui -p oink-agent --bin oink --bin oink-backend --bin oink-agent --features custom-protocol
stage="$(mktemp -d)"
trap 'rm -rf -- "$stage"' EXIT HUP INT TERM
pkg="$stage/package"
install -d "$pkg/DEBIAN" "$pkg/usr/lib/oink-ubuntu/resources" "$pkg/usr/bin" "$pkg/usr/share/applications" "$pkg/usr/share/icons/hicolor/128x128/apps" "$pkg/usr/share/doc/oink-ubuntu"
target_root="${CARGO_TARGET_DIR:-target}"
install -m755 "$target_root/release/oink" "$target_root/release/oink-backend" "$target_root/release/oink-agent" "$pkg/usr/lib/oink-ubuntu/"
for notice in LICENSE LICENSE-ADDITIONAL-TERMS.md NOTICE THIRD_PARTY_LICENSES.md third-party-licenses.json; do
    install -m644 "$notice" "$pkg/usr/lib/oink-ubuntu/resources/"
done
cp -R licenses "$pkg/usr/lib/oink-ubuntu/resources/"
install -m644 apps/oink/src-tauri/icons/128x128.png "$pkg/usr/share/icons/hicolor/128x128/apps/oink-ubuntu.png"
install -m644 packaging/linux/oink-ubuntu.desktop "$pkg/usr/share/applications/"
install -m644 README.ubuntu.md "$pkg/usr/share/doc/oink-ubuntu/README.ubuntu.md"
install -m644 VALIDATION.ubuntu.md "$pkg/usr/share/doc/oink-ubuntu/VALIDATION.ubuntu.md"
cat > "$pkg/usr/bin/oink-ubuntu" <<'LAUNCHER'
#!/bin/sh
exec /usr/lib/oink-ubuntu/oink "$@"
LAUNCHER
chmod 755 "$pkg/usr/bin/oink-ubuntu"
version="0.3.3+ubuntu1"
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: oink-ubuntu
Version: $version
Architecture: amd64
Maintainer: Local Ubuntu adaptation
Depends: libc6 (>= 2.39), libcairo2 (>= 1.10.0), libdbus-1-3 (>= 1.9.14), libgcc-s1 (>= 4.2), libgdk-pixbuf-2.0-0 (>= 2.36.9), libglib2.0-0t64 (>= 2.66.0), libgtk-3-0t64 (>= 3.21.5), libjavascriptcoregtk-4.1-0, libpango-1.0-0 (>= 1.22.0), libpangocairo-1.0-0 (>= 1.14.0), libpulse0 (>= 0.99.1), libsoup-3.0-0 (>= 3.0.3), libssl3t64 (>= 3.0.0), libwebkit2gtk-4.1-0 (>= 2.41.90), libx11-6, libxext6, libxfixes3, libxrender1, libayatana-appindicator3-1, libasound2t64, pulseaudio-utils, xdg-utils
Recommends: fonts-noto-cjk
Section: utils
Priority: optional
Description: Modified Oink for Ubuntu 24.04 X11
 Independent local Ubuntu adaptation of the original GPL-3.0 Oink project.
 Shares keyboard, mouse, clipboard and audio with authenticated Oink peers.
CONTROL
node build-support/check-package-licenses.mjs "$pkg/usr/lib/oink-ubuntu" linux-x64
mkdir -p dist-linux
dpkg-deb --root-owner-group --build "$pkg" "dist-linux/oink-ubuntu_${version}_amd64.deb"
install -d "$stage/portable"
cp -R "$pkg/usr/lib/oink-ubuntu" "$stage/portable/oink-ubuntu"
install -m644 README.ubuntu.md "$stage/portable/oink-ubuntu/README.ubuntu.md"
install -m644 VALIDATION.ubuntu.md "$stage/portable/oink-ubuntu/VALIDATION.ubuntu.md"
cat > "$stage/portable/oink-ubuntu/launch-oink.sh" <<'PORTABLE'
#!/bin/sh
cd "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
exec ./oink "$@"
PORTABLE
chmod 755 "$stage/portable/oink-ubuntu/launch-oink.sh"
tar -czf "dist-linux/oink-ubuntu_${version}_amd64.tar.gz" -C "$stage/portable" oink-ubuntu
sha256sum "dist-linux/oink-ubuntu_${version}_amd64.deb" "dist-linux/oink-ubuntu_${version}_amd64.tar.gz"
