# Dext compatibility patch

Upstream: `ratatui-core 0.1.2` from crates.io.

Dext uses this exact vendored source through the root `[patch.crates-io]` entry. The patch preserves Dext's inline viewport and ordinary native-scrollback behavior on the current Ratatui stack, with the documented full-ownership resize rebuild. It must remain small and should be removed when a released upstream version passes Dext's PTY and WSL checks without it.

## Patch hunks

- `src/terminal/buffers.rs`
  - `Terminal::clear` uses Ratatui's `last_known_cursor_pos` instead of synchronously querying the backend.
  - Reason: Crossterm cursor queries share its global event reader with Dext's keyboard thread and can stall or time out during transcript replay.

- `src/terminal/inline.rs`
  - The no-scrolling-regions `insert_before` fallback calls `clear_viewport` directly instead of the public cursor-preserving `clear`.
  - Reason: this path has already positioned the cursor and does not need another backend round trip for every insertion chunk.
  - Adds `Terminal::reset_inline_viewport`, which clears the visible display, resets both diff buffers, anchors an inline viewport at the terminal origin, and avoids a cursor query.
  - Reason: Dext clears the still-visible stale-width display before purging scrollback on every effective transcript-pane width change; the complete logical transcript must then replay from a known origin without racing Crossterm's input reader or retaining a duplicate intro.

- `src/terminal/resize.rs`
  - Horizontal shrink retains `ClearType::All` for fullscreen/fixed viewports but skips it for inline viewports.
  - Reason: OS-level horizontal shrink must not perform an extra whole-display clear before Dext's owned synchronized clear/purge/full replay. Dext performs that replay immediately for every effective transcript-pane width change.

## Refresh procedure

1. Replace this directory with the exact source for the new released `ratatui-core` version, retaining `LICENSE`.
2. Check whether each hunk is fixed upstream; reapply only the still-required behavior.
3. Update the root exact versions and lockfile.
4. Run the commands in `docs/TUI.md`, especially the real-PTY resize test.
5. Perform the live terminal checks documented there before release.
6. Update this file with the new upstream version and remaining hunks.

Do not enable `scrolling-regions` as a substitute without revalidating settled rendering; the 0.30.2 trial changed Dext's banner presentation and dependency surface.
