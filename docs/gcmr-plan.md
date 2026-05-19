# GCMR Review — Git-Native Recovery, Preview, and Memory Merge for Dext

> Reviewed against current Dext architecture. Keep the useful Git plumbing, drop
> the broad "counterfactual runtime" framing, and build only features that make
> Dext safer without expanding the provider-visible tool surface.

---

## Executive decision

Do **not** build a general Git Counterfactual Memory Runtime. For Dext, the best
shape is three small, opt-in or low-clutter primitives:

| Rank | Feature | Verdict | Why it fits Dext |
|------|---------|---------|------------------|
| 1 | Pre-mutation recovery refs | **Build first** | Highest safety ROI. Gives Dext a local undo/recovery trail before risky writes, while keeping tools unchanged. |
| 2 | Local memory merge driver/checker | **Build second** | Directly protects `MEMORY.md`/`recall.md`, which are core Dext context assets. Must be explicit/local registration, not automatic repo pollution. |
| 3 | Mutation previews / dry-run index | **Pilot after 1–2** | Useful for approval prompts, but start with simple in-memory file previews. Use `GIT_INDEX_FILE` only where repo-aware previews add value. |
| — | Stored rejected candidates, notes, bisect, rerere, replace, bundles | **Defer/reject for runtime** | Interesting research/dev tools, but they add object-store clutter or duplicate session headers. Keep out of normal agent flow. |

Build order should be:

```text
Phase A: recovery refs + /undo preview
Phase B: memory merge/check/register
Phase C: mutation preview pilot, then optional alternate-index preview
```

This order is different from the original draft: memory merge is more Dext-core
than alternate-index previews, and simple previews should come before Git
plumbing-heavy previews.

---

## Corrections to the original draft

- `gix` is not libgit2 bindings; it is the gitoxide pure-Rust Git
  implementation. Avoid using it as a shorthand for libgit2.
- Claims like "nobody does X" should be softened to "no public evidence found".
  Closed-source agents cannot be evaluated that strongly.
- Dext hooks currently match exact tool names or `*`; they do **not** interpret
  regexes like `write_file|edit_file`.
- `.gitattributes` cannot include another attributes file with
  `!/.dext/gitattributes`. Use `.git/info/attributes` for local-only
  registration, or write a real versioned `.gitattributes` entry only when the
  user asks.
- `recall.md merge=union` is not good enough. `recall.md` is prompt-facing and
  compact; union merge can duplicate or bloat it. Prefer a Dext recall merge
  mode that dedupes/caps, or treat recall as rebuildable and keep ours.
- `git stash create` does not provide complete rollback semantics. It is good
  for tracked/index/worktree state, but untracked files and external side effects
  need explicit limitations or sidecar handling.
- `git_commit` needs special undo semantics. A working-tree restore does not
  move `HEAD`; undoing a commit must require an explicit reset-style operation.
- A single shared `.git/dext-dryrun.index` would race. Any alternate-index work
  must use unique temp indexes and clean them up.
- `git diff HEAD <tree>` is not the same as "diff against current dirty working
  tree" unless the alternate index is initialized from the relevant working-tree
  contents. Label the preview basis honestly.

---

## Current Dext integration facts

Relevant current code paths:

- File mutations are centralized in `execute_tool_with_cache` for `write_file`,
  `edit_file`, `multi_edit`, `todo_write`, and `git_commit`.
- `bash` executes through `execute_bash_async_*`, so risky shell commands need
  checkpoint handling in the agent planning/execution loop, not only inside the
  file-tool match arms.
- `tool_policy::classify_command_risk` already labels tool calls as `Read`,
  `Write`, or `Danger`.
- `tools.rs` keeps the provider-visible catalog lean. GCMR work should add
  runtime behavior and CLI/slash commands, not new LLM-facing tools.
- `Hooks` supports `pre_tool`, `post_tool`, and `user_prompt`, with exact tool
  matching or `*`, and passes `DEXT_TOOL_NAME`, `DEXT_TOOL_INPUT`, and
  `DEXT_TOOL_RESULT`.
- Mutating builtins are not parallelized today because only read-safe tools are
  considered parallel-safe.
- `WorkLedger`, session headers, provider health, verification records, prompt
  provenance, and compaction evidence already preserve much of the metadata that
  a broad counterfactual runtime would otherwise try to invent.
- `MEMORY.md` and `recall.md` are already special in Dext's context/memory
  model; `orchestrator::is_decision_log_path` recognizes them for evidence.

Design constraints:

