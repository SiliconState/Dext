# Architecture

Dext is a Rust terminal agent packaged as one binary. Most behavior is still intentionally local and inspectable: provider requests are built in process, tools are native functions/short-lived child processes, and session state is written as JSONL/log files under Dext state paths.

## Runtime flow

1. CLI options and environment are parsed.
2. Provider catalog/auth state is resolved.
3. A project-scoped agent is created with sandbox, approval, context, and tool profile settings.
4. Interactive TUI, plain REPL, one-shot, eval, or session subcommand mode is selected.
5. Each model turn streams assistant text/thinking/tool calls through an `EventSink`.
6. Tool calls are validated, permission-checked, optionally previewed, checkpointed when write-risk warrants it, executed, summarized, and appended to history.
7. Session checkpoints, logs, provider health, work ledger, and verification records are persisted.

## Core modules

- `src/main.rs`
  - Agent state and loop.
  - CLI parsing and top-level command dispatch.
  - Slash commands.
  - Built-in tool implementations.
  - Eval harness.
  - Prompt/context assembly, compaction, and event emission.

- `src/git_checkpoints.rs`
  - Git repository discovery for recovery snapshots.
  - Hidden checkpoint refs under `refs/dext/checkpoints/`.
  - Local checkpoint manifests and untracked-file sidecars under `.dext/checkpoints/`.
  - Undo list/preview/apply/prune helpers used by `/undo` and `dext undo`.

- `src/mutation_preview.rs`
  - Capped in-memory previews for `write_file`, `edit_file`, and `multi_edit`.
  - Sandbox-contained path resolution for previewed mutations.

- `src/memory_merge.rs`
  - Section-aware merge helpers for `MEMORY.md`.
  - Compact merge helpers for `recall.md`.
  - Explicit local Git merge-driver check/register/unregister support.

- `src/provider.rs`
  - Provider profile/catalog loading and normalization.
  - Built-in GLM, ChatGPT/Codex, OpenAI, Anthropic, DeepSeek, and local OpenAI-compatible profiles.
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

- `src/packs.rs`
  - Pack discovery, loading, and invocation.
  - Front matter parsing, precedence ordering, and pack listing/inspection rendering.

- `src/shelves.rs`
  - Shelf registry with typed manifests (`shelf.json`).
  - `ShelfManifest`, `PackManifest`, `Ability`, `Signal` types.
  - Scope-precedence resolution and ability metadata injection.

- `src/tool_policy.rs`
  - Tool input validation and command risk classification.
  - URL/host extraction and external-attempt guard helpers.
  - Bash guardrails (pipefail injection, unsafe pip flag blocking).

## Tool model

Dext exposes a deliberately small default native tool set:

- Filesystem: `read_file`, `read_symbol`, `write_file`, `edit_file`, `multi_edit`.
- Search: `fd`, `rg`.
- Shell/process: `bash` (atomic per call; the tool process group is cleaned up after exit).
- Network: `http`.
- Git: `git_diff`, `git_commit`.
- Tasks: `todo_read`, `todo_write`.
- Optional browser recipe: `browser` only when enabled.

The full catalog still implements specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`) for opt-in use via `--toolset full`, `DEXT_TOOLSET=full`, or `/tools full`. Frugal/tiny context modes force an even smaller toolset and hide `http` unless the user switches back to standard mode.

Bash is intentionally transaction-like: Dext starts tool commands in their own process group and cleans that group after the shell exits or is interrupted, so shell backgrounding (`cmd &`), `nohup`, and `disown` are not a supported way to keep servers alive across tool calls. `setsid`-style detaches are also unsupported because they escape Dext cleanup. If the user explicitly needs a persistent local service, prefer OS supervision without adding a Dext daemon tool. On Linux with systemd, use `systemd-run --user --unit=dext-<name> --same-dir <cmd>`, inspect with `systemctl --user status dext-<name>`/`journalctl --user-unit dext-<name>`, and stop it with `systemctl --user stop dext-<name>` when finished. Keep unit names prefixed with `dext-`; on platforms without systemd, use the platform's native supervisor or avoid a persistent service.

Lean schemas are the default to reduce prompt cost. Full schemas are available with `--tool-profile full` or `/tool-profile full`; `default` is treated as lean when parsing the env/CLI alias.

Capability-as-filesystem is deliberately parked. Dext does not implement `.dext/cap/` or virtual `/cap/...` today, and it should not become core without a concrete high-value use case and a safe pack/shelf prototype first. The unresolved risks are broad: permission mapping, hidden side effects, lifecycle cleanup, sandbox boundaries, streaming/progress, concurrency, discoverability, secrets/privacy, error semantics, versioning, and plugin-protocol creep.

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

## Git recovery and memory safety

Dext's Git-native recovery features are local runtime helpers, not
provider-visible tools:

- Before approved write-risk tool calls, Dext can create a checkpoint under
  hidden refs (`refs/dext/checkpoints/`) plus local manifests in
  `.dext/checkpoints/`. Direct file mutations receive path-specific restore
  hints.
- `/undo` and `dext undo` list, preview, and apply checkpoint restores. Normal
  restore updates worktree paths only; moving `HEAD` requires an explicit
  reset-head mode.
- Mutation previews render capped in-memory diffs for `write_file`, `edit_file`,
  and `multi_edit` before permission approval. The current `git` preview mode
  falls back to simple previews.
- Memory merge registration is explicit through `dext memory check/register`.
  It is local-only by default and targets `MEMORY.md` plus `recall.md`.

These helpers no-op outside Git repositories and preserve Dext's lean tool
surface: the model still sees the regular filesystem/Git tools, not extra
recovery tools.

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
