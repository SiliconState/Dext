# Dext PackOpt pack

SkillOpt-style optimization workflow for Dext packs and skill documents.

Start Dext with this pack:

```bash
dext --cd /path/to/project --pack packopt "optimize packs/autoresearch/PACK.md against the validation tasks"
```

The workflow document is [`PACK.md`](PACK.md). The helper is [`bin/packopt.py`](bin/packopt.py).

Core state files in the target project:

- `packopt.md` — session runbook and optimizer memory
- `packopt.sh` — validation script that prints `METRIC name=value`
- `packopt.jsonl` — append-only candidate log
- `packopt.last.json` — last validation details
- `packopt.runs/` — full validation logs
- `packopt.rejected.jsonl` — rejected edit memory
- `packopt.ideas.md` — optional backlog

Use `packopt` when the optimized object is procedural text (`PACK.md`, `SKILL.md`, `DEXT.md`). Use `autoresearch` when the optimized object is code or model/training configuration.
