# Dauerbot terminal component patches

Upstream: gpui-terminal 0.1.0, https://github.com/zortax/gpui-terminal.
License: MIT OR Apache-2.0. Original source and copyright notices retained.

Dauerbot patches add native input-method composition, clipboard paste/copy,
terminal selection, and callbacks for read-only input and remote scrolling.
The terminal emulator remains Alacritty. The component accepts streams and
never owns the Agent process.

The keyboard modifier behavior, input focus discipline, and VT-cell rendering
behavior were adapted in November 2026 from Herdr App's native terminal:
https://github.com/herdrdev/herdr-app at
`321c9a84bc928f1c31be59e73dad966d86a3e252` (`src/terminal.rs`).
Herdr App is licensed under Apache-2.0; its license is included here as
`LICENSE-APACHE`. These are modified adaptations for Dauerbot's pinned GPUI,
Alacritty emulator and legacy HERDR 0.9.5 ANSI transport, rather than an hc or
Ghostty runtime transplant. Native Option symbols and dead keys stay with AppKit. Only the requested
unmodified Option+M chord is reserved as AGY's Meta+M shortcut, outside active
composition; navigation/control modifiers retain their terminal encoding.

The adaptations include modifier-preserving navigation and Meta chords,
Return/IME and multiline paste handling, focus acquisition after input takeover,
RGB/inverse/dim/hidden cell colors, combining text, wide-cell backgrounds, and
pixel-aligned cell metrics. Existing upstream notices remain intact.

## Mechanism comparison and adaptation

Reference sources are Herdr App `src/terminal.rs` (`output_turn`, `ingest`,
`redraw_soon`, `Grid::painted`, `row_ops`, `paint_grid`, `key_bytes`, and its
`EntityInputHandler`) and `src/vt.rs` (`frame`, dirty rows, selection and
viewport). They were read at the commit above, including the data flow around
those functions rather than only the key table.

| Mechanism | Herdr App | Dauerbot adaptation |
| --- | --- | --- |
| Ingestion | Ordered output, six milliseconds per turn; never drops bytes | Ordered ANSI frame/delta ingestion in at most 16 KiB parser units with the same time budget, then yields to GPUI input even inside multi-MiB snapshots; each source resize precedes its first chunk exactly once |
| Refresh | Shared 33 ms clock, immediate for 200 ms after typing | 33 ms coalescing per view, immediate after typing; Dauerbot has one visible terminal rather than a multi-pane canvas |
| Retained drawing | Ghostty dirty rows retain row identities; row operations shaped only when invalidated | Raw VT cells retain each row's shaped drawing; content, fonts, pixel metrics, palette/OSC overrides invalidate it; selection remains an overlay |
| Row shaping | Batched styles with explicit glyph/cell positions; complex glyphs separate | Fixed-width ASCII style runs and separate wide/combining clusters, with calt/liga/dlig disabled in the shared cell-font factory; pinned GPUI has no `with_cell_positions` API |
| Metrics | Font tables, device-pixel cells and common baseline | Native font measurement, device-pixel rounding and common primary-font baseline; font tables and bundled Ghostty sprites are not transplanted |
| Input | VT keyboard encoder, focus ownership, committed IME text separate from marked text | Legacy xterm modifier encoding, native Option text/dead keys with a narrow AGY Option+M shortcut, committed/marked IME separation, focus acquisition at takeover; no implicit writes in observation mode |
| Viewport | Local Ghostty owns history/mouse/alternate screen modes | Legacy HERDR owns source modes/history; wheel events preserve geometry and gesture magnitude; rendered snapshots never become fabricated local history |
| Lifecycle | Native attach/detach with ordered stream termination | Descriptor/generation ownership remains unchanged; reconnect joins the old frame reader before publishing a new full baseline; dropping the view cancels its reader/redraw tasks |

The legacy terminal stream contains rendered ANSI snapshots and deltas, not
the program's raw output or full keyboard-protocol negotiation. It preserves
cell RGB and text attributes, but cannot expose Ghostty's Kitty keyboard,
graphics, focus-reporting and local dirty-row interfaces wholesale. Dauerbot
does not infer source application modes from its repaint emulator. Native
visual and keyboard acceptance therefore remains necessary alongside parser
and input-byte regression tests.

Pinned GPUI 0.2's `KeyDownEvent` has no `prefer_character_input` field. Its
macOS backend dispatches marked-text events through AppKit first
(`platform/mac/window.rs`); the terminal also yields whenever marked text is
active. The reference's `prefer_character_input` guard is Windows-only.
Consequently this adaptation preserves AppKit's Option character/dead-key path
and consumes only the requested unshifted Option+M when no composition owns it.
