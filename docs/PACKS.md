# Packs and shelves

Packs are Dext's modular “battery packs”: source-first workflows that add specialized behavior without expanding the provider-visible toolset or bloating the core binary. Dext provides the lifecycle—create, discover, inspect, maintain, and run—while users own the pack content.

Dext ships no packs. A Dext checkout and release binary contain only the pack and shelf infrastructure.

## Storage contract

Every pack lives inside a shelf:

```text
<shelf-root>/
└── <shelf>/
    ├── shelf.json              # optional typed metadata
    └── packs/
        └── <pack>/
            ├── PACK.md         # required workflow
            ├── phooks.json     # optional hook templates
            ├── bin/            # optional helpers
            ├── references/     # optional supporting context
            └── tests/          # optional pack tests
```

Dext discovers shelf roots in this precedence order:

1. Project `.dext/shelves/`
2. `DEXT_SHELVES_DIR` entries
3. User `~/.dext/shelves/` or `$DEXT_HOME/shelves/`

A `DEXT_SHELVES_DIR` entry may be one shelf directory containing `packs/`, or a root containing multiple shelf directories. The first pack name found wins.

Direct pack roots are intentionally unsupported: project `packs/`, project `.dext/packs/`, user `~/.dext/packs/`, `DEXT_PACKS_DIR`, and `DEXT_PACK_<NAME>_DIR` are not discovery inputs. `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` are still exported for an already selected pack so its helpers can locate their own files.

## Create

Create a reusable user pack:

```bash
dext pack create engineering/refactor-helper
```

This creates:

```text
~/.dext/shelves/engineering/packs/refactor-helper/PACK.md
```

Create an explicitly project-local pack:

```bash
dext pack create local/release-check --project
```

This creates:

```text
.dext/shelves/local/packs/release-check/PACK.md
```

The command validates lowercase shelf and pack identifiers, creates a small editable workflow, and refuses to overwrite an existing path. The interactive equivalent is:

```text
/pack create engineering/refactor-helper
/pack create local/release-check --project
```

For a dedicated shelf repository whose root contains multiple shelves, run Dext with that repository as the sandbox and ask it to create or maintain `<shelf>/packs/<name>`. Then validate against that root:

```bash
DEXT_SHELVES_DIR="$PWD" dext pack inspect <name>
DEXT_SHELVES_DIR="$PWD" dext pack run <name> "test task"
```

Keep external shelf repositories separate from Dext. Review and audit them independently before enabling them through `DEXT_SHELVES_DIR`.

## PACK.md

`PACK.md` contains YAML front matter followed by the agent-facing workflow:

```markdown
---
name: refactor-helper
description: Guide a bounded refactor with tests and verification.
credential-env: [SERVICE_TOKEN]
---

# Refactor Helper

## Use when

- The user requests a behavior-preserving refactor.

## Workflow

1. Inspect the affected code and tests.
2. Make one bounded change.
3. Run focused verification.
4. Report changes and gaps.
```

Front matter:

| Field | Required | Purpose |
|---|---|---|
| `name` | No | Pack identifier; defaults to the directory name. |
| `description` | No | Short text shown in listings and prompt summaries. |
| `credential-env` | No | Exact credential-shaped environment names required by the pack's own direct native helper. |

Credential declarations are honored only for packs from user or `DEXT_SHELVES_DIR` shelves. Project-local declarations are ignored so repository content cannot opt into inherited credentials. Provider credential names are always excluded.

## Inspect and run

```bash
dext pack list
dext pack inspect refactor-helper
dext pack run refactor-helper "refactor the parser"
dext --pack refactor-helper "refactor the parser"
```

Interactive equivalents:

```text
/pack list
/pack inspect refactor-helper
/pack run refactor-helper refactor the parser
```

A clear conversational request can also invoke a known pack, for example: `run refactor-helper on the parser`. Under approval `always`, conversational project-pack activation and project `shelf.json` metadata proceed automatically. Prompting profiles request one repository-scoped first-use confirmation. Explicit `/pack` or `dext pack` invocation confirms only the selected project workflow; unrelated project shelf metadata remains unapproved. Choosing prompt-level `Always` stores a bounded owner-private, single-link project-scoped approval marker on Unix; unsafe/permissive markers are ignored, and `/project-extensions reset` refuses unsafe marker shapes while removing a safe marker or clearing a session denial. Until approval under a prompting profile, project metadata cannot shadow a same-named user or run-shelf pack.

