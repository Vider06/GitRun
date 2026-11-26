#!/usr/bin/env python3
"""Scan tracked text files for credential-shaped strings and emit GitHub SARIF findings."""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any
from urllib.parse import quote

ROOT = Path(__file__).resolve().parents[2]
EXCLUDED_FILES = {"docs/ROADMAP_PHASES.md", "SECURITY.md"}
RULES = (
    {
        "id": "gitrun-github-token",
        "name": "GitHub access token",
        "pattern": re.compile(r"gh[pousr]" + r"_[A-Za-z0-9_]{20,}", re.IGNORECASE),
    },
    {
        "id": "gitrun-github-fine-grained-token",
        "name": "GitHub fine-grained personal access token",
        "pattern": re.compile(r"github" + r"_pat_[A-Za-z0-9_]{20,}", re.IGNORECASE),
    },
    {
        "id": "gitrun-aws-access-key",
        "name": "AWS access key ID",
        "pattern": re.compile(r"AKIA[0-9A-Z]{16}"),
    },
    {
        "id": "gitrun-private-key-header",
        "name": "Private key PEM header",
        "pattern": re.compile(
            r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----",
            re.IGNORECASE,
        ),
    },
    {
        "id": "gitrun-slack-token",
        "name": "Slack token",
        "pattern": re.compile(r"xox[baprs]" + r"-[A-Za-z0-9-]{10,}", re.IGNORECASE),
    },
    {
        "id": "gitrun-stripe-live-key",
        "name": "Stripe live secret key",
        "pattern": re.compile(r"sk_live" + r"_[0-9a-zA-Z]{24,}", re.IGNORECASE),
    },
)


def _redacted_sample(value: str) -> str:
    if value.startswith("-----BEGIN "):
        return "private-key header (contents not inspected)"
    if len(value) <= 10:
        return f"{value[:2]}… (redacted, {len(value)} chars)"
    return f"{value[:4]}…{value[-4:]} (redacted, {len(value)} chars)"


def _annotation_escape(value: str) -> str:
    return (
        value.replace("%", "%25")
        .replace("\r", "%0D")
        .replace("\n", "%0A")
        .replace(":", "%3A")
        .replace(",", "%2C")
    )


def _sarif(results: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [
            {
                "tool": {
                    "driver": {
                        "name": "GitRun Secret Pattern Scanner",
                        "semanticVersion": "1.0.0",
                        "rules": [
                            {
                                "id": rule["id"],
                                "name": rule["name"],
                                "shortDescription": {"text": rule["name"]},
                                "defaultConfiguration": {"level": "error"},
                            }
                            for rule in RULES
                        ],
                    }
                },
                "automationDetails": {"id": "gitrun-secret-pattern-scan"},
                "results": results,
            }
        ],
    }


def _load_tracked_files() -> list[str]:
    completed = subprocess.run(
        ("git", "ls-files", "-z"),
        cwd=ROOT,
        check=True,
        capture_output=True,
    )
    return [
        path
        for path in completed.stdout.decode("utf-8", errors="replace").split("\0")
        if path
    ]


def _scan() -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    for relative in _load_tracked_files():
        if relative in EXCLUDED_FILES:
            continue
        try:
            raw = (ROOT / relative).read_bytes()
        except OSError:
            continue
        if b"\0" in raw:
            continue
        lines = raw.decode("utf-8", errors="replace").splitlines()
        for line_number, line in enumerate(lines, 1):
            for rule in RULES:
                for match in rule["pattern"].finditer(line):
                    sample = _redacted_sample(match.group())
                    message = (
                        f"{rule['name']} pattern matched {sample}. "
                        "The credential-like value is redacted; rotate it if genuine."
                    )
                    results.append(
                        {
                            "ruleId": rule["id"],
                            "level": "error",
                            "message": {"text": message},
                            "locations": [
                                {
                                    "physicalLocation": {
                                        "artifactLocation": {
                                            "uri": quote(Path(relative).as_posix(), safe="/.-_")
                                        },
                                        "region": {
                                            "startLine": line_number,
                                            "startColumn": match.start() + 1,
                                            "endColumn": match.end() + 1,
                                        },
                                    }
                                }
                            ],
                        }
                    )
                    print(
                        "::error "
                        f"file={_annotation_escape(relative)},"
                        f"line={line_number},col={match.start() + 1},"
                        f"title={_annotation_escape(rule['name'])}::"
                        f"{_annotation_escape(message)}"
                    )
    return results


def _write_sarif(path: Path, results: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(_sarif(results), indent=2) + "\n",
        encoding="utf-8",
    )


def _fail_on_sarif_findings(path: Path) -> int:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        results = document["runs"][0]["results"]
    except (OSError, json.JSONDecodeError, KeyError, IndexError, TypeError) as exc:
        print(f"Unable to read the secret scanner SARIF report: {exc}", file=sys.stderr)
        return 2
    if results:
        print(
            f"Secret pattern scan found {len(results)} credential-like occurrence(s); "
            "the SARIF report has already been uploaded."
        )
        return 1
    print("Secret pattern scan found no credential-like strings.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sarif-file", type=Path)
    parser.add_argument(
        "--defer-failure",
        action="store_true",
        help="write SARIF and return success so the workflow can upload it before failing",
    )
    parser.add_argument(
        "--fail-on-findings",
        action="store_true",
        help="fail based on an existing SARIF report after the upload step",
    )
    args = parser.parse_args()

    if args.defer_failure and (args.sarif_file is None or args.fail_on_findings):
        parser.error(
            "--defer-failure requires --sarif-file and cannot be combined "
            "with --fail-on-findings"
        )
    if args.fail_on_findings:
        if args.sarif_file is None:
            parser.error("--fail-on-findings requires --sarif-file")
        return _fail_on_sarif_findings(args.sarif_file)

    results = _scan()
    if args.sarif_file is not None:
        _write_sarif(args.sarif_file, results)
    if args.defer_failure:
        print(f"Prepared SARIF report with {len(results)} finding(s); upload precedes the gate.")
        return 0
    return 1 if results else 0


if __name__ == "__main__":
    raise SystemExit(main())
