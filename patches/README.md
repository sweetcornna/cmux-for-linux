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
`decode_render_patch`. `cmux-gtk` renders text runs only, so dropping the
graphics payload loses nothing it would draw — inline images would need real
handling here.

Reproduce without the patch:

```bash
cargo run --release -p cmux-gtk -- --probe --session <name>
# Error("stream error: render snapshot contains unknown fields: graphics, history_epoch")
```

This affects any Rust SDK consumer of `terminal.attach()`, not just this fork,
and is worth reporting upstream.
