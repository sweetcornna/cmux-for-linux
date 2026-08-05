# cmux on Linux - port status and plan

This repository is a Linux-only fork of [cmux](https://github.com/manaflow-ai/cmux).
This document records what runs on Linux, what was removed and why, and the
state of the Linux GUI. Historical source-tree numbers here were measured
against the upstream tree at cmux 0.64.21 (preserved at the
`pre-linux-prune` tag). Remaining-work assessments are identified as such.

## Status

| Component | Language | Linux status |
| --- | --- | --- |
| `cmux-tui` multiplexer + public CLI | Rust | **shipping** - packaged as .deb/.rpm/AUR/AppImage/tarball |
| `cmux-relay` transport | Rust | **shipping** - same full `cmux` package |
| `libghostty-vt` terminal emulation | Zig | **shipping** - built from the `ghostty` submodule |
| `cmux-gtk` GTK4 frontend | Rust | **shipping** - included in the same full `cmux` package |

Verified on Ubuntu 26.04 x86_64: the single `.deb` installs `cmux`,
`cmux-relay` and `cmux-gtk`, registers both desktop entries and the man page,
starts a headless session, and drives a real PTY through `libghostty-vt`. The
`.rpm` installs the same full payload in a `fedora:42` container.

## Build dependencies Linux needs and upstream CI does not

- **Zig 0.16.x.** `cmux-tui/crates/ghostty-vt-sys/build.rs` shells out to
  `zig build` to produce `libghostty-vt.a` before any Rust crate compiles.
- **`libclang-dev`.** `ghostty-vt-sys` runs bindgen over `ghostty/vt.h`.
  Ubuntu's `libclang1-*` runtime package alone is not enough: without the
  `libclang-*-dev` package there is no clang builtin header directory, and the
  build dies with

  ```
  /usr/include/limits.h:124:16: fatal error: 'limits.h' file not found
  thread 'main' panicked at crates/ghostty-vt-sys/build.rs:97:10:
  bindgen failed for ghostty/vt.h
  ```

  This is the single most likely first-build failure on a clean Linux host.
- **The `ghostty` submodule.** `git submodule update --init ghostty`. A blobless
  fetch (`--filter=blob:none`) is enough and much faster than a full clone.
- **GTK4 development files.** The full package always includes `cmux-gtk`, so
  Debian and Ubuntu builds require `libgtk-4-dev` (or the distro equivalent).

The TUI and relay link only `libc`, `libm` and `libgcc_s`; the GTK frontend
also links the GTK4, Pango and cairo runtime libraries.

## What was removed, and the measurement behind it

Upstream cmux is a macOS application. The removed trees were the macOS Swift
app, the iOS app, the Xcode project and workspace, the web/webview/Chromium
surfaces, and upstream's macOS and iOS CI - 43 of the 44 upstream workflows.

The prune was not a guess about portability. Measured over the upstream tree:

| Area | Swift files | Total lines | Files importing SwiftUI/AppKit/Cocoa/WebKit/QuartzCore/Carbon/Metal | Lines in those files |
| --- | ---: | ---: | ---: | ---: |
| `Sources/` (the macOS app) | 1,661 | 416,481 | 629 | 276,661 |
| `CLI/` | 119 | 68,727 | 0 | 0 |
| `Packages/Shared/` | 851 | 110,856 | 1 | 53 |
| `Packages/macOS/` | 3,033 | 310,461 | 247 | 33,332 |

Framework imports across `Sources`, `CLI` and `Packages`:

```
3374 Foundation      90 CoreGraphics    11 Carbon
 676 SwiftUI         58 CryptoKit        9 UserNotifications
 645 AppKit          42 GhosttyKit       5 CoreServices
 234 Darwin          36 Combine          4 CoreText
  91 WebKit          35 OSLog            3 LocalAuthentication
```

Additional coupling in `Sources/`: 282 files reference `NSWindow`,
`NSView`, `NSViewController` or `NSApplication`, and there are 145 SwiftUI
`View` type declarations.

The decisive figure is the last column. Around **310,000 lines of UI code**
sit on SwiftUI and AppKit, and neither exists on Linux. `NSWindow`, `NSView`,
view controllers, the responder chain, `@State`/`@Environment` view graphs and
AppKit's IME handling have no Linux counterpart and no shim closes the gap.
Porting that bucket is a rewrite, not a port, and no amount of conditional
compilation avoids it.

Two smaller buckets existed and were removed with the rest:

- **Foundation-level Swift that would have compiled on Linux** - `CLI/`
  (68.7k lines, zero UI imports) and `Packages/Shared/` (110.8k lines, one UI
  file). Reusable in principle, but only useful in service of a Swift GUI.
- **Apple frameworks with Linux equivalents**, had a Swift port gone ahead:
  `CryptoKit` to `swift-crypto`, `Combine` to OpenCombine or `AsyncSequence`,
  `OSLog` to `swift-log`, Keychain to libsecret, `UserNotifications` to
  libnotify or the XDG portal, `Carbon` hotkeys to the XDG GlobalShortcuts
  portal, and Sparkle dropped because distro packages update through the
  package manager.

Recovering any of it is `git checkout pre-linux-prune -- <path>`, or a fetch
from the `upstream` remote.

## A Linux GUI

The GUI is a client of `cmux.protocol/1`, not a Swift port.

`cmux-tui-core` implements the session/workspace/screen/pane/tab/terminal tree
and serves that protocol over a Unix socket. It is the code that ships in this
repository's full package and that the PTY test above exercises. The GUI is a
window that speaks the protocol.

The GUI needs no terminal emulator. `spec/render.md` makes the server the only
VT implementation: clients draw styled runs, place the cursor and send input.
`cmux-gtk` therefore contains no VT parser and links neither libghostty nor
VTE. The `ghostty` submodule's GTK app runtime at `src/apprt/gtk/` remains
available if a future stage needs terminal emulation client-side.

Launching `cmux-gtk` with no arguments targets the default `main` session. If
its socket is missing or refuses the connection, the frontend starts a
headless `cmux` child and retries for up to five seconds; `--probe` remains a
connection-only diagnostic and never starts a session.

Each local cmux session is a separate mux backend and Unix control socket. The
GTK titlebar session selector therefore scans the active socket's runtime
directory for sibling Unix `*.sock` files and probes each socket, rather than
using the connected backend's `Client::sessions()` result as a machine-wide
catalog. The scan runs only when the menu opens. The same window can switch to
a live result or create a named headless session; failed switches rebuild the
previous connection and surface the error through the existing toast path.

| Stage | Deliverable | State |
| --- | --- | --- |
| 0 | Fork, Linux-only tree, toolchain, native packages | **done** |
| 1 | GTK4 window driven over `cmux.protocol/1`: workspace sidebar, terminal rendering, keyboard, resize, scrollback, selection | **done** - see [`../gui/README.md`](../gui/README.md) |
| 2 | Parity pass: pane layouts, tabs, mouse input, themes, macOS visual parity and input behavior | **done** |
| 3 | Package the GUI alongside the TUI | **done** - both frontends ship in the full `cmux` package |
| 4 | Agent and notification awareness in GTK | **in progress** - tab/sidebar markers and static agent rings are implemented |

The stage 2 work verified against live sessions includes pane splits with
per-pane PTY sizing, tab strips, click-to-focus, mouse forwarding to terminal
applications, border theming with xterm-256 indexes, a separate
`cmux-gtk.json` font setting, malformed-configuration safety, selection with a
Shift override, wheel scrollback, dynamic resize, workspace switching with
refreshed server topology, and keyboard input including Ctrl and Alt
sequences. Mouse checks included clicking in `htop` and wheel input in an
alternate-screen application. Additional stage 2 items now implemented and
covered by the GUI tests are `Ctrl+Shift+V` through the server's
bracketed-paste path, cell-throttled no-button mouse movement for mode 1003
applications, protocol-driven cursor and text blink animation with a stable
unfocused cursor, and the pure divider hit-test, ratio and throttle logic. Pane
split/close commands from the default `Ctrl+B` keymap and a right-click menu,
plus drag-to-resize split dividers, are now implemented as well. The final
stage 2 item, inline Kitty graphics, now uses the server's decoded RGB/RGBA
pixels and placement geometry: snapshots replace the scene, deltas merge image
upserts/deletions and replace placements when supplied, and server-projected
viewport coordinates keep images aligned while scrolling. Cairo preserves
source cropping and z-order around text, while the pane-wide inactive scrim
dims images and text together to 70%.

The GTK lifecycle pass now exposes both tab-like protocol levels without
conflating them. Workspace `ScreenId` entries form the workspace-level screen
strip and support focus, create, close and rename; the strip is hidden for one
screen and appears once a second screen exists. Pane `TabId` entries retain
their always-visible strip and support focus, terminal-tab creation, close and
rename. Hover close controls, middle-click close, double-click rename and
action-lane creation all enqueue Rust binding mutations; the frontend waits for
topology changes from the server resource event stream before redrawing. The
default `Ctrl+B` keymap matches the TUI: `c`, `t`, `x`, `Tab`, `Shift+Tab`, `,`,
`$` and `&` retain their distinct NewScreen, NewTab, CloseTab, tab-navigation,
RenameScreen, RenameWorkspace and CloseScreen semantics. In particular,
`Ctrl+B c` keeps screen creation reachable while the screen strip is hidden.

Workspace rows now expose hover close and double-click rename controls. A
20x20 titlebar button creates workspaces, and an adjacent 20x20 button opens the
session selector. Row drag gestures compute a server index for
`Workspace::move_to` and render the existing accent drop indicator. Closing the
last workspace or last tab is never preflighted or blocked by GTK: the request
reaches the server, and any rejection is displayed through the existing toast
path. Pure tests cover screen/pane tab hit geometry, tab wrapping, rename input
validation, workspace drag-index calculation, worker rebuild decisions and
session socket candidate parsing.

Session switching and transport recovery share one protocol-worker supervisor.
It unbinds GTK command routes, stops and joins the control thread, attachment
manager, session-event thread and every attachment thread, then reconnects.
Updates carry a connection generation so late events from the old runtime are
ignored. An attachment disconnect enters the same teardown and rebuild path
after the one-second reconnect interval; it no longer maintains a competing
per-attachment reconnect loop.

The session resource-event stream drives workspace topology. Its initial
snapshot supplies the first tree; deltas are filtered to session, workspace,
screen, pane, tab and terminal changes. Topology publications coalesce at 125ms
with a guaranteed trailing publication. A failed non-atomic tree walk is
re-armed for the next interval instead of tearing down the runtime, with an
error reported after three consecutive failures. Stream end or error uses the
existing supervisor teardown and rebuild path.

Stage 4 derives a terminal-keyed attention model directly from notification
and agent resources on that same session event stream. Unread severity rolls
up from terminal tabs to screen tabs and workspace rows; agent reports drive a
static task-status ring on terminal tabs and a highest-priority status line on
workspace rows. The same event thread maintains terminal working directories
from the resource snapshot and terminal deltas. Rows follow the official
title, status, git branch and path order; missing values omit their respective
lines, and the local home prefix in the path is abbreviated to `~`.

The git branch is a documented frontend exception because the workspace
protocol carries no git state. A dedicated worker runs
`git -C <cwd> status --porcelain=v2 --branch`, formats dirty branches as
`main*`, and uses the short commit for detached HEAD. Successes and failures are
cached per directory for five seconds and returned over the existing UI update
channel. Git is killed and reaped after a one-second timeout, so it never blocks
GTK. All other row details remain protocol-derived; snapshot events replace the
resource maps and deltas update them in place, so they add no protocol
round-trips and never trigger a topology tree walk. The server
clears a viewed surface only after publishing its focus resource change and
does not publish a notification upsert for the new read state, so GTK clears
that terminal's marker locally after a successful focus operation and trusts
the next snapshot as the resync boundary.

The GTK window chrome now follows the extracted upstream design contract in
[`gtk-design-parity.md`](gtk-design-parity.md): a custom 28px titlebar, a
240px resizable workspace sidebar, a screen strip shown only for multiple
screens, an always-visible 28px pane tab strip, derived 1px separators,
inactive-pane dimming, and overlay error toasts in place of a status bar.
Automatic chrome colors are recomputed from the active terminal's resolved
background. Explicit light/dark modes use the terminal background when its
luminance agrees and otherwise rebase chrome surfaces on `#feffff`/`#1e1e1e`;
terminal grid background and foreground colors remain server-resolved. Explicit
`cmux-tui.json` theme values still take precedence. This remains a presentation
change only; render state, input, mouse reporting and PTY sizing still cross
`cmux.protocol/1`, and the server remains the sole VT implementation.
Pane, tab, screen and workspace lifecycle operations plus divider ratios all
run through the Rust resource bindings on the protocol worker thread; the
server owns every mutation and the GTK frontend redraws only after matching
topology events arrive.

The GTK frontend also has per-pane overlay scrollbars, scrollback search and
runtime font zoom. Attachment scroll events provide the absolute viewport
offset used by the 4px rounded thumb; hovering or dragging uses the paired
active chrome semantic, and reaching the bottom hides it without animation.
`Ctrl+Shift+F` searches literal text case-insensitively across retained history
and the current viewport. The protocol worker pages `terminal.history.read`
backward with `before`/`limit`, caches plain rows per terminal, and returns
absolute match positions without blocking GTK; Enter and Shift+Enter navigate
matches and cairo highlights visible hits with accent at 25% opacity. `Ctrl++`,
`Ctrl+=`, `Ctrl+-` and `Ctrl+0` adjust or reset the configured GTK font at
runtime within 6pt-32pt, then reuse the existing attachment resize path for
every visible pane. None of these features adds a client-side VT parser, and
runtime zoom is never written to configuration.

There is no remaining stage 2 tail.

Stage 1 needed two SDK fixes, both carried in [`../patches/`](../patches/) and
both affecting any Rust SDK consumer: the typed render decoder rejected the
fields the server actually sends, and a protocol client had no way to claim
sizing authority for its own viewer lease. Neither was a protocol problem;
both were the public Rust surface lagging its own server. Inline rendering adds
a later SDK patch that exposes the already-generated graphics schema through
the public typed render decoder instead of discarding it. Patch 0005 makes tab
and terminal resource-event values satisfy their existing public snapshot
schemas; it changes no spec field or catalog fingerprint.

## Tracking upstream

Because this fork deleted most of the upstream tree, `git rebase upstream/main`
is **not** mechanical: every upstream commit touching a deleted path produces a
delete/modify conflict. Use the sync script instead, which copies only the
paths this fork builds from:

```bash
packaging/linux/sync-upstream.sh                   # from upstream/main
packaging/linux/sync-upstream.sh cmux-tui-v1.0.0  # any upstream revision
packaging/linux/build-all.sh                       # re-verify before committing
```

It syncs `cmux-tui/`, the `ghostty` submodule pointer, `LICENSE` and
`THIRD_PARTY_LICENSES.md`, and never touches `packaging/`, `docs/` or
`.github/`.

Nothing in this repository is intended as an upstream contribution.

## Licensing

cmux is GPL-3.0-or-later, which is what makes this fork and its package formats
redistributable. The corresponding source stays available at
`https://github.com/sweetcornna/cmux-for-linux`. Every format ships the
licence text at `/usr/share/licenses/cmux/LICENSE` and upstream's third-party
notices at `/usr/share/doc/cmux/THIRD_PARTY_LICENSES.md`.

Copyright in the cmux sources remains with Manaflow, Inc. and the upstream
contributors.
