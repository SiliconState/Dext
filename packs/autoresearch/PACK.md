---
name: autoresearch
description: Autonomous experiment loop pack for Dext. Use when asked to run autoresearch, optimize a metric overnight, repeatedly benchmark changes, or adapt Karpathy-style autoresearch to a project.
---

# Autoresearch pack

Run an autonomous research loop: understand the system, make one experimental change, measure it, keep wins, revert losses, record what was learned, repeat until interrupted.

This pack is inspired by:

- `karpathy/autoresearch`: small real LLM training setup, fixed 5-minute budget, one editable file, `val_bpb` lower-is-better, simple baseline loop.
- A prior generic autoresearch extension: structured `METRIC name=value` output, JSONL state, session docs, checks, ASI, hooks, compaction-resilient summaries, and finalization into reviewable branches.

Dext adaptation: no new provider-visible tools. Use the pack helper through the normal `bash` tool and keep durable state in project files.

## Files this pack uses

| File | Purpose |
| --- | --- |
| `autoresearch.md` | Session runbook: objective, metric, scope, constraints, what has been tried. A fresh Dext session can resume from this. |
| `autoresearch.sh` | Benchmark command. Must print `METRIC name=number` lines. |
| `autoresearch.jsonl` | Append-only experiment log: config headers, runs, metrics, status, ASI. |
| `autoresearch.last.json` | Last `run` command details, parsed metrics, output path. |
| `autoresearch.runs/` | Full benchmark logs, one file per run. |
| `autoresearch.checks.sh` | Optional correctness backpressure; runs after a passing benchmark. |
| `autoresearch.ideas.md` | Optional backlog for promising deferred ideas. |
| `autoresearch.config.json` | Optional config, currently `maxIterations`. |

All `autoresearch.*` files are preserved across pack reverts. Keep them untracked unless the user explicitly wants session artifacts committed.

## Helper commands

Set the pack path once. For reusable installs, prefer the user-global location under `~/.dext/packs/`; if you are editing the bundled repo copy directly, point `PACK` at that checkout instead:

```bash
PACK="${DEXT_PACK_AUTORESEARCH_DIR:-$HOME/.dext/packs/autoresearch}"
PYTHON=${PYTHON:-python3}
```

Initialize a session:

```bash
$PYTHON "$PACK/bin/autoresearch.py" --cwd . init \
  --name "<goal>" \
  --metric-name "<primary_metric>" \
  --metric-unit "<unit-or-empty>" \
  --direction lower
```

Run a benchmark:

```bash
$PYTHON "$PACK/bin/autoresearch.py" --cwd . run --timeout-seconds 900
```

Log a result:

```bash
$PYTHON "$PACK/bin/autoresearch.py" --cwd . log \
  --metric 1.234 \
  --status keep \
  --description "short experiment description" \
  --metrics '{"wall_seconds": 302.1}' \
  --asi '{"hypothesis":"what you tried","next_action_hint":"what to try next"}'
```

Summarize/resume:

```bash
$PYTHON "$PACK/bin/autoresearch.py" --cwd . status
```

Scaffold starter files:

```bash
$PYTHON "$PACK/bin/autoresearch.py" --cwd . scaffold \
  --goal "<goal>" \
  --command '<benchmark command that prints METRIC lines>' \
  --metric-name '<primary_metric>' \
  --metric-unit '<unit>' \
  --direction lower
```

## Setup workflow for Dext

1. **Use the real project root as cwd.** Run helper commands with `--cwd <project-root>` and keep `autoresearch.*` artifacts in that root. If this pack is invoked from inside another pack/skill optimization, do not mix artifact families: `autoresearch.*` belongs to the measured project, while `packopt.*` or other optimizer artifacts belong to their own session.
2. **Use the right web path.** Prefer Dext's native `http` tool for static documentation and text extraction. Rendered interaction belongs to the bundled `agent-browser` pack in a separate pack invocation; do not assume a native `browser` tool or runtime recipe exists. If the current run lacks an active browser helper, ask before falling back to manual browser commands.
3. **Infer or ask once** for goal, benchmark command, primary metric/direction, files in scope, and constraints. If the user already supplied enough context, proceed.
4. **Inspect the source deeply before changes.** Read the benchmark, hot files, configs, and existing tests. Do not start random edits.
5. **Create a branch** named `autoresearch/<slug>-<date>` unless the user asks to reuse the current branch. If the project is not a git repository, do not block: record that limitation in `autoresearch.md`, avoid destructive edits, and use explicit file backups or patches for rollback.
6. **Write `autoresearch.md`.** Make it good enough for a fresh Dext session to resume without prior chat.
7. **Write `autoresearch.sh`.** Use `set -euo pipefail`, keep output compact, and print structured metric lines. For fast/noisy benchmarks, run multiple samples and emit a median. For slow ML training, one run is enough.
8. **Optionally write `autoresearch.checks.sh`** when correctness must be guarded. Checks time does not count toward the metric.
9. **Initialize, run baseline, log baseline as `keep`.** Baseline is the first run in a segment.
10. **Enter the loop immediately.** Never ask whether to continue after setup unless blocked.

