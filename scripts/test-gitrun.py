#!/usr/bin/env python3
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
errors = []

def require(path: str):
    if not (ROOT / path).is_file():
        errors.append(f"missing: {path}")

def read_text(path: str) -> str:
    try:
        return (ROOT / path).read_text(encoding="utf-8")
    except OSError as exc:
        errors.append(f"read failed {path}: {exc}")
        return ""

def fail_if_errors() -> None:
    if errors:
        print("GitRun static check: FAIL")
        for error in errors:
            print(f" - {error}")
        sys.exit(1)

def check_shell_syntax(path: str) -> None:
    result = subprocess.run(
        ["bash", "-n", str(ROOT / path)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit code {result.returncode}"
        errors.append(f"shell syntax {path}: {detail}")

for path in [
    "Cargo.toml",
    "config/config.example.env",
    "docker-compose.yml",
    "docker/runner/Dockerfile",
    "docker/runner/entrypoint.sh",
    "systemd/gitrun.service",
    "scripts/install-server.sh",
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
    "crates/gitrun-dashboard-tauri/src-tauri/permissions/dashboard.toml",
    "crates/gitrun-setup/src/resources.rs",
]:
    require(path)

fail_if_errors()

scheduler_lib = read_text("crates/gitrun-scheduler/src/lib.rs")
for required in [
    "pub mod reconcile",
    "pub mod github",
    "pub mod gtuu",
    "pub fn run()",
    "pub fn run_gtuu_once()",
]:
    if required not in scheduler_lib:
        errors.append(f"Rust scheduler feature missing: {required}")

cli_source = read_text("crates/gitrun-cli/src/main.rs")
for required in ["gitrun_scheduler::run()", "gitrun_scheduler::run_gtuu_once()", "only_containers", 'name = "scheduler"']:
    if required not in cli_source:
        errors.append(f"Rust CLI scheduler integration missing: {required}")

config_source = read_text("config/config.example.env")
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
    ("GITRUN_RUNNER_LABELS", "self-hosted,Linux"),
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

release_workflow = read_text(".github/workflows/release.yml")

def validate_release_jobs(workflow: str) -> None:
    jobs_match = re.search(r"^jobs:\s*$", workflow, re.MULTILINE)
    if not jobs_match:
        errors.append("release workflow has no top-level jobs section")
        return

    jobs_block = workflow[jobs_match.end():]
    job_matches = list(re.finditer(r"^  ([A-Za-z0-9_-]+):\s*$", jobs_block, re.MULTILINE))
    if not job_matches:
        errors.append("release workflow contains no jobs")
        return

    for index, match in enumerate(job_matches):
        job_name = match.group(1)
        block_end = job_matches[index + 1].start() if index + 1 < len(job_matches) else len(jobs_block)
        job_block = jobs_block[match.end():block_end]

        runs_on = re.search(r"^    runs-on:\s*(.+?)\s*$", job_block, re.MULTILINE)
        timeout = re.search(r"^    timeout-minutes:\s*(\d+)\s*$", job_block, re.MULTILINE)

        if not runs_on:
            errors.append(f"release job {job_name} is missing runs-on")
        elif runs_on.group(1).strip() not in ("[self-hosted, Linux]", "['self-hosted', 'Linux']", "[\"self-hosted\", \"Linux\"]"):
            errors.append(
                f"release job {job_name} must use exactly [self-hosted, Linux], got: {runs_on.group(1).strip()}"
            )

        if not timeout:
            errors.append(f"release job {job_name} is missing timeout-minutes")
        elif timeout.group(1) != "60":
            errors.append(
                f"release job {job_name} must have timeout-minutes: 60, got: {timeout.group(1)}"
            )

validate_release_jobs(release_workflow)

for required in [
    "runs-on: [self-hosted, Linux]",
    "timeout-minutes: 60",
    "gh release create",
    "gh release upload",
    "cargo build --locked --release -p gitrun-cli --bin gitrun",
    "test -x target/release/gitrun",
    "Set Tauri release version",
    "Build unified GitRun executable",
    "x86_64-unknown-linux-gnu",
    "release-manifest.json",
    "dpkg-deb -f",
    "dpkg-deb --build --root-owner-group",
]:
    if required not in release_workflow:
        errors.append(f"release workflow requirement missing: {required}")

if "autoscaler/" in release_workflow or "gitrun_updater_utility.py" in release_workflow:
    errors.append("release workflow still references retired Python GTUU")

if re.search(r"^\s*runs-on:\s*\[.*(?:X64|ubuntu-latest|windows-latest|macos-).*$", release_workflow, re.MULTILINE):
    errors.append("release workflow contains a non-base Linux runner label")

if "ubuntu-latest" in release_workflow or "windows-latest" in release_workflow or "macos-" in release_workflow:
    errors.append("release workflow still references GitHub-hosted OS runners")

if "DOCKER_CONFIG: /tmp/gitrun-docker-config" in release_workflow:
    errors.append("release workflow retains obsolete Docker credential isolation")

if "dpkg-deb --build --root-owner-group" not in release_workflow:
    errors.append("release workflow Debian package build missing")

if "linux-x86_64-deb" not in release_workflow:
    errors.append("release workflow Debian manifest target missing")

if "target/release/gitrun-recovery" in release_workflow or "gitrun-recovery start" in release_workflow:
    errors.append("release workflow must publish only the unified gitrun executable")

if "target/release/gitrun-dashboard-tauri" in release_workflow or "target/release/bundle/deb" in release_workflow:
    errors.append("release workflow must not publish the standalone Tauri dashboard executable")


build_deb = read_text("scripts/build-deb.sh")
if "gitrun-recovery" in build_deb or "gitrun-dashboard-tauri" in build_deb:
    errors.append("build-deb.sh must package only the unified gitrun executable")
if "RECOVERY_BINARY" in build_deb or "DASHBOARD_BINARY" in build_deb:
    errors.append("build-deb.sh retains a retired secondary executable variable")

build_release_sh = read_text("scripts/build-release.sh")
build_release_ps1 = read_text("scripts/build-release.ps1")
for release_script in [build_release_sh, build_release_ps1]:
    if "gitrun-recovery" in release_script or "gitrun-dashboard-tauri" in release_script:
        errors.append("local release scripts must package only the unified gitrun executable")

desktop = read_text("packaging/gitrun.desktop")
if "Exec=gitrun" not in desktop:
    errors.append("desktop launcher must invoke the unified gitrun executable")

for retired in ["autoscaler/gitrun_manager.py", "autoscaler/gitrun_updater_utility.py", "docker/manager/Dockerfile", "bin/gitrun"]:
    if (ROOT / retired).exists():
        errors.append(f"retired file still present: {retired}")

compose = read_text("docker-compose.yml")
if "legacy-python" in compose or "docker/manager/Dockerfile" in compose:
    errors.append("compose still references the retired Python manager")

systemd = read_text("systemd/gitrun.service")
if "docker compose" in systemd or "--profile python" in systemd:
    errors.append("systemd still launches the legacy manager")
if "ExecStart=/usr/local/bin/gitrun scheduler" not in systemd:
    errors.append("systemd must launch the unified gitrun scheduler")

cli_manifest = read_text("crates/gitrun-cli/Cargo.toml")
if 'name = "gitrun"' not in cli_manifest:
    errors.append("Rust CLI binary target gitrun missing")
if 'gitrun-dashboard =' in cli_manifest or 'gitrun_dashboard =' in cli_manifest:
    errors.append("Rust CLI still depends on the retired egui dashboard")
if 'gitrun-recovery = { path = "../gitrun-recovery" }' not in cli_manifest:
    errors.append("Rust CLI must embed the recovery library")
if 'gitrun-dashboard-tauri = { path = "../gitrun-dashboard-tauri/src-tauri" }' not in cli_manifest:
    errors.append("Rust CLI must embed the Tauri dashboard library")

tauri_backend = read_text("crates/gitrun-dashboard-tauri/src-tauri/src/lib.rs")
tauri_frontend = read_text("crates/gitrun-dashboard-tauri/dist/js/app.js")
if "run_first_setup" not in tauri_backend or "is_first_run" not in tauri_backend:
    errors.append("Tauri first-run setup bridge missing")
if 'invoke("run_first_setup"' not in tauri_frontend:
    errors.append("Tauri first-run setup UI missing")

tauri_permissions = read_text("crates/gitrun-dashboard-tauri/src-tauri/permissions/dashboard.toml")
for required in ['"is_first_run"', '"run_first_setup"']:
    if required not in tauri_permissions:
        errors.append(f"Tauri permission missing: {required}")

workspace_manifest = read_text("Cargo.toml")
if "crates/gitrun-dashboard" in workspace_manifest.replace("crates/gitrun-dashboard-tauri", ""):
    errors.append("workspace still contains the retired egui dashboard")

lock_source = read_text("Cargo.lock")
for legacy in ['name = "gitrun-dashboard"', 'name = "eframe"', 'name = "egui"', 'name = "wgpu"']:
    if legacy in lock_source:
        errors.append(f"Cargo.lock still contains retired dashboard dependency: {legacy}")

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
        source = read_text(path)
        normalized_source = source.replace("gitrun-dashboard-tauri", "").replace("gitrun_dashboard_tauri", "")
        if legacy in normalized_source:
            errors.append(f"legacy dashboard reference in {path}: {legacy}")

runner_image = read_text("docker/runner/Dockerfile")
runner_entrypoint = read_text("docker/runner/entrypoint.sh")
for required in [
    "docker-ce-cli=",
    "docker-buildx-plugin=",
    "docker-compose-plugin=",
    "POWERSHELL_VERSION=",
    "RUNNER_SHA256=",
    "sha256sum -c -",
    "DOCKER_GPG_FINGERPRINT=",
    "rust:1.98.1-bookworm@sha256:",
]:
    if required not in runner_image:
        errors.append(f"runner image hardening requirement missing: {required}")
if "--supervise-runner" not in runner_entrypoint:
    errors.append("runner entrypoint does not start the GSR supervisor")
for obsolete in ["packages.microsoft.com/config/debian/12", "https://sh.rustup.rs", "RUNNER_ALLOW_RUNASROOT"]:
    if obsolete in runner_image:
        errors.append(f"runner image still contains obsolete dependency bootstrap: {obsolete}")

for required in ["GITRUN_CONFIG_FILE", "GITRUN_DOCKER_SOCKET", "GITRUN_STATE_DIR", "GITRUN_LOG_DIR"]:
    if required not in compose:
        errors.append(f"compose portability setting missing: {required}")

gsr_supervisor = read_text("crates/gitrun-gsr/src/exec_supervisor.rs")
for required in [
    "PTRACE_O_TRACESECCOMP",
    "PTRACE_O_TRACEFORK",
    "PTRACE_O_TRACECLONE",
    "PTRACE_O_EXITKILL",
    "SYS_execve",
    "SYS_execveat",
    "SECCOMP_RET_TRACE",
    "PR_SET_NO_NEW_PRIVS",
    "EXECVEAT_AT_EMPTY_PATH",
    "resolve_execveat_empty_path",
]:
    if required not in gsr_supervisor:
        errors.append(f"GSR kernel supervisor requirement missing: {required}")

docker_source = read_text("crates/gitrun-scheduler/src/docker.rs")
if '"--read-only".into()' in docker_source:
    errors.append("Linux runner containers must not use a read-only root filesystem")
if '"/tmp:rw,nosuid,nodev,noexec' in docker_source:
    errors.append("runner /tmp must remain executable for CI tooling")
if '--cap-add", "SYS_PTRACE' not in docker_source and '"SYS_PTRACE"' not in docker_source:
    errors.append("GSR Docker hardening must retain SYS_PTRACE for the PID-1 supervisor")

for script in [
    "scripts/install-macos.sh",
    "scripts/install-server.sh",
    "scripts/build-release.sh",
    "scripts/verify-release.sh",
    "scripts/build-deb.sh",
]:
    check_shell_syntax(script)

if errors:
    print("GitRun static check: FAIL")
    for error in errors:
        print(f" - {error}")
    sys.exit(1)

print("GitRun static check: PASS")
