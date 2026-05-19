# Dext Git-Native Recovery Plan

> Proper build plan for the three Git ideas that fit Dext: recovery refs,
> memory merge/check/register, and mutation previews. The goal is not a broad
> counterfactual runtime; it is a small set of runtime primitives that make the
> agent safer, faster to recover, and less likely to waste turns.

---

## Executive summary

Dext should build three Git-native features, in this order:

1. **Pre-mutation recovery refs + `/undo` preview**
   - Snapshot recoverable repository state before approved workspace mutations.
   - Add a preview-first restore path that never silently moves `HEAD`.
   - Highest safety and productivity return for the least model/tool surface cost.

2. **Local memory merge/check/register for `MEMORY.md` and `recall.md`**
   - Add a section-aware merge driver and explicit local registration flow.
   - Protects Dext's durable and prompt-facing memory files from bad text merges.
   - Zero normal per-turn cost when not merging.

3. **Mutation previews, then optional alternate-index previews**
   - Start with simple in-memory diffs for `write_file`, `edit_file`, and
     `multi_edit` before permission approval.
   - Later add unique `GIT_INDEX_FILE` previews where Git tree semantics matter.
   - Improves approval quality and prevents write/revert loops.

Do **not** build a general Git Counterfactual Memory Runtime. Dext already has
session headers, work ledger state, verification records, provider health,
compaction evidence, and project memory. A broad counterfactual layer would
mostly duplicate those systems while adding Git object-store clutter and user
surprise.

---

## Dext fit review

### What Dext already has

Relevant current architecture:

- `src/main.rs` centralizes direct file mutations in `execute_tool_with_cache`
  for `write_file`, `edit_file`, `multi_edit`, `todo_write`, and `git_commit`.
- `bash` and external process tools execute through the agent tool planning and
  execution loop, so pre-mutation behavior belongs around approved `Plan::Builtin`
  calls, not only inside file-tool match arms.
- `src/tool_policy.rs::classify_command_risk` already labels tool calls as
  `Read`, `Write`, or `Danger`.
- `src/tools.rs::needs_permission` and `is_parallel_safe_tool` keep permissions
  and parallelism policy out of model prompts.
- `Hooks` supports `pre_tool`, `post_tool`, and `user_prompt`; matching is exact
  tool name or `*`.
- Mutating builtins are not parallelized today, which makes checkpoint and
  preview sequencing simpler.
- Session headers and the work ledger already persist provider/runtime
  provenance, verification records, touched files, and compaction evidence.
- `MEMORY.md` and `recall.md` are already special context files and are recognized
  as decision-log paths by orchestration evidence code.

### Design rules

These features must follow Dext's existing style:

- **No provider-visible tool bloat.** Add runtime behavior, slash commands, and
  CLI subcommands; do not expose new LLM tools.
- **No automatic persistent Git config edits.** Memory merge registration must be
  explicit and local by default.
- **No silent `HEAD` movement.** Worktree undo is safe by default; commit undo
  requires an explicit reset-style command.
- **Graceful non-Git behavior.** All features degrade to no-op or simple diff mode
  outside Git repositories.
- **Preview before destructive recovery.** `/undo` and memory registration should
  show what will happen before mutating user state.
- **Bound runtime overhead.** The agent should become more reliable without
  slowing normal read/probe turns.

### Expected agent impact

| Feature | Agent improvement | User-visible improvement | Runtime cost target |
|---------|-------------------|--------------------------|---------------------|
| Recovery refs | Fewer unrecoverable bad writes; less model time spent reconstructing previous state. | One-command preview/undo instead of manual Git spelunking. | Zero on read-only turns; usually tens of ms before write-risk calls in normal repos. |
| Memory merge | More stable prompt memory across branches/machines; fewer corrupted or duplicated memory facts. | Fewer merge conflicts in `MEMORY.md`/`recall.md`; safer collaboration. | Zero per turn; only runs on `dext memory check/register` or Git merge. |
| Mutation previews | Better approval decisions; fewer write/revert loops; earlier detection of wrong edit scope. | Permission prompts show concrete diffs before files change. | Simple preview is O(file size) only on approval-gated writes; Git preview is opt-in. |

