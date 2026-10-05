# Terminal UI

Dext's interactive interface is an inline Ratatui application in the regular terminal buffer. It uses native terminal scrollback during ordinary operation instead of taking over the alternate screen. On every effective transcript-pane width change, Dext deliberately replaces that scrollback immediately with a complete replay at the new width. The backend viewer is the only alternate-screen surface.

## Behavior contract

TUI and dependency changes must preserve these behaviors:

- The main interface remains an inline viewport in the regular terminal buffer.
- Completed transcript output remains in native terminal scrollback during ordinary operation. Every effective transcript-pane width change immediately purges stale-width terminal history and rebuilds Dext's complete logical transcript; pre-Dext shell scrollback is intentionally not preserved by that rebuild.
- The settled banner, transcript, composer, status rows, expansion state, spacing, and styling change only through explicit TUI work, never merely because dependencies changed.
- The startup welcome stays in inline transcript scrollback, starts with one transcript-owned blank separator row below CLI diagnostics, and uses a compact four-zone layout: a Dext/version brand row, an adaptive working-directory and cached Git summary at 80 columns or wider, exactly two Model/Approval facts between rules, and one rotating tip drawn from verified TUI features. Width calculations and truncation use terminal cell width, and the Git probe runs off the render loop with only an 8 ms startup wait before falling back to path-only rendering.
- The empty composer prompt is `❯ Type a request…   @ files · / commands`; typing, login, permission, and paste-preview behavior retain their existing paths. Slash completion mirrors the canonical handled commands, including `/privacy`, `/preview`, `/context`, `/tool-profile`, `/diagnostics`, `/shelves`, `/project-extensions`, and `/undo`. `/login` completion shows every provider id exactly once and suppresses duplicate numbered-selector entries.
- Structured slash listings use the established `/sessions` hierarchy: count and section headers, two-space names, four-space details, and detached `Use:` footers. `/sessions` includes a human-readable UTC `started` value decoded from the timestamp already embedded in the persisted session ID or ID-shaped path; legacy sessions without that data show `unknown`. Dense name/description catalogs such as `/help` use aligned rows at 64 columns and wider and fall back to the stacked hierarchy when narrow. The TUI supplies its actual transcript-pane width; output keeps a two-cell gutter and a 120-column readability cap. `/system` preserves source/prompt paragraphs, blank lines, and leading indentation while wrapping prose. Dynamic fields are sanitized before layout; every physical row is bounded by Unicode display cells, with `?` replacing only a grapheme that cannot fit in an otherwise impossible one-cell measure. An explicit structured-slash event retains those layouts even when ANSI color is disabled. Generic slash confirmations, including `/model` and thinking-effort status, retain the faded info treatment.
- Frugal mode applies the stricter pseudo-tool-protocol sanitizer to partial-stream recovery, completed transcript/thinking blocks, live details, and the inspector: serialized or multiline tool-call-like assistant payloads are replaced with `[tool call redacted; waiting for structured tool event]` while surrounding prose, including text after an inline XML tool-call close, remains visible. Incremental thinking keeps the sanitizer's payload/XML state across sealed logical lines and uses a cloned state for the open-line preview, so a payload split across provider deltas or scrollback insertions cannot reappear. Overlong XML lines retain bounded tag-boundary state so a trusted trailing close can restore following prose without retaining the omitted payload; escape-bearing or ambiguous truncated lines remain conservatively hidden until a later trusted boundary. Sanitized-empty lines reset bullet sections when they end ordinary payload redaction, but not within an open XML span. Standard mode retains the narrower legacy line detector.
- The main status row shows the exact `main` branch label as `Main`, including `Main (dirty)` when the working tree is dirty, without renaming the branch or changing any other branch casing. It keeps a live cumulative agent-active elapsed clock at its right edge while Dext works; the clock pauses and hides while Dext is idle awaiting input.
- Anthropic thinking deltas and local llama.cpp `reasoning_content` are retained in the provider event stream; Anthropic blocks retain signatures, while local reasoning is replayed only within the current llama.cpp tool loop. In the default verbose display, each newline-terminated provider thinking line is sealed promptly into native scrollback as a reflowable logical unit; blank lines start a new bullet section, and the unterminated line remains at the transcript edge in a bounded multirow live tip rendered by the same code. Completed units remain rollback-eligible until the provider stream validates successfully: an empty completion, stream restart, interrupt, or active verbose hide removes that block and rebuilds scrollback when necessary; a mismatched nonempty completion replaces it and increments the inspector's mismatch counter. Turn boundaries commit instead: a failed turn keeps every sealed thinking line in scrollback and drops only the never-sealed open tail, so no later retry can purge an earlier turn's history. `Ctrl+V` hides or reconstructs only the active provisional block, while completed blocks stay in scrollback and stored provider blocks are unchanged. Provider newlines are therefore hard display boundaries. Individual newline-free lines over 8 KiB and blocks over 4,096 display units use explicit omission markers, including in the open preview; whitespace-only tails never produce a live omission. Displayable provider reasoning text is capped at 4 MiB. Line decoding processes completed lines individually rather than allocating a batch proportional to newline count, and the inspector renders backward only far enough to fill its four-row thinking tail. Thinking rows never exceed the available terminal-cell width, dropping the two-cell bullet gutter only at widths too narrow to contain it. `stream-json` exposes the existing thinking events, while console text and final JSON omit thinking content.
- Input and the viewport remain responsive while output streams and while the terminal is resized.
- Resize replay follows a full-ownership model. On every effective transcript-pane width change, Dext uses one synchronized update to clear the visible display and reset the inline viewport to the origin without a cursor query, purge stale-width scrollback, and immediately rebuild the complete logical transcript at the observed width before appending pending output. Clearing before purging removes the still-visible old intro before logical history replays it once. There is no quiet-settle debounce, visible-suffix overwrite, or short-history exception. This removes mixed old/new wrapping, duplicate transcript copies, and width/height-shrink bookkeeping edge cases; the deliberate tradeoffs are complete replay work during resize bursts and replacement of pre-Dext shell scrollback.
- Pending permission prompts render inside the inline viewport, never into scrollback; only the compact decision line is appended once resolved. Approval prompts and decisions must not trigger a full-history re-emit.
- Pending transcript insertion keeps an already prepared failed batch separate from newly queued raw output. A retry reuses that prepared batch without regrouping or reranking it; new output, including sealed thinking units, is prepared only after the retry succeeds. Thinking seal/rollback transitions group transcript insertion or purge/replay with the updated live viewport in one synchronized terminal update.
- The backend viewer remains the only alternate-screen surface.
- `Ctrl+L` opens a read-only todo modal in the inline UI; it never enters the alternate screen and remains available during ordinary idle or busy work. Permission and local-auth prompts intentionally retain input and rendering priority.

