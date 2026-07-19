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

Versioned GitHub releases also provide tested archives for Linux x86_64 GNU, macOS x86_64, macOS arm64, and Windows x86_64 MSVC. Each archive contains the binary, `README.md`, and `LICENSE`. Verify the downloaded archive against `SHA256SUMS` and its GitHub build-provenance attestation before running it:

```bash
version=vX.Y.Z
archive="dext-${version}-x86_64-unknown-linux-gnu.tar.gz"
gh release download "$version" --repo SiliconState/Dext \
  --pattern "$archive" --pattern SHA256SUMS
grep "  ${archive}$" SHA256SUMS | sha256sum --check -
gh attestation verify "$archive" --repo SiliconState/Dext
```

Use the matching target archive for other platforms. macOS can replace the checksum command with `shasum -a 256 -c -`; Windows can compare `Get-FileHash -Algorithm SHA256` with `SHA256SUMS`. Windows shell tools require a real Bash implementation such as Git for Windows; Dext skips the Windows/WSL app alias and selects `bash.exe` from `PATH`, or uses the explicit `DEXT_BASH_PATH` override. See [`docs/RELEASING.md`](docs/RELEASING.md) for owner and verification details.

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
# even smaller local mode:
dext --context-mode tiny --effort off
# or inside Dext:
/context tiny
/effort off
```

The default provider-visible toolset hides specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`). Use `dext --toolset full`, `DEXT_TOOLSET=full`, or `/tools full` only when you need the complete catalog.

## Common commands

CLI:

```bash
dext --resume
dext --fork
dext sessions
dext session export latest html dext-session.html
dext session analyze latest
dext doctor
dext doctor --approval auto-write --sandbox read-only --cd /path/to/project
dext undo --list
dext memory check
dext pack list
dext pack inspect autoresearch
# or inspect SkillOpt-style pack/skill optimization
dext pack inspect packopt
# Browser automation is a bundled pack, not a provider-visible tool:
dext pack inspect agent-browser
dext pack run agent-browser "inspect https://example.com"
dext pack run autoresearch "optimize the benchmark in this repo"
dext --eval
```

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
/context standard|frugal|tiny
/tools default|full
/tool-profile lean|full     # default is lean
/preview off|simple|git
/undo --list
/undo
/compact status
/pack list
/pack inspect autoresearch
/pack inspect packopt
/pack agent-browser inspect https://example.com
/pack run autoresearch optimize the benchmark in this repo
/save name
/export html path
/sessions analyze|grep|failures|verify-log|decisions
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

## Git safety and memory merging

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

Dext can also register section-aware Git merge drivers for its memory files:

```bash
dext memory check
dext memory register
dext memory unregister
# used by Git after registration:
dext memory merge [--recall] <base> <ours> <theirs> [marker-size] [path]
```

Registration is local-only by default: it writes repository-local Git config and
local attributes. Use `dext memory register --versioned-attributes` only when you
want `.gitattributes` entries committed for the project. `dext memory merge` is
the Git merge-driver entry point and is not normally run by hand. The merge
driver covers `MEMORY.md` and `recall.md` and is explicit; Dext does not
silently edit Git config or attributes during normal agent runs.

## Packs

See [`docs/PACKS.md`](docs/PACKS.md) for the full packs and shelves reference, including structure, discovery, building, and distributing packs.

Packs are source-first workflow bundles with a `PACK.md`. The bundled `agent-browser` pack supplies browser automation through the normal `bash` approval/sandbox path without adding a provider-visible browser tool. Runtime-essential bundled pack files are embedded in the binary and materialized into a content-addressed `$DEXT_HOME/bundled-packs/` cache for helper/hook execution. Existing `DEXT_HOME` ownership/write safety is validated without changing its mode; cache descendants are owner-private on Unix, reject symlink components, and repair bytes/modes from the binary. Cache failures are surfaced without hiding project/user packs. Dext otherwise discovers packs from `DEXT_PACK_<NAME>_DIR`, project `.dext/shelves/<shelf>/packs`, `.dext/packs`, `packs`, `DEXT_SHELVES_DIR`, `DEXT_PACKS_DIR`, user `~/.dext/shelves/<shelf>/packs`, `~/.dext/packs`, and embedded bundled packs. Shelf packs take precedence over same-named legacy project/user pack directories within the same scope. Typed shelf manifests named `shelf.json` are loaded from the same shelf roots into the runtime `ShelfRegistry` and exposed in prompt context plus `/shelves` / `dext shelves` as provider-neutral ability metadata.

The default installation target for reusable packs is user-global Dext scope: `~/.dext/packs/<name>/` for legacy packs or `~/.dext/shelves/<shelf>/packs/<name>/` for shelf packs. Use project-local `packs/<name>/`, `.dext/packs/<name>/`, or `.dext/shelves/<shelf>/packs/<name>/` only when the user explicitly asks for repo-scoped behavior.

