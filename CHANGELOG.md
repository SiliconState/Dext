# Changelog

## Unreleased

### Changed

- Upgraded the terminal stack to exact Ratatui 0.30.2, ratatui-core 0.1.2,
  tui-markdown 0.3.8, Crossterm 0.29.0, and unicode-width 0.2.2 versions.
  Dext carries a narrow exact-source ratatui-core compatibility patch for its
  inline viewport; the real-PTY suite now gates streaming input, populated
  resize bursts, whole-screen clears, cursor-query counts, replay bounds, and a
  bounded completion wait that tolerates slower macOS CI hosts.
- macOS Seatbelt profiles now allow both canonical `/private/...` scratch paths
  and their standard `/var` or `/tmp` aliases, keeping temp APIs confined and
  usable.
- Windows shell execution now skips Windows/WSL app aliases when resolving
  `bash.exe`, prefers a real Bash implementation such as Git for Windows from
  `PATH`, and supports an explicit `DEXT_BASH_PATH` override.
- Consolidated maintained project documentation and removed obsolete release
  plan/scope artifacts. The inline TUI contract, dependency stack, patch
  rationale, and regression procedure now live in `docs/TUI.md`.
- Kept packs as first-class workflow units while making storage and discovery
  shelf-only. Packs now live exclusively at `<shelf>/packs/<name>` under
  project, user, or `DEXT_SHELVES_DIR` roots; direct pack roots and
  `DEXT_PACKS_DIR` are no longer discovery inputs.

### Removed

- Removed the subagent feature completely: `/subagent` slash command,
  `subagent-runtime` CLI subcommand, detached/inline runners, steering,
  quality gates, TUI state, session artifacts dir, and all associated
  tests/fixtures. `/plan` preserved via a direct read-only planner.
  Net -1544 lines.
- Removed all repository-owned and embedded pack payloads. Dext ships the pack
  lifecycle and shelf integration, but no pack content; users own and
  distribute shelf repositories separately.

### Added

- Added the public GitHub Pages documentation site at
  `https://siliconstate.github.io/Dext/`, deployed from `docs/` by a
  least-privilege workflow with commit-pinned actions and offline validation of
  metadata, local links, and anchors.
- Added `dext pack create <shelf>/<name>` and `/pack create` scaffolding for
  user-global packs by default and explicit project-local packs with
  `--project`.
- Added an always-available read-only todo modal to the inline TUI. `Ctrl+L`
  opens persisted session/project todos while idle or busy without changing the
  backend viewer's alternate-screen behavior.
- Added an isolated Kimi Code provider at `https://api.kimi.com/coding` with
  coding-plan API keys created at `https://www.kimi.com/code/console`,
  `KIMI_API_KEY` support, K3 adaptive thinking/empty-signature compatibility,
  and zero incremental usage pricing. Custom Kimi-compatible profiles remain
  API-key based, and Moonshot Open Platform credentials are not conflated with
  Kimi Code coding-plan access.
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
  `edit_file`, and `multi_edit` show capped in-memory diffs before approval and
  bind execution to the approved file identity; stale targets fail instead of
  overwriting changed state. Successful replacements are atomic.
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

- Added a bounded owner-private tool execution journal for approved
  side-effect-capable calls. Resume reports unresolved starts as uncertain and
  never automatically replays them.
- Added strict capped provider stream/tool-call assembly and split stream parsing
  plus tool-round execution behind focused modules while retaining `Agent` as the
  facade. ChatGPT function calls now require one-to-one reconciliation against a
  successfully completed terminal response; incomplete, failed, truncated, or
  bare-`[DONE]` call streams cannot reach execution.
- Added GPT-5.6 Sol, Terra, and Luna to the ChatGPT and OpenAI catalogs with
  model-specific context/output limits, aliases, native `none` through `xhigh`
  reasoning, long-context pricing, and OpenAI `max_completion_tokens` shaping.
- Added versioned durable-state compatibility fixtures for sessions, provider
  catalog, auth store, tool journal, todo/settings, and checkpoint manifests.
- Extended `dext doctor` with structured policy/source, sandbox enforcement,
  bounded state integrity, unresolved journal, auth-permission, and checkpoint
  findings. Doctor is side-effect-free and does not resolve credential references
  or contact provider endpoints.
- Added tag-gated four-platform release archives, checksums, and GitHub build
  provenance attestations.

### Security

- The default approval profile is now `ask`; `--trust`, `DEXT_TRUST=1`, or an
  explicit `approval=always` remain high-trust opt-ins. Current-run policy always
  wins over approval provenance stored in resumed sessions.
- Native direct-file mutations now reject stale approved state and replace files
  atomically.
- Auth-store inspection reports schema/version/type and owner-only Unix mode
  without resolving environment or command references.
- Agent-run subprocesses now remove credential-shaped environment variables by
  default. Trusted model-invoked tools may explicitly opt in with
  `DEXT_INHERIT_TOOL_CREDENTIALS=1`; hooks and Dext-owned children remain scrubbed.
- Pack helper credentials are restricted to direct helpers from user-owned or
  `DEXT_SHELVES_DIR` packs. Project-local `credential-env` declarations are
  ignored so repository content cannot enable parent credential inheritance.
- Linux Landlock and macOS Seatbelt profiles now preserve every filesystem read
  available to the Dext process user while limiting writes to the roots allowed
  by `read-only` or `workspace-write`.
- Checkpoint manifests and sidecars now use owner-private storage, reject
  symlinked storage paths on Unix, add `/.dext/` to the repository-local Git
  exclude, and automatically retain at most 20 checkpoints for seven days.
- Added least-privilege CI, weekly `cargo audit`, and Cargo/GitHub Actions
  Dependabot configuration.

### Fixed

- ChatGPT/Codex `response.failed` events now retain a bounded provider message so
  generic request-ID failures that explicitly say the request can be retried use
  Dext's existing capped pre-output stream retry instead of being misclassified
  as permanent. Auth, quota, context, post-output, and exhausted-retry failures
  remain terminal.
- Restored consistent read autonomy across native tools, external search tools,
  and shell subprocesses without weakening write confinement; default privacy
  now redacts detected secrets instead of blocking sensitive-looking paths.
  Path-only queued steering is also preserved as literal user input and exact
  user-supplied paths are inspected with native tools before discovery or sudo.
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
  subsequent `bash` tool calls as well as hook processes, without exposing
  declared credential values to hooks.
- Shelf context aggregation now enforces the total byte budget, including
  separators and UTF-8-safe ellipses.
- Windows CI and release jobs now run the scheduler-sensitive bash fast-path
  latency regression alone after the remaining release tests. Its original
  `<90 ms` assertion remains unchanged.
- The tool-call mock provider now consumes its bounded request body before
  responding, preventing Windows resets caused by unread HTTP request data.
