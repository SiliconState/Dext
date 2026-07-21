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
  - CLI parsing, top-level command dispatch, and structured `dext doctor` rendering.
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

- `src/sandbox.rs`
  - Linux Landlock and macOS Seatbelt command construction.
  - Profile-specific write roots, private scratch directories, and offline diagnostic confinement.

- `src/provider.rs`
  - Provider profile/catalog loading, normalization, and bounded side-effect-free catalog/auth inspection.
  - Connect, first-byte, stream/body-idle, and non-stream body-size limits for provider transport.
  - Built-in GLM, ChatGPT/Codex, OpenAI, Anthropic, Kimi Code, DeepSeek, and local OpenAI-compatible profiles.
  - Live llama.cpp runtime context probing for the local provider; unavailable local servers fall back cleanly without aborting startup.
  - API-key and OAuth login flows.
  - Request builders for Anthropic, OpenAI-compatible, and ChatGPT/Codex response APIs.
  - Model alias normalization and provider/model switching helpers.

- `src/sse.rs`
  - Capped SSE framing shared by the runtime and Criterion benchmark target.

- `src/streaming.rs`
  - Provider-specific event validation/assembly.
  - Provider-neutral streamed blocks and strict final tool-argument construction.

- `src/tool_round.rs`
  - Tool-call planning, approval inputs, checkpoint/journal boundaries, dispatch, and result normalization.
  - Narrow runtime context supplied by `Agent`, which remains the facade.

- `src/tool_journal.rs`
  - Bounded owner-private start/terminal records for approved side-effect-capable calls.
  - Resume reconciliation metadata that never stores raw tool input/output or replays uncertain calls.

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
  - Inline Ratatui UI in the regular terminal buffer.
  - Transcript rendering, input box, status/live areas, permission prompts, slash completions, and the read-only `Ctrl+L` todo modal.
  - See [`TUI.md`](TUI.md) for the renderer contract, exact dependency stack, compatibility patch, and PTY gate.

- `src/packs.rs`
  - Shelf-contained pack scaffolding, discovery, loading, invocation, and precedence.
  - Front matter parsing and pack listing/inspection rendering.
  - Dext ships no pack content; user/project/environment shelves own all packs.

- `src/shelves.rs`
  - Shelf registry with typed manifests (`shelf.json`).
  - `ShelfManifest`, `PackManifest`, and provider-neutral ability metadata.
  - Scope-precedence resolution and bounded context injection for manifest-only shelves.
  - In-process shelf implementations can participate in the typed signal/effect loop; filesystem manifests do not register executable tools, commands, or arbitrary effect handlers.

- `vendor/ratatui-core/`
  - Exact upstream `ratatui-core 0.1.2` source selected through `[patch.crates-io]`.
  - Narrow inline-terminal fixes that avoid synchronous cursor-query stalls and whole-display clears during resize.
  - Hunk rationale and refresh instructions in `vendor/ratatui-core/DEXT_PATCH.md`.

## Tool model

Dext exposes a deliberately small default native tool set:

- Filesystem: `read_file`, `read_symbol`, `write_file`, `edit_file`, `multi_edit`.
- Search: `fd`, `rg`.
- Shell/process: `bash` (atomic per call; the child process tree is cleaned up after exit).
- Network: `http`.
- Git: `git_diff`, `git_commit`.
- Tasks: `todo_read`, `todo_write`.

Packs extend this tool model without changing it. Dext creates, discovers, inspects, maintains, and invokes user-authored packs under shelf roots; it ships no pack content and registers no pack as a provider-visible tool. Pack helpers run through the regular approval and sandbox path. On Windows, only native `.exe`/`.com` pack helpers can receive narrowly declared credentials through direct spawn; script helpers run through Bash with declared credentials removed.

