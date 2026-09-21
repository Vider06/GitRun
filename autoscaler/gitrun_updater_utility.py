#!/usr/bin/env python3
"""GTUU — GitRun Updater Utility.

Updates GitRun-managed permanent runner containers one at a time.
Dynamic runners are intentionally left alone because they are refreshed
when they are recreated by the autoscaler.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime
from pathlib import Path


API = "https://api.github.com"
API_VERSION = "2026-03-10"
USER_AGENT = "GitRun-GTUU/1.0"


def env_bool(name: str, default: bool = False) -> bool:
    value = os.getenv(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def env_int(name: str, default: int) -> int:
    try:
        return int(os.getenv(name, str(default)))
    except ValueError:
        return default


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=check,
    )


def token() -> str:
    value = os.getenv("GITHUB_TOKEN", "").strip()
    if not value:
        raise RuntimeError("GITHUB_TOKEN is not configured")


def split_repo(repo: str) -> tuple[str, str]:
    if not re.fullmatch(r"[^/]+/[^/]+", repo):
        raise ValueError(f"invalid repository: {repo}")
    return repo.split("/", 1)


def api_request(method: str, path: str, body: dict | None = None) -> dict:
    payload = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(
        API + path,
        data=payload,
        method=method,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token()}",
            "X-GitHub-Api-Version": API_VERSION,
            "User-Agent": USER_AGENT,
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read().decode("utf-8")
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(f"GitHub API {exc.code}: {detail}") from exc


def repositories() -> list[str]:
    raw = os.getenv("GITRUN_REPOSITORIES", "").strip()
    if not raw:
        raw = os.getenv("GITRUN_DEFAULT_REPOSITORY", "").strip()
    return [item.strip() for item in raw.split(",") if item.strip()]


def container_names() -> list[str]:
    result = docker(
        "ps",
        "-a",
        "--filter",
        "label=gitrun.runner=true",
        "--filter",
        "label=gitrun.managed=true",
        "--format",
        "{{.Names}}",
    )
    return [name.strip() for name in result.stdout.splitlines() if name.strip()]


def inspect_container(name: str) -> dict:
    result = docker("inspect", name, check=False)
    if result.returncode:
        return {}
    try:
        return json.loads(result.stdout)[0]
    except (IndexError, json.JSONDecodeError):
        return {}


def container_labels(container: dict) -> dict[str, str]:
    return dict(container.get("Config", {}).get("Labels") or {})


def permanent_container(container: dict) -> bool:
    labels = container_labels(container)
    if labels.get("gitrun.permanent", "").lower() == "true":
        return True
    # Containers created before GTUU did not have role labels. Treat them as
    # permanent so an upgrade does not silently abandon the warm pool.
    return labels.get("gitrun.dynamic", "").lower() != "true"


def container_image_id(container: dict) -> str:
    return str(container.get("Image") or "")


def local_image_id(image: str) -> str:
    result = docker("image", "inspect", "--format", "{{.Id}}", image, check=False)
    return result.stdout.strip() if result.returncode == 0 else ""


def pull_runner_image(image: str) -> str:
    print(f"GTUU: pulling runner image {image}")
    result = docker("pull", image, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"docker pull failed for {image}")
    image_id = local_image_id(image)
    if not image_id:
        raise RuntimeError(f"unable to inspect runner image {image} after pull")
    print(f"GTUU: current image id {image_id}")
    return image_id


def runner_info(repo: str, name: str) -> dict | None:
    owner, project = split_repo(repo)
    runners = api_request("GET", f"/repos/{owner}/{project}/actions/runners?per_page=100").get("runners", [])
    return next((runner for runner in runners if runner.get("name") == name), None)


def remove_github_runner(repo: str, runner_id: int) -> None:
    owner, project = split_repo(repo)
    api_request("DELETE", f"/repos/{owner}/{project}/actions/runners/{runner_id}")


def registration_token(repo: str) -> str:
    owner, project = split_repo(repo)
    return api_request(
        "POST",
        f"/repos/{owner}/{project}/actions/runners/registration-token",
    )["token"]


def create_replacement(repo: str, old_name: str, image: str, permanent: bool) -> str:
    safe = re.sub(r"[^a-zA-Z0-9_.-]", "-", old_name)
    replacement = f"{safe}-gtuu-{os.getpid()}-{int(time.time()) % 100000}"
    token_value = registration_token(repo)
    labels = os.getenv("GITRUN_RUNNER_LABELS", "self-hosted,Linux,X64")
    ephemeral = os.getenv("GITRUN_EPHEMERAL", "false")
    disable_update = os.getenv("GITRUN_DISABLE_UPDATE", "false")

    docker_labels = [
        "--label",
        "gitrun.runner=true",
        "--label",
        f"gitrun.repo={repo}",
        "--label",
        "gitrun.managed=true",
        "--label",
        f"gitrun.permanent={'true' if permanent else 'false'}",
        "--label",
        f"gitrun.dynamic={'false' if permanent else 'true'}",
    ]

    command = [
        "run",
        "-d",
        "--name",
        replacement,
        *docker_labels,
        "--cpus",
        os.getenv("GITRUN_CONTAINER_CPUS", "1"),
        "--memory",
        os.getenv("GITRUN_CONTAINER_MEMORY", "1g"),
        "--pids-limit",
        os.getenv("GITRUN_CONTAINER_PIDS", "1024"),
        "--restart",
        "unless-stopped",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=256m",
        "-e",
        f"RUNNER_URL=https://github.com/{repo}",
        "-e",
        f"RUNNER_TOKEN={token_value}",
        "-e",
        f"RUNNER_NAME={old_name}",
        "-e",
        f"RUNNER_LABELS={labels}",
        "-e",
        f"RUNNER_EPHEMERAL={ephemeral}",
        "-e",
        f"RUNNER_DISABLE_UPDATE={disable_update}",
        image,
    ]
    result = docker(*command, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or "docker replacement creation failed")
    return replacement


def wait_for_online(repo: str, runner_name: str, timeout: int = 120) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        runner = runner_info(repo, runner_name)
        if runner and runner.get("status") == "online":
            return True
        time.sleep(3)
    return False


def remove_container(name: str) -> None:
    result = docker("rm", "-f", name, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"unable to remove container {name}")


def update_container(repo: str, name: str, image: str, new_image_id: str) -> str:
    old = inspect_container(name)
    if not old:
        return "missing"

    old_image_id = container_image_id(old)
    if old_image_id == new_image_id:
        return "current"

    runner = runner_info(repo, name)
    if runner and runner.get("busy"):
        return "busy"

    if runner and runner.get("id"):
        remove_github_runner(repo, int(runner["id"]))

    remove_container(name)
    replacement = create_replacement(repo, name, image, permanent_container(old))

    if not wait_for_online(repo, name):
        # The new Docker container has the old GitHub runner name by design.
        # Leave it running for diagnosis rather than silently restoring the
        # obsolete container image.
        print(f"GTUU: replacement {replacement} did not become online", file=sys.stderr)
        return "offline"

    # The replacement is now serving the same GitHub runner registration.
    # Give it the stable container name expected by operators.
    rename = docker("rename", replacement, name, check=False)
    if rename.returncode:
        raise RuntimeError(rename.stderr.strip() or f"unable to rename replacement {replacement}")
    print(f"GTUU: updated {name} ({repo})")
    return "updated"


def acquire_lock() -> Path:
    state_dir = Path(os.getenv("GITRUN_STATE_DIR", "/var/lib/gitrun"))
    state_dir.mkdir(parents=True, exist_ok=True)
    lock = state_dir / "gtuu.lock"
    try:
        lock.mkdir()
    except FileExistsError as exc:
        raise RuntimeError("another GTUU run is already active") from exc
    (lock / "pid").write_text(str(os.getpid()), encoding="utf-8")
    return lock


def release_lock(lock: Path) -> None:
    try:
        (lock / "pid").unlink(missing_ok=True)
        lock.rmdir()
    except OSError:
        pass


def update_permanent_containers() -> int:
    image = os.getenv("GITRUN_RUNNER_IMAGE", "gitrun-runner:latest")
    repos = repositories()
    if not repos:
        print("GTUU: no repositories configured")
        return 0

    docker("info")
    new_image_id = pull_runner_image(image)

    updated = 0
    for name in container_names():
        container = inspect_container(name)
        labels = container_labels(container)
        if not permanent_container(container):
            continue

        repo = labels.get("gitrun.repo", "").strip()
        if repo not in repos:
            continue

        result = update_container(repo, name, image, new_image_id)
        if result == "updated":
            updated += 1
        elif result == "busy":
            print(f"GTUU: skip busy runner {name} ({repo})")
    print(f"GTUU: complete — {updated} permanent runner(s) updated")
    return updated


def main(argv: list[str]) -> int:
    if argv not in ([], ["--only-containers"]):
        print("Usage: gitrun-updater-utility [--only-containers]", file=sys.stderr)
        return 2

    lock = acquire_lock()
    try:
        update_permanent_containers()
    except Exception as exc:
        print(f"GTUU: FAIL — {exc}", file=sys.stderr)
        return 1
    finally:
        release_lock(lock)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
