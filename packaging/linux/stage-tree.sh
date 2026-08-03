#!/usr/bin/env bash
# Lay out the FHS tree that every Linux package format installs.
#
# Every packager (deb, rpm, AUR, AppImage, tarball) consumes this one tree, so
# the installed layout is identical across formats and only the metadata
# differs. Run build-binaries.sh first.
#
# Usage: stage-tree.sh [destdir]
#   destdir defaults to build/linux/stage

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

STAGE="${1:-$BUILD_DIR/stage}"
VERSION="$(resolve_version)"

for bin in cmux-tui cmux-relay; do
  [ -x "$BUILD_DIR/bin/$bin" ] || \
    die "missing $BUILD_DIR/bin/$bin — run packaging/linux/build-binaries.sh first"
done

log "staging $PKG_NAME $VERSION into $STAGE"
rm -rf "$STAGE"
mkdir -p \
  "$STAGE/usr/bin" \
  "$STAGE/usr/share/applications" \
  "$STAGE/usr/share/man/man1" \
  "$STAGE/usr/share/bash-completion/completions" \
  "$STAGE/usr/share/zsh/site-functions" \
  "$STAGE/usr/share/fish/vendor_completions.d" \
  "$STAGE/usr/share/doc/$PKG_NAME" \
  "$STAGE/usr/share/licenses/$PKG_NAME"

# Binaries. `cmux` is the user-facing command name and matches the name the
# upstream npm and PyPI packages install; cmux-tui stays available so scripts
# written against the upstream binary name keep working.
install -m 0755 "$BUILD_DIR/bin/cmux-tui" "$STAGE/usr/bin/cmux-tui"
install -m 0755 "$BUILD_DIR/bin/cmux-relay" "$STAGE/usr/bin/cmux-relay"
ln -sf cmux-tui "$STAGE/usr/bin/cmux"

# Desktop entry + icons.
install -m 0644 "$PKG_DIR/common/cmux.desktop" "$STAGE/usr/share/applications/cmux.desktop"
for size in 16 32 128 256 512; do
  src="$REPO_ROOT/Assets.xcassets/AppIcon.appiconset/$size.png"
  [ -f "$src" ] || continue
  mkdir -p "$STAGE/usr/share/icons/hicolor/${size}x${size}/apps"
  install -m 0644 "$src" "$STAGE/usr/share/icons/hicolor/${size}x${size}/apps/cmux.png"
done

# Man page. The version line is substituted at stage time so packages never
# claim a version the binary does not report.
sed -e "s/@VERSION@/$VERSION/g" "$PKG_DIR/common/cmux.1.in" \
  > "$STAGE/usr/share/man/man1/cmux.1"
gzip -9n "$STAGE/usr/share/man/man1/cmux.1"

# Shell completions.
install -m 0644 "$PKG_DIR/common/completions/cmux.bash" \
  "$STAGE/usr/share/bash-completion/completions/cmux"
install -m 0644 "$PKG_DIR/common/completions/_cmux" \
  "$STAGE/usr/share/zsh/site-functions/_cmux"
install -m 0644 "$PKG_DIR/common/completions/cmux.fish" \
  "$STAGE/usr/share/fish/vendor_completions.d/cmux.fish"

# Documentation and licensing. cmux is GPL-3.0-or-later, so the complete
# license text ships with every binary package.
install -m 0644 "$REPO_ROOT/LICENSE" "$STAGE/usr/share/licenses/$PKG_NAME/LICENSE"
install -m 0644 "$REPO_ROOT/LICENSE" "$STAGE/usr/share/doc/$PKG_NAME/copyright"
install -m 0644 "$REPO_ROOT/THIRD_PARTY_LICENSES.md" \
  "$STAGE/usr/share/doc/$PKG_NAME/THIRD_PARTY_LICENSES.md"
install -m 0644 "$TUI_DIR/README.md" "$STAGE/usr/share/doc/$PKG_NAME/README.md"

printf '%s\n' "$VERSION" > "$BUILD_DIR/VERSION"
log "staged tree ready ($(du -sh "$STAGE" | cut -f1))"
