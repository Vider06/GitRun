#!/usr/bin/env python3
from pathlib import Path
import ast
import re
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
    "config/config.example.env",
    "docker-compose.yml",
    "docker/runner/Dockerfile",
    "docker/runner/entrypoint.sh",
    "systemd/gitrun.service",
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
    "crates/gitrun-dashboard-tauri/package.json",
    "crates/gitrun-dashboard-tauri/dist/index.html",
    "crates/gitrun-dashboard-tauri/dist/js/app.js",
    "crates/gitrun-dashboard-tauri/src-tauri/Cargo.toml",
    "crates/gitrun-dashboard-tauri/src-tauri/build.rs",
    "crates/gitrun-dashboard-tauri/src-tauri/src/main.rs",
    "crates/gitrun-dashboard-tauri/src-tauri/src/lib.rs",
    "crates/gitrun-dashboard-tauri/src-tauri/tauri.conf.json",
    "crates/gitrun-setup/src/resources.rs",
]:
    require(path)

scheduler_lib = (ROOT / "crates/gitrun-scheduler/src/lib.rs").read_text(encoding="utf-8")
for required in [
    "pub mod reconcile",
    "pub mod github",
    "pub mod gtuu",
    "pub fn run()",
    "pub fn run_gtuu_once()",
]:
    if required not in scheduler_lib:
        errors.append(f"Rust scheduler feature missing: {required}")

cli_source = (ROOT / "crates/gitrun-cli/src/main.rs").read_text(encoding="utf-8")
for required in ["gitrun_scheduler::run()", "gitrun_scheduler::run_gtuu_once()", "only_containers", 'name = "scheduler"']:
    if required not in cli_source:
        errors.append(f"Rust CLI scheduler integration missing: {required}")

config_path = ROOT / "config/config.example.env"
config_source = config_path.read_text(encoding="utf-8")
for key, expected in [
    ("GITRUN_MIN_RUNNERS", "3"),
    ("GITRUN_MAX_RUNNERS", "8"),
    ("GITRUN_IDLE_TIMEOUT", "120"),
    ("GITRUN_POLL_INTERVAL", "5"),
    ("GITRUN_CONTAINER_CPUS", "1"),
    ("GITRUN_CONTAINER_MEMORY", "1g"),
    ("GITRUN_CONTAINER_PIDS", "1024"),
    ("GITRUN_AUTO_CONTAINER_UPDATE", "false"),
    ("GITRUN_CONTAINER_UPDATE_TIME", "03:00"),
    ("GITRUN_AUTO_CONTAINER_RECOVERY", "true"),
    ("GITRUN_CONTAINER_RECOVERY_COOLDOWN", "60"),
    ("GITRUN_SHARED_CACHE_VOLUME", "gitrun-runner-shared"),
    ("GITRUN_RUNNER_LABELS", "self-hosted,Linux,X64,gitrun-ci"),
    ("GITRUN_RUNNER_HOME_SIZE", "8g"),
    ("GITRUN_RUNNER_HOME_BACKEND", "tmpfs"),
    ("GITRUN_GITHUB_CONNECT_TIMEOUT", "5"),
    ("GITRUN_GITHUB_REQUEST_TIMEOUT", "20"),
    ("GITRUN_GTUU_SCHEDULE_TIMEZONE", "utc"),
    ("GITRUN_GSR_DOCKER_SOCKET_HARDENING", "true"),
    ("GITRUN_GSR_ALLOW_UNSAFE_RUNNER", "false"),
    ("GITRUN_GSR_COMMAND_POLICY_ENABLED", "true"),
    ("GITRUN_GSR_COMMAND_BASELINE_BLACKLIST_ENABLED", "true"),
    ("GITRUN_GSR_COMMAND_BLACKLIST_ENABLED", "false"),
    ("GITRUN_GSR_COMMAND_WHITELIST_ENABLED", "false"),
    ("GITRUN_GSR_VIOLATION_ACTION", "kill"),
    ("GITRUN_GSR_WORKFLOW_VALIDATION_ENABLED", "true"),
    ("GITRUN_GSR_ZIZMOR_ENABLED", "false"),
    ("GITRUN_GSR_ZIZMOR_LICENSE_ACCEPTED", "false"),
]:
    if f"{key}={expected}" not in config_source:
        errors.append(f"config default mismatch: {key}={expected}")

config_rs = (ROOT / "crates/gitrun-core/src/config.rs").read_text(encoding="utf-8")
supported = set(re.findall(r'get\("([A-Z][A-Z0-9_]+)"\)', config_rs))
example_keys = set(
    match.group(1)
    for match in re.finditer(r"^(?:#\s*)?([A-Z][A-Z0-9_]+)=", config_source, re.MULTILINE)
    if match.group(1).startswith("GITRUN_")
)
missing_in_example = sorted(supported - example_keys)
if missing_in_example:
    errors.append(
        "config template missing supported variables: " + ", ".join(missing_in_example)
    )
