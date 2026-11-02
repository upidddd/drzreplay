#!/bin/sh
# Build the .deb and publish it as a GitHub release  —  usage: packaging/release.sh
# Needs `gh` logged in. Bump VERSION in src/drzreplay/core/config.py first.
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(PYTHONPATH=src python3 -c 'from drzreplay.core.config import VERSION; print(VERSION)')"
REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
DEB="$(packaging/build-deb.sh)"
OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
cp "$DEB" "$OUT/drzreplay_all.deb"                      # stable name for releases/latest/download
sed "s|@REPO@|$REPO|g" packaging/install.sh > "$OUT/install.sh"
git push origin HEAD
gh release create "v$VERSION" "$DEB" "$OUT/drzreplay_all.deb" "$OUT/install.sh" \
    --title "DrzReplay $VERSION" --notes "${NOTES:-DrzReplay $VERSION}"
echo "https://github.com/$REPO/releases/tag/v$VERSION"
