# Changelog

## Unreleased

### Removed

- Removed the subagent feature completely: `/subagent` slash command,
  `subagent-runtime` CLI subcommand, detached/inline runners, steering,
  quality gates, TUI state, session artifacts dir, and all associated
  tests/fixtures. `/plan` preserved via a direct read-only planner.
  Net -1544 lines.

### Added

- Added Git-native Dext checkpoints before approved write-risk tool calls.
  Checkpoints are stored as hidden refs under `refs/dext/checkpoints/` with
  local manifests under `.dext/checkpoints/`, giving users a deterministic
  preview/apply path after bad writes without adding provider-visible tools.
- Added `/undo` in interactive sessions and `dext undo` on the CLI.
  - `dext undo --list` lists recent checkpoints.
  - `dext undo --preview <id>` previews a checkpoint restore without changing
    files.
  - `dext undo --apply <id>` restores the checkpointed worktree paths.
  - `dext undo --prune` removes stale checkpoints.
  - `--reset-head` is explicit; normal undo never silently moves `HEAD`.
- Added mutation previews for approval-gated direct file tools. `write_file`,
  `edit_file`, and `multi_edit` show capped in-memory diffs before approval.
  Configure with `/preview`, `--preview off|simple|git`, or
  `DEXT_MUTATION_PREVIEW=off|simple|git`. The accepted `git` mode currently
  falls back to simple previews until alternate-index previews are implemented.
- Added explicit memory merge-driver support for Dext memory files.
  - `dext memory check` reports whether `MEMORY.md` and `recall.md` have merge
    drivers registered.
  - `dext memory register` installs local-only Git merge-driver configuration
    for section-aware merges.
  - `dext memory register --versioned-attributes` may also write versioned
    `.gitattributes` entries when a project intentionally wants them.
  - `dext memory unregister` removes the local registration.
  - `dext memory merge [--recall] <base> <ours> <theirs>` is the Git merge-driver
    entry point used after registration.

### Fixed

- Fixed `--tool-profile default`/`DEXT_TOOL_PROFILE=default` to select the documented default lean schema profile instead of expanding to full schemas.
- Recovery setup now no-ops cleanly outside Git repositories.
- Checkpoint sidecar paths resolve against the sandbox root and restore only the
  intended hinted paths.
- Changing the sandbox root resets cached Git-root discovery.
- Memory merge registration resolves the Git toplevel before touching versioned
  `.gitattributes`, so running from a subdirectory does not write attributes in
  the wrong place.
- Recall merges prefer local content for ambiguous deletions, dedupe additions
  from the incoming side, and keep a clean trailing newline.
- Mutation preview path handling enforces sandbox containment and avoids
  double-counting trailing new-file additions.
- Active packs now expose `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` to
  subsequent `bash` tool calls as well as hook processes.
- Shelf context aggregation now enforces the total byte budget, including
  separators and UTF-8-safe ellipses.
