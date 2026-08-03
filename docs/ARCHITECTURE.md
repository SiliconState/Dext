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
  - Built-in tool implementations, including bounded, cancellation-aware native file reads and an 8 MiB `read_symbol` source-input ceiling.
  - Eval harness.
  - Prompt/context assembly and compaction; turn-stable guidance/pack/shelf prompt sections are cached separately from the volatile per-round environment tail, and each ancestor guidance/recall file is bounded at 1 MiB for both prompt loading and provenance hashing.
  - Event emission through the sinks it owns.

- `src/git_checkpoints.rs`
  - Git repository discovery for recovery snapshots.
  - Hidden checkpoint refs under `refs/dext/checkpoints/`.
  - Local checkpoint manifests and untracked-file sidecars under `.dext/checkpoints/`.
  - Strict current-row parsing plus full-grammar, integrity-checked retirement of recognized pre-JSON rows and their matched refs.
  - Manifest-first retention compaction so failed ref cleanup leaves only harmless orphan refs instead of manifest entries naming deleted refs.
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
  - Interrupt-aware parallel read execution that returns one result for every provider tool-call ID even when queued tasks are aborted; native blocking reads observe the shared cancellation flag between bounded chunks.
  - Post-mutation invalidation of cached pack discovery before the next provider request.
  - Narrow runtime context supplied by `Agent`, which remains the facade.

- `src/tool_journal.rs`
  - Bounded owner-private start/terminal records for approved side-effect-capable calls.
  - Resume reconciliation metadata that never stores raw tool input/output or replays uncertain calls.

- `src/tools.rs`
  - Tool catalog and a shared registry for required fields, permission, process, parallel, and default-profile metadata.
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
  - Transcript rendering, input box, status/live areas, permission prompts, slash completions, and the read-only `Ctrl+L` todo modal; todo state loading shares the 256 KiB runtime bound.
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

- `src/privacy.rs`
  - Privacy policy, sensitive-path/search-scope denial, and secret/PII redaction for anything that may leave the machine.

- `src/usage.rs`
  - Per-provider token-usage normalization into disjoint input buckets, per-model pricing, and spend/token budget caps with at most one cap per dimension.

- `src/events.rs`
  - The `AgentEvent` stream and the `EventSink` trait each front end implements. The sinks themselves stay with the console I/O layer in `main.rs`.

- `src/crash.rs`
  - Panic hook, redacted owner-only crash snapshots, and the bounded event breadcrumb trail they record.

- `src/process_tree.rs`
  - Child session/process-group detachment and whole-tree teardown after exit, timeout, interrupt, task cancellation, or guard drop.

- `src/secret_redactor.rs`
  - Streaming credential scrubbing for child-process output, holding back only enough tail bytes to catch a pattern split across reads.

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

Packs extend this tool model without changing it. Dext creates, discovers, inspects, maintains, and invokes user-authored packs under shelf roots; it ships no pack content and registers no pack as a provider-visible tool. Explicit `/pack` or `dext pack` invocation confirms only the selected project workflow. Conversational auto-invocation and all unrelated project `shelf.json` metadata require one first-use confirmation per active repository; `Always` persists a bounded owner-private, single-link project-scoped decision marker on Unix, and `/project-extensions reset` refuses unsafe marker shapes while removing a safe marker or clearing a session denial. Denied project metadata stays out of the model prompt and cannot shadow same-named trusted user/run metadata, while approved project context is labeled as repository-controlled. `PACK.md` and `shelf.json` reads require regular non-symlink files no larger than 1 MiB before prompt-level caps are applied. Pack helpers then run through the regular approval and sandbox path. On Windows, only native `.exe`/`.com` pack helpers can receive narrowly declared credentials through direct spawn; script helpers run through Bash with declared credentials removed.

