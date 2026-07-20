# Terminal UI

Dext's interactive interface is an inline Ratatui application in the regular terminal buffer. It preserves native terminal scrollback instead of taking over the alternate screen. The backend viewer is the only alternate-screen surface.

## Behavior contract

TUI and dependency changes must preserve these behaviors:

- The main interface remains an inline viewport in the regular terminal buffer.
- Completed transcript output remains in native terminal scrollback.
- The settled banner, transcript, composer, status rows, expansion state, spacing, and styling change only through explicit TUI work, never merely because dependencies changed.
- The startup welcome stays in inline transcript scrollback and uses a compact four-zone layout: a Dext/version brand row, an adaptive working-directory and cached Git summary at 80 columns or wider, exactly two Model/Approval facts between rules, and one rotating tip drawn from verified TUI features. Width calculations and truncation use terminal cell width, and the Git probe runs off the render loop with only an 8 ms startup wait before falling back to path-only rendering.
- The empty composer prompt is `❯ Type a request…   @ files · / commands`; typing, login, permission, paste-preview, and work-map behavior retain their existing paths.
- The main status row keeps a live cumulative agent-active elapsed clock at its right edge while Dext works; it pauses and hides while Dext is idle awaiting input.
- Input and the viewport remain responsive while output streams and while the terminal is resized.
- Resize replay is cohesive: no item-by-item reconstruction, whole-screen flash, cursor-query stall, or cursor-query timeout.
- The backend viewer remains the only alternate-screen surface.
- `Ctrl+L` opens a read-only todo modal in the inline UI; it never enters the alternate screen and remains available during ordinary idle or busy work. Permission and local-auth prompts intentionally retain input and rendering priority.

A dependency update that violates this contract is rejected even if it compiles and unit tests pass.

## Todo view

Press `Ctrl+L` during ordinary idle or busy work to open the current session todo list. Security-critical permission and local-auth prompts intentionally take priority and must be resolved or canceled first. The modal loads the persisted session/project todo state at startup, refreshes after `todo_read` or `todo_write`, and supports arrow, Page Up/Down, Home/End, and mouse-wheel scrolling. Close it with `Ctrl+L`, `Esc`, or `q`.

The first version is intentionally read-only. Todo edits still use the existing `todo_write` path so validation, permission, checkpoint, and session-state behavior are not duplicated in the TUI. Empty-state parsing matches Dext's generated empty-list lines exactly, so ordinary todo text cannot clear the modal accidentally. When todo progress is the live-status fallback above the composer, Dext uses the compact `Todos 1/3 ■■□□□□□ · Active: …` form while preserving the active task when space allows. The modal is rendered inside the inline viewport; `Ctrl+B` and the backend viewer remain the only alternate-screen path.

## Status and backend viewer

The main status row reserves its right edge for a live cumulative agent-active clock while Dext is handling a turn. It advances during provider waits, tool calls, permission/auth waits, and in-turn compaction; while Dext is idle awaiting user input, the clock pauses and is hidden, then resumes on the next turn. It updates through the existing redraw cadence and uses compact `7s`, `7m 05s`, and `1h 07m` forms without adding a timer thread.

`Ctrl+B` opens the existing alternate-screen backend viewer for captured `bash` output. It uses the same event stream, bounded ring buffer, command selection, scrolling, and permission/auth priority as before. The viewer visually matches the main TUI with a Dext header, agent-active clock, command summary, styled stdout/stderr lanes, output panel, command position, and compact key footer. Close it with `Ctrl+B`, `Esc`, or `q`; switch captured commands with Tab/Shift+Tab and scroll with arrows, Page Up/Down, Home/End, or the mouse wheel.

## Dependency stack

The renderer dependencies are exact so unrelated lockfile refreshes cannot change terminal behavior:

- `ratatui = 0.30.2`
- `ratatui-core = 0.1.2`
- `tui-markdown = 0.3.8`
- `crossterm = 0.29.0`
- `unicode-width = 0.2.2`

## Ratatui compatibility patch

Unmodified Ratatui 0.30.2 regressed Dext's inline experience. Its fallback `insert_before` path called the public `Terminal::clear`, which synchronously queried the terminal cursor. Crossterm serves that query through the same global event reader used by Dext's input thread, multiplying terminal round trips during transcript insertion and resize. Horizontal shrink also cleared the entire visible display, exposing replay as a flash.

Enabling Ratatui's `scrolling-regions` feature was rejected because it changed settled rendering and expanded the backend dependency graph.

Dext patches the exact upstream `ratatui-core 0.1.2` source through `[patch.crates-io]`. The patch is limited to three inline-terminal corrections:

1. `Terminal::clear` preserves Ratatui's tracked cursor position instead of synchronously querying the backend.
2. Fallback `insert_before` clears the viewport directly rather than calling the cursor-preserving public clear.
3. Horizontal shrink avoids `ClearType::All` for inline viewports; the normal viewport clear and full next draw remain in place.

The vendored source and hunk-level rationale live under `vendor/ratatui-core/`. This is a narrow compatibility patch, not a renderer fork. Remove it when a released upstream version satisfies the same regression gate without changing settled behavior.

## Regression coverage

Run the complete renderer gate after any TUI or terminal dependency change:

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo test -p ratatui-core --lib --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

The PTY smoke suite exercises the real binary and requires:

- banner and composer visibility at narrow and wide sizes;
- editable input during live streaming;
- process survival and responsive input through a populated-history resize burst;
- zero whole-screen clears during inline resize;
- cursor queries bounded by resize events rather than transcript size;
- terminal-height-bounded replay chunks;
- completed stream output and accepted input after resize, with a bounded 10-second completion wait so slower macOS CI hosts do not create false negatives.

Before releasing a renderer/backend update, also perform a live WSL2 check because ConPTY latency and perceptual flicker cannot be fully modeled by the Linux PTY. Resize a populated streaming session repeatedly and reject any visible replay, flash, input stall, scrollback loss, or mode-switching change. Native Linux and tmux checks are also recommended when terminal behavior changes.

## Dependency maintenance

1. Change only the TUI dependency set and lockfile.
2. Run the unmodified stack against the focused PTY resize test.
3. If it fails, identify the smallest upstream boundary; do not compensate with a UI redesign.
4. Prefer a released upstream fix. Otherwise refresh the exact vendored crate and reapply only the still-required patch.
5. Run the complete renderer gate and live terminal checks.
6. Compare the vendored patch with upstream and document each remaining hunk in `vendor/ratatui-core/DEXT_PATCH.md`.
7. Remove obsolete patch hunks immediately when upstream behavior passes the gate.

Performance changes such as stream burst coalescing require measured CPU/output evidence and must preserve immediate first paint after idle. They are separate from dependency maintenance.
