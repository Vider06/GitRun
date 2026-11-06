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
    "scripts/ci/force-ubuntu-archive-mirror.sh": "CI-only apt-get configures the Ubuntu archive mirror on the runner; it is not a product runtime path",
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
    expected_jobs = (
        "prepare",
        "build-server",
        "build-dashboard",
        "collect",
        "package-server",
        "package-dashboard",
        "migration-package",
        "validate",
        "sign",
        "release",
    )
    for job in expected_jobs:
        count = release.count(f"\n  {job}:\n")
        if count != 1:
            errors.append(f"release workflow must declare {job} exactly once; found {count}")

    install_notes = release.find('"## Install or update",')
    overview_notes = release.find('"## Release overview",')
    if install_notes < 0 or overview_notes < 0 or install_notes > overview_notes:
        errors.append("release notes must put install/update instructions before the release overview")

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
        "kind: gnu",
        "kind: musl",
        "ghcr.io/void-linux/void-musl-full:20260801r1",
        "libwebkit2gtk41-devel",
        "rustup-init",
        "runner: ubuntu-22.04-arm",
        "runner: ubuntu-22.04",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-deb.deb",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-rpm.rpm",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-archlinux.pkg.tar.zst",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-musl-apk.apk",
        'if [ "$DESKTOP_ARCH" = "x86_64" ]',
        "Alpine ARM64 Desktop APK must not be published",
        "GitRun-Desktop-$VERSION-linux-$DESKTOP_ARCH-musl-xbps.xbps",
        "desktop_package_formats:[\"deb\",\"rpm\",\"archlinux\",\"appimage\",\"apk\",\"xbps\"],desktop_package_arches:{apk:[\"x86_64\"],xbps:[\"x86_64\",\"aarch64\"]}",
        'depends: ["(webkit2gtk4.1 or libwebkit2gtk-4_1-0)", polkit]',
        "grep -Fq libwebkit2gtk-4_1-0",
    )
    for target in desktop_targets:
        if target not in release:
            errors.append(f"Desktop release matrix is missing {target}")

    # Desktop GNU and musl targets are one matrix per lifecycle stage. Keep
    # the four target variants together while preserving distro-specific steps.
    build_job = release.split("  build-dashboard:\n", 1)[1].split("  collect:\n", 1)[0]
    package_job = release.split("  package-dashboard:\n", 1)[1].split("  migration-package:\n", 1)[0]
    for legacy_job in ("build-dashboard-musl:", "package-dashboard-musl:"):
        if legacy_job in release:
            errors.append(f"Desktop matrix must not be split into the legacy job {legacy_job}")
    for target in required_targets:
        if f"target: {target}" not in build_job:
            errors.append(f"Unified Desktop build matrix is missing {target}")
        if f"target: {target}" not in package_job:
            errors.append(f"Unified Desktop package matrix is missing {target}")
    if "kind: gnu" not in build_job or "kind: musl" not in build_job:
        errors.append("Unified Desktop build matrix must retain GNU and musl build paths")
    if "kind: gnu" not in package_job or "kind: musl" not in package_job:
        errors.append("Unified Desktop package matrix must retain GNU and musl package paths")
    native_package_contracts = (
        "package-server:\n    name: Package Server target matrix\n    needs: [prepare, collect]\n    runs-on: ${{ matrix.runner }}",
        "package-dashboard:\n    name: Package independent Desktop",
        "kind: gnu",
        "kind: musl",
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


def check_native_init_package_resources(errors: list[str]) -> None:
    """Validate the packaged OpenRC/runit files as executable service contracts."""
    release = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
    resources = (ROOT / "crates/gitrun-setup/src/resources.rs").read_text(encoding="utf-8")
    setup = (ROOT / "crates/gitrun-setup/src/lib.rs").read_text(encoding="utf-8")

    expected = {
        "packaging/gitrun-service": ("GITHUB_TOKEN|GITRUN_*", "/usr/bin/gitrun"),
        "packaging/openrc/gitrun": ("#!/sbin/openrc-run", 'command_args_foreground="scheduler"', "supervisor=supervise-daemon", "respawn_delay=5", "mkdir -p /var/log/gitrun", "need net docker"),
        "packaging/openrc/gitrun-gsr": ("#!/sbin/openrc-run", 'command_args_foreground="gsr-watchdog"', "supervisor=supervise-daemon", "respawn_delay=5", "mkdir -p /var/log/gitrun", "need gitrun"),
        "packaging/runit/gitrun/run": ("#!/bin/sh", "gitrun-service scheduler"),
        "packaging/runit/gitrun/log/run": ("#!/bin/sh", "svlogd -tt /var/log/gitrun/gitrun"),
        "packaging/runit/gitrun-gsr/run": ("#!/bin/sh", "gitrun-service gsr-watchdog"),
        "packaging/runit/gitrun-gsr/log/run": ("#!/bin/sh", "svlogd -tt /var/log/gitrun/gitrun-gsr"),
        "packaging/apk-pre-remove": ("rc-service", "rc-update del"),
        "packaging/apk-post-upgrade": ("rc-service", "restart"),
        "packaging/xbps/INSTALL": ("ACTION", "sv restart", "UPDATE"),
        "packaging/xbps/REMOVE": ("ACTION", "sv down", "UPDATE"),
        "packaging/gitrun-migration-prerm": ("in-favour", "gitrun-server", "systemctl disable --now"),
    }
    for relative, markers in expected.items():
        path = ROOT / relative
        source = read_text(path)
        mode = subprocess.run(
            ["git", "ls-files", "--stage", "--", relative],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        if mode.returncode or not mode.stdout.startswith("100755 "):
            errors.append(f"{relative} must be tracked with executable mode 100755")
        if source is None:
            errors.append(f"missing native init resource: {relative}")
            continue
        for marker in markers:
            if marker not in source:
                errors.append(f"{relative} is missing required contract marker: {marker}")
        syntax = subprocess.run(
            ["sh", "-n", str(path)], check=False, capture_output=True, text=True
        )
        if syntax.returncode:
            errors.append(
                f"{relative} failed shell syntax validation: {syntax.stderr.strip()}"
            )

    resource_markers = (
        'include_str!("../../../packaging/gitrun-service")',
        'include_str!("../../../packaging/openrc/gitrun")',
        'include_str!("../../../packaging/openrc/gitrun-gsr")',
        'include_str!("../../../packaging/runit/gitrun/run")',
        'include_str!("../../../packaging/runit/gitrun/log/run")',
        'include_str!("../../../packaging/runit/gitrun-gsr/run")',
        'include_str!("../../../packaging/runit/gitrun-gsr/log/run")',
    )
    for marker in resource_markers:
        if marker not in resources:
            errors.append(f"setup resources are not sourced from the packaged file: {marker}")

    for marker in (
        "resources::GITRUN_SERVICE",
        "resources::OPENRC_GITRUN",
        "resources::OPENRC_GITRUN_GSR",
        "resources::RUNIT_GITRUN_RUN",
        "resources::RUNIT_GITRUN_LOG_RUN",
        "resources::RUNIT_GITRUN_GSR_RUN",
        "resources::RUNIT_GITRUN_GSR_LOG_RUN",
        'Path::new("/etc/init.d/gitrun")',
        'Path::new("/etc/sv").join(service)',
        'Path::new("/var/service")',
    ):
        if marker not in setup:
            errors.append(f"setup lifecycle is missing packaged native-init contract: {marker}")

    workflow_markers = (
        'install -m 0755 packaging/gitrun-service "$package_stage/usr/libexec/gitrun/gitrun-service"',
        'install -m 0755 packaging/openrc/gitrun "$package_stage/etc/init.d/gitrun"',
        'install -m 0755 packaging/openrc/gitrun-gsr "$package_stage/etc/init.d/gitrun-gsr"',
        'usr/libexec/gitrun/gitrun-service',
        'etc/init.d/gitrun etc/init.d/gitrun-gsr',
        "preremove: packaging/apk-pre-remove",
        "postupgrade: packaging/apk-post-upgrade",
        ".pre-deinstall$",
        ".post-upgrade$",
        'install -m 0755 packaging/xbps/INSTALL "$xbps_stage/INSTALL"',
        'install -m 0755 packaging/xbps/REMOVE "$xbps_stage/REMOVE"',
        ' -p install-script -S ',
        ' -p remove-script -S ',
        'install -m 0755 packaging/runit/gitrun/run "$xbps_stage/etc/sv/gitrun/run"',
        'install -m 0755 packaging/runit/gitrun/log/run "$xbps_stage/etc/sv/gitrun/log/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/run "$xbps_stage/etc/sv/gitrun-gsr/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/log/run "$xbps_stage/etc/sv/gitrun-gsr/log/run"',
        'xbps_contents="$(tar -tf "$xbps_file")"',
        'XBPS package must contain runit resources, not systemd units',
        'install -m 0755 packaging/gitrun-migration-prerm "$root/DEBIAN/prerm"',
        'test ! -s "$mock_log"',
        "grep -Fq 'disable --now gitrun-gsr.service gitrun.service' \"$mock_log\"",
    )
    for marker in workflow_markers:
        if marker not in release:
            errors.append(f"release packaging is missing native-init integration: {marker}")

    apk_override = (
        release.split("            apk:\n", 1)[1].split("            archlinux:\n", 1)[0]
        if "            apk:\n" in release and "            archlinux:\n" in release
        else ""
    )
    if "preremove:" in apk_override:
        errors.append("nFPM APK overrides do not support an APK-specific preremove key; use the generic preremove hook")
    if "postupgrade: packaging/apk-post-upgrade" not in apk_override:
        errors.append("APK post-upgrade hook must use the APK-specific postupgrade field")

    if "init-neutral" in release:
        errors.append("native APK/XBPS packages must not be described as init-neutral")


def check_portable_privilege_helpers(errors: list[str]) -> None:
    cli = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    if "run_privileged_cli_action(" not in cli or "trusted_privilege_command()" not in cli:
        errors.append("CLI uninstall/reinstall must use the trusted platform privilege helper directly")
    if "run_embedded_privileged_helper(" in cli:
        errors.append("CLI must not execute user-owned temporary scripts with elevated privileges")
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
    if "pkexec_path()" not in backend or "gitrun_platform::trusted_privilege_escalation_binary" not in backend:
        errors.append(
            "Dashboard service controls must retain a trusted pkexec/sudo/doas privilege fallback"
        )
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
        "trusted_service_manager_binary(manager)",
        "gitrun_platform::PackageManager::Apt",
        "gitrun_platform::PackageManager::Dnf",
        "gitrun_platform::PackageManager::Zypper",
        "gitrun_platform::PackageManager::Pacman",
        "gitrun_platform::PackageManager::Apk",
        "gitrun_platform::PackageManager::Xbps",
        "gitrun_platform::PackageManager::Emerge",
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
    if '"-Syu"' in setup:
        errors.append("Docker bootstrap must not trigger a full Arch system upgrade")

    install_services = setup.split("fn install_gitrun_service_definitions", 1)[1].split("fn start_gitrun_services", 1)[0]
    start_services = setup.split("fn start_gitrun_services", 1)[1].split("fn stop_gitrun_services", 1)[0]
    if "ensure_runit_service_link(service)" in install_services:
        errors.append("runit service links must not be enabled while setup is still writing configuration")
    if "ensure_runit_service_link(service)" not in start_services:
        errors.append("runit service links must be created only in the service-start phase")


def check_updater_native_package_contract(errors: list[str]) -> None:
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "current_linux_package_manager()",
        "linux_dependency_packages(",
        "run_linux_package_command(",
        "P::Apt",
        "P::Dnf",
        "P::Zypper",
        "P::Pacman",
        "P::Apk",
        "P::Xbps",
        "P::Emerge",
        '(P::Dnf, "Docker") => &["moby-engine", "docker-compose", "docker-buildx"][..]',
    )
    missing = [item for item in required if item not in updater]
    if missing:
        errors.append(
            "Dependency updates must use native Linux package managers; missing: "
            + ", ".join(missing)
        )
    if '"-Syu"' in updater:
        errors.append("Dependency updates must not trigger a full Arch system upgrade")


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
    check_native_init_package_resources(errors)
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
        "This guard prevents their spread; it does not certify every package lifecycle, updater, recovery or runner workload across all targets.",
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
 <<< \\"$apk_contents\\"",
        "grep -Eq '(^|/)\\\\.post-upgrade        'install -m 0755 packaging/runit/gitrun/run "$xbps_stage/etc/sv/gitrun/run"',
        'install -m 0755 packaging/runit/gitrun/log/run "$xbps_stage/etc/sv/gitrun/log/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/run "$xbps_stage/etc/sv/gitrun-gsr/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/log/run "$xbps_stage/etc/sv/gitrun-gsr/log/run"',
        'xbps_contents="$(tar -tf "$xbps_file")"',
    )
    for marker in workflow_markers:
        if marker not in release:
            errors.append(f"release packaging is missing native-init integration: {marker}")

    if "init-neutral" in release:
        errors.append("native APK/XBPS packages must not be described as init-neutral")


