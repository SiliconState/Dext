# Usage

## CLI overview

```text
wolf [TASK...]        run one-shot with TASK
wolf -p               read task from stdin
wolf                  interactive TUI/REPL
wolf --resume         resume the project-scoped latest session
wolf --fork           resume latest into an isolated unsaved branch
wolf sessions         list project latest + named sessions
wolf session ...      export/analyze/grep/failures/verify-log/decisions
wolf auth ...         provider/model/auth management
wolf --eval [NAME]    run eval harness
```

Run `wolf --help` for the exact options supported by the installed binary.

## Authentication

List providers:

```bash
wolf auth providers
```

Login with browser/OAuth where supported:

```bash
wolf auth login chatgpt
```

Store an API key:

```bash
wolf auth login glm <api-key>
```

Switch providers/models:

```bash
wolf auth provider chatgpt
wolf auth models all
```

Inside the interactive session, the matching slash commands are:

```text
/providers
/provider chatgpt
/models all
/login chatgpt
/logout chatgpt
/model gpt-5.3-codex
```

Credentials are stored in Wolf state, not in the repository. Do not commit `.env`, `.wolf/`, exported sessions, or auth stores.

## Interactive workflow

Start Wolf:

```bash
wolf
```

Useful slash commands:

```text
/help                         show commands
/status                       runtime diagnostics
/tools                        list exposed/approval/auto-approved tools
/approval ask                 ask before privileged tools
/sandbox-profile read-only    prevent write operations
/context frugal               lower token usage
/compact status               inspect compaction threshold/history size
/tokens                       approximate token-heavy messages
/save name                    save a named session
/export html session.html     export transcript
/sessions analyze             inspect latest session
/quit                         exit
```

## One-shot and automation

```bash
wolf "inspect this repo and list missing tests"
printf 'summarize stdin\n' | wolf -p
wolf --output json "return a short answer"
wolf --output stream-json "run a small diagnostic"
```

Use `--no-session` for disposable runs that should not write project session/log state:

```bash
wolf --no-session "quick answer only"
```

## Context and cost controls

```bash
wolf --frugal
wolf --context-mode frugal
wolf --tool-profile lean
wolf --budget '$2'
wolf --budget 200000t
```

Frugal mode uses lean schemas, smaller caps, and more aggressive context reduction. Standard mode still defaults to lean schemas to avoid prompt bloat.

## Permission and sandbox controls

```bash
wolf --approval ask
wolf --approval never
wolf --approval auto-read
wolf --approval auto-write
wolf --sandbox read-only
wolf --sandbox workspace-write
wolf --sandbox danger-full-access
```

Avoid `--fangs-out` unless you intentionally want all gated tools auto-approved.

## Session commands

```bash
wolf sessions
wolf session export latest jsonl latest.jsonl
wolf session export latest html latest.html
wolf session analyze latest
wolf session grep "error" latest
wolf session failures latest
wolf session verify-log latest
wolf session decisions latest
```

Session exports can include prompts, tool output, credentials accidentally pasted by a user, and local paths. Treat them as private unless reviewed.

## Environment variables

Provider/model:

```bash
WOLF_PROVIDER=glm
WOLF_MODEL=glm-4.6
WOLF_MODEL_FORCE=1
WOLF_BASE_URL=https://api.example.test
WOLF_API_KEY=...
```

Provider-specific credential fallbacks:

```bash
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
OPENROUTER_API_KEY=...
```

State and logs:

```bash
WOLF_HOME=~/.wolf
WOLF_SESSIONS_DIR=~/.wolf/sessions
WOLF_LOGS_DIR=~/.wolf/logs
WOLF_LOG_ARCHIVES=4
```

Runtime controls:

```bash
WOLF_NO_TUI=1
WOLF_APPROVAL=ask
WOLF_SANDBOX_PROFILE=workspace-write
WOLF_CONTEXT_MODE=standard
WOLF_TOOL_PROFILE=lean
WOLF_THINKING_EFFORT=high
WOLF_BUDGET_CAP='$5'
WOLF_EXTERNAL_TIMEOUT_SECS=60
WOLF_BASH_TIMEOUT_SECS=60
WOLF_HOOK_TIMEOUT_SECS=60
```

## Development commands

```bash
cargo fmt
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
cargo bench
```

After changing Wolf itself, reinstall the interactive binary:

```bash
cargo install --path . --force
```
