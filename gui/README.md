# cmux-gtk

A GTK4 frontend for cmux. See
[`../docs/linux-port.md`](../docs/linux-port.md) for the Linux port status.

```bash
cmux --headless --session main &     # a session to attach to
cargo run --release -- --session main
```

## What it does

- connects to a running cmux session over `cmux.protocol/1`
- lists the session's workspaces in a sidebar
- renders pane splits and tabs from the server's styled render stream
- sends keyboard and supported mouse input back to each pane's PTY

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
| Panes | split layouts with per-pane PTY sizing and click-to-focus |
| Tabs | a tab strip for the session's tabs |
| Input | keyboard input, including Ctrl and Alt sequences; `Ctrl+Shift+V` paste with server-side bracketed-paste handling; mouse input to applications |
| Workspaces | sidebar switching with a topology refresh every 3 seconds |
| Resize | dynamic window resize updates the pane PTY sizes |
| Scrollback | the mouse wheel scrolls the viewport |
| Selection | drag to select; Shift overrides application mouse handling; `Ctrl+Shift+C` copies to the clipboard |
| Blink | protocol-provided text and cursor blink attributes, with a stable hollow block cursor while the window is unfocused |
| Theme | border colours from `cmux-tui.json`; GTK font from `cmux-gtk.json` or `CMUX_GTK_FONT` |

## Configuration

The GTK frontend honors these keys under `theme` in `cmux-tui.json`:

- `border_active`
- `border_inactive`
- `selection_background`
- `selection_foreground`

Xterm-256 colour indexes are accepted for these values. A malformed
`cmux-tui.json` or `cmux-gtk.json` does not prevent startup; the frontend
retains its defaults for settings it cannot read.

The GTK font is separate from the shared TUI theme. Set it in `cmux-gtk.json`:

```json
{
  "font": "monospace 11"
}
```

`CMUX_GTK_FONT` sets the font for the process and takes precedence over the
value in `cmux-gtk.json`:

```bash
CMUX_GTK_FONT="monospace 12" cmux-gtk --session main
```

## Mouse policy

| Input | Handled by the GTK frontend | Forwarded to the pane application |
| --- | --- | --- |
| Click | Focuses the pane | The button event is forwarded when the application has requested mouse input |
| Shift+drag | Selects text locally | No |
| Wheel over an alternate-screen application using mouse input | No local scrollback | Yes |
| Wheel otherwise | Scrolls local history | No |
| Pointer move with no button held | No | Once per cell change; the server forwards it only when the application has enabled mode 1003 |

## Verified

The following were checked against live sessions:

- pane splits render with per-pane PTY sizes, and dynamic window resizing
  updates those sizes;
- the tab strip is present, panes accept click-to-focus, and workspace
  switching follows topology refreshed every 3 seconds;
- keyboard input includes Ctrl and Alt sequences;
- application mouse forwarding works for clicks in `htop` and wheel input in
  an alternate-screen application;
- wheel scrollback and drag selection work, with Shift overriding application
  mouse handling;
- border colours and xterm-256 indexes are read from `cmux-tui.json`, the GTK
  font is read separately from `cmux-gtk.json`, and a malformed configuration
  does not prevent startup.

## Known gaps

- Inline images are not implemented. The work was assessed at roughly 450-650
  lines and deferred.
- No pane create, close or resize from the GUI itself.