def check_portable_privilege_helpers(errors: list[str]) -> None:
    cli = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    if "run_privileged_cli_action(" not in cli or "trusted_privilege_command()" not in cli:
        errors.append("CLI uninstall/reinstall must use the trusted platform privilege helper directly")
    if "run_embedded_privileged_helper(" in cli:
        errors.append("CLI must not execute user-owned temporary scripts with elevated privileges")
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
    if "pkexec_path()" not in backend or "gitrun_platform::trusted_privilege_escalation_binary" not in backend:
        errors.append(
            "Dashboard service controls must retain a trusted pkexec/sudo/doas privilege fallback"
        )
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
        "trusted_service_manager_binary(manager)",
        "gitrun_platform::PackageManager::Apt",
        "gitrun_platform::PackageManager::Dnf",
        "gitrun_platform::PackageManager::Zypper",
        "gitrun_platform::PackageManager::Pacman",
        "gitrun_platform::PackageManager::Apk",
        "gitrun_platform::PackageManager::Xbps",
        "gitrun_platform::PackageManager::Emerge",
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
    if '"-Syu"' in setup:
        errors.append("Docker bootstrap must not trigger a full Arch system upgrade")


def check_updater_native_package_contract(errors: list[str]) -> None:
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "current_linux_package_manager()",
        "linux_dependency_packages(",
        "run_linux_package_command(",
        "P::Apt",
        "P::Dnf",
        "P::Zypper",
        "P::Pacman",
        "P::Apk",
        "P::Xbps",
        "P::Emerge",
        '(P::Dnf, "Docker") => &["moby-engine", "docker-compose", "docker-buildx"][..]',
    )
    missing = [item for item in required if item not in updater]
    if missing:
        errors.append(
            "Dependency updates must use native Linux package managers; missing: "
            + ", ".join(missing)
        )
    if '"-Syu"' in updater:
        errors.append("Dependency updates must not trigger a full Arch system upgrade")


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
    check_native_init_package_resources(errors)
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
 <<< \\"$apk_contents\\"",
        "if grep -Eq '^\\\\./(etc|usr/lib)/systemd/' <<< \\"$xbps_contents\\"; then",
        'install -m 0755 packaging/runit/gitrun/run "$xbps_stage/etc/sv/gitrun/run"',
        'install -m 0755 packaging/runit/gitrun/log/run "$xbps_stage/etc/sv/gitrun/log/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/run "$xbps_stage/etc/sv/gitrun-gsr/run"',
        'install -m 0755 packaging/runit/gitrun-gsr/log/run "$xbps_stage/etc/sv/gitrun-gsr/log/run"',
        'xbps_contents="$(tar -tf "$xbps_file")"',
    )
    for marker in workflow_markers:
        if marker not in release:
            errors.append(f"release packaging is missing native-init integration: {marker}")

    if "init-neutral" in release:
        errors.append("native APK/XBPS packages must not be described as init-neutral")


