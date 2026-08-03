# cmux-gtk

A GTK4 frontend for cmux. See
[`../docs/linux-port.md`](../docs/linux-port.md) for the Linux port status.

The native Linux `cmux` package installs the TUI, relay and this GTK frontend
together. After installing it, launch `cmux-gtk` or click its desktop icon:

```bash
sudo apt install ./cmux_<version>_amd64.deb
cmux-gtk
```

With no arguments it connects to the `main` session. If that session is not
running, the GUI starts `cmux --headless --session main`, waits up to five
seconds and then attaches. `--session <name>` applies the same behavior to a
named session. From a source checkout, use
`cargo run --release -- --session main` in this directory.

## What it does

- connects to a cmux session over `cmux.protocol/1`, starting it when needed
- lists the session's workspaces in a sidebar
- renders pane splits and tabs from the server's styled render stream
- creates, closes and resizes panes through server-owned layout mutations
- sends keyboard and supported mouse input back to each pane's PTY

## What it deliberately does not do

**It contains no terminal emulator.** `cmux-tui/spec/render.md` makes the server
the only VT implementation: it sends styled runs, a cursor, resolved colours
and decoded Kitty image pixels with placement geometry, and a client draws
them. So there is no VT parser here, no libghostty, no VTE - only a cell grid,
Pango text, Pixbuf/cairo image drawing and a key mapper. Everything that would
normally be terminal-emulation complexity stays server-side.

## Layout

| File | Responsibility |
| --- | --- |
| `src/main.rs` | GTK application, window, sidebar, wiring |
| `src/config.rs` | runtime chrome palette, TUI theme overrides and GTK font loading |
| `src/session.rs` | protocol worker threads; render text and image payloads reach the UI without blocking it on a socket |
| `src/screen.rs` | cell and image state, with snapshot/patch merge semantics |
| `src/view.rs` | cairo/Pango/Pixbuf drawing and key-to-bytes translation |

It lives outside `cmux-tui/` because `packaging/linux/sync-upstream.sh`
replaces that directory wholesale from upstream.

## Debugging

`--probe` runs the protocol worker with no GTK or automatic session startup and
prints every update, which separates protocol failures from drawing failures:

```bash
cargo run --release -- --probe --session main
```

## Features

| | |
| --- | --- |
| Window chrome | custom 28px titlebar, terminal-background-derived colors, resizable 240px workspace sidebar, overlay error toasts, and no status bar |
| Panes | server-owned split layouts with per-pane PTY sizing, click-to-focus, split-right/split-down creation and pane close from the keyboard or context menu |
| Pane chrome | derived 1px separators with 6px resize hit regions, resize cursors, optional configured 2px active borders, and 70% dimming for inactive panes |
| Tabs | an always-visible 28px strip with focused/unfocused active states and the upstream action-lane fade |
| Input | keyboard input, including Ctrl and Alt sequences; `Ctrl+B %` / `Ctrl+B "` split right/down, `Ctrl+B X` closes a pane, `Ctrl+Shift+V` pastes through server-side bracketed-paste handling, and mouse input reaches applications |
| Workspaces | macOS-parity sidebar rows, draggable width clamped to one third of the window, switching, and a topology refresh every 3 seconds |
| Resize | dynamic window resize updates pane PTY sizes; dragging a split divider sends throttled server ratio mutations and a final authoritative value |
| Scrollback | the mouse wheel scrolls the viewport |
| Selection | drag to select; Shift overrides application mouse handling; `Ctrl+Shift+C` copies to the clipboard |
| Blink | protocol-provided text and cursor blink attributes, with a stable hollow block cursor while the window is unfocused |
| Inline images | server-decoded Kitty RGB/RGBA pixels with source cropping, cell-relative scaling, scroll-aware viewport placement and z-order; inactive panes use the same 70% dimming as text |
| Theme | runtime chrome colors from the active terminal background plus explicit `cmux-tui.json` overrides; GTK font from `cmux-gtk.json` or `CMUX_GTK_FONT` |

The visual metrics and color rules follow
[`../docs/gtk-design-parity.md`](../docs/gtk-design-parity.md).

## Theme

Window chrome is recomputed whenever the active terminal's resolved background
changes. The separator uses the upstream lightness offset, the sidebar is a
solid approximation of the macOS sidebar material, and `theme.chrome` accepts
`auto`, `light`, or `dark`. `auto` uses the same luminance threshold as the
TUI. Before the first render state arrives, fallback backgrounds are `#1e1e1e`
for dark/auto and `#feffff` for light.

The GTK frontend honors these keys under `theme` in `cmux-tui.json`:

- `chrome`
- `border_active`
- `border_inactive`
- `selection_background`
- `selection_foreground`
- `sidebar_rail`
- `sidebar_active_bg`
- `tab_bg`
- `tab_active_bg`

Explicit theme colors take precedence over the parity defaults. In particular,
the 2px active-pane border is off unless `border_active` is configured.
`sidebar_rail` overrides the rail color when workspace metadata supplies a
workspace color; no rail is drawn for an uncolored workspace.
Xterm-256 colour indexes are accepted for color values. A malformed
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
| Right-click | Focuses the pane and opens its split/close `PopoverMenu` | No |
| Drag from a split divider | Resizes the server-owned split through a 6px hit region and shows a row/column resize cursor | No |
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

No remaining stage 2 gaps are currently tracked.
