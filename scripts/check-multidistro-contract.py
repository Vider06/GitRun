#!/usr/bin/env python3
"""Guard the host portability boundary against new distro-specific assumptions.

This is intentionally a regression gate, not a claim that the current host
integration is already fully portable. Existing legacy call sites are pinned
below until their platform adapters replace them. New occurrences outside
those exact files fail immediately.
"""
from __future__ import annotations

import re
import subprocess
import sys
from collections import Counter
from pathlib import PurePosixPath

RULES = {
    "apt-get": re.compile(r"\bapt-get\b"),
    "dpkg-query": re.compile(r"\bdpkg-query\b"),
    "dpkg": re.compile(r"\bdpkg(?:-deb)?\b"),
    "systemctl": re.compile(r"\bsystemctl\b"),
    "systemd-path": re.compile(r"/etc/systemd/"),
}

# Exact existing host-runtime debt. These counts may only decrease; when a
# call site is migrated, remove or lower its entry in the same change.
BASELINE = {
    "apt-get": {
        "crates/gitrun-setup/src/lib.rs": 13,
        "crates/gitrun-updater/src/lib.rs": 14,
        "crates/gitrun-cli/src/main.rs": 2,
        "crates/gitrun-scheduler/src/api_service.rs": 4,
    },
    "dpkg-query": {
        "crates/gitrun-setup/src/lib.rs": 3,
        "crates/gitrun-cli/src/main.rs": 1,
    },
    "systemctl": {
        "crates/gitrun-setup/src/lib.rs": 38,
        "crates/gitrun-recovery/src/lib.rs": 14,
        "crates/gitrun-management/src/backend.rs": 13,
        "crates/gitrun-management/src/control_manager.rs": 2,
        "crates/gitrun-cli/src/main.rs": 6,
    },
    "systemd-path": {
        "crates/gitrun-setup/src/lib.rs": 22,
        "crates/gitrun-recovery/src/lib.rs": 7,
        "crates/gitrun-management/src/backend.rs": 2,
        "crates/gitrun-cli/src/main.rs": 6,
    },
}

def tracked_files() -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"], check=True, capture_output=True
    )
    return [p.decode() for p in result.stdout.split(b"\0") if p]

def main() -> int:
    files = tracked_files()
    actual: dict[str, Counter[str]] = {rule: Counter() for rule in RULES}
    unexpected: list[str] = []
    for name in files:
        path = PurePosixPath(name)
        if path.suffix not in {".rs", ".sh", ".bash", ".ps1"}:
            continue
        if not (name.startswith(("crates/", "scripts/", "packaging/")) or name in {"install.sh", "uninstall.sh"}):
            continue
        try:
            content = open(name, encoding="utf-8").read()
        except (OSError, UnicodeError):
            continue
        for rule, pattern in RULES.items():
            count = len(pattern.findall(content))
            if count:
                actual[rule][name] = count
                if name not in BASELINE[rule]:
                    unexpected.append(f"{name}: {rule} ({count} occurrence(s))")

    errors = []
    for rule, expected_by_file in BASELINE.items():
        for name, count in actual[rule].items():
            allowed = expected_by_file.get(name)
            if allowed is None:
                continue
            if count > allowed:
                errors.append(f"{name}: {rule} increased from {allowed} to {count}")
        for name, allowed in expected_by_file.items():
            count = actual[rule].get(name, 0)
            if count > allowed:
                errors.append(f"{name}: {rule} increased from {allowed} to {count}")

    if unexpected:
        errors.extend(f"unapproved host-specific command: {item}" for item in unexpected)

    # The shared platform contract must keep all supported package managers and
    # init backends represented, and the detection crate must retain unit tests.
    platform = open("crates/gitrun-platform/src/lib.rs", encoding="utf-8").read()
    for token in ("Apt,", "Dnf,", "Zypper,", "Pacman,", "Apk,", "Xbps,", "Emerge,", "Systemd,", "OpenRc,", "Runit,"):
        if token not in platform:
            errors.append(f"platform detection contract is missing {token}")
    if "#[cfg(test)]" not in platform:
        errors.append("platform detection crate has no unit-test module")

    if errors:
        print("Multi-distro portability contract: FAIL")
        for error in errors:
            print(f" - {error}")
        print("\nKnown legacy call sites must be migrated, not copied.")
        return 1

    print("Multi-distro portability regression guard: PASS")
    print("Existing Debian/systemd host call sites remain explicitly tracked.")
    print("Container build dependencies and package-build tooling are outside this host-runtime scan.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
