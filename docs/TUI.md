# Terminal UI

Dext's interactive interface is an inline Ratatui application in the regular terminal buffer. It uses native terminal scrollback during ordinary operation instead of taking over the alternate screen. On every effective transcript-pane width change, Dext deliberately replaces that scrollback immediately with a complete replay at the new width. The backend viewer is the only alternate-screen surface.

## Behavior contract

TUI and dependency changes must preserve these behaviors:

- The main interface remains an inline viewport in the regular terminal buffer.
- Completed transcript output remains in native terminal scrollback during ordinary operation. Every effective transcript-pane width change immediately purges stale-width terminal history and rebuilds Dext's complete logical transcript; pre-Dext shell scrollback is intentionally not preserved by that rebuild.
- The settled banner, transcript, composer, status rows, expansion state, spacing, and styling change only through explicit TUI work, never merely because dependencies changed.
- The startup welcome stays in inline transcript scrollback, starts with one transcript-owned blank separator row below CLI diagnostics, and uses a compact four-zone layout: a Dext/version brand row, an adaptive working-directory and cached Git summary at 80 columns or wider, exactly two Model/Approval facts between rules, and one rotating tip drawn from verified TUI features. Width calculations and truncation use terminal cell width, and the Git probe runs off the render loop with only an 8 ms startup wait before falling back to path-only rendering.
- The empty composer prompt is `❯ Type a request…   @ files · / commands`; typing, login, permission, and paste-preview behavior retain their existing paths. Slash completion mirrors the canonical handled commands, including `/privacy`, `/preview`, `/context`, `/tool-profile`, `/diagnostics`, `/shelves`, `/project-extensions`, and `/undo`. `/login` completion shows every provider id exactly once and suppresses duplicate numbered-selector entries.
- Structured slash listings use the established `/sessions` hierarchy: count and section headers, two-space names, four-space details, and detached `Use:` footers. Dense name/description catalogs such as `/help` use aligned rows at 64 columns and wider and fall back to the stacked hierarchy when narrow. The TUI supplies its actual transcript-pane width; output keeps a two-cell gutter and a 120-column readability cap. `/system` preserves source/prompt paragraphs, blank lines, and leading indentation while wrapping prose. Dynamic fields are sanitized before layout; every physical row is bounded by Unicode display cells, with `?` replacing only a grapheme that cannot fit in an otherwise impossible one-cell measure. An explicit structured-slash event retains those layouts even when ANSI color is disabled. Generic slash confirmations, including `/model` and thinking-effort status, retain the faded info treatment.
- Frugal mode applies the stricter pseudo-tool-protocol sanitizer to partial-stream recovery, completed transcript/thinking blocks, live details, and the inspector: serialized or multiline tool-call-like assistant payloads are replaced with `[tool call redacted; waiting for structured tool event]` while surrounding prose remains visible. Standard mode retains the narrower legacy line detector.
- The main status row shows the exact `main` branch label as `Main`, including `Main (dirty)` when the working tree is dirty, without renaming the branch or changing any other branch casing. It keeps a live cumulative agent-active elapsed clock at its right edge while Dext works; the clock pauses and hides while Dext is idle awaiting input.
- Anthropic thinking deltas are retained in the provider event stream and finalized with their signatures for tool-loop replay. The TUI shows live and completed thinking only while verbose display is enabled (the default); toggling verbose hides it without changing stored provider blocks. `stream-json` exposes thinking events, while console text and final JSON omit thinking content.
- Input and the viewport remain responsive while output streams and while the terminal is resized.
- Resize replay follows a full-ownership model. On every effective transcript-pane width change, Dext uses one synchronized update to clear the visible display and reset the inline viewport to the origin without a cursor query, purge stale-width scrollback, and immediately rebuild the complete logical transcript at the observed width before appending pending output. Clearing before purging removes the still-visible old intro before logical history replays it once. There is no quiet-settle debounce, visible-suffix overwrite, or short-history exception. This removes mixed old/new wrapping, duplicate transcript copies, and width/height-shrink bookkeeping edge cases; the deliberate tradeoffs are complete replay work during resize bursts and replacement of pre-Dext shell scrollback.
- Pending permission prompts render inside the inline viewport, never into scrollback; only the compact decision line is appended once resolved. Approval prompts and decisions must not trigger a full-history re-emit.
- Pending transcript insertion keeps an already prepared failed batch separate from newly queued raw output. A retry reuses that prepared batch without regrouping or reranking it; new output is prepared only after the retry succeeds.
- The backend viewer remains the only alternate-screen surface.
- `Ctrl+L` opens a read-only todo modal in the inline UI; it never enters the alternate screen and remains available during ordinary idle or busy work. Permission and local-auth prompts intentionally retain input and rendering priority.