When selected, Dext reads `PACK.md` only when it is a regular non-symlink file no larger than 1 MiB, then caps invocation context to 32 KiB and keeps the pack active for the session. `shelf.json` manifests use the same 1 MiB regular-file/no-follow load boundary. Pack hooks and environment are activated only after workflow and optional runtime activation succeed. Dext exports `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` to subsequent `bash` calls and pack hook processes. Changing the sandbox root clears active pack state.

## Optional executable runtime protocol

A reviewed pack may add `runtime.json` to expose a small dynamic tool set and lifecycle effects without adding pack-specific code to Dext. This is optional; `PACK.md`-only packs continue to use the native Dext tools.

```json
{
  "version": 1,
  "command": "bin/my-pack-runtime",
  "args": [],
  "max_continuations": 100,
  "tools": [
    {
      "name": "measure_target",
      "description": "Measure the configured optimization target.",
      "risk": "write",
      "input_schema": {
        "type": "object",
        "properties": {"note": {"type": "string"}},
        "additionalProperties": false
      }
    }
  ]
}
```

The v1 boundary is fail-closed:

- `runtime.json` is a regular non-symlink file capped at 256 KiB. Unknown manifest fields, unsupported versions, full-native-catalog/active-dynamic/host-approval-operation name collisions, invalid provider tool names, oversized descriptions/schemas/arguments, and malformed or unsupported nested schema keywords are rejected. Protocol v1 schemas require an explicit supported type at every node and implement only `type`, `properties`, `required`, boolean `additionalProperties`, `items`, `enum`, and bounded `description`. Dynamic tool schemas use the session's selected lean/full schema profile on every provider wire; lean removes schema annotation descriptions but retains the runtime tool's top-level action description and argument structure.
- `command` is a relative path to a regular executable inside the canonical pack root. Symlinked, non-executable, absolute, escaping, or larger-than-256-MiB commands are rejected. Approval identifies the executable SHA-256; Dext rehashes the regular no-follow file before every invocation and requires reactivation if bytes changed.
- Runtime activation validates executable identity separately from selecting `PACK.md`. Approval `always` proceeds automatically, approval `never` disables it, and prompting-profile `Always` decisions remain exact-identity scoped. Changing approval or sandbox policy revokes the active executable runtime, removes its dynamic grants/denials, and discards queued callbacks; invoking the pack again requires activation under the new policy. Activation, idle, and tool events all use the selected sandbox profile.
- Every declared tool has `read`, `write`, or `danger` risk (`write` by default). Read tools need no mutation checkpoint. Write/danger tools use durable side-effect fencing and a fail-closed Git checkpoint before execution when a repository is present.
- Runtime subprocesses inherit no credential-shaped environment values, including pack helper credential declarations and `DEXT_INHERIT_TOOL_CREDENTIALS`. They are one-shot process-group-contained calls, not daemons; timeout defaults to 120 seconds and bounds stdin delivery plus root execution, while output drain after process-tree cleanup has a separate one-second cap. A manifest may set `timeout_seconds` from 1–604800 for long-running helpers, and `DEXT_PACK_RUNTIME_TIMEOUT_SECS` overrides it within the same bound. A present malformed, non-Unicode, zero, or out-of-range override fails closed instead of falling back.

Dext sends one JSON object on stdin for each `activate`, `tool`, or `idle` event:

```json
{
  "version": 1,
  "event": "tool",
  "pack": "my-pack",
  "session_id": "...",
  "cwd": "/active/project",
  "state": {},
  "context": {
    "turn_id": "turn-...",
    "iteration": 2,
    "history_messages": 8,
    "compacted": false
  },
  "tool": "measure_target",
  "input": {"note": "baseline"}
}
```

The helper writes exactly one JSON response object to stdout:

```json
{
  "version": 1,
  "content": "measurement complete",
  "is_error": false,
  "state": {"runs": 1},
  "effects": [
    {"type": "steer", "text": "Compare against the baseline."},
    {"type": "continue", "prompt": "Run the next bounded experiment.", "delay_ms": 100},
    {"type": "view", "title": "Experiments", "markdown": "# Results"}
  ]
}
```

