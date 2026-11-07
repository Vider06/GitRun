#!/usr/bin/env python3
"""Fail-closed per-file Kani + Verus proof map and execution gate."""

from __future__ import annotations

import csv
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "verification/source-proof-map.tsv"
REPORT = Path(os.environ.get("RUNNER_TEMP", "/tmp")) / "gitrun-dual-verifier-coverage.tsv"
EXPECTED_HEADER = ["SOURCE_FILE", "KANI_ENTRYPOINT", "VERUS_ENTRYPOINT"]
PATH_ATTRIBUTE = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]')


def tracked_sources() -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {item.decode() for item in result.stdout.split(b"\0") if item}


def imported_sources(entrypoint: Path) -> set[str]:
    imports = set()
    for imported in PATH_ATTRIBUTE.findall(entrypoint.read_text(encoding="utf-8")):
        try:
            imports.add((entrypoint.parent / imported).resolve().relative_to(ROOT.resolve()).as_posix())
        except ValueError:
            continue
    return imports


def main() -> int:
    sources = tracked_sources()
    rows: dict[str, tuple[str, str]] = {}
    errors: list[str] = []

    if not sources:
        print("No tracked Rust sources found; refusing an empty proof pass.", file=sys.stderr)
        return 1
    if not MANIFEST.is_file():
        print(f"Required dual-verifier manifest is missing: {MANIFEST}", file=sys.stderr)
        return 1

    with MANIFEST.open(encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle, delimiter="\t")
        if next(reader, []) != EXPECTED_HEADER:
            errors.append("Manifest header must be SOURCE_FILE, KANI_ENTRYPOINT, VERUS_ENTRYPOINT.")
        for line, row in enumerate(reader, start=2):
            if not row or (len(row) == 1 and not row[0].strip()):
                continue
            if len(row) != 3:
                errors.append(f"Manifest line {line}: expected exactly 3 TSV columns.")
                continue
            source, kani_name, verus_name = (value.strip() for value in row)
            if source in rows:
                errors.append(f"Duplicate source mapping: {source}")
                continue
            rows[source] = (kani_name, verus_name)

    for stale in sorted(set(rows) - sources):
        errors.append(f"Manifest contains a non-tracked or stale Rust source: {stale}")
    for missing in sorted(sources - set(rows)):
        errors.append(f"UNVERIFIED: no Kani + Verus mapping for {missing}")

    kani_used: set[str] = set()
    verus_used: set[str] = set()
    ready: list[tuple[str, str, str]] = []
    statuses = {source: ["UNVERIFIED", "UNVERIFIED"] for source in sources}

    for source, (kani_name, verus_name) in sorted(rows.items()):
        if source not in sources:
            continue
        for index, (tool, name, used) in enumerate(
            (("KANI", kani_name, kani_used), ("VERUS", verus_name, verus_used))
        ):
            entrypoint = ROOT / name
            if not name or not entrypoint.is_file():
                errors.append(f"{tool} entrypoint missing for {source}: {name or '<empty>'}")
                statuses[source][index] = "ERROR"
                continue
            if name in used:
                errors.append(f"{tool} entrypoint reused; require a per-file proof: {name}")
                continue
            used.add(name)
            if source not in imported_sources(entrypoint):
                errors.append(f"{tool} entrypoint {name} does not directly #[path]-import {source}")
                continue
            content = entrypoint.read_text(encoding="utf-8")
            if tool == "KANI" and "#[kani::proof]" not in content:
                errors.append(f"KANI entrypoint {name} has no #[kani::proof] harness")
                continue
            if tool == "VERUS" and "verus!" not in content:
                errors.append(f"VERUS entrypoint {name} has no verus! verification module")
                continue
            ready.append((source, tool, name))
            statuses[source][index] = "READY"

    if errors:
        REPORT.parent.mkdir(parents=True, exist_ok=True)
        with REPORT.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
            writer.writerow(["SOURCE_FILE", "KANI_STATUS", "VERUS_STATUS"])
            for source in sorted(sources):
                writer.writerow([source, *statuses[source]])
        print(
            f"Dual-verifier coverage incomplete: {len(errors)} issue(s) across "
            f"{len(sources)} tracked Rust source file(s).",
            file=sys.stderr,
        )
        for error in errors[:200]:
            print(f"::error::{error}", file=sys.stderr)
        print("No incomplete mapping is accepted as a passing proof.", file=sys.stderr)
        return 1

    results = {source: ["UNVERIFIED", "UNVERIFIED"] for source in sources}
    for source, tool, name in ready:
        command = ["kani", str(ROOT / name)] if tool == "KANI" else [
            os.environ.get("VERUS_PATH", "verus"), str(ROOT / name)
        ]
        try:
            completed = subprocess.run(command, cwd=ROOT, check=False)
        except OSError as exc:
            errors.append(f"{tool} could not execute for {source}: {exc}")
            results[source][0 if tool == "KANI" else 1] = "ERROR"
            continue
        status = "PROVEN" if completed.returncode == 0 else "FAILED"
        results[source][0 if tool == "KANI" else 1] = status
        if completed.returncode:
            errors.append(f"{tool} proof failed for {source} (exit {completed.returncode})")

    REPORT.parent.mkdir(parents=True, exist_ok=True)
    with REPORT.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(["SOURCE_FILE", "KANI_STATUS", "VERUS_STATUS"])
        for source in sorted(sources):
            writer.writerow([source, *results[source]])
    if errors or any(status != "PROVEN" for pair in results.values() for status in pair):
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    print(f"Both Kani and Verus proved all {len(sources)} tracked Rust source files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
