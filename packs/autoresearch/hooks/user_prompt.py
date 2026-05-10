#!/usr/bin/env python3
"""Light prompt hook for the autoresearch pack.

Dext appends stdout to the user prompt. Keep output tiny and opportunistic.
"""

from __future__ import annotations

import os
from pathlib import Path


def main() -> int:
    text = os.environ.get("DEXT_USER_INPUT", "").lower()
    if "autoresearch" not in text:
        return 0
    cwd = Path.cwd()
    pack = os.environ.get("DEXT_PACK_AUTORESEARCH_DIR", "packs/autoresearch")
    if (cwd / "autoresearch.jsonl").exists() or (cwd / "autoresearch.md").exists():
        print(
            "Autoresearch pack hint: existing autoresearch state found. "
            f"Run `python3 {pack}/bin/autoresearch.py status` before continuing."
        )
    else:
        print(
            "Autoresearch pack hint: read the pack workflow first, then scaffold/init "
            "autoresearch.md, autoresearch.sh, and the baseline run."
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
