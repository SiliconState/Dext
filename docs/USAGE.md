# Usage

## CLI overview

```text
dext [TASK...]        run one-shot with TASK
dext -p               read task from stdin
dext                  interactive TUI/REPL
dext --resume         resume the project-scoped latest session
dext --fork           resume latest into an isolated unsaved branch
dext sessions         list project latest + named sessions
dext session ...      export/analyze/grep/failures/verify-log/decisions
dext auth ...         provider/model/auth management
dext --eval [NAME]    run eval harness
```

Run `dext --help` for the exact options supported by the installed binary.

## Authentication

List providers:

```bash
dext auth providers
```

Login with browser/OAuth where supported:

```bash
dext auth login chatgpt
```

Store an API key:

```bash
dext auth login glm <api-key>
dext auth login openai <api-key>
dext auth login anthropic <api-key>
dext auth login deepseek <api-key>
```

Switch providers/models:

```bash
dext auth provider local
dext auth models all
```

Inside the interactive session, the matching slash commands are:

```text
/providers
/provider chatgpt
/models all
/login chatgpt
/logout chatgpt
/model local/qwen-local
```

Credentials are stored in Dext state, not in the repository. Do not commit `.env`, `.dext/`, exported sessions, or auth stores.

## Local Qwen / llama.cpp

Dext includes a `local` provider for an OpenAI-compatible llama.cpp server at `http://127.0.0.1:8080`. Start your server first, then select it:

```bash
dext auth provider local
# or inside Dext:
/model local/qwen-local
/effort off
```

For the installed Qwen model, a known-good server command is:

```bash
cd /home/abaka/Documents/Projects/Overload/MoE/llama.cpp
./build/bin/llama-server \
  -m /home/abaka/Documents/Projects/Overload/MoE/models/Qwen3.6-35B-A3B-Q4_K_M.gguf \
  -ngl 11 -c 4096 -np 1 --cache-ram 0 --no-warmup -t 12 \
  --reasoning off --flash-attn on --host 127.0.0.1 --port 8080
```

Use `--context-mode tiny --effort off` or `/context tiny` plus `/effort off` for the lowest local token/compute pressure.

## Interactive workflow

Start Dext:

```bash
dext
```

Useful slash commands:

```text
/help                         show commands
/status                       runtime diagnostics
/tools                        list exposed/approval/auto-approved tools
/approval ask                 ask before privileged tools
/sandbox-profile read-only    prevent write operations
/context tiny                 skinny local/token mode
/compact status               inspect compaction threshold/history size
/tokens                       approximate token-heavy messages
/save name                    save a named session
/export html session.html     export transcript
/sessions analyze             inspect latest session
/pack list                    list discovered packs
/pack run autoresearch <task> invoke a pack in-session
/quit                         exit
```

## One-shot and automation

```bash
dext "inspect this repo and list missing tests"
printf 'summarize stdin\n' | dext -p
dext --output json "return a short answer"
dext --output stream-json "run a small diagnostic"
dext pack run autoresearch "improve this benchmark"
dext --pack autoresearch "improve this benchmark"
```

Use `--no-session` for disposable runs that should not write project session/log state:

```bash
dext --no-session "quick answer only"
```


## Packs

Dext packs are regular source directories containing `PACK.md` plus optional helper scripts and `phooks.json`. Discovery checks, in precedence order:

1. `DEXT_PACK_<NAME>_DIR` environment variables
2. project `.dext/shelves/<shelf>/packs`, `.dext/packs`, and `packs`
3. `DEXT_SHELVES_DIR` entries (`<shelf>/packs/<pack>` or a direct shelf root containing `packs/<pack>`)
4. `DEXT_PACKS_DIR` entries
5. user `~/.dext/shelves/<shelf>/packs` and `~/.dext/packs`
6. bundled packs in the Dext repository

