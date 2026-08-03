# cmux on Linux — port status and plan

This repository is a Linux-only fork of [cmux](https://github.com/manaflow-ai/cmux).
This document records what runs on Linux, what was removed and why, and what a
Linux GUI would take. Every number here was measured against the upstream tree
at cmux 0.64.21 (preserved at the `pre-linux-prune` tag), not estimated.

## Status

| Component | Language | Linux status |
| --- | --- | --- |
| `cmux-tui` multiplexer + public CLI | Rust | **shipping** — packaged as .deb/.rpm/AUR/AppImage/tarball |
| `cmux-relay` transport | Rust | **shipping** — same packages |
| `libghostty-vt` terminal emulation | Zig | **shipping** — built from the `ghostty` submodule |
| Linux GUI | — | **not started** — see [A Linux GUI](#a-linux-gui) |

Verified on Ubuntu 26.04 x86_64: the `.deb` installs, puts `cmux` on `PATH`,
registers the man page, starts a headless session, and drives a real PTY
through `libghostty-vt`. The `.rpm` installs in a `fedora:42` container.

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

Nothing else is required. The resulting binaries link only `libc`, `libm` and
`libgcc_s`.

## What was removed, and the measurement behind it

Upstream cmux is a macOS application. The removed trees were the macOS Swift
app, the iOS app, the Xcode project and workspace, the web/webview/Chromium
surfaces, and upstream's macOS and iOS CI — 43 of the 44 upstream workflows.

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

- **Foundation-level Swift that would have compiled on Linux** — `CLI/`
  (68.7k lines, zero UI imports) and `Packages/Shared/` (110.8k lines, one UI
  file). Reusable in principle, but only useful in service of a Swift GUI.
- **Apple frameworks with Linux equivalents**, had a Swift port gone ahead:
  `CryptoKit`→`swift-crypto`, `Combine`→OpenCombine or `AsyncSequence`,
  `OSLog`→`swift-log`, Keychain→libsecret, `UserNotifications`→libnotify or
  the XDG portal, `Carbon` hotkeys→the XDG GlobalShortcuts portal, Sparkle→
  dropped, since distro packages update through the package manager.

Recovering any of it is `git checkout pre-linux-prune -- <path>`, or a fetch
from the `upstream` remote.

## A Linux GUI

The GUI is not started. When it is, it should be a client of
`cmux.protocol/1`, not a Swift port.

`cmux-tui-core` already implements the session/workspace/screen/pane/tab/
terminal tree and serves that protocol over a Unix socket — it is the code
that ships in this repository's packages and that the PTY test above
exercises. A GUI becomes a window that speaks the protocol, and the protocol
is already specified (`cmux-tui/spec/`), already tested, and already running
on Linux.

Terminal rendering has a supported path that does not involve Apple
frameworks: the `ghostty` submodule carries a GTK app runtime at
`src/apprt/gtk/` (`App.zig`, `Surface.zig`). The 42 `GhosttyKit` imports in
the removed macOS app were the Apple XCFramework wrapper around the same
library.

| Stage | Deliverable | State |
| --- | --- | --- |
| 0 | Fork, Linux-only tree, toolchain, native packages | **done** |
| 1 | GTK4 shell: one window, session/workspace sidebar, one terminal surface via ghostty's GTK apprt, driven over `cmux.protocol/1` | not started |
| 2 | Parity pass: splits, tabs, layouts, the agent notification surface | not started |
| 3 | Package the GUI alongside the TUI in the same five formats | not started |

## Tracking upstream

Because this fork deleted most of the upstream tree, `git rebase upstream/main`
is **not** mechanical: every upstream commit touching a deleted path produces a
delete/modify conflict. Use the sync script instead, which copies only the
paths this fork builds from:

```bash
packaging/linux/sync-upstream.sh              # from upstream/main
packaging/linux/sync-upstream.sh cmux-tui-v1.0.0
packaging/linux/build-all.sh                  # re-verify before committing
```

It syncs `cmux-tui/`, the `ghostty` submodule pointer, `LICENSE` and
`THIRD_PARTY_LICENSES.md`, and never touches `packaging/`, `docs/` or
`.github/`.

Nothing in this repository is intended as an upstream contribution.

## Licensing

cmux is GPL-3.0-or-later, which is what makes this fork and its packages
redistributable. The corresponding source stays available at
`https://github.com/sweetcornna/cmux-for-linux`. Every package ships the full
licence text at `/usr/share/licenses/cmux/LICENSE` and upstream's third-party
notices at `/usr/share/doc/cmux/THIRD_PARTY_LICENSES.md`.

Copyright in the cmux sources remains with Manaflow, Inc. and the upstream
contributors.