The main performance gain is not raw request latency. It is fewer wasted agent
iterations, fewer corrective edits, faster recovery after a bad mutation, and
more reliable long-term context.

---

## Build order

```text
Phase 1: Recovery refs + /undo preview
    ↓
Phase 2: Memory merge/check/register
    ↓
Phase 3: Simple mutation previews
    ↓
Phase 4: Optional GIT_INDEX_FILE previews
```

Why this order:

1. Recovery refs are the most generally useful safety primitive and can protect
   later work.
2. Memory merge protects Dext's core context layer and should not wait on dry-run
   plumbing.
3. Simple previews deliver most approval value without Git complexity.
4. Alternate indexes are useful, but only after the UX and preview contract are
   proven with simpler code.

---

## Phase 1 — Pre-mutation recovery refs

### Goal

Before Dext performs an approved workspace mutation, create a local Git recovery
point under `refs/dext/checkpoints/...`. Add `/undo` and CLI support to preview
and restore the latest checkpoint.

### Why this is the best first feature

Dext agents can already write files, run shell commands, stage commits, and edit
memory. When a write is wrong, the current recovery path is manual: inspect diff,
reconstruct previous content, or use raw Git. That burns tool calls, context, and
user time.

Recovery refs improve agent performance by converting a multi-turn repair task
into a local deterministic operation:

- fewer repeated `read_file`/`git_diff`/manual repair loops;
- less chance the model loses track of previous file state;
- lower risk when using high-autonomy approval profiles;
- safer experimentation during large refactors;
- less context pollution from recovery chatter.

### Scope

Create checkpoints for mutations that Git can reasonably help recover:

- Always: `write_file`, `edit_file`, `multi_edit`, `todo_write`, `git_commit`.
- When `classify_command_risk(...) != Read`: `bash`, `awk`, `csvkit`.
- Exclude by default: `http`, `browser`, provider login/auth flows, and other
  external side effects. They can be logged but not Git-restored.

Create the checkpoint after permission approval but before `pre_tool` hooks run.
Hooks may mutate or block, so they should be covered by the recovery point.

### User experience

Slash/CLI commands:

```text
/undo                         # preview latest checkpoint restore
/undo --apply                 # apply latest restore after preview/confirmation
/undo <id-or-ref>             # preview a specific checkpoint
/undo <id-or-ref> --apply     # apply a specific checkpoint restore
dext undo --list              # list recent Dext checkpoints
dext undo --preview <id>      # non-interactive preview
dext undo --apply <id>        # non-interactive apply
```

Safe defaults:

- Preview first with a capped diff.
- Restore worktree/index paths; do not run `git clean` by default.
- Do not move `HEAD` for normal undo.
- If the checkpoint was made before `git_commit`, explain that commit undo needs
  an explicit command such as `dext undo --reset-head <id>`.
- Preserve checkpoint refs after restore until pruning; users can inspect them.

### Implementation plan

New module:

```rust
// src/git_checkpoints.rs
struct Checkpoint {
    id: String,
    ref_name: String,
    oid: String,
    label: String,
    tool_name: String,
    created_at_ms: u128,
    head: String,
    paths_hint: Vec<String>,
    includes_untracked_sidecar: bool,
}

fn repo_root(root: &Path) -> Result<Option<PathBuf>, String>;
fn create_checkpoint(root: &Path, tool: &str, paths_hint: &[String]) -> Result<Option<Checkpoint>, String>;
fn list_checkpoints(root: &Path, limit: usize) -> Result<Vec<Checkpoint>, String>;
fn preview_restore(root: &Path, checkpoint: &Checkpoint) -> Result<String, String>;
fn restore_worktree(root: &Path, checkpoint: &Checkpoint, apply: RestoreMode) -> Result<(), String>;
fn prune(root: &Path, keep: usize, max_age_hours: u64) -> Result<(), String>;
```

Git strategy:

1. Find the Git root with `git -C <root> rev-parse --show-toplevel`.
2. If no Git repo exists, return `Ok(None)` and continue normally.
3. Build a sanitized ref like
   `refs/dext/checkpoints/<session>/<timestamp>-<ordinal>-<tool>`.
