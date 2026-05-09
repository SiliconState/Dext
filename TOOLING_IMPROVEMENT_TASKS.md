# Tooling and workflow improvement tasks

Source: review of `dext-session-1778099507.jsonl` plus user correction that
pi-memory is an external shared memory tool, not part of Dext.

## Goals

- Reduce avoidable tool failures.
- Reduce assumptions about shell/tool availability.
- Make symbol/source searches more reliable.
- Keep cleanup/final-verification loops efficient without hiding real failures.
- Clarify pi-memory as an external tool usable by Dext or any agent.

## Tasks

### 1. Add a shell/tool availability guide to the system prompt

Problem: the agent used `rg` inside `bash` because `rg` exists as a Dext API tool,
but `rg` was not available in the shell PATH.

Action:
- Add prompt guidance: API tools are not shell binaries.
- Prefer native Dext tools (`rg`, `fd`, `jq`, `http`) directly instead of assuming
  matching commands exist in bash.
- If a shell binary is required, probe once with `command -v <name>` before use.
- For portable shell text search, use `grep`/`awk` when appropriate.

Acceptance:
- Prompt text contains explicit API-tool-vs-shell-binary warning.
- Add/adjust a prompt snapshot/unit test if the project has prompt tests.

### 2. Add command-pattern linting or warnings for common tool misuse

Problem examples from session:
- `cargo test --release test_a test_b` failed because Cargo accepts one test
  filter before `--`.
- `python` failed because only `python3` exists in this environment.
- `git show ... | rg` failed because shell `rg` was unavailable.

Action:
- Add preflight warnings in the bash tool path for high-confidence mistakes:
  - `cargo test ... <filter1> <filter2>` before `--`.
  - bare `python` when `python3` is available and `python` is missing.
  - known API tool names used as shell commands after pipes when unavailable
    (`rg`, `fd`, `jq`, etc.), if cheap to detect.
- Warning should be model-visible and suggest a corrected command.
- Do not block unless there is already an existing policy reason to block.

Acceptance:
- Tests cover each pattern and suggested correction.
- Legitimate commands still execute.

### 3. Improve symbol-search guidance and failure recovery

Problem: `read_symbol` was called with guessed symbols (`struct Usage`,
`usage_spans`) and failed.

Action:
- Update prompt guidance to require `rg` first unless the exact symbol name is
  known from prior evidence.
- Add examples:
  - Use `rg -n "struct Usage|impl Usage|fn usage" src`.
  - Then call `read_symbol` with exact symbol, or `read_file` with a narrow
    offset around the hit.
- On `read_symbol` not found, tool result should suggest `rg` with the queried
  token stripped of Rust keywords like `struct`, `fn`, `impl`.

Acceptance:
- Prompt guidance updated.
- `read_symbol` not-found result includes a concise next-step hint.
- Test covers `struct Usage` -> suggest searching `Usage`.

### 4. Soften similarity guard for safe validation after edits

Problem: the bash similarity guard blocked repeated cleanup checks like
`git diff --check`, `cargo fmt --check`, and installed-binary probes after edits.

Action:
- Track whether files changed since the last similar command. Allow a repeated
  safe validation command after an edit/write/multi_edit/tool-generated mutation.
- Add a small allowlist for idempotent verification commands:
  - `git status --short`
  - `git diff --check`
  - `cargo fmt --check`
  - `cargo test ...` exact reruns after edits may warn but should not be blocked
    by near-duplicate detection.
- Keep guard strict for repeated failing endpoint/auth/probe loops.

Acceptance:
- Test: repeated `git diff --check` after an edit is allowed.
- Test: repeated identical failing curl/auth probe remains blocked/warned.
- Session ledger records when a repeated command was allowed because files changed.

### 5. Normalize memory sync whitespace for external pi-memory output

Problem: `pi-memory sync MEMORY.md` produced trailing Markdown spaces caught by
`git diff --check`.

Action:
- Add a Dext-side helper/wrapper pattern in docs or code for memory sync:
  run `pi-memory sync`, then strip trailing whitespace in tracked Markdown.
- Prefer fixing pi-memory itself if editing that external project; otherwise keep
  Dext guidance explicit and non-invasive.
- Clarify that pi-memory is external and Dext should use official CLI/API flows,
  not mutate its DB.

Acceptance:
- DEXT guidance says pi-memory is external.
- A documented command or helper strips trailing whitespace after sync.
- `git diff --check` passes after memory sync.

### 6. Add a final-review workflow for large dirty trees

Problem: the session committed a large multi-concern diff after many broad reads.
The result was correct, but inefficient and harder to review.

Action:
- Add prompt guidance for dirty trees over a threshold, e.g. >5 files or >1000
  changed lines:
  - run `git diff --stat`;
  - group files by concern;
  - prefer separate commits when concerns are separable;
  - review targeted diffs per group instead of broad source windows.
- Avoid broad source reads after full verification unless a specific risk is found.

Acceptance:
- Prompt guidance updated.
- Optional test/snapshot for injected Git guidance.

### 7. Improve installed-binary verification ergonomics

Problem: verifying installed strings with repeated `strings | grep` probes ran
into similarity guard friction.

Action:
- Add a canonical verification command or Dext CLI subcommand for installed build
  provenance, e.g. `dext --version --verbose` showing source path/git hash/build
  time/features.
- Prefer semantic installed checks over `strings` probes.

Acceptance:
- `dext --version --verbose` or equivalent displays enough provenance to confirm
  install freshness.
- Tests cover version/provenance output if feasible.

## Suggested implementation order

1. Prompt/guidance updates: tasks 1, 3, 5, 6.
2. Similarity guard safe-validation fix: task 4.
3. Bash misuse warnings: task 2.
4. Installed provenance command: task 7.

## Verification plan

For Dext source changes:

```bash
cargo build --release
cargo test --release
cargo test --release --test tui_smoke -- --nocapture
cargo install --path . --force
```

Also run targeted tests for any new policy/prompt/tooling behavior.
