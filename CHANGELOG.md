# Changelog

## Unreleased

### Changed

- Rebuilt the public README around a concise product overview, real Dext frontend captures, direct quick start, plain safety boundaries, and links to focused references instead of duplicating the technical documentation. Follow-up review replaced the overbroad “project-aware memory” shorthand with explicit project-scoped autosave/resume and user-authored recall/Seat semantics, changed paste-ready auth examples to keep credentials out of shell arguments, and made incomplete CLI login guidance point to Dext's `/login` paste path for API keys or manual OAuth callbacks. Added lightweight Linux/macOS and Windows installers that require exact release tags and safe regular-file destinations, select a matching release, verify `SHA256SUMS`, validate the candidate's reported version before replacing an existing binary, install per-user, and use a documented exact-revision locked source fallback when no tagged release is available; Windows uses `File.Replace` when supported and a rollback-preserving rename fallback only for explicit unsupported-operation failures, retaining a recoverable backup if rollback fails. Fixed the public `irm | iex` path under Windows PowerShell 5.1 by replacing nullable modern-.NET architecture metadata with native Windows environment detection, including 32-bit PowerShell on 64-bit Windows. Attestation-required mode fails closed instead of using an unattested source build. CI now executes offline Unix and Windows installer harnesses covering success, no-clobber failures, strict tags/destinations, source pinning/fallback disablement, malformed API state, version mismatch, and Windows replacement rollback/recovery; both Windows PowerShell 5.1 and PowerShell 7 parse and execute the complete harness plus an in-memory install matching `irm | iex`. Release tags must be annotated and point to a commit contained in `origin/main`; Pages deployment has a bounded 15-minute action deadline within a 20-minute job. For the `v0.1.0` publication, Windows joined Ubuntu/macOS as a required `main` check, `v*` tags gained update/deletion protection, immutable releases and private vulnerability reporting were enabled, and vulnerability alerts plus Dependabot security updates were activated; R-008 now tracks only the residual trusted-maintainer boundary for initial tag creation.

- Pack Runtime Protocol v1 now fails closed across the full lifecycle: recursive schemas accept only the implemented keyword subset; native-name and host approval-operation collisions use the full occupied catalog during activation and resume; executable bytes are hashed, displayed at approval, and rechecked before every call; prompt-level `Always` approval is scoped to that exact runtime identity; changing approval or sandbox policy revokes the active executable runtime and queued callbacks; current-run approval/sandbox policy remains authoritative during resume and saved grants are discarded before restoration; restoration preflights exact canonical pack-directory/source identity, manifest/hash/state accounting, project trust, and executable approval before mutating the live agent; activation/idle/read events enforce read-only confinement inside the executor; protocol-sized stdout is preserved and content/effects/queued prompts reject unsafe terminal controls; malformed timeout overrides fail closed; stdin delivery and root execution share the configured deadline, and output drain after process-tree cleanup has a separate one-second cap; state/effects/continuation accounting applies atomically; and pending continuations persist, cancel/refund on interrupt, and remain bounded. Runtime calls also persist state/results without read-tool debounce, surfaced lifecycle errors are privacy-redacted, `/allow`/`/allowed` recognize active dynamic tools, and any runtime-bearing session uses format v5 so pre-runtime binaries reject rather than silently discard executable-runtime state.

- Autoresearch now requires an initial Git commit and a real non-symlink `.auto` directory, accepts measurement/check/hook programs only as executable regular non-symlink files, requires each run to be logged before another run or reinitialization, rejects new runs while stopped/capped, restores the complete persisted segment/cap/continuation/stopped state plus outstanding measured results, caps and validates state/records/metric cardinality, treats measured metrics as authoritative, permits `keep` only for an actual improvement, validates status/result consistency, records unmeasured crashes without fabricating metrics, keeps confidence finite/JSON-safe, and treats post-persistence hook failures as nonfatal steering. Its append-only `.auto/log.jsonl` is bounded regular no-follow/single-link evidence outside keep commits; runtime-owned Git operations scrub ambient routing/credentials, suppress hooks/fsmonitor/signing/external diff/filters, parse NUL-delimited dirty paths, and keep stdin, root execution, and post-exit pipe handling bounded. Only one active autoresearch session is supported per working tree.

- ChatGPT Responses finalize errors for malformed function-call arguments now use one bounded fail-closed recovery when no visible text streamed: Dext compacts once if a safe split exists, retries exactly once without executing the invalid call, and surfaces a repeated protocol error instead of stopping immediately or looping.

- The pack and shelf registry summaries moved from the volatile environment tail
  into the cached system block, so they are no longer re-billed at full input
  rate on every tool round. The shared prompt-scan cache is rebuilt each user
  turn and invalidated after mutation-capable tools or approved hooks, keeping
  tool-created or edited packs visible on the next provider request; shelf
  summary generation is also cached instead of recomputed per request.
