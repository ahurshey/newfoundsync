#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Write the current commit to .nfs-git-sha at the repository root, for build.rs to read.
#
# flatpak-builder copies the source without .git, so the build inside it cannot ask git which commit
# it is building and every Flatpak used to report itself as "0.1.3 (unknown)". Run this before any
# flatpak-builder invocation (build-bundle.sh does); it is cheap and safe to run every time.
#
# The stamp gets the same "-dirty" suffix build.rs would add, judged over the same inputs: files that
# end up in the binary. Editing docs or CI does not mark a build dirty.
set -euo pipefail
root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
sha="$(git -C "$root" rev-parse --short=12 HEAD)"
if [ -n "$(git -C "$root" status --porcelain --untracked-files=no -- \
      crates/desktop/src crates/desktop/web crates/desktop/Cargo.toml crates/core Cargo.lock third_party)" ]; then
  sha="${sha}-dirty"
fi
printf '%s\n' "$sha" > "$root/.nfs-git-sha"
echo "stamped $sha"
