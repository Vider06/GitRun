#!/usr/bin/env python3
"""Guard GitRun's multi-distro boundaries and package-format contracts.

This is a regression guard, not a claim that every host lifecycle is already
portable. Known legacy host integrations are inventoried with exact per-file
budgets until they are migrated to gitrun-platform. New or expanded direct
platform assumptions fail the check.
"""
from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

PATTERNS = {
    "apt-get": re.compile(r"\bapt-get\b"),
    "dpkg": re.compile(r"\bdpkg(?:-query)?\b"),
    "systemctl": re.compile(r"\bsystemctl\b"),
    "systemd-path": re.compile(r"/(?:etc|usr/lib|lib)/systemd/"),
    "systemd-run": re.compile(r"\bsystemd-run\b"),
}

# Temporary, explicit inventory of runtime paths not yet routed through the
# platform abstraction. Counts are upper bounds: a new occurrence in an
# existing file is a regression just as much as a new file would be.
RUNTIME_BASELINE = {
    "crates/gitrun-setup/src/lib.rs": {
        "apt-get": 13, "dpkg": 12, "systemctl": 38, "systemd-path": 28,
    },
    "crates/gitrun-updater/src/lib.rs": {"apt-get": 14},
    "crates/gitrun-cli/src/main.rs": {
        "apt-get": 2, "dpkg": 5, "systemctl": 6, "systemd-path": 12,
    },
    "crates/gitrun-recovery/src/lib.rs": {
        "systemctl": 6, "systemd-path": 7,
    },
    "crates/gitrun-management/src/backend.rs": {
        "systemctl": 11, "systemd-path": 6,
    },
    "crates/gitrun-management/src/control_manager.rs": {
        "systemctl": 2, "systemd-path": 3, "systemd-run": 1,
    },
    "crates/gitrun-cli/src/manage.rs": {"systemctl": 1},
}

# Conditional host integrations are not migration debt: they are reachable
# only after explicit service-manager detection and have a tested native route.
CONDITIONAL_RUNTIME_ALLOWLIST = {
    "crates/gitrun-recovery/src/json_recovery.rs": {
        "systemd-run": 2,
        "description": "systemd-only transient-unit path; OpenRC/runit use direct privileged restore",
    },
}

# These occurrences are intentional and must remain isolated to their
# documented execution context rather than copied into host lifecycle code.
CONTEXT_ALLOWLIST = {
    "crates/gitrun-platform/src/lib.rs": "native package-manager and service-manager detection",
    "crates/gitrun-core/src/compatibility.rs": "command compatibility classifier; strings are not executed",
    "crates/gitrun-scheduler/src/api_service.rs": "package installation runs as container root inside the pinned Debian runner image",
    "scripts/ci/ci-lib.sh": "CI-only apt/dpkg helpers provision Debian-based GitHub Actions runners; they are not product runtime paths",
    "scripts/ci/ci-lib.sh": "CI-only apt/dpkg helpers provision Debian-based GitHub Actions runners; they are not product runtime paths",
}

PREFIX_ALLOWLIST = (
    ".github/workflows/",  # GitHub-hosted/self-hosted runner provisioning, not product runtime.
    "docker/",             # Pinned Debian container build environments.
    "Core/Dockers/",       # Pinned Debian runner/workbench container build environments.
    "systemd/",            # Native systemd unit definitions.
)

FILE_ALLOWLIST = {
    "packaging/gitrun-postinst": "DEB-only maintainer hook",
    "packaging/gitrun-prerm": "DEB-only maintainer hook",
    "scripts/build-deb.sh": "DEB-only package builder",
}

SCANNED_SUFFIXES = {".rs", ".sh", ".ps1", ".service"}


def tracked_files() -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, check=True, capture_output=True
    )
    return [item.decode("utf-8") for item in result.stdout.split(b"\0") if item]


def read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return None


