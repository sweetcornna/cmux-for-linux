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

## Verified

On Ubuntu 26.04, against a live session: styled runs render with correct
colours, bold, italic, underline, inverse and faint; `ls --color` output and
shell prompt colours match the TUI; the block cursor tracks the server's
position; and text typed into the window reaches the PTY, with the result
arriving back through the render stream. Keyboard verification needs a window
manager — under a bare `Xvfb` nothing grants focus and every key press is
silently dropped, which looks like a broken key handler.

## Known gaps

- One terminal only. The sidebar lists workspaces and focuses them, but the
  view stays attached to the terminal it first found.
- The viewer size is fixed at 100x30. Resizing the window does not resize the
  terminal: the viewer lease lives on the attachment's connection, which the
  stream-reading thread owns while blocked on the iterator.
- No scrollback, selection, mouse input, splits, tabs or theming.
- `graphics` payloads (inline images) are dropped; see `../patches/README.md`.
