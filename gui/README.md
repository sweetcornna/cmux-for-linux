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

The titlebar session button lists live cmux sockets in the current session's
runtime directory. The list refreshes when the menu opens. Selecting another
session switches the same window to it; **New session...** accepts a non-empty
name, connects to an existing session with that name, or starts a new headless
session beside the current socket. A failed switch restores the previous
session and reports the failure in a toast.

## What it does

- connects to a cmux session over `cmux.protocol/1`, starting it when needed
- lists, switches and creates local sessions without opening another window
- seeds workspace topology from the session event snapshot and coalesces
  topology-affecting deltas into sidebar and layout refreshes
- renders pane splits and tabs from the server's styled render stream
- creates, closes and resizes panes through server-owned layout mutations
- sends keyboard and supported mouse input back to each pane's PTY, with keys
  encoded server-side against the pane's live terminal state

## What it deliberately does not do

**It contains no terminal emulator.** `cmux-tui/spec/render.md` makes the server
the only VT implementation: it sends styled runs, a cursor, resolved colours
and decoded Kitty image pixels with placement geometry, and a client draws
them. So there is no VT parser here, no libghostty, no VTE - only a cell grid,
Pango text, Pixbuf/cairo image drawing and a key namer. Everything that would
normally be terminal-emulation complexity stays server-side.

That rule covers input as well. Which bytes a key produces is not a property of
the key: it depends on application cursor mode, `modifyOtherKeys`, and the
Kitty keyboard flags an agent turns on to tell `Shift+Tab` and `Shift+Enter`
apart from `Tab` and `Enter`. So a key this window can name travels over
`terminal.input.keys` as a chord such as `shift+tab` or `ctrl+right` and is
encoded by the same Ghostty encoder the TUI uses, against that pane's live
terminal. Only the characters the encoder has no physical key for - `Ctrl+@`,
`Ctrl+^`, `Ctrl+_`, and most of what a non-US layout produces - are encoded
here, and plain text stays with the input method.

## Git branch exception

The frontend speaks the cmux protocol for terminal, topology, focus, status and
working-directory state, and it still contains no VT parser. The one deliberate
exception is the workspace git-branch line: the protocol carries no git state,
so a background GUI worker runs
`git -C <cwd> status --porcelain=v2 --branch` for each active terminal working
directory. Results, including failures, are cached per directory for five
seconds and return through the same update channel as protocol work. Each child
has a one-second deadline and is killed and reaped on timeout, so Git never
blocks the GTK thread.

## Layout

| File | Responsibility |
| --- | --- |
| `src/main.rs` | GTK application, window, sidebar, wiring |
| `src/attention.rs` | pure notification severity and agent-state rollups by terminal |
| `src/config.rs` | runtime chrome palette, TUI theme overrides and GTK font loading |
| `src/git_branch.rs` | timed Git status worker and per-directory branch cache |
| `src/search.rs` | case-insensitive scrollback matching, result navigation and pure search geometry |
| `src/session.rs` | protocol worker supervisor, session socket enumeration and joined control/attachment runtimes; render payloads reach GTK without blocking it on a socket |
| `src/screen.rs` | cell and image state, with snapshot/patch merge semantics |
| `src/view.rs` | cairo/Pango/Pixbuf drawing and key-to-chord translation |

It lives outside `cmux-tui/` because `packaging/linux/sync-upstream.sh`
replaces that directory wholesale from upstream.

## Debugging

`--probe` runs the protocol worker with no GTK or automatic session startup and
prints every update, which separates protocol failures from drawing failures:

```bash
cargo run --release -- --probe --session main
```

If `--probe` receives updates but the GTK window does not change, enable the
GTK consumer trace. `CMUX_GTK_TRACE=1` prints one stderr line for every update
handled by the GTK main loop, including workspace counts and the terminal ID
for render, attachment, scroll and search updates:

```bash
CMUX_GTK_TRACE=1 cargo run --release -- --session main
```

## Features

