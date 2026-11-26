#!/usr/bin/env python3
"""Verify the installer-pinned runner Dockerfile and assemble its minimal build context."""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def run(command: list[str], *, cwd: Path = ROOT) -> None:
    subprocess.run(command, cwd=cwd, check=True)


def main() -> int:
    updater = (ROOT / "crates/gitrun-updater/src/lib.rs").read_text(encoding="utf-8")
    ref_match = re.search(
        r'pub const CANONICAL_PREMADE_RUNNER_REF: &str = "([0-9a-f]{40})";',
        updater,
    )
    path_match = re.search(
        r'pub const CANONICAL_PREMADE_RUNNER_PATH: &str = "([^"]+)";',
        updater,
    )
    if not ref_match or not path_match:
        raise RuntimeError("canonical Premade runner ref/path constants are missing or malformed")
    pinned_ref, pinned_path = ref_match.groups()

    runner_temp = Path(os.environ["RUNNER_TEMP"])
    premade = runner_temp / "premade-runner.Dockerfile"
    adapted = runner_temp / "premade-runner-adapted.Dockerfile"
    url = f"https://raw.githubusercontent.com/Vider06/GitRun/{pinned_ref}/{pinned_path}"
    run([
        "curl", "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--tlsv1.2", url, "-o", str(premade),
    ])

    source = premade.read_text(encoding="utf-8")
    for marker in (
        "COPY Cargo.toml Cargo.lock ./",
        "COPY crates ./crates",
        'ENTRYPOINT ["/entrypoint.sh"]',
    ):
        if marker not in source:
            raise RuntimeError(f"installer-pinned Dockerfile is missing required marker: {marker}")

    old_copy = "COPY Cargo.toml Cargo.lock ./\nCOPY crates ./crates"
    new_copy = (
        "COPY Cargo.toml Cargo.lock ./\n"
        "COPY crates/gitrun-core ./crates/gitrun-core\n"
        "COPY crates/gitrun-exe ./crates/gitrun-exe\n"
        "COPY crates/gitrun-gsr ./crates/gitrun-gsr\n"
        "COPY crates/gitrun-platform ./crates/gitrun-platform"
    )
    if old_copy not in source:
        raise RuntimeError("installer Dockerfile workspace COPY block changed unexpectedly")
    adapted_source = source.replace(old_copy, new_copy, 1)
    if "COPY crates ./crates" in adapted_source:
        raise RuntimeError("adapted Dockerfile still copies the full crates workspace")
    for marker in (
        "COPY crates/gitrun-core ./crates/gitrun-core",
        "COPY crates/gitrun-exe ./crates/gitrun-exe",
        "COPY crates/gitrun-gsr ./crates/gitrun-gsr",
        "COPY crates/gitrun-platform ./crates/gitrun-platform",
    ):
        if marker not in adapted_source:
            raise RuntimeError(f"adapted Dockerfile is missing required marker: {marker}")
    adapted.write_text(adapted_source, encoding="utf-8")

    print(f"Linting immutable installer-pinned Dockerfile: {pinned_path}@{pinned_ref}")
    run(["hadolint", "--no-color", "--failure-threshold", "error", str(premade)])
    print("Linting setup-adapted installer Dockerfile")
    run(["hadolint", "--no-color", "--failure-threshold", "error", str(adapted)])

    context = runner_temp / "premade-runner-context"
    shutil.rmtree(context, ignore_errors=True)
    (context / "docker/runner").mkdir(parents=True)

    resources = (ROOT / "crates/gitrun-setup/src/resources.rs").read_text(encoding="utf-8")
    start = resources.find("RUNNER_BUILD_FILES:")
    if start < 0:
        raise RuntimeError("RUNNER_BUILD_FILES inventory is missing from setup resources")
    end = resources.find("];", start)
    if end < 0:
        raise RuntimeError("RUNNER_BUILD_FILES inventory is not terminated")
    inventory = resources[start:end]
    targets = [
        item for item in re.findall(r'"([^"]+)"', inventory)
        if item in ("Cargo.toml", "Cargo.lock") or item.startswith("crates/")
    ]
    if not targets:
        raise RuntimeError("RUNNER_BUILD_FILES inventory produced no build-context files")

    for target in targets:
        destination = context / target
        if target == "Cargo.toml":
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(
                '[workspace]\nresolver = "2"\n'
                'members = ["crates/gitrun-core", "crates/gitrun-exe", '
                '"crates/gitrun-gsr", "crates/gitrun-platform"]\n',
                encoding="utf-8",
            )
        elif target == "Cargo.lock":
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(
                ROOT / "crates/gitrun-setup/src/runner-bootstrap.lock",
                destination,
            )
        else:
            source_path = ROOT / target
            if not source_path.is_file():
                raise RuntimeError(f"runner build inventory references missing file: {target}")
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_path, destination)

    shutil.copy2(ROOT / "docker/runner/entrypoint.sh", context / "docker/runner/entrypoint.sh")
    shutil.copy2(adapted, context / "Dockerfile")

    required = (
        "Cargo.toml",
        "Cargo.lock",
        "crates/gitrun-core/Cargo.toml",
        "crates/gitrun-exe/src/lib.rs",
        "crates/gitrun-gsr/src/bin/gitrun-gsr-agent.rs",
        "crates/gitrun-gsr/src/bin/gitrun-api.rs",
        "crates/gitrun-platform/Cargo.toml",
        "crates/gitrun-platform/src/lib.rs",
        "docker/runner/entrypoint.sh",
    )
    missing = [item for item in required if not (context / item).is_file() or (context / item).stat().st_size == 0]
    if missing:
        raise RuntimeError("installer runner build context is missing: " + ", ".join(missing))

    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with Path(summary).open("a", encoding="utf-8") as stream:
            stream.write(
                "### Installer Dockerfile source\n\n"
                "- Fetched the exact immutable Premade Dockerfile pinned by gitrun-updater.\n"
                "- Applied the same workspace-copy adaptation as gitrun-setup.\n"
                "- Prepared the minimal four-crate workspace and pinned bootstrap lockfile for the actual image build below.\n\n"
            )
    print(f"Prepared installer-pinned runner build context at {context}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"::error::{error}", file=sys.stderr)
        raise SystemExit(1)