unsupported_example = sorted(example_keys - supported)
if unsupported_example:
    errors.append(
        "config template contains unsupported parser variables: " + ", ".join(unsupported_example)
    )
if "GITRUN_LOG_LEVEL=" in config_source:
    errors.append("config template exposes unsupported legacy GITRUN_LOG_LEVEL")

release_workflow = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")
for required in [
    "runs-on: [self-hosted, Linux]",
    "timeout-minutes: 60",
    "gh release create",
    "gh release upload",
    "cargo build --locked --release -p gitrun-cli --bin gitrun",
    "test -x target/release/gitrun",
    "npm run tauri build -- --ci --no-bundle",
    "npm run tauri bundle -- --bundles deb --no-binary-patching",
    "package-manager-cache: false",
    "x86_64-unknown-linux-gnu",
    "release-manifest.json",
    "dpkg-deb -f",
]:
    if required not in release_workflow:
        errors.append(f"release workflow requirement missing: {required}")

if "autoscaler/" in release_workflow or "gitrun_updater_utility.py" in release_workflow:
    errors.append("release workflow still references retired Python GTUU")

if "runs-on: [self-hosted, Linux, X64]" in release_workflow:
    errors.append("release workflow still requires the legacy X64 label")

if "ubuntu-latest" in release_workflow or "windows-latest" in release_workflow or "macos-" in release_workflow:
    errors.append("release workflow still references GitHub-hosted OS runners")

if "DOCKER_CONFIG: /tmp/gitrun-docker-config" in release_workflow:
    errors.append("release workflow retains obsolete Docker credential isolation")

if "target/release/bundle/deb" not in release_workflow:
    errors.append("release workflow Tauri Debian output path missing")

if "linux-x86_64-deb" not in release_workflow:
    errors.append("release workflow Debian manifest target missing")


for retired in ["autoscaler/gitrun_manager.py", "autoscaler/gitrun_updater_utility.py", "docker/manager/Dockerfile", "bin/gitrun"]:
    if (ROOT / retired).exists():
        errors.append(f"retired file still present: {retired}")

compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
if "legacy-python" in compose or "docker/manager/Dockerfile" in compose:
    errors.append("compose still references the retired Python manager")

systemd = (ROOT / "systemd/gitrun.service").read_text(encoding="utf-8")
if "docker compose" in systemd or "--profile python" in systemd:
    errors.append("systemd still launches the legacy manager")

cli_manifest = (ROOT / "crates/gitrun-cli/Cargo.toml").read_text(encoding="utf-8")
if 'name = "gitrun"' not in cli_manifest:
    errors.append("Rust CLI binary target gitrun missing")
if 'gitrun-dashboard' in cli_manifest or 'gitrun_dashboard' in cli_manifest:
    errors.append("Rust CLI still depends on the retired egui dashboard")

tauri_backend = (ROOT / "crates/gitrun-dashboard-tauri/src-tauri/src/lib.rs").read_text(encoding="utf-8")
tauri_frontend = (ROOT / "crates/gitrun-dashboard-tauri/dist/js/app.js").read_text(encoding="utf-8")
if "run_first_setup" not in tauri_backend or "is_first_run" not in tauri_backend:
    errors.append("Tauri first-run setup bridge missing")
if 'invoke("run_first_setup"' not in tauri_frontend:
    errors.append("Tauri first-run setup UI missing")

for legacy in ["gitrun-dashboard", "gitrun_dashboard", "eframe", "egui"]:
    for path in [
        "Cargo.toml",
        "crates/gitrun-cli/Cargo.toml",
        "crates/gitrun-cli/src/main.rs",
        "scripts/build-release.sh",
        "scripts/build-release.ps1",
        "scripts/build-deb.sh",
        "packaging/gitrun.desktop",
    ]:
        source = (ROOT / path).read_text(encoding="utf-8")
        normalized_source = source.replace("gitrun-dashboard-tauri", "")
        if legacy in normalized_source:
            errors.append(f"legacy dashboard reference in {path}: {legacy}")

runner_image = (ROOT / "docker/runner/Dockerfile").read_text(encoding="utf-8")
for required in ["docker-ce-cli", "docker-compose-plugin", "powershell", "gh", "packages.microsoft.com/config/debian/12", "https://sh.rustup.rs"]:
    if required not in runner_image:
        errors.append(f"runner image CI dependency missing: {required}")

compose = (ROOT / "docker-compose.yml").read_text(encoding="utf-8")
for required in ["GITRUN_CONFIG_FILE", "GITRUN_DOCKER_SOCKET", "GITRUN_STATE_DIR", "GITRUN_LOG_DIR"]:
    if required not in compose:
        errors.append(f"compose portability setting missing: {required}")

for script in [
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
