#!/usr/bin/env python3
from pathlib import Path
import ast
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
    "bin/gitrun",
    "config/config.example.env",
    "docker-compose.yml",
    "docker/runner/Dockerfile",
    "docker/runner/entrypoint.sh",
    "docker/manager/Dockerfile",
    "systemd/gitrun.service",
    "autoscaler/gitrun_manager.py",
    "scripts/install-server.sh",
]:
    require(path)

check_python("autoscaler/gitrun_manager.py")

cli = (ROOT / "bin/gitrun").read_text(encoding="utf-8")
for command in ["overview", "status", "runners", "health", "doctor",
                "logs", "last-crash", "usage", "connect", "repositories",
                "service", "start", "stop", "restart"]:
    if f"{command})" not in cli and f"{command}|repos)" not in cli:
        errors.append(f"CLI command missing: {command}")

manager = (ROOT / "autoscaler/gitrun_manager.py").read_text(encoding="utf-8")
for required in ["GITRUN_MIN_RUNNERS", "GITRUN_MAX_RUNNERS",
                 "GITRUN_IDLE_TIMEOUT", "GITRUN_REPOSITORIES",
                 "registration-token", "status=queued", "self-hosted"]:
    if required not in manager:
        errors.append(f"autoscaler feature missing: {required}")

if errors:
    print("GitRun static check: FAIL")
    for error in errors:
        print(f" - {error}")
    sys.exit(1)

print("GitRun static check: PASS")
