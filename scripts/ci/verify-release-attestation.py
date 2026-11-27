#!/usr/bin/env python3
"""Verify the successful GitRun CI attestation for the exact current main SHA."""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from io import BytesIO
from typing import Any

ARTIFACT_NAME = "gitrun-main-verification-attestation"
REQUIRED_JOBS = (
    "rust-source-inventory",
    "kani-formal-verification",
    "verus-formal-verification",
    "formal-verification",
    "miri",
    "summary",
)


class GateError(RuntimeError):
    """A release attestation check failed."""


def api_request(url: str, token: str) -> bytes:
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "GitRun-release-attestation",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:1500]
        raise GateError(
            f"GitHub API returned HTTP {error.code} for {url}: {detail}"
        ) from error
    except urllib.error.URLError as error:
        raise GateError(f"GitHub API request failed for {url}: {error}") from error


def api_json(url: str, token: str) -> dict[str, Any]:
    try:
        value = json.loads(api_request(url, token))
    except json.JSONDecodeError as error:
        raise GateError(f"GitHub API returned invalid JSON for {url}") from error
    if not isinstance(value, dict):
        raise GateError(f"GitHub API returned an unexpected response for {url}")
    return value


def write_summary(lines: list[str]) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    with open(summary_path, "a", encoding="utf-8") as summary_file:
        summary_file.write("# Release CI attestation gate\n\n")
        summary_file.write("\n".join(lines) + "\n")


def describe_jobs(jobs: list[dict[str, Any]]) -> list[str]:
    lines = []
    for job in jobs:
        name = job.get("name", f"job-{job.get('id', 'unknown')}")
        conclusion = job.get("conclusion")
        lines.append(
            f"- CI job **{name}**: status={job.get('status')}, "
            f"conclusion={conclusion}."
        )
        for step in job.get("steps", []):
            if step.get("conclusion") not in {"success", "skipped", None}:
                lines.append(
                    f"  - Failed step **{step.get('name', 'unnamed')}**: "
                    f"status={step.get('status')}, "
                    f"conclusion={step.get('conclusion')}."
                )
    return lines


