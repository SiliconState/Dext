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
  - Agent state and loop, including value-owned parsed-stream outcomes, bounded incomplete-response continuation, and preservation of every nonempty assistant response before any paired tool results are appended.
  - CLI parsing, top-level command dispatch, and structured `dext doctor` rendering.
  - Slash commands.
  - Built-in tool implementations, including bounded, cancellation-aware native file reads and an 8 MiB `read_symbol` source-input ceiling.
  - Eval harness.
  - Prompt/context assembly and compaction; Responses-based summaries reject incomplete terminals, retry within the bounded summary stream budget, and carry parsed usage across retries. The standard built-in agent prompt is compact and invariant-driven, leaving tool-specific syntax in lean schemas rather than duplicating it in universal prose. Turn-stable guidance/pack/shelf prompt sections are cached separately from the volatile per-round environment tail. Context-file cache identity includes size/time metadata plus file identity/change metadata where the platform exposes it, so same-size atomic replacements invalidate within a turn. That tail omits toolset/schema labels already represented by provider definitions and host-only compaction thresholds, while retaining actionable runtime policy/model state; variable cwd/Git/provider/model values are byte-bounded and JSON-quoted when needed to remain one line, and persisted ledger/provider-health strings are collapsed and bounded before prompt rendering. Context strategy budgets omit all-zero rows before the first tool action, then remain explicit after actions to preserve reset/warning/pivot state. Each ancestor guidance/recall file is bounded at 1 MiB; provenance paths and raw-file hashes come from the same bounded reads actually included in the composed prompt. Aggregate project-context and per-section caps include headings or truncation markers as applicable, and wholly omitted files are excluded from prompt provenance.
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
  - API-key, ChatGPT OAuth, and Anthropic Claude Pro/Max OAuth login flows; runtime auth retains whether a resolved secret is an API key or OAuth token. OAuth callback binding is loopback-only, accepted connections use blocking I/O under one two-second complete-header deadline, result pages wait for exchange/storage completion, exchange/refresh transport is bounded and redirect-free, and active OAuth credentials are rechecked at user-turn boundaries.
  - Request builders for Anthropic, OpenAI-compatible, and ChatGPT/Codex response APIs. Public adaptive Anthropic models (Sonnet 4.6, Sonnet 5, Opus 4.6/4.7/4.8, Opus 5, and Fable 5) omit `thinking.display`; transformed OAuth and API-key request fixtures verify the same adaptive shape.
  - Model alias normalization and provider/model switching helpers.

- `src/claude_subscription.rs`
  - Dext-native, version-pinned Claude subscription request compatibility for the official built-in Anthropic profile.
  - Billing/version fingerprints, seeded XXH64 body checksum, OAuth request metadata, validated optional Claude identity discovery, and request/session identifiers.
  - Pure protocol tests derived from public compatibility vectors; API-key and non-official/custom provider requests bypass this module.

- `src/sse.rs`
  - Capped SSE framing shared by the runtime and Criterion benchmark target.

- `src/streaming.rs`
  - Provider-specific event validation/assembly.
  - Provider-neutral streamed blocks and strict final tool-argument construction.
  - Anthropic implicit terminal handling completes open display blocks but never authorizes an unstopped tool call; explicitly stopped max-token calls are discarded only for EOF-shaped argument JSON, while malformed values remain errors.

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

- `src/pack_runtime.rs`
  - Optional pack-owned executable runtime protocol (`runtime.json` v1), dynamic tool schemas/risk, one-shot JSON request/response execution, bounded state/effects, and manifest identity checks.
  - Activation/idle/read confinement, credential isolation, continuation budgets, and session snapshots; Dext ships no runtime payloads.

- `src/packs.rs`
  - Shelf-contained pack scaffolding, discovery, loading, invocation, precedence, and optional runtime-manifest discovery.
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

