#!/usr/bin/env python3
from pathlib import Path
import ast
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
errors = []

def require(path: str):
    if not (ROOT / path).is_file():
        errors.append(f"missing: {path}")

def check_python(path: str):
    try:
        ast.parse((ROOT / path).read_text(encoding="utf-8"))
    except Exception as exc:
        errors.append(f"python syntax {path}: {exc}")

for path in [
    "Cargo.toml",
    "bin/gitrun",
    "config/config.example.env",
    "docker-compose.yml",
    "docker/runner/Dockerfile",
    "docker/runner/entrypoint.sh",
    "docker/manager/Dockerfile",
    "systemd/gitrun.service",
    "autoscaler/gitrun_manager.py",
    "scripts/install-server.sh",
    "scripts/install-linux.sh",
    "scripts/install-macos.sh",
    "scripts/install-windows.ps1",
    "scripts/build-release.sh",
    "scripts/build-release.ps1",
    "scripts/verify-release.sh",
    "SECURITY.md",
    "docs/ROADMAP_PHASES.md",
    "docs/RUST_MIGRATION.md",
    "crates/gitrun-core/Cargo.toml",
    "crates/gitrun-core/src/lib.rs",
    "crates/gitrun-core/src/config.rs",
    "crates/gitrun-core/src/runner.rs",
    "crates/gitrun-core/src/state.rs",
    "crates/gitrun-cli/Cargo.toml",
    "crates/gitrun-cli/src/main.rs",
    "crates/gitrun-updater/Cargo.toml",
    "crates/gitrun-updater/src/lib.rs",
    "crates/gitrun-recovery/Cargo.toml",
    "crates/gitrun-recovery/src/lib.rs",
    "crates/gitrun-dashboard/Cargo.toml",
    "crates/gitrun-dashboard/src/main.rs",
]:
    require(path)

check_python("autoscaler/gitrun_manager.py")

cli = (ROOT / "bin/gitrun").read_text(encoding="utf-8")
for command in [
    "overview", "status", "runners", "health", "doctor", "logs",
    "last-crash", "usage", "connect", "repositories", "service",
    "start", "stop", "restart",
]:
    if f"{command})" not in cli and f"{command}|repos)" not in cli:
        errors.append(f"CLI command missing: {command}")

manager = (ROOT / "autoscaler/gitrun_manager.py").read_text(encoding="utf-8")
for required in [
    "GITRUN_MIN_RUNNERS", "GITRUN_MAX_RUNNERS", "GITRUN_IDLE_TIMEOUT",
    "GITRUN_REPOSITORIES", "registration-token", "status=queued", "self-hosted",
]:
    if required not in manager:
        errors.append(f"autoscaler feature missing: {required}")

config = (ROOT / "config/config.example.env").read_text(encoding="utf-8")
for key, expected in [
    ("GITRUN_MIN_RUNNERS", "3"),
    ("GITRUN_MAX_RUNNERS", "8"),
    ("GITRUN_IDLE_TIMEOUT", "120"),
    ("GITRUN_CONTAINER_CPUS", "1"),
    ("GITRUN_CONTAINER_MEMORY", "1g"),
]:
    if f"{key}={expected}" not in config:
        errors.append(f"config default mismatch: {key}={expected}")

compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
for required in ["GITRUN_CONFIG_FILE", "GITRUN_DOCKER_SOCKET", "GITRUN_STATE_DIR", "GITRUN_LOG_DIR"]:
    if required not in compose:
        errors.append(f"compose portability setting missing: {required}")

if subprocess.run(["bash", "-n", str(ROOT / "bin/gitrun")], capture_output=True, text=True).returncode != 0:
    errors.append("shell syntax: bin/gitrun")

for script in [
    "scripts/install-linux.sh", "scripts/install-macos.sh",
    "scripts/install-server.sh", "scripts/build-release.sh", "scripts/verify-release.sh",
]:
    if subprocess.run(["bash", "-n", str(ROOT / script)], capture_output=True, text=True).returncode != 0:
        errors.append(f"shell syntax: {script}")

if errors:
    print("GitRun static check: FAIL")
    for error in errors:
        print(f" - {error}")
    sys.exit(1)

print("GitRun static check: PASS")
