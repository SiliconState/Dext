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
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
cargo audit --deny warnings
```

Also scan untracked and ignored files before deciding what to preserve locally vs. delete.

## Runtime safety notes

- The approval profile defaults to `ask`, but startup trust mode is enabled by
  default and auto-approves gated tools. Use `--no-trust` or `DEXT_TRUST=0` when
  interactive approval is required.
- `--approval never` prevents privileged tool execution.
- `--sandbox-profile read-only` (or `--sandbox read-only`) is recommended for review-only tasks. On supported Linux/macOS hosts, confined profiles hide unrelated content below the user's home. The default `workspace-write` profile permits writes only under the sandbox root, scratch roots, and common per-user toolchain cache roots. `danger-full-access` disables kernel confinement.
- Provider credentials are used by Dext's HTTP client but credential-shaped environment variables are removed from agent-run subprocesses by default. Set `DEXT_INHERIT_TOOL_CREDENTIALS=1` only for an explicitly trusted tool that requires them.
- Privacy redaction and sensitive native-read path guards are enabled by default. Disable them only when raw local data is intentionally required.
- `--trust` and `danger-full-access` are high-trust modes. Use only in controlled environments.
- Dext Git checkpoints are best-effort local recovery aids. They may include
  file content in hidden refs or owner-private `.dext/checkpoints/` sidecars, and they do not
  cover arbitrary external side effects. Dext excludes `/.dext/` through the repository-local Git exclude file and automatically retains no more than 20 checkpoints for seven days. Never mirror-push `refs/dext/*`.
- OAuth/API-key login should use Dext's official CLI/slash flows. Do not copy credentials from unrelated tools or stores.
