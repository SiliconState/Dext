# Wolf Work Map plan

## Goal

Build lightweight session navigation around coding work evidence instead of a message tree. Wolf should help the user find, package, and continue from meaningful work moments without exposing tree/leaf concepts or rewriting session storage.

## Product model

- **Map**: derived overview of a session's important work moments.
- **Waypoint**: an addressable moment such as an intent, evidence lookup, change, failure, verification, decision, compaction, or result.
- **Packet**: compact evidence bundle for one waypoint or a span of waypoints.
- **Focus**: continuation context seeded from a waypoint, normally carrying later lessons forward.
- **Track**: optional continuation session created from a waypoint and packet.

JSONL session files remain the source of truth. The map is rebuildable derived state. Pi/pi-memory remains for curated durable decisions and semantic recall, not exact transcript navigation.

## Implementation status

Implemented in the current Wolf tree:

- Sparse deterministic waypoint derivation from session JSONL/current history and work ledger evidence.
- Map, packet, focus, track, and tracks rendering.
- CLI commands: `wolf session map|packet|focus|tracks|track open`.
- Slash commands: `/map`, `/packet`, `/focus`, `/track open`, `/tracks`, plus `/sessions` aliases.
- Text-first TUI composer drawer that inserts editable `/focus`, `/packet`, or `/track open` commands.
- Additive track sessions with `track_origin` header metadata.
- Unit coverage for waypoint/packet/focus/track-origin behavior.

## Phase review

### Phase 1 — Derived map, no storage migration

**Makes sense.** This is the safest first slice because it adds value without changing how sessions are written. A deterministic map can be rebuilt from existing JSONL and from the current in-memory history.

**Keep it constrained:** do not show every message. Derive sparse waypoints from user intent, read/evidence tools, edits, failed tools, verification commands, compaction/resume markers, decisions, and final assistant results.

**Commands:**

```text
wolf session map [latest|NAME|PATH]
/map [latest|NAME|PATH]
/sessions map [latest|NAME|PATH]
```

**Initial waypoint kinds:**

- intent: user asks or changes objective
- evidence: read/search/http/git/todo observations that informed work
- change: file edits, writes, commits, or changed-file ledger evidence
- failure: failed command/tool or explicit blocker
- verify: build/test/lint/install command result
- decision: durable design choice or user preference
- compact: compaction/session-summary boundary
- result: assistant final/deliverable summary

### Phase 2 — Evidence packets

**Makes sense and is the core primitive.** A packet is more useful than raw replay because it carries the important evidence in a form that can be pasted into a new turn, a new session, or a subagent prompt.

**Commands:**

```text
wolf session packet @w07 [latest|NAME|PATH]
wolf session packet @w03..@w08 [latest|NAME|PATH]
/packet @w07 [latest|NAME|PATH]
/packet @w03..@w08 [latest|NAME|PATH]
/sessions packet @w07 [latest|NAME|PATH]
```

**Packet sections:**

- source session and selected message ranges
- selected waypoints
- intent
- evidence
- files
- commands
- verification
- decisions
- failures/blockers
- constraints/safety notes

### Phase 3 — Focus command

**Makes sense if focus is explicit and safe.** Focus must not pretend to rewind files. It should seed future model context with a packet and a visible safety notice.

**Commands:**

```text
/focus @w07 [--exact]
/focus @w07 --carry failures,decisions,files
wolf session focus @w07 [latest|NAME|PATH] [--exact]
```

**Default:** carry-forward. Use the selected waypoint plus later relevant lessons, especially failures, decisions, touched files, and verification. Exact replay is opt-in.

**Safety invariant:** focus changes model context only; it does not revert files, reset git state, or discard later session logs.

### Phase 4 — TUI work map drawer

**Makes sense if it stays text-first and does not rewrite scrollback.** Do not build a dashboard. `/map` should open a compact drawer inside the composer/input panel rather than append a navigable prompt into terminal history. Selection should only insert editable commands into the composer; it should not mutate agent state directly.

**Initial keys:**

```text
↑/↓  select waypoint
Enter insert /focus @wNN
p     insert /packet @wNN
t     insert /track open @wNN
Esc   close drawer
```

The drawer borrows height from the input panel, is capped to preserve transcript space, and keeps terminal scrollback stable while still making session navigation feel first-class.

### Phase 5 — Tracks

**Makes sense after packets/focus exist.** A track can be a normal named Wolf session seeded with a focus packet and origin metadata. Avoid in-file branching or worktree coupling for the first implementation.

**Commands:**

```text
/track open @w07 [name]
/tracks
wolf session track open @w07 [name] [latest|NAME|PATH]
wolf session tracks
```

**Track metadata:**

```json
{
  "source_session": "...",
  "source_waypoint": "@w07",
  "mode": "carry",
  "packet_hash": "...",
  "created_at": 1234567890
}
```

A track is resumed like any named session.

## Implementation principles

- Keep names non-tree-related: map, waypoint, packet, focus, track.
- Do not migrate session storage for map/packet/focus.
- Track metadata is additive and optional.
- Keep generated map deterministic and evidence-backed.
- Prefer compact output; the map should fit a terminal viewport.
- TUI should remain lightweight and command-driven.
- Pi is a memory/semantic-recall companion, not the exact transcript navigator.
- Agent browser is available for future browser/web interaction tasks, but this plan should not depend on external sources.

## Expanded implementation checklist

### Data and derivation

1. Add `WorkMapKind`, `WorkMapWaypoint`, `WorkMap`, `WorkMapSelection`, and `FocusMode`.
2. Build a tool-use index by call id so tool results can inherit command/file metadata.
3. Derive waypoints from:
   - user text blocks
   - assistant text blocks with decision/result signals
   - tool uses for evidence/change commands
   - tool results for failures and verification
   - header work ledger for objective, decisions, changed files, blocked items, and verification records
4. Deduplicate adjacent identical waypoints.
5. Assign stable display ids after derivation: `@w01`, `@w02`, ...

### Rendering

1. Render a compact map with source, model/provider, message count, waypoint count, and command hints.
2. Render packets for a single waypoint or inclusive range.
3. Render focus packets as packet output plus safety/carry-forward notes.
4. Cap category lengths to avoid flooding the TUI.

### CLI and slash commands

1. Extend `wolf session` with `map`, `packet`, `focus`, `tracks`, and `track open`.
2. Add top-level slash aliases: `/map`, `/packet`, `/focus`, `/track`, `/tracks`.
3. Extend `/sessions` with `map`, `packet`, `focus`, and `tracks`.
4. Keep CLI commands read-only except `track open`, which writes a named session.

### TUI

1. Add a `WorkMap` event so console/json/TUI can distinguish map output from normal slash output.
2. In TUI, store a small work-map drawer with text, waypoint ids, selected id, and scroll offset.
3. Render `/map` as a composer drawer, not as navigable transcript scrollback.
4. Add safe key handling: select waypoint, insert command text, close drawer.
5. Add slash completions/help entries.

### Verification

1. Add unit tests for waypoint extraction, packet rendering, focus safety text, and track-origin serialization.
2. Run focused tests first.
3. Run required Wolf checks before declaring done:
   - `cargo build --release`
   - `cargo test --release`
   - if TUI changed: `cargo test --release --test tui_smoke -- --nocapture`
   - `cargo install --path . --force`
