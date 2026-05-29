# Dext autoresearch pack

Autonomous experiment loop infrastructure for Dext without adding provider-visible tools.

Start Dext with this pack after installing it globally (recommended) or pointing `DEXT_PACK_AUTORESEARCH_DIR` at a local checkout:

```bash
export DEXT_PACK_AUTORESEARCH_DIR="$HOME/.dext/packs/autoresearch"
# optional hooks:
export DEXT_HOOKS_FILE="$DEXT_PACK_AUTORESEARCH_DIR/phooks.json"
dext --cd /path/to/project "Read $DEXT_PACK_AUTORESEARCH_DIR/PACK.md and run autoresearch for <goal>."
```

The workflow document is [`PACK.md`](PACK.md). The helper is [`bin/autoresearch.py`](bin/autoresearch.py).

Core state files in the target project:

- `autoresearch.md` — session runbook
- `autoresearch.sh` — benchmark script that prints `METRIC name=value`
- `autoresearch.jsonl` — append-only run log
- `autoresearch.last.json` — last run details
- `autoresearch.runs/` — full run logs
- `autoresearch.checks.sh` — optional correctness checks
- `autoresearch.ideas.md` — optional backlog

Hook templates use `phooks.json` so they do not collide with an active project `hooks.json`.
