#!/usr/bin/env python3
from __future__ import annotations
import json, logging, os, re, signal, subprocess, sys, threading, time, urllib.error, urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from uuid import uuid4

API = "https://api.github.com"
API_VERSION = "2026-03-10"
logging.basicConfig(level=os.getenv("GITRUN_LOG_LEVEL","INFO"), format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("gitrun")

@dataclass(frozen=True)
class RepoConfig:
    full_name: str
    minimum: int
    maximum: int

def env_bool(name: str, default: bool = False) -> bool:
    value = os.getenv(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}

def env_int(name: str, default: int) -> int:
    try: return int(os.getenv(name, str(default)))
    except ValueError: return default

def repositories() -> list[RepoConfig]:
    raw = os.getenv("GITRUN_REPOSITORIES","").strip() or os.getenv("GITRUN_DEFAULT_REPOSITORY","").strip()
    minimum, maximum = env_int("GITRUN_MIN_RUNNERS",3), env_int("GITRUN_MAX_RUNNERS",8)
    return [RepoConfig(r.strip(),minimum,maximum) for r in raw.split(",") if re.fullmatch(r"[^/]+/[^/]+",r.strip())]

def token() -> str:
    value=os.getenv("GITHUB_TOKEN","").strip()
    if not value: raise RuntimeError("GITHUB_TOKEN is not configured")
    return value

def api_request(method: str, path: str, body: dict|None=None) -> dict:
    data=None if body is None else json.dumps(body).encode()
    req=urllib.request.Request(API+path,data=data,method=method,headers={"Accept":"application/vnd.github+json","Authorization":f"Bearer {token()}","X-GitHub-Api-Version":API_VERSION,"User-Agent":"GitRun/0.2.0","Content-Type":"application/json"})
    try:
        with urllib.request.urlopen(req,timeout=20) as response:
            raw=response.read().decode("utf-8")
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as exc:
        detail=exc.read().decode("utf-8",errors="replace")[:500]
        raise RuntimeError(f"GitHub API {exc.code}: {detail}") from exc

def split_repo(repo:str)->tuple[str,str]: return repo.split("/",1)
def registration_token(repo:str)->str:
    o,n=split_repo(repo); return api_request("POST",f"/repos/{o}/{n}/actions/runners/registration-token")["token"]
def list_runners(repo:str)->list[dict]:
    o,n=split_repo(repo); return api_request("GET",f"/repos/{o}/{n}/actions/runners?per_page=100").get("runners",[])
def queued_jobs(repo:str)->int:
    o,n=split_repo(repo); runs=api_request("GET",f"/repos/{o}/{n}/actions/runs?status=queued&per_page=100").get("workflow_runs",[])
    count=0
    for run in runs:
        try:
            jobs=api_request("GET",f"/repos/{o}/{n}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100").get("jobs",[])
            count += sum(1 for j in jobs if j.get("status")=="queued" and "self-hosted" in {str(x).lower() for x in j.get("labels",[])})
        except Exception: log.exception("Unable to inspect queued jobs for %s run %s",repo,run.get("id"))
    return count
def docker(*args:str,check=True):
    return subprocess.run(["docker",*args],text=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=check)
def managed_containers(repo:str)->list[str]:
    r=docker("ps","-a","--filter","label=gitrun.runner=true","--filter",f"label=gitrun.repo={repo}","--format","{{.Names}}")
    return [x for x in r.stdout.splitlines() if x.strip()]
def container_status(name:str)->dict:
    r=docker("inspect","-f","{{json .State}}",name,check=False)
    try:return json.loads(r.stdout) if r.returncode==0 else {}
    except json.JSONDecodeError:return {}
def create_runner(repo:str, permanent: bool=False)->None:
    registration=registration_token(repo); safe=re.sub(r"[^a-zA-Z0-9_.-]","-",repo); name=f"gitrun-{safe}-{uuid4().hex[:8]}"
    image=os.getenv("GITRUN_RUNNER_IMAGE","gitrun-runner:latest")
    labels=os.getenv("GITRUN_RUNNER_LABELS","self-hosted,Linux,X64")
    cmd=["run","-d","--name",name,"--label","gitrun.runner=true","--label",f"gitrun.repo={repo}","--label","gitrun.managed=true",
         "--label",f"gitrun.permanent={str(permanent).lower()}","--label",f"gitrun.dynamic={str(not permanent).lower()}",
         "--cpus",os.getenv("GITRUN_CONTAINER_CPUS","1"),"--memory",os.getenv("GITRUN_CONTAINER_MEMORY","1g"),"--pids-limit",os.getenv("GITRUN_CONTAINER_PIDS","1024"),
         "--restart","unless-stopped","--read-only","--tmpfs","/tmp:rw,nosuid,nodev,size=256m",
         "-e",f"RUNNER_URL=https://github.com/{repo}","-e",f"RUNNER_TOKEN={registration}","-e",f"RUNNER_NAME={name}",
         "-e",f"RUNNER_LABELS={labels}","-e",f"RUNNER_EPHEMERAL={os.getenv('GITRUN_EPHEMERAL','false')}","-e",f"RUNNER_DISABLE_UPDATE={os.getenv('GITRUN_DISABLE_UPDATE','false')}",image]
    result=docker(*cmd,check=False)
    if result.returncode: raise RuntimeError(result.stderr.strip() or "docker run failed")
    log.info("Created %s runner %s for %s", "permanent" if permanent else "dynamic", name, repo)
def container_is_permanent(name: str) -> bool:
    result = docker("inspect", "-f", '{{index .Config.Labels "gitrun.dynamic"}}', name, check=False)
    return result.returncode == 0 and result.stdout.strip().lower() != "true"

def state_path()->Path:return Path(os.getenv("GITRUN_STATE_DIR","/var/lib/gitrun"))/"state.json"
def load_state()->dict:
    try:return json.loads(state_path().read_text(encoding="utf-8"))
    except (FileNotFoundError,json.JSONDecodeError):return {"idle_since":{}}
def save_state(state:dict)->None:
    p=state_path();p.parent.mkdir(parents=True,exist_ok=True);tmp=p.with_suffix(".tmp");tmp.write_text(json.dumps(state,indent=2),encoding="utf-8");os.replace(tmp,p)
def remove_runner(repo:str,name:str)->None:
    o,n=split_repo(repo)
    try:
        runner=next((x for x in list_runners(repo) if x.get("name")==name),None)
        if runner and runner.get("id"):
            try:api_request("DELETE",f"/repos/{o}/{n}/actions/runners/{runner['id']}")
            except RuntimeError as exc:
                if "GitHub API 404" not in str(exc):raise
    finally:
        r=docker("rm","-f",name,check=False)
        if r.returncode:log.warning("Could not remove runner %s: %s",name,r.stderr.strip())
def reconcile(cfg:RepoConfig)->None:
    repo=cfg.full_name;containers=managed_containers(repo);runners=list_runners(repo)
    online=[r for r in runners if r.get("status")=="online"];busy=[r for r in online if r.get("busy")];queued=queued_jobs(repo)
    desired=min(cfg.maximum,max(cfg.minimum,len(busy)+queued))
    for name in list(containers):
        if container_status(name).get("Status")=="exited":remove_runner(repo,name);containers.remove(name)
    current=len(containers)
    permanent_count=sum(1 for name in containers if container_is_permanent(name))
    while current < desired and permanent_count < cfg.minimum:
        create_runner(repo, permanent=True)
        current += 1
        permanent_count += 1
    while current < desired:
        create_runner(repo, permanent=False)
        current += 1
    state=load_state();idle=state.setdefault("idle_since",{});by_name={r.get("name"):r for r in runners};now=datetime.now(timezone.utc)
    for name in containers:
        runner=by_name.get(name)
        if runner and runner.get("busy"):idle.pop(name,None)
        elif runner and runner.get("status")=="online":idle.setdefault(name,now.isoformat())
    for name in list(idle):
        if name not in containers:idle.pop(name,None)
    if os.getenv("GITRUN_EPHEMERAL","false").lower()!="true" and current>cfg.minimum:
        removable=current-cfg.minimum;timeout=env_int("GITRUN_IDLE_TIMEOUT",120);candidates=[]
        for name,stamp in idle.items():
            if name not in containers:continue
            try:age=(now-datetime.fromisoformat(stamp)).total_seconds()
            except ValueError:continue
            if age>=timeout:candidates.append((name,age))
        ordered=sorted(candidates,key=lambda x:(container_is_permanent(x[0]),-x[1]))
        for name,_ in ordered[:removable]:
            remove_runner(repo,name)
            idle.pop(name,None)
    save_state(state)
def write_crash(exc:BaseException)->None:
    p=Path(os.getenv("GITRUN_STATE_DIR","/var/lib/gitrun"))/"last-crash";p.parent.mkdir(parents=True,exist_ok=True);p.write_text(f"{datetime.now(timezone.utc).isoformat()}\n{type(exc).__name__}: {exc}\n",encoding="utf-8")

def gtuu_schedule_loop()->None:
    last_run_date=None
    while not stopping:
        enabled=env_bool("GITRUN_AUTO_CONTAINER_UPDATE",False)
        schedule=os.getenv("GITRUN_CONTAINER_UPDATE_TIME","03:00").strip()
        now=datetime.now()
        if enabled and len(schedule)==5 and schedule[2]==":" and schedule == now.strftime("%H:%M") and now.date()!=last_run_date:
            utility=Path(__file__).with_name("gitrun_updater_utility.py")
            log.info("Starting GTUU scheduled permanent-runner update")
            result=subprocess.run([sys.executable,str(utility),"--only-containers"],check=False)
            if result.returncode==0:
                log.info("GTUU scheduled update completed")
            else:
                log.error("GTUU scheduled update failed with exit code %s",result.returncode)
            last_run_date=now.date()
        time.sleep(15)

stopping=False
def stop_handler(_signum,_frame):global stopping;stopping=True
def main()->int:
    global stopping
    signal.signal(signal.SIGTERM,stop_handler);signal.signal(signal.SIGINT,stop_handler)
    interval=max(1,env_int("GITRUN_POLL_INTERVAL",5));repos=repositories()
    if not repos:log.error("No repositories configured.");return 2
    log.info("GitRun autoscaler starting")
    threading.Thread(target=gtuu_schedule_loop,name="gitrun-gtuu",daemon=True).start()
    while not stopping:
        for cfg in repos:
            try:reconcile(cfg)
            except Exception as exc:log.exception("Reconciliation failed for %s",cfg.full_name);write_crash(exc)
        for _ in range(interval):
            if stopping:break
            time.sleep(1)
    log.info("GitRun autoscaler stopped");return 0
if __name__=="__main__":sys.exit(main())
