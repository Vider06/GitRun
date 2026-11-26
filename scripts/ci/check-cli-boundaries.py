#!/usr/bin/env python3
"""Assert that the CLI and Control Manager remain independent of the Dashboard."""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    result = subprocess.run(
        ("cargo", "tree", "--locked", "-e", "normal", "-p", "gitrun-cli"),
        cwd=ROOT, check=True, capture_output=True, text=True,
    )
    graph = result.stdout
    errors = []
    if "gitrun-dashboard-tauri" in graph:
        errors.append("gitrun-cli must not depend on the dashboard package")
    if "tauri v" in graph:
        errors.append("gitrun-cli must not link the Tauri runtime")
    required = (
        "crates/gitrun-management/src/backend.rs",
        "crates/gitrun-management/src/control_manager.rs",
    )
    for relative in required:
        if not (ROOT / relative).is_file():
            errors.append(f"required shared management backend file is missing: {relative}")
    legacy = ROOT / "crates/gitrun-dashboard-tauri/src-tauri/src/control_manager.rs"
    if legacy.exists():
        errors.append(f"dashboard-local Control Manager implementation must not exist: {legacy.relative_to(ROOT)}")
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("CLI and shared Control Manager remain dashboard-independent.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
