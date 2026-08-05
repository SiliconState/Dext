# Dext

Dext is a single-binary Rust coding agent that runs from your terminal, keeps project-scoped session state, and gives the model a small set of native tools for filesystem, search, shell/data, HTTP, Git, and task tracking.

Dext is source-first: prompts, runtime state, tool policies, provider wiring, and the TUI live in this repository with no external service required beyond whichever model provider you authenticate.

## Highlights

- Interactive terminal agent with an inline ratatui TUI; no alternate-screen takeover.
- One-shot mode for scripted tasks and JSON/stream-JSON output for automation.
- Provider catalog with built-in GLM, ChatGPT/Codex, OpenAI, Anthropic, Kimi Code, DeepSeek, and local OpenAI-compatible profiles plus env/catalog overrides.
- OAuth/API-key auth flows stored outside the repo under Dext state directories.
- Lean default tool schemas and smaller default toolset, with optional full schemas/full toolset and frugal context mode.
- Project-scoped latest sessions, named session export/analyze/grep/failure/verification helpers.
- Permission and sandbox profiles for read-only, workspace-write, and explicit danger modes.
- Eval harness, release tests, and PTY-backed TUI regression coverage for streaming and resize behavior.
- Exact Ratatui dependency versions plus a documented, narrowly vendored `ratatui-core` compatibility patch.
- Git-native safety helpers: pre-mutation checkpoints, `/undo`/`dext undo`, mutation previews, and explicit memory-file merge-driver registration.

## Install

Requires Rust stable with edition 2024 support.

Install from source:

```bash
git clone https://github.com/SiliconState/Dext.git
cd Dext
cargo install --path . --force --locked
```

Versioned GitHub releases also provide tested archives for Linux x86_64 GNU, macOS x86_64, macOS arm64, and Windows x86_64 MSVC, plus a CycloneDX JSON SBOM. Each archive contains the binary, `README.md`, and `LICENSE`. Verify the downloaded archive and SBOM against `SHA256SUMS` and their GitHub build-provenance attestations before use:

```bash
set -euo pipefail
version=vX.Y.Z
archive="dext-${version}-x86_64-unknown-linux-gnu.tar.gz"
gh release download "$version" --repo SiliconState/Dext \
  --pattern "$archive" --pattern dext.cdx.json --pattern SHA256SUMS
awk -v archive="$archive" '
  $2 == archive { print; archive_found++ }
  $2 == "dext.cdx.json" { print; sbom_found++ }
  END { if (archive_found != 1 || sbom_found != 1) exit 1 }
' SHA256SUMS > selected-SHA256SUMS
sha256sum --check selected-SHA256SUMS
rm selected-SHA256SUMS
gh attestation verify "$archive" --repo SiliconState/Dext
gh attestation verify dext.cdx.json --repo SiliconState/Dext
```

Use the matching target archive for other platforms. macOS can replace the checksum command with `shasum -a 256 -c selected-SHA256SUMS`; Windows can compare both `Get-FileHash -Algorithm SHA256` values with `SHA256SUMS`. Windows shell tools require a real Bash implementation such as Git for Windows; Dext skips the Windows/WSL app alias and selects `bash.exe` from `PATH`, or uses the explicit `DEXT_BASH_PATH` override. See [`docs/RELEASING.md`](docs/RELEASING.md) for owner and verification details.

Then run:

```bash
dext --help
dext
```

For local development without installing:

```bash
cargo run -- --help
```

## Quick start

Authenticate a provider:

```bash
dext auth providers
dext auth login chatgpt          # ChatGPT/Codex OAuth
dext auth login glm <api-key>    # ZAI GLM key
dext auth login openai <api-key> # OpenAI Platform key
dext auth login anthropic <api-key>
dext auth login kimi              # Open Kimi Code console, then paste the coding-plan API key
dext auth login kimi <api-key>    # Or store the Kimi Code API key directly
dext auth login deepseek <api-key>
# Local llama.cpp/Qwen on 127.0.0.1:8080, no key. Start one local model service first.
dext auth provider local
# Dext uses the server's live context window and accepts its configured model alias.
```

Start an interactive session:

```bash
dext
```

Run a one-shot task:

```bash
dext "summarize this repository"
```

Read a prompt from stdin:

```bash
printf 'explain Cargo.toml\n' | dext -p
```

Use low-token mode (local providers choose `frugal` automatically unless you explicitly select a mode):

```bash
dext --frugal --effort off
# or inside Dext:
/context frugal
/effort off
```