## Loop rules

LOOP FOREVER until interrupted or `maxIterations` is reached:

1. Read `python3 <pack>/bin/autoresearch.py status`, `autoresearch.md`, recent `autoresearch.jsonl`, and relevant source as needed.
2. Pick one hypothesis. Prefer high-signal changes grounded in measurements/source understanding.
3. Edit only in-scope project files. Do not mutate `autoresearch.*` as the optimization itself.
4. Run `python3 <pack>/bin/autoresearch.py run`.
5. Read `autoresearch.last.json` for parsed metrics and output path.
6. Decide:
   - primary metric improved: `keep`
   - primary metric worse/equal: `discard`
   - benchmark crashed/timed out: `crash`
   - benchmark passed but checks failed: `checks_failed`
7. Log with `python3 <pack>/bin/autoresearch.py log ...`. Include ASI every time.
8. Update `autoresearch.md` periodically with distilled findings, not every noisy detail.
9. Put deferred ideas in `autoresearch.ideas.md` as checkbox bullets.
10. Continue without asking the user for permission to keep going.

## ASI discipline

`--asi` is the memory that survives reverts and compaction. Always include at least:

```json
{"hypothesis":"..."}
```

For discards/crashes/check failures add:

```json
{
  "hypothesis": "what you tried",
  "rollback_reason": "why it was rejected",
  "next_action_hint": "adjacent idea or avoid pattern"
}
```

Useful optional fields: `learned`, `bottleneck`, `error`, `files_touched`, `risk`, `next_focus`.

## Benchmark script contract

`autoresearch.sh` should:

- Use `#!/usr/bin/env bash` and `set -euo pipefail`.
- Run cheap prechecks before slow work when possible.
- Avoid flooding stdout; redirect verbose logs to files.
- Print one or more lines exactly like `METRIC name=value`.
- Use a primary metric name matching `init --metric-name`.
- Emit secondary metrics that help tradeoff decisions, for example wall seconds, memory, tokens, binary size, or failure counts.

Example for Karpathy autoresearch:

```bash
#!/usr/bin/env bash
set -euo pipefail
uv run train.py > run.log 2>&1
cat run.log | tail -80
val=$(grep '^val_bpb:' run.log | awk '{print $2}')
vram=$(grep '^peak_vram_mb:' run.log | awk '{print $2}')
seconds=$(grep '^training_seconds:' run.log | awk '{print $2}')
printf 'METRIC val_bpb=%s\n' "$val"
printf 'METRIC peak_vram_mb=%s\n' "$vram"
printf 'METRIC training_seconds=%s\n' "$seconds"
```

## Karpathy-specific rules

When running against `karpathy/autoresearch` or a close fork:

- Read `README.md`, `program.md`, `prepare.py`, and `train.py` before setup.
- Treat `prepare.py` and the evaluator as off limits unless the user explicitly changes the rules.
- Edit `train.py` only for experiments.
- First run is baseline.
- Use `val_bpb` as primary metric, lower is better.
- Fixed training budget makes changes comparable on the same hardware.
- If smaller hardware struggles, consider lower-entropy data, smaller vocab, lower sequence length/eval tokens in forks, lower `DEPTH`, `WINDOW_PATTERN="L"`, and smaller power-of-two `TOTAL_BATCH_SIZE` — but do not change off-limit files in the canonical setup.

## Optional pack hooks

Pack hook templates are named `phooks.json`, not `hooks.json`, to avoid confusing them with an active project hook file. Activate with:

```bash
export DEXT_PACK_AUTORESEARCH_DIR="$HOME/.dext/packs/autoresearch"
export DEXT_HOOKS_FILE="$DEXT_PACK_AUTORESEARCH_DIR/phooks.json"
dext --cd /path/to/project "Read $DEXT_PACK_AUTORESEARCH_DIR/PACK.md and run autoresearch."
```

The pack works without hooks. Hooks are only light steering/status helpers.

## What this pack intentionally improves

- Compared with Karpathy's barebones loop: structured JSONL state, resumable Markdown runbook, parsed metrics, optional checks, full run logs, ASI, and deterministic status summaries.
- Compared with external extension approaches: no Dext core patch, no new provider-visible tools, no global package, and simple stdlib Python that can be copied into any project.
- Compared with generic coding loops: explicit keep/discard semantics, preserved experiment memory, and a bias toward measurable improvement rather than speculative refactors.