- Objective checkpoints now avoid treating the descriptive word `verifiable` as
  a verification request, recognize explicit non-code verification reports such
  as “checks pass,” and refresh the work ledger immediately after each tool
  round. This prevents a completed custom verification from remaining
  `[unresolved]` in the next model request.
- Hardened and accelerated the built-in `http` client without changing provider,
  OAuth, or local-context transport: HTTP/2 and gzip/Brotli decoding are enabled
  only for the tool; connect/read inactivity, resolver work, total request time,
  response source, and declared body sizes are bounded; `--extract-text` reads a
  128 KB head instead of draining oversized pages; and raw output retains its
  smaller head/tail context cap. One bounded 60-second DNS cache retains at
  most 32 addresses only after validating each complete DNS answer and limits
  lingering libc lookups. IPv4 current-network/broadcast, IP multicast, and
  IPv6 unspecified destinations remain blocked under every trusted-network
  override. Duplicate,
  transport/framing, and method-override headers plus URL credentials are
  rejected while ordinary headers can override defaults. Headerless/bodyless
  GET and HEAD requests may follow validated cross-origin redirects without an
  automatic `Referer`; HTTPS downgrades are blocked, while requests with custom
  headers, bodies, or other methods remain same-origin so arbitrary credentials
  and 307/308 bodies cannot be replayed across origins. URL details are removed
  from transport errors, body-bearing nominal read methods require Danger
  approval, and HTTPie-style delimiters are resolved by their earliest operator
  so padded auth headers and typed/query values are not misclassified.
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

- Replaced `/plan` with conversational planning turn policies. The objective
  tracker now classifies planning/analysis-only prompts (including explicit
  “don’t change anything” phrasing) and bare plan approvals (“go”, “proceed
  with the plan”); Dext injects a matching advisory-only or
  implementation-authorized turn policy into the volatile runtime status —
  never the cached stable prompt — so weaker models get deterministic per-turn
  structure without hard tool gating. Explicit mutation intent always
  overrides advisory phrasing, question-phrased prompts are never read as
  approvals, and mid-turn queued user updates re-evaluate the policy;
  approval prompts and `/sandbox-profile read-only` remain the enforcement
  layers.

### Removed

- Removed the completed repository-local `.auto` prompt-efficiency experiment scaffold and now ignore the entire root `.auto/` workspace. Dext runtime/build/CI and the user-owned autoresearch pack do not depend on those project experiment files.

- Retired the unused work-map/session-navigation experiment: `/map`, `/packet`,
  `/focus`, `/tracks`, `/track`, `/branches`, their CLI/session aliases,
  waypoint metadata, event surface, and TUI drawer. Existing session JSONL still
  loads; retired `track_origin` metadata is ignored and omitted when rewritten.
  Read-only session inspection remains available through list, brief, analyze,
  grep, failures, verification, decisions, and export.
- Retired the pre-JSON 8- and 9-field checkpoint manifest encodings and the
  9-field JSON form; only the 11- and 12-field rows this build writes are
  accepted. This deletes the second, weaker path-validation rule those rows
  selected — a manifest can no longer choose how strictly its own paths are
  checked — along with the ambiguous relative-hint resolution they required.
  A recognized retired row is skipped with a warning instead of failing the
  whole listing, so one stale row can no longer take out `/undo` and block every
  write-risk tool. Recognition requires an intact checkpoint header; the
  recorded OID must match any live ref, and normal retention removes the matched
  retired ref with its manifest row. Genuine corruption or tampering still
  fails closed. The `legacy_sidecar_paths`
  field is renamed `direct_sidecar_paths`: despite the old name it is the
  current encoding's exact sidecar-membership index and is load-bearing for
  fail-closed restore.
- Removed the subagent feature completely: `/subagent` slash command,
  `subagent-runtime` CLI subcommand, detached/inline runners, steering,
  quality gates, TUI state, session artifacts dir, and all associated
  tests/fixtures. Net -1544 lines.
- Removed the unused `/plan` slash command and its hidden read-only planner turn,
  temporary agent-state swapping, duplicated CLI/TUI dispatch, completion entry,
  welcome tip, and planner-only regression test. Planning is now an ordinary
  conversation: ask Dext to inspect and propose a plan without editing, revise
  it in context, then tell it to proceed. A former `/plan ...` input is no
  longer intercepted and is delivered as a normal prompt.
- Removed all repository-owned and embedded pack payloads. Dext ships the pack
  lifecycle and shelf integration, but no pack content; users own and
  distribute shelf repositories separately.

