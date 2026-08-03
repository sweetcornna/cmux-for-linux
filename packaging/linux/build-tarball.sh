#!/usr/bin/env bash
# Build the portable .tar.gz plus its install.sh.
#
# The archive holds the same complete TUI, relay and GTK4 FHS tree every other
# format installs, rooted at the archive top level, so
# `install.sh --prefix /usr/local` is a plain copy.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch
STAGE="${CMUX_STAGE:-$BUILD_DIR/stage}"
VERSION="$(resolve_version)"
OUT_DIR="$BUILD_DIR/dist"

[ -d "$STAGE/usr/bin" ] || die "no staged tree at $STAGE — run packaging/linux/stage-tree.sh first"

name="$PKG_NAME-$VERSION-linux-$RUST_ARCH"
work="$BUILD_DIR/tarball/$name"

log "building $name.tar.gz"
rm -rf "$BUILD_DIR/tarball"
mkdir -p "$work" "$OUT_DIR"

# Copy the tree without the leading usr/ so the archive is prefix-relative.
cp -a "$STAGE/usr/." "$work/"
install -m 0755 "$PKG_DIR/common/install.sh" "$work/install.sh"
install -m 0644 "$REPO_ROOT/LICENSE" "$work/LICENSE"

tar --numeric-owner --owner=0 --group=0 --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH:-0}" \
    -czf "$OUT_DIR/$name.tar.gz" -C "$BUILD_DIR/tarball" "$name"

( cd "$OUT_DIR" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )

log "wrote $OUT_DIR/$name.tar.gz"