A dependency update that violates this contract is rejected even if it compiles and unit tests pass.

## Todo view

Press `Ctrl+L` during ordinary idle or busy work to open the current session todo list. Security-critical permission and local-auth prompts intentionally take priority and must be resolved or canceled first. The modal loads the persisted session/project todo state at startup, refreshes after `todo_read` or `todo_write`, and supports arrow, Page Up/Down, Home/End, and mouse-wheel scrolling. Close it with `Ctrl+L`, `Esc`, or `q`.

The first version is intentionally read-only. Todo edits still use the existing `todo_write` path so validation, permission, checkpoint, and session-state behavior are not duplicated in the TUI. Empty-state parsing matches Dext's generated empty-list lines exactly, so ordinary todo text cannot clear the modal accidentally. When todo progress is the live-status fallback above the composer, its battery follows the list length up to seven cells: `Todos 3/4 ■■■□` uses one cell per task, while longer lists such as `Todos 15/20 ■■■■■□□` stay capped and proportional. Partial progress always retains at least one filled and one empty cell, and the active task remains visible when space allows. The modal is rendered inside the inline viewport; `Ctrl+B` and the backend viewer remain the only alternate-screen path.

## Theme

Thinking and steering blocks use a contrast-aware palette. Set `DEXT_THEME=light` or `DEXT_THEME=dark` to override it. Without an override, Dext converts the terminal's `COLORFGBG` 16/256-color background index to luminance when available and otherwise keeps the dark palette.

## Status and backend viewer

The main status row reserves its right edge for a live cumulative agent-active clock while Dext is handling a turn. It advances during provider waits, tool calls, permission/auth waits, and in-turn compaction; while Dext is idle awaiting user input, the clock pauses and is hidden, then resumes on the next turn. It updates through the existing redraw cadence and uses compact `7s`, `7m 05s`, and `1h 07m` forms without adding a timer thread.

`Ctrl+B` opens the existing alternate-screen backend viewer for captured `bash` output. It uses the same event stream, bounded ring buffer, command selection, scrolling, and permission/auth priority as before. The viewer visually matches the main TUI with a Dext header, agent-active clock, command summary, styled stdout/stderr lanes, output panel, command position, and compact key footer. Close it with `Ctrl+B`, `Esc`, or `q`; switch captured commands with Tab/Shift+Tab and scroll with arrows, Page Up/Down, Home/End, or the mouse wheel.

## Dependency stack

The renderer dependencies are exact so unrelated lockfile refreshes cannot change terminal behavior. The lockfile also pins Ratatui's transitive `lru` cache to patched `0.18.2`:

- `ratatui = 0.30.2`
- `ratatui-core = 0.1.2`
- `tui-markdown = 0.3.8`
- `crossterm = 0.29.0`
- `unicode-width = 0.2.2`

## Ratatui compatibility patch

Unmodified Ratatui 0.30.2 regressed Dext's inline experience. Its fallback `insert_before` path called the public `Terminal::clear`, which synchronously queried the terminal cursor. Crossterm serves that query through the same global event reader used by Dext's input thread, multiplying terminal round trips during transcript insertion and resize. Horizontal shrink also cleared the entire visible display, exposing replay as a flash.

