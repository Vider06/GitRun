#!/usr/bin/env python3
"""Check security-sensitive release and runtime configuration invariants."""
from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REQUIRED = {
    ".github/workflows/release.yml": (
        "GITRUN_UPDATE_SIGNING_PRIVATE_KEY_B64",
        "GITRUN_UPDATE_SIGNING_KEY_ID",
    ),
    "crates/gitrun-updater/src/lib.rs": (
        "GITRUN_UPDATE_SIGNATURE_REQUIRED",
        "signature_v2",
        "legacy_signing_payload",
    ),
    ".github/workflows/release-type-change.yml": (
        "signature_v2",
        "legacy_payload",
        "v2_payload",
    ),
    "config/config.example.env": ("GITRUN_RUNNER_NETWORK",),
}
FORBIDDEN = {
    ".github/workflows/release.yml": (
        "id-token: write",
        "aws-actions/configure-aws-credentials@",
        "ED25519_SHA_512",
    ),
}


def main() -> int:
    errors: list[str] = []
    for relative, needles in REQUIRED.items():
        content = (ROOT / relative).read_text(encoding="utf-8")
        for needle in needles:
            if needle not in content:
                errors.append(f"{relative}: required security marker missing: {needle}")
    for relative, needles in FORBIDDEN.items():
        content = (ROOT / relative).read_text(encoding="utf-8")
        for needle in needles:
            if needle in content:
                errors.append(f"{relative}: forbidden security marker present: {needle}")
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Security-sensitive workflow and runtime invariants are present.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