1. No provider-visible tool bloat.
2. No automatic edits to user `.git/config`, `.gitattributes`, or hooks.
3. Graceful no-op outside Git repositories.
4. Recovery commands must preview what they will restore before changing files.
5. Never silently move `HEAD`; commit undo/reset must be explicit.
6. Keep hidden refs under `refs/dext/...` and prune them.
7. Treat untracked files and non-file side effects honestly; do not promise full
   rollback unless implemented.

---

## Phase A — Pre-mutation recovery refs

### Goal

Before an approved local mutation, create a recovery point in the user's actual
Git repository. This gives Dext a black-box recorder for agent writes without
adding provider-visible tools.

### Scope

Checkpoint before local mutations that can affect the workspace:

- Always: `write_file`, `edit_file`, `multi_edit`, `todo_write`, `git_commit`.
- When risk is not `Read`: `bash`, `awk`, `csvkit`.
- Usually exclude: `http` and `browser`, because Git cannot roll back external
  service/browser side effects. They can still be logged, but not recovered by
  Git refs.

Create the checkpoint after the user/approval profile allows the call, but
**before** `pre_tool` hooks run. Hooks may mutate or block, so recovery should
cover them too.

### Implementation sketch

New focused module:

```rust
// src/git_checkpoints.rs
struct Checkpoint {
    id: String,
    ref_name: String,
    oid: String,
    label: String,
    created_at_ms: u128,
    head: String,
    paths_hint: Vec<String>,
    includes_untracked_sidecar: bool,
}

fn repo_root(root: &Path) -> Option<PathBuf>;
fn create_checkpoint(root: &Path, label: &str, paths_hint: &[String]) -> Result<Option<Checkpoint>, String>;
fn preview_restore(root: &Path, checkpoint: &Checkpoint) -> Result<String, String>;
fn restore_worktree(root: &Path, checkpoint: &Checkpoint, paths: &[String]) -> Result<(), String>;
fn prune(root: &Path, keep_per_session: usize, max_age_hours: u64) -> Result<(), String>;
```

Git plumbing:

1. Find the repository with `git -C <root> rev-parse --show-toplevel`.
2. Determine a safe ref name such as
   `refs/dext/checkpoints/<session>/<timestamp>-<ordinal>-<tool>`.
3. If tracked/index/worktree state is dirty, use `git stash create <message>`
   and store the returned commit with `git update-ref <ref> <oid>`.
4. If the repo is clean, store `HEAD` in the hidden ref. `git stash create` may
   return nothing for a clean repo; that is not an error.
5. Persist checkpoint metadata in the session header/log and a local
   `.dext/checkpoints/*.jsonl` manifest for `dext undo` lookup.
6. Prune only under `refs/dext/checkpoints/`.

Untracked files need an explicit policy:

- MVP may document that Git recovery refs do not capture arbitrary untracked
  files.
- Better Dext-fit behavior: for direct file tools, if the target exists and is
  untracked, copy its original bytes into a `.dext/checkpoints/<id>/...` sidecar
  and include that in the checkpoint manifest. Do not try to solve arbitrary
  untracked changes from `bash` in the first pass.

### Restore UX

Add a user-facing command, not a provider tool:

```text
/undo                 # show latest checkpoint preview, ask/confirm in interactive mode
/undo --apply         # restore latest tracked worktree paths after preview
/undo <checkpoint>    # inspect/restore a specific checkpoint
```

Default restore should:

- show `git diff <checkpoint> -- <paths>` or an equivalent preview first;
- restore tracked paths with `git restore --source <ref> --worktree --staged -- <paths>`;
- restore sidecar-backed untracked direct-file targets when available;
- not run `git clean` by default;
- not move `HEAD`.

For `git_commit`, `/undo` should say that the checkpoint predates a commit and
require an explicit form such as `/undo --reset-head <checkpoint>` before using
`git reset --soft`/`--mixed`. Silent commit rollback is too dangerous.

### Tests

- Non-Git directory returns a clean no-op.
- Clean repo creates a checkpoint ref pointing at `HEAD`.
- Dirty tracked file round-trips through create + restore.
- Staged changes are preserved/restored as designed.
- Direct-file untracked sidecar restores the previous bytes, if sidecar support
  is implemented.
- `bash` write-risk commands checkpoint before execution.
- `pre_tool` hook mutation is covered by a checkpoint.
- `git_commit` undo refuses to move `HEAD` without the explicit reset flag.
- Pruning keeps the newest N refs and never touches non-Dext refs.
- Windows path/ref sanitization.

