# Security Policy

## Supported versions

Dext is pre-1.0. Security fixes are handled on `main` unless release branches are introduced later.

## Reporting a vulnerability

Do not open a public issue for secrets, credential leakage, prompt/session disclosure, sandbox escape, or provider-auth flaws.

Report privately to the repository owner through GitHub private vulnerability reporting if enabled, or contact the owner directly.

Please include:

- Affected commit/version.
- Operating system and shell.
- Minimal reproduction steps.
- Whether credentials/session exports/logs are involved.
- Any relevant redacted logs.

## Secret handling

Never commit real credentials. The following must remain local/private:

- `.env`
- `.dext/`
- `.dext/checkpoints/` recovery manifests and sidecars
- `~/.dext/auth.json`
- `~/.dext/providers.json` if it contains private endpoints or tokens
- `dext-session-*.jsonl`
- `dext-session-*.html`
- `DEXT.todo.json`
- terminal/session logs and crash snapshots

Use `.env.example` for documented variable names only. Do not put real values there.

## Session export warning

Dext sessions and exports can contain:

- User prompts.
- Model responses.
- Tool inputs/outputs.
- Local paths and filenames.
- Environment snippets.
- Accidentally pasted credentials.

Review and redact before sharing.

## Pre-publish checklist

Before pushing public code:

