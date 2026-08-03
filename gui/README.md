# cmux-gtk

A GTK4 frontend for cmux. Stage 1 of the Linux GUI in
[`../docs/linux-port.md`](../docs/linux-port.md).

```bash
cmux --headless --session main &     # a session to attach to
cargo run --release -- --session main
```

## What it does

- connects to a running cmux session over `cmux.protocol/1`
- lists the session's workspaces in a sidebar
- attaches to a terminal and renders it from the server's styled render stream
- sends key presses back to the PTY

## What it deliberately does not do

**It contains no terminal emulator.** `cmux-tui/spec/render.md` makes the server
the only VT implementation: it sends styled runs, a cursor and resolved
colours, and a client draws them. So there is no VT parser here, no libghostty,
no VTE — only a cell grid, Pango text and a key mapper. Everything that would
normally be terminal-emulation complexity stays server-side.

## Layout

| File | Responsibility |
| --- | --- |
| `src/main.rs` | GTK application, window, sidebar, wiring |
| `src/session.rs` | protocol worker threads; the UI thread never blocks on a socket |
| `src/screen.rs` | the cell grid, and snapshot/patch merge semantics |
| `src/view.rs` | cairo/Pango drawing and key-to-bytes translation |

It lives outside `cmux-tui/` because `packaging/linux/sync-upstream.sh`
replaces that directory wholesale from upstream.

## Debugging

`--probe` runs the protocol worker with no GTK and prints every update, which
separates protocol failures from drawing failures:

```bash
cargo run --release -- --probe --session main
```

## Features

| | |
| --- | --- |
| Rendering | styled runs with colour, bold, italic, underline, inverse and faint; block/underline/bar cursor |
| Input | full keyboard, including Ctrl and Alt sequences and the arrow/navigation keys |
| Workspaces | sidebar lists them and switches the attached terminal; refreshed every 3s so workspaces created elsewhere appear |
| Resize | the PTY follows the window — the frontend claims exclusive sizing authority for its viewer lease |
| Scrollback | mouse wheel scrolls the viewport; the status line shows when it is not at the bottom |
| Selection | drag to select, `Ctrl+Shift+C` copies to the clipboard |

## Verified

On Ubuntu 26.04, against live sessions:

- styled runs render with correct colours and all five attributes; `ls --color`
  output and shell prompt colours match the TUI;
- text typed into the window reaches the PTY, confirmed by reading the terminal
  back over the protocol;
- resizing the window resizes the PTY: 80x24 → 98x36 → 54x19 → 126x45, with the
  server reporting `accepted=true` at each step;
- clicking a sidebar row re-attaches to that workspace's terminal;
- the wheel scrolls into history and the status line switches to "scrolled back";
- a drag selects text and `Ctrl+Shift+C` puts it on the clipboard.

Two things make GUI testing under `Xvfb` misleading, and both produced false
failures before being accounted for: nothing grants keyboard focus without a
window manager, and `xdotool search` also matches GTK's 1x1 helper windows, so
resizing "the window" can silently resize nothing.

## Known gaps

- One terminal per workspace. A workspace with several terminals attaches to
  the first; there is no tab strip yet.
- No splits or tabs of its own — it renders one terminal, not a pane layout.
- No mouse reporting to the application, and no theming beyond the server's
  colours.
- `graphics` payloads (inline images) are dropped; see `../patches/README.md`.
