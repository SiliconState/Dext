# Usage

## CLI overview

```text
dext [TASK...]        run one-shot with TASK
dext -p               read task from stdin
dext                  interactive TUI/REPL
dext --resume         resume the project-scoped latest session
dext --seat NAME      start a new session with a durable project identity
dext --seat NAME --resume  resume that seat's latest session
dext seat list|show NAME  inspect project seats
dext --fork           resume latest into an isolated unsaved branch
dext sessions         list project latest + named sessions
dext session ...      brief/export/analyze/grep/failures/verify-log/decisions/prune
dext auth ...         provider/model/auth management
dext undo ...         list, preview, or restore Dext Git checkpoints
dext --eval [NAME]    run eval harness
```

Run `dext --help` for the exact options supported by the installed binary.

## Seats

A Seat is a durable project-scoped agent identity, while a session is one disposable incarnation of that identity. Start a new seated session with `dext --seat planner`; resume that identity's latest durable session with `dext --seat planner --resume`; inspect records with `dext seat list` or `dext seat show planner`; maintain context with `dext seat set planner --label "Planning role"` and `dext seat set planner --summary-file ./planner-context.txt`. Clear fields with `--clear-label` or `--clear-summary`; `--summary-file -` reads bounded UTF-8 from stdin.

Seat records contain bounded identity metadata and the latest session id under `~/.dext/projects/<project-key>/seats/<seat-id>/seat.json`. Seat ids are portable lowercase 1–128 byte components; trailing dots and Windows device names are rejected. Selection alone is contextual and does not persist an empty record: the first successful durable session save or explicit `seat set` creates it. Unseated writes remain format v3 for compatibility; seated writes use v4 so pre-Seat binaries fail safely. Valid transitional v3 Seat headers remain loadable and fully validated, then upgrade on the next seated save; Seat metadata is rejected in v1–v2. Headers are bounded at 256 KiB on save, review, and restore. A session attached to one Seat cannot be resumed under another id or a same-named Seat from another project; malformed metadata and seated headers without project sandbox provenance also fail before state mutation. Explicitly resuming an unseated legacy session while a Seat is selected attaches identity on the next durable save. `--no-session --seat NAME` neither creates a record nor advances a pointer. Prompt-visible labels and summaries are privacy-redacted single-line JSON marked as user-authored non-instruction data. Switching projects clears active identity. `/reset` serializes pointer update and transcript removal; failed removal preserves history and attempts pointer rollback.

Crew maps directly portable role names to `crew.<agent>` and passes them to child Dext processes with `--no-session`. Existing valid custom role names outside the portable Seat grammar receive deterministic `crew.agent-<hash>` ids rather than becoming invalid. The child receives identity context but cannot update durable Seat state; crew's trusted parent remains authoritative. Project-scoped roles may read an existing summary; isolated roles intentionally use a private temporary project's context. Captured, detached-systemd, and tmux-pane workers use one effective absolute Dext state root even when supervisor home variables are stale.

## Authentication

List providers:

```bash
dext auth providers
```

Login with browser/OAuth where supported, or open a provider console for an API key:

```bash
dext auth login chatgpt
dext auth login kimi
```

`dext auth login kimi` opens `https://www.kimi.com/code/console`. Create or copy the API key associated with the Kimi coding plan, then paste it into Dext. Dext stores it as an API key and uses the isolated `https://api.kimi.com/coding` profile; it does not use Kimi device OAuth.

Store an API key:

```bash
dext auth login glm <api-key>
dext auth login openai <api-key>
dext auth login anthropic <api-key>
dext auth login kimi <api-key>
dext auth login deepseek <api-key>
```

The bundled Kimi provider also accepts `KIMI_API_KEY`, using a key created at `https://www.kimi.com/code/console`. Kimi coding-plan API keys are separate from the independently billed Moonshot Open Platform and its `MOONSHOT_API_KEY`; Dext does not substitute one for the other. Custom Kimi-compatible catalog profiles remain API-key based and must use an ID other than the reserved built-in IDs `kimi`, `kimi-code`, `kimi-coding`, and `kimi-membership`. If an older catalog already uses one of those IDs, rename that custom profile before upgrading; Dext rejects the collision rather than silently converting it.

An API-key value in `~/.dext/auth.json` may intentionally use `ENV_VAR_NAME` to resolve from the process environment or `!command` to resolve from command stdout. The command form runs through `bash -lc`; treat write access to `auth.json` as code-execution authority and use command references only when explicitly intended.

Import credentials from an external auth file (`~/.dext/external-auth.json` or `DEXT_EXTERNAL_AUTH_FILE`):

