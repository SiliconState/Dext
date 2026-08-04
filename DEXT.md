# Dext project context

Dext is a single-binary Rust coding agent. This file is auto-injected into the
system prompt from the sandbox root, so keep it short and high-signal.

## Shell process lifecycle

The `bash` tool is deliberately atomic. Dext launches bash in a separate process group with `kill_on_drop`; after normal exit, timeout, or interrupt it terminates the tool process group. This keeps hidden shell state and orphaned background jobs out of the agent lifecycle.

`nohup`, `disown`, and `cmd &` are therefore not supported persistence mechanisms for agent-started servers. `setsid`-style detaches are also unsupported because they escape Dext cleanup. When a user explicitly needs a long-lived local service, prefer the host OS supervisor instead of adding Dext daemon state or a provider-visible daemon tool. On Linux with systemd, use `systemd-run --user --unit=dext-<name> --same-dir <cmd>`, inspect with `systemctl --user status dext-<name>`/`journalctl --user-unit dext-<name>`, and stop it with `systemctl --user stop dext-<name>` when done. Prefix agent-started units with `dext-` so cleanup is discoverable.

## Architecture
- `src/main.rs` — agent loop, provider HTTP, permissions/sandboxing, slash
  commands, CLI entry, eval, and remaining orchestration.
- `src/seats.rs` — project-scoped durable agent identity records and Seat-specific session lookup.
- `src/session.rs` — session/log persistence, project state locks, state paths,
  and TUI terminal restore helpers.
- `src/tools.rs` — tool catalog, permission/parallel metadata, and lean/full
  provider tool schemas.
- `src/provider.rs` — provider catalog/auth, request shaping, normalization, and transport deadlines.
- `src/sse.rs`, `src/streaming.rs`, `src/tool_round.rs`, `src/tool_journal.rs` — bounded SSE framing, provider event assembly, tool-round execution, and durable side-effect fencing.
- `src/git_checkpoints.rs`, `src/mutation_preview.rs` — Git-native recovery
  refs and file mutation previews.
- `src/sandbox.rs`, `src/tool_policy.rs`, `src/orchestrator.rs` — OS
  confinement, tool risk/validation policy, and runtime work-state controls.
- `src/packs.rs`, `src/shelves.rs` — shelf-contained pack creation, discovery,
  invocation, and typed ability metadata.
- `src/tui.rs` — Ratatui inline TUI using the regular terminal buffer.
- `vendor/ratatui-core/` — exact upstream source plus Dext's narrow inline-terminal compatibility patch.
- `benches/dext_bench.rs` — criterion perf harness.

## Self-modification
When changing Dext itself:
1. Locate code first with rg/read_symbol/read_file; do not guess.
2. Keep edits surgical and focused. Prefer existing files over new files.
3. Verify before declaring done:
   - `cargo fmt --all -- --check`
   - `cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings`
   - `cargo audit --deny warnings`
   - `cargo deny check licenses`
   - `cargo test -p ratatui-core --lib --locked`
   - `cargo build --release --locked`
   - `cargo test --release --locked`
   - If `src/tui.rs` or terminal dependencies changed: `cargo test --release --locked --test tui_smoke -- --nocapture`
4. Reinstall the interactive binary after code changes: `cargo install --path . --force --locked`.
   Never skip this; `target/release/dext` is not what `dext` on PATH invokes.
5. On Windows, failed install with “Access is denied” usually means another
   `dext.exe` is running. Ask the user to close it; do not kill processes.

## Project style
- No explanatory comments for obvious behavior; names should carry intent.
- Add comments only for non-obvious why/invariants/platform footguns.
- No backwards-compat shims for unreleased code.
- Keep runtime clutter (`.dext/`, scratch logs, one-off docs,
  screenshots) ignored or deleted, not committed.

## Documentation and risk register
- `docs/index.html` is the canonical main technical documentation. Update it in the same change as every user-visible, runtime, architecture, security, provider, tool, test, CI, or release behavior change; update focused Markdown docs alongside it when their subject changes.
- Do not log documentation drift as an operational risk. Prevent it through the same-change rule above.
- `docs/RISK_REGISTER.md` tracks open non-documentation risks. Update entries when controls, evidence, ownership, likelihood, impact, or status changes.

## Context files
- `DEXT.md` is tracked project guidance and is auto-injected from the sandbox
  root and its ancestors. Keep it terse and machine-facing.
- `recall.md` is an optional ignored prompt cache. It is auto-injected when
  present, but Dext does not create or update it automatically.
- Do not create or update `recall.md` unless the user asks.

## Packs and shelves
- Packs extend Dext without bloating the core or provider-visible toolset.
- Every pack lives at `<shelf>/packs/<name>` under `.dext/shelves`,
  `~/.dext/shelves`, or a `DEXT_SHELVES_DIR` root.
- Use `dext pack create <shelf>/<name>` for reusable user packs and add
  `--project` only for explicitly project-local packs.
- Dext ships no pack content; users own, review, maintain, and distribute their
  shelves separately.

## Provider notes
- Built-in providers include GLM, ChatGPT/Codex, OpenAI, Anthropic, Kimi Code, DeepSeek, and local OpenAI-compatible. Custom provider profiles can
  be configured through the provider catalog.
- Auth/model/provider commands exist in CLI and slash-command form
  (`providers`, `provider`, `models`, `login`, `logout`).
- Env overrides remain available for provider/model/base URL/API keys.

## Platform notes
- Windows Git Bash: TUI detection uses stdout terminal status; stdin is read
  only with `-p`.
- Windows file locks can block build/install while `dext.exe` is running.
