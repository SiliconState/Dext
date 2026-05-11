# Dext project context

Dext is a single-binary Rust coding agent. This file is auto-injected into the
system prompt from the sandbox root, so keep it short and high-signal.

## Architecture
- `src/main.rs` — agent loop, provider HTTP, permissions/sandboxing, slash
  commands, CLI entry, eval, and remaining orchestration.
- `src/session.rs` — session/log persistence, project state locks, state paths,
  and TUI terminal restore helpers.
- `src/tools.rs` — tool catalog, permission/parallel metadata, and lean/full
  provider tool schemas.
- `src/provider.rs` — provider catalog/auth, request shaping, and normalization.
- `src/tui.rs` — ratatui inline TUI using the regular terminal buffer.
- `benches/dext_bench.rs` — criterion perf harness.

## Self-modification
When changing Dext itself:
1. Locate code first with rg/read_symbol/read_file; do not guess.
2. Keep edits surgical and focused. Prefer existing files over new files.
3. Verify before declaring done:
   - `cargo build --release`
   - `cargo test --release`
   - If `src/tui.rs` changed: `cargo test --release --test tui_smoke -- --nocapture`
4. Reinstall the interactive binary after code changes: `cargo install --path . --force`.
   Never skip this; `target/release/dext` is not what `dext` on PATH invokes.
5. On Windows, failed install with “Access is denied” usually means another
   `dext.exe` is running. Ask the user to close it; do not kill processes.

## Project style
- No explanatory comments for obvious behavior; names should carry intent.
- Add comments only for non-obvious why/invariants/platform footguns.
- No backwards-compat shims for unreleased code.
- Keep runtime clutter (`.dext/`, `.pi/`, scratch logs, one-off docs,
  screenshots) ignored or deleted, not committed.

## Context and memory
- `recall.md` is a compact prompt-facing recall cache; durable long-form
  memory lives in `MEMORY.md` / pi-memory (`--project dext`).
- Log durable decisions/findings to pi-memory, sync `MEMORY.md`, then keep only
  the distilled prompt-worthy cache in `recall.md`.
- Prefer context engineering and memory quality over adding tools. Minimize tool
  duplication and prompt-injected historical detail.

## Provider notes
- Built-in providers include GLM and ChatGPT/Codex. Custom provider profiles can
  be configured through the provider catalog.
- Auth/model/provider commands exist in CLI and slash-command form
  (`providers`, `provider`, `models`, `login`, `logout`).
- Env overrides remain available for provider/model/base URL/API keys.

## Platform notes
- Windows Git Bash: TUI detection uses stdout terminal status; stdin is read
  only with `-p`.
- Windows file locks can block build/install while `dext.exe` is running.
