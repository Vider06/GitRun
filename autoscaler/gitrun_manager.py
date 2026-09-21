#!/usr/bin/env python3
from __future__ import annotations

import json
import logging
import os
import re
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

API = "https://api.github.com"
API_VERSION = "2026-03-10"

logging.basicConfig(
    level=os.getenv("GITRUN_LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s %(message)s",
)
log = logging.getLogger("gitrun")


@dataclass(frozen=True)
class RepoConfig:
    full_name: str
    minimum: int
    maximum: int


def env_int(name: str, default: int) -> int:
    try:
        return int(os.getenv(name, str(default)))
    except ValueError:
        return default


def repositories() -> list[RepoConfig]:
    raw = os.getenv("GITRUN_REPOSITORIES", "").strip()
    if not raw:
        default = os.getenv("GITRUN_DEFAULT_REPOSITORY", "").strip()
        raw = default

    result: list[RepoConfig] = []
    default_min = env_int("GITRUN_MIN_RUNNERS", 3)
    default_max = env_int("GITRUN_MAX_RUNNERS", 20)

    for item in raw.split(","):
        repo = item.strip()
        if not repo:
            continue
        if not re.fullmatch(r"[^/]+/[^/]+", repo):
            log.error("Ignoring invalid repository: %s", repo)
            continue
        result.append(RepoConfig(repo, default_min, default_max))

    return result


def token() -> str:
    value = os.getenv("GITHUB_TOKEN", "").strip()
    if not value:
        raise RuntimeError("GITHUB_TOKEN is not configured")
    return value


def api_request(method: str, path: str, body: dict | None = None) -> dict:
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        API + path,
        data=data,
        method=method,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token()}",
            "X-GitHub-Api-Version": API_VERSION,
            "User-Agent": "GitRun/0.1.0",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            payload = response.read().decode("utf-8")
            return json.loads(payload) if payload else {}
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(f"GitHub API {exc.code}: {detail}") from exc


def split_repo(repo: str) -> tuple[str, str]:
    return repo.split("/", 1)


def registration_token(repo: str) -> str:
    owner, name = split_repo(repo)
    payload = api_request(
        "POST", f"/repos/{owner}/{name}/actions/runners/registration-token"
    )
    return payload["token"]


def list_runners(repo: str) -> list[dict]:
    owner, name = split_repo(repo)
    return api_request(
        "GET", f"/repos/{owner}/{name}/actions/runners?per_page=100"
    ).get("runners", [])


def queued_jobs(repo: str) -> int:
    owner, name = split_repo(repo)
    runs = api_request(
        "GET",
        f"/repos/{owner}/{name}/actions/runs?status=queued&per_page=100",
    ).get("workflow_runs", [])

    count = 0
    for run in runs:
        try:
            jobs = api_request(
                "GET",
                f"/repos/{owner}/{name}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100",
            ).get("jobs", [])
            for job in jobs:
                if job.get("status") != "queued":
                    continue
                labels = {str(label).lower() for label in job.get("labels", [])}
                if "self-hosted" in labels:
                    count += 1
        except Exception:
            log.exception("Unable to inspect queued jobs for %s run %s", repo, run.get("id"))
    return count


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=check,
    )


def managed_containers(repo: str) -> list[str]:
    result = docker(
        "ps", "-a",
        "--filter", "label=gitrun.runner=true",
        "--filter", f"label=gitrun.repo={repo}",
        "--format", "{{.Names}}",
    )
    return [line for line in result.stdout.splitlines() if line.strip()]


def container_started_at(name: str) -> datetime | None:
    result = docker(
        "inspect", "-f", "{{.State.StartedAt}}",
        name, check=False
    )
    if result.returncode != 0:
        return None
    try:
        return datetime.fromisoformat(result.stdout.strip().replace("Z", "+00:00"))
    except ValueError:
        return None


def container_status(name: str) -> dict:
    result = docker(
        "inspect", "-f",
        "{{json .State}}",
        name, check=False
    )
    if result.returncode != 0:
        return {}
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError:
        return {}


def create_runner(repo: str) -> None:
    registration = registration_token(repo)
    safe_repo = re.sub(r"[^a-zA-Z0-9_.-]", "-", repo)
    name = f"gitrun-{safe_repo}-{uuid4().hex[:8]}"
    image = os.getenv("GITRUN_RUNNER_IMAGE", "gitrun-runner:latest")
    labels = os.getenv("GITRUN_RUNNER_LABELS", "self-hosted,Linux,X64")

    command = [
        "run", "-d",
        "--name", name,
        "--label", "gitrun.runner=true",
        "--label", f"gitrun.repo={repo}",
        "--label", "gitrun.managed=true",
        "--cpus", os.getenv("GITRUN_CONTAINER_CPUS", "2"),
        "--memory", os.getenv("GITRUN_CONTAINER_MEMORY", "4g"),
        "--pids-limit", os.getenv("GITRUN_CONTAINER_PIDS", "2048"),
        "--restart", "unless-stopped",
        "-e", f"RUNNER_URL=https://github.com/{repo}",
        "-e", f"RUNNER_TOKEN={registration}",
        "-e", f"RUNNER_NAME={name}",
        "-e", f"RUNNER_LABELS={labels}",
        "-e", f"RUNNER_EPHEMERAL={os.getenv('GITRUN_EPHEMERAL', 'false')}",
        "-e", f"RUNNER_DISABLE_UPDATE={os.getenv('GITRUN_DISABLE_UPDATE', 'false')}",
        image,
    ]

    result = docker(*command, check=False)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or "docker run failed")
    log.info("Created runner %s for %s", name, repo)


