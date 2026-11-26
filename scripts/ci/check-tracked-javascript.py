#!/usr/bin/env python3
"""Run Node's syntax checker over tracked JavaScript files in one package."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: check-tracked-javascript.py <path-prefix> <package-label>", file=sys.stderr)
        return 2
    prefix, label = sys.argv[1:]
    result = subprocess.run(
        (
            "git", "ls-files", "-z", "--",
            f"{prefix}*.js", f"{prefix}*.mjs", f"{prefix}*.cjs",
        ),
        cwd=ROOT, check=True, capture_output=True,
    )
    files = [
        item for item in result.stdout.decode("utf-8", errors="replace").split("\0")
        if item and not item.endswith(".min.js")
    ]
    if not files:
        print(f"::warning::{label}: no tracked JavaScript files found")
        return 0

    failures = 0
    for relative in files:
        checked = subprocess.run(
            ("node", "--check", relative),
            cwd=ROOT, check=False, capture_output=True, text=True,
        )
        if checked.stdout:
            print(checked.stdout, end="")
        if checked.stderr:
            print(checked.stderr, end="", file=sys.stderr)
        if checked.returncode:
            print(
                f"::error file={relative},title=JavaScript syntax::Node syntax check failed",
                file=sys.stderr,
            )
            failures += 1
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
