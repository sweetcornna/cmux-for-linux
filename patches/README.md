# Carried patches

Patches this fork applies to upstream files under `cmux-tui/`.

`packaging/linux/sync-upstream.sh` replaces `cmux-tui/` wholesale from upstream,
which would discard any edit made there. It re-applies every `*.patch` in this
directory after the sync, fails loudly if one no longer applies, and reports
one that upstream has already taken so it can be deleted.

Anything this fork *authors* belongs outside `cmux-tui/` — `gui/`,
`packaging/`, `docs/` — not in a patch. Patches are only for changing upstream
code.

## 0001 — accept `graphics` and `history_epoch` in render frames

The hand-written typed-stream decoder in the Rust SDK
(`bindings/rust/src/resource/typed_stream.rs`) validates render frames with a
`finish()` call that rejects unknown fields, but never consumes the `graphics`
and `history_epoch` fields the server actually sends. Every real
`terminal.attach()` render snapshot therefore failed to decode with:

```
render snapshot contains unknown fields: graphics, history_epoch
```

The generated private-protocol types (`RenderStateEvent`, `RenderDeltaEvent`)
already carry both fields, so this is the public decoder lagging its own
server rather than a protocol disagreement.

The patch consumes and discards them in `decode_render_snapshot` and
`decode_render_patch`. This is the compatibility base in the carried patch
series; patch 0004 builds on it and replaces the graphics discard with public
typed decoding.

Reproduce without the patch:

```bash
cargo run --release -p cmux-gtk -- --probe --session <name>
# Error("stream error: render snapshot contains unknown fields: graphics, history_epoch")
```

This affects any Rust SDK consumer of `terminal.attach()`, not just this fork,
and is worth reporting upstream.

## 0002 — let a terminal attachment claim sizing authority

A viewer lease says "I am looking at this terminal at this size"; it does not
resize the PTY. Sizing authority is a separate request, `client.sizing.set`,
and the server rejects it unless the *same client* already holds a size lease:

```
the selected client has no size lease for the terminal
```

The SDK's `Client::set_sizing` sends that over the shared control connection,
which is a different client from the one holding the lease, so it can never
succeed. The lease lives on the attachment's own connection, and
`TerminalAttachment` exposed only `resize` and `release`; `connection_control`
is `pub(crate)`.

The result is that a Rust protocol client can render a terminal but can never
drive its size. The patch adds `TerminalAttachment::set_sizing`, which issues
the request on the connection that owns the lease.

Verified: with the patch, resizing the `cmux-gtk` window drives the PTY
through 80x24 → 98x36 → 54x19 → 126x45. Without it the PTY stays at whatever
size it was created with.

Like 0001, this affects any Rust SDK consumer, not just this fork.

## 0003 — route a `paste` flag through `terminal.input.write`

`cmux.protocol/1` had no paste operation. `Terminal::write_text` writes raw
bytes, and `TextInputOptions` fed only the browser surface's insert-text path,
so a protocol client could not reach the server's bracketed-paste logic. The
server already owns that logic — `Surface::write_paste` snapshots DEC private
mode 2004 and conditionally wraps the payload — but nothing routed to it.