### Why this wins

This is the best immediate Dext feature from the proposal: small surface area,
large safety improvement, no model prompt/tool cost, and useful even when the
agent or provider misbehaves.

---

## Phase B — Memory merge driver and checker

### Goal

Make `MEMORY.md` and `recall.md` safer under branch/machine/session merges.
Dext relies on these files for durable and prompt-facing context, so plain text
conflicts can damage future runs.

### Registration strategy

Registration must be explicit and local by default:

```text
dext memory check       # report whether memory merge config is active
dext memory register    # write local .git/config + .git/info/attributes
dext memory unregister  # remove Dext's local entries
dext memory merge ...   # merge-driver entry point used by Git
```

Local registration should write:

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
# .git/info/attributes (local-only, not committed)
MEMORY.md merge=dext-memory
recall.md merge=dext-recall
```

Optional team mode can write versioned `.gitattributes`, but only behind a flag
such as `dext memory register --versioned-attributes`.

Do not warn on every session if unregistered. Put it in `/doctor`, workflow
diagnostics, or `dext memory check`, otherwise it becomes prompt/runtime noise.

### Merge algorithm

For `MEMORY.md`:

1. Parse ATX headings and preserve the preamble.
2. Use heading path (`## Recent Decisions` / `### ...`) as the section key.
3. For sections changed on one side only, take the changed side.
4. For additive list-item changes on both sides, union unique items in stable
   order, preferring ours order then adding theirs.
5. For dated decision blocks, dedupe by normalized title/date when possible and
   sort newest-first only inside sections that already use newest-first order.
6. For true conflicting edits to the same decision/block, do **not** silently
   keep ours. Emit a clear conflict block and return non-zero, or keep both with
   a `Dext memory merge conflict` marker that requires human cleanup.
7. Preserve comments and unknown sections; the driver must be conservative.

For `recall.md`:

- Treat it as compact prompt-facing cache, not authoritative memory.
- Dedupe repeated bullets/lines and cap merged content.
- If a conflict is ambiguous, prefer ours and add a short marker, or instruct the
  user to regenerate recall from `MEMORY.md`. Do not use raw union merge.

### Implementation sketch

```rust
// src/memory_merge.rs
struct Section {
    path: Vec<String>,
    heading_line: String,
    body: String,
}

fn parse_sections(input: &str) -> ParsedMemory;
fn merge_memory(base: &str, ours: &str, theirs: &str) -> MergeOutcome;
fn merge_recall(base: &str, ours: &str, theirs: &str) -> MergeOutcome;
fn register(repo: &Path, mode: RegisterMode) -> Result<(), String>;
fn check(repo: &Path) -> MemoryMergeStatus;
```

Keep this as a CLI/slash-command feature. Do not expose a `memory_merge` model
tool.

### Tests

- Additive decisions under `## Recent Decisions` from both branches merge.
- Same decision edited differently creates an explicit conflict, not silent data
  loss.
- Unknown headings and preamble survive unchanged.
- Duplicate recall bullets are deduped and capped.
- Local registration writes `.git/info/attributes` and `.git/config`, not tracked
  files.
- Unregister removes only Dext-owned config/attribute entries.
- Merge driver works when invoked with `%O %A %B %L %P` temp files from Git.

### Why this wins

This fits Dext's memory-first direction better than a speculative
counterfactual engine. It protects the context artifacts that influence future
agent behavior and can be built without adding provider-visible tools.

---

## Phase C — Mutation previews and optional alternate index

### Goal

Show what a direct file mutation would change before applying it when the user
is being asked for permission, and optionally use Git plumbing for repo-aware
candidate previews.

### Dext-first approach

Start with simple previews:

- For `write_file`, read existing file (or note new file) and compute an
  in-memory diff against the proposed content.
- For `edit_file` and `multi_edit`, Dext already computes the updated content
  before writing; reuse that to preview the exact result.
- Show the preview in the permission prompt for write tools in `Ask`/`AutoRead`
  modes.
- Skip by default in `AutoWrite`/`Always` to avoid latency/clutter, unless an env
  or CLI flag requests previews.

This avoids Git complexity for the highest-value case and works outside Git
repos.

### When `GIT_INDEX_FILE` is worth it

Add alternate-index previews only after simple previews land, and only for cases
where Git adds information:

- preview candidate tree vs `HEAD` for review;
- preserve file mode/executable bit from the index;
- compare multi-file candidates in one tree;
- optionally store a candidate tree for debugging when explicitly requested.

