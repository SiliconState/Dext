import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PACK = Path(__file__).resolve().parents[1]
HELPER = PACK / "bin" / "autoresearch.py"


class AutoresearchHelperTests(unittest.TestCase):
    def run_helper(self, cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [sys.executable, str(HELPER), "--cwd", str(cwd), *args],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if check and result.returncode != 0:
            self.fail(f"helper failed {result.returncode}\nSTDOUT:\n{result.stdout}\nSTDERR:\n{result.stderr}")
        return result

    def test_run_log_keep_and_discard(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            subprocess.run(["git", "init", "-q"], cwd=cwd, check=True)
            subprocess.run(["git", "config", "user.email", "dext@example.test"], cwd=cwd, check=True)
            subprocess.run(["git", "config", "user.name", "Dext Test"], cwd=cwd, check=True)
            (cwd / "model.txt").write_text("base\n")
            subprocess.run(["git", "add", "model.txt"], cwd=cwd, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "base"], cwd=cwd, check=True)

            (cwd / "autoresearch.sh").write_text("#!/usr/bin/env bash\nset -euo pipefail\necho 'METRIC score=10'\necho 'METRIC wall_ms=5'\n")
            os.chmod(cwd / "autoresearch.sh", 0o755)
            self.run_helper(cwd, "init", "--name", "score", "--metric-name", "score", "--direction", "lower")
            self.run_helper(cwd, "run")
            last = json.loads((cwd / "autoresearch.last.json").read_text())
            self.assertEqual(last["primary_metric"], 10)
            self.run_helper(
                cwd,
                "log",
                "--metric", "10",
                "--status", "keep",
                "--description", "baseline",
                "--metrics", '{"wall_ms":5}',
                "--asi", '{"hypothesis":"baseline"}',
            )

            (cwd / "model.txt").write_text("bad\n")
            self.run_helper(
                cwd,
                "log",
                "--metric", "11",
                "--status", "discard",
                "--description", "worse",
                "--asi", '{"hypothesis":"bad","rollback_reason":"worse","next_action_hint":"try good"}',
            )
            self.assertEqual((cwd / "model.txt").read_text(), "base\n")
            lines = [json.loads(line) for line in (cwd / "autoresearch.jsonl").read_text().splitlines()]
            self.assertEqual(lines[0]["type"], "config")
            runs = [line for line in lines if "run" in line]
            self.assertEqual([r["status"] for r in runs], ["keep", "discard"])

    def test_summary_is_deterministic(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            self.run_helper(cwd, "init", "--name", "demo", "--metric-name", "score", "--direction", "lower")
            self.run_helper(cwd, "log", "--metric", "5", "--status", "keep", "--description", "baseline", "--asi", '{"hypothesis":"base"}')
            out = self.run_helper(cwd, "status").stdout
            self.assertIn("# Autoresearch Summary", out)
            self.assertIn("Goal: demo", out)
            self.assertIn("#1 keep", out)

    def test_failed_run_still_returns_zero_for_logging(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            (cwd / "autoresearch.sh").write_text("#!/usr/bin/env bash\nset -euo pipefail\necho boom\nexit 7\n")
            os.chmod(cwd / "autoresearch.sh", 0o755)
            self.run_helper(cwd, "init", "--name", "demo", "--metric-name", "score")
            result = self.run_helper(cwd, "run", check=True)
            self.assertEqual(result.returncode, 0)
            last = json.loads((cwd / "autoresearch.last.json").read_text())
            self.assertFalse(last["passed"])
            self.assertEqual(last["exit_code"], 7)


if __name__ == "__main__":
    unittest.main()