Packs normally extend this tool model without changing it. Dext creates, discovers, inspects, maintains, and invokes user-authored packs under shelf roots and ships no pack content. A reviewed pack may optionally declare a `runtime.json` v1 native helper; only while that runtime is active, Dext appends its validated dynamic tools to provider schemas and `/allow`/`/revoke`/`/allowed`. Runtime activation has a separate executable-code approval, `never` disables it, and a prompt-level `Always` decision is scoped to the exact canonical pack-directory/source identity, manifest digest, and executable digest. Executable bytes are rehashed before every call. Changing approval or sandbox policy revokes the runtime, dynamic grants/denials, and queued callbacks. Activation/idle/read tools enforce read-only confinement inside the executor with credentials scrubbed; declared write/danger tools keep normal approval, sandbox, durable journal, and fail-closed Git checkpoint controls. Recursive schemas accept only the implemented v1 keyword subset, and runtime names cannot collide with the full native catalog, active dynamic tools, or host approval pseudo-operations. One-shot JSON calls preserve the full protocol response cap; malformed timeout overrides fail closed; stdin delivery and root execution share the configured deadline, while output drain after process-tree cleanup has a separate one-second cap. Runtime-exposed content/effect/view/queued-prompt text rejects unsafe terminal controls. Response state/effects/continuation accounting applies atomically; bounded state, used continuation count, and queued prompts persist, while interrupted delayed prompts are canceled and refunded. Content, steering, views, and surfaced lifecycle errors are privacy-redacted; opaque owner-private state is not rewritten and must not contain secrets. Session restore preserves current-run approval and sandbox policy, discards saved grants, and preflights project trust, exact source/directory identity, manifest/hash/state accounting, and current executable approval before mutating the live agent; failure cannot partially apply the saved sandbox/model/session state. Explicit `/pack` or `dext pack` invocation still confirms only the selected project workflow. Conversational auto-invocation and all unrelated project `shelf.json` metadata require one first-use confirmation per active repository; `Always` persists a bounded owner-private, single-link project-scoped decision marker on Unix, and `/project-extensions reset` refuses unsafe marker shapes while removing a safe marker or clearing a session denial. Denied project metadata stays out of the model prompt and cannot shadow same-named trusted user/run metadata, while approved project context is labeled as repository-controlled. Compact pack/shelf metadata is bounded and rendered on one safe line; shelf Context bodies preserve ordinary newlines/tabs but normalize unsafe terminal controls and Unicode line separators. `PACK.md` and `shelf.json` reads require regular non-symlink files no larger than 1 MiB before prompt-level caps are applied. On Windows, only native `.exe`/`.com` pack helpers can receive narrowly declared credentials through direct spawn; scripts and runtime helpers receive none.