4. If tracked/index/worktree state is dirty, use `git stash create` and store the
   resulting object with `git update-ref`.
5. If the repo is clean, store `HEAD` directly. `git stash create` returning an
   empty string for a clean repo is expected.
6. Write checkpoint metadata to a local manifest under `.dext/checkpoints/` for
   listing and restore UX.
7. Prune only refs under `refs/dext/checkpoints/`.

Untracked policy:

- MVP: document that arbitrary untracked files from shell commands are not fully
  recovered.
- Direct file tools: if the target exists and is untracked, copy the old bytes to
  a `.dext/checkpoints/<id>/...` sidecar before mutation.
- Never promise rollback of external service state or arbitrary background
  process effects.

### Integration points

- Planning loop in `src/main.rs`: after approval and before `pre_tool` hooks,
  call `maybe_create_checkpoint` for write-risk `Plan::Builtin` calls.
- `execute_tool_with_cache`: no large rewrite needed for the MVP; direct tools
  remain responsible for mutation.
- `WorkLedger`/session header: store last checkpoint id/ref and optionally a
  small recent checkpoint list.
- Slash command dispatcher: add `/undo` preview/apply flows.
- CLI parser: add `dext undo` subcommands.

### Performance plan

Target overhead:

- Read-only turns: **0 ms**; checkpoint code must not run.
- Clean Git repo write: one `rev-parse`/`update-ref` path, target p50 under
  roughly 20 ms on normal repos.
- Dirty tracked repo write: `stash create` path, target p50 under roughly
  50-100 ms on normal repos.
- Large repos: cap preview output and log slow checkpoint creation; do not block
  indefinitely.

Optimizations:

- Cache repo root per agent session.
- Skip checkpointing when the tool is read-only by policy.
- Coalesce repeated checkpoints only when safe: never skip if a previous mutation
  succeeded since the last checkpoint and no equivalent checkpoint exists.
- Use capped metadata and manifests; do not inject checkpoint payloads into model
  context.

Measure:

- Record checkpoint creation duration in debug/session logs.
- Count checkpoint failures separately from tool failures; checkpoint failure
  should warn but not crash normal tool execution unless strict mode is enabled.
- Add a small benchmark fixture for clean, dirty, and untracked direct-file cases.

### Tests

- Non-Git directory no-ops.
- Clean repo creates a hidden ref pointing at `HEAD`.
- Dirty tracked file restores previous worktree state.
- Staged changes are handled according to documented restore mode.
- Direct-file untracked sidecar restores original bytes.
- Write-risk `bash` creates a checkpoint; read-only `bash` does not.
- `pre_tool` hook mutation is covered by a checkpoint.
- Commit undo refuses to move `HEAD` without explicit reset mode.
- Ref pruning keeps newest N and never touches non-Dext refs.
- Windows path/ref sanitization.

### Done criteria

- `/undo` preview and apply work from normal CLI and TUI sessions.
- No provider-visible tool changes.
- Checkpoint failures are visible but do not corrupt tool flow.
- Tests cover Git and non-Git roots.

---

## Phase 2 — Memory merge/check/register

### Goal

Protect `MEMORY.md` and `recall.md` with a Dext-aware merge path. These files
feed future agent behavior; a bad merge can silently damage the agent's long-term
context.

### Why this matters for agent quality

Dext uses memory as curated project state, not just documentation. Merge damage
has direct runtime consequences:

- duplicate or contradictory memory can steer future agents incorrectly;
- lost decisions can cause repeated design debates;
- bloated `recall.md` increases prompt pressure;
- unresolved conflict markers can leak into prompt context and degrade answers.

A section-aware merge improves agent performance by keeping context cleaner and
more stable across branches/machines. It reduces repeated explanation and repair
turns caused by memory drift.

### Registration model

Registration must be explicit and local by default:

```text
dext memory check
dext memory register
dext memory unregister
dext memory merge <base> <ours> <theirs> <marker-size> <path>
dext memory merge --recall <base> <ours> <theirs> <marker-size> <path>
```

Local registration writes only local Git metadata:

```ini
# .git/config
[merge "dext-memory"]
    name = Dext section-aware memory merge
    driver = dext memory merge %O %A %B %L %P
[merge "dext-recall"]
    name = Dext compact recall merge
    driver = dext memory merge --recall %O %A %B %L %P
```