A dependency update that violates this contract is rejected even if it compiles and unit tests pass.

## Help and saved-session picker

Press `?` with an empty composer, or F1 even with a draft, to open the full keymap in the inline viewport. The keymap scrolls with Up/Down, Page Up/Down, Home/End, and the mouse wheel; `?`, F1, or Esc closes it. Narrow widths stack shortcut labels and descriptions instead of clipping them. While open, typed keys and pastes cannot edit or submit the composer. Ctrl+C/Ctrl+D retain their interrupt/quit behavior. Pending permission and local-auth prompts take precedence.

Enter `/resume` while idle to browse the available latest, autosaved, and named sessions. Each selectable row shows its category/name, human-readable UTC start date/time, and the useful tail of its path. The start time is decoded from the timestamp already embedded in the persisted session ID (or ID-shaped session path); Dext does not create another clock value, and legacy sessions without that data show `unknown`. The picker excludes unreadable headers and, when a Seat is active, sessions with a different Seat, a seated session without sandbox provenance, or a saved sandbox in another project. Selectable rows show category, name, and a path with its useful tail retained; arrow/Page/Home/End keys navigate, Enter loads the selected file through the existing session loader, and Esc cancels without modifying the draft. Selection is by path rather than an ambiguous short name; the loader revalidates the chosen file before restoring. A lookup runs off the render loop; cancelled or superseded results are ignored. On successful load the TUI refreshes the effective workspace, Git status, todo list, model, reasoning mode, usage, and context; Git probes started before the load cannot replace the restored Git status, even when the session stays in the same workspace. Explicit `/resume NAME` and `/resume PATH` retain their existing CLI/TUI behavior; the popup does not affect them. A missing/invalid session produces an error and returns to ready without loading it; while a selected session is being restored, the composer waits instead of submitting another request against partially restored state. Permission and local-auth prompts retain priority, including paste into the masked local-auth prompt, and the backend viewer remains the only alternate-screen surface.

