#!/usr/bin/env python3
"""Verify the successful GitRun CI attestation for the exact current main SHA."""
from __future__ import annotations
import json, os, sys, time, urllib.error, urllib.parse, urllib.request, zipfile
from io import BytesIO

ARTIFACT = "gitrun-main-verification-attestation"
REQUIRED = ("rust-source-inventory", "kani-formal-verification", "verus-formal-verification",
            "formal-verification", "miri", "summary")
class GateError(RuntimeError): pass

def request(url: str, token: str, accept: str = "application/vnd.github+json") -> bytes:
    req = urllib.request.Request(url, headers={"Accept": accept, "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "GitRun-release-attestation"})
    try:
        with urllib.request.urlopen(req, timeout=60) as response: return response.read()
    except urllib.error.HTTPError as e:
        detail=e.read().decode("utf-8", "replace")[:1500]
        raise GateError(f"GitHub API HTTP {e.code} for {url}: {detail}") from e
    except urllib.error.URLError as e: raise GateError(f"GitHub API request failed for {url}: {e}") from e

def get_json(url: str, token: str) -> dict:
    try: value=json.loads(request(url, token))
    except json.JSONDecodeError as e: raise GateError(f"GitHub API returned invalid JSON for {url}") from e
    if not isinstance(value, dict): raise GateError(f"unexpected GitHub API response for {url}")
    return value

def summary(lines: list[str]) -> None:
    path=os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as out:
            out.write("# Release CI attestation gate\n\n" + "\n".join(lines) + "\n")

def main() -> int:
    token=os.environ.get("GH_TOKEN",""); api=os.environ.get("GITHUB_API_URL","https://api.github.com").rstrip("/")
    repo=os.environ["GITHUB_REPOSITORY"]; sha=os.environ["RELEASE_SHA"].lower()
    ref=os.environ["RELEASE_REF"]; event=os.environ["RELEASE_EVENT"]; report=[]
    if not token: raise GateError("GH_TOKEN is missing; actions:read permission is required")
    if event=="workflow_dispatch" and ref!="refs/heads/main":
        raise GateError(f"manual release is allowed only from refs/heads/main, received {ref}")
    if event=="push" and not ref.startswith("refs/tags/v"):
        raise GateError(f"automatic release requires a v*.*.* tag, received {ref}")
    if event not in {"push","workflow_dispatch"}: raise GateError(f"unsupported event: {event}")
    repo_path=urllib.parse.quote(repo,safe="/")
    ref_url=f"{api}/repos/{repo_path}/git/ref/heads/main"
    main_sha=get_json(ref_url,token).get("object",{}).get("sha","").lower()
    if not main_sha: raise GateError("could not resolve refs/heads/main")
    if sha!=main_sha: raise GateError(f"release SHA {sha} is not current main HEAD {main_sha}")
    report.append(f"- Current main HEAD matches release SHA: `{sha}`.")
    workflow=get_json(f"{api}/repos/{repo_path}/actions/workflows/ci.yml",token)
    workflow_id=workflow.get("id")
    if not workflow_id: raise GateError("could not resolve GitRun CI workflow ID")
    query=urllib.parse.urlencode({"branch":"main","event":"push","head_sha":sha,"per_page":100})
    runs_url=f"{api}/repos/{repo_path}/actions/workflows/{workflow_id}/runs?{query}"
    deadline=time.monotonic()+1800; selected=None
    while time.monotonic()<=deadline:
        current=get_json(ref_url,token).get("object",{}).get("sha","").lower()
        if current!=sha: raise GateError(f"main moved during gate: expected {sha}, now {current}")
        payload=get_json(runs_url,token)
        candidates=[r for r in payload.get("workflow_runs",[]) if r.get("head_sha","").lower()==sha
            and r.get("head_branch")=="main" and r.get("event")=="push"
            and r.get("path")==".github/workflows/ci.yml"]
        if candidates:
            candidates.sort(key=lambda r:(r.get("run_number",0),r.get("created_at","")),reverse=True)
            selected=candidates[0]
            if selected.get("status")=="completed": break
        time.sleep(20)
    if selected is None: raise GateError(f"no exact main CI run completed/appeared within 30 minutes for {sha}")
    run_id=int(selected["id"]); run_url=selected.get("html_url","")
    report.append(f"- Exact CI run: [#{selected.get('run_number')} / {run_id}]({run_url}).")
    jobs=get_json(f"{api}/repos/{repo_path}/actions/runs/{run_id}/jobs?per_page=100",token).get("jobs",[])
    failed=[]
    for job in jobs:
        if job.get("conclusion") not in {"success","skipped"}:
            failed.append(f"- **{job.get('name',job.get('id'))}**: status={job.get('status')}, conclusion={job.get('conclusion')}")
            for step in job.get("steps",[]):
                if step.get("conclusion") not in {"success","skipped",None}:
                    failed.append(f"  - Step **{step.get('name','unnamed')}**: {step.get('conclusion')}")
    if selected.get("conclusion")!="success" or failed:
        report.append(f"- CI conclusion: **{selected.get('conclusion')}**.")
        report.extend(failed or ["- No failed job detail was returned by GitHub; inspect the linked run."])
        summary(report)
        raise GateError(f"exact CI run #{run_id} did not pass; see failed job and step details above")
    producer=next((j for j in jobs if j.get("name")=="Publish main verification attestation"),None)
    if not producer or producer.get("conclusion")!="success":
        raise GateError("CI succeeded but the attestation producer job is missing or unsuccessful")
    aq=urllib.parse.urlencode({"name":ARTIFACT,"per_page":100})
    artifacts=get_json(f"{api}/repos/{repo_path}/actions/runs/{run_id}/artifacts?{aq}",token).get("artifacts",[])
    matches=[a for a in artifacts if a.get("name")==ARTIFACT and not a.get("expired",False)]
    if len(matches)!=1: raise GateError(f"expected exactly one unexpired {ARTIFACT} in run {run_id}; found {len(matches)}")
    url=matches[0].get("archive_download_url")
    if not url: raise GateError("GitHub did not provide the artifact download URL")
    try:
        with zipfile.ZipFile(BytesIO(request(url,token))) as archive:
            names=[n for n in archive.namelist() if n.endswith("gitrun-verification-attestation.json")]
            if len(names)!=1: raise GateError(f"artifact must contain exactly one attestation JSON; found {len(names)}")
            doc=json.loads(archive.read(names[0]))
    except (zipfile.BadZipFile,json.JSONDecodeError,KeyError) as e:
        raise GateError(f"cannot decode attestation artifact: {e}") from e
    expected={"schema_version":1,"repository":repo,"workflow_path":".github/workflows/ci.yml",
        "event":"push","branch":"main","commit_sha":sha,"run_id":run_id}
    for key,value in expected.items():
        if doc.get(key)!=value: raise GateError(f"attestation field {key!r} mismatch: expected {value!r}, got {doc.get(key)!r}")
    records=doc.get("jobs")
    if not isinstance(records,dict) or not records: raise GateError("attestation has no per-job records")
    required=doc.get("required_jobs")
    if not isinstance(required,list) or set(REQUIRED)-set(required):
        raise GateError("attestation required_jobs omits one or more mandatory formal-verification jobs")
    for name,record in sorted(records.items()):
        if not isinstance(record,dict): raise GateError(f"malformed per-job record: {name}")
        result=record.get("result"); report.append(f"- CI job `{name}`: **{result}**.")
        if result not in {"success","skipped"}: raise GateError(f"CI job {name!r} recorded unsuccessful result {result!r}")
    for name in REQUIRED:
        if not isinstance(records.get(name),dict) or records[name].get("result")!="success":
            raise GateError(f"mandatory job {name!r} did not succeed in attestation")
    report.extend([f"- Attestation: `{ARTIFACT}` (artifact ID {matches[0].get('id')}).",
        f"- Per-job records validated: **{len(records)}**.", "- Kani, Verus, dual-verifier gate, Miri and CI summary passed.",
        "\n**Release gate passed.**"])
    summary(report); print("\n".join(report)); return 0

if __name__=="__main__":
    try: sys.exit(main())
    except GateError as error:
        print(f"RELEASE BLOCKED: {error}",file=sys.stderr)
        summary([f"- **FAILED:** {error}"])
        sys.exit(1)
