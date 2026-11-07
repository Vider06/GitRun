#!/usr/bin/env python3
"""Render the GitRun CI verdict and update the pull-request summary."""
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

started = time.monotonic()
tmp = Path(os.environ.get("RUNNER_TEMP", "/tmp"))
steps = Path(os.environ.get("CI_STEPS", tmp / "ci-steps.tsv"))
jobs = [
("variables-verification","Repository CI variables","always",""),
("environment","Runner environment","always","variables-verification"),
("detect","Detect components","always","variables-verification"),
("cargo-lock","Cargo.lock validation","rust","detect environment"),
("python-quality","Python quality checks","python","cargo-lock"),
("ci-selftest","CI self-test","workflows","cargo-lock"),
("repository","Hygiene and syntax","always","cargo-lock"),
("workflows","Workflow lint and security","workflows","cargo-lock"),
("shell","Shell","shell","cargo-lock"),("powershell","PowerShell","powershell","cargo-lock"),
("frontend","JS, TS and CSS","node","cargo-lock"),("audit-node","npm audit","node","cargo-lock"),
("rust-lint","Rust lint","rust","cargo-lock"),("rust-test","Rust tests","rust","cargo-lock rust-lint"),
("rust-build","Rust release build","rust","cargo-lock rust-test"),("audit-rust","Cargo audit and deny","rust","cargo-lock"),
("security-hardening","Security hardening regression gate","rust","cargo-lock rust-lint"),
("gsr-watchdog","GSR watchdog lifecycle and failure detection","rust","cargo-lock rust-lint"),
("runtime-contracts","Runtime contracts and configuration integration","rust","cargo-lock rust-lint"),
("systemd-verify","systemd-analyze unit verification","rust","cargo-lock"),
("native-service-manager-contracts","OpenRC and runit service-manager contracts","native","cargo-lock"),
("platform-target-check","Server and updater Linux target matrix","rust","cargo-lock rust-build"),
("release-package-certification","Release Server package matrix","rust","cargo-lock rust-lint platform-target-check"),
("migration-package-certification","One-time legacy migration package","rust","cargo-lock rust-lint platform-target-check"),
("release-inputs-certification","Release metadata and shared inputs","rust","cargo-lock"),
("release-asset-assembly-certification","Final Release asset assembly and exact inventory","rust","release-inputs-certification release-package-certification migration-package-certification release-dashboard-package-certification release-dashboard-musl-build-certification release-dashboard-musl-package-certification formal-verification miri"),
("platform-compatibility","Linux platform and updater target contracts","rust","cargo-lock rust-lint"),
("multi-distro-contracts","Multi-distro portability contracts","rust","cargo-lock rust-lint"),
("release-contracts","Server, Desktop and migration release contracts","workflows","cargo-lock"),
("formal-verification","Rust source assurance and Kani formal verification","rust","cargo-lock rust-lint"),
("miri","Miri interpreter and pure updater regression","rust","cargo-lock rust-lint"),
("tauri","Tauri GUI","tauri","cargo-lock rust-lint frontend"),
("tauri-arm64","Tauri GUI native ARM64 build","tauri","cargo-lock rust-lint frontend"),
("release-dashboard-package-certification","Release Desktop GNU package matrix","tauri","tauri tauri-arm64"),
("release-dashboard-musl-build-certification","Release Desktop musl build matrix","tauri","cargo-lock rust-lint frontend"),
("release-dashboard-musl-package-certification","Release Desktop musl package matrix","tauri","release-dashboard-musl-build-certification"),
("tauri-recovery","Tauri GitRun Recovery GUI","tauri","cargo-lock rust-lint frontend"),
("docker","Docker","docker","cargo-lock")]
def output(job,key):
    return str((job.get("outputs") or {}).get(key) or "")
