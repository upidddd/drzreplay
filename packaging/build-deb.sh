#!/bin/sh
# Build dist/drzreplay_<version>_all.deb  —  usage: packaging/build-deb.sh
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(PYTHONPATH="$ROOT/src" python3 -c 'from drzreplay.core.config import VERSION; print(VERSION)')"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
P="$ROOT/packaging"
D="$P/debian"
F="$P/files"

install -d "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/lib/drzreplay" \
    "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/scalable/apps" \
    "$STAGE/usr/share/doc/drzreplay" "$STAGE/etc/xdg/autostart" "$STAGE/etc/sudoers.d"

cp -r "$ROOT/src/drzreplay" "$STAGE/usr/lib/drzreplay/"
find "$STAGE/usr/lib/drzreplay" -name '__pycache__' -prune -exec rm -rf {} +
rm -f "$STAGE/usr/lib/drzreplay/drzreplay/assets/intro/"*.png
install -m 755 "$F/drzreplay.sh" "$STAGE/usr/bin/drzreplay"
install -m 644 "$F/drzreplay.desktop" "$STAGE/usr/share/applications/com.drz.DrzReplay.desktop"
install -m 644 "$F/drzreplay-autostart.desktop" "$STAGE/etc/xdg/autostart/drzreplay.desktop"
install -m 644 "$ROOT/src/drzreplay/assets/icons/drzreplay.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/drzreplay.svg"
{
    echo "DrzReplay $VERSION"
    echo
    echo "Program code: MIT License."
    echo
    cat "$ROOT/src/drzreplay/assets/CREDITS.txt"
} > "$STAGE/usr/share/doc/drzreplay/copyright"
chmod 644 "$STAGE/usr/share/doc/drzreplay/copyright"
# passwordless Update button: a root helper that only installs the official package, and its sudo rule
REPO="$(PYTHONPATH="$ROOT/src" python3 -c 'from drzreplay.core.config import REPO; print(REPO)')"
sed "s#@REPO@#$REPO#" "$F/drzreplay-update" > "$STAGE/usr/lib/drzreplay/drzreplay-update"
install -m 440 "$F/drzreplay.sudoers" "$STAGE/etc/sudoers.d/drzreplay"
VISUDO="$(command -v visudo || echo /usr/sbin/visudo)"
if [ -x "$VISUDO" ]; then
    "$VISUDO" -cqf "$STAGE/etc/sudoers.d/drzreplay"     # a broken file here would break sudo itself
fi
printf '%s\n' /etc/xdg/autostart/drzreplay.desktop /etc/sudoers.d/drzreplay > "$STAGE/DEBIAN/conffiles"
for s in postinst prerm postrm; do install -m 755 "$D/$s" "$STAGE/DEBIAN/$s"; done
SIZE="$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)"
sed -e "s/@VERSION@/$VERSION/" -e "s/@SIZE@/$SIZE/" "$D/control.in" > "$STAGE/DEBIAN/control"

find "$STAGE" -type d -exec chmod 755 {} +
find "$STAGE/usr/lib/drzreplay" -type f -exec chmod 644 {} +
chmod 755 "$STAGE/usr/lib/drzreplay/drzreplay-update"
chmod 440 "$STAGE/etc/sudoers.d/drzreplay"
mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/drzreplay_${VERSION}_all.deb"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
echo "$OUT"
