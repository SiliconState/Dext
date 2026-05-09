# Architecture

Dext is a Rust terminal agent packaged as one binary. Most behavior is still intentionally local and inspectable: provider requests are built in process, tools are native functions/short-lived child processes, and session state is written as JSONL/log files under Dext state paths.

## Runtime flow

1. CLI options and environment are parsed.
2. Provider catalog/auth state is resolved.
3. A project-scoped agent is created with sandbox, approval, context, and tool profile settings.
4. Interactive TUI, plain REPL, one-shot, eval, or session subcommand mode is selected.
5. Each model turn streams assistant text/thinking/tool calls through an `EventSink`.
6. Tool calls are validated, permission-checked, executed, summarized, and appended to history.
7. Session checkpoints, logs, provider health, work ledger, and verification records are persisted.

## Core modules

- `src/main.rs`
  - Agent state and loop.
  - CLI parsing and top-level command dispatch.
  - Slash commands.
  - Built-in tool implementations.
  - Eval harness.
  - Prompt/context assembly, compaction, and event emission.

- `src/provider.rs`
  - Provider profile/catalog loading and normalization.
  - Built-in GLM and ChatGPT/Codex profiles.
  - API-key and OAuth login flows.
  - Request builders for Anthropic, OpenAI-compatible, and ChatGPT/Codex response APIs.
  - Model alias normalization and provider/model switching helpers.

- `src/tools.rs`
  - Tool catalog.
  - Permission-required and parallel-safe metadata.
  - Lean/full provider tool-schema rendering.

- `src/tool_policy.rs`
  - Tool input validation.
  - Command risk classification.
  - URL/host extraction and external-attempt guard helpers.

- `src/orchestrator.rs`
  - Turn phase tracking.
  - External-result dedupe.
  - Similarity/circuit-breaker/partial-delivery guardrails.
  - Turn telemetry.

- `src/session.rs`
  - `DEXT_HOME`, session, log, and project-state paths.
  - Project-scoped locks and stale-lock cleanup.
  - Atomic state writes.
  - Terminal restore helpers used by crash/panic paths.

- `src/tui.rs`
  - Inline ratatui UI in the regular terminal buffer.
  - Transcript rendering, input box, status/live areas, permission prompts, and slash completions.

## Tool model

Dext exposes a deliberately small native tool set:

- Filesystem: `read_file`, `read_symbol`, `write_file`, `edit_file`, `multi_edit`.
- Search: `fd`, `rg`, `fzf`.
- Shell/data: `bash`, `jq`, `awk`, `csvkit`.
- Network: `http`.
- Git: `git_diff`, `git_log`, `git_commit`.
- Tasks: `todo_read`, `todo_write`.
- Optional browser recipe: `browser` only when enabled.

Lean schemas are the default to reduce prompt cost. Full schemas are available with `--tool-profile full` or `/tool-profile full`.

## Sessions and state

Dext distinguishes project files from runtime state:

- Project files live in the Git repository.
- State defaults to `~/.dext` or `DEXT_HOME`.
- Project latest sessions/logs are scoped by a stable project key.
- Named sessions can be stored under `DEXT_SESSIONS_DIR`.
- Session exports (`dext-session-*.jsonl`, `dext-session-*.html`) are ignored by Git.

Session JSONL files include header provenance, exposed tools, approval/sandbox context, provider health, ledger entries, and message history. They may contain sensitive prompts or tool output; do not publish them blindly.

## Safety model

Dext has three safety layers:

1. Tool validation rejects malformed calls before execution.
2. Permission profiles decide which privileged tools require approval.
3. Sandbox profiles constrain filesystem/process behavior at the agent-policy level.

Profiles:

- Approval: `ask`, `auto-read`, `auto-write`, `never`, `always`.
- Sandbox: `read-only`, `workspace-write`, `danger-full-access`.

`--trust` enables trust mode and auto-approves gated tools.

## Context modes

- `standard`: normal caps and lean tool schemas by default.
- `frugal`: lower prompt/history/tool caps, lean schemas, and deterministic compaction choices intended to reduce token spend.

Compaction preserves recent tool evidence and summarizes older conversation when history approaches the model-aware context budget.

## Verification surface

Expected checks before releasing Dext changes:

```bash
cargo fmt
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
```

The TUI smoke test launches the real compiled binary inside a pseudo-terminal and verifies banner/help/exit behavior without requiring a human terminal session.
