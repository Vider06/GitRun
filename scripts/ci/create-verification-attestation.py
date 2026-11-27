#!/usr/bin/env python3
"""Create a per-job CI attestation for pushes to main."""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path


REQUIRED_JOBS = (
    "rust-source-inventory",
    "kani-formal-verification",
    "verus-formal-verification",
    "formal-verification",
    "miri",
    "summary",
)


def main() -> int:
    if os.environ.get("EVENT") != "push" or os.environ.get("BRANCH") != "main":
        raise SystemExit("attestation may only be generated for push events on main")

    try:
        needs = json.loads(os.environ["NEEDS"])
    except (KeyError, json.JSONDecodeError) as error:
        raise SystemExit(
            f"cannot decode GitHub Actions needs context: {error}"
        ) from error

    if not isinstance(needs, dict) or not needs:
        raise SystemExit("GitHub Actions needs context is empty")

    jobs = {}
    for job_id, details in sorted(needs.items()):
        if not isinstance(details, dict):
            raise SystemExit(f"invalid needs entry for job {job_id!r}")

        result = details.get("result")
        if result not in {"success", "failure", "cancelled", "skipped"}:
            raise SystemExit(
                f"job {job_id!r} has an unknown result: {result!r}"
            )

        outputs = details.get("outputs", {})
        jobs[job_id] = {
            "result": result,
            "outputs": outputs if isinstance(outputs, dict) else {},
        }

    # The artifact upload is the final step; if it succeeds, this job succeeds.
    jobs["verification-attestation"] = {"result": "success", "outputs": {}}

    missing = [job for job in REQUIRED_JOBS if job not in jobs]
    if missing:
        raise SystemExit(
            "attestation is missing required job records: " + ", ".join(missing)
        )

    run_attempt = int(os.environ["RUN_ATTEMPT"])
    artifact_name = f"gitrun-main-verification-attestation-{run_attempt}"

    document = {
        "schema_version": 1,
        "repository": os.environ["REPOSITORY"],
        "workflow_name": os.environ["WORKFLOW_NAME"],
        "workflow_path": ".github/workflows/ci.yml",
        "event": os.environ["EVENT"],
        "branch": os.environ["BRANCH"],
        "commit_sha": os.environ["COMMIT_SHA"],
        "run_id": int(os.environ["RUN_ID"]),
        "run_attempt": run_attempt,
        "artifact_name": artifact_name,
        "run_url": os.environ["RUN_URL"],
        "created_at": datetime.now(timezone.utc).isoformat(),
        "jobs": jobs,
        "required_jobs": list(REQUIRED_JOBS),
    }

    destination = (
        Path(os.environ.get("RUNNER_TEMP", "/tmp"))
        / "gitrun-verification-attestation.json"
    )
    destination.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    print(f"Created attestation for {len(jobs)} CI jobs at {destination}")
    for job_id, details in jobs.items():
        print(f"CI job {job_id}: {details['result']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