### Added

- Added Pack Runtime Protocol v1. Reviewed packs may declare a bounded `runtime.json` one-shot native helper that exposes dynamic tools while active and returns bounded state, steering, delayed continuation, and Markdown views. Runtime activation has separate executable-code approval; activation/idle/read calls are read-only-confined and credential-scrubbed; declared write/danger tools retain normal approval, sandbox, side-effect journaling, and fail-closed Git checkpoints. Session restore re-resolves the pack and requires exact source/manifest-hash identity.

- Added project-scoped Seats as durable agent identities across disposable sessions. `--seat NAME` starts a seated session, `--seat NAME --resume` resumes its latest incarnation, `dext seat list|show` inspects records, and `dext seat set` maintains bounded labels/summaries. Portable ids, owner-safe/private state paths, atomic metadata updates, deferred record creation, 256 KiB session-header bounds, transactional reset pointer handling, and cross-Seat/cross-project/provenance checks fail closed. Plain unseated writes retain v3 compatibility and Seat-only writes use v4; runtime-bearing writes use v5, while transitional v3 Seat headers remain validated and loadable. No-session/forked runs do not advance Seat state. Crew supplies direct or deterministic fallback role identities while pinning one absolute Dext state root across captured, detached, and pane workers.
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
- Added a bounded owner-private tool execution journal for approved
  side-effect-capable calls. Resume reports unresolved starts as uncertain and
  never automatically replays them.
- Added strict capped provider stream/tool-call assembly and split stream parsing
  plus tool-round execution behind focused modules while retaining `Agent` as the
  facade. ChatGPT function calls now require one-to-one reconciliation against a
  successfully completed terminal response; incomplete, failed, truncated, or
  bare-`[DONE]` call streams cannot reach execution.
- Added GPT-5.6 Sol, Terra, and Luna to the ChatGPT and OpenAI catalogs with
  model-specific context/output limits, aliases, long-context pricing, and
  provider-isolated reasoning controls. Official OpenAI API-key GPT-5.6 requests
  now use `/v1/responses`: `--effort max` sends native `reasoning.effort=max`,
  while independent `--reasoning-mode standard|pro` / `/reasoning-mode` control
  `reasoning.mode`; summaries use the same contract. ChatGPT OAuth retains its
  Codex Responses contract, `none` through `xhigh` effort (`max` maps to
  `xhigh`), and receives no Platform-only mode field. Other models, custom
  endpoints, and built-in providers retain their prior request shapes.
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
- Added least-privilege CI, weekly vulnerability and dependency-license checks,
  Cargo/GitHub Actions Dependabot configuration, and a checksummed, attested
  CycloneDX SBOM for release publication.

### Fixed

- Pricing override tests now exercise pure override/default composition without
  mutating process-global pricing environment variables, preventing parallel
  local-provider and cloud-pricing tests from observing transient prices.
