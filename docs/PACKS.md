# Packs and shelves

Packs and shelves are Dext's extension system. A pack is a source-first directory containing a `PACK.md` workflow document plus optional scripts and hooks. A shelf is a named group of packs with an optional `shelf.json` manifest.

Neither packs nor shelves add provider-visible tools. They teach Dext a workflow using files the model reads and commands Dext runs through the existing `bash` tool. This makes packs portable across providers and models.

## Packs

### Structure

A pack is a directory containing at least `PACK.md`:

```text
my-pack/
├── PACK.md          # required: agent-facing workflow document
├── phooks.json      # optional: hook templates (never overwrites project hooks.json)
├── bin/
│   └── helper.py    # optional: helper scripts
└── tests/
    └── test.py      # optional: pack tests
```

`PACK.md` is the only required file. It contains YAML front matter and a Markdown workflow:

```markdown
---
name: my-pack
description: What this pack does. Shown in pack listings.
---

# My pack

Instructions for Dext to follow when this pack is active.
Include setup steps, loop rules, helper commands, and state files.
```

Front matter fields:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | no | Pack identifier. Defaults to directory name. |
| `description` | no | One-line description for listings. |

### Discovery

Dext searches for packs in this precedence order. First match wins:

1. `DEXT_PACK_<NAME>_DIR` environment variable
2. Project `.dext/shelves/<shelf>/packs/<name>/`
3. Project `.dext/packs/<name>/`
4. Project `packs/<name>/`
5. `DEXT_SHELVES_DIR` entries (`<shelf>/packs/<name>/`)
6. `DEXT_PACKS_DIR` entries
7. User `~/.dext/shelves/<shelf>/packs/<name>/`
8. User `~/.dext/packs/<name>/`
9. Bundled packs in the Dext repository

For reusable packs, default to user-global Dext scope: `~/.dext/packs/<name>/` for legacy packs or `~/.dext/shelves/<shelf>/packs/<name>/` for shelf packs. Use the project-local entries above only when the user explicitly asks for a repo-specific pack.

### Running a pack

CLI:

```bash
dext pack list
dext pack inspect my-pack
dext pack run my-pack "task description"
dext --pack my-pack "task description"
```

Inside an interactive session:

```text
/pack list
/pack inspect my-pack
/pack run my-pack task description here
```

Conversational invocation works when the message clearly references a known pack name:

```text
run autoresearch on improving this benchmark
```

When a pack runs, Dext reads `PACK.md` as the initial agent context and sets `DEXT_PACK_DIR` and `DEXT_PACK_<NAME>_DIR` environment variables. If the pack has `phooks.json`, those hooks activate for the session.

### Building a pack

1. Default to a user-global install location for reusable packs:
   - legacy pack: `~/.dext/packs/<name>/`
   - shelf pack: `~/.dext/shelves/<shelf>/packs/<name>/`
   Use project-local `packs/<name>/`, `.dext/packs/<name>/`, or `.dext/shelves/<shelf>/packs/<name>/` only when the user explicitly wants repo-scoped behavior.
2. Create the directory and `PACK.md` with front matter.
3. Write clear workflow instructions: setup, loop rules, helper commands, state files.
4. Add helper scripts in `bin/` — called through `bash` via `$DEXT_PACK_DIR/bin/helper.py`.
5. Optionally add `phooks.json` for steering/validation hooks.
6. Test locally:

```bash
dext pack inspect my-pack   # verify discovery and front matter
dext pack run my-pack "test task"  # run on a disposable task
```

7. Distribute by placing in a shelf, a shared directory, or bundling in a repo.

### Hook templates

Packs use `phooks.json` (never `hooks.json`) to avoid colliding with a project's active hooks. Pack hooks can steer, validate, or summarize tool calls, but the pack must still work without hooks enabled.

To activate pack hooks:

```bash
export DEXT_HOOKS_FILE="$DEXT_PACK_MY_PACK_DIR/phooks.json"
```

Or Dext activates them automatically when running a pack that has `phooks.json`.

## Shelves

### Structure

A shelf is a directory grouping one or more packs with an optional typed manifest:

```text
my-shelf/
├── shelf.json       # optional: typed manifest with abilities
└── packs/
    ├── research/
    │   ├── PACK.md
    │   └── bin/
    └── deploy/
        └── PACK.md
```

Without `shelf.json`, a shelf is just a pack group discovered by directory structure. With `shelf.json`, it declares typed abilities that Dext can expose to the model as provider-neutral metadata.

### Manifest schema

```json
{
  "id": "community",
  "name": "Community packs",
  "description": "Shared workflow packs",
  "packs": [
    {
      "id": "research",
      "name": "Research",
      "version": "1.0.0",
      "description": "Deep research workflow",
      "abilities": [
        {
          "ability": "command",
          "name": "research",
          "usage": "/research <topic>",
          "description": "Start a research cycle"
        },
        {
          "ability": "tool",
          "name": "research-helper",
          "description": "Pack-local helper metadata",
          "schema": {"type": "object"},
          "grants": ["read", "process"],
          "exposure": "on_demand"
        },
        {
          "ability": "context",
          "name": "research-state",
          "description": "Current research findings",
          "budget": 2000
        }
      ]
    }
  ]
}
```

Ability types:

| Type | Purpose |
|------|---------|
| `tool` | Declares tool-like metadata with `schema`, `grants`, and `exposure`; it is registry metadata, not a provider-visible tool implementation. |
| `command` | Declares a slash command |
| `hook` | Declares hook signals the pack listens to |
| `context` | Declares named context the pack provides |

### Shelf discovery

Shelves are discovered from the same paths as packs. A shelf root is any directory containing `<shelf-name>/packs/` subdirectories or a `shelf.json` manifest.

```bash
# Point to a directory containing multiple shelves
export DEXT_SHELVES_DIR=/path/to/shelves

# Or use project/user defaults
# .dext/shelves/<name>/packs/
# ~/.dext/shelves/<name>/packs/
```

### Listing shelves

```bash
dext shelves    # CLI
/shelves        # interactive
```

Shelf metadata is injected into the model context as typed ability records, not as new provider tools. Tool abilities require `schema`, `grants`, and `exposure` (`hidden`, `on_demand`, or `visible`) so the registry can describe capability shape without executing it directly.

## Reference example

The bundled `autoresearch` pack implements an autonomous experiment loop, and `packopt` applies a SkillOpt-style loop to pack/skill documents. They ship in the repository as bundled examples, but reusable installations should normally go into user-global Dext scope (`~/.dext/...`) so they are callable from any project:

- `packs/autoresearch/PACK.md` — autoresearch workflow document
- `packs/autoresearch/bin/autoresearch.py` — autoresearch helper script
- `packs/autoresearch/phooks.json` — autoresearch steering hooks
- `packs/packopt/PACK.md` — bounded-edit pack/skill optimization workflow
- `packs/packopt/bin/packopt.py` — validation/log/rejected-memory helper

Run it:

```bash
dext pack run autoresearch "optimize the benchmark in this repo"
dext pack run packopt "improve ~/.dext/packs/autoresearch/PACK.md against held-out tasks"
```

Inspect it to see a full pack structure:

```bash
dext pack inspect autoresearch
dext pack inspect packopt
```