```bash
dext auth login chatgpt import
```

The external auth file is a JSON object mapping provider identifiers to credential objects. Dext tries candidate keys for each provider and imports the first match.

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
/model local/qwen3.6-35b-a3b-mtp-ud-q5_k_m
```

Credentials are stored in Dext state, not in the repository. Do not commit project `.env`, `.dext/`, exported sessions, or auth stores. Dext never auto-loads a project `.env` or searches parent directories; optional Dext dotenv settings belong in the user-owned `~/.dext/.env` (or `$DEXT_HOME/.env`).

### Provider catalog metadata

`~/.dext/providers.json` is auto-normalized to catalog v2 while continuing to accept v1 profiles. A provider may set `request_contract` to `anthropic-messages`, `openai-chat-completions`, `openai-responses`, or `chatgpt-responses`; this controls request and response routing independently of the provider id. Optional `model_aliases`, `model_defaults`, and per-model `model_specs` supply canonical ids, context/output limits, effort levels, reasoning modes, capabilities (`tools`, `reasoning`, `image_input`, and `prompt_cache`), and pricing. Explicit per-model metadata takes precedence; a selected effort uses an exact advertised level when available and otherwise clamps to the nearest supported level. Responses main turns and summaries both use the selected model's resolved levels; Off sends `none` only when advertised, otherwise the reasoning object is omitted. Context hints embedded in model names (such as `-128k` or `[1m]`) take precedence over provider-wide context defaults. Built-in metadata only fills omitted values. Legacy `context_window`, `model_context_windows`, and `model_effort_levels` fields remain accepted. `DEXT_PROMPT_CACHE=on|off` overrides catalog prompt-cache capabilities for Anthropic-style requests; auto mode uses catalog metadata.

The built-in ChatGPT and OpenAI API catalogs include `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`; OpenAI also retains the official unsuffixed `gpt-5.6` Sol id, while ChatGPT normalizes it to `gpt-5.6-sol`. Shorthands are `gpt56`/`gpt56sol`, `gpt56terra`, and `gpt56luna`. Each variant declares a 1,050,000-token context window and 128,000-token output metadata.

On the official API-key `openai` provider at `api.openai.com`, the four listed GPT-5.6 ids use `/v1/responses`; unknown `gpt-5.6-*` suffixes do not silently inherit that route. Effort and execution mode are independent: `--effort off|minimal|low|medium|high|xhigh|max` maps through the selected model's advertised levels to native `reasoning.effort` values from `none` through literal `max`, while `--reasoning-mode standard|pro`, `DEXT_REASONING_MODE`, or `/reasoning-mode` sets `reasoning.mode` (default `standard`). Normal requests and compaction summaries use the Responses reasoning object and `max_output_tokens`; summaries resolve their own model's effort metadata and preserve the selected Standard/Pro mode. Function tools use the flat Responses shape with `strict:false`, and tool-bearing stateless requests ask the provider for opaque `reasoning.encrypted_content` so Dext can replay it within the current tool turn. Content-filter terminals discard visible output, function calls, and opaque reasoning state instead of replaying it. The selected mode is shown as `/pro` in the status row and `/status` reports whether it is active. `DEXT_COMPACT_MODEL` is normalized through the active provider; its request contract, reasoning capability, effort levels, mode, and usage pricing are based on the resolved summary model rather than the main conversation model.

The OAuth-backed `chatgpt` provider remains on the Codex Responses contract. Its GPT-5.6 effort levels are `none`, `low`, `medium`, `high`, and `xhigh`; selecting Dext `max` maps to `xhigh`. Dext does not send the Platform-only `reasoning.mode` or `max_output_tokens` fields to that backend. A selected reasoning mode remains visible but inactive after switching to ChatGPT, a non-GPT-5.6 model, a custom endpoint, or any other provider; those request shapes are unchanged.

Built-in input/cached-input/output prices per million tokens are Sol `$5/$0.50/$30`, Terra `$2.50/$0.25/$15`, and Luna `$1/$0.10/$6`; above 272,000 input tokens, Dext applies the documented 2× input/cache and 1.5× output tier unless explicit pricing overrides are set.

The built-in Kimi Code catalog uses Anthropic Messages semantics at `https://api.kimi.com/coding`, defaults to K3, and reports zero incremental token cost because access is covered by the coding plan. Verified K3 metadata enables adaptive thinking with `max` effort and preserves empty thinking signatures required by that model; these compatibility rules do not apply to generic/custom Anthropic profiles.

## Local Qwen / llama.cpp

