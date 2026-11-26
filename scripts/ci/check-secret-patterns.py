#!/usr/bin/env python3
"""Scan tracked text files for credential-shaped strings without printing values."""
from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PATTERN = re.compile(
    "".join(
        (
            r"gh[pousr]" + r"_[A-Za-z0-9_]{20,}|",
            r"github" + r"_pat_[A-Za-z0-9_]{20,}|",
            r"AKIA[0-9A-Z]{16}|-----BEGIN (RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----|",
            r"xox[baprs]" + r"-[A-Za-z0-9-]{10,}|",
            r"sk_live" + r"_[0-9a-zA-Z]{24,}",
        )
    ),
    re.IGNORECASE,
)


def main() -> int:
    files = subprocess.run(
        ("git", "ls-files", "-z"), cwd=ROOT, check=True, capture_output=True,
    ).stdout.decode("utf-8", errors="replace").split("\0")
    errors = 0
    excluded = {"docs/ROADMAP_PHASES.md", "SECURITY.md"}
    for relative in filter(None, files):
        if relative in excluded:
            continue
        try:
            raw = (ROOT / relative).read_bytes()
        except OSError:
            continue
        if b"\0" in raw:
            continue
        for number, line in enumerate(raw.decode("utf-8", errors="replace").splitlines(), 1):
            if PATTERN.search(line):
                print(
                    f"::error file={relative},line={number},title=possible credential::"
                    "credential-like string committed (value hidden): revoke it and purge it from history"
                )
                errors += 1
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
