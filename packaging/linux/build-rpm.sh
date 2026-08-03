#!/usr/bin/env bash
# Build the .rpm from the staged FHS tree.
#
# rpmbuild is not packaged on every developer machine, so by default this runs
# inside a Fedora container. Set CMUX_RPM_NATIVE=1 to use a local rpmbuild.

source "$(dirname "$(readlink -f "$0")")/lib/common.sh"

detect_arch
# Each package has its own spec: the file lists, dependencies and %check
# assertions differ, so one spec with conditionals would be worse.
RPM_PKG_NAME="${CMUX_PKG_NAME:-$PKG_NAME}"
SPEC="$PKG_DIR/rpm/$RPM_PKG_NAME.spec"
[ -f "$SPEC" ] || die "no spec for package '$RPM_PKG_NAME' at $SPEC"
STAGE="${CMUX_STAGE:-$BUILD_DIR/stage}"
VERSION="$(resolve_version)"
OUT_DIR="$BUILD_DIR/dist"
CONTAINER_IMAGE="${CMUX_RPM_IMAGE:-fedora:42}"

[ -d "$STAGE/usr/bin" ] || die "no staged tree at $STAGE — run packaging/linux/stage-tree.sh first"

# RPM versions must not contain '-'. resolve_version only emits '+' and '.'
# separators for untagged builds, but guard anyway.
RPM_VERSION="${VERSION//-/.}"

work="$BUILD_DIR/rpm/$RPM_PKG_NAME"
log "building $RPM_PKG_NAME $RPM_VERSION ($RPM_ARCH) .rpm"
rm -rf "$work"
mkdir -p "$work/SOURCES" "$work/SPECS" "$OUT_DIR"

tar --numeric-owner --owner=0 --group=0 --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH:-0}" \
    -czf "$work/SOURCES/cmux-stage.tar.gz" -C "$STAGE" usr
cp "$SPEC" "$work/SPECS/package.spec"

if [ "${CMUX_RPM_NATIVE:-0}" = 1 ]; then
  need rpmbuild
  rpmbuild -bb \
    --define "_topdir $work" \
    --define "cmux_version $RPM_VERSION" \
    --define "cmux_stage_tar cmux-stage.tar.gz" \
    --target "$RPM_ARCH" \
    "$work/SPECS/package.spec"
else
  need docker
  log "using container image $CONTAINER_IMAGE (set CMUX_RPM_NATIVE=1 for a local rpmbuild)"
  # rpmbuild runs as root inside the container so rpm-build can be installed;
  # the produced tree is chowned back to the invoking user before exit.
  # -i is required: the build script is fed to `sh -s` on stdin.
  docker run --rm -i \
    -v "$work:/work" \
    -e "CMUX_RPM_VERSION=$RPM_VERSION" \
    -e "CMUX_RPM_ARCH=$RPM_ARCH" \
    -e "CMUX_HOST_UID=$(id -u)" \
    -e "CMUX_HOST_GID=$(id -g)" \
    "$CONTAINER_IMAGE" \
    /bin/sh -eus <<'CONTAINER'
command -v rpmbuild >/dev/null 2>&1 || dnf -y -q install rpm-build tar >/dev/null
rpmbuild -bb \
  --define "_topdir /work" \
  --define "cmux_version $CMUX_RPM_VERSION" \
  --define "cmux_stage_tar cmux-stage.tar.gz" \
  --target "$CMUX_RPM_ARCH" \
  /work/SPECS/package.spec
chown -R "$CMUX_HOST_UID:$CMUX_HOST_GID" /work
CONTAINER
fi

found=0
while IFS= read -r rpm; do
  cp "$rpm" "$OUT_DIR/"
  base="$(basename "$rpm")"
  ( cd "$OUT_DIR" && sha256sum "$base" > "$base.sha256" )
  log "wrote $OUT_DIR/$base"
  found=1
done < <(find "$work/RPMS" -name '*.rpm' -type f 2>/dev/null)

[ "$found" = 1 ] || die "rpmbuild produced no .rpm under $work/RPMS"