| | |
| --- | --- |
| Window chrome | custom draggable 28px titlebar with GTK-configured double-, middle- and right-click actions, terminal-derived automatic colors with explicit light/dark bases, resizable 240px workspace sidebar, overlay error toasts, and no status bar |
| Sessions | titlebar selector for live sibling sockets, current-session highlighting, in-place switching with rollback, and prompt-based headless session creation |
| Panes | server-owned split layouts with per-pane PTY sizing, click-to-focus, split-right/split-down creation and pane close from the keyboard or context menu |
| Pane chrome | derived 1px separators with 6px resize hit regions, resize cursors, optional configured 2px active borders, and 70% dimming for inactive panes |
| Screen tabs | a workspace with multiple screens uses a 28px strip with focused/unfocused states, hover close, middle-click close, double-click rename and a new-screen button in the upstream action-lane fade; the strip is hidden for one screen |
| Pane tabs | each always-visible pane strip supports switching, hover or middle-click close, double-click rename and terminal-tab creation; closing the last tab follows server collapse semantics |
| Notification markers | unread terminal notifications add severity-colored bullets to pane and screen tabs and a highest-severity dot to each affected workspace row |
| Agent state | terminal tabs show the upstream 12-spoke spinner while an agent is working and a static 9px ring for blocked, idle and done; the spinner turns only while the window is focused. Workspace rows show the highest-priority reported state across their terminals |
| Input | keyboard input encoded server-side against each pane's terminal, so `Shift+Tab`, `Shift+Enter`, function keys, `Insert`, modified arrows such as `Ctrl+Right` and application cursor mode all reach the application the way the TUI sends them; the default `Ctrl+B` lifecycle shortcuts listed below; `Ctrl+Shift+V` pastes through server-side bracketed-paste handling; and mouse input reaches applications |
| Workspaces | macOS-parity sidebar rows with optional agent-status, git-branch and active-terminal working-directory details, switching, hover close, double-click rename, drag reordering and titlebar creation; the resizable sidebar is clamped to one third of the window and topology follows coalesced server resource events |
| Resize | dynamic window resize updates pane PTY sizes; dragging a split divider sends throttled server ratio mutations; runtime font zoom reuses the same resize channel for every visible pane |
| Scrollback | the mouse wheel scrolls the viewport; a rounded 4px overlay thumb appears off-bottom, widens on hover, and supports direct dragging |
| Scrollback search | `Ctrl+Shift+F` opens a prompt-themed search bar; literal case-insensitive matches cover retained history plus the current viewport, Enter/Shift+Enter navigate them, and visible hits use accent at 25% opacity |
| Selection | drag to select; Shift overrides application mouse handling; `Ctrl+Shift+C` copies to the clipboard |
| Blink | protocol-provided text and cursor blink attributes, with a stable hollow block cursor while the window is unfocused |
| Inline images | server-decoded Kitty RGB/RGBA pixels with source cropping, cell-relative scaling, scroll-aware viewport placement and z-order; inactive panes use the same 70% dimming as text |
| Theme | automatic runtime chrome colors from the active terminal background, explicit light/dark chrome bases and `cmux-tui.json` overrides; a `terminal` section in `cmux-gtk.json` restyles the grid's colors, ANSI palette, cursor and cell metrics; GTK font from `cmux-gtk.json` or `CMUX_GTK_FONT`, with non-persistent 6pt-32pt runtime zoom |

The visual metrics and color rules follow
[`../docs/gtk-design-parity.md`](../docs/gtk-design-parity.md).

## Keyboard shortcuts

The GUI follows the TUI's default `Ctrl+B` prefix. Press and release the prefix,
then press the action key:

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+F` | Open scrollback search for the focused terminal |
| `Enter` / `Shift+Enter` (in search) | Jump to the previous / next match |
| `Esc` (in search) | Close scrollback search |
| `Ctrl++` / `Ctrl+=` | Increase the terminal font size by 1pt, up to 32pt |
| `Ctrl+-` | Decrease the terminal font size by 1pt, down to 6pt |
| `Ctrl+0` | Reset the terminal font to the configured default |
| `Ctrl+B c` | Create a workspace screen (`NewScreen`) |
| `Ctrl+B t` | Create a terminal tab in the focused pane (`NewTab`) |
| `Ctrl+B x` | Close the focused pane tab (`CloseTab`); the server collapses a pane when this was its last tab |
| `Ctrl+B Tab` / `Ctrl+B Shift+Tab` | Focus the next / previous pane tab, wrapping at either end |
| `Ctrl+B ,` | Rename the active workspace screen |
| `Ctrl+B &` | Close the active workspace screen (`CloseScreen`), not merely its focused pane tab |
| `Ctrl+B $` | Rename the active workspace |
| `Ctrl+B %` / `Ctrl+B "` | Split the focused pane right / down |
| `Ctrl+B X` | Close the focused pane |

Every other key belongs to the focused pane, `Shift+Tab` included: it reaches
the application as a backtab rather than moving GTK's focus, which is the press
coding agents such as Claude Code read as "switch mode".

`Ctrl+B c` keeps screen creation reachable while a one-screen workspace hides
the screen strip. Once the strip appears, its `+` button also creates a screen.
The titlebar `+` button creates a workspace. Those operations have separate TUI
actions and are not remapped onto `NewTab`.

## Theme

Window chrome is recomputed whenever the active terminal's resolved background
changes. The separator uses the upstream lightness offset, the sidebar is a
solid approximation of the macOS sidebar material, and `theme.chrome` accepts
`auto`, `light`, or `dark`. `auto` derives every chrome surface from the
terminal background using the TUI luminance threshold. An explicit `light` or
`dark` mode continues using that terminal background when its luminance agrees;
when it disagrees, chrome surfaces instead use the mode fallback (`#feffff` for
light or `#1e1e1e` for dark). Terminal cells retain their own resolved
background and foreground in every mode. Before the first render state arrives,
dark/auto uses `#1e1e1e` and light uses `#feffff`.

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
- `scrollbar_thumb_fg`
- `scrollbar_thumb_active_fg`
- `prompt_bg`
- `prompt_fg`
- `prompt_border`
- `prompt_input_bg`
- `prompt_input_fg`
- `notification_info`
- `notification_warning`
- `notification_error`

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

