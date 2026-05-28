---
name: packopt
description: SkillOpt-style workflow for improving a Dext pack or skill document with scored rollouts, bounded edits, held-out validation, and rejected-edit memory.
---

# PackOpt pack

Optimize a Dext pack workflow document (`PACK.md`) or another agent skill file using a SkillOpt-style loop: collect scored rollouts, propose a small patch, validate it on held-out tasks, keep only strict wins, and preserve rejected edits as negative feedback.

This pack is for improving reusable procedural text. It does not add provider-visible tools; use the helper through normal `bash` calls and keep session state in project files.

## Files this pack uses

| File | Purpose |
| --- | --- |
| `packopt.md` | Session runbook: objective, target skill, metric, task splits, constraints, and learned optimizer memory. |
| `packopt.sh` | Validation command. Must print `METRIC name=number` lines. |
| `packopt.jsonl` | Append-only log of config, candidates, validation metrics, rejected edits, and ASI. |
| `packopt.last.json` | Last validation command details, parsed metrics, output path. |
| `packopt.runs/` | Full validation logs, one file per candidate run. |
| `packopt.rejected.jsonl` | Rejected candidates with score, summary, and reusable avoid-patterns. |
| `packopt.ideas.md` | Optional backlog for deferred edit ideas. |
| `packopt.config.json` | Optional config, currently `maxIterations`. |

Keep `packopt.*` files untracked unless the user explicitly wants session artifacts committed.

## Helper commands

Set the pack path once:

```bash
PACK="$PWD/packs/packopt"  # from this repository; adjust if installed elsewhere
PYTHON=${PYTHON:-python3}
```

Initialize a session:

```bash
$PYTHON "$PACK/bin/packopt.py" --cwd . init \
  --name "<goal>" \
  --target-file packs/<name>/PACK.md \
  --metric-name "<primary_metric>" \
  --direction higher
```

Run validation:

```bash
$PYTHON "$PACK/bin/packopt.py" --cwd . run --timeout-seconds 900
```

Log a candidate:

```bash
$PYTHON "$PACK/bin/packopt.py" --cwd . log \
  --metric 0.75 \
  --status keep \
  --description "short patch description" \
  --patch-summary "what changed in the skill" \
  --metrics '{"wall_seconds":302.1}' \
  --asi '{"hypothesis":"what this patch should improve"}'
```

Summarize/resume:

```bash
$PYTHON "$PACK/bin/packopt.py" --cwd . status
```

Scaffold starter files:

```bash
$PYTHON "$PACK/bin/packopt.py" --cwd . scaffold \
  --goal "<goal>" \
  --target-file packs/<name>/PACK.md \
  --command '<validation command that prints METRIC lines>' \
  --metric-name '<primary_metric>' \
  --direction higher
```

## Setup workflow for Dext

1. Infer or ask once for the target skill file, objective, validation command, primary metric/direction, and any edit constraints. If the user already supplied enough context, proceed.
2. Inspect the target skill/pack and any available task suite before editing. Understand how the pack is invoked and scored.
3. Split tasks into train/validation/test when possible. Use train tasks to discover failure patterns, validation tasks for keep/reject decisions, and test tasks only for final reporting.
4. Write `packopt.md` with enough context for a fresh Dext session to resume.
5. Write `packopt.sh`. It should run the target pack/skill on the validation split or a cheap proxy and print `METRIC name=value` lines.
6. Initialize, run the baseline, log baseline as `keep`.
7. Enter the optimization loop immediately. Do not ask whether to continue after setup unless blocked.

## Optimization loop

LOOP until interrupted or `maxIterations` is reached:

1. Read `python3 <pack>/bin/packopt.py status`, `packopt.md`, recent `packopt.jsonl`, `packopt.rejected.jsonl`, and the target skill file.
2. Review rollout evidence from train tasks, prior validation logs, and rejected-edit memory.
3. Pick one compact hypothesis about a recurring procedural failure.
4. Apply a bounded patch to the target skill file. Default edit budget: 1--4 localized edits. Prefer `append`, `insert_after`, `replace`, or `delete`; avoid full rewrites unless the skill is tiny or broken.
5. Do not edit protected sections delimited by `<!-- SLOW_UPDATE_START -->` and `<!-- SLOW_UPDATE_END -->` during step-level patches.
6. Run `python3 <pack>/bin/packopt.py --cwd . run`.
7. Read `packopt.last.json` for parsed metrics and output path.
8. Decide:
   - metric strictly improves over the best kept score: `keep`
   - metric is equal or worse: `discard`
   - validation crashed/timed out: `crash`
9. Log with `python3 <pack>/bin/packopt.py log ...`. Always include `--patch-summary` and ASI.
10. For discards/crashes, include `rollback_reason` and `next_action_hint` in ASI. The helper appends rejected candidates to `packopt.rejected.jsonl` and reverts non-artifact changes in git repos.
11. Periodically update `packopt.md` with distilled optimizer memory: what kinds of edits helped, what was too vague/brittle, and which regressions future patches must guard against.
12. Continue without asking for permission to keep going.

## Patch discipline

- Optimize the skill document, not the scorer or validation split.
- Keep the target model/provider/harness fixed while comparing candidates.
- Use strict held-out gating: ties are rejected.
- Prefer general, procedural, reusable rules over instance-specific fixes.
- Preserve useful existing rules unless validation evidence says they hurt.
- Keep deployed artifacts compact and inspectable.
- Never accumulate all reflections into the skill. Most proposed edits should be rejected or deferred.

## ASI discipline

`--asi` is the memory that survives reverts and compaction. Always include:

```json
{"hypothesis":"why this bounded patch should improve held-out score"}
```

For rejected candidates include:

```json
{
  "hypothesis": "what you tried",
  "rollback_reason": "why validation rejected it",
  "next_action_hint": "adjacent idea or avoid pattern"
}
```

Useful optional fields: `learned`, `failure_pattern`, `regression_risk`, `files_touched`, `target_file`, `next_focus`.

## Validation script contract

`packopt.sh` should:

- Use `#!/usr/bin/env bash` and `set -euo pipefail`.
- Evaluate the current target skill on the validation split, not the training examples used to invent the patch.
- Avoid flooding stdout; redirect verbose logs to files.
- Print one or more lines exactly like `METRIC name=value`.
- Use a primary metric name matching `init --metric-name`.
- Emit secondary metrics such as wall seconds, pass count, failure count, token cost, or task count.

Example:

```bash
#!/usr/bin/env bash
set -euo pipefail
python3 scripts/eval_pack.py --pack autoresearch --split validation > packopt-eval.log 2>&1
tail -80 packopt-eval.log
score=$(grep '^score:' packopt-eval.log | awk '{print $2}')
passes=$(grep '^passes:' packopt-eval.log | awk '{print $2}')
printf 'METRIC score=%s\n' "$score"
printf 'METRIC passes=%s\n' "$passes"
```

## When to use autoresearch instead

Use `autoresearch` when the optimized object is project code or a model/training setup. Use `packopt` when the optimized object is an agent-facing procedural text artifact such as `PACK.md`, `SKILL.md`, `DEXT.md`, or a benchmark skill file.
