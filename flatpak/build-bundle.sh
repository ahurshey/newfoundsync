#!/usr/bin/env bash
# Build the Newfoundsync Flatpak bundle. Run from the repo root on a Linux box with flatpak.
#
# On an immutable host (Bazzite / Silverblue) flatpak-builder cannot be installed into /usr, so it is
# used AS A FLATPAK (org.flatpak.Builder). Everything here is --user scoped: nothing touches the OS
# image. Also passes --disable-rofiles-fuse, which is unreliable on overlayfs.
set -euo pipefail

# Derived, never hardcoded: a hand-maintained version here silently produces a bundle whose filename
# disagrees with the binary inside it, which is exactly how you end up unable to tell whether the
# thing on your Desktop is the build you just made.
VER="$(grep -m1 '^version' crates/desktop/Cargo.toml | cut -d'"' -f2)"
if [ -z "$VER" ]; then echo "cannot read the version from crates/desktop/Cargo.toml" >&2; exit 1; fi
APP=ca.newfoundsync.Newfoundsync
MANIFEST="${1:-flatpak/$APP.yml}"
BUILD="${BUILD_DIR:-$HOME/nfs-flatpak-build}"
REPO="${REPO_DIR:-$HOME/nfs-flatpak-repo}"
OUT="${OUT_DIR:-$HOME/Desktop}/Newfoundsync-${VER}.flatpak"

echo "== building $APP $VER from $MANIFEST"
flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --user -y --noninteractive flathub org.flatpak.Builder \
  org.freedesktop.Platform//24.08 org.freedesktop.Sdk//24.08
rm -rf "$BUILD" "$REPO"
flatpak run org.flatpak.Builder --user --force-clean --disable-rofiles-fuse \
  --repo="$REPO" "$BUILD" "$MANIFEST"

mkdir -p "$(dirname "$OUT")"
flatpak build-bundle "$REPO" "$OUT" "$APP"
flatpak install --user -y --noninteractive --reinstall "$OUT"

echo
echo "== installed:"
flatpak info "$APP" | grep -E '^ *(ID|Version|Branch):'
ls -lh "$OUT"
echo
echo "Run:  flatpak run $APP"
