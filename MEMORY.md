# Memory — dext

> Durable knowledge base for this project. Keep this file focused on a small
> set of current, manually useful project decisions.

---

## Project State

**Phase:** Work Map/cache status cleanup verified
**Updated:** 2026-05-06
**Summary:** Work Map session navigation and cache-visible usage status are implemented, verified, and installed.

**Next:**
- Use live sessions to validate Work Map drawer ergonomics
- Watch cache marker/status behavior under real ChatGPT/Codex usage

**Session Stats:** 18 sessions · 213878271 tokens · $64.05 total cost
**Last Model:** openai-codex/gpt-5.3-codex
**Last Session:** 019df92c-4835-71cc-b5a8-35c3563c2fc0

---

## Recent Decisions

- **Provider-visible toolset reduction; `/tools` owns slash switching**
  *Choice:* Keep implementations for specialized tools (`jq`, `fzf`, `awk`, `git_log`, `csvkit`) but hide them from the default provider-visible toolset; expose them only through explicit full toolset opt-in. CLI/env opt-ins remain `--toolset full` and `DEXT_TOOLSET=full`, but interactive switching belongs to existing `/tools full|default|status`, not a separate `/toolset` command. Frugal/tiny modes force a still smaller toolset. Capability-as-filesystem (`.dext/cap/` or virtual `/cap/...`) is not implemented and should not be built in core without a concrete, high-value use case; at most, prototype explicitly behind packs/shelves with reviewable files and normal file/bash flows.
  *Why:* Toolset reduction gives immediate prompt/schema savings without adding a new abstraction. `/tools` already owns tool visibility/status, so a second slash command duplicates the model and user interface. Capability FS has too many unresolved risks for core now: permission ambiguity, hidden side effects, lifecycle cleanup, sandbox boundary confusion, streaming/progress, concurrency/locking, discoverability, secrets/privacy, error semantics, versioning, and recreating a plugin protocol. Park it hard.
  `2026-05-20` `active`

- **GLM-5.1 empty-tool-argument loop (session 1779186436)**
  *Choice:* Investigated a 299-iteration session where 76% of tool calls failed. Root cause: GLM-5.1 silently drops function_call arguments when the intended JSON payload is large (multi-KB content strings), emitting `tool_use` blocks with `input: {}`. The model entered a 186-iteration degenerate loop of empty `bash` calls, self-diagnosed the issue in its text output but could not break out. User had to explicitly say "try chunking" for recovery.
  *Why:* Three compounding failures: (1) no heuristic break for repeated identical empty-tool failures, (2) no runtime hint to suggest chunking or alternative strategies, (3) error-message deduplication missing — 186 identical "missing command" errors pollute context without signal.
  *Mitigations to build:* (a) detect N-consecutive empty-input tool calls and inject a "chunk your output or use a different tool" runtime note, (b) coalesce repeated identical errors in context, (c) consider GLM-specific payload-size advisory for write-heavy ops.
  `2026-05-22` `active`

- **Git-native recovery/memory plan (GCMR review)**
  *Choice:* Keep the Git plumbing but reject a broad "counterfactual runtime." Build Dext-fit primitives in this order: (1) pre-mutation recovery refs plus `/undo` preview, (2) explicit local memory merge/check/register for `MEMORY.md` and `recall.md`, (3) mutation previews starting with simple in-memory diffs and only later optional `GIT_INDEX_FILE` previews. Do not add provider-visible tools.
  *Why:* Recovery refs give the best safety ROI without prompt/tool bloat. Memory merge protects core context assets. Alternate indexes are useful only after simpler previews, and rejected candidates/notes/bisect/rerere/replace/bundles add clutter or duplicate session state.
  *Constraints:* No automatic `.git/config`, `.gitattributes`, or hook edits; no silent `HEAD` moves; hidden refs under `refs/dext/...` with pruning; graceful no-op outside Git; honest limitations for untracked files and external side effects.
  `2026-05-18` `active`