Enabling Ratatui's `scrolling-regions` feature was rejected because it changed settled rendering and expanded the backend dependency graph.

Dext patches the exact upstream `ratatui-core 0.1.2` source through `[patch.crates-io]`. The patch is limited to four inline-terminal corrections:

1. `Terminal::clear` preserves Ratatui's tracked cursor position instead of synchronously querying the backend.
2. Fallback `insert_before` clears the viewport directly rather than calling the cursor-preserving public clear.
3. Horizontal shrink avoids `ClearType::All` for inline viewports; the normal viewport clear and full next draw remain in place.
4. `Terminal::reset_inline_viewport` clears the visible display, resets both diff buffers, and anchors an inline viewport at the terminal origin without querying the cursor. On every effective transcript-pane width change, Dext calls it before Crossterm purges stale-width scrollback, then replays its complete logical transcript once at the observed width.

The vendored source and hunk-level rationale live under `vendor/ratatui-core/`. This is a narrow compatibility patch, not a renderer fork. Remove it when a released upstream version satisfies the same regression gate without changing settled behavior.

## Regression coverage

Run the complete renderer gate after any TUI or terminal dependency change:

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo deny check licenses
cargo test -p ratatui-core --lib --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

The Unix PTY smoke suite starts each Dext child in a fresh session with the slave PTY as its controlling terminal and applies resize geometry through that slave endpoint, matching real terminal resize delivery on macOS and Linux. Resize assertions wait for the replay marker with a bounded deadline rather than assuming a fixed scheduler delay on shared CI hosts. It exercises the real binary and requires:

- banner and composer visibility at narrow and wide sizes;
- editable input during live streaming;
- process survival and responsive input through a populated-history resize burst;
- one visible-display clear before one scrollback purge for every effective populated-transcript width change, followed immediately by a complete logical-transcript replay at the observed width with exactly one Dext intro;
- repeated frames at the same width do not rebuild, while simultaneous width/height shrink still reconstructs the complete transcript from the origin;
- cursor queries bounded by resize events rather than transcript size;
- replay chunks bounded by terminal height, with pending output appended only after reconstruction;

Windows CI and release workflows also run `tests/tui_smoke_windows.rs`, a native ConPTY real-binary smoke test that submits `/status`, verifies the default `approval=always` and `sandbox=danger-full-access` policy, exits through `/quit`, and requires clean termination. The harness forces null std handles so the child binds pseudoconsole stdio even under redirected test capture, and companion self-check tests validate the pseudoconsole plumbing itself with `cmd.exe` and a non-interactive `dext --version` run. This addresses the previous Windows interactive-test gap without adding a runtime dependency.

Before releasing a renderer/backend update, also perform a live WSL2 check because ConPTY latency and perceptual flicker cannot be fully modeled by automated smoke tests. Resize a populated streaming session repeatedly and reject any crash, input stall, mixed-width or duplicate history, unexpected scrollback loss outside the documented full-ownership rebuild, or mode-switching change. Full replay during each observed width change and loss of pre-Dext shell scrollback are documented tradeoffs, not regressions. Native Linux and tmux checks are also recommended when terminal behavior changes.

## Dependency maintenance

1. Change only the TUI dependency set and lockfile.
2. Run the unmodified stack against the focused PTY resize test.
3. If it fails, identify the smallest upstream boundary; do not compensate with a UI redesign.
4. Prefer a released upstream fix. Otherwise refresh the exact vendored crate and reapply only the still-required patch.
5. Run the complete renderer gate and live terminal checks.
6. Compare the vendored patch with upstream and document each remaining hunk in `vendor/ratatui-core/DEXT_PATCH.md`.
7. Remove obsolete patch hunks immediately when upstream behavior passes the gate.

Performance changes such as stream burst coalescing require measured CPU/output evidence and must preserve immediate first paint after idle. They are separate from dependency maintenance.