## Project path picker

Press `Ctrl+P` while Dext owns the terminal to open the inline, read-only project path picker. Type to fuzzy-filter names and relative paths; arrows, Page Up/Down, Home/End and the mouse wheel move the selection. Right enters a selected directory; Left returns toward the project root. Enter inserts the selected file or directory path at the draft cursor, without submitting the draft; Esc cancels without editing it. Spaces and backticks are wrapped in Markdown code delimiters. Paths are plain prompt text, not file attachments or shell-quoted arguments. Tab retains slash completion/reasoning controls outside the picker.

The picker searches from the current sandbox root, scanning at most 20,000 entries and 16 directory levels in a background task; the header shows how many matches are displayed out of the matching scanned entries, with at most 100 ranked results shown and an explicit scan-limit indicator. Descending into a directory starts a fresh scan there. Hidden and generated directories are shown but not descended into automatically; symlinks are neither traversed nor selected, including a directory replaced by a symlink after the scan. Unreadable directories are skipped. Scan work is canceled on navigation, dismissal, or root change, and late results are ignored. On Windows the visible/queryable path uses `/` even though filesystem paths use `\`. `/sandbox` and resumed sessions update the root. Permission, local-auth, and login-input prompts close the picker and retain priority; paste while the picker is open is ignored, not inserted into the draft. The backend viewer retains its alternate-screen input ownership.

## Todo view

Press `Ctrl+L` during ordinary idle or busy work to open the current session todo list. Security-critical permission and local-auth prompts intentionally take priority and must be resolved or canceled first. The modal loads the persisted session/project todo state at startup, refreshes after `todo_read` or `todo_write`, and supports arrow, Page Up/Down, Home/End, and mouse-wheel scrolling. Close it with `Ctrl+L`, `Esc`, or `q`.

The first version is intentionally read-only. Todo edits still use the existing `todo_write` path so validation, permission, checkpoint, and session-state behavior are not duplicated in the TUI. Empty-state parsing matches Dext's generated empty-list lines exactly, so ordinary todo text cannot clear the modal accidentally. When todo progress is the live-status fallback above the composer, its battery follows the list length up to seven cells: `Todos 3/4 ■■■□` uses one cell per task, while longer lists such as `Todos 15/20 ■■■■■□□` stay capped and proportional. Partial progress always retains at least one filled and one empty cell, and the active task remains visible when space allows. The modal is rendered inside the inline viewport; `Ctrl+B` and the backend viewer remain the only alternate-screen path.

## Theme

Thinking and steering blocks use a contrast-aware palette. Set `DEXT_THEME=light` or `DEXT_THEME=dark` to override it. Without an override, Dext converts the terminal's `COLORFGBG` 16/256-color background index to luminance when available and otherwise keeps the dark palette.

## Status and backend viewer

The main status row reserves its right edge for a live cumulative agent-active clock while Dext is handling a turn. It advances during provider waits, tool calls, permission/auth waits, and in-turn compaction; while Dext is idle awaiting user input, the clock pauses and is hidden, then resumes on the next turn. It updates through the existing redraw cadence and uses compact `7s`, `7m 05s`, and `1h 07m` forms without adding a timer thread.

`Ctrl+B` opens the existing alternate-screen backend viewer for captured `bash` output. It uses the same event stream, bounded ring buffer, command selection, scrolling, and permission/auth priority as before. The viewer visually matches the main TUI with a Dext header, agent-active clock, command summary, styled stdout/stderr lanes, output panel, command position, and compact key footer. Close it with `Ctrl+B`, `Esc`, or `q`; switch captured commands with Tab/Shift+Tab and scroll with arrows, Page Up/Down, Home/End, or the mouse wheel.

Background compaction defaults on; `/compact background off|on|status` controls its saved per-session preference between foreground turns. Turning it off cancels/reaps and accounts any idle job without applying it, and regular compaction remains available. `--background-compact=on|off` overrides restored settings; `DEXT_BACKGROUND_COMPACT` sets the new/legacy session default only. Failed setting saves report an error without success and keep disabled/newly enabled speculation safely off. The setting event clears a disabled badge without marking a turn busy. Background-only summarization displays a separate ready/background status without making the agent busy, disabling input, or advancing the active-turn clock. The existing agent owner services summary completion while idle; installed summaries enter the transcript once. The owner checkpoints cumulative usage after idle completion, including failed/discarded summaries, and again after shutdown settlement so those charges survive resume without another prompt. A real headroom wait displays “waiting for compaction” but remains interruptible. Esc also cancels an idle background job. Terminal states clear its badge; stale ready/apply events cannot revive a cancelled job, and resume/root changes clear transient state. Legacy blocking compaction retains its existing busy behavior.

## Dependency stack

The renderer dependencies are exact so unrelated lockfile refreshes cannot change terminal behavior. The lockfile also pins Ratatui's transitive `lru` cache to patched `0.18.2`:

- `ratatui = 0.30.2`
- `ratatui-core` and `ratatui-crossterm` from unmodified upstream revision `7767679c138b383933fef4227e7fbf077b7cfeca` (both report `0.1.2`)
- `tui-markdown = 0.3.8`
- `crossterm = 0.29.0`
- `unicode-width = 0.2.2`

## Unmodified upstream integration

Dext carries no vendored Ratatui source or local dependency patch. Cargo's `[patch.crates-io]` section selects unmodified upstream core and Crossterm backend crates at one immutable Git revision until a published release contains the required fixes. The override also unifies transitive users such as `tui-markdown`; it is a source selection, not a fork. Source builds require access to that revision or a populated Cargo cache.

Upstream [#2694](https://github.com/ratatui/ratatui/pull/2694) preserves the real cursor using terminal save/restore rather than an input-reading cursor query on Crossterm. Both core and backend must include that change. Upstream [#2670](https://github.com/ratatui/ratatui/pull/2670) preserves output above inline viewports on horizontal shrink. Dext leaves upstream `clear` and `insert_before` unchanged. The `scrolling-regions` feature remains disabled.

Transcript purge/replay remains Dext-owned behavior in `src/tui.rs`. `ReplayBackend` delegates ordinary backend operations, including cursor save/restore. Only during an explicit reset after clearing the display does it supply the known cursor origin and suppress reservation lines and the redundant viewport clear. Public `Terminal::resize` reanchors the inline viewport; two buffer swaps reset both diff buffers. The reset flag is cleared even when resize returns an error. Production then purges scrollback and replays the logical transcript inside a synchronized update. Geometry checks defer replay when minimized or when the expected pane width has changed, before attempting autoresize. Once validated, clear/purge/replay uses one scoped geometry snapshot so another OS resize cannot defer history reconstruction after the display has been erased. The snapshot is released on success or error; the next frame observes the live size and rebuilds again when needed.

The integration gate is `cargo test --release --locked --bin dext tui::tests::`, replacing the former workspace-only vendored-core test command. The real-PTY suite remains mandatory for terminal changes. Neither a passing upstream unit suite nor removal of vendoring alone proves terminal compatibility.

## Regression coverage

Run the complete renderer gate after any TUI or terminal dependency change:

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo deny check licenses
cargo test --release --locked --bin dext tui::tests::
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

The TUI regression coverage combines the real Unix PTY smoke suite with focused state/render unit tests. The PTY starts each Dext child in a fresh session with the slave PTY as its controlling terminal and applies resize geometry through that slave endpoint, matching real terminal resize delivery on macOS and Linux. Resize assertions wait for the replay marker with a bounded deadline rather than assuming a fixed scheduler delay on shared CI hosts. The PTY coverage requires:

- banner and composer visibility at narrow and wide sizes;
- editable input during live streaming;
- process survival and responsive input through a populated-history resize burst;
- one visible-display clear before one scrollback purge for every effective populated-transcript width change, followed immediately by a complete logical-transcript replay at the observed width with exactly one Dext intro;
- repeated frames at the same width do not rebuild, while simultaneous width/height shrink still reconstructs the complete transcript from the origin;
- cursor queries bounded by resize events rather than transcript size;
- replay chunks bounded by terminal height, with pending output appended only after reconstruction;

Focused state/render tests additionally require:

- reset failures at every cursor-move stage restore backend delegation, including the final cursor restore;
- zero-sized, status-only, and stale-width geometry defers before autoresize can query the cursor, append reservation lines, clear the viewport, or change its area, and rendering resumes after geometry recovery;
- provisional thinking lines seal in order without duplication, use one bullet per blank-line-delimited section, keep the open tail in the live tip, and render the same rows/styles before and after sealing;
- empty completion, stream restart, interrupt, and active verbose-hide rollback remove tagged pending/retry/transcript units and force a full replay when real scrollback may already contain them;
- withheld completion tails append once, mismatched completions replace streamed units and surface the inspector counter, and split CRLF/whitespace boundaries do not seal early;
- frugal pseudo-tool payloads stay redacted across arbitrary delta and seal boundaries, same-line XML suffix prose remains visible, and overlong lines and excessive line counts use bounded explicit display omissions;
- thinking rows stay within the available terminal-cell width, including widths too narrow for the ordinary two-cell bullet gutter;
- cap-boundary open previews agree with sealed units, newline-flood decoding avoids line-batch allocation, and bounded inspector-tail rendering matches the full-render suffix;
- backend scrollback replay after a thinking discard removes stale units without duplicating surviving prose;

Windows CI and release workflows also run `tests/tui_smoke_windows.rs`, a native ConPTY real-binary smoke test that submits `/status`, verifies the default `approval=always` and `sandbox=danger-full-access` policy, exits through `/quit`, and requires clean termination. The path-picker ConPTY fixture separates Escape cancellation from Ctrl+D with a 150 ms input gap, matching the Unix fixture, so adjacent bytes are not interpreted as an Alt-modified control key. The harness forces null std handles so the child binds pseudoconsole stdio even under redirected test capture, and companion self-check tests validate the pseudoconsole plumbing itself with `cmd.exe` and a non-interactive `dext --version` run. This addresses the previous Windows interactive-test gap without adding a runtime dependency.

Before releasing a renderer/backend update, also perform a live WSL2 check because ConPTY latency and perceptual flicker cannot be fully modeled by automated smoke tests. Resize a populated streaming session repeatedly and reject any crash, input stall, mixed-width or duplicate history, unexpected scrollback loss outside the documented full-ownership rebuild, or mode-switching change. Full replay during each observed width change and loss of pre-Dext shell scrollback are documented tradeoffs, not regressions. Native Linux and tmux checks are also recommended when terminal behavior changes.

## Dependency maintenance

1. Review the upstream diff and update only the terminal dependency set and lockfile; never follow a floating Git branch.
2. Keep core and backend cursor capabilities aligned and verify there is one resolved core with `cargo tree -i ratatui-core`.
3. Run Dext's focused TUI tests and real-PTY resize test without modifying dependency source.
4. If behavior regresses, retain the last verified pin and report the smallest upstream issue; do not compensate with a UI redesign.
5. Run the complete renderer gate and live terminal checks.
6. When a published release includes the required fixes and passes those gates, replace the Git source overrides with exact registry versions.
7. Keep Dext-specific transcript ownership and replay tests in Dext rather than adding APIs to a dependency fork.

Performance changes such as stream burst coalescing require measured CPU/output evidence and must preserve immediate first paint after idle. They are separate from dependency maintenance.