def check_platform_catalog(errors: list[str]) -> None:
    source = (ROOT / "crates/gitrun-platform/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "Apt", "Dnf", "Zypper", "Pacman", "Apk", "Xbps", "Emerge",
        "Systemd", "OpenRc", "Runit",
        "ManagedService", "ServiceAction", "service_command_candidates",
        "service_command", "service_is_installed", "trusted_privilege_escalation_binary",
        "known_distributions_prefer_their_native_package_manager",
    )
    missing = [item for item in required if item not in source]
    if missing:
        errors.append("gitrun-platform is missing expected platform contracts: " + ", ".join(missing))


def check_release_package_boundaries(errors: list[str]) -> None:
    release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    required_targets = (
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    )
    for target in required_targets:
        if target not in release:
            errors.append(f"release matrix no longer declares {target}")

    desktop_targets = (
        "target: x86_64-unknown-linux-gnu",
        "target: aarch64-unknown-linux-gnu",
        "target: x86_64-unknown-linux-musl",
        "target: aarch64-unknown-linux-musl",
        "build-dashboard-musl:",
        "runner: ubuntu-24.04-arm",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-deb.deb",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-rpm.rpm",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-archlinux.pkg.tar.zst",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-musl-apk.apk",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-musl-xbps.xbps",
        "desktop_package_formats:[\"deb\",\"rpm\",\"archlinux\",\"appimage\",\"apk\",\"xbps\"]",
        'depends: ["(webkit2gtk4.1 or libwebkit2gtk-4_1-0)", polkit]',
        "grep -Fq libwebkit2gtk-4_1-0",
    )
    for target in desktop_targets:
        if target not in release:
            errors.append(f"Desktop release matrix is missing {target}")
    native_package_contracts = (
        "package-server:\n    name: Package Server target matrix\n    needs: [prepare, collect]\n    runs-on: ${{ matrix.runner }}",
        "package-dashboard-musl:",
        "runs-on: ${{ matrix.runner }}",
        "xbps_static_arch: aarch64",
        "XBPS_STATIC_ARCH: ${{ matrix.xbps_static_arch }}",
        "xbps-static-latest.${XBPS_STATIC_ARCH}-musl.tar.xz",
        '-D "musl libwebkit2gtk41 polkit"',
        'aarch64-unknown-linux-gnu) arch=arm64; formats="deb rpm archlinux"',
        '"GitRun-Server-$VERSION-linux-aarch64-arch.pkg.tar.zst"',
    )
    for contract in native_package_contracts:
        if contract not in release:
            errors.append(f"Native package matrix is missing {contract}")
    if "xbps-static-latest.x86_64-musl.tar.xz" in release:
        errors.append("XBPS packaging must not hardcode the x86_64 static tool for ARM64 jobs")
    if "pattern: dashboard-package-*" not in release:
        errors.append("Desktop package collection must merge every target-specific package artifact")

    if 'xbps_stage="$RUNNER_TEMP/xbps-stage"' not in release:
        errors.append("XBPS must use a dedicated init-neutral staging directory")
    if 'cp -a "$stage" "$xbps_stage"' not in release:
        errors.append("XBPS staging must be copied from the neutral Server stage")
    xbps_block = release.split('xbps_root="$RUNNER_TEMP/xbps"', 1)
    if len(xbps_block) != 2:
        errors.append("could not locate the XBPS packaging block")
    else:
        block = xbps_block[1].split("Upload immutable target packages", 1)[0]
        if '"$xbps_create"' not in block or '"$xbps_stage"' not in block:
            errors.append("XBPS archive creation must consume the init-neutral stage")
        if '"$xbps_create"' in block and '"$package_stage"' in block:
            errors.append("XBPS archive must not consume the systemd-enabled package stage")


def check_portable_privilege_helpers(errors: list[str]) -> None:
    cli = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    if 'Command::new("/bin/sh")' not in cli or "trusted_privilege_command()" not in cli:
        errors.append("CLI privileged shell helpers must use POSIX sh and the trusted platform privilege helper")
    if "trusted_privilege_escalation_binary()" not in updater:
        errors.append("updater dependency installation must support the platform privilege helper")

    for relative in ("scripts/uninstall.sh", "scripts/reinstall-latest.sh"):
        source = read_text(ROOT / relative)
        if source is None:
            errors.append(f"missing portable privilege script: {relative}")
            continue
        if not source.startswith("#!/bin/sh"):
            errors.append(f"{relative} must use the POSIX shell")
        if "[[ " in source or "(( " in source or "bash" in source:
            errors.append(f"{relative} contains Bash-only syntax")
        if "trusted_helper" not in source or "/usr/bin/doas" not in source:
            errors.append(f"{relative} must validate and support doas as a privilege helper")
        syntax = subprocess.run(["sh", "-n", str(ROOT / relative)], check=False, capture_output=True, text=True)
        if syntax.returncode:
            errors.append(f"{relative} failed POSIX shell syntax validation: {syntax.stderr.strip()}")


