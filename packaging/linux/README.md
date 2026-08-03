# Native Linux packaging

This directory turns the Rust `cmux-tui` workspace into native Linux packages.
Upstream cmux ships the same binaries only through npm and PyPI; nothing here
modifies upstream code.

## What gets packaged

| Path | Contents |
| --- | --- |
| `/usr/bin/cmux-tui` | the TUI multiplexer and public CLI |
| `/usr/bin/cmux-relay` | stdio↔socket transport primitive |
| `/usr/bin/cmux` | symlink to `cmux-tui`, matching the upstream npm command name |
| `/usr/share/applications/cmux.desktop` | desktop entry (`Terminal=true`) |
| `/usr/share/icons/hicolor/*/apps/cmux.png` | icons from `common/icons/` |
| `/usr/share/man/man1/cmux.1.gz` | man page generated from `common/cmux.1.in` |
| `/usr/share/{bash-completion,zsh,fish}/…` | shell completions |
| `/usr/share/doc/cmux/`, `/usr/share/licenses/cmux/` | README, third-party notices, GPL-3.0 text |

There is no GUI package yet; see [`../../docs/linux-port.md`](../../docs/linux-port.md).

## Build requirements

- Rust stable (1.91+ per the workspace `rust-version`)
- Zig **0.16.x** — `ghostty-vt-sys` compiles `libghostty-vt.a` from the
  `ghostty` submodule before any Rust crate builds
- the `ghostty` submodule checked out: `git submodule update --init ghostty`
- `clang` and `libclang-dev` — bindgen needs clang's builtin header directory,
  or the build fails with `'limits.h' file not found`
- `dpkg-deb` for `.deb`; `dpkg-dev` additionally enables real `dpkg-shlibdeps`
  dependency resolution
- `docker` for `.rpm` (a Fedora container supplies `rpmbuild`), or a local
  `rpmbuild` with `CMUX_RPM_NATIVE=1`
- network access on the first AppImage build, to fetch `appimagetool`

## Usage

```bash
# everything, for the host architecture
packaging/linux/build-all.sh

# one format, reusing binaries already built
CMUX_SKIP_BUILD=1 packaging/linux/build-all.sh deb

# pin the version instead of deriving it from the nearest linux-v* tag
CMUX_VERSION=0.64.21 packaging/linux/build-all.sh
```

Artifacts land in `build/linux/dist/`, each with a `.sha256` next to it.

## Layout

```
packaging/linux/
├── build-all.sh          orchestrator
├── build-binaries.sh     cargo build -p cmux-tui -p cmux-relay, then strip
├── stage-tree.sh         the one FHS tree every format installs
├── build-tarball.sh      portable .tar.gz + install.sh
├── build-deb.sh          dpkg-deb
├── build-rpm.sh          rpmbuild, containerised by default
├── build-aur.sh          renders aur/PKGBUILD for the current version
├── build-appimage.sh     appimagetool
├── sync-upstream.sh      pull cmux-tui/ and ghostty from upstream
├── common/               desktop entry, man page source, icons, completions, install.sh
├── rpm/cmux.spec         binary-repack spec
└── aur/PKGBUILD          cmux-bin
```

Every format consumes the same staged tree, so the installed layout is
identical across them and only packaging metadata differs.

## Versioning

This fork versions its packaging independently of upstream, in its own
`linux-v*` tag namespace. Upstream's `cmux-tui-v*` and `v*` tags are left
alone: `sync-upstream.sh` fetches upstream tags, so a shared namespace would
eventually collide.

`resolve_version` in `lib/common.sh` derives the version from the nearest
`linux-v*` tag:

- exactly on `linux-v1.2.3` → `1.2.3`
- three commits past it → `1.2.3+3.gabc1234`
- no tag reachable → `0.0.0+g<sha>`

`CMUX_VERSION` overrides it. The `.deb` appends `-1` as the Debian revision;
the `.rpm` replaces any `-` with `.` because RPM forbids it in `Version`.

Pushing a `linux-v*` tag is what publishes a release: CI builds both
architectures, renders the AUR `PKGBUILD` with the real digests for each, and
creates the GitHub Release. Keep tag versions free of `-` and `+` — Arch
rejects `-` in `pkgver`, and `+` becomes `%2B` in download URLs.

## Known gaps

- The `.deb` and `.rpm` are built for the host architecture only. Cross builds
  need `CMUX_RUST_TARGET` plus a matching Zig target, which
  `build-binaries.sh` passes through but which is not exercised here.
- The AUR `PKGBUILD` is a `-bin` package pointing at GitHub release tarballs.
  A from-source PKGBUILD would need Zig 0.16 and a vendored Cargo registry
  inside the makepkg sandbox.
- `lintian` and `rpmlint` are advisory here; neither package has been through a
  distro archive review.
