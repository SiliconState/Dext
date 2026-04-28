# Security Policy

## Supported versions

Wolf is pre-1.0. Security fixes are handled on `master` unless release branches are introduced later.

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
- `.wolf/`
- `.pi/`
- `~/.wolf/auth.json`
- `~/.wolf/providers.json` if it contains private endpoints or tokens
- `wolf-session-*.jsonl`
- `wolf-session-*.html`
- `WOLF.todo.json`
- terminal/session logs and crash snapshots

Use `.env.example` for documented variable names only. Do not put real values there.

## Session export warning

Wolf sessions and exports can contain:

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
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
```

Also scan untracked and ignored files before deciding what to preserve locally vs. delete.

## Runtime safety notes

- `--approval ask` is the conservative default for privileged tools.
- `--approval never` prevents privileged tool execution.
- `--sandbox read-only` is recommended for review-only tasks.
- `--fangs-out` and `danger-full-access` are high-trust modes. Use only in controlled environments.
- OAuth/API-key login should use Wolf's official CLI/slash flows. Do not copy credentials from unrelated tools or stores.
