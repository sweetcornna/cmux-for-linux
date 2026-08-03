#!/usr/bin/env bash
# Build every native Linux package format end to end.
#
# Usage:
#   packaging/linux/build-all.sh [format ...]
#
# With no arguments this builds the binaries, stages the FHS tree, and then
# produces the tarball, .deb, .rpm, AUR directory and AppImage. Naming formats
# explicitly skips the rest, for example:
#
#   packaging/linux/build-all.sh deb
#   CMUX_VERSION=0.64.21 packaging/linux/build-all.sh tarball deb
#
# Set CMUX_SKIP_BUILD=1 to reuse binaries already in build/linux/bin.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch

formats=("$@")
if [ ${#formats[@]} -eq 0 ]; then
  formats=(tarball deb rpm aur appimage)
fi

if [ "${CMUX_SKIP_BUILD:-0}" != 1 ]; then
  "$PKG_DIR/build-binaries.sh"
else
  log "CMUX_SKIP_BUILD=1 — reusing $BUILD_DIR/bin"
fi

"$PKG_DIR/stage-tree.sh"

VERSION="$(resolve_version)"
failed=()
for format in "${formats[@]}"; do
  script="$PKG_DIR/build-$format.sh"
  [ -x "$script" ] || die "unknown format: $format"
  log "--- $format ---"
  if ! "$script"; then
    log "warning: $format failed"
    failed+=("$format")
  fi
done

log "artifacts for $PKG_NAME $VERSION ($RUST_ARCH):"
ls -la "$BUILD_DIR/dist" 2>/dev/null | sed 's/^/    /'

if [ ${#failed[@]} -gt 0 ]; then
  die "these formats failed: ${failed[*]}"
fi
log "all requested formats built"