def fmt(value):
    try:
        n=max(0,int(float(value)))
        return str(n//60)+"m "+str(n%60).zfill(2)+"s" if n>=60 else str(n)+"s"
    except (ValueError,TypeError): return "—"
def make_report():
    needs=json.loads(os.environ.get("NEEDS","{}"))
    detected=needs.get("detect",{}).get("result")=="success"
    states={}; times={}; caches={}; reasons={}
    counts={"success":0,"failure":0,"cancelled":0,"blocked":0,"na":0}
    for jid,label,flag,deptext in jobs:
        item=needs.get(jid,{})
        result=item.get("result","skipped")
        times[jid]=output(item,"secs"); caches[jid]=output(item,"cache")
        active=flag=="always" or (detected and output(needs.get("cargo-lock",{}),flag)=="true") or (jid=="cargo-lock" and not detected)
        if jid=="native-service-manager-contracts":
            active=output(needs.get("cargo-lock",{}),"rust")=="true" or output(needs.get("cargo-lock",{}),"workflows")=="true"
        if result=="success": state="success"
        elif result=="failure": state="failure"
        elif result=="cancelled": state="cancelled"
        elif not active: state="na"
        else:
            blockers=[dep for dep in deptext.split() if needs.get(dep,{}).get("result")!="success"]
            state="blocked" if blockers else "failure"
            reasons[jid]=" ".join(blockers) if blockers else "skipped unexpectedly"
        states[jid]=state; counts[state]+=1
    verdict="failure" if counts["failure"] or not detected else ("cancelled" if counts["cancelled"] else "success")
    heading={"success":"All applicable gates passed","failure":"Verification failed","cancelled":"Run cancelled"}[verdict]
    lines=["## "+heading+" — GitRun CI","","### Release-readiness diagnostics","","| Release surface | Required CI gate(s) | Result |","|---|---|---|"]
    surfaces=[
    ("Server GNU/musl × x86_64/ARM64 packages","release-package-certification"),
    ("Release metadata, library archive, SBOM and provenance","release-inputs-certification"),
    ("Exact combined Release asset set and checksums","release-asset-assembly-certification"),
    ("Legacy migration DEB (1.9.1 only)","migration-package-certification"),
    ("Desktop GNU DEB/RPM/Arch × x86_64/ARM64","release-dashboard-package-certification"),
    ("Desktop musl build × x86_64/ARM64","release-dashboard-musl-build-certification"),
    ("Desktop musl APK/XBPS","release-dashboard-musl-package-certification"),
    ("Rust per-file inventory + Kani","formal-verification"),("Targeted interpreter run","miri")]
    for label,jid in surfaces: lines.append("| "+label+" | "+jid+" | "+states.get(jid,"na").upper()+" |")
    lines += ["","Commit: "+os.environ.get("HEAD_SHA","")[:8]+" · ref: "+os.environ.get("REF_NAME","")+" · event: "+os.environ.get("EVENT","")+" · run #"+os.environ.get("ATTEMPT","")+" ("+os.environ.get("RUN_URL","")+")","",
    str(counts["success"])+" passed · "+str(counts["failure"])+" failed · "+str(counts["blocked"])+" blocked · "+str(counts["na"])+" not applicable · "+str(counts["cancelled"])+" cancelled","",chr(96)*3+"mermaid","flowchart LR"]
    for jid,label,_,deptext in jobs:
        node=jid.replace("-","_")
        lines.append("  "+node+'["'+label+"<br/>"+states[jid].upper()+" "+fmt(times[jid])+'"]:::'+states[jid])
    verdict_class={"success":"success","failure":"failure","cancelled":"cancelled"}[verdict]
    lines.append('  verdict{{"'+heading+'"}}:::'+verdict_class)
    for jid,_,_,deptext in jobs:
        node=jid.replace("-","_")
        for dep in deptext.split(): lines.append("  "+dep.replace("-","_")+" --> "+node)
        lines.append("  "+node+" --> verdict")
    lines += ["  classDef success fill:#dafbe1,stroke:#1a7f37,color:#0a3622","  classDef failure fill:#ffebe9,stroke:#cf222e,color:#82071e","  classDef na fill:#f6f8fa,stroke:#8c959f,color:#57606a","  classDef blocked fill:#fff8c5,stroke:#bf8700,color:#633c01","  classDef cancelled fill:#eaeef2,stroke:#57606a,color:#24292f",chr(96)*3]
    lines += ["","| Status | Job | Time | Depends on | Cache | Note |","|---|---|---|---|---|---|"]
    for jid,label,_,deptext in jobs:
        value=states[jid]
        note=("blocked by "+reasons[jid]) if value=="blocked" else ("not applicable" if value=="na" else reasons.get(jid,""))
        if jid=="platform-target-check": note=(note+"; " if note else "")+"each target publishes an independent job report"
        if jid=="migration-package-certification" and output(needs.get(jid,{}),"migration_release")!="true":
            note=(note+"; " if note else "")+"not applicable: not an enabled 1.9.1 migration release"
        lines.append("| "+value.upper()+" | "+jid+" — "+label+" | "+fmt(times[jid])+" | "+(deptext.replace(" ",", ") or "—")+" | "+(caches[jid] or "—")+" | "+note+" |")
    if verdict=="failure":
        lines += ["","### Errors",""]
        for jid,label,_,_ in jobs:
            if states[jid]=="failure":
                lines += ["<details open><summary>"+label+" ("+jid+")</summary>","",output(needs.get(jid,{}),"errors") or "No error excerpt was captured: open the job log.","</details>",""]
        if not detected: lines += ["> Component detection did not succeed; applicability could not be determined reliably.",""]
    return "\n".join(lines)+"\n",verdict
def update_comment(body):
    repo=os.environ["GITHUB_REPOSITORY"]; pr=os.environ["PR_NUMBER"]
    base="https://api.github.com/repos/"+repo
    headers={"Authorization":"Bearer "+os.environ["GH_TOKEN"],"Accept":"application/vnd.github+json","X-GitHub-Api-Version":"2022-11-28","Content-Type":"application/json"}
    marker="<!-- gitrun-ci-summary -->"; comment_id=None
    for page in range(1,6):
        req=urllib.request.Request(base+"/issues/"+pr+"/comments?per_page=100&page="+str(page),headers=headers)
        with urllib.request.urlopen(req,timeout=20) as response: items=json.load(response)
        found=next((x for x in items if marker in x.get("body","")),None)
        if found: comment_id=found["id"]; break
        if len(items)<100: break
    url=base+"/issues/comments/"+str(comment_id) if comment_id else base+"/issues/"+pr+"/comments"
    payload=json.dumps({"body":marker+"\n"+body[:60000]}).encode()
    req=urllib.request.Request(url,data=payload,headers=headers,method="PATCH" if comment_id else "POST")
    with urllib.request.urlopen(req,timeout=20) as response: response.read()
    print("PR summary comment updated" if comment_id else "PR summary comment created")
def main():
    body,verdict=make_report()
    (tmp/"report.md").write_text(body,encoding="utf-8")
    summary=os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary,"a",encoding="utf-8") as stream: stream.write(body)
    print(body,end="")
    if os.environ.get("EVENT")=="pull_request" and os.environ.get("PR_NUMBER") and verdict!="cancelled":
        try: update_comment(body)
        except (OSError,KeyError,urllib.error.URLError) as exc: print("Warning: PR comment could not be posted: "+str(exc),file=sys.stderr)
    if verdict=="cancelled": print("Run cancelled; existing PR comment left untouched")
    if verdict=="failure": raise SystemExit("CI release-readiness verdict: failure")
    print("Verdict: "+verdict)
if __name__=="__main__":
    status=0
    try: main()
    except SystemExit as exc: status=int(exc.code) if isinstance(exc.code,int) else 1
    except Exception:
        status=1
        raise
    finally:
        steps.parent.mkdir(parents=True,exist_ok=True)
        with steps.open("a",encoding="utf-8") as stream:
            stream.write("Build workflow verdict and pipeline report\t"+("ok" if status==0 else "fail")+"\t"+str(int(time.monotonic()-started))+"\n")
    raise SystemExit(status)