The patch adds an optional boolean `paste` field to the existing
`terminal.input.write` operation (spec, catalog fingerprints, and the Rust
SDK's `Terminal::paste`/`paste_with`), and routes `paste: true` to
`Surface::write_paste` in the resource router. Old clients are unaffected;
the field is optional and defaults to false.

This is what `cmux-gtk` uses for `Ctrl+Shift+V`. The payload is passed
through verbatim, matching the native TUI's own terminal paste behavior
(`app.rs` `paste()`), which delegates all mode-2004 decisions to the server
side of the same code path.

Like 0001 and 0002, the missing paste route affects any protocol client, not
just this fork, and is worth reporting upstream.

## 0004 - expose render graphics through the typed Rust SDK

The generated private protocol already defines `RenderGraphics`,
`RenderGraphicsDelta`, raw RGB/RGBA images, placement geometry and image
deletions, but the public resource API used by `terminal.attach()` discarded
that field in patch 0001. This patch adds public equivalents to
`RenderSnapshot` and `RenderPatch`, decodes base64 pixels to bytes at the SDK
boundary, and preserves optional fields so frames from servers without
graphics remain compatible. Unknown future image formats remain typed as
unsupported instead of failing the whole attachment; a renderer can skip or
show a placeholder for that image.

This is a new patch rather than a rewrite of 0001 even though both touch the
same decoder. Keeping 0001 unchanged preserves the original compatibility fix
and makes the dependency explicit: sync applies 0001 first, then 0004 promotes
the consumed graphics value into typed data. It also keeps the carried patch
history reviewable and localizes future upstream conflicts to the semantic
upgrade.

## 0005 — make resource event values satisfy snapshot schemas

The resource event stream decodes every upsert value as the corresponding
public snapshot type. Full-tree and tab-move delta encoders in
`mux/resource_content.rs` omitted the required `focused` field from
`TabSnapshot` values, so the first tree mutation ended the stream with:

```
invalid tab: missing field `focused`
```

The full-tree terminal encoder also omitted the required `lifecycle` field.
`TerminalSnapshot` requires `running` to be true exactly for the `running`
lifecycle and requires `exit` exactly for the `exited` lifecycle. The same
incomplete terminal value was returned by `terminal.move` even though that
operation returns a `TerminalSnapshot`.

Reproduce without the patch:

```bash
cmux-tui --headless --session delta-schema &
cargo run --release -p cmux-gtk -- --probe --session delta-schema
cmux-tui --session delta-schema workspace create --name second
# Disconnected: session event stream error: invalid tab: missing field `focused`
```

The patch derives tab focus from the owning pane and mirrors the existing
terminal snapshot lifecycle, `cwd` and exit mapping. The fields were already
required by `spec/resource-operations-v1.json`, so no spec or catalog
fingerprint changes are needed. This affects any protocol client consuming
session resource events or typed `terminal.move` results, not just this fork,
and is worth reporting upstream.

## 0006 — align CLI resource enums with the protocol spec

The CLI validated notification levels and agent states against vocabularies
that disagree with the public resource schema. It accepted notification level
`success` and agent states `running`, `waiting` and `error`, but the server
rejects those values with:

```
value is outside the allowed enum
```

Reproduce without the patch:

```bash
cmux agent report --terminal <terminal-id> --state running --source socket
cmux notification create --title status --body updated --level success --terminal <terminal-id>
```

`spec/resource-operations-v1.json` defines `AgentState` as `working`,
`blocked`, `idle`, `done` or `unknown`, and `NotificationLevel` as `info`,
`warning` or `error`. The patch makes both CLI validators use those exact
enums and adds parser coverage for every canonical value and the rejected
legacy spellings.

This affects any CLI user reporting agent state or creating notifications,
not just this fork, and is worth reporting upstream.

## 0007 — create the fallback runtime directory before binding into it

`platform::fallback_runtime_dir()` returns `/tmp/cmux-tui-<uid>` and nothing
creates it, so every caller that binds a socket there fails when the directory
is absent. Three server tests bind straight into it and fail on a machine that
has never had one:

```
$ rm -rf /tmp/cmux-tui-1000
$ cargo test --release -p cmux-tui-core
server::tests::clear_history_does_not_block_unrelated_surface_input_on_one_connection
server::tests::lifecycle_command_waits_for_active_clear_history_on_one_connection
server::tests::write_side_eof_drains_accepted_surface_requests
test result: FAILED. 838 passed; 3 failed
```

The failure is `Os { code: 2, kind: NotFound }` from `transport::listen`, not an
assertion, because the path's parent does not exist.

The patch creates the directory in that one accessor at 0o700, which is what
`XDG_RUNTIME_DIR` would have provided. Fixing the accessor rather than each
test also covers the production path, which reaches this fallback whenever
`XDG_RUNTIME_DIR` is unset. With the patch the suite is 841 passed, 0 failed.

Any consumer of this crate on a host without `XDG_RUNTIME_DIR` hits the same
missing directory, so this is worth reporting upstream.
