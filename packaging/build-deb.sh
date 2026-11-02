#!/bin/sh
# Build dist/drzreplay_<version>_all.deb  —  usage: packaging/build-deb.sh
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(PYTHONPATH="$ROOT/src" python3 -c 'from drzreplay.config import VERSION; print(VERSION)')"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
P="$ROOT/packaging"

install -d "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/lib/drzreplay" \
    "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/scalable/apps" \
    "$STAGE/usr/share/doc/drzreplay" "$STAGE/etc/xdg/autostart"

cp -r "$ROOT/src/drzreplay" "$ROOT/src/assets" "$STAGE/usr/lib/drzreplay/"
find "$STAGE/usr/lib/drzreplay" -name '__pycache__' -prune -exec rm -rf {} +
rm -f "$STAGE/usr/lib/drzreplay/assets/intro/"*.png
install -m 755 "$P/drzreplay.sh" "$STAGE/usr/bin/drzreplay"
install -m 644 "$P/drzreplay.desktop" "$STAGE/usr/share/applications/com.drz.DrzReplay.desktop"
install -m 644 "$P/drzreplay-autostart.desktop" "$STAGE/etc/xdg/autostart/drzreplay.desktop"
install -m 644 "$ROOT/src/assets/icons/drzreplay.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/drzreplay.svg"
{
    echo "DrzReplay $VERSION"
    echo
    echo "Program code: MIT License."
    echo
    cat "$ROOT/src/assets/CREDITS.txt"
} > "$STAGE/usr/share/doc/drzreplay/copyright"
chmod 644 "$STAGE/usr/share/doc/drzreplay/copyright"
echo "/etc/xdg/autostart/drzreplay.desktop" > "$STAGE/DEBIAN/conffiles"
for s in postinst prerm postrm; do install -m 755 "$P/$s" "$STAGE/DEBIAN/$s"; done
SIZE="$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)"
sed -e "s/@VERSION@/$VERSION/" -e "s/@SIZE@/$SIZE/" "$P/control.in" > "$STAGE/DEBIAN/control"

find "$STAGE" -type d -exec chmod 755 {} +
find "$STAGE/usr/lib/drzreplay" -type f -exec chmod 644 {} +
mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/drzreplay_${VERSION}_all.deb"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
echo "$OUT"
