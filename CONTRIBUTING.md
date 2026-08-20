# Contributing

Dext is a compact Rust terminal agent. Keep changes source-first, reviewable, and low-bloat.

## Development setup

```bash
git clone https://github.com/SiliconState/Dext.git
cd Dext
cargo build
```

Install the local binary:

```bash
cargo install --path . --force --locked
```

## Before editing

- Locate the relevant code with `rg`, `read_symbol`, or focused file reads.
- Keep edits surgical.
- Prefer existing modules for small changes; when an existing file mixes several stable domains, extract one focused module instead of extending a monolith.
- Avoid adding overlapping tools or prompt/schema bloat.
- Do not commit runtime clutter, generated logs, session exports, credentials, local auth stores, repository-local `.auto/` experiment workspaces, or one-off screenshots. Curated product screenshots used by README/docs are reviewed documentation assets.
- Treat `docs/index.html` as the canonical main technical documentation. Update it in the same change as user-visible/runtime/architecture/security/provider/tool/test/CI/release behavior, plus any focused Markdown guide for that subject.
- Update `docs/RISK_REGISTER.md` when a non-documentation risk, control, owner, likelihood, impact, or review trigger changes. Do not add documentation-drift risks; prevent them with the canonical-page same-change rule.

## Code style

- Names should explain ordinary behavior.
- Add comments only for non-obvious invariants, security reasoning, or platform footguns.
- No backwards-compatibility shims for unreleased internal behavior.
- Keep terminal/TUI behavior text-first and sparse.
- Installers in `scripts/` are user-facing supply-chain code. Keep them dependency-light, checksum-verifying, per-user by default, require exact release tags and safe regular-file destinations, validate selected release versions before replacement, preserve a recoverable prior binary across non-atomic fallback failures, and cover release/fallback/failure paths with platform-native functional CI tests. Windows installer changes must parse and execute the complete in-memory installation harness under both inbox Windows PowerShell 5.1 and PowerShell 7.
- Use Dext's built-in HTTP implementation rather than shelling to curl for core behavior.

## Verification

Run the narrowest useful check first, then the release checks before publishing:

```bash
cargo fmt --all -- --check
cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings
cargo audit --deny warnings
cargo deny check licenses
cargo test -p ratatui-core --lib --locked
cargo bench --no-run --locked
cargo build --release --locked
cargo test --release --locked
```

If `src/tui.rs`, a terminal dependency, or the vendored Ratatui patch changed:

```bash
cargo test --release --locked --test tui_smoke -- --nocapture
```

Follow the renderer contract and live-terminal acceptance in [`docs/TUI.md`](docs/TUI.md).

After code changes intended for interactive use:

```bash
cargo install --path . --force --locked
```

On Windows, branch CI also runs the native descendant-lifecycle test for kill-on-close Job Objects. If install fails with `Access is denied`, close running `dext.exe` processes and retry.

Run the complete release suite and install directly in the active environment. The default `danger-full-access` profile already adds no filesystem confinement and therefore needs no special release escape process. If you explicitly select `workspace-write` or `read-only`, shared `/tmp`, arbitrary PTYs, and Cargo metadata outside approved roots may be denied; run the final gate under the default profile or directly in the parent environment instead. A child cannot relax container, VM, namespace, seccomp, Seatbelt, service-account, or host restrictions inherited from its parent.

## Security hygiene

Before committing/pushing, inspect:

```bash
git status --short --ignored
git diff --stat
git diff
git grep -n -I -i -E 'api[_-]?key|secret|token|oauth|authorization|bearer|password|private[_-]?key|refresh[_-]?token|client[_-]?secret'
```

Do not commit:

- `.env`
- `.dext/`
- `.dext/checkpoints/`
- `.auto/`
- `target/`
- `dext-session-*`
- `DEXT.todo.json`
- one-off scratch files

## Pull request expectations

Include:

- What changed.
- Why it changed.
- Verification commands run.
- Any security/session-state implications.
- Canonical `docs/index.html` and focused-guide updates for changed behavior.
- Risk-register updates for changed open non-documentation risks, or a statement that none changed.