The full catalog still implements specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`) for opt-in use via `--toolset full`, `DEXT_TOOLSET=full`, or `/tools full`. Frugal/tiny retain the default core tool capabilities while using lean schemas and smaller context/result budgets. Catalog metadata for required fields, permission/side-effect capability, external-process dispatch, parallel safety, and default/full exposure is centralized in `tools.rs`; schema-registry drift is regression-tested. Descriptions, schemas, risk-specific parsing, and per-tool summaries remain in their focused code paths rather than being forced into one oversized object.

Bash is intentionally transaction-like: Dext cleans the complete child process tree after the shell exits, times out, is interrupted, or an in-flight process-tree guard is dropped during task cancellation/unwinding. Unix children run in a detached session/process group; Windows children start suspended, enter a kill-on-close Job Object, and resume only after assignment. On Windows, shell-backed tools require a real Bash implementation such as Git for Windows; every shell execution path uses the same resolver, Dext skips Windows/WSL app aliases when selecting `bash.exe` from `PATH`, and `DEXT_BASH_PATH` can select an explicit executable. Cross-platform path rendering strips Windows verbatim prefixes, uses forward slashes for Git/shell arguments, and filters Unix, drive-letter, and UNC absolute paths from session work ledgers. Shell backgrounding (`cmd &`), `nohup`, and `disown` are therefore not a supported way to keep servers alive across tool calls. Unix `setsid`-style detaches are also unsupported because they escape process-group cleanup. If the user explicitly needs a persistent local service, prefer OS supervision without adding a Dext daemon tool. On Linux with systemd, use `systemd-run --user --unit=dext-<name> --same-dir <cmd>`, inspect with `systemctl --user status dext-<name>`/`journalctl --user-unit dext-<name>`, and stop it with `systemctl --user stop dext-<name>` when finished. Keep unit names prefixed with `dext-`; on platforms without systemd, use the platform's native supervisor or avoid a persistent service.

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

Session JSONL files include header provenance, exposed tools, approval/sandbox context, provider health, ledger entries, and message history. Invalid persisted usage costs or empty/non-positive budget caps are rejected on load rather than influencing resumed budget enforcement. They may contain sensitive prompts or tool output; do not publish them blindly. Read-only session review remains available through list, brief, analyze, grep, failure, verification, and decision views. Briefs are distilled rather than raw transcripts but may still contain sensitive ledger, path, failure, and verification data. Export writes an explicit copy; prune removes stale locks and project directories that contain no state other than stale locks while preserving session JSONL and all other project state.

If richer session navigation is requested again, first instrument actual demand. Prefer an on-demand, recent-first turn timeline derived from structured tool results and ledger facts: one row per user turn, no prose keyword classifier, no provider-visible tool, no context mutation, and no durable identity scheme until a measured use case requires one.

## Safety model

Dext has three safety layers:

1. Tool validation rejects malformed calls before execution.
2. Permission profiles decide which privileged tools require approval.
3. Sandbox profiles constrain filesystem/process behavior at the agent-policy level.

Profiles:

- Approval: `ask`, `auto-read`, `auto-write`, `never`, `always`. `auto-write` still prompts for Danger-class shell commands. Destructive Git worktree/ref/stash changes, per-command config overrides, and unknown aliases/subcommands are Danger. Because repository configuration can execute pagers, filters, fsmonitor, diff/textconv drivers, hooks, or aliases, shell Git is also Danger unless it uses explicit `git --no-pager` and matches a narrow helper-free metadata-inspection allowlist; commands such as `grep`, `diff-tree`, `ls-files`, `check-ignore`, and `check-attr` remain gated because they can invoke fsmonitor. Use hardened native Git tools for review operations. Recognized dynamic/wrapper command paths and inline/stdin interpreter code, including shell input redirections and heredocs, are Danger too; dynamic command words include variable/command, glob, brace, tilde, and attached-redirection expansion. Actual shell curl/wget/HTTPie/XH requests are gated because startup configuration and request bodies are not safely inferable; use the native `http` tool for reads. Interpreter matching covers attached/clustered flags and common versioned Python/PyPy/Perl/Node/Ruby/PHP launchers; Windows `.exe`/`.com` command and wrapper matching is case-insensitive.
- Sandbox: `read-only`, `workspace-write`, `danger-full-access`. On supported Linux/macOS hosts, confined profiles preserve every read available to the Dext process user. `workspace-write` permits writes only under the sandbox root, scratch/device roots, and common per-user toolchain caches; `read-only` retains only required scratch/device writes. macOS Seatbelt profiles authorize both canonical `/private/...` scratch paths and their `/var` or `/tmp` aliases so standard temp APIs remain confined but usable.
- Tool subprocesses remove credential-shaped environment variables by default. `DEXT_INHERIT_TOOL_CREDENTIALS=1` is an explicit high-trust opt-in for model-invoked bash/external tools that require the parent credential environment; hooks and Dext-owned subprocesses always scrub credentials. Pack-declared helper credentials are narrower still, and project-local declarations are ignored. Approved `pre_tool` and `post_tool` hooks receive privacy-redacted tool input; `post_tool` output is redacted too.

Dext starts with approval profile `ask`. Interactive frontends request approval for gated tools; non-interactive and JSON runs deny instead of blocking. Startup precedence is the last CLI policy flag, then valid `DEXT_APPROVAL`, then true `DEXT_TRUST`, then `ask`. `--trust` and `DEXT_TRUST=1` explicitly select `always`; resumed sessions retain their historical profile only as provenance and current-run policy clears stale grants. Approval policy and sandbox confinement are independent.

Durable session open, stale-lock reclamation, cleanup, and prune operations are serialized across Dext processes by an owner-private operation lock under `DEXT_HOME`; stale removal revalidates the recorded token and PID while holding that lock. Each active session then uses a `session.lock.json` identity record. Durable sessions also keep a small owner-private `tool-journal.json` beside the active transcript. Approved side-effect-capable calls are fenced as `started` before dispatch and updated to `completed`, `failed`, or `interrupted` immediately after execution. Journal entries contain only bounded metadata and an input digest, not raw input/output. Resume reconciles pending transcript calls without replay. `--no-session` and `--fork` intentionally disable both durable state and this crash-side-effect recovery.

`dext doctor` is an observational diagnostics path. It reuses the startup approval resolver and bounded state inspectors, reports effective policy/source separately from sandbox kernel enforcement, and inspects only active/latest provider, auth, session, todo, settings, journal, and checkpoint state. It does not repair files, resolve credential references, or contact provider endpoints.

## Git recovery and mutation previews

Recovery checkpoints and mutation previews are local runtime helpers, not
provider-visible tools:

- Before approved write-risk tool calls, Dext can create a checkpoint under
  hidden refs (`refs/dext/checkpoints/`) plus owner-private manifests and
  sidecars in `.dext/checkpoints/`. Storage containers must be real directories; on Unix they must
  be current-user-owned, `.dext` must not be group/world-writable, and managed checkpoint
  directories are owner-private. Locked Unix mutations may repair modes only on current-user-owned
  managed directories; inspection does not repair them, restore rejects unsafe sidecar/blob containers,
  and prune retains unsafe artifact directory trees with bounded warnings while unlinking only an orphan
  top-level sidecar symlink without following it. Dext adds `/.dext/` to the repository-local
  Git exclude and automatically retains at most 20 checkpoints for seven days.
  Direct file mutations receive path-specific restore hints. Write-risk
  `bash`/`awk`/`csvkit` checkpoints inventory at most 500 existing untracked
  paths, preserve regular files within 8 MiB/file and 32 MiB total bounds, and
  preserve bounded UTF-8 symlink targets without following them. Regular-file
  content uses owner-private SHA-256-addressed blobs shared by retained
  checkpoints; unchanged source paths reuse the session cache only while both source and blob
  metadata fingerprints remain stable. Preview and restore rehash blobs before trusting them;
  pruning or a failed checkpoint creation removes valid unreferenced/new blobs; malformed or unsafe
  blob entries and sidecar directory trees remain untouched and emit bounded warnings without stopping other
  retention cleanup, while an orphan top-level sidecar symlink with a valid checkpoint ID is unlinked
  without following it. Owner execute state is descriptor metadata. Current manifests record exact
  direct-sidecar membership; older manifests without that field fail conservatively before mutation
  when a missing artifact is ambiguous rather than deleting current path content. Recognized retired
  rows must match the complete retired field grammar and any live ref's recorded OID. Retention
  publishes the compacted manifest before deleting expired/retired refs or artifacts, so a cleanup
  failure leaves only orphan state rather than a manifest naming a deleted recovery point. Non-UTF-8 names,
  unsupported types, and path/type/size caps are explicit
  partial-recovery gaps. If any such gap remains, Dext requests separate repository/session-scoped
  approval and records the gap; denial blocks the call, while approval preserves
  the tracked/staged state and bounded subset. Other checkpoint failures remain
  fail-closed. Each call is checkpointed at its sequential dispatch boundary,
  so later calls in one round include earlier mutations. In repositories without
  an initial commit, writes that would overwrite existing worktree/index state
  fail closed because Git has no normal restore base. A workspace with no `.git`
  marker is non-Git unless ambient `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR`
  routing exists; routed-without-marker and malformed-marker cases fail loudly
  because Dext-owned Git commands scrub routing variables. Never mirror-push
  `refs/dext/*`.
- `/undo` and `dext undo` list, preview, and apply checkpoint restores. Normal
  restore updates worktree paths only; moving `HEAD` requires an explicit
  reset-head mode.
- Mutation previews render capped, alignment-aware Myers line diffs for `write_file`, `edit_file`,
  and `multi_edit` before permission approval. The 4 KiB display cap remains;
  line counts cover the full proposed change even when rendering is truncated,
  final-newline-only changes are explicit, and very high line-count inputs use a
  bounded conservative fallback. The current `git` preview mode
  falls back to these simple previews.

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
