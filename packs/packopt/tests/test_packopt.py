import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PACK = Path(__file__).resolve().parents[1]
HELPER = PACK / "bin" / "packopt.py"


class PackOptHelperTests(unittest.TestCase):
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

    def init_git(self, cwd: Path) -> None:
        subprocess.run(["git", "init", "-q"], cwd=cwd, check=True)
        subprocess.run(["git", "config", "user.email", "dext@example.test"], cwd=cwd, check=True)
        subprocess.run(["git", "config", "user.name", "Dext Test"], cwd=cwd, check=True)

    def test_keep_discard_rejected_memory_and_revert(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            self.init_git(cwd)
            (cwd / "SKILL.md").write_text("base\n")
            subprocess.run(["git", "add", "SKILL.md"], cwd=cwd, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "base"], cwd=cwd, check=True)

            self.run_helper(cwd, "init", "--name", "demo", "--target-file", "SKILL.md", "--metric-name", "score", "--direction", "higher")
            self.run_helper(
                cwd,
                "log",
                "--metric", "10",
                "--status", "keep",
                "--description", "baseline",
                "--patch-summary", "baseline skill",
                "--asi", '{"hypothesis":"baseline"}',
            )

            (cwd / "SKILL.md").write_text("bad\n")
            self.run_helper(
                cwd,
                "log",
                "--metric", "9",
                "--status", "discard",
                "--description", "worse patch",
                "--patch-summary", "added brittle rule",
                "--asi", '{"hypothesis":"bad","rollback_reason":"score drop","next_action_hint":"try specific guardrail"}',
            )
            self.assertEqual((cwd / "SKILL.md").read_text(), "base\n")
            rejected = [json.loads(line) for line in (cwd / "packopt.rejected.jsonl").read_text().splitlines()]
            self.assertEqual(rejected[0]["patchSummary"], "added brittle rule")
            self.assertEqual(rejected[0]["asi"]["rollback_reason"], "score drop")

            (cwd / "SKILL.md").write_text("better\n")
            self.run_helper(
                cwd,
                "log",
                "--metric", "12",
                "--status", "keep",
                "--description", "better patch",
                "--patch-summary", "added reusable rule",
                "--asi", '{"hypothesis":"improve validation"}',
            )
            self.assertEqual((cwd / "SKILL.md").read_text(), "better\n")
            out = self.run_helper(cwd, "status").stdout
            self.assertIn("# PackOpt Summary", out)
            self.assertIn("Best     (#3): 12", out)
            self.assertIn("Rejected Memory", out)

    def test_keep_requires_strict_improvement(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            self.init_git(cwd)
            (cwd / "SKILL.md").write_text("base\n")
            subprocess.run(["git", "add", "SKILL.md"], cwd=cwd, check=True)
            subprocess.run(["git", "commit", "-q", "-m", "base"], cwd=cwd, check=True)
            self.run_helper(cwd, "init", "--name", "demo", "--target-file", "SKILL.md", "--metric-name", "score", "--direction", "higher")
            self.run_helper(cwd, "log", "--metric", "10", "--status", "keep", "--description", "baseline", "--patch-summary", "baseline")
            result = self.run_helper(
                cwd,
                "log",
                "--metric", "10",
                "--status", "keep",
                "--description", "tie",
                "--patch-summary", "no improvement",
                check=False,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("strict improvement", result.stderr)

    def test_scaffold_run_and_last_json(self):
        with tempfile.TemporaryDirectory() as td:
            cwd = Path(td)
            self.run_helper(
                cwd,
                "scaffold",
                "--goal", "improve demo skill",
                "--target-file", "SKILL.md",
                "--command", "printf 'METRIC score=7\\nMETRIC tasks=3\\n'",
                "--metric-name", "score",
            )
            self.run_helper(cwd, "init", "--name", "demo", "--target-file", "SKILL.md", "--metric-name", "score")
            self.run_helper(cwd, "run")
            last = json.loads((cwd / "packopt.last.json").read_text())
            self.assertTrue(last["passed"])
            self.assertEqual(last["primary_metric"], 7)
            self.assertEqual(last["metrics"]["tasks"], 3)


if __name__ == "__main__":
    unittest.main()
