# Dext packs directive

Packs are source-first bundles that teach Dext a workflow without adding provider-visible tools. Keep packs reviewable, user-scoped, and runnable with the existing Dext tool surface.

## Pack rules

- A pack lives under `packs/<name>/`, `.dext/packs/<name>/`, `~/.dext/packs/<name>/`, or a shelf at `.dext/shelves/<shelf>/packs/<name>/` / `~/.dext/shelves/<shelf>/packs/<name>/`.
- Reusable packs should normally live in user-global Dext scope (`~/.dext/packs/<name>/` or `~/.dext/shelves/<shelf>/packs/<name>/`). Use project-local pack roots only when the user explicitly wants repo-scoped behavior.
- Each pack should include `PACK.md` as its canonical agent-facing workflow document.
- Prefer pack-local CLIs/scripts over new LLM-facing tools. Dext should call them through the existing `bash` tool.
- Keep persistent session state in project files that are easy to inspect and recover from; prefer JSONL plus a compact Markdown runbook.
- Keep runtime clutter ignored or untracked. Do not require committing `.dext/`, logs, dashboards, or experiment result streams.
- Pack hook templates are named `phooks.json` to distinguish them from a project's active Dext `hooks.json`. To activate pack hooks, either set `DEXT_HOOKS_FILE=/path/to/pack/phooks.json` or intentionally merge/copy entries into the project's `hooks.json`.
- Hook scripts should be transparent helpers. They may steer, block, or summarize, but the pack must still work without hooks.

## Activation pattern

```bash
export DEXT_PACK_<NAME>_DIR="$HOME/.dext/packs/<name>"  # default reusable install
# or: export DEXT_SHELVES_DIR=/path/to/shelf-root
# where packs live at /path/to/shelf-root/<shelf>/packs/<name>
export DEXT_HOOKS_FILE="$DEXT_PACK_<NAME>_DIR/phooks.json"   # optional
dext --cd /path/to/project "Read $DEXT_PACK_<NAME>_DIR/PACK.md and use that pack."
```

For examples, see [`autoresearch/`](autoresearch/) for code/metric loops and [`packopt/`](packopt/) for SkillOpt-style pack/skill document optimization.
