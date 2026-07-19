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
cargo test -p ratatui-core --lib --locked
cargo build --release --locked
cargo test --release --locked
cargo test --release --locked --test tui_smoke -- --nocapture
```

Also scan untracked and ignored files before deciding what to preserve locally vs. delete. Owner tag creation, immutable asset handling, checksum verification, and GitHub build-provenance verification are documented in [`docs/RELEASING.md`](docs/RELEASING.md). Terminal dependency and renderer changes must also satisfy [`docs/TUI.md`](docs/TUI.md), including its PTY gate and live-terminal acceptance. Published release archives should be used only after both `SHA256SUMS` and `gh attestation verify <archive> --repo SiliconState/Dext` succeed.

## Runtime safety notes

- Project `.env` files are not loaded by Dext. Optional Dext dotenv configuration is loaded only from the user-owned `~/.dext/.env` (or `$DEXT_HOME/.env` when `DEXT_HOME` is already set by the parent environment), so repository content cannot silently change runtime policy or inherited process environment.
- Dext starts with approval profile `ask`. Interactive frontends prompt for gated tools; non-interactive and JSON runs deny rather than wait for input. Automation requiring writes must explicitly select `--approval auto-write`, `--approval always`, or `--trust`.
- Startup precedence is the last CLI policy flag (`--trust`, `--no-trust`, `--approval`, or `--approval-profile`), then a valid `DEXT_APPROVAL`, then true `DEXT_TRUST`, then `ask`. `DEXT_TRUST=1` is an explicit alias for `approval=always`; false values do not opt in. Resume always reapplies the current-run policy and cannot reactivate saved trust grants.
- `--approval never` prevents privileged tool execution.
- `--sandbox-profile read-only` (or `--sandbox read-only`) is recommended for review-only tasks. On supported Linux/macOS hosts, confined profiles preserve every filesystem read available to the Dext process user. `read-only` permits writes only to required scratch/device roots; the default `workspace-write` additionally permits writes under the sandbox root and common per-user toolchain cache roots. `danger-full-access` disables kernel confinement. Dext warns at startup and in `dext doctor` when a confined profile cannot apply kernel enforcement; in that fallback, native write guards remain but shell and external-tool subprocesses are unconfined.
- Run Dext's complete release suite and local install directly in a trusted host terminal or CI. The default sandbox intentionally denies shared `/tmp`, arbitrary PTYs, and Cargo install metadata such as `~/.cargo/.crates.toml`; self-hosted release checks may consequently fail for host-capability reasons. A separate controlled `dext --sandbox-profile danger-full-access --approval always` process may orchestrate this gate. An environment assignment inside an already-confined shell cannot relax its parent kernel sandbox. Do not weaken the default boundary for test convenience.
- Filesystem sandboxing does not restrict outbound network access. The built-in `http` tool blocks loopback, private/CGNAT, unique-local, link-local, and metadata destinations by default. It connects directly and ignores proxy environment variables so destination DNS/IP checks cannot be delegated to a proxy. Its narrowly scoped trusted-network overrides do not constrain provider transport or arbitrary clients run through `bash`.
- Provider credentials are used by Dext's HTTP client but credential-shaped environment variables are removed from agent-run subprocesses by default. An environment-selected, user-global, or bundled pack may declare exact names in `credential-env`; matching inherited values then reach only a simple direct invocation of that active pack's own `bin/` helper, not hooks, arbitrary shell commands, prompts, logs, or sessions, and provider-auth names remain excluded. Project-local declarations are ignored so repository content cannot enable parent credential inheritance. Set `DEXT_INHERIT_TOOL_CREDENTIALS=1` only for an explicitly trusted model-invoked tool that requires the full parent credential environment; hooks and Dext-owned subprocesses remain scrubbed.
- Privacy redaction is enabled by default while user-readable files remain readable. Before tool results enter model context or session logs, Dext replaces private-key blocks, real secret assignments, and explicitly labeled SSNs, payment-card numbers, and account identifiers. Ordinary unlabeled long numbers and decimal market/HTTP values are not classified as cards, and the compact redaction note appears only after an actual replacement. `DEXT_PRIVACY=strict` or `/privacy strict` additionally blocks sensitive-looking native read paths. Disable privacy only when raw, unredacted local data is intentionally required.
- Durable sessions keep a small owner-private tool journal containing bounded metadata and input digests for approved side-effect-capable calls; it excludes raw tool input and output. Resume uses this journal to classify pending transcript calls without replaying them. An unresolved start is an uncertain outcome, not evidence of success. `--no-session` and `--fork` intentionally provide no durable side-effect crash recovery.
- `--trust` and `danger-full-access` are high-trust modes. Use only in controlled environments.
- Dext Git checkpoints are best-effort local recovery aids. They may include
  file content in hidden refs or owner-private `.dext/checkpoints/` sidecars, and they do not
  cover arbitrary external side effects. Dext excludes `/.dext/` through the repository-local Git exclude file and automatically retains no more than 20 checkpoints for seven days. Never mirror-push `refs/dext/*`.
- OAuth/API-key login should use Dext's official CLI/slash flows. Do not copy credentials from unrelated tools or stores. Stored API-key references beginning with `!` intentionally execute the remainder through `bash -lc`; therefore `~/.dext/auth.json` integrity is code-execution-sensitive. Dext saves this file as an owner-only regular file on Unix and `dext doctor` reports missing, unsafe, uninspectable, or invalid state without resolving references or printing secrets. Windows ACLs are reported as not evaluated rather than claimed secure.
- `dext doctor` is observational and bounded to active/latest state. It does not repair or rewrite files, execute environment or `!command` credential references, contact model/local-provider endpoints, or print secret-bearing JSON. Its warning count is textual; warnings retain exit status 0 for script compatibility.

## Known limitations

The maintained cross-domain register is [`docs/RISK_REGISTER.md`](docs/RISK_REGISTER.md). This section expands the checkpoint-restore limitation because it needs operational handling during recovery. Documentation drift is governed by the canonical `docs/index.html` same-change rule and is not an operational risk entry.

### Concurrent same-user checkpoint restore mutation

Checkpoint restore is not a security boundary against another process running as the same operating-system user. Dext serializes its own checkpoint operations, validates checkpoint refs and manifests, rejects unsafe symlink and hardlink destinations, preflights all declared restore paths before mutation, and atomically replaces each untracked sidecar file. However, tracked worktree restoration is delegated to path-based Git commands. A concurrent same-user process can change or replace repository directories after preflight and before or during a multi-path restore. The restore may then fail after applying only some paths; on platforms or filesystems with weaker path-resolution guarantees, the concurrent mutation may also redirect a path operation within the authority already held by that user.

Treat repositories being restored as trusted, quiescent workspaces. Stop editors, build tools, hooks, and other agents that may rewrite the worktree; review the restore preview; preserve the checkpoint ref; and verify `git status` and the restored files afterward. If hostile same-user concurrency is in scope, do not rely on checkpoint restore for isolation—use a separate operating-system account, container, or virtual machine. Fully closing this race requires descriptor-relative, no-follow filesystem traversal and restore operations rather than additional path-based preflight checks.
