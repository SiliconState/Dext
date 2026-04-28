# Memory — wolf

> Durable knowledge base for this project, synced through the shared external `pi-memory` system (SQLite in your user `.pi/memory` directory).
>
> **Refresh live sections:**
> ```bash
> pi-memory sync MEMORY.md --project wolf --limit 6
> ```

---

## Project State

<!-- pi-memory:state:start -->
**Phase:** Pi session-discovery recommendations implemented  
**Updated:** 2026-04-27  
**Summary:** Implemented lightweight long-session improvements: larger/cached explicit read windows, read_symbol, structured work/provider/verification ledgers, evidence-backed compaction, session analysis commands, artifact-backed verification records, subagent quality gates, crash snapshots, and provider reasoning/streamed thinking checks. Release build/test/install passed.  

**Next:**
- Validate session analysis and ledger output in a live long Wolf session
- Continue expanding real-world eval coverage
- Use the PTY-backed TUI smoke test for non-interactive TUI verification

**Session Stats:** 17 sessions · 213083614 tokens · $63.68 total cost
**Last Model:** openai-codex/gpt-5.3-codex
**Last Session:** 019dacec-7f13-7469-9be9-b212bc909ace
<!-- pi-memory:state:end -->

---

## Recent Decisions

<!-- pi-memory:decisions:start -->
- **Frugal context mode**  
  *Choice:* Added --frugal, /context frugal, lean tool-profile schemas, smaller prompt/tool/history caps, and deterministic fact-card compaction for low-token operation.  
  *Why:* Wolf should preserve capability while avoiding repeated prompt/schema/tool-result bloat; frugal mode makes large context emergency capacity rather than default payload.  
  `2026-04-27` `active`

- **PTY-backed TUI verification**  
  *Choice:* Added a Unix integration smoke test (cargo test --release --test tui_smoke -- --nocapture) that launches the real wolf binary in an openpty pseudo-terminal, answers cursor-position queries, verifies banner/help/key handling, exits via Ctrl+D, and uses isolated WOLF_HOME/sandbox paths.  
  *Why:* Replaces the manual-only TUI caveat with a real terminal-path check while adding no runtime code or dependencies.  
  `2026-04-27` `active`

- **TUI Critic remaining density/progress polish**  
  *Choice:* Implemented the remaining docs/Critic.md TUI items: consecutive read_file results to the same file now merge across flushes into one grouped block with read counts and line-span hints; dense tool streams get separators every 10 calls and grouped read_file blocks are dimmed; todo_read/todo_write progress is persisted in status/live indicators; rg output middle-truncates absurdly long individual lines; repeated assistant wolf labels dim after the first response.  
  *Why:* These changes address long-turn scanability without adding new tools or moving away from Wolf's sparse text-first TUI architecture.  
  `2026-04-27` `active`

- **TUI critique Tier 1 pass**  
  *Choice:* Implemented docs/Critic.md Tier 1 fixes with low-bloat inline UI changes: todo_write now returns checklist output plus status delta; tool/result truncation summaries include explicit hidden line/char metadata; permission prompts collapse to compact two-line ask/keys copy; Ctrl+O toggles the original tool block inline instead of creating a secondary expansion block.  
  *Why:* The critique identified high-impact scanability issues in long Wolf TUI sessions; these fixes preserve existing transcript architecture and same-target tool grouping while reducing duplicate UI and opaque summaries.  
  `2026-04-27` `active`

- **TUI transcript headings and tool borders**  
  *Choice:* Markdown H1 transcript headings render as bold light-cyan text without a blue background; prefixed transcript/tool output lines wrap within the remaining content width instead of relying on padding, preventing long rg/bash/read output from bleeding into swim-lane borders.  
  *Why:* The blue title background was visually distracting, and long tool-result rows could exceed the prefixed card width and visually collide with the vertical border.  
  `2026-04-26` `active`

- **Tool stream capture minimum is 6KB**  
  *Choice:* Raise process-backed tool stream capture from 4000 to 6000 bytes and keep adaptive model-result caps from shrinking below 6000 bytes.  
  *Why:* The user was still seeing a 4000-byte cap in fd/rg/bash results; the floor should match the expected 6000-byte read/tool output budget while explicit read_file windows remain larger.  
  `2026-04-26` `active`

<!-- pi-memory:decisions:end -->

---

## Findings & Lessons

Query live: `pi-memory query --project wolf` or `pi-memory search <keyword>`

## Memory Curation Notes

- Keep this file focused on a small set of current, manually useful project decisions.
- Prefer curated manual decisions/findings over auto-extracted session artifacts.
- Avoid logging ephemeral workflow chatter as decisions (for example: spawned subagents, "Done", "Synced", or similar progress narration).
- Avoid logging garbage entities from session ingest; keep only stable project-specific entities that would matter in a future session.
- Use Pi session ingest mainly for session linkage and recovery context; add manual memory entries for durable architectural knowledge.
- For this repo, always use `--project wolf` with pi-memory, or export `PI_MEMORY_PROJECT=wolf` before logging/syncing memory.

---

## Quick Reference

```bash
# Log
pi-memory log decision "Title" --choice "..." --rationale "..." --project wolf
pi-memory log finding  "Fact"  --category "..." --confidence verified --project wolf
pi-memory log lesson   "What broke" --why "..." --fix "..." --project wolf
pi-memory log entity   "Name" --type service --description "..." --project wolf

# Read
pi-memory state wolf
pi-memory query   --project wolf --type decision --limit 12
pi-memory query   --project wolf --type finding  --limit 12
pi-memory search  <keyword> --project wolf --limit 20
pi-memory recent  --n 20
pi-memory export  --project wolf --format md

# Sync this file
pi-memory sync MEMORY.md --project wolf --limit 6

# List all projects
pi-memory projects

# Optional cleanup patterns (direct DB surgery; back up first)
# python3 + sqlite3 can prune noisy workflow decisions/entities when the CLI
# does not expose delete/edit commands.
```
