#!/usr/bin/env python3
"""Assert that the headless workspace dependency graph excludes GUI libraries."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    result = subprocess.run(
        (
            "cargo", "tree", "--locked", "--workspace",
            "--exclude", "gitrun-dashboard-tauri", "-e", "normal",
        ),
        cwd=ROOT, check=True, capture_output=True, text=True,
    )
    for line in result.stdout.splitlines():
        if "tauri v" in line or "webkit2gtk v" in line:
            print(
                "::error::The headless workspace dependency graph unexpectedly includes Tauri or WebKitGTK",
                file=sys.stderr,
            )
            return 1
    print("Headless workspace dependency graph excludes Tauri and WebKitGTK.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