- Windows checkpoint cache tests now compare normalized path identities instead of
  treating Git's forward-slash path and the filesystem's verbatim `\\?\` path as
  different repositories.
- Cross-platform CI now avoids compiling Unix-only executable restoration metadata
  as an unused Windows value, retains the session operation-lock file handle on
  every platform without dead-code warnings, and runs the invalid-UTF-8 filename
  fixture only on Unix filesystems that permit creating that fixture; macOS APFS
  rejects the filename before Dext can inspect it.
- Session pruning now preserves every project directory containing session or
  other project state and removes only stale locks plus stale lock-only directory
  trees. Session open, stale reclamation, cleanup, and prune share an
  owner-private cross-process operation lock; stale deletion revalidates token
  and PID identity under that lock so a replacement live lock cannot be removed.
  Session briefs now carry an explicit privacy reminder because distilled ledger,
  path, failure, and verification data may still be sensitive.
- Interrupting a parallel built-in tool round now actually cancels it. Queued
  calls previously acquired their concurrency permit and began executing after
  Ctrl-C, `read_file`/`read_symbol`/`todo_read`/`rg`/`fd` had incomplete
  cancellation paths, and the round waiter noticed an interrupt only after some
  task completed. The waiter now polls cancellation independently of task
  completion, in-flight tasks are aborted, a call that wins its permit after the
  interrupt refuses to start, and every abandoned `tool_use` id is reported as
  interrupted rather than as an unknown outcome. Native file loading checks
  cancellation between bounded chunks; `read_file` stops once it detects data
  beyond an explicit limit and advances past a single over-cap line,
  `read_symbol` rejects source inputs above 8 MiB, and todo state is capped at
  256 KiB across tool/prompt/TUI loading. Each ancestor `DEXT.md`/`recall.md`
  input is a regular non-symlink file capped at 1 MiB for both prompt loading and
  session provenance hashing. Zero, mistyped,
  overflowing, or out-of-range native read selectors now fail instead of being
  clamped or silently defaulted.
- Privacy-strict search scope now recognizes compact ripgrep globs such as
  `-g.env`/`-ig .env`, wildcard-prefixed sensitive globs such as `*.env`,
  `.env.*` variants, and attached operand-changing `-ePATTERN`/`-fFILE` forms,
  while respecting where an attached short-option value begins, closing
  spelling-dependent bypasses without treating letters inside values as flags.
- Combined budget caps now accept the documented compact `t` token suffix and
  reject duplicate dimensions, empty components, and unrepresentable token
  counts instead of silently retaining or saturating a value. Invalid
  `DEXT_BUDGET_CAP` configuration now fails startup instead of disabling the
  guard. Resumed session
  headers reject invalid persisted usage costs and empty/non-positive caps.
- JSON and stream-JSON sinks now record each structural crash breadcrumb exactly
  once; text-mode delegation no longer double-records the same event.
- Child process-tree guards now terminate unfinished Unix process groups when
  dropped during cancellation or unwinding, closing the lifecycle gap between
  explicit exit/timeout/interrupt cleanup paths.
- Recovery checkpoint loading no longer blocks write-risk tools on a recognized
  retired manifest row: recognition validates the complete retired field grammar
  as well as the header, preview omits untracked snapshot entries with unsafe
  host-native targets, malformed rows fail closed, and runtime manifest reads
  are capped at 16 MiB. Retention durably compacts the manifest before deleting
  expired or retired refs, so a later cleanup failure cannot leave `/undo`
  naming an already-deleted ref. Matched retired refs are removed when retention
  compacts their rows. (The pre-JSON row support this entry originally described
  was retired before release; see Removed.)
- Official OpenAI GPT-5.6 Responses requests use flat function tools with
  `strict:false`, explicitly request opaque encrypted reasoning state for
  stateless tool continuation, preserve valid returned state only across the
  current tool turn, and reject or recover incomplete/content-filter terminal
  states without executing unfinished calls. Content-filter terminals discard
  visible output, tool calls, and opaque reasoning state. GPT-5.6 compaction now
  preserves the selected Standard/Pro mode, and Responses main requests and
  summaries resolve effort through the selected model's advertised levels before
  nearest-level fallback; `off` sends `none` only when the model advertises it.
  This avoids unsupported raw effort values and avoids promoting Low/Medium to
  High when exact levels exist. OpenAI-local `gpt56*` aliases now resolve consistently;
  only the four supported GPT-5.6 ids auto-select the official Responses route
  or supported-variant effort mapping. `DEXT_COMPACT_MODEL` aliases are
  normalized before routing, reasoning-capable Responses summaries retain the
  larger summary allowance even when main effort is Off, explicitly
  non-reasoning Responses models omit the reasoning object, and summary usage is
  priced against the resolved summary model instead of the main model.
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
- Local llama.cpp context discovery now owns its blocking HTTP client on a
  dedicated thread, so an offline local provider cleanly falls back instead of
  panicking when the probe runs inside Tokio.
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
- User-shelf native mutations now require content below a concrete pack
  directory containing a regular `PACK.md`; shelf metadata and loose files
  directly under `packs/` remain outside the write exception, and application
  revalidates the destination and marker before atomic replacement.
- Backend viewer output now normalizes CRLF across arbitrary stream chunks
  without inserting blank rows.
- Todo empty-state parsing now matches generated sentinel lines exactly, and
  failed native edit blocks explicitly report that no edits were applied.
- The main TUI status row now keeps a live cumulative agent-active elapsed
  clock at its right edge. It advances while Dext handles a turn, then pauses
  and hides while Dext waits idle for user input; the live todo fallback shows
  completed/total progress as an up-to-seven-cell battery.
- The alternate-screen backend viewer now visually matches the main TUI with a
  Dext header, agent-active clock, command summary, styled stdout/stderr lanes,
  command position, and compact controls while preserving its existing event,
  selection, scrolling, and security behavior.
- The inline startup welcome now presents an adaptive brand/location row, exactly
  two Model/Approval facts, and a session-rotated verified tip; narrow terminals
  drop the location segment, terminal-cell width drives alignment/truncation,
  and Git status probing no longer blocks the render loop. The empty composer
  also advertises `@ files` and `/ commands` beside its request prompt.
- Todo progress batteries now track short lists one cell per task and scale
  proportionally up to a seven-cell cap for longer lists, while preserving
  visibly incomplete progress. The inline welcome now owns its blank transcript
  separator so CLI approval and sandbox diagnostics remain visually distinct
  through inline viewport placement and replay.
