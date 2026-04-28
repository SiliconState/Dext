# WOLF persistent memory

Prompt-facing cache only. Durable project memory lives in `MEMORY.md`, synced
through pi-memory with `--project wolf`. Keep this file compact and curated.

## Workflow
- Query `MEMORY.md` / pi-memory for durable history before major decisions.
- Log material decisions/findings to pi-memory, sync `MEMORY.md`, then distill
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
- `MEMORY.md` + pi-memory are the long-form source of truth;
  `WOLF.memory.md` is only the compact cache.
- Eval supports outcome-oriented assertions on files and command output.
- `codex` canonicalizes to `chatgpt`; ChatGPT/Codex share one OAuth-backed
  provider path. Model selection pins the selected model for the current
  provider/session.
- ChatGPT model normalization accepts compact aliases like `gpt5.3codex` and
  canonicalizes to API slugs.
- Tool stream capture floors are 6KB in standard mode; frugal intentionally uses
  lower prompt-facing caps. Explicit `read_file` offset+limit has a larger cap
  and overlap cache; `read_symbol` supports symbol-first source reads.
- Compaction should split near the tail, preserve recent tool evidence, and use
  capped old tool results so summaries retain tool-derived facts.
- Steering is queued-only: inject only at safe boundaries, keep it visible in
  session/work-ledger state, and explicitly address it in the next/final answer.
- Session state is evidence-backed: headers persist prompt/runtime provenance,
  exposed/approval/auto-approved tools, cleaned work ledger, provider health,
  verification records, and lightweight tool-result metadata.
- Bash/external process tools run subprocesses in their own Unix process group
  and clean up leftover children after exit/timeout/interrupt.
- `/subagent` reports are model-visible to the parent and quality-gated; the
  parent must review output before acting.
- TUI design stays text-first and sparse in the regular terminal buffer. Keep
  permission prompts inline/compact, transcript content sanitized/wrapped, and
  use the PTY smoke test for TUI verification.

## User preferences
- Favor context engineering and memory quality over adding more tools.
- Minimize tool duplication. No MCP for Wolf; prefer native/CLI integrations.
- Prefer Wolf's built-in reqwest-backed `http` tool before shelling to curl/xh.
- Use proper OAuth/web flows; do not copy Pi auth credentials manually.
- Prefer live/thinking/scroll progress above the input box only.
- Keep repo source-first and reviewable; avoid runtime clutter.

## Current focus
- Validate session analysis/ledger/session-discovery behavior in live long Wolf
  sessions and continue expanding real-world eval coverage.
- Continue low-bloat modularization into focused modules before adding new ones.

## Open question
- How much historical detail should remain prompt-injected versus only in
  pi-memory / `MEMORY.md`?
