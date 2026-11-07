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
EXPECTED_HEADER = [
    "SOURCE_FILE",
    "KANI_ENTRYPOINT",
    "VERUS_ENTRYPOINT",
    "PROOF_SCOPE",
    "KANI_OBLIGATIONS",
    "KANI_CASES",
    "VERUS_OBLIGATIONS",
    "STATUS",
]
PATH_ATTRIBUTE = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]')
KANI_PROOF = re.compile(r"#\s*\[\s*kani\s*::\s*proof\s*\]")
VERUS_MODULE = re.compile(r"\bverus\s*!")


def tracked_sources() -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {item.decode() for item in result.stdout.split(b"\0") if item}


def source_closure(entrypoint: Path) -> set[str]:
    """Return all repository-local path modules reachable from an entrypoint."""
    root = ROOT.resolve()
    visited: set[Path] = set()

    def visit(path: Path) -> None:
        resolved = path.resolve()
        try:
            resolved.relative_to(root)
        except ValueError:
            return
        if resolved in visited or not resolved.is_file():
            return
        visited.add(resolved)
        source = resolved.read_text(encoding="utf-8")
        for imported in PATH_ATTRIBUTE.findall(source):
            visit(resolved.parent / imported)

    visit(entrypoint)
    return {path.relative_to(root).as_posix() for path in visited}


def write_report(
    sources: set[str], statuses: dict[str, list[str]]
) -> None:
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    with REPORT.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
        writer.writerow(["SOURCE_FILE", "KANI_STATUS", "VERUS_STATUS"])
        for source in sorted(sources):
            writer.writerow([source, *statuses[source]])


def write_summary(
    sources: set[str], statuses: dict[str, list[str]], *, phase: str, issues: int
) -> None:
    totals = {
        status: sum(pair.count(status) for pair in statuses.values())
        for status in ("PROVEN", "FAILED", "UNVERIFIED", "ERROR")
    }
    kani_proven = sum(pair[0] == "PROVEN" for pair in statuses.values())
    verus_proven = sum(pair[1] == "PROVEN" for pair in statuses.values())
    fully_proven = sum(pair == ["PROVEN", "PROVEN"] for pair in statuses.values())
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return

    with Path(summary).open("a", encoding="utf-8") as handle:
        handle.write("### Dual-verifier Rust source coverage\n\n")
        handle.write("| Metric | Result |\n|---|---:|\n")
        handle.write(f"| Tracked Rust source files | {len(sources)} |\n")
        handle.write(f"| Fully proven by Kani and Verus | {fully_proven} |\n")
        handle.write(f"| Kani proof results: PROVEN | {kani_proven} |\n")
        handle.write(f"| Verus proof results: PROVEN | {verus_proven} |\n")
        handle.write(f"| FAILED results | {totals['FAILED']} |\n")
        handle.write(f"| UNVERIFIED results | {totals['UNVERIFIED']} |\n")
        handle.write(f"| ERROR results | {totals['ERROR']} |\n")
        handle.write(f"| Preflight / execution issues | {issues} |\n")
        handle.write(f"| Gate phase | {phase} |\n\n")
        handle.write(
            "A source is fully proven only when both independent verifier results "
            "are PROVEN. Inventory, compilation, linting, smoke proofs, and "
            "structural source-closure checks do not count as formal proofs.\n\n"
        )


