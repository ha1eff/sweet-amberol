#!/usr/bin/env bash
# Build a Debian package from the current Meson build tree.
#
# Usage:
#   ./packaging/build-deb.sh              # version 2026.2+local
#   ./packaging/build-deb.sh local4       # version 2026.2+local4
#
# The package installs under /usr/local, matching the prefix baked into
# src/config.rs by Meson. Run `meson compile -C build` first if the sources
# changed (this script also does it).

set -euo pipefail

cd "$(dirname "$0")/.."

BUILD_DIR="build"
SUFFIX="${1:-local}"
VERSION="$(sed -n "s/^[[:space:]]*version:[[:space:]]*'\([^']*\)'.*/\1/p" meson.build | head -1)"
if [ -z "$VERSION" ]; then
    echo "error: unable to read project version from meson.build" >&2
    exit 1
fi
PKG_VERSION="${VERSION}+${SUFFIX}"
ARCH="$(dpkg --print-architecture)"

DIST="$PWD/dist"
STAGE="$DIST/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE" "$DIST"

echo "==> Compiling"
meson compile -C "$BUILD_DIR"

echo "==> Staging install tree"
DESTDIR="$STAGE" meson install --no-rebuild -C "$BUILD_DIR" >/dev/null

echo "==> Stripping binary"
strip --strip-unneeded "$STAGE/usr/local/bin/amberol"

echo "==> Computing library dependencies"
# dpkg-shlibdeps wants a debian/control next to the current directory.
mkdir -p "$DIST/debian"
cat > "$DIST/debian/control" <<'CONTROL'
Source: amberol
Section: sound
Priority: optional
Maintainer: Local Build <local@localhost>
Standards-Version: 4.6.2

Package: amberol
Architecture: any
Depends: ${shlibs:Depends}, ${misc:Depends}
Description: Plays music, and nothing else
 Local build of Amberol.
CONTROL
SHLIBS="$(cd "$DIST" && dpkg-shlibdeps -O "$STAGE/usr/local/bin/amberol" 2>/dev/null \
    | sed -n 's/^shlibs:Depends=//p')"
rm -rf "$DIST/debian"
if [ -z "$SHLIBS" ]; then
    echo "error: dpkg-shlibdeps produced no dependencies" >&2
    exit 1
fi

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: amberol
Version: ${PKG_VERSION}
Section: sound
Priority: optional
Architecture: ${ARCH}
Maintainer: Local Build <local@localhost>
Installed-Size: $(du -sk "$STAGE/usr" | cut -f1)
Depends: ${SHLIBS}, dconf-gsettings-backend | gsettings-backend, gstreamer1.0-plugins-good, gstreamer1.0-plugins-bad
Description: Plays music, and nothing else
 Local build of Amberol with the Lollypop-style album grid,
 SQLite library index, cached album covers and Russian translation.
CONTROL

cat > "$STAGE/DEBIAN/postinst" <<'SCRIPT'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    if command -v glib-compile-schemas >/dev/null 2>&1; then
        glib-compile-schemas /usr/local/share/glib-2.0/schemas || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f /usr/local/share/icons/hicolor || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/local/share/applications || true
    fi
fi
exit 0
SCRIPT

cat > "$STAGE/DEBIAN/postrm" <<'SCRIPT'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
    if command -v glib-compile-schemas >/dev/null 2>&1; then
        glib-compile-schemas /usr/local/share/glib-2.0/schemas || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t -f /usr/local/share/icons/hicolor || true
    fi
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q /usr/local/share/applications || true
    fi
fi
exit 0
SCRIPT

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/postrm"
chmod 644 "$STAGE/DEBIAN/control"

DEB="$DIST/amberol_${PKG_VERSION}_${ARCH}.deb"
rm -f "$DEB"
echo "==> Building package"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
rm -rf "$STAGE"

echo
echo "Package: $DEB"
ls -lh "$DEB"
sha256sum "$DEB"