def state_path() -> Path:
    return Path(os.getenv("GITRUN_STATE_DIR", "/var/lib/gitrun")) / "state.json"


def load_state() -> dict:
    path = state_path()
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, json.JSONDecodeError):
        return {"idle_since": {}}


def save_state(state: dict) -> None:
    path = state_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(state, indent=2), encoding="utf-8")


def remove_runner(name: str) -> None:
    result = docker("rm", "-f", name, check=False)
    if result.returncode == 0:
        log.info("Removed runner %s", name)
    else:
        log.warning("Could not remove runner %s: %s", name, result.stderr.strip())


def reconcile(repo_cfg: RepoConfig) -> None:
    repo = repo_cfg.full_name
    containers = managed_containers(repo)
    runners = list_runners(repo)
    online = [r for r in runners if r.get("status") == "online"]
    busy = [r for r in online if r.get("busy")]
    queued = queued_jobs(repo)

    desired = max(repo_cfg.minimum, len(busy) + queued)
    desired = min(desired, repo_cfg.maximum)

    log.info(
        "%s: containers=%d online=%d busy=%d queued=%d desired=%d",
        repo, len(containers), len(online), len(busy), queued, desired,
    )

    # Remove exited containers so they cannot block capacity.
    for name in list(containers):
        state = container_status(name)
        if state.get("Status") == "exited":
            log.warning("Removing exited runner container %s", name)
            remove_runner(name)
            containers.remove(name)

    current = len(containers)

    while current < desired:
        create_runner(repo)
        current += 1

    state = load_state()
    idle_since = state.setdefault("idle_since", {})
    by_name = {r.get("name"): r for r in runners}
    now = datetime.now(timezone.utc)

    for name in containers:
        runner = by_name.get(name)
        if runner and runner.get("busy"):
            idle_since.pop(name, None)
        elif runner and runner.get("status") == "online":
            idle_since.setdefault(name, now.isoformat())

    for name in list(idle_since):
        if name not in containers:
            idle_since.pop(name, None)

    save_state(state)

    if os.getenv("GITRUN_EPHEMERAL", "false").lower() == "true":
        return

    if current <= repo_cfg.minimum:
        return

    idle_timeout = env_int("GITRUN_IDLE_TIMEOUT", 120)
    removable = max(0, current - repo_cfg.minimum)
    candidates: list[tuple[str, float]] = []

    for name in containers:
        stamp = idle_since.get(name)
        if not stamp:
            continue
        try:
            idle_age = (now - datetime.fromisoformat(stamp)).total_seconds()
        except ValueError:
            continue
        if idle_age >= idle_timeout:
            candidates.append((name, idle_age))

    for name, _idle_age in sorted(candidates, key=lambda x: x[1], reverse=True):
        if removable <= 0:
            break
        remove_runner(name)
        idle_since.pop(name, None)
        removable -= 1

    save_state(state)


def write_crash(exc: BaseException) -> None:
    path = Path(os.getenv("GITRUN_STATE_DIR", "/var/lib/gitrun")) / "last-crash"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        f"{datetime.now(timezone.utc).isoformat()}\n"
        f"{type(exc).__name__}: {exc}\n",
        encoding="utf-8",
    )


stopping = False


def stop_handler(_signum: int, _frame: object) -> None:
    global stopping
    stopping = True


def main() -> int:
    global stopping
    signal.signal(signal.SIGTERM, stop_handler)
    signal.signal(signal.SIGINT, stop_handler)

    interval = env_int("GITRUN_POLL_INTERVAL", 5)

    if not repositories():
        log.error("No repositories configured.")
        return 2

    log.info("GitRun autoscaler starting")
    log.info("Configured repositories: %s", ", ".join(r.full_name for r in repositories()))

    while not stopping:
        try:
            for repo_cfg in repositories():
                try:
                    reconcile(repo_cfg)
                except Exception as exc:
                    log.exception("Reconciliation failed for %s", repo_cfg.full_name)
                    write_crash(exc)
        except Exception as exc:
            log.exception("Autoscaler loop failed")
            write_crash(exc)

        for _ in range(interval):
            if stopping:
                break
            time.sleep(1)

    log.info("GitRun autoscaler stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
