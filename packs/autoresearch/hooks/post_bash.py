#!/usr/bin/env python3
"""Post-bash hint for autoresearch helper calls."""

from __future__ import annotations

import json
import os
from pathlib import Path


def load_tool_input() -> dict:
    raw = os.environ.get("DEXT_TOOL_INPUT", "{}")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def main() -> int:
    tool_input = load_tool_input()
    command = str(tool_input.get("command", ""))
    result = os.environ.get("DEXT_TOOL_RESULT", "")
    if "autoresearch.py" not in command:
        return 0
    if " run" in command and "PASS" in result:
        last = Path("autoresearch.last.json")
        if last.exists():
            print("Autoresearch pack hint: run finished. Read autoresearch.last.json, then call the helper's `log` command with metric/status/ASI.")
    elif " log" in command and "logged #" in result:
        print("Autoresearch pack hint: choose the next hypothesis and continue the loop; do not stop unless interrupted or blocked.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
