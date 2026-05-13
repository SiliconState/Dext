# Dext recall

Compact prompt-facing recall only. Durable project memory lives in `MEMORY.md`.
Keep this file compact and curated.

## Workflow
- Query `MEMORY.md` for durable history before major decisions.
- Log material decisions/findings to `MEMORY.md`, then distill
  only prompt-worthy facts here.
- Avoid storing changelog detail here; completed implementation history belongs
  in commits, session logs, or `MEMORY.md`.

## Durable decisions
- Tool surface should stay lean; avoid overlapping tools that bloat prompts.
- `--frugal` / `/context frugal` enable low-token operation: lean tool schemas,
  reduced context/history/tool caps, deterministic compaction, and medium
  default effort.
- Standard prompts should also stay budget-conscious: compact system sections,
  capped runtime ledger/provider health, and minimal prompt-facing memory.
- `MEMORY.md` is the long-form source of truth; `recall.md` is
  only the compact prompt-facing cache.
- Eval supports outcome-oriented assertions on files and command output.
- `codex` canonicalizes to `chatgpt`; ChatGPT/Codex share one OAuth-backed
  provider path. Model selection pins the selected model for the current
  provider/session.
- ChatGPT model normalization accepts compact aliases like `gpt5.3codex` and
  canonicalizes to API slugs.
- Tool stream capture floors are 6KB in standard mode; frugal intentionally uses
  lower prompt-facing caps. Explicit `read_file` offset+limit has a larger cap
  and overlap cache; `read_symbol` supports symbol-first source reads. Process
  stream caps prefer head+tail preservation over cap increases.
- Compaction should split near the tail, preserve recent tool evidence, and use
  capped old tool results so summaries retain tool-derived facts. Standard mode
  auto-compacts at 90% of model context by end-turn and 80% after safe active
  tool-result checkpoints; explicit `/compact N%`/env overrides still win.
- Steering is queued-only: route active-turn user input directly to the steering
  channel (not behind the main command queue), inject only at safe boundaries, keep
  it visible in session/work-ledger state, and explicitly address it in the next/final answer.
- Session state is evidence-backed: headers persist prompt/runtime provenance,
  exposed/approval/auto-approved tools, cleaned work ledger, provider health,
  verification records, workflow diagnostics, and lightweight tool-result metadata.
- Bash/external process tools run subprocesses in their own Unix process group
  and clean up leftover children after exit/timeout/interrupt.
- `/subagent` reports are model-visible to the parent and quality-gated; the
  parent must review output before acting.
- TUI design stays text-first and sparse in the regular terminal buffer. Keep
  permission prompts inline/compact, transcript content sanitized/wrapped, and
  use the PTY smoke test for TUI verification.
- Benchmark/simple-CLI work should avoid unnecessary todo churn, prefer stdlib or repo-declared test runners, compare structured output semantically, and avoid broad rereads of freshly written files.
- Usage displays should split actual non-cached input from cache reads/writes;
  TUI status keeps the cached marker visible after first usage even when zero.
  Context pressure may still include cache because providers count it in request context.
- User may say Wolf/wolf when referring to Dext; treat it as the old Dext name.

## User preferences
- Favor context engineering and memory quality over adding more tools.
- Minimize tool duplication. No MCP for Dext; prefer native/CLI integrations.
- Do not add LLM-facing cargo tools; agent/runtime internals can use structured
  cargo handling when useful, but keep the exposed tool list lean.
- Prefer Dext's built-in reqwest-backed `http` tool before shelling to curl/xh.
- Use proper OAuth/web flows; do not copy credentials from unrelated tools manually.
- Prefer live/thinking/scroll progress above the input box only.
- Agent browser is allowed when useful for browser automation or web surfing.
- Keep repo source-first and reviewable; avoid runtime clutter.
- Prefer simple, flowing one-word names where possible; avoid compound or
  overqualified names when a clear single word works. Never use `name.name.ext`;
  prefer `hooks.json` over `DEXT.hooks.json`.
- Packs should be user-scoped and buildable/scaffoldable by Dext on demand for
  any project; keep packs separate from Dext core and avoid provider-visible
  tool bloat. Pack hook templates should use `phooks.json` so they do not
  collide with a project's active `hooks.json`.
- For ambiguous cleanup asks, ask once max; if unresolved, default to reversible `git stash` and verify.

## Current focus
- Compaction thresholds now use standard 90% end-turn / 80% active safe-checkpoint triggers with recent tool evidence preserved; verification/install passed after this update. Broader dirty tree still includes provider/auth, subagent/tool, and TUI changes across `.env.example`, `Cargo.toml`, `Cargo.lock`, `README.md`, `docs/USAGE.md`, `src/main.rs`, `src/main_tests.rs`, `src/orchestrator.rs`, `src/provider.rs`, `src/tools.rs`, and `src/tui.rs`.

## Open question
- How much historical detail should remain prompt-injected versus only in
  `MEMORY.md`?