The stable extension contract is intentionally provider-neutral: a pack is files plus instructions that any LLM/provider can read, with optional shell helpers and `phooks.json` steering. Scaffold reusable packs in user-global Dext scope by default (`~/.dext/packs/<name>/PACK.md` or `~/.dext/shelves/<shelf>/packs/<name>/PACK.md`), keep helper scripts inside that directory, and validate with `dext pack inspect <name>` plus a real `dext pack run <name> ...` on a disposable task. Use project-local `packs/<name>/PACK.md` or `.dext/...` variants only when the user explicitly wants the pack tied to one repository. Use `packopt` for SkillOpt-style improvement of pack/skill documents with bounded edits, strict held-out validation, and rejected-edit memory. Shared shelves live at `<shelf>/packs/<pack>` and can be distributed through project, user, or `DEXT_SHELVES_DIR` paths without adding provider-visible tools. `shelf.json` manifests add typed ability metadata for internal registry resolution; the runtime lists and prompt-injects those records without expanding the provider-visible tool list.

Invoke a pack conversationally:

```text
run autoresearch on improving this project benchmark
run packopt on improving ~/.dext/packs/autoresearch/PACK.md against held-out tasks
```

Or invoke it explicitly:

```text
/pack run autoresearch improve this benchmark
/pack run packopt improve ~/.dext/packs/autoresearch/PACK.md against held-out tasks
```

CLI equivalents:

```bash
dext pack list
dext pack inspect autoresearch
dext pack inspect packopt
dext pack inspect agent-browser
dext pack run agent-browser "inspect https://example.com"
dext pack run autoresearch "improve this benchmark"
dext pack run packopt "improve ~/.dext/packs/autoresearch/PACK.md against held-out tasks"
dext --pack autoresearch "improve this benchmark"
dext --pack packopt "improve ~/.dext/packs/autoresearch/PACK.md against held-out tasks"
```

If a pack has `phooks.json`, Dext activates those hook templates for the current session. While the pack is active, Dext passes `DEXT_PACK_DIR` plus `DEXT_PACK_<NAME>_DIR` to its `bash` tool commands and hook processes. An environment-selected, user-global, or bundled pack may declare exact credential names in `credential-env`; inherited values are exposed only to a simple direct invocation of that active pack's own native `bin/` helper, never hooks or arbitrary shell commands, and provider-auth names remain excluded. On Windows, only `.exe`/`.com` helpers qualify for direct credential inheritance; script helpers use Bash with declared credentials removed. Project-local declarations are ignored and reported by `pack inspect`, so repository content cannot enable parent credential inheritance. Shelf packs use the same invocation path and remain regular source directories.

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

Runtime state and credentials live outside version control. Project-local runtime directories such as `.dext/`, `target/`, `.env`, `DEXT.todo.json`, `autoresearch.*`, `packopt.*`, and `dext-session-*` exports are ignored.

Usage metrics are recorded in session headers and `/usage` after provider turns. Cloud providers use returned usage objects when available; OpenAI-compatible streaming requests ask for usage chunks, while local llama.cpp derives exact prompt/cache/output counts from streamed `timings` and records zero dollar cost unless pricing env overrides are set.

## Development

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
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
- `src/memory_merge.rs` — explicit Git merge-driver helpers for `MEMORY.md` and `recall.md`.
- `src/provider.rs` — provider catalog, auth, OAuth/API-key handling, request shaping, transport deadlines/body bounds, and side-effect-free bounded state/auth-permission inspection.
- `src/session.rs` — session persistence, project state paths, logs, lock cleanup, terminal restore.
- `src/tools.rs` — tool catalog and provider-facing tool schemas.
- `src/tool_policy.rs` — validation and command/external-source guardrails.
- `src/sse.rs` — bounded SSE framing shared by runtime and Criterion benchmarks.
- `src/streaming.rs` — provider event validation and stream/tool-call assembly.
- `src/tool_round.rs` — tool-call planning, approval, checkpoint/journal boundaries, dispatch, and result normalization.
- `src/tool_journal.rs` — bounded private side-effect start/terminal journal and recovery metadata.
- `.github/workflows/release.yml` — tag/version gate, four-platform release builds, checksums, attestations, and GitHub release publication.
- `docs/RELEASING.md` — owner release checklist and archive verification.
- `src/orchestrator.rs` — turn telemetry, dedupe, circuit-breaker, and workflow guards.
- `src/tui.rs` — inline terminal UI.
- `vendor/ratatui-core/` — exact upstream source plus Dext's narrow inline-terminal compatibility patch.
- `src/packs.rs` — pack discovery, loading, and invocation.
- `src/shelves.rs` — shelf registry with typed manifests and abilities.
- `tests/` — integration tests and replay fixtures.
- `benches/` — criterion benchmarks.
- `DEXT.md` / `recall.md` — prompt-facing project guidance and recall for Dext working on itself.
- `MEMORY.md` — durable project memory.
- `docs/PACKS.md` — packs and shelves reference.
- `docs/TUI.md` — inline TUI contract, dependency stack, compatibility patch, and regression gate.
- `docs/index.html` — canonical browsable technical documentation; update it in the same change as runtime, architecture, security, provider, tool, test, CI, or release behavior.
- `docs/RISK_REGISTER.md` — open non-documentation risks, controls, owners, and review triggers.

More detail: the canonical [`docs/index.html`](docs/index.html), [`docs/RISK_REGISTER.md`](docs/RISK_REGISTER.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/USAGE.md`](docs/USAGE.md), [`docs/TUI.md`](docs/TUI.md), [`SECURITY.md`](SECURITY.md), and [`CONTRIBUTING.md`](CONTRIBUTING.md).
