#!/usr/bin/env python3
"""Build and validate a Server XBPS package using its canonical repository name."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path


REQUIRED_ARCHIVE_PATHS = (
    "./usr/libexec/gitrun/gitrun-service",
    "./etc/sv/gitrun/run",
    "./etc/sv/gitrun/log/run",
    "./etc/sv/gitrun-gsr/run",
    "./etc/sv/gitrun-gsr/log/run",
)


def run_capture(command: list[str], *, cwd: Path, env: dict[str, str]) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if completed.returncode:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(
            f"Command failed ({completed.returncode}): {' '.join(command)}"
            + (f"\n{detail}" if detail else "")
        )
    return completed.stdout


def build_server_package(args: argparse.Namespace) -> Path:
    output = args.output.resolve()
    stage = args.stage.resolve()
    repository = args.repository.resolve()
    install_hook = args.install_hook.resolve()
    remove_hook = args.remove_hook.resolve()

    for required in (args.create, args.rindex, args.query):
        if not Path(required).is_file():
            raise FileNotFoundError(f"XBPS tool does not exist: {required}")
    if not stage.is_dir():
        raise FileNotFoundError(f"XBPS package staging directory does not exist: {stage}")
    if not install_hook.is_file() or not remove_hook.is_file():
        raise FileNotFoundError("XBPS lifecycle hook source is missing")
    if "sv restart" not in install_hook.read_text(encoding="utf-8"):
        raise RuntimeError(
            f"XBPS INSTALL hook for {args.package_id} does not restart active runit services on upgrade"
        )
    if "sv down" not in remove_hook.read_text(encoding="utf-8"):
        raise RuntimeError(
            f"XBPS REMOVE hook for {args.package_id} does not stop runit services before removal"
        )

    output.mkdir(parents=True, exist_ok=True)
    if repository.exists():
        shutil.rmtree(repository)
    repository.mkdir(parents=True)

    package_id = args.package_id
    environment = os.environ.copy()
    environment["XBPS_ARCH"] = args.architecture

    create_command = [
        args.create,
        "-A",
        args.architecture,
        "-n",
        package_id,
        "-D",
        args.dependency,
        "-H",
        "https://github.com/Vider06/GitRun",
        "-l",
        "Apache-2.0",
        "-m",
        "Vider06",
        "-s",
        "GitRun headless server",
        str(stage),
    ]
    subprocess.run(
        create_command,
        cwd=repository,
        env=environment,
        check=True,
    )

    # XBPS repository indexes store the package's canonical archive filename.
    # Keep that filename intact until every repository query has completed.
    archives = sorted(repository.glob(f"{package_id}*.xbps"))
    if len(archives) != 1 or not archives[0].is_file() or archives[0].stat().st_size == 0:
        names = ", ".join(path.name for path in archives) or "<none>"
        raise RuntimeError(
            f"Expected exactly one non-empty XBPS archive for {package_id}; found: {names}"
        )
    archive = archives[0]

    archive_listing = run_capture(
        ["tar", "-tf", str(archive)], cwd=repository, env=environment
    ).splitlines()
    for required in REQUIRED_ARCHIVE_PATHS:
        if required not in archive_listing:
            raise RuntimeError(f"XBPS package is missing runit resource: {required}")
    if any(
        path.startswith("./etc/systemd/") or path.startswith("./usr/lib/systemd/")
        for path in archive_listing
    ):
        raise RuntimeError("XBPS package must contain runit resources, not systemd units")

    subprocess.run(
        [args.rindex, "-a", str(archive)],
        cwd=repository,
        env=environment,
        check=True,
    )

    dependencies = run_capture(
        [
            args.query,
            "-R",
            f"--repository={repository}",
            "-x",
            package_id,
        ],
        cwd=repository,
        env=environment,
    )
    if args.dependency not in dependencies:
        raise RuntimeError(
            f"XBPS dependency contract failed for {package_id}: expected "
            f"'{args.dependency}', received '{dependencies.strip()}'."
        )

    install_script = run_capture(
        [
            args.query,
            "-R",
            f"--repository={repository}",
            "--property=install-script",
            package_id,
        ],
        cwd=repository,
        env=environment,
    )
    remove_script = run_capture(
        [
            args.query,
            "-R",
            f"--repository={repository}",
            "--property=remove-script",
            package_id,
        ],
        cwd=repository,
        env=environment,
    )
    if "sv restart" not in install_script:
        raise RuntimeError(
            f"XBPS package {package_id} has no install-script property with the runit upgrade restart"
        )
    if "sv down" not in remove_script:
        raise RuntimeError(
            f"XBPS package {package_id} has no remove-script property with the runit shutdown"
        )

    final_path = output / f"GitRun-Server-{args.version}-linux-{args.cpu}-xbps.xbps"
    shutil.copy2(archive, final_path)

    output_archives = list(output.glob("*.xbps"))
    output_files = [path for path in output.iterdir() if path.is_file()]
    if len(output_archives) != 1:
        raise RuntimeError(
            f"Expected one publishable XBPS archive in {output}; found {len(output_archives)}"
        )
    if len(output_files) < 3:
        raise RuntimeError(
            f"Expected at least three Server package outputs in {output}; found {len(output_files)}"
        )

    print(f"Validated XBPS repository archive: {archive.name}")
    print(f"Published package artifact: {final_path.name}")
    return final_path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--repository", type=Path, required=True)
    parser.add_argument("--create", required=True)
    parser.add_argument("--rindex", required=True)
    parser.add_argument("--query", required=True)
    parser.add_argument("--architecture", required=True)
    parser.add_argument("--dependency", required=True)
    parser.add_argument("--package-id", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--cpu", required=True)
    parser.add_argument("--install-hook", type=Path, required=True)
    parser.add_argument("--remove-hook", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    try:
        build_server_package(parse_args())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"XBPS Server package certification failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