The default provider-visible toolset hides specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`). Use `dext --toolset full`, `DEXT_TOOLSET=full`, or `/tools full` only when you need the complete catalog; non-JSON startup prints `[tools] toolset full` whenever the full catalog is selected, including in frugal mode. Static tools and active pack-runtime tools share one provider-neutral `{name, description, schema}` layer before Anthropic, OpenAI Chat Completions, OpenAI Responses, or ChatGPT Responses wrapping; the selected lean/full schema profile applies to both. Frugal mode preserves explicit toolset/schema selections while applying smaller context/result/read-capture budgets. `/context` changes those live caps for subsequent native reads and tool results; an explicit context choice remains pinned across provider switches and session restore, while automatic defaults follow the active local/cloud provider. A valid CLI context mode overrides `DEXT_CONTEXT_MODE`. The retired `tiny` mode and `--tiny` alias are rejected rather than silently mapped to frugal.

## Common commands

CLI:

```bash
dext --resume
dext --seat planner
dext --seat planner --resume
dext seat list
dext seat show planner
dext seat set planner --label "Planning role"
dext seat set planner --summary-file ./planner-context.txt
dext --fork
dext sessions
dext session brief latest
dext session export latest html dext-session.html
dext session analyze latest
dext session prune --days=7
dext doctor
dext doctor --approval auto-write --sandbox read-only --cd /path/to/project
dext undo --list
dext pack create personal/my-pack
dext pack inspect my-pack
dext pack run my-pack "task description"
dext --eval
```

A **Seat** is a durable project-scoped agent identity; a session is one disposable incarnation. `--seat NAME` starts a new session associated with the Seat, while `--seat NAME --resume` resumes its latest durable session. Seat ids are portable lowercase 1–128 byte path components using ASCII letters, digits, `-`, `_`, and `.`; trailing dots and Windows device names are rejected. Selecting a new Seat does not create an empty record: persistence begins with the first successful durable session save or explicit `dext seat set`. Use `dext seat set NAME --label TEXT`, `--summary-file PATH|-`, `--clear-label`, and `--clear-summary` to maintain bounded context without placing summaries directly in process arguments. Unseated writes remain format v3 for backward compatibility; seated writes use v4 so older binaries reject rather than silently ignore identity. Valid transitional v3 Seat headers remain loadable and validated, then upgrade on the next seated save. Explicit resume rejects a different Seat id, the same Seat name from another project, malformed or unprovenanced metadata, and headers over 256 KiB before applying saved state. `--no-session --seat NAME` supplies identity context without creating or updating durable state. Prompt-visible label/summary context is privacy-redacted and encoded as one JSON data line with an explicit non-authority note. Changing projects clears the active Seat. `/reset` updates the matching pointer and removes the transcript under one state-operation guard; on deletion failure it preserves history and attempts to restore the pointer.

Crew maps directly portable role names to read-only contextual Seats such as `crew.reviewer`. Existing valid custom role names that cannot fit the portable Seat grammar receive a deterministic `crew.agent-<hash>` identity, preserving crew's prior role-name compatibility. Children retain `--no-session`, so the trusted parent remains authoritative and child runs do not advance durable pointers. Project-scoped roles can load an existing summary; isolated roles intentionally use their private temporary project's context. Captured, detached-systemd, and tmux-pane workers pin the same effective absolute Dext state root, preventing stale supervisor environments from selecting a different Seat store.

Interactive slash commands:

```text
/help
/providers
/provider chatgpt
/models all
/login chatgpt
/model local/qwen3.6-35b-a3b-mtp-ud-q5_k_m
/approval ask|auto-read|auto-write|never|always
/sandbox-profile read-only|workspace-write|danger-full-access
/context standard|frugal
/tools default|full
/tool-profile lean|full     # default is lean
/preview off|simple|git
/undo --list
/undo
/compact status
/pack list
/pack create personal/my-pack
/pack inspect my-pack
/pack run my-pack task description
/save name
/export html path
/sessions analyze|brief|grep|failures|verify-log|decisions
```


## Safety diagnostics

`dext doctor` renders concise `ok`, `info`, and `warn` findings for the effective approval profile and source, sandbox profile and kernel enforcement, provider/auth integrity, auth-file permissions, the bounded latest session/todo/settings/tool-journal state, and Git checkpoint availability. Optional `--approval`, `--sandbox`, and `--cd` arguments show the posture that those explicit startup choices would produce.

Doctor is observational: it does not repair or rewrite state, resolve environment or `!command` credential references, contact model/local-provider endpoints, or print credential-bearing JSON. Warnings are counted in the report but retain the existing exit status 0 so optional findings do not break scripts.

## Persistent local services

Dext treats each bash call as atomic: commands run in a dedicated Unix session/process group or a kill-on-close Windows Job Object, and the complete child process tree is cleaned up after the shell exits, times out, or is interrupted. Do not expect `cmd &`, `nohup`, or `disown` to keep servers alive across tool calls. Unix `setsid`-style detaches are unsupported because they escape process-group cleanup.

Prefer static files or one-shot commands when possible. If the user explicitly needs a long-lived local service, use the host OS supervisor and clean it up when finished. On Linux with systemd:

```bash
systemd-run --user --unit=dext-preview --same-dir python3 -m http.server 8000
systemctl --user status dext-preview
journalctl --user-unit dext-preview -n 100 --no-pager
systemctl --user stop dext-preview
```

Use `dext-` prefixes for agent-started units so they are easy to inspect and stop (`systemctl --user list-units 'dext-*'`). On macOS/Windows or Linux without systemd, use the platform's native supervisor if needed; otherwise avoid persistent background processes.

## Git safety

Dext creates lightweight Git checkpoints before approved write-risk tool calls
when the sandbox root is inside a Git repository, with direct file mutations
receiving path-specific restore hints. Checkpoints live under hidden refs
(`refs/dext/checkpoints/`) with private local manifests in `.dext/checkpoints/`.
Checkpoint files use owner-only permissions on Unix, `.dext/` is added to the
repository-local Git exclude file, and automatic retention keeps at most 20
checkpoints for no longer than seven days. They are best-effort safety snapshots
for Dext edits; they do not replace normal commits, and they do not cover
arbitrary external side effects. Do not mirror-push `refs/dext/*`.

Use undo commands to inspect or restore checkpointed paths:

```bash
dext undo --list
dext undo --preview <checkpoint-id>
dext undo --apply <checkpoint-id>
dext undo --prune
```

In an interactive session:

```text
/undo --list
/undo                 # preview latest checkpoint
/undo --apply         # restore latest checkpointed worktree paths
/undo <id>
/undo <id> --apply
/undo --prune
```

Normal undo restores worktree paths and never silently moves `HEAD`. The CLI's
explicit `--reset-head` mode is only for cases where you intentionally want a
checkpoint to move the current branch state.

Mutation previews show capped diffs for direct file-writing tools before an
approval prompt:

```bash
dext --preview off|simple|git
DEXT_MUTATION_PREVIEW=simple
```

Inside Dext:

```text
/preview              # status
/preview simple
/preview off
```

`git` preview mode is accepted for forward compatibility and currently falls
back to simple in-memory previews.

### Optional prompt recall

Dext auto-injects tracked `DEXT.md` guidance and an optional ignored `recall.md`
when those files are present in the sandbox ancestry. It does not create or
update either file automatically.

## Packs

Packs are Dext's modular “battery packs”: source-first workflows that add specialized behavior without bloating the core binary. A reviewed pack may optionally add a bounded executable `runtime.json` helper that exposes dynamic tools only while the pack is active. Dext owns the create, discover, inspect, maintain, and run lifecycle but ships no pack content. Every pack lives inside a shelf at `<shelf>/packs/<name>`.

Create a reusable user pack, then edit and exercise it:

```bash
dext pack create personal/my-pack
dext pack inspect my-pack
dext pack run my-pack "test task"
```

Use `dext pack create local/my-pack --project` for an explicitly project-local
pack under `.dext/shelves/`. Dext discovers project shelves,
`DEXT_SHELVES_DIR`, and user `~/.dext/shelves`; direct `packs/`, `.dext/packs`,
`~/.dext/packs`, and `DEXT_PACKS_DIR` roots are not supported. Shelf repositories
remain separate from Dext and should be reviewed and maintained on their own.

Inside a session, use `/pack create`, `/pack list`, `/pack inspect`, and `/pack run`. Selected packs continue to use normal Dext tools, approvals, sandboxing, hooks, and helper environment variables. Optional executable runtimes are one-shot and credential-scrubbed; pin approved executable bytes, enforce the implemented recursive-schema subset and per-event confinement, preserve the full bounded protocol response, apply state/effects/continuations atomically, and persist bounded runtime state plus queued continuations. Policy changes revoke active runtimes and queued callbacks; resume preflights exact canonical pack-directory/source identity, manifest/hash/state accounting, project trust, and executable approval before applying saved session fields. Declared write/danger tools retain normal approval, journaling, and Git checkpoints, and active dynamic tools participate in `/allow`, `/revoke`, and `/allowed`. See [`docs/PACKS.md`](docs/PACKS.md) for the complete authoring, runtime protocol, and maintenance contract.

## Configuration and state

Dext loads optional dotenv settings only from the user-owned state file `~/.dext/.env` (or `$DEXT_HOME/.env`), never from a project directory or its parents. Keep project `.env` files for the project itself; Dext deliberately ignores them so a repository cannot change approval, sandbox, privacy, or credential-inheritance policy. Prefer `dext auth login ...` for provider credentials. Provider credentials loaded from the user state dotenv or parent shell are removed from agent-run subprocess environments by default; set `DEXT_INHERIT_TOOL_CREDENTIALS=1` only when a trusted model-invoked tool explicitly needs them. Hooks and Dext-owned subprocesses remain scrubbed even with that opt-in. Privacy redaction is on by default while user-readable files remain readable. Set `DEXT_PRIVACY=strict` or use `/privacy strict` to block sensitive-looking native read paths; set `DEXT_PRIVACY=0` or use `/privacy off` only when raw, unredacted tool output is intentionally required.

Privacy redaction replaces private-key blocks, real secret assignments, and explicitly labeled SSNs, payment-card numbers, and account identifiers before tool results enter model context or session logs. Ordinary unlabeled long numbers and decimal market/HTTP values are not classified as cards. A compact `[privacy: redacted ...; raw values withheld]` note appears only when a value was actually replaced.

Useful environment variables:

```bash
DEXT_PROVIDER=local
DEXT_MODEL=qwen3.6-35b-a3b-mtp-ud-q5_k_m
DEXT_BASE_URL=http://127.0.0.1:8080
DEXT_THINKING_EFFORT=off
# Experimental: force well-formed tool calls on local llama.cpp (opt-in).
# Constrains generation with a GBNF grammar so small models cannot emit a
# tool call with empty/dropped arguments. Off by default; validate against
# your llama.cpp build before relying on it, as it forces a tool call.
DEXT_LLAMA_TOOL_GRAMMAR=1
# or cloud:
DEXT_PROVIDER=chatgpt
DEXT_MODEL=gpt-5.3-codex
DEXT_BASE_URL=https://example.test/v1
DEXT_API_KEY=...
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
KIMI_API_KEY=...  # Kimi Code coding-plan key from https://www.kimi.com/code/console; MOONSHOT_API_KEY is separate.
OPENAI_API_KEY=...
DEEPSEEK_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
DEXT_HOME=~/.dext
DEXT_SESSIONS_DIR=~/.dext/sessions
DEXT_LOGS_DIR=~/.dext/logs
DEXT_APPROVAL=ask  # default; prompts interactively and denies gated tools in non-interactive runs
DEXT_TRUST=1  # explicit alias for DEXT_APPROVAL=always
DEXT_PRIVACY=1  # default: redact detected secrets while keeping user-readable files readable
# DEXT_PRIVACY=strict  # additionally block sensitive-looking native read paths
# Explicit high-trust opt-in for model-invoked bash/external tools; hooks and
# Dext-owned subprocesses always remove *_API_KEY and other credential variables.
# DEXT_INHERIT_TOOL_CREDENTIALS=1
DEXT_SANDBOX_PROFILE=workspace-write  # writes only in sandbox, scratch, and common toolchain caches
# Built-in http tool only: trusted-network opt-ins (all off by default).
# This client connects directly and ignores proxy environment variables so its
# destination DNS/IP checks cannot be delegated to a proxy.
# DEXT_HTTP_ALLOW_LOOPBACK=1
# DEXT_HTTP_ALLOW_PRIVATE=1
# DEXT_HTTP_ALLOW_LINK_LOCAL=1
DEXT_CONTEXT_MODE=standard
DEXT_TOOLSET=default
DEXT_TOOL_PROFILE=lean
DEXT_MUTATION_PREVIEW=simple
DEXT_BUDGET_CAP=$5
# Optional pricing overrides in USD per million tokens:
DEXT_INPUT_USD_PER_MTOK=1
DEXT_OUTPUT_USD_PER_MTOK=5
DEXT_CACHE_READ_USD_PER_MTOK=0.1
DEXT_CACHE_CREATE_USD_PER_MTOK=1.25
```

Runtime state and credentials live outside version control. Project-local runtime artifacts such as `.dext/`, `.auto/log.jsonl`, `target/`, `.env`, `DEXT.todo.json`, and `dext-session-*` exports are ignored; reproducible autoresearch workflow files under `.auto/` may remain tracked.

Usage metrics are recorded in session headers and `/usage` after provider turns. Cloud providers use returned usage objects when available; OpenAI-compatible streaming requests ask for usage chunks, while local llama.cpp derives exact prompt/cache/output counts from streamed `timings` and records zero dollar cost unless pricing env overrides are set.

## Development

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
cargo install --path . --force --locked
```

Use the complete renderer gate after changing `src/tui.rs`, terminal dependencies, or the vendored compatibility patch. See [`docs/TUI.md`](docs/TUI.md) for the inline-terminal behavior contract and patch maintenance procedure.

Run the final full suite and install directly in a trusted host terminal. Dext's default `workspace-write` sandbox intentionally blocks shared `/tmp`, arbitrary pseudo-terminals, and Cargo install metadata outside approved cache roots. A release gate invoked through a confined Dext `bash` tool may therefore report cascading temp-directory failures, deny every TUI smoke test, and fail to write `~/.cargo/.crates.toml`. To orchestrate the gate with Dext, launch a separate controlled process with `dext --sandbox-profile danger-full-access --approval always`; changing an environment variable inside the confined shell cannot relax its parent kernel sandbox. Do not weaken the default sandbox to make self-hosted tests pass. See [`docs/RELEASING.md`](docs/RELEASING.md).

## Repository map

- `src/main.rs` — agent facade, CLI, turn orchestration, and remaining tool execution adapters.
- `src/git_checkpoints.rs` — Git-native pre-mutation checkpoints, hidden recovery refs, undo preview/apply support.
- `src/mutation_preview.rs` — capped in-memory diffs for direct file-tool approval prompts.
- `src/provider.rs` — provider catalog, auth, OAuth/API-key handling, request shaping, transport deadlines/body bounds, and side-effect-free bounded state/auth-permission inspection.
- `src/sandbox.rs` — OS confinement, profile-specific write roots, private scratch, and offline diagnostic isolation.
- `src/session.rs` — session persistence, project state paths, logs, lock cleanup, terminal restore.
- `src/seats.rs` — project-scoped durable agent identity records and seat-specific session lookup.
- `src/tools.rs` — tool catalog and provider-facing tool schemas.
- `src/tool_policy.rs` — validation and command/external-source guardrails.
- `src/sse.rs` — bounded SSE framing shared by runtime and Criterion benchmarks.
- `src/streaming.rs` — provider event validation and stream/tool-call assembly.
- `src/tool_round.rs` — tool-call planning, approval, checkpoint/journal boundaries, dispatch, and result normalization.
- `src/tool_journal.rs` — bounded private side-effect start/terminal journal and recovery metadata.
- `deny.toml` — dependency-license allowlist enforced by security and release workflows.
- `.github/workflows/release.yml` — tag/version gate, four-platform release builds, sorted checksums, SBOM, attestations, and GitHub release publication.
- `docs/RELEASING.md` — owner release checklist and asset verification.
- `src/orchestrator.rs` — turn telemetry, dedupe, circuit-breaker, and workflow guards.
- `src/tui.rs` — inline terminal UI.
- `vendor/ratatui-core/` — exact upstream source plus Dext's narrow inline-terminal compatibility patch.
- `src/packs.rs` — pack discovery, loading, and invocation.
- `src/shelves.rs` — shelf registry with typed manifests and abilities.
- `tests/` — integration tests and replay fixtures.
- `benches/` — criterion benchmarks.
- `DEXT.md` — tracked, auto-injected machine-facing project guidance.
- `recall.md` — optional ignored prompt cache; injected only when present.
- `docs/PACKS.md` — packs and shelves reference.
- `docs/TUI.md` — inline TUI contract, dependency stack, compatibility patch, and regression gate.
- `docs/index.html` — canonical browsable technical documentation; update it in the same change as runtime, architecture, security, provider, tool, test, CI, or release behavior.
- `docs/RISK_REGISTER.md` — open non-documentation risks, controls, owners, and review triggers.

More detail: the canonical [`docs/index.html`](docs/index.html), [`docs/RISK_REGISTER.md`](docs/RISK_REGISTER.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/USAGE.md`](docs/USAGE.md), [`docs/TUI.md`](docs/TUI.md), [`SECURITY.md`](SECURITY.md), and [`CONTRIBUTING.md`](CONTRIBUTING.md).
