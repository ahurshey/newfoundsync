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
# The desktop bundle includes PipeWire capture and the VA-API AV1 Auto/CPU fallback. Pass a
# manifest explicitly only when intentionally making the smaller audio/web-relay-only Flatpak.
MANIFEST="${1:-flatpak/$APP.screencast.yml}"
BUILD="${BUILD_DIR:-$HOME/nfs-flatpak-build}"
# TWO repo paths, because the process that WRITES the repo and the process that READS it are both
# sandboxed, and no single path satisfies both.
#
# REPO is where flatpak-builder writes. It must be a normal host path: flatpak-builder here runs as
# org.flatpak.Builder, i.e. itself a Flatpak, and every Flatpak gets a PRIVATE /var/tmp -- that is
# part of the sandbox's tmpfs setup and it overrides even `--filesystem=host`, which org.flatpak.Builder
# does hold. Point --repo at /var/tmp and the build "succeeds" while writing into a scratch directory
# that evaporates when the builder exits, and the next command fails with:
#   error: '/var/tmp/nfs-flatpak-repo' is not a valid repository
REPO="${REPO_DIR:-$HOME/nfs-flatpak-repo}"
# SERVE_REPO is the copy the file:// remote actually points at, and it must NOT be under $HOME. The
# reader is a software centre, itself a sandboxed Flatpak; Bazaar's filesystem permissions are roughly
# /var/tmp, /var/lib/flatpak, ~/.var/app and xdg-data/flatpak. $HOME is not among them and is mode 700
# on most setups besides, so from in there the store cannot read the ostree summary and cannot list the
# remote's refs:
#   Failed to download optional summary: Remote listing for <remote> not available;
#     server has no summary file. Check the URL passed to remote-add was valid.
#   Warning: ... No such ref 'app/<app>/x86_64/master' in remote <remote>
# It still lists the app (it is installed) but has no remote component to take an icon from, so you get
# the generic gears placeholder. /var/tmp is world-readable (1777) and, unlike /tmp, survives a reboot.
SERVE_REPO="${SERVE_REPO_DIR:-/var/tmp/nfs-flatpak-repo}"
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

# Publish the built repo where a sandboxed software centre can actually read it (see SERVE_REPO).
# Replaced wholesale rather than synced: an ostree repo is content-addressed, so a stale objects/ dir
# left behind from a previous build is dead weight the remote would keep advertising.
rm -rf "$SERVE_REPO"
cp -a "$REPO" "$SERVE_REPO"
# The store reads this as an unprivileged sandboxed process, so every directory needs +x to traverse
# and every file +r. cp -a preserves the build-time mode, which is not guaranteed to be world-readable.
chmod -R a+rX "$SERVE_REPO"

# INSTALL SYSTEM-WIDE FROM THE REPO AS A REMOTE. Both halves of that matter, and getting either wrong
# is what makes the app show a generic gears icon in a software centre while its name, summary and
# description all render correctly (which is what makes it so confusing to chase).
#
# 1. AS A REMOTE, NOT FROM THE BUNDLE FILE. `flatpak install <file>.flatpak` invents a throwaway
#    origin ("<app>-origin") flagged `disabled,no-enumerate` with an EMPTY url and no appstream index.
#    Note that plain `flatpak remotes` HIDES disabled remotes, so that leftover origin is invisible
#    unless you pass --show-disabled.
#
# 2. SYSTEM, NOT USER -- because of a Bazaar limitation, not anything about our packaging.
#    bz-flatpak-instance.c does:
#        #ifdef SANDBOXED_LIBFLATPAK
#          if (is_noenumerate || installation == self->user)
#        #else
#          if (is_noenumerate)
#        #endif
#            retrieve_refs_for_noenumerable_remote (...)   /* never loads appstream */
#    Bazaar's Flathub build defines SANDBOXED_LIBFLATPAK, so EVERY remote in the USER installation is
#    forced down the non-enumerable path that never loads appstream data -- and therefore never finds
#    an icon, no matter how correct the metadata is. Only system installations get appstream loaded.
#    Flathub apps show icons because Flathub is a system remote. Its log tells on itself:
#        Found 1 installed apps from non-enumerable remote 'newfoundsync-local'
#
# The `--system` operations need polkit authorisation, so expect an auth prompt here. That is the
# price of the icon being right; drop back to --user everywhere below if you would rather not.
#
# flatpak-builder already exported appstream data into $REPO. This materialises
# /var/lib/flatpak/appstream/$REMOTE/, including the scaled PNGs the store actually draws. Bazaar
# builds its icon path as <appstream_dir>/icons/flatpak/<W>x<H>/<file>, so verify with:
#   ls /var/lib/flatpak/appstream/$REMOTE/*/active/icons/flatpak/128x128/
REMOTE="${REMOTE_NAME:-newfoundsync-local}"
flatpak remote-add --system --no-gpg-verify --if-not-exists "$REMOTE" "file://$SERVE_REPO"
# Point an existing remote at this build's repo, in case $REPO moved since it was first added.
flatpak remote-modify --system --no-gpg-verify --url="file://$SERVE_REPO" "$REMOTE"
flatpak install --system -y --noninteractive --reinstall "$REMOTE" "$APP"
# Must come AFTER the install: the index is per-remote, and this is the step the store reads.
flatpak update --appstream --system "$REMOTE"

# A leftover --user install SHADOWS the system one we just made, putting the generic icon right back.
if flatpak list --user --app --columns=application 2>/dev/null | grep -qx "$APP"; then
  echo
  echo "!! WARNING: $APP is ALSO installed per-user, which shadows the system install"
  echo "!! above and puts the generic icon back. Remove the duplicate with:"
  echo "!!   flatpak uninstall --user -y $APP"
fi
# Same trap in the other direction: opening the bundle in a software centre re-creates the sideload
# origin that started all this.
if flatpak remotes --system --show-disabled --columns=name 2>/dev/null | grep -qx "${APP}-origin"; then
  echo
  echo "!! WARNING: the sideload origin ${APP}-origin is back, which means the bundle was opened"
  echo "!! in a software centre instead of installed by this script. It has no appstream index."
  echo "!!   flatpak uninstall --system -y $APP && flatpak remote-delete --system --force ${APP}-origin"
fi

echo
echo "== installed:"
flatpak info "$APP" | grep -E '^ *(ID|Version|Branch|Origin):'
echo "== appstream icons the store will draw:"
# /var/lib/flatpak, not ~/.local/share/flatpak -- we install to the SYSTEM installation above, and the
# appstream index is per-installation. An empty list here means the store will fall back to a generic
# icon, so treat it as the check that the whole remote/appstream path actually worked.
find "/var/lib/flatpak/appstream/$REMOTE" -name '*.png' 2>/dev/null | sed 's/^/   /'
ls -lh "$OUT"
echo
echo "Run:  flatpak run $APP"