def check_portable_privilege_helpers(errors: list[str]) -> None:
    cli = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    if "run_privileged_cli_action(" not in cli or "trusted_privilege_command()" not in cli:
        errors.append("CLI uninstall/reinstall must use the trusted platform privilege helper directly")
    if "run_embedded_privileged_helper(" in cli:
        errors.append("CLI must not execute user-owned temporary scripts with elevated privileges")
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
    if "pkexec_path()" not in backend or "gitrun_platform::trusted_privilege_escalation_binary" not in backend:
        errors.append(
            "Dashboard service controls must retain a trusted pkexec/sudo/doas privilege fallback"
        )
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
        "trusted_service_manager_binary(manager)",
        "gitrun_platform::PackageManager::Apt",
        "gitrun_platform::PackageManager::Dnf",
        "gitrun_platform::PackageManager::Zypper",
        "gitrun_platform::PackageManager::Pacman",
        "gitrun_platform::PackageManager::Apk",
        "gitrun_platform::PackageManager::Xbps",
        "gitrun_platform::PackageManager::Emerge",
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
    if '"-Syu"' in setup:
        errors.append("Docker bootstrap must not trigger a full Arch system upgrade")


def check_updater_native_package_contract(errors: list[str]) -> None:
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    required = (
        "current_linux_package_manager()",
        "linux_dependency_packages(",
        "run_linux_package_command(",
        "P::Apt",
        "P::Dnf",
        "P::Zypper",
        "P::Pacman",
        "P::Apk",
        "P::Xbps",
        "P::Emerge",
        '(P::Dnf, "Docker") => &["moby-engine", "docker-compose", "docker-buildx"][..]',
    )
    missing = [item for item in required if item not in updater]
    if missing:
        errors.append(
            "Dependency updates must use native Linux package managers; missing: "
            + ", ".join(missing)
        )
    if '"-Syu"' in updater:
        errors.append("Dependency updates must not trigger a full Arch system upgrade")


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
    check_native_init_package_resources(errors)
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
