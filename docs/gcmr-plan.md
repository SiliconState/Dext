# GCMR Review — Git-Native Recovery, Preview, and Memory Merge for Dext

> Concise review/index for the Git ideas. The full implementation plan lives in
> [`docs/recovery.md`](recovery.md).

---

## Decision

Do **not** build a broad Git Counterfactual Memory Runtime. Dext already has
session headers, work ledger state, verification records, provider health,
compaction evidence, and curated memory. A broad counterfactual layer would
mostly duplicate those systems while adding Git object-store clutter and user
surprise.

Build three narrow Git-native primitives instead; the first implementation is
now in core with Phase 4 still deferred:

1. **Pre-mutation recovery refs + `/undo` preview** — implemented.
2. **Explicit local memory merge/check/register** for `MEMORY.md` and
   `recall.md` — implemented.
3. **Mutation previews** — simple in-memory diffs are implemented; optional
   `GIT_INDEX_FILE` previews remain deferred for cases where Git tree semantics
   help.

---

## Why these three fit Dext

| Idea | Keep? | Agent impact | Runtime cost |
|------|-------|--------------|--------------|
| Recovery refs | Yes, first | Converts bad-write recovery from multi-turn reconstruction into deterministic local preview/undo. Fewer corrective tool calls and less context churn. | Zero on read-only turns; small Git cost only before write-risk calls. |
| Memory merge/check/register | Yes, second | Keeps `MEMORY.md`/`recall.md` coherent across branches and machines, preserving future prompt quality. | Zero normal per-turn cost; only on explicit check/register or Git merge. |
| Mutation previews | Yes, third | Shows concrete diffs before permission approval, reducing wrong writes and write/revert loops. | Simple mode is O(file size) only on approval-gated direct file writes; Git mode opt-in. |

Together they make Dext safer and more reliable without adding provider-visible
tools.

---

## Dext constraints

- No provider-visible tool bloat.
- No automatic `.git/config`, `.gitattributes`, or hook edits.
- No silent `HEAD` movement; commit undo must require an explicit reset command.
- Graceful no-op outside Git repositories.
- Preview before destructive recovery.
- Keep hidden refs under `refs/dext/...` and prune them.
- Treat untracked files and external side effects honestly.

---

## Build order

```text
Phase 1: Recovery refs + /undo preview          implemented
    ↓
Phase 2: Memory merge/check/register            implemented
    ↓
Phase 3: Simple mutation previews               implemented
    ↓
Phase 4: Optional GIT_INDEX_FILE previews        deferred
```

Rationale:

1. Recovery refs are the highest ROI safety primitive and can protect later work.
2. Memory merge protects Dext's long-term context layer.
3. Simple previews deliver most approval value without Git complexity.
4. Alternate indexes are useful only after the preview UX is proven and remain
   optional follow-up work.

---

## Implemented surface

- `src/git_checkpoints.rs` creates, lists, previews, restores, and prunes
  recovery refs under `refs/dext/checkpoints/`. It stores local manifests and
  untracked sidecars under `.dext/checkpoints/`.
- `/undo` and `dext undo` expose checkpoint listing, preview, apply, and prune.
  Normal apply restores worktree paths and does not move `HEAD`.
- `src/memory_merge.rs` powers `dext memory check/register/unregister/merge`.
  Registration is local-only by default; versioned attributes are opt-in.
- `src/mutation_preview.rs` powers capped previews for `write_file`,
  `edit_file`, and `multi_edit` via `/preview`, `--preview`, and
  `DEXT_MUTATION_PREVIEW`.

---

## Rejected/deferred runtime ideas

| Idea | Decision | Reason |
|------|----------|--------|
| Stored rejected candidates by default | Defer/debug-only | Object-store clutter and privacy risk; session logs already explain rejected paths. |
| Git notes for agent memory | Reject | Duplicates session headers and `MEMORY.md`. |
| Git bisect over memory/runtime | Research only | Useful for debugging, not normal agent flow. |
| `git rerere` memory conflict replay | Reject by default | Can silently replay stale resolutions; memory should be explicit. |
| `git replace` counterfactual histories | Research only | Too surprising for user repos. |
| Automatic Git config/attribute/hook install | Reject | Violates Dext's source-first, reviewable behavior. |

---

## Full plan

See [`docs/USAGE.md`](USAGE.md) and [`../README.md`](../README.md) for the
current user-facing recovery, preview, and memory-merge commands. This file now
serves as the design decision record and follow-up index.