Do not store rejected candidates by default. They create hidden object-store
clutter and duplicate session logs. Add explicit debug/eval mode later if needed.

### Alternate-index design notes

- Use a unique temp index per preview, e.g.
  `.git/dext/tmp/dryrun-<pid>-<nonce>.index`; never a shared
  `.git/dext-dryrun.index`.
- Initialize from the right basis and label that basis:
  - index/`HEAD` basis for candidate-vs-commit preview;
  - direct in-memory diff for candidate-vs-current-working-file preview.
- For new files, write a blob with `git hash-object -w` and add it with
  `git update-index --add --cacheinfo` using a repo-relative path.
- Preserve mode from the existing index entry when available; default text files
  to `100644`.
- Clean temp indexes on success, rejection, error, and process shutdown where
  possible.
- Gracefully no-op outside Git repos.

### Activation

Possible knobs:

```text
--preview                 # simple previews in permission prompts
--preview=git             # try Git alternate-index previews where possible
DEXT_MUTATION_PREVIEW=off|simple|git
/preview off|simple|git
```

Keep names final during implementation; the important decision is that previews
are runtime behavior, not model tools.

### Tests

- `write_file` preview for existing and new files.
- `edit_file` preview after exact-match validation.
- `multi_edit` preview after all edits apply atomically.
- Permission denial leaves the working tree unchanged.
- Non-Git directory still shows simple preview.
- Alternate index handles new file, modified file, mode preservation, dirty
  working tree labeling, cleanup, and Windows paths.

---

## Quick prototype available today with hooks

A hooks-only checkpoint prototype is useful for experimentation, but it is not a
replacement for Phase A because it lacks metadata, pruning, exact risk handling,
and `/undo` integration.

Dext hook matching is exact or `*`, so filter in shell:

```json
{
  "pre_tool": [
    {
      "match": "*",
      "command": "case \"$DEXT_TOOL_NAME\" in write_file|edit_file|multi_edit|todo_write|git_commit|bash|awk|csvkit) ;; *) exit 0;; esac\ngit rev-parse --is-inside-work-tree >/dev/null 2>&1 || exit 0\noid=$(git stash create \"dext hook checkpoint $DEXT_TOOL_NAME\" 2>/dev/null || true)\n[ -n \"$oid\" ] || oid=$(git rev-parse HEAD 2>/dev/null || true)\n[ -n \"$oid\" ] || exit 0\nref=\"refs/dext/hooks/$(date +%s%N)-$DEXT_TOOL_NAME\"\ngit update-ref \"$ref\" \"$oid\" 2>/dev/null || true\nprintf 'checkpoint %s %s\\n' \"$ref\" \"$oid\""
    }
  ]
}
```

Caveats:

- This snapshots read-only bash calls too unless the shell filter parses
  `DEXT_TOOL_INPUT`.
- It does not capture untracked files unless they are already in Git state.
- It does not provide restore UX or pruning.
- It can be blocked by hook timeout/output caps.

---

## Rejected or deferred ideas

| Idea | Decision | Reason |
|------|----------|--------|
| Hidden refs for every rejected candidate | Defer/debug-only | Object-store clutter and privacy risk. Session logs already explain rejected paths. |
| Git notes for agent memory | Reject for runtime | Dext session headers and `MEMORY.md` already provide structured memory/provenance. Notes add another store to reconcile. |
| `git bisect` over memory/runtime state | Research only | Useful for debugging regressions, not normal agent execution. |
| `git rerere` for memory conflicts | Reject for default | Rerere can silently replay stale conflict resolutions; memory should be explicit. |
| `git replace` counterfactual histories | Research only | Too surprising for user repos and hard to explain safely. |
| Bundles as memory/runtime primitive | Reframe | If needed, build a plain `dext export/import` artifact rather than exposing Git bundle complexity. |
| Automatic `.gitattributes`/hook installation | Reject | Violates source-first, reviewable repo behavior. Registration must be explicit. |

---

## Acceptance checklist

Before implementing any phase:

- No new provider-visible tools.
- No automatic persistent Git config changes.
- Works from repo subdirectories and worktrees.
- Non-Git directories degrade cleanly.
- Windows path/ref behavior tested.
- Hidden refs and temp indexes are pruned/cleaned.
- User-facing restore/merge commands preview before mutating.
- Tests cover dirty, clean, staged, untracked, and failure paths.
- Verification for Dext code changes remains:
  - `cargo build --release`
  - `cargo test --release`
  - TUI smoke test if `src/tui.rs` changes
  - `cargo install --path . --force`