The full catalog still implements specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`) for opt-in use via `--toolset full`, `DEXT_TOOLSET=full`, or `/tools full`. Frugal/tiny retain the default core tool capabilities while using lean schemas and smaller context/result budgets.

Bash is intentionally transaction-like: Dext cleans the complete child process tree after the shell exits, times out, or is interrupted. Unix children run in a detached session/process group; Windows children start suspended, enter a kill-on-close Job Object, and resume only after assignment. On Windows, shell-backed tools require a real Bash implementation such as Git for Windows; every shell execution path uses the same resolver, Dext skips Windows/WSL app aliases when selecting `bash.exe` from `PATH`, and `DEXT_BASH_PATH` can select an explicit executable. Cross-platform path rendering strips Windows verbatim prefixes, uses forward slashes for Git/shell arguments, and filters Unix, drive-letter, and UNC absolute paths from session work ledgers. Shell backgrounding (`cmd &`), `nohup`, and `disown` are therefore not a supported way to keep servers alive across tool calls. Unix `setsid`-style detaches are also unsupported because they escape process-group cleanup. If the user explicitly needs a persistent local service, prefer OS supervision without adding a Dext daemon tool. On Linux with systemd, use `systemd-run --user --unit=dext-<name> --same-dir <cmd>`, inspect with `systemctl --user status dext-<name>`/`journalctl --user-unit dext-<name>`, and stop it with `systemctl --user stop dext-<name>` when finished. Keep unit names prefixed with `dext-`; on platforms without systemd, use the platform's native supervisor or avoid a persistent service.

Provider transport uses a 15-second connect deadline, a first-header deadline of 180 seconds for cloud providers or 600 seconds for local llama.cpp, and an idle deadline of 90 seconds for cloud streams/bodies or 300 seconds for local ones. Positive `DEXT_PROVIDER_CONNECT_TIMEOUT_SECS`, `DEXT_PROVIDER_FIRST_BYTE_TIMEOUT_SECS`, and `DEXT_PROVIDER_STREAM_IDLE_TIMEOUT_SECS` values override those defaults. Initial calls and retries share the same policy. Non-stream summary JSON is capped at 4 MiB and provider error diagnostics at 4,000 bytes. SSE framing bounds in-progress allocation before appending oversized chunks while incrementally accepting one large read containing many valid events.

Lean schemas are the default to reduce prompt cost. Full schemas are available with `--tool-profile full` or `/tool-profile full`; `default` is treated as lean when parsing the env/CLI alias.

The current extension model is packs and shelves. Dext does not expose `.dext/cap/` or virtual `/cap/...` paths; adding another capability protocol would require a concrete use case and a security model for permissions, side effects, lifecycle, concurrency, discovery, secrets, and versioning.

## Sessions and state

Dext distinguishes project files from runtime state:

- Project files live in the Git repository.
- State defaults to `~/.dext` or `DEXT_HOME`.
- Runtime provider/auth loads reject symlinks, non-regular files, oversized content, and foreign Unix ownership. Group/world-writable provider catalogs are rejected; owner-owned auth files with loose Unix mode are repaired to `0600` on load. Doctor uses bounded no-follow, inode-stable inspection and reports the same policy without repairing files.
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
- Sandbox: `read-only`, `workspace-write`, `danger-full-access`. On supported Linux/macOS hosts, confined profiles preserve every read available to the Dext process user. `workspace-write` permits writes only under the sandbox root, scratch/device roots, and common per-user toolchain caches; `read-only` retains only required scratch/device writes. macOS Seatbelt profiles authorize both canonical `/private/...` scratch paths and their `/var` or `/tmp` aliases so standard temp APIs remain confined but usable.
- Tool subprocesses remove credential-shaped environment variables by default. `DEXT_INHERIT_TOOL_CREDENTIALS=1` is an explicit high-trust opt-in for model-invoked bash/external tools that require the parent credential environment; hooks and Dext-owned subprocesses always scrub credentials. Pack-declared helper credentials are narrower still, and project-local declarations are ignored.

Dext starts with approval profile `ask`. Interactive frontends request approval for gated tools; non-interactive and JSON runs deny instead of blocking. Startup precedence is the last CLI policy flag, then valid `DEXT_APPROVAL`, then true `DEXT_TRUST`, then `ask`. `--trust` and `DEXT_TRUST=1` explicitly select `always`; resumed sessions retain their historical profile only as provenance and current-run policy clears stale grants. Approval policy and sandbox confinement are independent.

Durable sessions use a small owner-private `tool-journal.json` beside the active session transcript. Approved side-effect-capable calls are fenced as `started` before dispatch and updated to `completed`, `failed`, or `interrupted` immediately after execution. Entries contain only bounded metadata and an input digest, not raw input/output. Resume reconciles pending transcript calls without replay. `--no-session` and `--fork` intentionally disable both durable state and this crash-side-effect recovery.

`dext doctor` is an observational diagnostics path. It reuses the startup approval resolver and bounded state inspectors, reports effective policy/source separately from sandbox kernel enforcement, and inspects only active/latest provider, auth, session, todo, settings, journal, and checkpoint state. It does not repair files, resolve credential references, or contact provider endpoints.

## Git recovery and mutation previews

Recovery checkpoints and mutation previews are local runtime helpers, not
provider-visible tools:

- Before approved write-risk tool calls, Dext can create a checkpoint under
  hidden refs (`refs/dext/checkpoints/`) plus owner-private manifests and
  sidecars in `.dext/checkpoints/`. Dext adds `/.dext/` to the repository-local
  Git exclude and automatically retains at most 20 checkpoints for seven days.
  Direct file mutations receive path-specific restore hints. Never mirror-push
  `refs/dext/*`.
- `/undo` and `dext undo` list, preview, and apply checkpoint restores. Normal
  restore updates worktree paths only; moving `HEAD` requires an explicit
  reset-head mode.
- Mutation previews render capped in-memory diffs for `write_file`, `edit_file`,
  and `multi_edit` before permission approval. The current `git` preview mode
  falls back to simple previews.

Checkpoint helpers no-op outside Git repositories. Recovery and preview behavior preserve Dext's lean tool surface: the model still sees the regular filesystem/Git tools, not extra recovery tools.

## Context modes

- `standard`: normal caps and lean tool schemas by default; this remains the default for frontier/cloud providers.
- `frugal`: the automatic local-provider default; lower prompt/history/tool-result caps and bounded LLM-backed compaction without removing core tool capabilities or overriding an explicit toolset/schema selection.
- `tiny`: the smallest prompt/history variant for constrained local models.

Frugal and tiny use a compact task-graph discipline for nontrivial work: steps have required inputs and observable outputs, independent reads can run in parallel, verified results are reused, and recovery repairs only the affected step. Dext intentionally does not maintain a graph runtime or impose local-only action locks, round ceilings, output suppression, or forced finalization.

Compaction preserves recent tool evidence and summarizes older conversation when history approaches the model-aware context budget.

## Verification surface

Expected release assets also include a CycloneDX JSON SBOM. The release workflow includes the SBOM in `SHA256SUMS`, provenance attestation, verification, and publication alongside the four platform archives. The first successful tag run remains explicitly tracked in [`RELEASING.md`](RELEASING.md) until this path has end-to-end evidence.

Expected checks before releasing Dext changes:

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo deny check licenses
cargo test -p ratatui-core --lib --locked
cargo bench --no-run --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

The TUI smoke suite launches the real compiled binary inside a pseudo-terminal. In addition to launch/help/exit coverage, it checks narrow and wide layouts, multiline input, live-stream input, resize survival, bounded cursor queries, zero whole-screen resize clears, and completed output after resize. Renderer changes also follow the live-terminal checks in [`TUI.md`](TUI.md).

On Windows CI and release builders, the scheduler-sensitive `fast_bash_command_returns_without_100ms_poll_tail` regression runs alone after the remaining release tests. Its original `<90 ms` assertion remains unchanged; isolation prevents unrelated suite load from obscuring the process-wait regression it measures. The tool-call mock provider consumes its bounded `Content-Length` request body before responding so Windows does not reset the connection with unread request data.
