#!/usr/bin/env python3
"""Regression-test the tracked-file cache fingerprint helper."""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "scripts/ci/hash-tracked-files.sh"


def fingerprint(*patterns: str) -> str:
    result = subprocess.run(
        ("bash", str(HELPER), *patterns),
        cwd=ROOT, check=True, capture_output=True, text=True,
    )
    return result.stdout.strip()


def main() -> int:
    first = fingerprint("Cargo.lock", "Cargo.toml", "**/Cargo.toml")
    second = fingerprint("Cargo.lock", "Cargo.toml", "**/Cargo.toml")
    empty = fingerprint(".gitrun-ci-nonexistent-fixture/**")
    errors = []
    if not re.fullmatch(r"[0-9a-f]{64}", first):
        errors.append("tracked-file fingerprint is not a 64-character lowercase SHA-256")
    if first != second:
        errors.append("tracked-file fingerprint is not stable across identical inputs")
    if empty:
        errors.append("unmatched tracked-file patterns must produce an empty fingerprint")
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Tracked-file cache fingerprint is stable and handles unmatched patterns.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
