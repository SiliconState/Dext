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
cargo install --path . --force
```

## Before editing

- Locate the relevant code with `rg`, `read_symbol`, or focused file reads.
- Keep edits surgical.
- Prefer existing modules over new files unless a boundary is clearly stable.
- Avoid adding overlapping tools or prompt/schema bloat.
- Do not commit runtime clutter, generated logs, session exports, screenshots, credentials, or local auth stores.

## Code style

- Names should explain ordinary behavior.
- Add comments only for non-obvious invariants, security reasoning, or platform footguns.
- No backwards-compatibility shims for unreleased internal behavior.
- Keep terminal/TUI behavior text-first and sparse.
- Use Dext's built-in HTTP implementation rather than shelling to curl for core behavior.

## Verification

Run the narrowest useful check first, then the release checks before publishing:

```bash
cargo fmt
cargo build --release
cargo test --release
```

If `src/tui.rs` changed:

```bash
cargo test --release --test tui_smoke -- --nocapture
```

After code changes intended for interactive use:

```bash
cargo install --path . --force
```

On Windows, if install fails with `Access is denied`, close running `dext.exe` processes and retry.

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