```gitattributes
# .git/info/attributes; local-only, not committed
MEMORY.md merge=dext-memory
recall.md merge=dext-recall
```

Optional team mode can write versioned `.gitattributes`, but only with an
explicit flag such as:

```text
dext memory register --versioned-attributes
```

No automatic warnings in every prompt. Put status in `/doctor`, `dext memory
check`, and workflow diagnostics to avoid prompt clutter.

### Merge behavior

`MEMORY.md`:

1. Parse ATX headings and preserve preamble.
2. Use heading path as section key.
3. If only one side changed a section from base, take that side.
4. If both sides add list items, union unique items in stable order.
5. For decision blocks, dedupe by normalized title/date where possible.
6. For true conflicting edits to the same decision, do not silently keep ours.
   Emit a clear conflict block and return non-zero, or keep both with an explicit
   Dext conflict marker requiring cleanup.
7. Preserve unknown sections and comments.

`recall.md`:

- Treat as compact prompt-facing cache, not the source of truth.
- Dedupe repeated bullets and cap merged size.
- Prefer ours for ambiguous conflicts and add a short marker telling the user to
  regenerate from `MEMORY.md` if needed.
- Do not use raw `merge=union`; it can duplicate and bloat prompt-facing memory.

### Implementation plan

New module:

```rust
// src/memory_merge.rs
struct ParsedMemory {
    preamble: String,
    sections: Vec<Section>,
}

struct Section {
    heading_path: Vec<String>,
    heading_line: String,
    body: String,
}

struct MergeOutcome {
    content: String,
    clean: bool,
    warnings: Vec<String>,
}

fn parse_memory(input: &str) -> ParsedMemory;
fn merge_memory(base: &str, ours: &str, theirs: &str) -> MergeOutcome;
fn merge_recall(base: &str, ours: &str, theirs: &str) -> MergeOutcome;
fn register(repo: &Path, mode: RegisterMode) -> Result<(), String>;
fn unregister(repo: &Path) -> Result<(), String>;
fn check(repo: &Path) -> Result<MemoryMergeStatus, String>;
```

CLI/slash integration:

- Add CLI subcommands under `dext memory ...`.
- Add slash commands only for interactive status/register guidance if useful.
- Do not add an LLM-facing tool.

### Performance plan

Target overhead:

- Normal agent turns: **0 ms** if no explicit check/merge is requested.
- `dext memory check`: a few local Git config/attribute reads.
- Merge driver: O(size of `MEMORY.md` + `recall.md`), normally tiny compared to
  model and build/test time.

Agent performance improvement:

- reduces future prompt inconsistency from memory conflicts;
- lowers token pressure by keeping `recall.md` deduped and compact;
- avoids repeated model reasoning caused by lost durable decisions;
- makes multi-branch Dext work safer without runtime prompt bloat.

Measure:

- Unit-test merge output semantically.
- Log merge warnings and conflict reasons to stderr for Git.
- Add `dext memory check --json` for structured diagnostics usable by `/doctor`.

### Tests

- Additive decisions under `## Recent Decisions` merge cleanly.
- Same decision edited differently produces explicit conflict behavior.
- Unknown headings and preamble are preserved.
- Duplicate recall bullets are deduped and capped.
- Local register writes `.git/info/attributes` and `.git/config`, not tracked
  files.
- Unregister removes only Dext-owned entries.
- Merge driver works when invoked by Git with temp files.
- Non-Git directory returns actionable status, not panic.

### Done criteria

- `dext memory check/register/unregister` works locally.
- Merge driver is conservative and never silently drops conflicting memory.
- No prompt injection or provider-visible tool changes.

---

## Phase 3 — Simple mutation previews

### Goal

Show the exact file diff before applying direct file mutations when permission is
being requested. Use the simplest implementation first: compute the proposed
content in memory and show a capped diff in the approval prompt.

### Why this improves agent performance

Today the user often approves a write based on tool name and JSON arguments. The
actual diff is only shown after mutation. That can cause:

- denied/retried tool calls when the user cannot assess scope;
- wrong edits that require follow-up repair;
- context churn from write-then-revert loops;
- lower trust in auto-write workflows.

Simple previews let Dext catch wrong scope before disk mutation. This improves
performance by reducing corrective turns and making approval decisions faster and
more confident.

### Initial scope

Preview only direct file tools:

- `write_file`: diff existing content or show new-file preview.
- `edit_file`: validate exact match, compute updated content, show diff.
- `multi_edit`: validate all edits atomically, compute final content, show diff.

Do not preview arbitrary shell effects in this phase. `bash` can still be
protected by recovery refs.

### UX

Permission prompt should include:

- tool name and path;
- diff summary: added/removed lines and file status;
- capped unified diff;
- note if preview was truncated;
- clear denial behavior: denial leaves the working tree unchanged.

Activation:

```text
/preview off|simple|git
--preview off|simple|git
DEXT_MUTATION_PREVIEW=off|simple|git
```

Suggested defaults:

- `Ask` / `AutoRead`: show simple previews for direct file tools.
- `AutoWrite` / `Always`: skip by default to avoid friction, unless explicitly
  enabled.
- Non-interactive mode: include preview only in permission/error output when it
  would have asked.

### Implementation plan

Core functions:

```rust
struct MutationPreview {
    path: PathBuf,
    status: PreviewStatus,
    added: usize,
    removed: usize,
    diff: String,
    truncated: bool,
}

fn preview_write_file(root: &Path, path: &str, content: &str) -> Result<MutationPreview, String>;
fn preview_edit_file(root: &Path, path: &str, old: &str, new: &str) -> Result<MutationPreview, String>;
fn preview_multi_edit(root: &Path, path: &str, edits: &[Edit]) -> Result<MutationPreview, String>;
```

Integration:

- Compute preview before `request_permission` for direct file tools.
- Extend the internal permission event/sink to carry optional preview text.
- Reuse the same exact-match validation as the real edit path, or share helper
  functions so preview and apply cannot diverge.
- Keep preview text out of provider-visible tool schemas.
- Cap preview bytes independently from model tool-result caps.

### Performance plan

Target overhead:

- Only on approval-gated direct file writes.
- O(file size) read/compare for the target file.
- Preview cap prevents huge TUI/prompt output.
- No Git subprocess cost in simple mode.

Agent performance improvement:

- fewer mistaken writes that need repair;
- faster user approvals because diffs are visible up front;
- lower context churn from rejected/retried tool calls;
- more reliable long-running refactors because the human can catch scope errors
  before mutation.

Measure:

- Count preview approvals/denials.
- Track preview generation duration.
- Track whether denied previews are followed by corrected writes.
- Add tests that denial leaves content untouched.

### Tests

- Existing-file `write_file` preview matches post-write diff.
- New-file `write_file` preview is clear and capped.
- `edit_file` preview fails on zero or multiple matches exactly like apply.
- `multi_edit` preview validates all edits before showing output.
- Permission denial leaves working tree unchanged.
- Truncated preview still reports added/removed counts.
- Non-Git directories work.

### Done criteria

- Direct file approval prompts include useful capped diffs.
- Preview and apply share validation enough to avoid mismatch.
- No provider-visible tool schema changes.

---

## Phase 4 — Optional alternate-index previews

### Goal

Use `GIT_INDEX_FILE` only after simple previews are proven, and only where Git
adds value: tree-level candidates, mode handling, and multi-file repo-aware diffs.

### When to use it

Use alternate indexes for:

- candidate tree vs `HEAD` previews;
- multi-file preview batches;
- preserving executable bits and index metadata;
- explicit debug/eval workflows where storing a candidate tree is useful.

Do **not** store rejected candidates by default. They can leak sensitive content,
clutter the object store, and duplicate session logs.

### Design notes

- Use a unique temp index per preview:
  `.git/dext/tmp/dryrun-<pid>-<nonce>.index`.
- Never use a shared `.git/dext-dryrun.index`.
- Label preview basis honestly: `HEAD`, index, or current working file.
- For new files, write a blob with `git hash-object -w` and add it with
  `git update-index --add --cacheinfo` using repo-relative paths.
