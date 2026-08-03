# Native Linux packaging

This directory belongs to the `linux` maintenance branch of cmux. It turns the
Rust `cmux-tui` workspace into native Linux packages. Upstream `main` ships the
same binaries only through npm and PyPI; nothing here changes upstream code.

## What gets packaged

| Path | Contents |
| --- | --- |
| `/usr/bin/cmux-tui` | the TUI multiplexer and public CLI |
| `/usr/bin/cmux-relay` | stdio↔socket transport primitive |
| `/usr/bin/cmux` | symlink to `cmux-tui`, matching the upstream npm command name |
| `/usr/share/applications/cmux.desktop` | desktop entry (`Terminal=true`) |
| `/usr/share/icons/hicolor/*/apps/cmux.png` | icons lifted from `Assets.xcassets` |
| `/usr/share/man/man1/cmux.1.gz` | man page generated from `common/cmux.1.in` |
| `/usr/share/{bash-completion,zsh,fish}/…` | shell completions |
| `/usr/share/doc/cmux/`, `/usr/share/licenses/cmux/` | README, third-party notices, GPL-3.0 text |

The `cmux-browser`, `webviews`, `agent-chat` and Swift components are **not**
in these packages. They are macOS-app surfaces; see `docs/linux-port.md` for
the GUI port status.

## Build requirements

- Rust stable (1.91+ per the workspace `rust-version`)
- Zig **0.16.x** — `ghostty-vt-sys` compiles `libghostty-vt.a` from the
  `ghostty` submodule before any Rust crate builds
- the `ghostty` submodule checked out: `git submodule update --init ghostty`
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

# pin the version instead of deriving it from the nearest cmux-tui-v* tag
CMUX_VERSION=0.64.21 packaging/linux/build-all.sh
```

Artifacts land in `build/linux/dist/`, each with a `.sha256` next to it.

## Layout

```
packaging/linux/
├── build-all.sh          orchestrator
├── build-binaries.sh     cargo build -p cmux-tui -p cmux-relay
├── stage-tree.sh         the one FHS tree every format installs
├── build-tarball.sh      portable .tar.gz + install.sh
├── build-deb.sh          dpkg-deb
├── build-rpm.sh          rpmbuild, containerised by default
├── build-aur.sh          renders aur/PKGBUILD for the current version
├── build-appimage.sh     appimagetool
├── common/               desktop entry, man page source, completions, install.sh
├── rpm/cmux.spec         binary-repack spec
└── aur/PKGBUILD          cmux-bin
```

Every format consumes the same staged tree, so the installed layout is
identical across them and only packaging metadata differs.

## Versioning

`resolve_version` in `lib/common.sh` derives the version from the nearest
`cmux-tui-v*` tag:

- exactly on `cmux-tui-v1.2.3` → `1.2.3`
- three commits past it → `1.2.3+3.gabc1234`
- no tag reachable → `0.0.0+g<sha>`

`CMUX_VERSION` overrides it. The `.deb` appends `-1` as the Debian revision;
the `.rpm` replaces any `-` with `.` because RPM forbids it in `Version`.

## Known gaps

- The `.deb` and `.rpm` are built for the host architecture only. Cross builds
  need `CMUX_RUST_TARGET` plus a matching Zig target, which
  `build-binaries.sh` passes through but which is not exercised here.
- The AUR `PKGBUILD` is a `-bin` package pointing at GitHub release tarballs.
  A from-source PKGBUILD would need Zig 0.16 and a vendored Cargo registry
  inside the makepkg sandbox.
- `lintian` and `rpmlint` are advisory here; neither package has been through a
  distro archive review.