Dext includes a `local` provider for an OpenAI-compatible llama.cpp server at `http://127.0.0.1:8080`. Local providers use frugal context by default unless you explicitly select `standard` or `tiny`. On startup and provider/model switches, Dext probes llama.cpp (`/props`, `/slots`, then model endpoints) and uses the live runtime context window for whichever model alias the server exposes. You can select any local alias; no model-specific context value is built into the local provider. Start one server first, then select its alias:

```bash
dext auth provider local
# or inside Dext, using the alias configured in llama-server:
/model local/qwen3.6-35b-a3b-mtp-ud-q5_k_m
/effort off
```

Example local GGUF launch commands:

```bash
cd /path/to/llama.cpp
# Qwen3.6
./build/bin/llama-server \
  -m /path/to/models/Qwen3.6-35B-A3B-UD-Q5_K_M.gguf \
  --alias qwen3.6-35b-a3b-mtp-ud-q5_k_m -ngl 99 -c 131072 -t 24 \
  --flash-attn on --spec-type draft-mtp --spec-draft-n-max 2 \
  --host 127.0.0.1 --port 8080
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
/tools full                   expose specialized tools for this session
/approval ask                 ask before privileged tools
/sandbox-profile read-only    prevent write operations
/context tiny                 skinny local/token mode
/effort max                  select maximum model effort
/reasoning-mode pro          select GPT-5.6 Pro mode; active only on official OpenAI Responses
/compact status               inspect compaction threshold/history size
/preview simple              show direct file-tool mutation previews
/undo                        preview latest Dext Git checkpoint
/undo --apply                restore latest checkpointed worktree paths
/tokens                       approximate token-heavy messages
/save name                    save a named session
/export html session.html     export transcript
/sessions analyze             inspect latest session
/sessions brief               render a distilled continuation packet
/pack list                    list discovered packs
/pack run my-pack <task>      invoke a shelf pack in-session
/quit                         exit
```

## Terminal UI

The interactive interface uses an inline Ratatui viewport in the regular terminal buffer, preserving native scrollback. Enter submits; Shift+Enter or Alt+Enter inserts a newline; Ctrl+D quits; `?` opens the complete keymap when the input is empty. Input remains editable while a turn streams.

Dext pins the terminal stack exactly and carries a narrow vendored `ratatui-core` compatibility patch to avoid synchronous cursor-query stalls and whole-display clears during inline resize. See [`TUI.md`](TUI.md) for the behavior contract, dependency versions, regression coverage, and patch maintenance procedure.

## One-shot and automation

```bash
dext "inspect this repo and list missing tests"
printf 'summarize stdin\n' | dext -p
dext --output json "return a short answer"
dext --output stream-json "run a small diagnostic"
DEXT_PROVIDER=openai DEXT_MODEL=gpt-5.6-terra dext --effort max --reasoning-mode pro "solve this"
dext --preview simple "make a small documented edit"
dext undo --list
dext pack create personal/my-pack
dext pack run my-pack "run its workflow"
```

Use `--no-session` for disposable runs that should not write project session/log state. Because no durable state or tool journal is written, side-effect crash recovery is unavailable in this mode:

```bash
dext --no-session "quick answer only"
```

Likewise, `--fork` resumes into an unsaved branch without a durable tool journal; export or save the branch explicitly if it must be retained.


## Packs

See [`PACKS.md`](PACKS.md) for the full packs and shelves reference.

Dext packs are regular source directories inside shelves. Discovery checks, in precedence order:

1. project `.dext/shelves/<shelf>/packs/<pack>`
2. `DEXT_SHELVES_DIR` entries (`<shelf>/packs/<pack>` or a shelf root containing `packs/<pack>`)
3. user `~/.dext/shelves/<shelf>/packs/<pack>`

Create a reusable user pack with `dext pack create <shelf>/<name>`, or add `--project` for an explicitly project-local pack. Dext creates a minimal `PACK.md`, refuses overwrite, and discovers the new pack through the same shelf path immediately. Dext ships no pack content; shelf repositories are owned, reviewed, versioned, and distributed separately.

Direct `packs/`, `.dext/packs`, `~/.dext/packs`, `DEXT_PACKS_DIR`, and `DEXT_PACK_<NAME>_DIR` locations are not discovery roots. The latter per-pack environment names are still exported after selection so a pack can locate its own helpers.

