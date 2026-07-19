---
name: agent-browser
description: Drive websites with Vercel Labs' native Rust agent-browser CLI using snapshots and semantic refs.
---

# Agent Browser

Use the upstream native Rust [`agent-browser`](https://github.com/vercel-labs/agent-browser) CLI for rendered-page interaction, authenticated browser sessions, screenshots, forms, and browser-based testing.

## Setup

The pack helper expects the upstream binary on `PATH`, in `~/.cargo/bin`, or at `AGENT_BROWSER_BIN`.

```bash
cargo install agent-browser --locked
agent-browser install
# Linux only, when browser libraries are missing and the user approves system changes:
agent-browser install --with-deps
```

The npm and Homebrew distributions also install the same native CLI, but Cargo is preferred when a Rust-native installation is requested.

## Invocation

The pack is available without a runtime browser toggle or provider-visible browser tool:

```text
/pack agent-browser inspect https://example.com
```

or:

```bash
dext pack agent-browser "inspect https://example.com"
```

Within this pack, call the helper explicitly:

```bash
AB="$DEXT_PACK_DIR/bin/agent-browser"
"$AB" --help
```

## Workflow

1. Derive one worktree-scoped session and reuse it for every command:

   ```bash
   AB="$DEXT_PACK_DIR/bin/agent-browser"
   SESSION="$("$AB" session id --scope worktree --prefix dext)"
   ```

2. Open the page, snapshot interactive elements, act through semantic refs, and re-snapshot after every page change:

   ```bash
   "$AB" --session "$SESSION" open https://example.com
   "$AB" --session "$SESSION" snapshot -i -u
   "$AB" --session "$SESSION" click @e2
   "$AB" --session "$SESSION" snapshot -i -u
   ```

3. Prefer `snapshot` refs or semantic `find role|text|label` locators. Use CSS selectors only as a fallback.
4. Use `read <url>` for documentation and text extraction when browser interaction is unnecessary.
5. Keep output bounded with focused snapshots, selectors, `--max-output`, or `--json` plus narrow processing.
6. Close the session when finished, including error paths:

   ```bash
   "$AB" --session "$SESSION" close
   ```

## Safety

- Browser commands run through Dext's normal `bash` approval and sandbox policy; this pack grants no implicit approval and adds no hidden tool.
- Do not bypass CAPTCHAs, paywalls, access controls, robots policies, or rate limits.
- Never place passwords or tokens in chat or command text. Use the site's normal interactive login flow and a named restored session when the user authorizes it.
- Browser profiles and saved state can contain credentials. Keep them private and never commit them.
- Ask before downloads, uploads, form submission, purchases, account changes, or destructive actions.

## Troubleshooting

```bash
"$DEXT_PACK_DIR/bin/agent-browser" doctor
"$DEXT_PACK_DIR/bin/agent-browser" install
```

If a command reports a stale ref, take a new snapshot. If an overlay covers a target, interact with the overlay first. If Chrome is unavailable, run setup only with user approval.
