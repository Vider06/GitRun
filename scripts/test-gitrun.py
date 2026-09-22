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
    "autoscaler/gitrun_updater_utility.py",
    "scripts/install-server.sh",
    "scripts/install-linux.sh",
    "scripts/install-macos.sh",
    "scripts/install-windows.ps1",
    "scripts/build-release.sh",
    "scripts/build-release.ps1",
    "scripts/verify-release.sh",
    "scripts/build-deb.sh",
    ".github/workflows/release.yml",
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
    "crates/gitrun-setup/Cargo.toml",
    "crates/gitrun-setup/src/lib.rs",
    "crates/gitrun-updater/Cargo.toml",
    "crates/gitrun-updater/src/lib.rs",
    "crates/gitrun-recovery/Cargo.toml",
    "crates/gitrun-recovery/src/lib.rs",
    "crates/gitrun-dashboard/Cargo.toml",
    "crates/gitrun-dashboard/src/main.rs",
    "crates/gitrun-dashboard/src/lib.rs",
    "crates/gitrun-setup/src/resources.rs",
]:
    require(path)

check_python("autoscaler/gitrun_manager.py")
check_python("autoscaler/gitrun_updater_utility.py")

cli = (ROOT / "bin/gitrun").read_text(encoding="utf-8")
for command in [
    "overview", "status", "runners", "health", "doctor", "logs",
    "last-crash", "usage", "connect", "repositories", "service",
    "start", "stop", "restart", "update",
]:
    if f"{command})" not in cli and f"{command}|repos)" not in cli:
        errors.append(f"CLI command missing: {command}")

manager = (ROOT / "autoscaler/gitrun_manager.py").read_text(encoding="utf-8")
for required in [
    "GITRUN_MIN_RUNNERS", "GITRUN_MAX_RUNNERS", "GITRUN_IDLE_TIMEOUT",
    "GITRUN_REPOSITORIES", "registration-token", "status=queued", "self-hosted", "gitrun-ci",
    "docker", "def docker(*args", "GITRUN_AUTO_CONTAINER_UPDATE", "GITRUN_CONTAINER_UPDATE_TIME",
    "GITRUN_AUTO_CONTAINER_RECOVERY", "GITRUN_CONTAINER_RECOVERY_COOLDOWN", "restart_container", "container_recovery",
    "GITRUN_SHARED_CACHE_VOLUME", "shared_cache_volume", "ensure_shared_cache_volume", "CARGO_HOME", "CARGO_TARGET_DIR",
    "gtuu_schedule_loop",
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
    ("GITRUN_AUTO_CONTAINER_UPDATE", "false"),
    ("GITRUN_CONTAINER_UPDATE_TIME", "03:00"),
    ("GITRUN_AUTO_CONTAINER_RECOVERY", "true"),
    ("GITRUN_CONTAINER_RECOVERY_COOLDOWN", "60"),
    ("GITRUN_SHARED_CACHE_VOLUME", "gitrun-runner-shared"),
    ("GITRUN_RUNNER_LABELS", "self-hosted,Linux,X64,gitrun-ci"),
]:
    if f"{key}={expected}" not in config:
        errors.append(f"config default mismatch: {key}={expected}")

release_workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
for required in [
    "runs-on: [self-hosted, Linux, X64, gitrun-ci]",
    "gh release create",
    "cargo build --locked --release -p gitrun-cli --bin gitrun",
    "test -x target/release/gitrun",
    "./scripts/build-deb.sh",
    "dpkg-deb --info",
    "gh release upload",
    "gitrun.deb",
]:
    if required not in release_workflow:
        errors.append(f"release workflow requirement missing: {required}")

if "gh release upload \"$VERSION\" target/release/gitrun" in release_workflow:
    errors.append("release workflow still uploads the raw binary directly")

if "ubuntu-latest" in release_workflow or "windows-latest" in release_workflow or "macos-" in release_workflow:
    errors.append("release workflow still references GitHub-hosted OS runners")

if "x86_64-unknown-linux-gnu" not in release_workflow:
    errors.append("release workflow Linux x86_64 target missing")

if "DOCKER_CONFIG: /tmp/gitrun-docker-config" not in release_workflow:
    errors.append("release workflow Docker credential isolation missing")

cli_manifest = (ROOT / "crates/gitrun-cli/Cargo.toml").read_text(encoding="utf-8")
if 'name = "gitrun"' not in cli_manifest:
    errors.append("Rust CLI binary target gitrun missing")

runner_image = (ROOT / "docker/runner/Dockerfile").read_text(encoding="utf-8")
for required in ["docker-ce-cli", "docker-compose-plugin", "powershell", "gh", "packages.microsoft.com/config/debian/12", "https://sh.rustup.rs"]:
    if required not in runner_image:
        errors.append(f"runner image CI dependency missing: {required}")

compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
for required in ["GITRUN_CONFIG_FILE", "GITRUN_DOCKER_SOCKET", "GITRUN_STATE_DIR", "GITRUN_LOG_DIR"]:
    if required not in compose:
        errors.append(f"compose portability setting missing: {required}")

for script in [
    "bin/gitrun",
    "scripts/install-linux.sh", "scripts/install-macos.sh",
    "scripts/install-server.sh", "scripts/build-release.sh",
    "scripts/verify-release.sh", "scripts/build-deb.sh",
]:
    if subprocess.run(["bash", "-n", str(ROOT / script)], capture_output=True, text=True).returncode != 0:
        errors.append(f"shell syntax: {script}")

if errors:
    print("GitRun static check: FAIL")
    for error in errors:
        print(f" - {error}")
    sys.exit(1)

print("GitRun static check: PASS")