- **Git-native recovery primitives implemented**
  *Choice:* Added core recovery refs/checkpoints with `/undo` and `dext undo`, explicit `dext memory check/register` merge-driver support for `MEMORY.md`/`recall.md`, and mutation previews for file-writing tools behind `/preview`, `--preview`, and `DEXT_MUTATION_PREVIEW`.
  *Why:* Delivers the GCMR plan as Dext-native primitives without provider-visible tool bloat or automatic Git config changes. Memory merge registration resolves the Git toplevel before touching versioned `.gitattributes`; preview `git` mode is accepted but currently falls back to simple in-memory previews until alternate-index previews are built. User-facing documentation now lives in `README.md`, `docs/USAGE.md`, `docs/ARCHITECTURE.md`, `docs/gcmr-plan.md`, `SECURITY.md`, and `CHANGELOG.md`; `docs/recovery.md` remains only a working/design file.
  `2026-05-19` `active`

- **Compaction active and end-turn thresholds**
  *Choice:* Standard mode auto-compacts at 90% of model context at end-turn and 80% after safe active tool-result checkpoints; explicit /compact percent and DEXT_MAX_HISTORY_CHARS overrides still win.
  *Why:* Earlier compaction preserves evidence before active turns exceed provider context while keeping end-turn compaction near the tail and budget-conscious.
  `2026-05-08` `active`

- **Usage status shows prompt-cache visibility**
  *Choice:* Dext usage displays split actual non-cached input, cached input reads/writes, and output; the TUI status line keeps the cached-input marker visible after first usage even when it is zero so cache status is not hidden.
  *Why:* ChatGPT/Codex and Anthropic report prompt-cache usage differently, and users need to see whether cache reads are happening without mistaking cached tokens for fresh input.
  `2026-05-06` `active`

- **Work Map TUI uses composer drawer, not scrollback navigation**
  *Choice:* /map opens a compact drawer inside the composer/input panel, with selection inserting editable /focus, /packet, or /track open commands; packet/focus/tracks outputs still render as transcript output.
  *Why:* Navigation should feel first-class without rewriting terminal history or reviving the older floating input popup.
  `2026-05-06` `active`

- **Work Map uses non-tree waypoint model from JSONL**
  *Choice:* Implement session navigation as a derived Work Map using Map, Waypoint, Packet, Focus, and Track; avoid tree/leaf terminology; JSONL/session headers remain the source of truth; Focus loads context only and never rewinds filesystem state; Tracks are additive sessions with origin metadata.
  *Why:* Deterministic local evidence should resolve transcript conflicts and support rebuildable packets/focus, while MEMORY.md remains durable curated recall rather than exact transcript storage.
  `2026-05-05` `active`

- **Session map/teleport architecture separates JSONL evidence from memory**
  *Choice:* Keep Dext JSONL/session headers as source of truth for exact navigation and build derived SessionMap/teleport packets from local evidence; use MEMORY.md for curated durable decisions, cross-session semantic recall, and optional tags/links, not for direct transcript storage or mutation.
  *Why:* Exact session replay, offsets, verification artifacts, and forks must remain local, deterministic, and source-first.
  `2026-05-05` `active`

- **Frugal context mode**
  *Choice:* Added --frugal, /context frugal, lean tool-profile schemas, smaller prompt/tool/history caps, and deterministic fact-card compaction for low-token operation.
  *Why:* Dext should preserve capability while avoiding repeated prompt/schema/tool-result bloat; frugal mode makes large context emergency capacity rather than default payload.
  `2026-04-27` `active`

---

## Memory Curation Notes

- Keep this file focused on a small set of current, manually useful project decisions.
- Prefer curated manual decisions/findings over auto-extracted session artifacts.
- Avoid logging ephemeral workflow chatter as decisions (for example: spawned subagents, "Done", "Synced", or similar progress narration).
- Avoid logging garbage entities from session ingest; keep only stable project-specific entities that would matter in a future session.
