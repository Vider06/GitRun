#!/usr/bin/env python3
"""Validate a staged GitRun musl Dashboard AppDir without executing its ELF files."""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

MACHINES = {
    "x86_64-unknown-linux-musl": "Advanced Micro Devices X86-64",
    "aarch64-unknown-linux-musl": "AArch64",
}


def command(*args: str) -> str:
    result = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode:
        raise RuntimeError(f"{' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--appdir", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=tuple(MACHINES))
    args = parser.parse_args()

    appdir = args.appdir.resolve()
    expected_machine = MACHINES[args.target]
    errors: list[str] = []

    required = (
        "AppRun",
        "GitRun.desktop",
        ".DirIcon",
        "sharun",
        "shared/lib/lib.path",
        "bin/gitrun-dashboard-tauri",
        "gitrun.png",
    )
    for relative in required:
        if not (appdir / relative).exists():
            errors.append(f"missing required AppDir entry: {relative}")
    for relative in ("AppRun", "sharun", "bin/gitrun-dashboard-tauri"):
        path = appdir / relative
        if path.exists() and not path.stat().st_mode & 0o111:
            errors.append(f"{relative} is not executable")

    desktop_files = list(appdir.glob("*.desktop"))
    if [path.name for path in desktop_files] != ["GitRun.desktop"]:
        errors.append(f"expected exactly GitRun.desktop, found {[path.name for path in desktop_files]}")

    apprun = appdir / "AppRun"
    if apprun.is_file():
        content = apprun.read_text(encoding="utf-8")
        for marker in (
            'export SHARUN_DIR="$HERE"',
            'exec "$HERE/bin/gitrun-dashboard-tauri"',
        ):
            if marker not in content:
                errors.append(f"AppRun is missing launcher contract: {marker}")

    elf_count = 0
    musl_interpreters = 0
    for path in sorted(appdir.rglob("*")):
        if path.is_symlink() or not path.is_file():
            continue
        try:
            with path.open("rb") as source:
                if source.read(4) != b"\x7fELF":
                    continue
        except OSError:
            continue
        elf_count += 1
        try:
            header = command("readelf", "-h", str(path))
            program_headers = command("readelf", "-lW", str(path))
            dynamic = command("readelf", "-dW", str(path))
        except RuntimeError as exc:
            errors.append(str(exc))
            continue

        machine = re.search(r"^\s*Machine:\s*(.+)$", header, re.MULTILINE)
        actual_machine = machine.group(1).strip() if machine else "unknown"
        if actual_machine != expected_machine:
            errors.append(f"{path.relative_to(appdir)} has machine {actual_machine}, expected {expected_machine}")

        match = re.search(r"Requesting program interpreter:\s*([^\]]+)", program_headers)
        interpreter = match.group(1).strip() if match else ""
        if "ld-linux" in interpreter or "ld-2" in interpreter:
            errors.append(f"{path.relative_to(appdir)} contains a glibc interpreter: {interpreter}")
        elif interpreter:
            if "ld-musl-" not in interpreter:
                errors.append(f"{path.relative_to(appdir)} has an unrecognized interpreter: {interpreter}")
            else:
                musl_interpreters += 1

        if "libc.so.6" in dynamic or "ld-linux" in dynamic:
            errors.append(f"{path.relative_to(appdir)} has a glibc dynamic dependency")

    if elf_count < 2:
        errors.append(f"expected multiple ELF files, found {elf_count}")
    if musl_interpreters < 1:
        errors.append("no dynamic musl interpreter was staged in the AppDir")

    if errors:
        print("Musl Dashboard AppDir validation: FAIL", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"Musl Dashboard AppDir validation: PASS ({args.target}, {elf_count} ELF files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