def main() -> int:
    token = os.environ.get("GH_TOKEN", "")
    api_url = os.environ.get("GITHUB_API_URL", "https://api.github.com").rstrip("/")
    repository = os.environ["GITHUB_REPOSITORY"]
    release_sha = os.environ["RELEASE_SHA"].lower()
    release_ref = os.environ["RELEASE_REF"]
    event = os.environ["RELEASE_EVENT"]
    report: list[str] = []

    if not token:
        raise GateError("GH_TOKEN is missing; actions:read permission is required")
    if event == "workflow_dispatch" and release_ref != "refs/heads/main":
        raise GateError(
            f"manual release is allowed only from refs/heads/main, received {release_ref}"
        )
    if event == "push" and not release_ref.startswith("refs/tags/v"):
        raise GateError(
            f"automatic release requires a v*.*.* tag, received {release_ref}"
        )
    if event not in {"push", "workflow_dispatch"}:
        raise GateError(f"unsupported release event: {event}")

    repository_path = urllib.parse.quote(repository, safe="/")
    main_ref_url = f"{api_url}/repos/{repository_path}/git/ref/heads/main"
    main_sha = (
        api_json(main_ref_url, token).get("object", {}).get("sha", "").lower()
    )
    if not main_sha:
        raise GateError("could not resolve refs/heads/main from GitHub")
    if release_sha != main_sha:
        raise GateError(
            f"release commit {release_sha} is not current main HEAD {main_sha}; "
            "the release is blocked"
        )
    report.append(f"- Current main HEAD matches release SHA: `{release_sha}`.")

    workflow = api_json(
        f"{api_url}/repos/{repository_path}/actions/workflows/ci.yml", token
    )
    workflow_id = workflow.get("id")
    if not workflow_id:
        raise GateError("could not resolve the GitRun CI workflow ID")

    query = urllib.parse.urlencode(
        {
            "branch": "main",
            "event": "push",
            "head_sha": release_sha,
            "per_page": 100,
        }
    )
    runs_url = (
        f"{api_url}/repos/{repository_path}/actions/workflows/"
        f"{workflow_id}/runs?{query}"
    )
    deadline = time.monotonic() + 30 * 60
    selected_run: dict[str, Any] | None = None

    while time.monotonic() <= deadline:
        current_main_sha = (
            api_json(main_ref_url, token).get("object", {}).get("sha", "").lower()
        )
        if current_main_sha != release_sha:
            raise GateError(
                f"main moved while waiting for CI: expected {release_sha}, "
                f"now {current_main_sha}"
            )

        runs = api_json(runs_url, token).get("workflow_runs", [])
        candidates = [
            run
            for run in runs
            if run.get("head_sha", "").lower() == release_sha
            and run.get("head_branch") == "main"
            and run.get("event") == "push"
            and run.get("path") == ".github/workflows/ci.yml"
        ]
        if candidates:
            candidates.sort(
                key=lambda run: (
                    run.get("run_number", 0),
                    run.get("created_at", ""),
                ),
                reverse=True,
            )
            selected_run = candidates[0]
            if selected_run.get("status") == "completed":
                break
        time.sleep(20)

    if selected_run is None or selected_run.get("status") != "completed":
        raise GateError(
            f"no completed GitRun CI push run for main SHA {release_sha} "
            "appeared within 30 minutes"
        )

    run_id = int(selected_run["id"])
    run_url = selected_run.get("html_url", "")
    report.append(
        f"- Exact CI run: [#{selected_run.get('run_number')} / {run_id}]({run_url})."
    )
    jobs_url = (
        f"{api_url}/repos/{repository_path}/actions/runs/{run_id}/jobs?per_page=100"
    )
    jobs = api_json(jobs_url, token).get("jobs", [])
    failed_jobs = [
        job
        for job in jobs
        if job.get("conclusion") not in {"success", "skipped"}
    ]

    if selected_run.get("conclusion") != "success" or failed_jobs:
        report.append(
            f"- CI workflow conclusion: **{selected_run.get('conclusion')}**."
        )
        report.extend(describe_jobs(jobs))
        write_summary(report)
        failed_names = [
            job.get("name", str(job.get("id"))) for job in failed_jobs
        ]
        raise GateError(
            f"exact CI run #{run_id} did not pass; unsuccessful jobs: "
            + (", ".join(failed_names) if failed_names else "workflow-level failure")
        )

    producer = next(
        (
            job
            for job in jobs
            if job.get("name") == "Publish main verification attestation"
        ),
        None,
    )
    if producer is None or producer.get("conclusion") != "success":
        raise GateError(
            "CI passed but the 'Publish main verification attestation' job "
            "is missing or unsuccessful"
        )

    artifact_query = urllib.parse.urlencode(
        {"name": ARTIFACT_NAME, "per_page": 100}
    )
    artifacts_url = (
        f"{api_url}/repos/{repository_path}/actions/runs/{run_id}/artifacts?"
        f"{artifact_query}"
    )
    artifacts = api_json(artifacts_url, token).get("artifacts", [])
    matches = [
        artifact
        for artifact in artifacts
        if artifact.get("name") == ARTIFACT_NAME
        and not artifact.get("expired", False)
    ]
    if len(matches) != 1:
        raise GateError(
            f"expected exactly one unexpired '{ARTIFACT_NAME}' artifact "
            f"in CI run {run_id}; found {len(matches)}"
        )

    archive_url = matches[0].get("archive_download_url")
    if not archive_url:
        raise GateError("GitHub did not provide the artifact archive URL")
    try:
        with zipfile.ZipFile(BytesIO(api_request(archive_url, token))) as archive:
            json_files = [
                name
                for name in archive.namelist()
                if name.endswith("gitrun-verification-attestation.json")
            ]
            if len(json_files) != 1:
                raise GateError(
                    "attestation archive must contain exactly one JSON file; "
                    f"found {len(json_files)}"
                )
            attestation = json.loads(archive.read(json_files[0]))
    except (zipfile.BadZipFile, json.JSONDecodeError, KeyError) as error:
        raise GateError(f"could not decode the CI attestation archive: {error}") from error

    expected_fields = {
        "schema_version": 1,
        "repository": repository,
        "workflow_path": ".github/workflows/ci.yml",
        "event": "push",
        "branch": "main",
        "commit_sha": release_sha,
        "run_id": run_id,
    }
    for field, expected_value in expected_fields.items():
        actual_value = attestation.get(field)
        if actual_value != expected_value:
            raise GateError(
                f"attestation field {field!r} mismatch: "
                f"expected {expected_value!r}, got {actual_value!r}"
            )

    attested_jobs = attestation.get("jobs")
    if not isinstance(attested_jobs, dict) or not attested_jobs:
        raise GateError("attestation contains no per-job results")
    required_jobs = attestation.get("required_jobs")
    if not isinstance(required_jobs, list) or set(REQUIRED_JOBS) - set(required_jobs):
        raise GateError(
            "attestation required_jobs omits mandatory formal-verification jobs"
        )

    for job_id, details in sorted(attested_jobs.items()):
        if not isinstance(details, dict):
            raise GateError(f"attestation record for job {job_id!r} is malformed")
        result = details.get("result")
        report.append(f"- CI job `{job_id}`: **{result}**.")
        if result not in {"success", "skipped"}:
            raise GateError(
                f"CI job {job_id!r} has unsuccessful attested result {result!r}"
            )

    for job_id in REQUIRED_JOBS:
        details = attested_jobs.get(job_id)
        if not isinstance(details, dict) or details.get("result") != "success":
            raise GateError(
                f"mandatory verification job {job_id!r} did not succeed"
            )

    report.append("- CI job `verification-attestation`: **success**.")
    report.append(
        f"- Attestation artifact: `{ARTIFACT_NAME}` "
        f"(artifact ID {matches[0].get('id')})."
    )
    report.append(f"- Per-job records validated: **{len(attested_jobs)}**.")
    report.append(
        "- Kani, Verus, dual-verifier gate, Miri and CI summary all passed."
    )
    report.append("\n**Release gate passed.**")
    write_summary(report)
    print("\n".join(report))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except GateError as error:
        print(f"RELEASE BLOCKED: {error}", file=sys.stderr)
        write_summary([f"- **FAILED:** {error}"])
        sys.exit(1)