Requests/responses are capped at 256 KiB, content and steering/continuation text at 128 KiB, state at 64 KiB, markdown views at 128 KiB, effects at 16 per call, and continuation delays at 30 seconds. The subprocess capture path preserves the full response ceiling instead of applying ordinary external-tool output caps. Runtime-exposed content, effect text, view titles/Markdown, and queued prompts reject unsafe terminal controls. Compact pack/shelf metadata is bounded and normalized to one safe line before prompt injection; shelf Context bodies preserve ordinary newlines/tabs but normalize terminal controls and Unicode line separators. Response state/effects and continuation accounting are validated and committed atomically. State, used continuation count, and up to 32 queued continuation prompts totaling 64 KiB persist in the owner-private session header; any runtime-bearing session uses format v5 so pre-runtime Dext binaries reject it rather than silently discard executable-runtime state. An interrupt while waiting for a delayed prompt cancels and refunds it. Resume preserves the current-run approval and sandbox policies and discards saved grants. Before changing the live agent, it preflights project-extension trust, the exact saved source plus canonical pack-directory fingerprint, manifest/hash/state accounting, and approval against the current executable digest; a shadowing same-name pack, changed/missing runtime, malformed snapshot, or denied approval cannot partially apply the saved sandbox/model/session state. Runtime content, steering, views, and surfaced activation/idle errors pass through privacy redaction before model, log, or TUI exposure. Opaque structured state is bounded and owner-private but is not rewritten by privacy redaction, so helpers must not place secrets in state. A pack can request at most its declared `max_continuations` (hard cap 1,000) across the saved runtime state. While active, dynamic tools participate in `/allow`, `/revoke`, and `/allowed`; policy changes revoke those grants with the runtime.


## Maintain

Packs are ordinary files, so Dext maintains them with its normal read/edit/test tools and existing approval and sandbox policy. Outside the active project sandbox, the native mutation exception is limited to content inside a concrete user pack directory (`~/.dext/shelves/<shelf>/packs/<pack>/...`) containing a regular `PACK.md`; shelf manifests and loose files directly under `packs/` are not writable through that exception. Dext revalidates the destination and marker before atomic replacement. A practical loop is:

1. Inspect the shelf manifest, `PACK.md`, helpers, and tests.
2. State the behavior the pack should add.
3. Make the smallest workflow or helper change.
4. Run pack-local tests.
5. Run `dext pack inspect <name>`.
6. Exercise `dext pack run <name> ...` on a disposable task.
7. Review the shelf repository diff before publishing it.

Dext does not auto-update packs or fetch shelf repositories. Versioning, review, distribution, and security auditing belong to the shelf owner.

## Helpers and hooks

Optional ordinary helpers live under the pack's `bin/` directory and run through normal Dext execution policy. A reviewed native helper declared by `runtime.json` instead participates in the bounded one-shot runtime protocol above and may expose dynamic provider tools only while that pack runtime is active.

`phooks.json` contains pack hook templates and is distinct from a project's `hooks.json`. Dext adds these hooks for the active session; the pack should remain understandable and useful without them.

Credential handling is deliberately narrow:

- Declared values can reach only a simple direct invocation of the active pack's own native `bin/` helper.
- Hooks, arbitrary Bash, pipelines, redirections, prompts, logs, and sessions do not receive those values.
- On Windows, only native `.exe` and `.com` helpers qualify; scripts run through Bash with declared credentials removed.
- `DEXT_INHERIT_TOOL_CREDENTIALS=1` is a separate high-trust opt-in for trusted model-invoked tools and is not required for normal packs.

## Optional shelf.json

A shelf may include `shelf.json` to describe packs and typed abilities:

```json
{
  "id": "engineering",
  "name": "Engineering",
  "description": "Engineering workflow packs",
  "packs": [
    {
      "id": "refactor-helper",
      "name": "Refactor Helper",
      "version": "0.1.0",
      "description": "Behavior-preserving refactor workflow",
      "abilities": [
        {
          "ability": "command",
          "name": "refactor-helper",
          "usage": "refactor-helper <target>",
          "description": "Run the refactor workflow"
        }
      ]
    }
  ]
}
```

List typed shelf metadata with:

```bash
dext shelves
```

or:

```text
/shelves
```

Manifest abilities are provider-neutral metadata. They do not register executable provider tools or arbitrary slash commands. Packs still execute through Dext's normal tool surface.
