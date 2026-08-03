#!/usr/bin/env sh
# Install cmux from the portable tarball.
#
# Usage:
#   ./install.sh [--prefix DIR] [--uninstall]
#
# Defaults to /usr/local when run as root, and to ~/.local otherwise. The
# tarball layout is prefix-relative, so installing is a copy plus an icon and
# desktop-database refresh.

set -eu

PREFIX=""
UNINSTALL=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --prefix=*) PREFIX="${1#--prefix=}"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$PREFIX" ]; then
  if [ "$(id -u)" = 0 ]; then PREFIX=/usr/local; else PREFIX="$HOME/.local"; fi
fi

SRC="$(cd "$(dirname "$0")" && pwd)"

FILES="bin/cmux bin/cmux-tui bin/cmux-relay
share/applications/cmux.desktop
share/man/man1/cmux.1.gz
share/bash-completion/completions/cmux
share/zsh/site-functions/_cmux
share/fish/vendor_completions.d/cmux.fish"

if [ "$UNINSTALL" = 1 ]; then
  echo "removing cmux from $PREFIX"
  for f in $FILES; do rm -f "$PREFIX/$f"; done
  rm -rf "$PREFIX/share/doc/cmux" "$PREFIX/share/licenses/cmux"
  for size in 16 32 128 256 512; do
    rm -f "$PREFIX/share/icons/hicolor/${size}x${size}/apps/cmux.png"
  done
else
  echo "installing cmux into $PREFIX"
  # Copy the payload directories the archive carries. Kept to an explicit list
  # rather than a find/read loop: this script runs under dash on Debian and
  # Ubuntu, where `read -d` does not exist.
  for dir in bin share; do
    [ -d "$SRC/$dir" ] || continue
    mkdir -p "$PREFIX/$dir"
    cp -a "$SRC/$dir/." "$PREFIX/$dir/"
  done
  [ -x "$PREFIX/bin/cmux-tui" ] || { echo "install failed: $PREFIX/bin/cmux-tui missing" >&2; exit 1; }
fi

# Refresh the caches that own the desktop entry and icons, when present.
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
fi

[ "$UNINSTALL" = 1 ] && { echo "done"; exit 0; }

case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "note: $PREFIX/bin is not in PATH; add it to your shell profile" >&2 ;;
esac

echo "done — run 'cmux' to start the default session"