- Preserve mode from the index when available; default text files to `100644`.
- Clean temp indexes on success, denial, error, and best-effort shutdown.
- Fall back to simple preview outside Git repos or when Git preview fails.

### Performance plan

Target overhead:

- Opt-in only at first.
- Git preview subprocess budget should stay small for normal repos; log slow
  previews.
- Never block direct writes if preview fails and the user has explicitly chosen
  non-strict mode.

Agent performance improvement:

- better review of multi-file changes before mutation;
- less mismatch between what Git will show after the write and what Dext previews;
- safer eval/debug workflows where candidate trees can be inspected without
  touching the worktree.

### Tests

- New file, modified file, deleted file candidate previews.
- Executable bit preservation.
- Dirty working tree basis labeling.
- Unique temp indexes under concurrent preview requests.
- Cleanup after success, denial, and failure.
- Windows path behavior.

---

## Metrics and telemetry

Add lightweight runtime metrics to logs/session metadata, not model prompts:

- checkpoint creation count, duration, and failures;
- latest checkpoint id/ref;
- undo preview/apply count;
- memory check/register status;
- memory merge warnings/conflicts;
- preview generation count, duration, truncation, approval/denial.

Success indicators:

- fewer manual recovery edits after bad mutations;
- fewer write/revert loops in session logs;
- fewer memory-file merge conflicts;
- lower prompt churn from duplicate `recall.md`/`MEMORY.md` content;
- no measurable slowdown on read-only turns.

---

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Checkpoint overhead in large repos | Run only before write-risk calls, cache repo root, log slow paths, allow disabling. |
| False confidence around untracked/external state | Document limits; sidecar direct-file untracked targets; never claim full rollback for shell side effects. |
| Hidden ref clutter | Keep refs under `refs/dext/checkpoints/`; prune by age/count; expose list/prune commands. |
| Accidental `HEAD` movement | Default undo restores worktree/index only; reset requires explicit flag and preview. |
| Memory merge silently drops facts | Conservative algorithm; explicit conflict markers/non-zero exit for true conflicts. |
| Prompt/tool bloat | Runtime commands only; no new provider-visible tools or schemas. |
| Preview output too large | Independent byte/line caps and truncation summaries. |
| Git config pollution | Local `.git/info/attributes` by default; versioned attributes only with explicit flag. |

---

## Implementation milestones

### Milestone 1: checkpoint core

- Add `git_checkpoints.rs`.
- Create/list/prune checkpoint refs.
- Add manifest writing under `.dext/checkpoints/`.
- Unit-test clean, dirty, non-Git, and pruning cases.

### Milestone 2: checkpoint integration and `/undo`

- Wire checkpoint creation before approved write-risk calls.
- Add `/undo` preview/apply and `dext undo` CLI.
- Add untracked sidecar support for direct file tools.
- Add session metadata/logging.

### Milestone 3: memory merge

- Add parser/merge module.
- Add `dext memory check/register/unregister/merge`.
- Register local `.git/info/attributes` by default.
- Add conservative conflict behavior and tests.

### Milestone 4: simple previews

- Add in-memory preview helpers for direct file tools.
- Extend permission UI with optional preview text.
- Add caps, truncation summaries, and denial tests.

### Milestone 5: optional Git previews

- Add unique alternate-index preview module.
- Keep opt-in behind `/preview git`, CLI flag, or env.
- Add concurrency, cleanup, mode, and Windows tests.

---

## Verification requirements

For docs-only edits:

- Inspect rendered Markdown/diff for consistency.
- Run `git diff --check`.

For Dext code changes:

- `cargo fmt --check`
- `cargo build --release`
- `cargo test --release`
- If `src/tui.rs` changes: `cargo test --release --test tui_smoke -- --nocapture`
- `cargo install --path . --force`

---

## Final recommendation

Build the three features, but keep them narrow:

1. **Recovery refs** make the agent safer immediately and reduce recovery work.
2. **Memory merge** protects Dext's context quality over time.
3. **Mutation previews** reduce approval uncertainty and write/revert loops.

Together they give Dext a practical advantage: safer autonomous edits, cleaner
long-term memory, and fewer wasted tool/model turns, without increasing the
LLM-facing tool surface.
