#!/usr/bin/env python3
"""Dext packopt helper.

Stdlib-only state for SkillOpt-style PACK.md / skill-document optimization:
validate candidate -> parse metrics -> log keep/reject -> preserve rejected memory.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

STATUS_VALUES = {"keep", "discard", "crash"}
DIRECTION_VALUES = {"lower", "higher"}
METRIC_RE = re.compile(r"^METRIC\s+([\w.µ]+)=(\S+)\s*$", re.MULTILINE)
DENIED_METRIC_NAMES = {"__proto__", "constructor", "prototype"}
RECENT_RUN_LIMIT = 50
TAIL_LINES = 20
TAIL_BYTES = 8192


class PackError(Exception):
    pass


def artefact_path(workdir: Path, name: str) -> Path:
    return workdir / name


def jsonl_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.jsonl")


def rejected_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.rejected.jsonl")


def md_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.md")


def ideas_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.ideas.md")


def script_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.sh")


def config_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.config.json")


def last_path(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.last.json")


def runs_dir(workdir: Path) -> Path:
    return artefact_path(workdir, "packopt.runs")


def now_ms() -> int:
    return int(time.time() * 1000)


def parse_json_object(raw: str | None, label: str) -> dict[str, Any]:
    if not raw:
        return {}
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise PackError(f"{label} must be JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise PackError(f"{label} must be a JSON object")
    return value


def read_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise PackError(f"failed to parse {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise PackError(f"{path} must contain a JSON object")
    return value


def write_json(path: Path, value: dict[str, Any]) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def append_jsonl(path: Path, value: dict[str, Any]) -> None:
    with path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n")


def parse_jsonl_line(line: str) -> dict[str, Any] | None:
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, dict) else None


def nonempty_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if line.strip()]


def metric_unit_from_name(name: str) -> str:
    if name.endswith("µs"):
        return "µs"
    if name.endswith("_ms"):
        return "ms"
    if name.endswith("_s") or name.endswith("_sec"):
        return "s"
    if name.endswith("_kb"):
        return "kb"
    if name.endswith("_mb"):
        return "mb"
    return ""


def normalized_status(value: Any) -> str:
    return value if value in STATUS_VALUES else "keep"


def normalized_direction(value: Any) -> str:
    return value if value in DIRECTION_VALUES else "higher"


def reconstructed_state(workdir: Path) -> dict[str, Any]:
    state: dict[str, Any] = {
        "name": None,
        "target_file": None,
        "metric_name": "score",
        "metric_unit": "",
        "direction": "higher",
        "current_segment": 0,
        "results": [],
        "secondary_metrics": [],
        "max_iterations": None,
    }
    cfg = read_json(config_path(workdir))
    if isinstance(cfg.get("maxIterations"), int) and cfg["maxIterations"] > 0:
        state["max_iterations"] = int(cfg["maxIterations"])
    path = jsonl_path(workdir)
    if not path.exists():
        return state

    segment = 0
    secondary_names: set[str] = set()
    for line in nonempty_lines(path.read_text(encoding="utf-8")):
        entry = parse_jsonl_line(line)
        if not entry:
            continue
        if entry.get("type") == "config":
            if isinstance(entry.get("name"), str):
                state["name"] = entry["name"]
            if isinstance(entry.get("targetFile"), str):
                state["target_file"] = entry["targetFile"]
            if isinstance(entry.get("metricName"), str):
                state["metric_name"] = entry["metricName"]
            if isinstance(entry.get("metricUnit"), str):
                state["metric_unit"] = entry["metricUnit"]
            state["direction"] = normalized_direction(entry.get("bestDirection"))
            if state["results"]:
                segment += 1
                secondary_names.clear()
            state["current_segment"] = segment
            continue
        if entry.get("type") == "hook" or not isinstance(entry.get("run"), int):
            continue
        metrics = entry.get("metrics") if isinstance(entry.get("metrics"), dict) else {}
        clean_metrics = {k: v for k, v in metrics.items() if isinstance(k, str) and isinstance(v, (int, float))}
        run = {
            "run": int(entry.get("run") or len(state["results"]) + 1),
            "commit": entry.get("commit") if isinstance(entry.get("commit"), str) else "",
            "metric": float(entry.get("metric")) if isinstance(entry.get("metric"), (int, float)) else 0.0,
            "metrics": clean_metrics,
            "status": normalized_status(entry.get("status")),
            "description": entry.get("description") if isinstance(entry.get("description"), str) else "",
            "patch_summary": entry.get("patchSummary") if isinstance(entry.get("patchSummary"), str) else "",
            "timestamp": int(entry.get("timestamp")) if isinstance(entry.get("timestamp"), (int, float)) else 0,
            "segment": segment,
            "confidence": entry.get("confidence") if isinstance(entry.get("confidence"), (int, float)) else None,
            "asi": entry.get("asi") if isinstance(entry.get("asi"), dict) else None,
        }
        state["results"].append(run)
        for name in clean_metrics:
            if name not in secondary_names:
                secondary_names.add(name)
                state["secondary_metrics"].append({"name": name, "unit": metric_unit_from_name(name)})
    return state


def current_results(state: dict[str, Any]) -> list[dict[str, Any]]:
    segment = state.get("current_segment", 0)
    return [r for r in state["results"] if r.get("segment") == segment]


def baseline_metric(state: dict[str, Any]) -> float | None:
    runs = current_results(state)
    return float(runs[0]["metric"]) if runs else None


def is_better(value: float, current: float, direction: str) -> bool:
    return value < current if direction == "lower" else value > current


def best_metric(state: dict[str, Any]) -> float | None:
    direction = state.get("direction", "higher")
    kept = [float(r["metric"]) for r in current_results(state) if r.get("status") == "keep" and math.isfinite(float(r.get("metric", 0)))]
    if not kept:
        return None
    return min(kept) if direction == "lower" else max(kept)


def compute_confidence(state: dict[str, Any]) -> float | None:
    runs = [r for r in current_results(state) if math.isfinite(float(r.get("metric", 0)))]
    if len(runs) < 3:
        return None
    values = [float(r["metric"]) for r in runs]
    med = statistics.median(values)
    mad = statistics.median(abs(v - med) for v in values)
    if mad == 0:
        return None
    base = baseline_metric(state)
    best = best_metric(state)
    if base is None or best is None or best == base:
        return None
    return abs(best - base) / mad


def parse_metric_lines(output: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for name, raw in METRIC_RE.findall(output):
        if name in DENIED_METRIC_NAMES:
            continue
        try:
            value = float(raw)
        except ValueError:
            continue
        if math.isfinite(value):
            metrics[name] = value
    return metrics


def tail_text(text: str, max_lines: int = TAIL_LINES, max_bytes: int = TAIL_BYTES) -> tuple[str, bool]:
    encoded = text.encode("utf-8", errors="replace")
    truncated = False
    if len(encoded) > max_bytes:
        encoded = encoded[-max_bytes:]
        truncated = True
        while encoded and (encoded[0] & 0xC0) == 0x80:
            encoded = encoded[1:]
        text = encoded.decode("utf-8", errors="replace")
    lines = text.splitlines()
    if len(lines) > max_lines:
        text = "\n".join(lines[-max_lines:])
        truncated = True
    return text, truncated


def run_process(command: str, timeout_seconds: int, workdir: Path) -> dict[str, Any]:
    t0 = time.time()
    proc = subprocess.Popen(
        command,
        cwd=workdir,
        shell=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=(os.name != "nt"),
    )
    timed_out = False
    try:
        output, _ = proc.communicate(timeout=timeout_seconds if timeout_seconds > 0 else None)
    except subprocess.TimeoutExpired:
        timed_out = True
        if os.name != "nt":
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        else:
            proc.kill()
        try:
            output, _ = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            output, _ = proc.communicate()
    duration = time.time() - t0
    return {
        "command": command,
        "exit_code": proc.returncode,
        "timed_out": timed_out,
        "duration_seconds": duration,
        "output": output or "",
    }


def git(workdir: Path, *args: str, check: bool = False) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        ["git", *args],
        cwd=workdir,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and result.returncode != 0:
        msg = (result.stdout + result.stderr).strip()
        raise PackError(f"git {' '.join(args)} failed ({result.returncode}): {msg}")
    return result


def in_git_repo(workdir: Path) -> bool:
    return git(workdir, "rev-parse", "--is-inside-work-tree").returncode == 0


def head_short(workdir: Path) -> str:
    result = git(workdir, "rev-parse", "--short=7", "HEAD")
    return result.stdout.strip() if result.returncode == 0 else "unknown"


def stage_non_artifacts(workdir: Path) -> bool:
    git(workdir, "add", "-A", "--", ".", check=True)
    git(
        workdir,
        "reset",
        "-q",
        "--",
        ":(glob)**/packopt.*",
        ":(glob)**/packopt.*/**",
        check=False,
    )
    return git(workdir, "diff", "--cached", "--quiet").returncode != 0


def commit_keep(workdir: Path, description: str, result_payload: dict[str, Any]) -> tuple[str, str]:
    if not in_git_repo(workdir):
        return "unknown", "not a git repo; skipped commit"
    before = head_short(workdir)
    if not stage_non_artifacts(workdir):
        return before, "nothing to commit"
    message = f"packopt: {description}\n\nResult: {json.dumps(result_payload, sort_keys=True)}"
    result = git(workdir, "commit", "-m", message)
    if result.returncode != 0:
        return before, "commit failed: " + (result.stdout + result.stderr).strip()[:400]
    return head_short(workdir), (result.stdout + result.stderr).strip().splitlines()[0]


def revert_non_artifacts(workdir: Path) -> str:
    if not in_git_repo(workdir):
        return "not a git repo; skipped revert"
    checkout = git(
        workdir,
        "checkout",
        "--",
        ".",
        ":(exclude,glob)**/packopt.*",
        ":(exclude,glob)**/packopt.*/**",
    )
    clean = git(
        workdir,
        "clean",
        "-fd",
        "-e",
        "packopt.*",
        "-e",
        "**/packopt.*/**",
    )
    out = (checkout.stdout + checkout.stderr + clean.stdout + clean.stderr).strip()
    return out or "reverted changes; packopt artifacts preserved"


def format_metric(value: float, unit: str = "") -> str:
    if value == int(value):
        return f"{int(value)}{unit}"
    return f"{value:.6g}{unit}"


def format_delta(value: float, base: float | None) -> str:
    if base is None or base == 0 or value == base:
        return ""
    pct = ((value - base) / base) * 100
    sign = "+" if pct > 0 else ""
    return f" ({sign}{pct:.1f}%)"


def command_runs_packopt_sh(command: str) -> bool:
    stripped = command.strip()
    return bool(re.match(r"^(?:(?:bash|sh)\s+)?(?:\./)?packopt\.sh(?:\s|$)", stripped))


def cmd_init(args: argparse.Namespace) -> int:
    workdir = Path(args.cwd).resolve()
    if args.direction not in DIRECTION_VALUES:
        raise PackError("--direction must be lower or higher")
    target = Path(args.target_file)
    target_for_log = str(target)
    if target.is_absolute():
        try:
            target_for_log = str(target.resolve().relative_to(workdir))
        except ValueError:
            target_for_log = str(target.resolve())
    path = jsonl_path(workdir)
    if path.exists() and not args.reinit:
        state = reconstructed_state(workdir)
        if state["results"] or state["name"]:
            raise PackError("packopt.jsonl already exists; use --reinit to start a new segment")
    entry = {
        "type": "config",
        "name": args.name,
        "targetFile": target_for_log,
        "metricName": args.metric_name,
        "metricUnit": args.metric_unit or "",
        "bestDirection": args.direction,
    }
    append_jsonl(path, entry)
    if args.max_iterations:
        cfg = read_json(config_path(workdir))
        cfg["maxIterations"] = int(args.max_iterations)
        write_json(config_path(workdir), cfg)
    print(f"initialized: {args.name}")
    print(f"target: {target_for_log}")
    print(f"metric: {args.metric_name} ({args.metric_unit or 'unitless'}, {args.direction} is better)")
    print(f"state: {path}")
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    workdir = Path(args.cwd).resolve()
    state = reconstructed_state(workdir)
    metric_name = state["metric_name"]
    command = args.command
    if not command:
        if not script_path(workdir).exists():
            raise PackError("no command provided and packopt.sh does not exist")
        command = "bash packopt.sh"
    if script_path(workdir).exists() and not command_runs_packopt_sh(command) and args.require_script:
        raise PackError("packopt.sh exists; run it through this pack helper or pass --no-require-script")

    run_number = len(state["results"]) + 1
    result = run_process(command, args.timeout_seconds, workdir)
    passed = result["exit_code"] == 0 and not result["timed_out"]
    metrics = parse_metric_lines(result["output"])
    primary = metrics.get(metric_name)

    runs_dir(workdir).mkdir(exist_ok=True)
    output_path = runs_dir(workdir) / f"run-{run_number:04d}.log"
    output_path.write_text(result["output"], encoding="utf-8")

    last = {
        "run": run_number,
        "command": command,
        "exit_code": result["exit_code"],
        "timed_out": result["timed_out"],
        "duration_seconds": result["duration_seconds"],
        "passed": passed,
        "metrics": metrics,
        "primary_metric": primary,
        "metric_name": metric_name,
        "output_path": str(output_path),
        "timestamp": now_ms(),
    }
    write_json(last_path(workdir), last)

    if result["timed_out"]:
        print(f"TIMEOUT after {result['duration_seconds']:.1f}s")
    elif not passed:
        print(f"FAILED exit={result['exit_code']} seconds={result['duration_seconds']:.1f}")
    else:
        print(f"PASS seconds={result['duration_seconds']:.1f}")
    if metrics:
        print("metrics: " + " ".join(f"{k}={v:g}" for k, v in metrics.items()))
        if primary is not None:
            print(f"primary: {metric_name}={primary:g}")
    else:
        print("metrics: none parsed; emit lines like METRIC name=number")
    print(f"output: {output_path}")
    tail, truncated = tail_text(result["output"])
    if tail:
        print("--- tail ---")
        print(tail)
        if truncated:
            print("--- tail truncated; inspect output path for full log ---")
    return 0


def append_rejected(workdir: Path, entry: dict[str, Any], best_before: float | None) -> None:
    rejected = {
        "run": entry["run"],
        "metric": entry["metric"],
        "bestBefore": best_before,
        "status": entry["status"],
        "description": entry["description"],
        "patchSummary": entry.get("patchSummary", ""),
        "timestamp": entry["timestamp"],
    }
    if entry.get("asi"):
        rejected["asi"] = entry["asi"]
    append_jsonl(rejected_path(workdir), rejected)


def cmd_log(args: argparse.Namespace) -> int:
    workdir = Path(args.cwd).resolve()
    state = reconstructed_state(workdir)
    if args.status not in STATUS_VALUES:
        raise PackError("--status must be keep, discard, or crash")

    metric = float(args.metric)
    if not math.isfinite(metric):
        raise PackError("--metric must be finite")
    metrics = parse_json_object(args.metrics, "--metrics")
    asi = parse_json_object(args.asi, "--asi")
    metrics = {k: float(v) for k, v in metrics.items() if isinstance(v, (int, float)) and math.isfinite(float(v))}
    commit = args.commit or head_short(workdir)
    best_before = best_metric(state)

    if args.status == "keep" and best_before is not None and not is_better(metric, best_before, state["direction"]):
        if args.force:
            print("warning: forced keep without strict improvement", file=sys.stderr)
        else:
            raise PackError(
                f"keep requires strict improvement over best kept {state['metric_name']}={format_metric(best_before, state['metric_unit'])}; "
                "log as discard or pass --force"
            )
    if args.status != "keep" and best_before is not None and is_better(metric, best_before, state["direction"]):
        print("warning: rejected metric is better than current best", file=sys.stderr)

    result_payload = {"status": args.status, state["metric_name"]: metric, **metrics}
    git_note = ""
    if args.status == "keep":
        commit, git_note = commit_keep(workdir, args.description, result_payload)

    entry = {
        "run": len(state["results"]) + 1,
        "commit": str(commit)[:7],
        "metric": metric,
        "metrics": metrics,
        "status": args.status,
        "description": args.description,
        "patchSummary": args.patch_summary,
        "timestamp": now_ms(),
        "segment": state["current_segment"],
        "confidence": None,
    }
    if asi:
        entry["asi"] = asi

    projected = reconstructed_state(workdir)
    projected["results"].append({**entry, "segment": projected["current_segment"]})
    conf = compute_confidence(projected)
    entry["confidence"] = conf
    append_jsonl(jsonl_path(workdir), entry)

    revert_note = ""
    if args.status != "keep":
        append_rejected(workdir, entry, best_before)
        revert_note = revert_non_artifacts(workdir)

    print(f"logged #{entry['run']}: {args.status} — {args.description}")
    base = baseline_metric(projected)
    if base is not None:
        print(f"baseline {state['metric_name']}: {format_metric(base, state['metric_unit'])}")
        print(f"this     {state['metric_name']}: {format_metric(metric, state['metric_unit'])}{format_delta(metric, base)}")
    if best_before is not None:
        print(f"best before: {format_metric(best_before, state['metric_unit'])}")
    if conf is not None:
        print(f"confidence: {conf:.1f}x noise floor")
    if git_note:
        print(f"git: {git_note}")
    if revert_note:
        print(f"git: {revert_note}")
    max_iterations = projected.get("max_iterations")
    if isinstance(max_iterations, int) and len(current_results(projected)) >= max_iterations:
        print(f"STOP: maxIterations reached ({max_iterations})")
    return 0


def build_summary(workdir: Path) -> str:
    state = reconstructed_state(workdir)
    runs = current_results(state)
    lines: list[str] = [
        "# PackOpt Summary",
        "",
        "Persisted files are the source of truth. Continue the loop from this summary plus live tools.",
        "",
        "## Session",
        "",
        f"Goal: {state['name'] or '—'}",
        f"Target: {state['target_file'] or '—'}",
        f"Metric: {state['metric_name']} — {state['direction']} is better",
        f"Runs so far: {len(runs)}",
    ]
    if runs:
        base = float(runs[0]["metric"])
        best = best_metric(state)
        lines.append(f"Baseline (#{runs[0]['run']}): {format_metric(base, state['metric_unit'])}")
        if best is not None:
            best_run = next((r for r in runs if r.get("status") == "keep" and float(r["metric"]) == best), None)
            run_label = f"#{best_run['run']}" if best_run else "?"
            lines.append(f"Best     ({run_label}): {format_metric(best, state['metric_unit'])}{format_delta(best, base)}")
    rules = md_path(workdir).read_text(encoding="utf-8").strip() if md_path(workdir).exists() else ""
    if rules:
        lines += ["", "## Runbook / Optimizer Memory (packopt.md)", "", rules]
    ideas = ideas_path(workdir).read_text(encoding="utf-8").strip() if ideas_path(workdir).exists() else ""
    if ideas:
        lines += ["", "## Ideas Backlog (packopt.ideas.md)", "", ideas]
    lines += ["", f"## Recent Runs (last {min(len(state['results']), RECENT_RUN_LIMIT)})", ""]
    if not state["results"]:
        lines.append("No runs yet — run the baseline first.")
    else:
        first_by_segment: dict[int, float] = {}
        for run in state["results"]:
            first_by_segment.setdefault(int(run["segment"]), float(run["metric"]))
        for run in state["results"][-RECENT_RUN_LIMIT:]:
            base = first_by_segment.get(int(run["segment"]))
            parts = [
                f"#{run['run']} {str(run['status']).ljust(7)} {format_metric(float(run['metric']))}{format_delta(float(run['metric']), base)}",
                f"desc: {run['description']}",
            ]
            if run.get("patch_summary"):
                parts.append(f"patch: {run['patch_summary']}")
            if run.get("asi"):
                asi = run["asi"]
                if isinstance(asi.get("hypothesis"), str):
                    parts.append(f"hypothesis: {asi['hypothesis']}")
                if isinstance(asi.get("next_action_hint"), str):
                    parts.append(f"next: {asi['next_action_hint']}")
            lines.append("- " + " | ".join(parts))
    if rejected_path(workdir).exists():
        rejected = [parse_jsonl_line(line) for line in nonempty_lines(rejected_path(workdir).read_text(encoding="utf-8"))]
        rejected = [r for r in rejected if r]
        if rejected:
            lines += ["", f"## Rejected Memory (last {min(len(rejected), 10)})", ""]
            for item in rejected[-10:]:
                detail = item.get("patchSummary") or item.get("description") or "rejected candidate"
                asi = item.get("asi") if isinstance(item.get("asi"), dict) else {}
                hint = f"; next: {asi['next_action_hint']}" if isinstance(asi.get("next_action_hint"), str) else ""
                lines.append(f"- #{item.get('run')} {item.get('status')} {format_metric(float(item.get('metric', 0)))}: {detail}{hint}")
    return "\n".join(lines) + "\n"


def cmd_status(args: argparse.Namespace) -> int:
    print(build_summary(Path(args.cwd).resolve()), end="")
    return 0


def cmd_scaffold(args: argparse.Namespace) -> int:
    workdir = Path(args.cwd).resolve()
    if md_path(workdir).exists() and not args.force:
        raise PackError("packopt.md exists; pass --force to overwrite scaffold")
    if script_path(workdir).exists() and not args.force:
        raise PackError("packopt.sh exists; pass --force to overwrite scaffold")
    target = args.target_file
    md_path(workdir).write_text(
        "\n".join(
            [
                f"# PackOpt: {args.goal}",
                "",
                "## Objective",
                args.goal,
                "",
                "## Target skill",
                target,
                "",
                "## Metric and gate",
                f"- Primary metric: `{args.metric_name}` ({args.direction} is better).",
                "- Baseline is the first logged keep run.",
                "- Keep candidates only on strict held-out validation improvement; reject ties.",
                "",
                "## Task splits",
                "- Train: TODO — tasks/logs used to discover recurring failure patterns.",
                "- Validation: TODO — tasks run by `packopt.sh` for keep/reject decisions.",
                "- Test: TODO — untouched final-report tasks.",
                "",
                "## Edit constraints",
                "- Optimize the target skill, not the scorer or validation split.",
                "- Use 1--4 localized edits per candidate.",
                "- Prefer procedural rules over instance-specific fixes.",
                "- Do not edit protected `SLOW_UPDATE` sections during step-level patches.",
                "",
                "## Optimizer memory",
                "- Accepted patterns: TODO",
                "- Rejected/avoid patterns: TODO",
                "- Regression risks: TODO",
                "",
            ]
        ),
        encoding="utf-8",
    )
    script_path(workdir).write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        f"{args.command}\n",
        encoding="utf-8",
    )
    os.chmod(script_path(workdir), 0o755)
    print(f"wrote {md_path(workdir)}")
    print(f"wrote {script_path(workdir)}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Dext packopt helper")
    parser.add_argument("--cwd", default=".", help="project working directory")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("init", help="initialize or reinitialize a packopt session")
    p.add_argument("--name", required=True)
    p.add_argument("--target-file", required=True)
    p.add_argument("--metric-name", default="score")
    p.add_argument("--metric-unit", default="")
    p.add_argument("--direction", choices=sorted(DIRECTION_VALUES), default="higher")
    p.add_argument("--max-iterations", type=int)
    p.add_argument("--reinit", action="store_true", help="append a new config segment")
    p.set_defaults(func=cmd_init)

    p = sub.add_parser("run", help="run packopt.sh or a validation command and parse metrics")
    p.add_argument("--command")
    p.add_argument("--timeout-seconds", type=int, default=900)
    p.add_argument("--require-script", dest="require_script", action="store_true", default=True)
    p.add_argument("--no-require-script", dest="require_script", action="store_false")
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("log", help="log a candidate and keep/revert changes")
    p.add_argument("--metric", type=float, required=True)
    p.add_argument("--status", choices=sorted(STATUS_VALUES), required=True)
    p.add_argument("--description", required=True)
    p.add_argument("--patch-summary", required=True)
    p.add_argument("--metrics", default="{}")
    p.add_argument("--asi", default="{}")
    p.add_argument("--commit", default="")
    p.add_argument("--force", action="store_true")
    p.set_defaults(func=cmd_log)

    p = sub.add_parser("status", help="print resumable summary")
    p.set_defaults(func=cmd_status)

    p = sub.add_parser("scaffold", help="write starter packopt.md and packopt.sh")
    p.add_argument("--goal", required=True)
    p.add_argument("--target-file", required=True)
    p.add_argument("--command", required=True)
    p.add_argument("--metric-name", default="score")
    p.add_argument("--direction", choices=sorted(DIRECTION_VALUES), default="higher")
    p.add_argument("--force", action="store_true")
    p.set_defaults(func=cmd_scaffold)
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except PackError as exc:
        print(f"packopt: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