This is the stable extension contract: packs act as modular battery packs without expanding Dext's provider-visible tool list. Optional `shelf.json` manifests are loaded into `ShelfRegistry`, resolved by scope precedence, shown by `dext shelves` / `/shelves`, and summarized to the model as typed ability metadata rather than executable provider tools.

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
dext pack create personal/my-pack
dext pack inspect my-pack
dext pack run my-pack "run its workflow"
```

Inside a session:

```text
/pack create personal/my-pack
/pack inspect my-pack
/pack run my-pack run its workflow
```

Conversational invocation also works when the message clearly asks to run or use a known pack, for example: `run my-pack on this task`. Conversational project-pack activation and project `shelf.json` metadata require one first-use confirmation for the active repository. Explicit `/pack` or `dext pack` invocation confirms only the selected project workflow; unrelated project shelf metadata remains unapproved. Choosing `Always` stores a bounded owner-private, single-link project-scoped approval marker on Unix; unsafe/permissive markers are ignored, and `/project-extensions reset` refuses unsafe marker shapes while removing a safe marker or clearing a session denial. Before approval, project metadata is omitted and cannot shadow a same-named user or run-shelf pack. Dext accepts `PACK.md` and `shelf.json` only as regular non-symlink files no larger than 1 MiB; the selected workflow is then capped to 32 KiB for model context.

A running pack stays active for the current session. Dext passes `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` to subsequent `bash` tool commands and pack hook processes, so workflows can invoke their own helpers. A pack from a user or `DEXT_SHELVES_DIR` shelf may declare exact names in the inline `credential-env` front-matter list; matching inherited values are available only to a simple direct invocation of that active pack's own native `bin/` helper. On Windows, only `.exe`/`.com` helpers qualify for this direct credential path; script helpers run through Bash with declared credentials removed. Project-local declarations are ignored and reported by `pack inspect`, so repository content cannot enable parent credential inheritance. Credential values are not exposed to hooks, arbitrary bash, pipelines/redirections, external tools, prompts, logs, or sessions, and provider-auth names remain excluded. A pack's `phooks.json`, when present, is added to the session hook set; changing the sandbox root clears active pack environment and hooks.

A reviewed pack may also declare `runtime.json` protocol version 1. Its regular, non-symlink, pack-contained native executable receives one bounded JSON request on stdin for each activation, dynamic tool call, or idle event and must return one bounded JSON response on stdout. Runtime activation has a separate executable-code approval; `never` disables it. Activation, idle, and tools declared `read` run read-only-confined. Tools declared `write` or `danger` keep normal per-call approval, sandbox, durable side-effect journal, and fail-closed Git checkpoint controls. Runtime helpers receive no inherited credentials. Bounded state and continuation counts persist in session headers and restore only when pack identity/source and the `runtime.json` SHA-256 still match. Responses may return tool content, state, steering, a delayed bounded continuation, and a privacy-redacted Markdown view. See [PACKS.md](PACKS.md#optional-executable-runtime-protocol) for schemas and exact caps.

## Git checkpoints, undo, and mutation previews

When Dext is running inside a Git repository, approved write-risk tool calls can
create lightweight recovery checkpoints immediately before each sequential dispatch. This means a
later call in one tool round captures state produced by earlier calls. Direct
file mutations receive path-specific restore hints. Checkpoints use hidden refs
under `refs/dext/checkpoints/` and owner-private local manifests and sidecars
under `.dext/checkpoints/` on Unix. Storage containers must be real directories;
on Unix they must be current-user-owned, `.dext` must not be group/world-writable,
and managed checkpoint, sidecar, and blob directories are owner-private. Locked
Unix mutations may repair modes only on current-user-owned managed directories;
inspection does not repair them, restore rejects unsafe sidecar/blob containers,
and prune retains unsafe artifact directory trees with bounded warnings while unlinking only
an orphan top-level sidecar symlink without following it. Dext adds
`/.dext/` to the repository-local
Git exclude file and automatically retains at most 20 checkpoints for no longer
than seven days. They are intended for Dext write recovery, not as a replacement
for commits. Write-risk `bash`/`awk`/`csvkit` calls inventory up to 500 existing
untracked paths, preserve regular files up to 8 MiB each and 32 MiB total, and
preserve bounded UTF-8 symlink targets without following them. Non-UTF-8 names,
unsupported types, and path/type/size bounds are reported as explicit partial-recovery gaps.
Regular-file content is stored once in owner-private SHA-256-addressed blobs shared by retained
checkpoints; unchanged source paths reuse a session cache only while source and blob metadata
fingerprints remain stable, preview/restore rehash blobs before trusting them, and prune or failed
checkpoint creation removes valid unreferenced/new blobs. Malformed or unsafe blob entries and
sidecar directory trees remain untouched and produce bounded warnings without stopping other
retention cleanup; an orphan top-level sidecar symlink with a valid checkpoint ID is unlinked
without following it. Owner execute state is retained in metadata. Current manifests record exact
direct-sidecar membership; older manifests that lack that field fail conservatively before mutation
when a missing artifact is ambiguous rather than deleting current path content. A recognized retired
8/9-field pre-JSON row is skipped with a warning rather than failing the whole listing, so one stale
row cannot disable `/undo` or block write-risk tools. Recognition requires an intact header and valid
retired field grammar, and its recorded OID must match any live checkpoint ref; mismatches and other
corrupt or tampered rows fail closed. Normal retention writes the compacted manifest before deleting
expired or integrity-matched retired refs, so a later ref-cleanup failure leaves only harmless orphan
refs rather than a manifest naming an already-deleted recovery point. Untracked
preview entries with unsafe host-native targets are omitted; a malformed field fails its row rather
than degrading it. Runtime manifest reads are capped at 16 MiB. If path/type/size limits make
untracked recovery partial, Dext asks
separately before the command and caches approval for the current repository and
session; approval keeps tracked/staged recovery and the bounded untracked subset,
while denial blocks the command. Other checkpoint failures remain fail-closed. In
a Git repository without an initial commit, Dext also blocks writes that would
overwrite existing worktree/index state because there is no normal Git restore
base. A workspace with no `.git` marker remains a non-Git no-op unless ambient
`GIT_DIR`, `GIT_WORK_TREE`, or `GIT_COMMON_DIR` routing is set; routed-without-marker
and malformed-marker cases fail loudly because Dext-owned Git commands scrub those
variables. Never mirror-push `refs/dext/*`.

CLI undo commands:

```bash
dext undo --list
dext undo --preview <checkpoint-id>
dext undo --apply <checkpoint-id>
dext undo --prune
```

Interactive undo commands:

```text
/undo --list
/undo                 # preview latest checkpoint
/undo --apply         # restore latest checkpointed worktree paths
/undo <id>
/undo <id> --apply
/undo --prune
```

Normal undo restores checkpointed worktree paths with literal Git pathspecs and does not move `HEAD`. Sidecar files and symlinks are revalidated and atomically replaced on supported platforms.
Moving `HEAD` requires the CLI's explicit reset-head mode.

Mutation previews are shown before permission prompts for `write_file`,
`edit_file`, and `multi_edit` when preview mode is enabled:

```bash
dext --preview off|simple|git
DEXT_MUTATION_PREVIEW=simple
```

Inside a session:

```text
/preview
/preview simple
/preview off
```

`git` is accepted as a preview mode but currently uses the same in-memory preview implementation as `simple`. The display is capped at 4 KiB, while added/removed counts cover the full proposed change. Final-newline-only changes are shown explicitly, and very high line-count inputs use a bounded conservative fallback.

## Context and cost controls

```bash
dext --frugal
dext --context-mode tiny
dext --context-mode frugal
dext --tool-profile lean    # default schema verbosity
dext --toolset full
DEXT_TOOLSET=full dext
dext --budget '$2'
dext --budget 200000t
dext --budget '$2 + 200000t'  # stop at either dimension
```

Frugal mode uses lean schemas by default, keeps the selected toolset, applies smaller caps, and compacts context more aggressively. Tiny mode uses a condensed prompt and caps history around 80% of the detected model window (bounded 8k–32k chars). Explicit `--toolset` and `--tool-profile` choices remain available in every context mode. The default toolset hides specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`); set `DEXT_TOOLSET=full`, run `dext --toolset full`, or use `/tools full` when you need them.

Budget caps accept the documented compact `t` suffix as well as `tok`, reject duplicate dollar or token dimensions in combined caps instead of silently keeping one value, and reject empty combined components. An invalid `DEXT_BUDGET_CAP` fails startup instead of silently disabling the guard.

Native `read_file` windows require positive integer offsets/limits, stop once they detect additional data beyond an explicit limit, and check cancellation between bounded input chunks, including within very long lines. `read_symbol` validates its line/context selectors, checks cancellation while loading, and rejects source files above 8 MiB; use `rg` plus focused `read_file` windows for larger files.

`/usage`, `/status`, JSON output, and session headers report provider token usage after each completed model request. Anthropic-family responses use input/output/cache counters, OpenAI-compatible streaming requests ask for usage chunks, and local llama.cpp uses streamed `timings.cache_n/prompt_n/predicted_n` so cached-prefix reuse is counted separately from new prompt tokens. Dollar estimates use provider/model price tables or the `DEXT_*_USD_PER_MTOK` environment overrides listed below; local defaults to zero dollar cost.

## Permission and sandbox controls

```bash
dext --approval ask
dext --approval never
dext --approval auto-read
dext --approval auto-write
dext --trust       # explicit high-trust opt-in (`approval=always`)
dext --no-trust    # explicitly select the default `ask` profile
dext --sandbox-profile read-only
dext --sandbox-profile workspace-write
dext --sandbox-profile danger-full-access
# --sandbox accepts the same profile names, or a directory as the sandbox root
```

Dext starts with the `ask` approval profile. Interactive frontends prompt before gated tools; non-interactive and JSON runs deny those calls instead of waiting for input. Automation that needs writes must opt in explicitly with `--approval auto-write`, `--approval always`, or `--trust`. Startup policy precedence is the last CLI safety flag (`--trust`, `--no-trust`, `--approval`, or `--approval-profile`), then a valid `DEXT_APPROVAL`, then true `DEXT_TRUST`, then `ask`. `DEXT_TRUST=1` is an alias for `approval=always`; false values do not override the safer fallback. Resuming a session never restores its saved trust grants over the current-run policy. Destructive Git worktree/ref/stash changes, per-command config overrides, and unknown aliases/subcommands remain gated. Because repository configuration can execute pagers, filters, fsmonitor, diff/textconv drivers, hooks, or aliases, shell Git is Danger unless it uses explicit `git --no-pager` and matches a narrow helper-free metadata-inspection allowlist; commands such as `grep`, `diff-tree`, `ls-files`, `check-ignore`, and `check-attr` remain gated because they can invoke fsmonitor. Prefer Dext’s hardened native Git tools for review operations. Recognized dynamic/wrapper command paths and inline/stdin code—including shell input redirections and heredocs—through common versioned Python/PyPy/Perl/Node/Ruby/PHP launchers are Danger too. Dynamic command words include variable/command, glob, brace, tilde, and attached-redirection expansion forms. Actual shell curl/wget/HTTPie/XH requests are gated because startup configuration and request bodies are not safely inferable; use Dext’s native `http` tool for read requests. Approval and filesystem sandboxing are independent, and filesystem sandboxing does not restrict outbound network access.

For durable sessions, approved side-effect-capable tool calls receive a bounded, redacted start/terminal journal under the private session state directory. On resume, pending transcript calls are reconciled without replay: absent starts are marked `not_started`, unresolved starts are `uncertain`, and terminal entries recover their status without claiming unavailable output. `--no-session` and `--fork` deliberately omit this journal, so side-effect crash recovery is unavailable in those modes.

## Safety diagnostics

```bash
dext doctor
dext doctor --approval auto-write --sandbox read-only --cd /path/to/project
```

Doctor reports `ok`, `info`, and `warn` findings for the effective approval profile and source, effective sandbox profile, kernel enforcement, provider catalog/auth integrity and versions, auth-file permissions, bounded latest session/todo/settings/tool-journal state, unresolved journal calls, and Git checkpoint support/latest metadata. It inspects only active/latest state and preserves the existing exit status 0 when warnings are present.

Doctor does not repair or rewrite state, resolve environment or `!command` credential references, invoke provider/local-model APIs, or print credential-bearing JSON. Use the explicit flags to inspect the posture that those startup choices would produce.

## Session commands

Seats select identity; sessions remain transcript and crash-recovery units:

```bash
dext --seat planner
dext --seat planner --resume
dext seat list
dext seat show planner
```

```bash
dext sessions
dext session brief latest
dext session export latest jsonl latest.jsonl
dext session export latest html latest.html
dext session analyze latest
dext session grep "error" latest
dext session failures latest
dext session verify-log latest
dext session decisions latest
dext session prune --days=7          # dry run
dext session prune --days=7 --apply  # remove stale locks and stale lock-only project dirs; all other state is preserved
```

Session commands can surface prompts, tool output, work-ledger entries, failure details, local paths, and credentials accidentally pasted by a user. Briefs omit the raw transcript but can still contain sensitive distilled data. Treat all session command output and exports as private unless reviewed.

## Environment variables

Provider/model:

```bash
DEXT_PROVIDER=local
DEXT_MODEL=qwen3.6-35b-a3b-mtp-ud-q5_k_m
DEXT_BASE_URL=http://127.0.0.1:8080
DEXT_THINKING_EFFORT=off
# cloud examples:
DEXT_PROVIDER=glm
DEXT_MODEL=glm-5.2[1m]
# provider key env fallbacks: ZAI_API_KEY, CHATGPT_ACCESS_TOKEN, OPENAI_API_KEY,
# ANTHROPIC_API_KEY, KIMI_API_KEY, DEEPSEEK_API_KEY
DEXT_MODEL_FORCE=1
DEXT_BASE_URL=https://api.example.test
DEXT_API_KEY=...
# Provider transport deadlines: positive values override all cloud/local defaults.
# Defaults: connect 15s; first headers 180s cloud / 600s local;
# stream or response-body idle 90s cloud / 300s local.
DEXT_PROVIDER_CONNECT_TIMEOUT_SECS=15
DEXT_PROVIDER_FIRST_BYTE_TIMEOUT_SECS=180
DEXT_PROVIDER_STREAM_IDLE_TIMEOUT_SECS=90
# ChatGPT/Codex only: model to switch to when a codex implementation model
# stalls on repeated no-mutation turns (default: the provider's default model).
DEXT_IMPL_FALLBACK_MODEL=gpt-5.4
```

Provider-specific credential fallbacks:

```bash
ZAI_API_KEY=...
ANTHROPIC_API_KEY=...
OPENAI_API_KEY=...
KIMI_API_KEY=...  # Kimi Code plan key; MOONSHOT_API_KEY is separate
DEEPSEEK_API_KEY=...
CHATGPT_ACCESS_TOKEN=...
```

Provider credentials are available to Dext's own HTTP client, but credential-shaped environment variables (`*_API_KEY`, `*_TOKEN`, `*_PASSWORD`, client secrets, cloud credentials, SSH agent variables, and related known names) are removed from agent-run subprocesses by default. Set `DEXT_INHERIT_TOOL_CREDENTIALS=1` only for a trusted model-invoked bash or external tool that explicitly requires the parent credential environment. Hooks, diagnostics, evals, checkpoints, browser launchers, and other Dext-owned subprocesses remain scrubbed even with that opt-in.

Privacy redaction is enabled by default while user-readable files remain readable. Before approved hooks run, `DEXT_TOOL_INPUT` is privacy-redacted for both `pre_tool` and `post_tool`, and `DEXT_TOOL_RESULT` is redacted for `post_tool`. Before tool results enter model context or session logs, Dext replaces private-key blocks, real secret assignments, and explicitly labeled SSNs, payment-card numbers, and account identifiers. Ordinary unlabeled long numbers and decimal market/HTTP values are not treated as cards. A compact redaction note is appended only when a value was actually replaced. Set `DEXT_PRIVACY=strict` or use `/privacy strict` to additionally block sensitive-looking native read paths and hidden, ignored, symlink-following, or sensitive-glob search scopes, including compact/combined ripgrep forms such as `-g.env` and `-ig .env` plus wildcard-prefixed sensitive globs such as `*.env`. Set `DEXT_PRIVACY=0` or use `/privacy off` only when raw, unredacted local data is intentionally required. On supported Linux/macOS hosts, kernel sandboxing preserves reads available to the Dext process user while confining writes: `read-only` permits only required scratch/device writes, and `workspace-write` additionally permits writes to the sandbox and common toolchain caches. `danger-full-access` intentionally disables this confinement. Dext warns at startup and in `dext doctor` when a confined profile cannot apply kernel enforcement; in that fallback, native write guards remain but shell and external-tool subprocesses are unconfined.

The complete Dext release suite is a deliberate exception to normal confined agent work. Under `workspace-write`, shared `/tmp`, arbitrary pseudo-terminals (`/dev/ptmx` and `/dev/pts`), and Cargo metadata such as `~/.cargo/.crates.toml` remain unwritable. Self-hosted `cargo test` can therefore show widespread temporary-directory failures and cascading shared-lock poisoning; `tui_smoke` cannot allocate its PTYs; and `cargo install` cannot update Cargo's install registry. Run those final commands directly in a trusted host terminal, or start a separate controlled Dext process with `dext --sandbox-profile danger-full-access --approval always`. Setting the profile only inside an already-confined shell cannot relax its inherited kernel sandbox. Keep the default hardening intact; see [`RELEASING.md`](RELEASING.md).

The built-in `http` tool always blocks IPv4 current-network (`0.0.0.0/8`), IPv4 broadcast, IP multicast, and IPv6 unspecified destinations. It blocks loopback, RFC1918, CGNAT, IPv6 unique-local, link-local, and cloud-metadata destinations by default after DNS resolution and on redirects. It connects directly and ignores proxy environment variables so these destination checks cannot be bypassed through proxy-side DNS. The entire DNS answer is validated before at most 32 addresses per host are retained in one bounded 256-entry cache for 60 seconds; libc queueing plus lookup has one five-second deadline and at most eight lookups may remain in flight. Connect and 15-second idle-read timeouts sit inside a total request deadline capped at 10 minutes, including defaults inherited from `DEXT_EXTERNAL_TIMEOUT_SECS`. The client supports HTTP/2 plus gzip/Brotli response decoding, refuses declared bodies above 8 MiB, stops unknown-length raw bodies at exactly 8 MiB, and reads at most 128 KB of source for `--extract-text` before extraction. Raw model output remains capped to its smaller head/tail window. Model-supplied duplicate headers and transport/framing or method-override headers such as `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, and `X-HTTP-Method-Override` are rejected; ordinary headers, including one `User-Agent` override, remain supported. URL-embedded credentials are rejected and transport errors omit URLs. Automatic `Referer` emission and HTTPS-to-HTTP redirect downgrades are disabled. Headerless/bodyless GET and HEAD requests may follow validated cross-origin redirects; requests with custom headers, bodies, or other methods remain same-origin so arbitrary credential headers and 307/308 bodies cannot be replayed across origins. GET, HEAD, or OPTIONS requests carrying raw stdin/data are Danger-class rather than read-only; `--ignore-stdin` keeps an otherwise bodyless request read-only. Provider, OAuth, and local-context clients retain their previous HTTP/1 and no-auto-decompression behavior. Trusted local-network use can opt in narrowly with `DEXT_HTTP_ALLOW_LOOPBACK=1`, `DEXT_HTTP_ALLOW_PRIVATE=1`, or `DEXT_HTTP_ALLOW_LINK_LOCAL=1`. These controls apply only to the built-in tool, not provider transport or arbitrary network clients run through `bash`.

State and logs:

```bash
DEXT_HOME=~/.dext
DEXT_SESSIONS_DIR=~/.dext/sessions
DEXT_LOGS_DIR=~/.dext/logs
DEXT_LOG_ARCHIVES=4
DEXT_SHELVES_DIR=~/.dext/shelves:/path/to/shared-shelves
DEXT_EXTERNAL_AUTH_FILE=~/.dext/external-auth.json
```

Runtime controls:

```bash
DEXT_NO_TUI=1
DEXT_APPROVAL=ask  # default approval policy
DEXT_TRUST=1  # explicit alias for DEXT_APPROVAL=always
DEXT_PRIVACY=1  # default: redact detected secrets; reads remain available
# DEXT_PRIVACY=strict  # also block sensitive-looking native read paths
# DEXT_INHERIT_TOOL_CREDENTIALS=1  # high-trust model-invoked tool opt-in
DEXT_SANDBOX_PROFILE=workspace-write  # sandbox + scratch + toolchain cache writes
# Built-in http tool only; trusted-network opt-ins, all disabled by default.
# This client ignores proxy environment variables so DNS/IP checks stay local:
# DEXT_HTTP_ALLOW_LOOPBACK=1
# DEXT_HTTP_ALLOW_PRIVATE=1
# DEXT_HTTP_ALLOW_LINK_LOCAL=1
DEXT_CONTEXT_MODE=standard
DEXT_TOOLSET=default
DEXT_TOOL_PROFILE=lean
DEXT_MUTATION_PREVIEW=simple
DEXT_THINKING_EFFORT=off
DEXT_REASONING_MODE=standard
# Optional same-provider compaction model; aliases are normalized and usage is
# priced against the resolved summary model.
# DEXT_COMPACT_MODEL=gpt56luna
DEXT_BUDGET_CAP='$5'
# Optional pricing overrides in USD per million tokens (local defaults to zero
# cost unless these are set):
DEXT_INPUT_USD_PER_MTOK=1
DEXT_OUTPUT_USD_PER_MTOK=5
DEXT_CACHE_READ_USD_PER_MTOK=0.1
DEXT_CACHE_CREATE_USD_PER_MTOK=1.25
# Max output tokens requested per streaming completion (default 8192). The
# Anthropic thinking budget is always clamped below this to satisfy the API.
DEXT_MAX_OUTPUT_TOKENS=8192
DEXT_EXTERNAL_TIMEOUT_SECS=60
DEXT_BASH_TIMEOUT_SECS=60
# Optional shell override. On Windows, Dext skips WSL app aliases and selects a
# real bash.exe from PATH (normally Git for Windows).
# DEXT_BASH_PATH='C:\Program Files\Git\bin\bash.exe'
DEXT_HOOK_TIMEOUT_SECS=60
# cargo-check workflow diagnostics timeout (default 120)
DEXT_DIAGNOSTICS_TIMEOUT_SECS=120
# eval-harness shell command timeout (default 15)
DEXT_EVAL_TIMEOUT_SECS=15
```

## Development commands

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo test -p ratatui-core --lib --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
cargo bench
```

After changing Dext itself, reinstall the interactive binary:

```bash
cargo install --path . --force --locked
```