```bash
git status --short --ignored
git grep -n -I -i -E 'api[_-]?key|secret|token|oauth|authorization|bearer|password|private[_-]?key|refresh[_-]?token|client[_-]?secret'
find . -path ./.git -prune -o -path ./target -prune -o -type f -print
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo deny check licenses
cargo test -p ratatui-core --lib --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

Also scan untracked and ignored files before deciding what to preserve locally vs. delete. Dependency licenses are checked against `deny.toml`; release publication generates `dext.cdx.json` and includes it in checksum and provenance verification. Owner tag creation, immutable asset handling, checksum verification, and GitHub build-provenance verification are documented in [`docs/RELEASING.md`](docs/RELEASING.md). Terminal dependency and renderer changes must also satisfy [`docs/TUI.md`](docs/TUI.md), including its PTY gate and live-terminal acceptance. Published release assets should be used only after both `SHA256SUMS` and `gh attestation verify <asset> --repo SiliconState/Dext` succeed.

## Runtime safety notes

- Project `.env` files are not loaded by Dext. Optional Dext dotenv configuration is loaded only from the user-owned `~/.dext/.env` (or `$DEXT_HOME/.env` when `DEXT_HOME` is already set by the parent environment), so repository content cannot silently change runtime policy or inherited process environment.
- Dext starts with approval profile `ask`. Interactive frontends prompt for gated tools; non-interactive and JSON runs deny rather than wait for input. Automation requiring writes must explicitly select `--approval auto-write`, `--approval always`, or `--trust`.
- Startup precedence is the last CLI policy flag (`--trust`, `--no-trust`, `--approval`, or `--approval-profile`), then a valid `DEXT_APPROVAL`, then true `DEXT_TRUST`, then `ask`. `DEXT_TRUST=1` is an explicit alias for `approval=always`; false values do not opt in. Resume always reapplies the current-run policy and cannot reactivate saved trust grants.
- `--approval never` prevents privileged tool execution. `auto-write` still prompts for Danger-class commands. Dext classifies destructive Git worktree/ref/stash operations (including checkout, switch/reset/branch rewrites, stash flows, ref/tag/worktree changes, per-command config overrides, and unknown aliases/subcommands) as Danger rather than auto-approving them. Because repository configuration can execute pagers, filters, fsmonitor, diff/textconv drivers, hooks, or aliases, shell Git is also Danger unless it uses explicit `git --no-pager` and matches a narrow helper-free metadata-inspection allowlist; commands such as `grep`, `diff-tree`, `ls-files`, `check-ignore`, and `check-attr` remain gated because they can invoke fsmonitor. Use Dext’s hardened native Git tools for review operations. Inline/stdin interpreter execution—including shell input redirections and heredocs—and recognized dynamic/wrapper command paths are also Danger. Dynamic command words include variable/command, glob, brace, tilde, and attached-redirection expansion forms. Actual shell `curl`/`wget`/HTTPie/XH requests are gated because startup configuration, credentials, headers, and implicit request bodies are not safely inferable; use Dext’s native `http` tool for read requests. Interpreter detection covers attached/clustered inline flags and common versioned Python/PyPy/Perl/Node/Ruby/PHP launchers; Windows `.exe`/`.com` command and wrapper matching is case-insensitive.
- `--sandbox-profile read-only` (or `--sandbox read-only`) is recommended for review-only tasks. On supported Linux/macOS hosts, confined profiles preserve every filesystem read available to the Dext process user. `read-only` permits writes only to required scratch/device roots; the default `workspace-write` additionally permits writes under the sandbox root and common per-user toolchain cache roots. `danger-full-access` disables kernel confinement. Dext warns at startup and in `dext doctor` when a confined profile cannot apply kernel enforcement; in that fallback, native write guards remain but shell and external-tool subprocesses are unconfined.
- Run Dext's complete release suite and local install directly in a trusted host terminal or CI. The default sandbox intentionally denies shared `/tmp`, arbitrary PTYs, and Cargo install metadata such as `~/.cargo/.crates.toml`; self-hosted release checks may consequently fail for host-capability reasons. A separate controlled `dext --sandbox-profile danger-full-access --approval always` process may orchestrate this gate. An environment assignment inside an already-confined shell cannot relax its parent kernel sandbox. Do not weaken the default boundary for test convenience.
- Filesystem sandboxing does not restrict outbound network access. The built-in `http` tool blocks loopback, private/CGNAT, unique-local, link-local, and metadata destinations by default. It connects directly and ignores proxy environment variables so destination DNS/IP checks cannot be delegated to a proxy. Its narrowly scoped trusted-network overrides do not constrain provider transport or arbitrary clients run through `bash`.
- Project shelf metadata and conversational auto-invocation of project `PACK.md` workflows require one first-use confirmation for the active repository. Explicit `/pack` or `dext pack` invocation confirms only the selected project workflow; it does not approve unrelated project shelf metadata. `Always` stores a bounded owner-private, single-link, no-follow project-scoped approval marker in Dext state on Unix; unsafe or permissive markers are ignored, and `/project-extensions reset` refuses unsafe marker shapes instead of unlinking through them. Reset also clears a session denial so the next matching use asks again. Before approval, project pack/shelf metadata stays out of the model prompt, cannot shadow same-named trusted user/run metadata, and cannot contribute behavioral tool effects; approved shelf context and always-injected `DEXT.md` guidance are labeled as project-controlled. `PACK.md` and `shelf.json` reads reject symlinks/non-regular files and content over 1 MiB before bounded prompt parsing. Project-local credential declarations remain ignored.
- Provider credentials are used by Dext's HTTP client but credential-shaped environment variables are removed from agent-run subprocesses by default. A pack from a user or `DEXT_SHELVES_DIR` shelf may declare exact names in `credential-env`; matching inherited values then reach only a simple direct invocation of that active pack's own `bin/` helper, not hooks, arbitrary shell commands, prompts, logs, or sessions, and provider-auth names remain excluded. Project-local declarations are ignored so repository content cannot enable parent credential inheritance. Native mutations outside the active sandbox are allowed only below a concrete user pack directory (`~/.dext/shelves/<shelf>/packs/<pack>/...`) containing a regular `PACK.md`, never for shelf metadata or loose files directly under `packs/`; mutation application revalidates the destination and pack marker before atomic replacement. Same-user path races remain outside the isolation boundary and are tracked in the risk register. Set `DEXT_INHERIT_TOOL_CREDENTIALS=1` only for an explicitly trusted model-invoked tool that requires the full parent credential environment; hooks and Dext-owned subprocesses remain scrubbed.
- Privacy redaction is enabled by default while user-readable files remain readable. Tool input and output are redacted before they are exposed to approved `pre_tool`/`post_tool` hooks; output is also redacted before model context or session logs, and hook output then follows the normal result redaction path. Dext replaces private-key blocks, real secret assignments, and explicitly labeled SSNs, payment-card numbers, and account identifiers. Ordinary unlabeled long numbers and decimal market/HTTP values are not classified as cards, and the compact redaction note appears only after an actual replacement. `DEXT_PRIVACY=strict` or `/privacy strict` additionally blocks sensitive-looking native read paths. Disable privacy only when raw, unredacted local data is intentionally required.
- Durable sessions keep a small owner-private tool journal containing bounded metadata and input digests for approved side-effect-capable calls; it excludes raw tool input and output. Resume uses this journal to classify pending transcript calls without replaying them. An unresolved start is an uncertain outcome, not evidence of success. `--no-session` and `--fork` intentionally provide no durable side-effect crash recovery.
- `--trust` and `danger-full-access` are high-trust modes. Use only in controlled environments.
- Dext Git checkpoints are best-effort local recovery aids. They may include
  file content in hidden refs or owner-private `.dext/checkpoints/` sidecars, and they do not
  cover arbitrary external side effects. Write-risk `bash`/`awk`/`csvkit` checkpoints inventory
  up to 500 existing untracked paths, preserve regular files up to 8 MiB each and 32 MiB total,
  and preserve bounded UTF-8 symlink targets without following them. Non-UTF-8 names,
  unsupported types, and path/type/size caps are explicit partial-recovery gaps rather than opaque
  checkpoint failures. Checkpoint storage containers must be real directories; on Unix they must be
  current-user-owned, `.dext` must not be group/world-writable, and checkpoint, sidecar, and blob
  directories are owner-private. Locked mutating operations may repair modes only on managed directories
  already owned by the current user; observational inspection never repairs them, restore rejects
  unsafe sidecar/blob containers, and pruning retains unsafe artifact directories with bounded
  warnings. Regular-file content is stored
  once in owner-private SHA-256-addressed blobs and reused only while both source and blob metadata
  fingerprints remain unchanged; restore and preview rehash blobs, retained checkpoints share
  unchanged blobs, and pruning or a failed checkpoint creation removes valid unreferenced/new blobs.
  Unsafe or malformed entries in the blob or sidecar directories remain untouched for inspection and
  produce bounded warnings without stopping other retention cleanup.
  Owner execute state is retained in metadata, blob integrity is verified before restore and while
  copying into the atomic replacement, and restore fails closed if any declared sidecar is missing
  or corrupt. When path/type/size caps leave partial untracked
  recovery, Dext asks separately before the command, caches approval only for the current
  repository/session, and keeps tracked/staged recovery plus the bounded subset; denial blocks the
  command. Other checkpoint/storage failures remain fail-closed. Checkpoints are created at
  each sequential dispatch boundary, so a later tool call captures earlier mutations from the same
  round. In a repository with no initial commit, Dext blocks a write that would overwrite existing
  worktree/index state because Git cannot provide the normal restore base. A workspace with no
  `.git` marker remains a non-Git no-op without invoking Git, while a discovered but malformed
  repository marker fails closed. Ambient `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` routing
  without a filesystem `.git` marker also fails loudly: Dext-owned Git commands intentionally scrub
  those variables and will not silently treat such a routed repository as checkpoint-protected.
  Dext excludes `/.dext/`
  through the repository-local Git exclude file and automatically retains no more than 20
  checkpoints for seven days. Never mirror-push `refs/dext/*`.
- OAuth/API-key login should use Dext's official CLI/slash flows. Do not copy credentials from unrelated tools or stores. Stored API-key references beginning with `!` intentionally execute the remainder through `bash -lc`; therefore `~/.dext/auth.json` integrity is code-execution-sensitive. Dext saves this file as an owner-only regular file on Unix and `dext doctor` reports missing, unsafe, uninspectable, or invalid state without resolving references or printing secrets. Windows ACLs are reported as not evaluated rather than claimed secure.
- `dext doctor` is observational and bounded to active/latest state. It does not repair or rewrite files, execute environment or `!command` credential references, contact model/local-provider endpoints, or print secret-bearing JSON. Its warning count is textual; warnings retain exit status 0 for script compatibility.

## Known limitations

The maintained cross-domain register is [`docs/RISK_REGISTER.md`](docs/RISK_REGISTER.md). This section expands the checkpoint-restore limitation because it needs operational handling during recovery. Documentation drift is governed by the canonical `docs/index.html` same-change rule and is not an operational risk entry.

### Concurrent same-user checkpoint restore mutation

Checkpoint restore is not a security boundary against another process running as the same operating-system user. Dext serializes its own checkpoint operations, validates bounded checkpoint refs and manifests, uses literal Git pathspecs for declared paths, rejects unsafe symlink and hardlink destinations, preflights all declared restore paths before mutation, and atomically replaces each untracked sidecar file or symlink on supported platforms. However, tracked worktree restoration is delegated to path-based Git commands. A concurrent same-user process can change or replace repository directories after preflight and before or during a multi-path restore. The restore may then fail after applying only some paths; on platforms or filesystems with weaker path-resolution guarantees, the concurrent mutation may also redirect a path operation within the authority already held by that user.

Treat repositories being restored as trusted, quiescent workspaces. Stop editors, build tools, hooks, and other agents that may rewrite the worktree; review the restore preview; preserve the checkpoint ref; and verify `git status` and the restored files afterward. If hostile same-user concurrency is in scope, do not rely on checkpoint restore for isolation—use a separate operating-system account, container, or virtual machine. Fully closing this race requires descriptor-relative, no-follow filesystem traversal and restore operations rather than additional path-based preflight checks.
