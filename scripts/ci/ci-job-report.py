#!/usr/bin/env python3
"""Create the canonical, machine-readable result for one GitRun CI job."""
from __future__ import annotations

import argparse
import json
import os
import re
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SECRET_PATTERNS = [
    (re.compile(r"(?i)(authorization:\s*bearer\s+)[^\s]+"), r"\1[REDACTED]"),
    (re.compile(r"(?i)(token|password|secret|api[_-]?key)(\s*[=:]\s*)[^\s,;]+"), r"\1\2[REDACTED]"),
    (re.compile(r"\bgh[pousr]_[A-Za-z0-9_]{20,}\b"), "[REDACTED_GITHUB_TOKEN]"),
    (re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"), "[REDACTED_GITHUB_TOKEN]"),
]


def redact(value: str) -> str:
    for pattern, replacement in SECRET_PATTERNS:
        value = pattern.sub(replacement, value)
    return value


def read_tsv(path: Path, columns: int) -> list[list[str]]:
    if not path.is_file():
        return []
    rows: list[list[str]] = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        fields = line.split("\t")
        if len(fields) >= columns:
            rows.append(fields[:columns])
    return rows


def append_diagnostic(args: argparse.Namespace) -> None:
    log_path = Path(args.log_file)
    raw = log_path.read_text(encoding="utf-8", errors="replace") if log_path.is_file() else ""
    lines = [redact(line) for line in raw.splitlines()]
    # Keep actionable errors plus a bounded tail; never upload an unbounded compiler log.
    matches = [line for line in lines if re.search(r"(?i)error(?:\[|:|\s)|failed|panicked|verification results", line)]
    selected = list(dict.fromkeys(matches[-35:] + lines[-65:]))[-80:]
    record = {
        "step": args.step,
        "command": redact(args.command),
        "exit_code": args.exit_code,
        "log_file": log_path.name,
        "diagnostic_excerpt": "\n".join(selected)[-16000:],
    }
    target = Path(os.environ.get("RUNNER_TEMP", "/tmp")) / "gitrun-ci" / "diagnostics.jsonl"
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, ensure_ascii=False) + "\n")


def write_result(_: argparse.Namespace) -> None:
    temp = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
    ci_dir = temp / "gitrun-ci"
    diagnostics: list[dict[str, Any]] = []
    diagnostic_file = ci_dir / "diagnostics.jsonl"
    if diagnostic_file.is_file():
        for line in diagnostic_file.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                diagnostics.append(json.loads(line))
            except json.JSONDecodeError:
                diagnostics.append({"step": "diagnostic parser", "diagnostic_excerpt": redact(line)})

    steps = read_tsv(ci_dir / "steps.tsv", 3)
    checks = read_tsv(ci_dir / "checks.tsv", 4)
    caches = read_tsv(ci_dir / "caches.tsv", 5)
    annotations = (ci_dir / "annotations.txt").read_text(encoding="utf-8", errors="replace") if (ci_dir / "annotations.txt").is_file() else ""
    tails = (ci_dir / "tails.txt").read_text(encoding="utf-8", errors="replace") if (ci_dir / "tails.txt").is_file() else ""
    status = os.environ.get("GITRUN_JOB_STATUS", "unknown")
    started = (ci_dir / "started_at").read_text(encoding="utf-8").strip() if (ci_dir / "started_at").is_file() else None
    start_epoch = (ci_dir / "t0").read_text(encoding="utf-8").strip() if (ci_dir / "t0").is_file() else None
    duration = None
    if start_epoch and start_epoch.isdigit():
        duration = max(0, int(datetime.now(timezone.utc).timestamp()) - int(start_epoch))
    report_files: dict[str, str] = {}
    for candidate in sorted(temp.glob("gitrun-*.tsv")):
        if any(marker in candidate.name for marker in ("results", "coverage", "inventory")):
            report_files[candidate.name] = redact(candidate.read_text(encoding="utf-8", errors="replace")[:100000])
    tool_versions = read_tsv(ci_dir / "tools.tsv", 2)
    variable_details = temp / "gitrun-variables-details.md"
    job_details = (ci_dir / "details.md").read_text(encoding="utf-8", errors="replace") if (ci_dir / "details.md").is_file() else ""
    if not job_details and variable_details.is_file():
        job_details = variable_details.read_text(encoding="utf-8", errors="replace")
    result = {
        "schema_version": 1,
        "job": os.environ.get("GITHUB_JOB", "unknown"),
        "job_name": os.environ.get("GITRUN_JOB_NAME", os.environ.get("GITHUB_JOB", "unknown")),
        "status": status,
        "conclusion": status,
        "duration_seconds": duration,
        "started_at_utc": started,
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "repository": os.environ.get("GITHUB_REPOSITORY"),
        "workflow": os.environ.get("GITHUB_WORKFLOW"),
        "event": os.environ.get("GITHUB_EVENT_NAME"),
        "run_id": os.environ.get("GITHUB_RUN_ID"),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        "run_url": f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{os.environ.get('GITHUB_REPOSITORY', '')}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}",
        "commit": os.environ.get("GITHUB_SHA"),
        "ref": os.environ.get("GITHUB_REF"),
        "runner": {"name": os.environ.get("RUNNER_NAME"), "os": os.environ.get("RUNNER_OS"), "arch": os.environ.get("RUNNER_ARCH")},
        "tool_versions": {row[0]: row[1] for row in tool_versions},
        "steps": [{"name": row[0], "status": row[1], "duration_seconds": int(row[2]) if row[2].isdigit() else None} for row in steps],
        "checks": [{"level": row[0], "status": row[1], "name": row[2], "detail": redact(row[3])} for row in checks],
        "caches": [{"name": row[0], "status": row[1], "size": row[2], "requested_key": row[3], "matched_key": row[4]} for row in caches],
        "errors": diagnostics,
        "error_annotations": [redact(x) for x in annotations.splitlines()[:80]],
        "failure_tails": redact(tails[-16000:]),
        "job_details_markdown": redact(job_details[-20000:]),
        "result_files": report_files,
        "counts": {"steps": len(steps), "checks": dict(Counter(row[1] for row in checks)), "diagnostics": len(diagnostics)},
    }
    output = temp / "gitrun-job-result.json"
    output.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Canonical job result written: {output}")


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="action", required=True)
    diagnostic = sub.add_parser("diagnostic")
    diagnostic.add_argument("--step", required=True)
    diagnostic.add_argument("--command", required=True)
    diagnostic.add_argument("--exit-code", required=True, type=int)
    diagnostic.add_argument("--log-file", required=True)
    diagnostic.set_defaults(func=append_diagnostic)
    write = sub.add_parser("write")
    write.set_defaults(func=write_result)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()