# Dext

Dext is a single-binary Rust coding agent that runs from your terminal, keeps project-scoped session state, and gives the model a small set of native tools for filesystem, search, shell/data, HTTP, Git, and task tracking.

Dext is source-first: prompts, runtime state, tool policies, provider wiring, and the TUI live in this repository with no external service required beyond whichever model provider you authenticate.

## Highlights

- Interactive terminal agent with an inline ratatui TUI; no alternate-screen takeover.
- One-shot mode for scripted tasks and JSON/stream-JSON output for automation.
- Provider catalog with built-in GLM and ChatGPT/Codex profiles plus env/catalog overrides.
- OAuth/API-key auth flows stored outside the repo under Dext state directories.
- Lean default tool schemas with optional full schemas and frugal context mode.
- Project-scoped latest sessions, named session export/analyze/grep/failure/verification helpers.
- Permission and sandbox profiles for read-only, workspace-write, and explicit danger modes.
- Eval harness, release tests, and a PTY-backed TUI smoke test.
- Git-native safety helpers: pre-mutation checkpoints, `/undo`/`dext undo`, mutation previews, and explicit memory-file merge-driver registration.

## Install

Requires Rust stable with edition 2024 support.

```bash
git clone https://github.com/SiliconState/Dext.git
cd Dext
cargo install --path . --force
```

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
dext auth login deepseek <api-key>
dext auth provider local        # local llama.cpp/Qwen on 127.0.0.1:8080, no key
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

Use low-token mode:

```bash
dext --frugal --effort off
# even smaller local mode:
dext --context-mode tiny --effort off
# or inside Dext:
/context tiny
/effort off
```

## Common commands

CLI:

```bash
dext --resume
dext --fork
dext sessions
dext session export latest html dext-session.html
dext session analyze latest
dext undo --list
dext memory check
dext pack list
dext pack inspect autoresearch
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
/model local/qwen-local
/approval ask|auto-read|auto-write|never|always
/sandbox-profile read-only|workspace-write|danger-full-access
/context standard|frugal|tiny
/tool-profile lean|full
/preview off|simple|git
/undo --list
/undo
/compact status
/pack list
/pack inspect autoresearch
/pack run autoresearch optimize the benchmark in this repo
/save name
/export html path
/sessions analyze|grep|failures|verify-log|decisions
```


## Git safety and memory merging

Dext creates lightweight Git checkpoints before approved write-risk tool calls
when the sandbox root is inside a Git repository, with direct file mutations
receiving path-specific restore hints. Checkpoints live under hidden refs
(`refs/dext/checkpoints/`) with local manifests in `.dext/checkpoints/`.
They are best-effort safety snapshots for Dext edits; they do not replace normal
commits, and they do not cover arbitrary external side effects.

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

Packs are source-first workflow bundles with a `PACK.md`. Dext discovers packs from `DEXT_PACK_<NAME>_DIR`, project `.dext/shelves/<shelf>/packs`, `.dext/packs`, `packs`, `DEXT_SHELVES_DIR`, `DEXT_PACKS_DIR`, user `~/.dext/shelves/<shelf>/packs`, `~/.dext/packs`, and bundled repository packs. Shelf packs take precedence over same-named legacy project/user pack directories within the same scope. Typed shelf manifests named `shelf.json` are loaded from the same shelf roots into the runtime `ShelfRegistry` and exposed in prompt context plus `/shelves` / `dext shelves` as provider-neutral ability metadata.

The stable extension contract is intentionally provider-neutral: a pack is files plus instructions that any LLM/provider can read, with optional shell helpers and `phooks.json` steering. Scaffold a pack as `packs/<name>/PACK.md`, keep helper scripts inside that directory, and validate it with `dext pack inspect <name>` plus a real `dext pack run <name> ...` on a disposable task. Shared shelves live at `<shelf>/packs/<pack>` and can be distributed through project, user, or `DEXT_SHELVES_DIR` paths without adding provider-visible tools. `shelf.json` manifests add typed ability metadata for internal registry resolution; the runtime lists and prompt-injects those records without expanding the provider-visible tool list.

Invoke a pack conversationally:

```text
run autoresearch on improving this project benchmark
```

Or invoke it explicitly:

```text
/pack run autoresearch improve this benchmark
```

CLI equivalents:

```bash
dext pack list
dext pack inspect autoresearch
dext pack run autoresearch "improve this benchmark"
dext --pack autoresearch "improve this benchmark"
```

If a pack has `phooks.json`, Dext activates those hook templates for that invocation and passes `DEXT_PACK_DIR` plus `DEXT_PACK_<NAME>_DIR` to hook processes. Shelf packs use the same invocation path and remain regular source directories.

## Configuration and state

Dext reads `.env` for local development if present, but `.env` is ignored and must never be committed. Prefer `dext auth login ...` for credentials.

Useful environment variables:

```bash
DEXT_PROVIDER=local
DEXT_MODEL=qwen-local
DEXT_BASE_URL=http://127.0.0.1:8080
DEXT_THINKING_EFFORT=off
# or cloud:
DEXT_PROVIDER=chatgpt
DEXT_MODEL=gpt-5.3-codex
DEXT_BASE_URL=https://example.test/v1
DEXT_API_KEY=...
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...
DEEPSEEK_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
DEXT_HOME=~/.dext
DEXT_SESSIONS_DIR=~/.dext/sessions
DEXT_LOGS_DIR=~/.dext/logs
DEXT_APPROVAL=ask
DEXT_SANDBOX_PROFILE=workspace-write
DEXT_CONTEXT_MODE=standard
DEXT_TOOL_PROFILE=lean
DEXT_MUTATION_PREVIEW=simple
DEXT_BUDGET_CAP=$5
```

Runtime state and credentials live outside version control. Project-local runtime directories such as `.dext/`, `target/`, `.env`, `DEXT.todo.json`, and `dext-session-*` exports are ignored.

## Development

```bash
cargo fmt
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
cargo install --path . --force
```

Use the TUI smoke test after changing `src/tui.rs`.

## Repository map

- `src/main.rs` — agent loop, tool execution, CLI, slash commands, evals, orchestration.
- `src/git_checkpoints.rs` — Git-native pre-mutation checkpoints, hidden recovery refs, undo preview/apply support.
- `src/mutation_preview.rs` — capped in-memory diffs for direct file-tool approval prompts.
- `src/memory_merge.rs` — explicit Git merge-driver helpers for `MEMORY.md` and `recall.md`.
- `src/provider.rs` — provider catalog, auth, OAuth/API-key handling, request shaping.
- `src/session.rs` — session persistence, project state paths, logs, lock cleanup, terminal restore.
- `src/tools.rs` — tool catalog and provider-facing tool schemas.
- `src/tool_policy.rs` — validation and command/external-source guardrails.
- `src/orchestrator.rs` — turn telemetry, dedupe, circuit-breaker, and workflow guards.
- `src/tui.rs` — inline terminal UI.
- `src/packs.rs` — pack discovery, loading, and invocation.
- `src/shelves.rs` — shelf registry with typed manifests and abilities.
- `tests/` — integration tests and replay fixtures.
- `benches/` — criterion benchmarks.
- `DEXT.md` / `recall.md` — prompt-facing project guidance and recall for Dext working on itself.
- `MEMORY.md` — durable project memory.
- `docs/PACKS.md` — packs and shelves reference.

More detail: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/USAGE.md`](docs/USAGE.md), [`SECURITY.md`](SECURITY.md), and [`CONTRIBUTING.md`](CONTRIBUTING.md).
