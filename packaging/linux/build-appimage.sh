#!/usr/bin/env bash
# Build a self-contained AppImage from the staged FHS tree.
#
# The AppImage carries the TUI, relay and GTK4 frontend together. Running it
# from a shell opens the TUI; its desktop entry and a cmux-gtk symlink dispatch
# to the GUI, while a cmux-relay symlink dispatches to the relay.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch
STAGE="${CMUX_STAGE:-$BUILD_DIR/stage}"
VERSION="$(resolve_version)"
OUT_DIR="$BUILD_DIR/dist"
TOOL_DIR="$BUILD_DIR/tools"
APPIMAGETOOL_VERSION="${CMUX_APPIMAGETOOL_VERSION:-continuous}"

[ -d "$STAGE/usr/bin" ] || die "no staged tree at $STAGE — run packaging/linux/stage-tree.sh first"

# appimagetool needs FUSE to self-extract; --appimage-extract-and-run avoids
# that, which matters inside containers and CI runners.
export APPIMAGE_EXTRACT_AND_RUN=1

appimagetool="${CMUX_APPIMAGETOOL:-}"
if [ -z "$appimagetool" ]; then
  appimagetool="$TOOL_DIR/appimagetool-$APPIMAGE_ARCH.AppImage"
  if [ ! -x "$appimagetool" ]; then
    need curl
    mkdir -p "$TOOL_DIR"
    url="https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$APPIMAGE_ARCH.AppImage"
    log "downloading appimagetool from $url"
    curl -fSL --retry 3 -o "$appimagetool" "$url" \
      || die "could not download appimagetool; set CMUX_APPIMAGETOOL to a local copy"
    chmod +x "$appimagetool"
  fi
fi

appdir="$BUILD_DIR/appimage/cmux.AppDir"
log "building cmux $VERSION ($APPIMAGE_ARCH) AppImage"
rm -rf "$BUILD_DIR/appimage"
mkdir -p "$appdir" "$OUT_DIR"

cp -a "$STAGE/usr" "$appdir/usr"

# AppImage requires one desktop file, its icon and .DirIcon at the AppDir root.
# Its launcher selects the GUI while the AppImage's CLI default remains the TUI.
sed -e 's/^Exec=.*/Exec=cmux --gtk/' \
    -e 's/^TryExec=.*/TryExec=cmux/' \
    "$STAGE/usr/share/applications/cmux.desktop" > "$appdir/cmux.desktop"
if [ -f "$STAGE/usr/share/icons/hicolor/256x256/apps/cmux.png" ]; then
  install -m 0644 "$STAGE/usr/share/icons/hicolor/256x256/apps/cmux.png" "$appdir/cmux.png"
  cp "$appdir/cmux.png" "$appdir/.DirIcon"
fi

cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
# AppRun for the cmux AppImage.
#
# Dispatch on the name the AppImage was invoked as, so one file can provide
# all three commands. The root desktop entry uses --gtk because desktop
# integration invokes the AppImage by its artifact name.
set -eu

HERE="$(dirname "$(readlink -f "$0")")"

export PATH="$HERE/usr/bin:${PATH:-}"
export MANPATH="$HERE/usr/share/man:${MANPATH:-}"
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"

invoked="$(basename "${ARGV0:-$0}")"
case "$invoked" in
  cmux-relay*) exec "$HERE/usr/bin/cmux-relay" "$@" ;;
  cmux-gtk*)   exec "$HERE/usr/bin/cmux-gtk" "$@" ;;
esac

if [ "${1:-}" = "--gtk" ]; then
  shift
  exec "$HERE/usr/bin/cmux-gtk" "$@"
fi
exec "$HERE/usr/bin/cmux-tui" "$@"
EOF
chmod 0755 "$appdir/AppRun"

output="$OUT_DIR/$PKG_NAME-$VERSION-$APPIMAGE_ARCH.AppImage"
ARCH="$APPIMAGE_ARCH" "$appimagetool" --no-appstream "$appdir" "$output" 2>&1 | sed 's/^/    /'

[ -f "$output" ] || die "appimagetool did not produce $output"
chmod 0755 "$output"
( cd "$OUT_DIR" && sha256sum "$(basename "$output")" > "$(basename "$output").sha256" )

log "wrote $output"
