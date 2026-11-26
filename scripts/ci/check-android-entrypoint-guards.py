#!/usr/bin/env python3
"""Ensure every executable entry point guards unsupported Android runtimes."""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ENTRYPOINTS = (
    "crates/gitrun-cli/src/main.rs",
    "crates/gitrun-scheduler/src/main.rs",
    "crates/gitrun-gsr/src/main.rs",
    "crates/gitrun-gsr/src/bin/gitrun-api.rs",
    "crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs",
    "crates/gitrun-recovery/src/main.rs",
    "crates/gitrun-dashboard-tauri/src-tauri/src/main.rs",
)


def main() -> int:
    errors: list[str] = []
    for relative in ENTRYPOINTS:
        path = ROOT / relative
        if not path.is_file():
            errors.append(f"{relative}: executable entry point is missing")
            continue
        lines = path.read_text(encoding="utf-8").splitlines()
        main_line = next(
            (
                index
                for index, line in enumerate(lines)
                if re.match(r"^\s*(?:pub\s+)?fn\s+main\s*\(\s*\)\s*(?:->\s*[^{}]+)?\s*\{?\s*$", line)
            ),
            None,
        )
        if main_line is None:
            errors.append(f"{relative}: fn main() was not found")
            continue
        guard_line = next(
            (
                index
                for index in range(main_line + 1, min(len(lines), main_line + 7))
                if "ensure_supported_runtime()" in lines[index]
            ),
            None,
        )
        if guard_line is None:
            errors.append(
                f"{relative}: ensure_supported_runtime() missing or more than six lines after fn main()"
            )
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"Android runtime guard verified in all {len(ENTRYPOINTS)} executable entry points.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
