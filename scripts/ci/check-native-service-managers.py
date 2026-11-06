#!/usr/bin/env python3
"""Static and shell-level contracts for native Linux service managers.

This check validates the OpenRC (Alpine) and runit (Void) resources shipped
with GitRun, their setup integration, and their release-package wiring. It
does not claim to replace distro-native lifecycle tests on real hosts.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

SHELL_FILES = (
    "packaging/gitrun-service",
    "packaging/openrc/gitrun",
    "packaging/openrc/gitrun-gsr",
    "packaging/runit/gitrun/run",
    "packaging/runit/gitrun/log/run",
    "packaging/runit/gitrun-gsr/run",
    "packaging/runit/gitrun-gsr/log/run",
    "packaging/apk-post-upgrade",
    "packaging/apk-pre-remove",
    "packaging/xbps/INSTALL",
    "packaging/xbps/REMOVE",
)

REQUIRED = {
    "packaging/openrc/gitrun": (
        "#!/sbin/openrc-run", "supervisor=supervise-daemon",
        "command_args_foreground=\"scheduler\"", "respawn_delay=",
        "output_log=", "start_pre()", "need net docker",
    ),
    "packaging/openrc/gitrun-gsr": (
        "#!/sbin/openrc-run", "supervisor=supervise-daemon",
        "command_args_foreground=\"gsr-watchdog\"", "respawn_delay=",
        "output_log=", "start_pre()", "need gitrun",
    ),
    "packaging/runit/gitrun/run": ("exec /usr/libexec/gitrun/gitrun-service scheduler",),
    "packaging/runit/gitrun-gsr/run": ("exec /usr/libexec/gitrun/gitrun-service gsr-watchdog",),
    "packaging/runit/gitrun/log/run": ("exec svlogd", "/var/log/gitrun/gitrun"),
    "packaging/runit/gitrun-gsr/log/run": ("exec svlogd", "/var/log/gitrun/gitrun-gsr"),
    "packaging/apk-post-upgrade": ("rc-service", "status", "restart",),
    "packaging/apk-pre-remove": ("rc-service", "stop", "rc-update del",),
    "packaging/xbps/INSTALL": ('"${ACTION:-}"', '"${UPDATE:-no}"', "sv restart"),
    "packaging/xbps/REMOVE": ('"${ACTION:-}"', '"${UPDATE:-no}"', "sv down", '"/etc/sv/$service"'),
    "packaging/gitrun-service": ("/etc/gitrun/gitrun.env", "GITHUB_TOKEN|GITRUN_*", "exec \"$executable\""),
}


def fail(message: str, errors: list[str]) -> None:
    errors.append(message)


def read(relative: str, errors: list[str]) -> str:
    path = ROOT / relative
    try:
        return path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"{relative}: cannot read ({exc})", errors)
        return ""


def main() -> int:
    errors: list[str] = []
    for relative in SHELL_FILES:
        path = ROOT / relative
        if not path.is_file():
            fail(f"missing service-manager resource: {relative}", errors)
            continue
        result = subprocess.run(
            ["sh", "-n", str(path)], cwd=ROOT, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
        )
        if result.returncode:
            fail(f"{relative}: shell syntax failed: {result.stderr.strip()}", errors)
        mode = subprocess.run(
            ["git", "ls-files", "--stage", "--", relative], cwd=ROOT,
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
        )
        if mode.returncode or not mode.stdout.strip():
            fail(f"{relative}: file is not tracked by git", errors)
        elif not mode.stdout.split()[0].endswith("100755"):
            fail(f"{relative}: expected executable git mode 100755", errors)

    for relative, markers in REQUIRED.items():
        content = read(relative, errors)
        for marker in markers:
            if marker not in content:
                fail(f"{relative}: missing lifecycle marker {marker!r}", errors)

    resources = read("crates/gitrun-setup/src/resources.rs", errors)
    for symbol, path in (
        ("GITRUN_SERVICE", "packaging/gitrun-service"),
        ("OPENRC_GITRUN", "packaging/openrc/gitrun"),
        ("OPENRC_GITRUN_GSR", "packaging/openrc/gitrun-gsr"),
        ("RUNIT_GITRUN_RUN", "packaging/runit/gitrun/run"),
        ("RUNIT_GITRUN_LOG_RUN", "packaging/runit/gitrun/log/run"),
        ("RUNIT_GITRUN_GSR_RUN", "packaging/runit/gitrun-gsr/run"),
        ("RUNIT_GITRUN_GSR_LOG_RUN", "packaging/runit/gitrun-gsr/log/run"),
    ):
        if f"const {symbol}" not in resources or path not in resources:
            fail(f"resources.rs: {symbol} must include {path}", errors)

    setup = read("crates/gitrun-setup/src/lib.rs", errors)
    for symbol in (
        "resources::GITRUN_SERVICE", "resources::OPENRC_GITRUN",
        "resources::OPENRC_GITRUN_GSR", "resources::RUNIT_GITRUN_RUN",
        "resources::RUNIT_GITRUN_LOG_RUN", "resources::RUNIT_GITRUN_GSR_RUN",
        "resources::RUNIT_GITRUN_GSR_LOG_RUN",
    ):
        if symbol not in setup:
            fail(f"gitrun-setup must install shared resource {symbol}", errors)
    install_block = setup.split("fn install_gitrun_service_definitions", 1)
    start_block = setup.split("fn start_gitrun_services", 1)
    if len(install_block) != 2 or len(start_block) != 2:
        fail("gitrun-setup service install/start functions are missing", errors)
    else:
        install_body = install_block[1].split("fn start_gitrun_services", 1)[0]
        start_body = start_block[1].split("fn stop_gitrun_services", 1)[0]
        if "ensure_runit_service_link(service)" in install_body:
            fail("runit links must not be enabled while setup is still writing resources", errors)
        if "ensure_runit_service_link(service)" not in start_body:
            fail("runit links must be created in the service-start phase", errors)
        if "/etc/init.d/gitrun" not in install_body or "/etc/sv" not in install_body:
            fail("setup must install native OpenRC and runit service trees", errors)

    release = read(".github/workflows/release.yml", errors)
    for marker in (
        "packaging/openrc/gitrun", "packaging/openrc/gitrun-gsr",
        "packaging/apk-post-upgrade", "packaging/apk-pre-remove",
        "packaging/runit/gitrun/run", "packaging/runit/gitrun-gsr/run",
        "packaging/xbps/INSTALL", "packaging/xbps/REMOVE",
    ):
        if marker not in release:
            fail(f"release workflow does not package {marker}", errors)

    if errors:
        print("Native service-manager contracts: FAIL", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("Native service-manager contracts: PASS (OpenRC, runit, setup lifecycle and package wiring)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
