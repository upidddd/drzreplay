#!/bin/sh
# Build dist/drzreplay_<version>_<arch>.deb from the release binary  —  usage: packaging/build-deb.sh
# Run `cargo build --release` first.
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
ARCH="$(dpkg --print-architecture)"
BIN="${BIN:-$ROOT/target/release/drzreplay}"
[ -x "$BIN" ] || { echo "no $BIN: run cargo build --release first" >&2; exit 1; }
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
P="$ROOT/packaging"
D="$P/debian"
F="$P/files"

install -d "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/lib/drzreplay/drzreplay" \
    "$STAGE/usr/share/drzreplay/assets/icons" "$STAGE/usr/share/drzreplay/assets/sounds" \
    "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/scalable/apps" \
    "$STAGE/usr/share/doc/drzreplay" "$STAGE/etc/xdg/autostart" "$STAGE/etc/sudoers.d"

install -m 755 "$BIN" "$STAGE/usr/bin/drzreplay"
install -m 644 "$ROOT"/assets/icons/*.svg "$STAGE/usr/share/drzreplay/assets/icons/"
install -m 644 "$ROOT/assets/sounds/click.wav" "$STAGE/usr/share/drzreplay/assets/sounds/"
install -m 644 "$ROOT/assets/icons/drzreplay.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/drzreplay.svg"
# an old Python recorder restarts itself after the upgrade with `python3 -m drzreplay daemon`: hand it over
install -m 644 "$F/compat/__init__.py" "$F/compat/__main__.py" "$STAGE/usr/lib/drzreplay/drzreplay/"
install -m 644 "$F/drzreplay.desktop" "$STAGE/usr/share/applications/com.drz.DrzReplay.desktop"
install -m 644 "$F/drzreplay-autostart.desktop" "$STAGE/etc/xdg/autostart/drzreplay.desktop"
{
    echo "DrzReplay $VERSION"
    echo
    echo "Program code: MIT License."
    echo
    cat "$ROOT/assets/CREDITS.txt"
} > "$STAGE/usr/share/doc/drzreplay/copyright"
chmod 644 "$STAGE/usr/share/doc/drzreplay/copyright"
# passwordless Update button: a root helper that only installs the official package, and its sudo rule
REPO="$(sed -n 's/^pub const REPO: &str = "\(.*\)";/\1/p' "$ROOT/src/config.rs")"
sed "s#@REPO@#$REPO#" "$F/drzreplay-update" > "$STAGE/usr/lib/drzreplay/drzreplay-update"
install -m 440 "$F/drzreplay.sudoers" "$STAGE/etc/sudoers.d/drzreplay"
VISUDO="$(command -v visudo || echo /usr/sbin/visudo)"
if [ -x "$VISUDO" ]; then
    "$VISUDO" -cqf "$STAGE/etc/sudoers.d/drzreplay"     # a broken file here would break sudo itself
fi
printf '%s\n' /etc/xdg/autostart/drzreplay.desktop /etc/sudoers.d/drzreplay > "$STAGE/DEBIAN/conffiles"
for s in postinst prerm postrm; do install -m 755 "$D/$s" "$STAGE/DEBIAN/$s"; done
SIZE="$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)"
sed -e "s/@VERSION@/$VERSION/" -e "s/@ARCH@/$ARCH/" -e "s/@SIZE@/$SIZE/" "$D/control.in" > "$STAGE/DEBIAN/control"

find "$STAGE" -type d -exec chmod 755 {} +
chmod 755 "$STAGE/usr/lib/drzreplay/drzreplay-update"
chmod 440 "$STAGE/etc/sudoers.d/drzreplay"
mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/drzreplay_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$OUT" >/dev/null
echo "$OUT"