def main() -> int:
    sources = tracked_sources()
    rows: dict[str, tuple[str, str, str, str, str, str, str]] = {}
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
            errors.append(
                "Manifest header must define source, entrypoints, proof scope, Kani obligations, "
                "Kani cases, Verus obligations, and status."
            )
        for line, row in enumerate(reader, start=2):
            if not row or (len(row) == 1 and not row[0].strip()):
                continue
            if len(row) != len(EXPECTED_HEADER):
                errors.append(
                    f"Manifest line {line}: expected exactly {len(EXPECTED_HEADER)} TSV columns."
                )
                continue
            (
                source,
                kani_name,
                verus_name,
                proof_scope,
                kani_obligations,
                kani_cases,
                verus_obligations,
                plan_status,
            ) = (value.strip() for value in row)
            if not all((proof_scope, kani_obligations, kani_cases, verus_obligations)):
                errors.append(
                    f"Manifest line {line}: proof scope, Kani obligations/cases, "
                    "and Verus obligations are required."
                )
            if plan_status not in {"PLANNED", "IMPLEMENTED"}:
                errors.append(
                    f"Manifest line {line}: STATUS must be PLANNED or IMPLEMENTED, got {plan_status!r}."
                )
            if source in rows:
                errors.append(f"Duplicate source mapping: {source}")
                continue
            rows[source] = (
                kani_name,
                verus_name,
                proof_scope,
                kani_obligations,
                kani_cases,
                verus_obligations,
                plan_status,
            )

    for stale in sorted(set(rows) - sources):
        errors.append(f"Manifest contains a non-tracked or stale Rust source: {stale}")
    for missing in sorted(sources - set(rows)):
        errors.append(f"UNVERIFIED: no Kani + Verus mapping for {missing}")

    kani_used: set[str] = set()
    verus_used: set[str] = set()
    ready: list[tuple[str, str, str]] = []

    for source, mapping in sorted(rows.items()):
        kani_name, verus_name = mapping[:2]
        if source not in sources:
            continue
        if mapping[6] == "PLANNED":
            errors.append(
                f"UNVERIFIED: Kani and Verus proof implementation is still PLANNED for {source}"
            )
            continue
        for tool, name, used in (
            ("KANI", kani_name, kani_used),
            ("VERUS", verus_name, verus_used),
        ):
            entrypoint = ROOT / name
            if not name or not entrypoint.is_file():
                errors.append(f"{tool} entrypoint missing for {source}: {name or '<empty>'}")
                continue
            if name in used:
                errors.append(f"{tool} entrypoint reused; require a per-file proof: {name}")
                continue
            used.add(name)
            try:
                imported = source_closure(entrypoint)
            except (OSError, UnicodeError) as exc:
                errors.append(f"{tool} entrypoint could not be inspected for {source}: {exc}")
                continue
            if source not in imported:
                errors.append(
                    f"{tool} entrypoint {name} does not include {source} "
                    "through a repository-local #[path] module"
                )
                continue

            closure_sources = [
                (ROOT / imported_source).read_text(encoding="utf-8")
                for imported_source in sorted(imported)
            ]
            if tool == "KANI" and not any(
                KANI_PROOF.search(text) for text in closure_sources
            ):
                errors.append(
                    f"KANI closure for {source} contains no #[kani::proof] harness"
                )
                continue
            if tool == "VERUS" and not any(
                VERUS_MODULE.search(text) for text in closure_sources
            ):
                errors.append(
                    f"VERUS closure for {source} contains no verus! verification module"
                )
                continue
            ready.append((source, tool, name))

    # Execute every individually validated proof even when other source files
    # are still unmapped. The overall gate remains fail-closed: incomplete
    # coverage or any failed proof still returns non-zero. This lets the
    # manifest grow incrementally without hiding valid proof results.
    results = {source: ["UNVERIFIED", "UNVERIFIED"] for source in sources}
    for source, tool, name in ready:
        command = (
            ["kani", str(ROOT / name)]
            if tool == "KANI"
            else [os.environ.get("VERUS_PATH", "verus"), str(ROOT / name)]
        )
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

    write_report(sources, results)
    phase = "execution complete" if not errors else "execution complete with preflight issues"
    write_summary(sources, results, phase=phase, issues=len(errors))
    if errors or any(status != "PROVEN" for pair in results.values() for status in pair):
        print(
            f"Dual-verifier coverage incomplete: {len(errors)} preflight/execution "
            f"issue(s) across {len(sources)} tracked Rust source file(s).",
            file=sys.stderr,
        )
        for error in errors[:200]:
            print(f"::error::{error}", file=sys.stderr)
        return 1

    print(f"Both Kani and Verus proved all {len(sources)} tracked Rust source files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
