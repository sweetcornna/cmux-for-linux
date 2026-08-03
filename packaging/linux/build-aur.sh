#!/usr/bin/env bash
# Render the AUR PKGBUILD for the current version and, when the release
# tarballs are already present locally, fill in their sha256 digests.
#
# This does not publish to the AUR; it produces the directory you would push to
# ssh://aur@aur.archlinux.org/cmux-bin.git. `makepkg --printsrcinfo` runs when
# makepkg is available, since .SRCINFO must match the PKGBUILD on push.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

VERSION="$(resolve_version)"
OUT_DIR="$BUILD_DIR/dist"
work="$BUILD_DIR/aur"

log "rendering AUR PKGBUILD for $VERSION"
rm -rf "$work"
mkdir -p "$work"

sed -e "s/^pkgver=.*/pkgver=$VERSION/" "$PKG_DIR/aur/PKGBUILD" > "$work/PKGBUILD"

# Substitute real digests when the matching tarball was built locally.
for arch in x86_64 aarch64; do
  tarball="$OUT_DIR/$PKG_NAME-$VERSION-linux-$arch.tar.gz"
  [ -f "$tarball" ] || continue
  digest="$(sha256sum "$tarball" | cut -d' ' -f1)"
  sed -i -e "s/^sha256sums_$arch=.*/sha256sums_$arch=('$digest')/" "$work/PKGBUILD"
  log "sha256sums_$arch = $digest"
done

if command -v makepkg >/dev/null 2>&1; then
  ( cd "$work" && makepkg --printsrcinfo > .SRCINFO )
  log "wrote $work/.SRCINFO"
else
  log "makepkg not available; generate .SRCINFO on an Arch host before pushing"
fi

log "AUR package directory ready at $work"