Runtime zoom changes only the current process and never writes either config
file. Each change recomputes cell metrics and resizes every visible pane PTY.

## Terminal appearance

`cmux-tui.json` styles the window chrome for every frontend. A `terminal`
section in `cmux-gtk.json` restyles the grid itself, for this window only:

```json
{
  "font": "monospace 11",
  "terminal": {
    "foreground": "#161107",
    "background": "#f6f1e5",
    "cursor": "#0088fe",
    "cursor_shape": "bar",
    "cursor_blink": false,
    "line_height": 1.25,
    "letter_spacing": 0.5,
    "padding": 8,
    "palette": { "red": "#c0392b", "bright_blue": "#7aa6da" }
  }
}
```

| Key | Effect |
| --- | --- |
| `foreground`, `background` | Replace the defaults the server resolves for each frame. A configured background is also the base the chrome derives from, so the window matches its own grid. |
| `cursor` | Cursor colour, replacing the protocol's own. |
| `cursor_shape` | `block`, `bar` or `underline`, overriding the shape the terminal requested. |
| `cursor_blink` | Forces blinking on or off instead of following the protocol attribute. |
| `line_height` | Multiplier on the font's natural cell height, 0.8 to 3.0. Added space is split above and below so glyphs stay centred. |
| `letter_spacing` | Extra pixels per cell, up to 8. |
| `padding` | Pixels between a pane's edge and its first cell, up to 64. |
| `palette` | ANSI 0-15. Either an ordered array or named keys: `red`, `bright_blue`, or `color7`. |

Colours accept `#rrggbb`, `#rgb` or an xterm-256 index. Out-of-range metrics are
clamped rather than rejected, and an unreadable value leaves that single setting
at its default instead of discarding the file.

`palette` has one limitation worth knowing. `spec/render.md` has the server
send runs as resolved RGB with palette indexes already applied, so the frontend
recognises an ANSI slot by the colour the server resolved it to. A run that an
application recoloured through its own OSC 4 sequence no longer matches a known
slot and is drawn exactly as the server sent it.

## Mouse policy

| Input | Handled by the GTK frontend | Forwarded to the pane application |
| --- | --- | --- |
| Click | Focuses the pane | The button event is forwarded when the application has requested mouse input |
| Click on screen/pane tab chrome | Focuses, creates, closes or opens the rename prompt for the selected protocol resource | No |
| Middle-click on a screen or pane tab | Closes that exact protocol resource | No |
| Right-click | Focuses the pane and opens its split/close `PopoverMenu` | No |
| Drag a workspace row | Reorders it through `Workspace::move_to` and shows the accent drop indicator | No |
| Drag from a split divider | Resizes the server-owned split through a 6px hit region and shows a row/column resize cursor | No |
| Drag the overlay scrollbar thumb | Converts thumb travel to retained-history rows and scrolls through the server viewport API | No |
| Shift+drag | Selects text locally | No |
| Wheel over an alternate-screen application using mouse input | No local scrollback | Yes |
| Wheel otherwise | Scrolls local history | No |
| Pointer move with no button held | No | Once per cell change; the server forwards it only when the application has enabled mode 1003 |

## Verified

The following were checked against live sessions:

- pane splits render with per-pane PTY sizes, and dynamic window resizing
  updates those sizes;
- the tab strip is present, panes accept click-to-focus, and workspace
  switching follows refreshed server topology;
- keyboard input includes Ctrl and Alt sequences;
- application mouse forwarding works for clicks in `htop` and wheel input in
  an alternate-screen application;
- wheel scrollback and drag selection work, with Shift overriding application
  mouse handling;
- border colours and xterm-256 indexes are read from `cmux-tui.json`, the GTK
  font is read separately from `cmux-gtk.json`, and a malformed configuration
  does not prevent startup.

## Known gaps

The server drops an agent record only when its terminal is tombstoned, so a
record outlives the process that reported it: an agent that exits or is killed
without reporting a final state leaves `working` behind indefinitely. Rows
therefore stop presenting `working` and `blocked` for a terminal that has
exited, while settled states such as `done` remain. This does not invent a
second agent-state model, which `spec/frontends.md` rules out; it declines to
repeat an activity claim the terminal's own lifecycle contradicts.

The server clears a focused surface's in-memory unread marker after its
resource focus publication and emits no notification lifecycle change. GTK
therefore clears matching notification markers locally after a successful
terminal focus; the next session snapshot remains the authoritative resync.
