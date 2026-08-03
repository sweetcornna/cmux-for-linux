#!/usr/bin/env bash
# Build the release binaries that every Linux package format ships.
#
# Produces, under build/linux/bin/:
#   cmux-tui    the TUI multiplexer and public CLI
#   cmux-relay  the stdio<->socket transport primitive
#   cmux-gtk    the GTK4 frontend
#
# Requires a Rust toolchain and Zig 0.16.0 (ghostty-vt-sys compiles
# libghostty-vt.a from the ghostty submodule before the Rust crates build).

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch

TARGET="${CMUX_RUST_TARGET:-}"
PROFILE="${CMUX_CARGO_PROFILE:-release}"

need cargo
need "${ZIG:-zig}"

[ -f "$REPO_ROOT/ghostty/build.zig" ] || \
  die "ghostty submodule is not checked out. Run: git submodule update --init ghostty"

zig_version="$("${ZIG:-zig}" version)"
case "$zig_version" in
  0.16.*) ;;
  *) log "warning: ghostty expects Zig 0.16.x, found $zig_version" ;;
esac

log "building cmux-tui and cmux-relay (profile=$PROFILE${TARGET:+, target=$TARGET})"

cargo_args=(build --profile "$PROFILE" -p cmux-tui -p cmux-relay)
[ -n "$TARGET" ] && cargo_args+=(--target "$TARGET")

( cd "$TUI_DIR" && cargo "${cargo_args[@]}" )

# `--profile release` lands in target/release, not target/profile-name.
profile_dir="$PROFILE"
[ "$PROFILE" = "dev" ] && profile_dir="debug"

src_dir="$TUI_DIR/target"
[ -n "$TARGET" ] && src_dir="$src_dir/$TARGET"
src_dir="$src_dir/$profile_dir"

mkdir -p "$BUILD_DIR/bin"
for bin in cmux-tui cmux-relay; do
  [ -x "$src_dir/$bin" ] || die "expected binary not found: $src_dir/$bin"
  install -m 0755 "$src_dir/$bin" "$BUILD_DIR/bin/$bin"
done

# A cargo release build keeps debug info, which puts cmux-tui above 60 MB.
# Distro packages are expected to ship stripped binaries; set CMUX_NO_STRIP=1
# to keep the symbols for profiling or crash triage.
if [ "${CMUX_NO_STRIP:-0}" != 1 ] && command -v strip >/dev/null 2>&1; then
  for bin in cmux-tui cmux-relay; do
    before="$(stat -c %s "$BUILD_DIR/bin/$bin")"
    strip --strip-unneeded "$BUILD_DIR/bin/$bin"
    after="$(stat -c %s "$BUILD_DIR/bin/$bin")"
    log "stripped $bin: $((before / 1024)) KiB -> $((after / 1024)) KiB"
  done
fi

# The GTK frontend is part of the single full package, so its development
# files are a hard build requirement rather than an optional feature probe.
need pkg-config
pkg-config --exists gtk4 2>/dev/null || \
  die "gtk4 development files are missing (install libgtk-4-dev)"
[ -f "$REPO_ROOT/gui/Cargo.toml" ] || die "missing gui/Cargo.toml"

log "building cmux-gtk"
( cd "$REPO_ROOT/gui" && cargo build --profile "$PROFILE" )
gui_src="$REPO_ROOT/gui/target/$profile_dir/cmux-gtk"
[ -x "$gui_src" ] || die "expected binary not found: $gui_src"
install -m 0755 "$gui_src" "$BUILD_DIR/bin/cmux-gtk"
if [ "${CMUX_NO_STRIP:-0}" != 1 ] && command -v strip >/dev/null 2>&1; then
  strip --strip-unneeded "$BUILD_DIR/bin/cmux-gtk"
fi

log "binaries staged in $BUILD_DIR/bin"
ls -la "$BUILD_DIR/bin"
