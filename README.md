# Wolf

Wolf is a single-binary Rust coding agent that runs from your terminal, keeps project-scoped session state, and gives the model a small set of native tools for filesystem, search, shell/data, HTTP, Git, and task tracking.

Wolf is source-first: prompts, runtime state, tool policies, provider wiring, and the TUI live in this repository with no external service required beyond whichever model provider you authenticate.

## Highlights

- Interactive terminal agent with an inline ratatui TUI; no alternate-screen takeover.
- One-shot mode for scripted tasks and JSON/stream-JSON output for automation.
- Provider catalog with built-in GLM and ChatGPT/Codex profiles plus env/catalog overrides.
- OAuth/API-key auth flows stored outside the repo under Wolf state directories.
- Lean default tool schemas with optional full schemas and frugal context mode.
- Project-scoped latest sessions, named session export/analyze/grep/failure/verification helpers.
- Permission and sandbox profiles for read-only, workspace-write, and explicit danger modes.
- Eval harness, release tests, and a PTY-backed TUI smoke test.

## Install

Requires Rust stable with edition 2024 support.

```bash
git clone https://github.com/SiliconState/Wolf.git
cd Wolf
cargo install --path . --force
```

Then run:

```bash
wolf --help
wolf
```

For local development without installing:

```bash
cargo run -- --help
```

## Quick start

Authenticate a provider:

```bash
wolf auth providers
wolf auth login chatgpt          # ChatGPT/Codex OAuth
wolf auth login glm <api-key>    # ZAI GLM key
wolf auth login openai <api-key> # OpenAI Platform key
wolf auth login anthropic <api-key>
wolf auth login deepseek <api-key>
```

Start an interactive session:

```bash
wolf
```

Run a one-shot task:

```bash
wolf "summarize this repository"
```

Read a prompt from stdin:

```bash
printf 'explain Cargo.toml\n' | wolf -p
```

Use low-token mode:

```bash
wolf --frugal
# or inside Wolf:
/context frugal
```

## Common commands

CLI:

```bash
wolf --resume
wolf --fork
wolf sessions
wolf session export latest html wolf-session.html
wolf session analyze latest
wolf --eval
```

Interactive slash commands:

```text
/help
/providers
/provider chatgpt
/models all
/login chatgpt
/model gpt-5.3-codex
/approval ask|auto-read|auto-write|never|always
/sandbox-profile read-only|workspace-write|danger-full-access
/context standard|frugal
/tool-profile lean|full
/compact status
/save name
/export html path
/sessions analyze|grep|failures|verify-log|decisions
```

## Configuration and state

Wolf reads `.env` for local development if present, but `.env` is ignored and must never be committed. Prefer `wolf auth login ...` for credentials.

Useful environment variables:

```bash
WOLF_PROVIDER=chatgpt
WOLF_MODEL=gpt-5.3-codex
WOLF_BASE_URL=https://example.test/v1
WOLF_API_KEY=...
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...
DEEPSEEK_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
OPENROUTER_API_KEY=...
WOLF_HOME=~/.wolf
WOLF_SESSIONS_DIR=~/.wolf/sessions
WOLF_LOGS_DIR=~/.wolf/logs
WOLF_APPROVAL=ask
WOLF_SANDBOX_PROFILE=workspace-write
WOLF_CONTEXT_MODE=standard
WOLF_TOOL_PROFILE=lean
WOLF_BUDGET_CAP=$5
```

Runtime state and credentials live outside version control. Project-local runtime directories such as `.wolf/`, `.pi/`, `target/`, `.env`, `WOLF.todo.json`, and `wolf-session-*` exports are ignored.

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
- `src/provider.rs` — provider catalog, auth, OAuth/API-key handling, request shaping.
- `src/session.rs` — session persistence, project state paths, logs, lock cleanup, terminal restore.
- `src/tools.rs` — tool catalog and provider-facing tool schemas.
- `src/tool_policy.rs` — validation and command/external-source guardrails.
- `src/orchestrator.rs` — turn telemetry, dedupe, circuit-breaker, and workflow guards.
- `src/tui.rs` — inline terminal UI.
- `tests/` — integration tests and replay fixtures.
- `benches/` — criterion benchmarks.
- `WOLF.md` / `WOLF.memory.md` — prompt-facing project guidance for Wolf working on itself.
- `MEMORY.md` — durable project memory synced through pi-memory.

More detail: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/USAGE.md`](docs/USAGE.md), [`SECURITY.md`](SECURITY.md), and [`CONTRIBUTING.md`](CONTRIBUTING.md).
