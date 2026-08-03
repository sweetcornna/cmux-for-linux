#!/usr/bin/env bash
# Build the Debian/Ubuntu .deb from the staged FHS tree.
#
# Shared-library dependencies come from dpkg-shlibdeps when dpkg-dev is
# installed. Without it the package falls back to a conservative libc6 floor,
# which is correct for a statically linked or near-static build but should not
# be relied on for archive uploads.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch
need dpkg-deb
DEB_PKG_NAME="$PKG_NAME"
STAGE="${CMUX_STAGE:-$BUILD_DIR/stage}"
VERSION="$(resolve_version)"
OUT_DIR="$BUILD_DIR/dist"
DEB_VERSION="${VERSION}-1"

DEB_SUMMARY="$PKG_SUMMARY"
DEB_DESCRIPTION=" cmux keeps a tree of machines, sessions, workspaces, screens, panes, tabs,
 terminals and browsers, and exposes them through a noun-first CLI and a
 terminal UI. Terminal emulation is handled by libghostty-vt.
 .
 This package is built from the cmux for linux fork and ships the
 cmux-tui multiplexer, the cmux-relay transport primitive, the cmux-gtk GTK4
 frontend, a man page, shell completions, TUI and GUI desktop entries, and
 \"New cmux window here\" / \"New cmux workspace here\" context-menu entries
 for Nautilus, Nemo, Dolphin and Caja.
 .
 The Nautilus entries need python3-nautilus; without it the extension file is
 simply never loaded."

[ -d "$STAGE/usr/bin" ] || die "no staged tree at $STAGE — run packaging/linux/stage-tree.sh first"

work="$BUILD_DIR/deb/$DEB_PKG_NAME"
log "building $DEB_PKG_NAME $DEB_VERSION ($DEB_ARCH) .deb"
rm -rf "$work"
mkdir -p "$work" "$OUT_DIR"
cp -a "$STAGE/." "$work/"
mkdir -p "$work/DEBIAN"

# Debian keeps licences under /usr/share/doc/<pkg>/copyright only; drop the
# /usr/share/licenses copy the RPM and Arch layouts use.
rm -rf "$work/usr/share/licenses"

# Resolve the runtime dependency line.
depends="libc6 (>= 2.34), libgtk-4-1"
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
  log "resolving shared library dependencies with dpkg-shlibdeps"
  mkdir -p "$work/debian"
  : > "$work/debian/control"
  if resolved="$(cd "$work" && dpkg-shlibdeps -O --ignore-missing-info \
      usr/bin/cmux-tui usr/bin/cmux-relay usr/bin/cmux-gtk 2>/dev/null)"; then
    resolved="${resolved#shlibs:Depends=}"
    [ -n "$resolved" ] && depends="$resolved"
  else
    log "warning: dpkg-shlibdeps failed; keeping the conservative default"
  fi
  rm -rf "$work/debian"
fi
log "Depends: $depends"

installed_size="$(du -sk "$work" --exclude=DEBIAN | cut -f1)"

cat > "$work/DEBIAN/control" <<EOF
Package: $DEB_PKG_NAME
Version: $DEB_VERSION
Architecture: $DEB_ARCH
Maintainer: $PKG_MAINTAINER
Installed-Size: $installed_size
Depends: $depends
Conflicts: cmux-gtk
Replaces: cmux-gtk
Provides: cmux-gtk
Section: utils
Priority: optional
Suggests: python3-nautilus
Homepage: $PKG_HOMEPAGE
Description: ${DEB_SUMMARY}
${DEB_DESCRIPTION}
EOF

# md5sums over every regular file outside DEBIAN, in a stable order.
( cd "$work" && find . -path ./DEBIAN -prune -o -type f -print0 \
    | LC_ALL=C sort -z \
    | xargs -0 md5sum \
    | sed 's|  \./|  |' > DEBIAN/md5sums )
chmod 0644 "$work/DEBIAN/md5sums"

# Maintainer scripts: keep the desktop and icon caches current.
cat > "$work/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
  fi
fi
exit 0
EOF

cat > "$work/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf /usr/share/icons/hicolor || true
  fi
fi
exit 0
EOF

chmod 0755 "$work/DEBIAN/postinst" "$work/DEBIAN/postrm"

# Directories 0755, regular files 0644, binaries 0755 — dpkg-deb warns otherwise.
find "$work" -type d -exec chmod 0755 {} +
find "$work/usr/share" -type f -exec chmod 0644 {} +
find "$work/usr/bin" -type f -exec chmod 0755 {} +

deb="$OUT_DIR/${DEB_PKG_NAME}_${DEB_VERSION}_${DEB_ARCH}.deb"
fakeroot_cmd=""
command -v fakeroot >/dev/null 2>&1 && fakeroot_cmd="fakeroot"
$fakeroot_cmd dpkg-deb --root-owner-group --build "$work" "$deb"

( cd "$OUT_DIR" && sha256sum "$(basename "$deb")" > "$(basename "$deb").sha256" )

log "wrote $deb"
dpkg-deb --info "$deb" | sed 's/^/    /'

if command -v lintian >/dev/null 2>&1; then
  log "lintian (informational)"
  lintian --no-tag-display-limit "$deb" 2>&1 | sed 's/^/    /' || true
fi
