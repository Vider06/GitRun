#!/usr/bin/env python3
"""Audit Rust source ownership and the declared formal-proof scope.

This gate deliberately does not invoke Kani or Verus on individual Rust files.
Those tools must run against valid Cargo crates or dedicated verifier entrypoints;
compiling arbitrary modules as standalone crates produces misleading dependency
and crate-root failures. Actual proof execution belongs to the dedicated Kani
and Verus steps in the formal-verification job.
"""

from __future__ import annotations

import csv
import os
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


def tracked_sources() -> set[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
    )
    return {item.decode() for item in result.stdout.split(b"\\0") if item}


def write_report(
    sources: set[str], rows: dict[str, tuple[str, ...]]
) -> dict[str, int]:
    counts = {"IMPLEMENTED": 0, "PLANNED": 0}
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    with REPORT.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\\t", lineterminator="\\n")
        writer.writerow(
            ["SOURCE_FILE", "KANI_STATUS", "VERUS_STATUS", "SCOPE_STATUS"]
        )
        for source in sorted(sources):
            mapping = rows[source]
            scope = mapping[6]
            counts[scope] += 1
            # Manifest declarations are plans, not proof evidence. A source is
            # never labelled PROVEN by this inventory/audit step.
            status = "UNVERIFIED" if scope == "IMPLEMENTED" else "NOT_IN_SCOPE"
            writer.writerow([source, status, status, scope])
    return counts


def write_summary(counts: dict[str, int], total: int, issues: list[str]) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary:
        return
    with Path(summary).open("a", encoding="utf-8") as handle:
        handle.write("### Dual-verifier source-scope audit\n\n")
        handle.write("| Metric | Result |\n|---|---:|\n")
        handle.write(f"| Tracked Rust source files mapped | {total} |\n")
        handle.write(f"| Proof scopes declared IMPLEMENTED | {counts['IMPLEMENTED']} |\n")
        handle.write(f"| Proof scopes still PLANNED | {counts['PLANNED']} |\n")
        handle.write(f"| Manifest / ownership issues | {len(issues)} |\n\n")
        handle.write(
            "This report audits source ownership and declared proof scope only. "
            "A manifest entry is not proof evidence: entries marked IMPLEMENTED "
            "remain UNVERIFIED here until a dedicated Kani or Verus execution "
            "produces a successful result. PLANNED entries are NOT_IN_SCOPE, "
            "not silently treated as proven.\n\n"
        )


def main() -> int:
    sources = tracked_sources()
    issues: list[str] = []
    rows: dict[str, tuple[str, ...]] = {}

    if not sources:
        print("No tracked Rust sources found; refusing an empty audit.", file=sys.stderr)
        return 1
    if not MANIFEST.is_file():
        print(f"Required proof-scope manifest is missing: {MANIFEST}", file=sys.stderr)
        return 1

    with MANIFEST.open(encoding="utf-8", newline="") as handle:
        reader = csv.reader(handle, delimiter="\\t")
        if next(reader, []) != EXPECTED_HEADER:
            issues.append("Manifest header does not match the required eight-column schema.")
        for line, row in enumerate(reader, start=2):
            if not row or (len(row) == 1 and not row[0].strip()):
                continue
            if len(row) != len(EXPECTED_HEADER):
                issues.append(
                    f"Manifest line {line}: expected {len(EXPECTED_HEADER)} TSV columns."
                )
                continue
            values = tuple(value.strip() for value in row)
            source, kani, verus, scope, kani_obligations, kani_cases, verus_obligations, status = values
            if source in rows:
                issues.append(f"Duplicate source mapping: {source}")
                continue
            if not all((source, scope, kani_obligations, kani_cases, verus_obligations)):
                issues.append(f"Manifest line {line}: source and all proof-scope descriptions are required.")
            if status not in {"PLANNED", "IMPLEMENTED"}:
                issues.append(f"Manifest line {line}: invalid scope status {status!r}.")
            if status == "IMPLEMENTED":
                for tool, entrypoint in (("KANI", kani), ("VERUS", verus)):
                    if not entrypoint:
                        issues.append(f"{tool} entrypoint missing for implemented scope {source}.")
                    elif not (ROOT / entrypoint).is_file():
                        issues.append(
                            f"{tool} entrypoint does not exist for {source}: {entrypoint}"
                        )
            rows[source] = values

    for stale in sorted(set(rows) - sources):
        issues.append(f"Manifest contains a non-tracked or stale Rust source: {stale}")
    for missing in sorted(sources - set(rows)):
        issues.append(f"Tracked Rust source has no proof-scope mapping: {missing}")

    counts = write_report(sources, rows) if set(rows) >= sources else {
        "IMPLEMENTED": 0,
        "PLANNED": 0,
    }
    write_summary(counts, len(sources), issues)

    print(
        f"Audited {len(sources)} tracked Rust sources: "
        f"{counts['IMPLEMENTED']} declared proof scopes implemented, "
        f"{counts['PLANNED']} planned."
    )
    if issues:
        for issue in issues[:200]:
            print(f"::error::{issue}", file=sys.stderr)
        return 1

    print(
        "Rust source ownership and proof-scope manifest are consistent. "
        "No source is reported as formally proven by this audit."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
