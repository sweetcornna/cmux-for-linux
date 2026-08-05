# cmux for linux

Native Linux packages for [cmux](https://github.com/manaflow-ai/cmux), the
terminal multiplexer for AI coding agents, backed by `libghostty-vt`.

Upstream cmux is a macOS application that ships its Rust multiplexer only
through npm and PyPI. This repository is a Linux maintenance fork: it keeps
the parts that run natively on Linux, packages them the way Linux
distributions expect, and drops everything else.

```bash
sudo apt install ./cmux_<version>_amd64.deb      # Debian / Ubuntu
sudo dnf install ./cmux-<version>.x86_64.rpm     # Fedora / RHEL
yay -S cmux-bin                                  # Arch (AUR)
./cmux-<version>-x86_64.AppImage                 # anywhere
```

Then:

```bash
cmux                              # start or attach to the default session
cmux --headless --session agents  # run a session without a TUI
cmux workspace create --name api
cmux workspace current run -- cargo test
```

`man cmux` documents the noun-first CLI.

For a window instead of a TUI:

```bash
cmux-gtk                         # connect to "main", starting it if needed
cmux-gtk --session agents        # use another session
```

`cmux-gtk` renders pane splits and tabs, switches workspaces from a sidebar,
resizes each pane's PTY with the window, scrolls back with the wheel and copies
a drag-selection with `Ctrl+Shift+C`. It is installed by the same `cmux`
package as the TUI and relay. Clicking the cmux GTK desktop icon starts the GUI
and automatically starts the default session when none is running. See
[`gui/README.md`](gui/README.md).

## GTK frontend vs the TUI

Normal cmux is the TUI frontend installed by the `cmux` package. It is the
terminal-native multiplexer, runs anywhere cmux can run, including over SSH,
and is also the server that every frontend attaches to when run with
`--headless`. The same package installs both frontends.

`cmux-gtk` is the native GTK4 window onto a cmux session. It speaks
the same protocol, uses the same server and displays the same session. It is a
different face for the session, not a fork of multiplexer behavior.

Choose `cmux-gtk` when you want a desktop window with its own icon, launcher
and Alt-Tab entry; fonts and rendering through Pango rather than the limits of
the terminal emulator hosting the TUI; or a per-window font and theme without
changing terminal-emulator settings. Choose the TUI for SSH and remote use,
terminal-only environments, or features that the GTK frontend does not have
yet.

| Capability | TUI (`cmux`) | GTK (`cmux-gtk`) |
| --- | --- | --- |
| Panes and splits | Yes | Yes |
| Tabs | Yes | Yes |
| Mouse input to terminal applications | Yes | Yes |
| Scrollback | Yes | Yes |
| Selection and clipboard | Yes | Yes; copy with `Ctrl+Shift+C` |
| Themes | Reads `cmux-tui.json` | Reads `cmux-tui.json`; the font is set separately in `cmux-gtk.json` |
| Inline images | Depends on the terminal emulator's own support | Not yet |
| Create panes and resize them by dragging | Yes | Not yet; create and resize them through the CLI or TUI, and the GTK window follows the layout |
| Sidebar file browser | Yes | No |
| Machine rail and SSH targets | Yes | No |
| Works over SSH without X or Wayland | Yes | No |

The two frontends coexist. A GTK window and any number of TUI attachments can view
the same session at the same time.

## What is in this repository

| Path | What it is |
| --- | --- |
| `cmux-tui/` | the Rust workspace: the TUI multiplexer, public CLI, relay, and PTY layer |
| `gui/` | `cmux-gtk`, a GTK4 frontend that speaks `cmux.protocol/1` |
| `patches/` | fixes this fork carries against upstream files |
| `ghostty/` | submodule supplying `libghostty-vt`, the terminal emulator |
| `packaging/linux/` | everything that turns the above into `.deb`, `.rpm`, AUR, AppImage and tarball |
| `docs/linux-port.md` | port status, measurements, and the plan for a Linux GUI |

Everything unrelated to Linux was removed: the macOS Swift application, the
iOS app, the Xcode project, the web and webview surfaces, and upstream's
macOS/iOS CI. The full pre-prune tree is kept at the `pre-linux-prune` tag and
in the `upstream` remote.

## What the package installs

| Path | Contents |
| --- | --- |
| `/usr/bin/cmux` | symlink to `cmux-tui`, matching upstream's npm command name |
| `/usr/bin/cmux-tui` | the multiplexer and public CLI |
| `/usr/bin/cmux-relay` | stdio-to-socket transport primitive |
| `/usr/bin/cmux-gtk` | GTK4 frontend; starts its target session automatically when needed |
| `/usr/bin/cmux-open-here` | helper for file-manager context-menu actions |
| `/usr/share/applications/cmux.desktop` | GUI desktop entry (`Terminal=false`) |
| `/usr/share/icons/hicolor/*/apps/cmux.png` | icons |
| `/usr/share/{kio/servicemenus,file-manager/actions,nemo/actions,nautilus-python/extensions}/...` | file-manager context-menu integrations |
| `/usr/share/man/man1/cmux.1.gz` | man page |
| `/usr/share/{bash-completion,zsh,fish}/...` | shell completions |
| `/usr/share/doc/cmux/`, `/usr/share/licenses/cmux/` | docs, third-party notices, GPL-3.0 text |

The TUI and relay link only `libc`, `libm` and `libgcc_s`; `cmux-gtk` also uses
the packaged GTK4, Pango and cairo runtime libraries.

The package adds **New cmux window here** and **New cmux workspace here** to
supported file managers. The window action opens the GTK frontend with a new
workspace rooted at the selected directory. The workspace action adds that
workspace to the running session and reuses an attached window; if no viewer
is attached, it opens a GTK window so the new workspace is visible.

## Building from source

```bash
git clone https://github.com/sweetcornna/cmux-for-linux.git
cd cmux-for-linux
git submodule update --init --filter=blob:none ghostty

sudo apt install clang libclang-dev libgtk-4-dev dpkg-dev fakeroot
# plus a Rust toolchain and Zig 0.16.x on PATH

packaging/linux/build-all.sh
```

Artifacts land in `build/linux/dist/`, each with a matching `.sha256`.

**`libclang-dev` is not optional.** `ghostty-vt-sys` runs bindgen over
`ghostty/vt.h`; without clang's builtin header directory the build fails with
`'limits.h' file not found`. This is the most common first-build failure on a
clean Linux host.

See [`packaging/linux/README.md`](packaging/linux/README.md) for the packaging
reference and [`docs/linux-port.md`](docs/linux-port.md) for what is and is not
ported.

## Relationship to upstream

This fork tracks `manaflow-ai/cmux`. It adds `packaging/`, one CI workflow and
one document, and does not modify the upstream files it keeps, so rebases stay
mechanical:

```bash
git fetch upstream
git rebase upstream/main
packaging/linux/build-all.sh   # re-verify before pushing
```

Nothing here is intended as an upstream contribution. Bugs in the multiplexer
itself belong at [manaflow-ai/cmux](https://github.com/manaflow-ai/cmux);
packaging bugs belong here.

## Licence

cmux is **GPL-3.0-or-later**, which is what makes this fork and its package
formats redistributable. Every format ships the licence text at
`/usr/share/licenses/cmux/LICENSE` and upstream's third-party notices at
`/usr/share/doc/cmux/THIRD_PARTY_LICENSES.md`.

Copyright for the cmux sources remains with Manaflow, Inc. and the upstream
contributors; see [`LICENSE`](LICENSE) and
[`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).
