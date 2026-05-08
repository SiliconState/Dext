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
**Phase:** Work Map/cache status cleanup verified
**Updated:** 2026-05-06
**Summary:** Work Map session navigation and cache-visible usage status are implemented, verified, and installed.

**Next:**
- Use live sessions to validate Work Map drawer ergonomics
- Watch cache marker/status behavior under real ChatGPT/Codex usage

**Session Stats:** 18 sessions · 213878271 tokens · $64.05 total cost
**Last Model:** openai-codex/gpt-5.3-codex
**Last Session:** 019df92c-4835-71cc-b5a8-35c3563c2fc0
<!-- pi-memory:state:end -->

---

## Recent Decisions

<!-- pi-memory:decisions:start -->
- **Compaction active and end-turn thresholds**
  *Choice:* Standard mode auto-compacts at 90% of model context at end-turn and 80% after safe active tool-result checkpoints; explicit /compact percent and WOLF_MAX_HISTORY_CHARS overrides still win.
  *Why:* Earlier compaction preserves evidence before active turns exceed provider context while keeping end-turn compaction near the tail and budget-conscious.
  `2026-05-08` `active`

- **Usage status shows prompt-cache visibility**
  *Choice:* Wolf usage displays split actual non-cached input, cached input reads/writes, and output; the TUI status line keeps the cached-input marker visible after first usage even when it is zero so cache status is not hidden.
  *Why:* ChatGPT/Codex and Anthropic report prompt-cache usage differently, and users need to see whether cache reads are happening without mistaking cached tokens for fresh input.
  `2026-05-06` `active`

- **Work Map TUI uses composer drawer, not scrollback navigation**
  *Choice:* /map opens a compact drawer inside the composer/input panel, with selection inserting editable /focus, /packet, or /track open commands; packet/focus/tracks outputs still render as transcript output.
  *Why:* Navigation should feel first-class without rewriting terminal history or reviving the older floating input popup.
  `2026-05-06` `active`

- **Work Map uses non-tree waypoint model from JSONL**
  *Choice:* Implement session navigation as a derived Work Map using Map, Waypoint, Packet, Focus, and Track; avoid tree/leaf terminology; JSONL/session headers remain the source of truth; Focus loads context only and never rewinds filesystem state; Tracks are additive sessions with origin metadata.
  *Why:* Deterministic local evidence should resolve transcript conflicts and support rebuildable packets/focus, while pi-memory remains durable curated recall rather than exact transcript storage.
  `2026-05-05` `active`

- **Session map/teleport architecture separates JSONL evidence from Pi memory**
  *Choice:* Keep Wolf JSONL/session headers as source of truth for exact navigation and build derived SessionMap/teleport packets from local evidence; use pi-memory/MEMORY.md for curated durable decisions, cross-session semantic recall, and optional tags/links, not for direct transcript storage or mutation.
  *Why:* Pi is shared external memory and already canonical for durable Wolf knowledge, but exact session replay, offsets, verification artifacts, and forks must remain local, deterministic, and source-first.
  `2026-05-05` `active`

- **Frugal context mode**
  *Choice:* Added --frugal, /context frugal, lean tool-profile schemas, smaller prompt/tool/history caps, and deterministic fact-card compaction for low-token operation.
  *Why:* Wolf should preserve capability while avoiding repeated prompt/schema/tool-result bloat; frugal mode makes large context emergency capacity rather than default payload.
  `2026-04-27` `active`

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
