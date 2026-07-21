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

A clear conversational request can also invoke a known pack, for example: `run refactor-helper on the parser`.

When selected, Dext reads `PACK.md` as bounded invocation context and keeps the pack active for the session. It exports `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` to subsequent `bash` calls and pack hook processes. Changing the sandbox root clears active pack state.

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

Optional helpers live under the pack's `bin/` directory and run through normal Dext execution policy. Prefer small transparent helpers over new provider-visible tools.

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
