#!/usr/bin/env python3
"""Validate GitRun systemd units in an isolated, non-invasive systemd context."""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNITS = ("gitrun.service", "gitrun-gsr.service", "gitrun-control.service")
REQUIRED = {
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


def run(command: list[str], *, env: dict[str, str] | None = None) -> None:
    subprocess.run(command, cwd=ROOT, env=env, check=True)


def main() -> int:
    if shutil.which("systemd-analyze") is None:
        print("systemd-analyze is required; refusing to skip systemd unit validation", file=sys.stderr)
        return 1

    runner_temp = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
    unit_dir = runner_temp / "gitrun-ci" / "systemd-units"
    runtime_dir = runner_temp / "gitrun-ci" / "systemd-runtime"
    unit_dir.mkdir(parents=True, exist_ok=True)
    runtime_dir.mkdir(parents=True, exist_ok=True)

    stub = runtime_dir / "gitrun"
    stub.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    stub.chmod(0o755)

    for unit in UNITS:
        source = (ROOT / "systemd" / unit).read_text(encoding="utf-8")
        (unit_dir / unit).write_text(
            source.replace("/usr/local/bin/gitrun", str(stub)),
            encoding="utf-8",
        )

    group = subprocess.run(
        ("getent", "group", "gitrun-control"),
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if group.returncode:
        run(["sudo", "groupadd", "--system", "gitrun-control"])

    (unit_dir / "docker.service").write_text(
        "[Unit]\nDescription=GitRun CI-only Docker dependency stub\n"
        "[Service]\nType=oneshot\nExecStart=/usr/bin/true\nRemainAfterExit=yes\n",
        encoding="utf-8",
    )

    env = os.environ.copy()
    env.update(
        TERM="dumb",
        SYSTEMD_COLORS="0",
        SYSTEMD_PAGER="cat",
        SYSTEMD_UNIT_PATH=(
            f"{unit_dir}:/etc/systemd/system:/usr/lib/systemd/system:/lib/systemd/system"
        ),
    )
    command = [
        "systemd-analyze", "--man=no", "--recursive-errors=yes", "verify",
        *(str(unit_dir / unit) for unit in (*UNITS, "docker.service")),
    ]
    run(command, env=env)

    errors: list[str] = []
    for unit, markers in REQUIRED.items():
        source = (ROOT / "systemd" / unit).read_text(encoding="utf-8")
        for marker in markers:
            if marker not in source:
                errors.append(f"systemd/{unit}: required service contract missing: {marker}")

    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("Isolated systemd unit syntax, boot and restart contracts verified.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