def check_dashboard_service_manager_contract(errors: list[str]) -> None:
    backend = (ROOT / "crates/gitrun-management/src/backend.rs").read_text(encoding="utf-8")
    control = (ROOT / "crates/gitrun-management/src/control_manager.rs").read_text(encoding="utf-8")
    platform = (ROOT / "crates/gitrun-platform/src/lib.rs").read_text(encoding="utf-8")
    required_backend = (
        "gitrun_platform::detect_service_manager()",
        "gitrun_platform::service_command(",
        "gitrun_platform::service_is_installed(",
        "gitrun_platform::ManagedService::Scheduler",
        "gitrun_platform::ManagedService::Gsr",
    )
    missing = [item for item in required_backend if item not in backend]
    required_platform = ("ServiceManager::Systemd", "ServiceManager::OpenRc", "ServiceManager::Runit")
    missing.extend(item for item in required_platform if item not in platform)
    if missing:
        errors.append("Dashboard service operations are missing native init-system routing: " + ", ".join(missing))
    if 'PathBuf::from("/usr/bin/systemctl")' in backend or 'PathBuf::from("/bin/systemctl")' in backend:
        errors.append("Dashboard service controls must not hardcode systemctl executable paths")
    if "detect_service_manager()" not in control or "ServiceManager::Systemd" not in control:
        errors.append("Control Manager IPC must be gated to its installed systemd service backend")


def check_setup_native_lifecycle_contract(errors: list[str]) -> None:
    setup = (ROOT / "crates/gitrun-setup/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "current_package_manager()",
        "package_install_spec(",
        "docker_packages(",
        "buildx_packages(",
        "package_remove_spec(",
        "package_is_registered(",
        "install_gitrun_service_definitions(",
        "start_gitrun_services()",
        "stop_gitrun_services()",
        "PackageManager::Apt",
        "PackageManager::Dnf",
        "PackageManager::Zypper",
        "PackageManager::Pacman",
        "PackageManager::Apk",
        "PackageManager::Xbps",
        "PackageManager::Emerge",
        "docker_packages(manager: gitrun_platform::PackageManager)",
        "buildx_packages(manager: gitrun_platform::PackageManager)",
        'PackageManager::Dnf => &["moby-engine", "docker-compose"]',
        "ServiceManager::Systemd",
        "ServiceManager::OpenRc",
        "ServiceManager::Runit",
    )
    missing = [item for item in required if item not in setup]
    if missing:
        errors.append(
            "Setup must use native package/service manager lifecycle operations; "
            "missing: " + ", ".join(missing)
        )
    if 'Command::new("/usr/bin/apt-get").args(["install"' in setup:
        errors.append("Docker bootstrap must not invoke apt-get outside the Apt package-manager branch")


def check_updater_native_package_contract(errors: list[str]) -> None:
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "current_linux_package_manager()",
        "linux_dependency_packages(",
        "run_linux_package_command(",
        "PackageManager::Apt",
        "PackageManager::Dnf",
        "PackageManager::Zypper",
        "PackageManager::Pacman",
        "PackageManager::Apk",
        "PackageManager::Xbps",
        "PackageManager::Emerge",
        '(P::Dnf, "Docker") => &["moby-engine", "docker-compose", "docker-buildx"][..]',
    )
    missing = [item for item in required if item not in updater]
    if missing:
        errors.append(
            "Dependency updates must use native Linux package managers; missing: "
            + ", ".join(missing)
        )