Shelf packs are just source-first packs grouped under a shelf. If a shelf and legacy pack define the same pack name in a scope, the shelf pack wins. This is the current stable extension contract: scaffold `packs/<name>/PACK.md` or `<shelf>/packs/<name>/PACK.md`, keep scripts/data beside it, validate with `dext pack inspect <name>`, then run it on a disposable task before sharing. Because packs are plain files and normal commands, they work across users, LLMs, and providers without expanding the provider-visible tool list. Optional `shelf.json` manifests are loaded into `ShelfRegistry`, resolved by scope precedence, shown by `dext shelves` / `/shelves`, and summarized to the model as typed ability metadata rather than new provider-visible tools.

Inspect typed shelf manifests:

```bash
dext shelves
```

Inside a session:

```text
/shelves
```

Use:

```bash
dext pack list
dext pack inspect autoresearch
dext pack run autoresearch "improve this benchmark"
```

Inside a session:

```text
/pack list
/pack inspect autoresearch
/pack run autoresearch improve this benchmark
```

Conversational invocation also works when the message clearly asks to run/use a known pack, for example: `run autoresearch on reducing test runtime`.

## Context and cost controls

```bash
dext --frugal
dext --context-mode tiny
dext --context-mode frugal
dext --tool-profile lean
dext --budget '$2'
dext --budget 200000t
```

Frugal mode uses lean schemas, smaller caps, and more aggressive context reduction. Tiny mode keeps frugal's lean tools, uses a condensed prompt only for tiny, and caps history around 80% of the local model window (bounded 8k–32k chars). Standard mode and frugal mode keep the regular main-agent prompt.

## Permission and sandbox controls

```bash
dext --approval ask
dext --approval never
dext --approval auto-read
dext --approval auto-write
dext --sandbox read-only
dext --sandbox workspace-write
dext --sandbox danger-full-access
```

Use `--trust` only when you intentionally want all gated tools auto-approved.

## Session commands

```bash
dext sessions
dext session export latest jsonl latest.jsonl
dext session export latest html latest.html
dext session analyze latest
dext session grep "error" latest
dext session failures latest
dext session verify-log latest
dext session decisions latest
```

Session exports can include prompts, tool output, credentials accidentally pasted by a user, and local paths. Treat them as private unless reviewed.

## Environment variables

Provider/model:

```bash
DEXT_PROVIDER=local
DEXT_MODEL=qwen-local
DEXT_BASE_URL=http://127.0.0.1:8080
DEXT_THINKING_EFFORT=off
# cloud examples:
DEXT_PROVIDER=glm
DEXT_MODEL=glm-4.6
# provider key env fallbacks: ZAI_API_KEY, CHATGPT_ACCESS_TOKEN, OPENAI_API_KEY,
# ANTHROPIC_API_KEY, DEEPSEEK_API_KEY
DEXT_MODEL_FORCE=1
DEXT_BASE_URL=https://api.example.test
DEXT_API_KEY=...
```

Provider-specific credential fallbacks:

```bash
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...
DEEPSEEK_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
```

State and logs:

```bash
DEXT_HOME=~/.dext
DEXT_SESSIONS_DIR=~/.dext/sessions
DEXT_LOGS_DIR=~/.dext/logs
DEXT_LOG_ARCHIVES=4
DEXT_PACKS_DIR=~/.dext/packs:/path/to/project-packs
DEXT_SHELVES_DIR=~/.dext/shelves:/path/to/shared-shelves
```

Runtime controls:

```bash
DEXT_NO_TUI=1
DEXT_APPROVAL=ask
DEXT_SANDBOX_PROFILE=workspace-write
DEXT_CONTEXT_MODE=standard
DEXT_TOOL_PROFILE=lean
DEXT_THINKING_EFFORT=off
DEXT_BUDGET_CAP='$5'
DEXT_EXTERNAL_TIMEOUT_SECS=60
DEXT_BASH_TIMEOUT_SECS=60
DEXT_HOOK_TIMEOUT_SECS=60
```

## Development commands

```bash
cargo fmt
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
cargo bench
```

After changing Dext itself, reinstall the interactive binary:

```bash
cargo install --path . --force
```
