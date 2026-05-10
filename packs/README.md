# Dext packs directive

Packs are source-first bundles that teach Dext a workflow without adding provider-visible tools. Keep packs reviewable, user-scoped, and runnable with the existing Dext tool surface.

## Pack rules

- A pack lives under `packs/<name>/` in this repository or under a user/project pack directory such as `~/.dext/packs/<name>/` or `.dext/packs/<name>/`.
- Each pack should include `PACK.md` as its canonical agent-facing workflow document.
- Prefer pack-local CLIs/scripts over new LLM-facing tools. Dext should call them through the existing `bash` tool.
- Keep persistent session state in project files that are easy to inspect and recover from; prefer JSONL plus a compact Markdown runbook.
- Keep runtime clutter ignored or untracked. Do not require committing `.dext/`, logs, dashboards, or experiment result streams.
- Pack hook templates are named `phooks.json` to distinguish them from a project's active Dext `hooks.json`. To activate pack hooks, either set `DEXT_HOOKS_FILE=/path/to/pack/phooks.json` or intentionally merge/copy entries into the project's `hooks.json`.
- Hook scripts should be transparent helpers. They may steer, block, or summarize, but the pack must still work without hooks.

## Activation pattern

```bash
export DEXT_PACK_<NAME>_DIR=/path/to/packs/<name>
export DEXT_HOOKS_FILE="$DEXT_PACK_<NAME>_DIR/phooks.json"   # optional
dext --cd /path/to/project "Read $DEXT_PACK_<NAME>_DIR/PACK.md and use that pack."
```

For the first true pack, see [`autoresearch/`](autoresearch/).