def check_recovery_service_manager_contract(errors: list[str]) -> None:
    recovery = (ROOT / "crates/gitrun-recovery/src/json_recovery.rs").read_text(encoding="utf-8")
    required = (
        "main_library_restore_mode",
        "ServiceManager::Systemd",
        "ServiceManager::OpenRc",
        "ServiceManager::Runit",
        "MainLibraryRestoreMode::DirectRoot",
        "run_json_recovery_root_action(action.as_str())",
    )
    missing = [item for item in required if item not in recovery]
    if missing:
        errors.append(
            "Main Library restore must route systemd, OpenRC, and runit explicitly; "
            "missing: " + ", ".join(missing)
        )


def main() -> int:
    errors: list[str] = []
    known: list[str] = []
    conditional: list[str] = []
    files = tracked_files()

    for relative in files:
        path = Path(relative)
        if path.suffix not in SCANNED_SUFFIXES and path.name != "Dockerfile":
            continue
        if relative in FILE_ALLOWLIST or relative.startswith(PREFIX_ALLOWLIST):
            continue
        content = read_text(ROOT / path)
        if content is None:
            continue

        counts = {name: len(pattern.findall(content)) for name, pattern in PATTERNS.items()}
        counts = {name: count for name, count in counts.items() if count}
        if not counts:
            continue

        if relative in CONDITIONAL_RUNTIME_ALLOWLIST:
            contract = CONDITIONAL_RUNTIME_ALLOWLIST[relative]
            limits = {key: value for key, value in contract.items() if key != "description"}
            for name, count in counts.items():
                limit = limits.get(name, 0)
                if count != limit:
                    errors.append(
                        f"{relative}: conditional integration {name} count changed "
                        f"from the reviewed contract ({limit} -> {count})"
                    )
            conditional.append(
                f"- `{relative}`: {contract['description']} "
                f"({', '.join(f'{name}={count}' for name, count in counts.items())})"
            )
            continue

        if relative in RUNTIME_BASELINE:
            limits = RUNTIME_BASELINE[relative]
            for name, count in counts.items():
                limit = limits.get(name, 0)
                if count > limit:
                    errors.append(
                        f"{relative}: {name} occurrences grew from the reviewed baseline "
                        f"({limit} -> {count})"
                    )
            remaining = ", ".join(f"{name}={count}" for name, count in counts.items())
            known.append(f"- `{relative}`: {remaining}")
            continue

        if relative in CONTEXT_ALLOWLIST:
            known.append(f"- `{relative}`: {CONTEXT_ALLOWLIST[relative]}")
            continue

        errors.append(
            f"{relative}: unclassified platform-specific command/path occurrence(s): "
            + ", ".join(f"{name}={count}" for name, count in counts.items())
        )

    check_platform_catalog(errors)
    check_release_package_boundaries(errors)
    check_dashboard_service_manager_contract(errors)
    check_portable_privilege_helpers(errors)
    check_setup_native_lifecycle_contract(errors)
    check_updater_native_package_contract(errors)
    check_recovery_service_manager_contract(errors)

    summary = [
        "# Multi-distro contract audit",
        "",
        "The release package matrix and platform detection catalog are checked on every run.",
        "",
        "## Explicitly scoped platform-specific contexts",
        "",
        "- Debian-based Dockerfiles are intentional container build environments.",
        "- GitHub workflow package-manager commands provision the CI runner, not the installed product.",
        "- DEB maintainer hooks are package-format-specific.",
        "- The runner API package-install operation executes inside the pinned Debian runner image.",
        "",
        "## Conditional host-runtime integrations",
        "",
        "These references are intentionally limited to a service-manager-specific branch and are not classified as migration debt.",
        "",
        *(conditional or ["- No conditional host-runtime integrations found."]),
        "",
        "## Known host-runtime migration debt",
        "",
        "These reviewed legacy references remain and must be migrated to the shared platform layer. "
        "This guard prevents their spread; it does not certify full OpenRC/runit or non-Debian lifecycle support.",
        "",
        *(known or ["- No known direct platform-specific references found."]),
        "",
    ]
    summary_text = "\n".join(summary)
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as stream:
            stream.write(summary_text)

    print(summary_text)
    if errors:
        print("Multi-distro contract violations:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("Multi-distro contract audit: PASS (conditional integrations and known runtime debt are reported separately)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
