#!/usr/bin/env python3
"""Build GitRun and validate the installed systemd service-unit contracts."""
from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNITS = (
    "gitrun.service",
    "gitrun-gsr.service",
    "gitrun-control.service",
)
EXPECTED = {
    "gitrun.service": (
        "WantedBy=multi-user.target",
        "Restart=on-failure",
        "ExecStart=/usr/local/bin/gitrun scheduler",
    ),
    "gitrun-gsr.service": (
        "WantedBy=multi-user.target",
        "Restart=on-failure",
        "ExecStart=/usr/local/bin/gitrun gsr-watchdog",
    ),
    "gitrun-control.service": (
        "WantedBy=multi-user.target",
        "Restart=on-failure",
        "ExecStart=/usr/local/bin/gitrun control-manager",
    ),
}


def run(*command: str) -> None:
    subprocess.run(command, cwd=ROOT, check=True)


def main() -> int:
    if shutil.which("systemd-analyze") is None:
        print("systemd-analyze is required to validate the service units", file=sys.stderr)
        return 1

    run("cargo", "build", "--locked", "-p", "gitrun-cli", "--bin", "gitrun")
    run("sudo", "install", "-Dm0755", "target/debug/gitrun", "/usr/local/bin/gitrun")
    run(
        "sudo", "install", "-Dm0600", "config/config.example.env",
        "/etc/gitrun/gitrun.env",
    )
    for unit in UNITS:
        run(
            "sudo", "install", "-Dm0644", f"systemd/{unit}",
            f"/etc/systemd/system/{unit}",
        )

    group = subprocess.run(
        ("getent", "group", "gitrun-control"),
        cwd=ROOT,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if group.returncode != 0:
        run("sudo", "groupadd", "--system", "gitrun-control")

    run("sudo", "systemd-analyze", "verify", *UNITS)

    errors: list[str] = []
    for unit, markers in EXPECTED.items():
        content = (ROOT / "systemd" / unit).read_text(encoding="utf-8")
        for marker in markers:
            if marker not in content:
                errors.append(f"systemd/{unit}: required marker missing: {marker}")

    setup = (ROOT / "crates/gitrun-setup/src/lib.rs").read_text(encoding="utf-8")
    for marker in (
        "ServiceManager::Systemd",
        "systemctl",
        "gitrun.service",
        "gitrun-gsr.service",
        "gitrun-control.service",
    ):
        if marker not in setup:
            errors.append(f"crates/gitrun-setup/src/lib.rs: required marker missing: {marker}")

    cli = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
    if "gsr-watchdog" not in cli:
        errors.append("crates/gitrun-cli/src/main.rs: gsr-watchdog command marker missing")

    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Systemd unit syntax and service command alignment verified.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
