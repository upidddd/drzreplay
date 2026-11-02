#!/bin/sh
# DrzReplay installer / updater for Debian 13+ / Ubuntu 24.04+
#
#   curl -fsSL https://github.com/@REPO@/releases/latest/download/install.sh | sh
#
# Next to a drzreplay_*.deb (from the zip) it installs that file,
# otherwise it downloads the latest release.
set -e
REPO="@REPO@"

DEB=""
case "$0" in
    *install.sh) DEB="$(ls -1 "$(dirname "$0")"/drzreplay_*_all.deb 2>/dev/null | sort -V | tail -1)" ;;
esac
if [ -z "$DEB" ]; then
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    DEB="$TMP/drzreplay_all.deb"
    URL="https://github.com/$REPO/releases/latest/download/drzreplay_all.deb"
    echo "Downloading DrzReplay…"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --progress-bar -o "$DEB" "$URL"
    else
        wget -q --show-progress -O "$DEB" "$URL"
    fi
    chmod 644 "$DEB"
    chmod 755 "$TMP"        # apt's sandbox user must be able to read it
fi

UPDATE=""
if dpkg -s drzreplay >/dev/null 2>&1; then
    UPDATE=1
fi
echo "Installing (asks for your password)…"
sudo apt-get install -y "$DEB" </dev/null    # keep apt off the piped script; sudo asks on the terminal itself
echo
if [ -n "$I3SOCK" ] || echo "$XDG_CURRENT_DESKTOP" | grep -qi i3; then
    echo "i3: start a notification daemon in your i3 config if you have none:"
    echo "    exec --no-startup-id dunst"
    echo
fi
if [ -n "$UPDATE" ]; then
    sleep 1
    drzreplay start >/dev/null 2>&1 || true     # recorders older than 1.1.2 stop on upgrade
    echo "Updated to $(drzreplay --version 2>/dev/null). Settings and clips are kept."
else
    echo "Done! Opening DrzReplay…"
    (drzreplay >/dev/null 2>&1 &)
fi