The full catalog still implements specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`) for opt-in use via `--toolset full`, `DEXT_TOOLSET=full`, or `/tools full`; non-JSON startup emits `[tools] toolset full` for that opt-in in both standard and frugal modes. Frugal mode retains the selected toolset and schema profile while applying smaller context/result/capture budgets. The resolved live context mode is passed through sequential and parallel tool rounds, so `/context` immediately governs subsequent native read captures and result shaping instead of rereading startup environment state. Explicit context selections remain pinned across provider switches and session restoration; automatic selections follow the active local/cloud provider. Catalog metadata for required fields, permission/side-effect capability, external-process dispatch, parallel safety, and default/full exposure is centralized in `tools.rs`; schema-registry drift is regression-tested. Static native tools and active pack-runtime tools first become provider-neutral `{name, description, schema}` descriptors; the selected lean/full schema profile applies to both. Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, and ChatGPT Responses adapters preserve those three fields while adding contract-required nesting, strictness, and cache controls. Empty tool collections are omitted from serialized requests, including summaries and models whose metadata disables tools. Lean schema stripping removes schema annotations without deleting arguments named `description` or literal data with that key. Descriptions, schemas, risk-specific parsing, and per-tool summaries remain in their focused code paths rather than being forced into one oversized object.

Bash is intentionally transaction-like: Dext cleans the complete child process tree after the shell exits, times out, is interrupted, or an in-flight process-tree guard is dropped during task cancellation/unwinding. Unix children run in a detached session/process group; Windows children start suspended, enter a kill-on-close Job Object, and resume only after assignment. On Windows, shell-backed tools require a real Bash implementation such as Git for Windows; every shell execution path uses the same resolver, Dext skips Windows/WSL app aliases when selecting `bash.exe` from `PATH`, and `DEXT_BASH_PATH` can select an explicit executable. Cross-platform path rendering strips Windows verbatim prefixes, uses forward slashes for Git/shell arguments, and filters Unix, drive-letter, and UNC absolute paths from session work ledgers. Shell backgrounding (`cmd &`), `nohup`, and `disown` are therefore not a supported way to keep servers alive across tool calls. Unix `setsid`-style detaches are also unsupported because they escape process-group cleanup. If the user explicitly needs a persistent local service, prefer OS supervision without adding a Dext daemon tool. On Linux with systemd, use `systemd-run --user --unit=dext-<name> --same-dir <cmd>`, inspect with `systemctl --user status dext-<name>`/`journalctl --user-unit dext-<name>`, and stop it with `systemctl --user stop dext-<name>` when finished. Keep unit names prefixed with `dext-`; on platforms without systemd, use the platform's native supervisor or avoid a persistent service.

Provider transport uses a 15-second connect deadline, a first-header deadline of 180 seconds for cloud providers or 600 seconds for local llama.cpp, and an idle deadline of 90 seconds for cloud streams/bodies or 300 seconds for local ones. Positive `DEXT_PROVIDER_CONNECT_TIMEOUT_SECS`, `DEXT_PROVIDER_FIRST_BYTE_TIMEOUT_SECS`, and `DEXT_PROVIDER_STREAM_IDLE_TIMEOUT_SECS` values override those defaults. Initial calls and retries share the same policy. Non-stream summary JSON is capped at 4 MiB and provider error diagnostics at 4,000 bytes. SSE framing bounds in-progress allocation before appending oversized chunks while incrementally accepting one large read containing many valid events. A ChatGPT Responses finalize error for malformed function arguments is never executed; when no visible text was streamed, Dext compacts once if a safe split exists and retries exactly once, then surfaces the original protocol failure if it persists.

Lean schemas are the default to reduce prompt cost. Their descriptions carry each tool's unique action/safety/usage cues; argument structure remains in the stripped schema and cross-tool workflow stays in the compact system prompt. A hermetic clean-repository fixture measures the real standard stable block, a runtime tail with neutral provider/model placeholders, and deterministic normalized JSON for the default 13 lean `{name, description, schema}` descriptors; their total is capped below 6,000 bytes. This provider-neutral comparison payload is not an actual provider request, tokenizer count, billing count, formal canonical-JSON encoding, or universal maximum. The fixture separately reports actual tool-array bytes and a signed size delta from the normalized payload for Anthropic cache-on/cache-off, OpenAI Chat Completions, OpenAI Responses, and ChatGPT Responses. Semantic and full-request tests preserve essential lean cues and require static plus active runtime tools to retain identical names, descriptions, and schemas across every adapter. Full descriptions and schemas are available with `--tool-profile full` or `/tool-profile full`; `default` is treated as lean when parsing the env/CLI alias.

The current extension model is packs and shelves. Dext does not expose `.dext/cap/` or virtual `/cap/...` paths; adding another capability protocol would require a concrete use case and a security model for permissions, side effects, lifecycle, concurrency, discovery, secrets, and versioning.

## Sessions and state

Dext distinguishes project files from runtime state:

- Project files live in the Git repository.
- State defaults to `~/.dext` or `DEXT_HOME`.
- Runtime provider/auth loads reject symlinks, non-regular files, oversized content, and foreign Unix ownership. Group/world-writable provider catalogs are rejected; owner-owned auth files with loose Unix mode are repaired to `0600` on load. Doctor uses bounded no-follow, inode-stable inspection and reports the same policy without repairing files.
- Project latest sessions/logs and durable Seats are scoped by a stable project key.
- A Seat is a durable agent identity; a session is one disposable incarnation. Seat records are bounded owner-private JSON under `projects/<project-key>/seats/<seat-id>/seat.json`; selection is in-memory until a successful durable save or explicit metadata update, while transcripts and crash recovery remain session-owned.
- Named sessions can be stored under `DEXT_SESSIONS_DIR`.
- Session exports (`dext-session-*.jsonl`, `dext-session-*.html`) are ignored by Git.

Session JSONL files include header provenance, an optional Seat reference, active pack-runtime identity/state/used-and-pending continuation data, exposed tools, approval/sandbox context, provider health, ledger entries, and message history. Runtime snapshots are bounded inside the existing 256 KiB header ceiling and restore only under current-run approval/sandbox policy after saved grants are discarded. Restoration preflights project trust, exact canonical pack-directory/source identity, manifest/hash/state accounting, and approval against the current executable digest before applying any saved sandbox/model/session fields, so changed, missing, shadowed, denied, or malformed runtimes fail without partially mutating the live agent. Plain unseated writes retain format v3 and Seat-only writes use v4 for backward compatibility; runtime-bearing writes use v5 so pre-runtime binaries reject rather than ignore executable-runtime state. Valid transitional v3 Seat headers remain loadable and validated, then upgrade on the next Seat-only save; v1–v2 cannot carry Seat metadata. Header serialization and reads are bounded at 256 KiB. `--seat NAME` starts a new incarnation, `--seat NAME --resume` follows that Seat's pointer, and an explicitly resumed unseated session can be attached on its next save. Cross-Seat, cross-project, malformed, or unprovenanced seated restoration fails before saved state is applied. `--no-session --seat NAME` is contextual only and never creates or updates Seat state. Invalid accounting metadata is rejected on load. Session review remains available through list, brief, analyze, grep, failure, verification, and decision views. Export writes an explicit copy; `/reset` serializes matching pointer update and transcript removal and rolls the pointer back when deletion fails; prune preserves sessions, Seats, and all other non-lock state.

Seats deliberately do not own provider/model selection, permissions, tools, task graphs, or transcripts. Portable lowercase ids are validated before path construction; Unix state ancestors must be owner-safe, managed directories owner-private, and record files single-link/private/no-follow. `dext seat set` applies label/summary updates under the cross-process state-operation lock with atomic secret-file replacement. Prompt context is privacy-redacted, marked non-authoritative, and capped at 1,000 bytes for the complete rendered Seat section in both context modes; the independent persistent label validator remains capped at 128 characters. Changing project keys clears identity; durable labels override stale session labels. Crew directly maps portable names to `crew.<agent>` and uses deterministic hashed Seat ids for other previously valid custom role names. Child runs retain `--no-session`; captured, detached-systemd, and tmux-pane execution resolve one absolute Dext state root. Path-based Seat operations still have a hostile same-user ancestor-replacement race after validation; R-012 tracks descriptor-relative hardening.

If richer session navigation is requested again, first instrument actual demand. Prefer an on-demand, recent-first turn timeline derived from structured tool results and ledger facts: one row per user turn, no prose keyword classifier, no provider-visible tool, and no context mutation.

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
- `frugal`: the automatic local-provider default; lower prompt/history/tool-result and native-read capture caps plus bounded LLM-backed compaction, without removing core tool capabilities or overriding an explicit toolset/schema selection. `/context` applies it immediately to later tool rounds, and an explicit selection remains pinned across provider switches and session restoration.

The former `tiny` mode and `--tiny` alias are retired and rejected rather than silently mapped to `frugal`. Frugal also owns the stricter pseudo-tool-protocol sanitizer: partial-stream recovery and TUI transcript/inspector paths replace serialized or multiline tool-call-like assistant payloads with a redaction marker while retaining surrounding prose; standard keeps the narrower legacy line detector.

Frugal mode uses a compact task-graph discipline for nontrivial work: steps have required inputs and observable outputs, independent reads can run in parallel, verified results are reused, and recovery repairs only the affected step. Dext intentionally does not maintain a graph runtime or impose local-only action locks, round ceilings, output suppression, or forced finalization.

Compaction preserves recent tool evidence and summarizes older conversation when history approaches the model-aware context budget.

## Verification surface

Expected release assets also include a CycloneDX JSON SBOM. The release workflow includes the SBOM in `SHA256SUMS`, provenance attestation, verification, and publication alongside the four platform archives. This path completed end to end for `v0.1.0`; [`RELEASING.md`](RELEASING.md) records the workflow and verification evidence.

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

On Windows CI and release builders, the scheduler-sensitive `fast_bash_command_returns_without_100ms_poll_tail` regression runs alone after the remaining release tests. Its original `<90 ms` assertion remains unchanged; isolation prevents unrelated suite load from obscuring the process-wait regression it measures. The external-runner stdin-backpressure regression still requires bounded completion under the shared deadline, but accepts either the stdin-write or root-process timeout phase on Windows because pipe buffering can make the full write complete at the deadline boundary; Unix continues to require the stdin-write phase. The tool-call mock provider consumes its bounded `Content-Length` request body before responding so Windows does not reset the connection with unread request data.
