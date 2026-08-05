# Shared helpers for the Linux native packaging scripts.
# Sourced, never executed directly.

set -euo pipefail

PKG_NAME="cmux"
PKG_MAINTAINER="cmux for linux <travon_evenietyku@sanfranmail.com>"
PKG_HOMEPAGE="https://github.com/sweetcornna/cmux-for-linux"
PKG_LICENSE="GPL-3.0-or-later"
PKG_SUMMARY="Terminal multiplexer with TUI and GTK4 frontends for AI coding agents"

# Repo root: packaging/linux/lib/common.sh -> ../../..
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TUI_DIR="$REPO_ROOT/cmux-tui"
PKG_DIR="$REPO_ROOT/packaging/linux"
BUILD_DIR="${CMUX_LINUX_BUILD_DIR:-$REPO_ROOT/build/linux}"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"
}

# Resolve the version for this build.
#   CMUX_VERSION=... overrides everything.
#   Otherwise derive from the most recent linux-vX.Y.Z tag, plus a commit
#   suffix when HEAD is not exactly on that tag.
#
# The fork versions its packaging independently and owns the linux-v* tag
# namespace. Upstream's cmux-tui-v* and v* tags are deliberately not matched:
# sync-upstream.sh fetches upstream tags, so a shared namespace would collide.
resolve_version() {
  if [ -n "${CMUX_VERSION:-}" ]; then
    printf '%s' "$CMUX_VERSION"
    return
  fi
  local desc
  if desc="$(git -C "$REPO_ROOT" describe --tags --match 'linux-v*' --always 2>/dev/null)"; then
    case "$desc" in
      linux-v*)
        # linux-v1.2.3           -> 1.2.3
        # linux-v1.2.3-4-gabcdef -> 1.2.3+4.gabcdef
        printf '%s' "${desc#linux-v}" | sed -E 's/-([0-9]+)-g([0-9a-f]+)$/+\1.g\2/'
        return
        ;;
    esac
  fi
  # No release tag reachable: fall back to a date-less 0.0.0+<sha> so package
  # managers still get a monotonic-ish, valid version string.
  printf '0.0.0+g%s' "$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
}

# What `cmux --version` shows in parentheses after the crate version. The
# package version is what a user can act on - it matches what their package
# manager reports - and the commit disambiguates rebuilds of the same release.
build_commit_stamp() {
  local version commit
  version="$(resolve_version)"
  commit="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || true)"
  if [ -n "$commit" ]; then
    printf 'cmux-for-linux %s; %s' "$version" "$commit"
  else
    printf 'cmux-for-linux %s' "$version"
  fi
}

# The ghostty submodule commit, so a binary can be tied to the VT it was built
# against. Empty when the submodule is absent; the upstream binary filters that.
ghostty_commit_stamp() {
  git -C "$REPO_ROOT/ghostty" rev-parse --short HEAD 2>/dev/null || true
}

# Map `uname -m` to the Debian, RPM and Rust names for the same arch.
detect_arch() {
  case "$(uname -m)" in
    x86_64)  DEB_ARCH=amd64 RPM_ARCH=x86_64  RUST_ARCH=x86_64  APPIMAGE_ARCH=x86_64 ;;
    aarch64) DEB_ARCH=arm64 RPM_ARCH=aarch64 RUST_ARCH=aarch64 APPIMAGE_ARCH=aarch64 ;;
    *) die "unsupported architecture: $(uname -m)" ;;
  esac
  export DEB_ARCH RPM_ARCH RUST_ARCH APPIMAGE_ARCH
}
