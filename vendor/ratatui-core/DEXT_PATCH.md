# Dext compatibility patch

Upstream: `ratatui-core 0.1.2` from crates.io.

Dext uses this exact vendored source through the root `[patch.crates-io]` entry. The patch preserves Dext's inline viewport and native-scrollback behavior on the current Ratatui stack. It must remain small and should be removed when a released upstream version passes Dext's PTY and WSL checks without it.

## Patch hunks

- `src/terminal/buffers.rs`
  - `Terminal::clear` uses Ratatui's `last_known_cursor_pos` instead of synchronously querying the backend.
  - Reason: Crossterm cursor queries share its global event reader with Dext's keyboard thread and can stall or time out during transcript replay.

- `src/terminal/inline.rs`
  - The no-scrolling-regions `insert_before` fallback calls `clear_viewport` directly instead of the public cursor-preserving `clear`.
  - Reason: this path has already positioned the cursor and does not need another backend round trip for every insertion chunk.
  - Adds `Terminal::overwrite_before`, which draws the bottom rows of a rendered buffer directly above the inline viewport with absolute writes and no scrolling.
  - Reason: inline scrollback is append-only, so replaying a resized transcript through `insert_before` permanently appends a duplicate history copy to terminal scrollback; the overwrite repaints the visible tail in place.

- `src/terminal/resize.rs`
  - Horizontal shrink retains `ClearType::All` for fullscreen/fixed viewports but skips it for inline viewports.
  - Reason: a whole-display clear flashes native scrollback and exposes transcript replay. The following viewport clear plus complete draw repaints Dext's inline surface.

## Refresh procedure

1. Replace this directory with the exact source for the new released `ratatui-core` version, retaining `LICENSE`.
2. Check whether each hunk is fixed upstream; reapply only the still-required behavior.
3. Update the root exact versions and lockfile.
4. Run the commands in `docs/TUI.md`, especially the real-PTY resize test.
5. Perform the live terminal checks documented there before release.
6. Update this file with the new upstream version and remaining hunks.

Do not enable `scrolling-regions` as a substitute without revalidating settled rendering; the 0.30.2 trial changed Dext's banner presentation and dependency surface.
