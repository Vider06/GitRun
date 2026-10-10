import { invoke, content, esc, heading, panel, pill, empty, errorView, toast, confirmModal } from "../lib.js";

const apiOps = {
  GitVaultRun:["Read","Write","Exists","Delete","List"],
  GitDockRun:["Connect","Disconnect","Read","Write","Execute","Melt"],
  GitSaveRun:["File","Logs"], GitRegisterRun:["Register"],
  GitInstallRun:["Install","Remove","Update"], GitReadRun:["Read"],
  GitWriteRun:["Write"], GitVerifyRun:["Verify"], GitStatusRun:["Status"]
};
const logicOps = ["connect","read","write","execute","melt","mountable"];

export async function renderRepository(repo, navigate) {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Loading repository configuration…</p></div>';
  let detail, requirements, settings, config, runnerSnapshot, activitySnapshot;
  const forceRefresh = Boolean(window.__gitrunForceRefresh);
  try {
    [detail, requirements, settings, config, runnerSnapshot, activitySnapshot] = await Promise.all([
      invoke("get_repo_detail", {repo}),
      invoke("get_dock_requirements", {repo}),
      invoke("get_gitrun_settings"),
      invoke("get_config"),
      invoke("get_runner_snapshot", {forceRefresh}).catch((error) => ({checked_at:0,stale:true,complete:false,repositories_checked:0,repositories_total:0,workflow_repositories_checked:0,workflow_runs:[],runners:[],warnings:[String(error)]})),
      invoke("get_repository_activity", {forceRefresh}).catch((error) => ({checked_at:0,stale:true,complete:false,repositories_checked:0,repositories_total:0,repositories:[],warnings:[String(error)]}))
    ]);
  } catch (error) {
    if (generation !== window.__gitrunNavigationGeneration) return;
    content.innerHTML = errorView(error);
    return;
  }
  if (generation !== window.__gitrunNavigationGeneration) return;
  const repoSettings = (settings.repositories || {})[repo] || {};
  const docker = repoSettings.docker || detail.docker_policy || {};
  const logic = docker.logic_containers || {};
  const mounts = docker.allowed_mounts || {enabled:false,rules:[]};
  const overrides = repoSettings.api_overrides || {};
  const ruleList = detail.logic_rules || [];
  const repoRunners = (runnerSnapshot.runners || []).filter((runner) => runner.repository === repo);
  const repoActivity = (activitySnapshot.repositories || []).find((item) => item.repository === repo);
  const workflowSummary = (runnerSnapshot.workflow_runs || []).find((item) => item.repository === repo);
  const runnerRows = repoRunners.map((runner) => '<tr><td><strong>' + esc(runner.name) + '</strong><div class="subtle mono">ID ' + esc(runner.id) + '</div></td><td>' + pill(runner.online ? "Online" : runner.status, runner.online ? "good" : "bad") + '</td><td>' + pill(runner.busy ? "Busy" : "Idle", runner.busy ? "warn" : "neutral") + '</td><td>' + pill(runner.kind, runner.kind === "Permanent" ? "info" : runner.kind === "Dynamic" ? "good" : "warn") + '</td><td>' + esc(runner.container_status || "Unknown") + '</td><td>' + (runner.kind === "Permanent" ? '<button class="icon-action" data-runner-settings="' + esc(runner.name) + '" title="Permanent runner settings" aria-label="Permanent runner settings">⚙</button>' : '<span class="subtle">—</span>') + '</td></tr>').join("");
  const logicPolicyCards = Object.entries(logic).map(([name, policy]) => logicCard(name, policy)).join("");
  const apiRows = Object.keys(apiOps).map((api) => {
    const override = overrides[api] || {enabled:null,allowed_operations:null};
    const globalPolicy = settings.global && settings.global.apis && settings.global.apis[api] || {enabled:false,allowed_operations:[]};
    const enabled = override.enabled === true ? "allow" : override.enabled === false ? "deny" : "inherit";
    const operationMode = override.allowed_operations == null ? "inherit" : "custom";
    const selected = operationMode === "inherit" ? (globalPolicy.allowed_operations || []) : override.allowed_operations;
    return '<tr><td class="mono">' + esc(api) + '</td><td><select data-api-enabled="' + esc(api) + '"><option value="inherit" ' + (enabled === "inherit" ? "selected" : "") + '>Inherit</option><option value="allow" ' + (enabled === "allow" ? "selected" : "") + '>Allow</option><option value="deny" ' + (enabled === "deny" ? "selected" : "") + '>Deny</option></select></td><td><div class="field"><select data-api-op-mode="' + esc(api) + '"><option value="inherit" ' + (operationMode === "inherit" ? "selected" : "") + '>Inherit global operations</option><option value="custom" ' + (operationMode === "custom" ? "selected" : "") + '>Use explicit operation allowlist</option></select></div><div class="chip-row">' + apiOps[api].map((op) => '<label class="checkbox-row"><input type="checkbox" data-api-op="' + esc(api + "|" + op) + '" ' + (selected.includes(op) ? "checked" : "") + '><span>' + esc(op) + '</span></label>').join("") + '</div></td></tr>';
  }).join("");
  const mountRows = (mounts.rules || []).map((rule, index) => mountCard(rule, index)).join("");
  content.innerHTML = heading("REPOSITORY", repo, "Repository operations, execution boundaries and repository-specific policy.", '<button class="btn" data-view="overview">← Overview</button><button class="btn btn-primary" id="save-repo-policy">Save changes</button>') +
    '<div class="grid metrics-grid">' +
      '<article class="panel metric-card"><div class="metric-top">GitVault groups</div><div class="metric-value">' + (detail.vault_groups || []).length + '</div><div class="metric-foot">Configured secret scopes</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Logic rules</div><div class="metric-value">' + ruleList.length + '</div><div class="metric-foot">Global dynamic-runner matching</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Workflow requirements</div><div class="metric-value">' + (requirements || []).length + '</div><div class="metric-foot">Detected GitDockRun calls</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Live runners</div><div class="metric-value">' + (repoRunners.length || (runnerSnapshot.repositories_checked === runnerSnapshot.repositories_total && runnerSnapshot.repositories_total > 0 ? "0" : "Unknown")) + '</div><div class="metric-foot">' + (runnerSnapshot.stale ? "Cached snapshot" : "GitHub API snapshot") + '</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Stars</div><div class="metric-value">' + (repoActivity ? esc(repoActivity.stars) : "Unknown") + '</div><div class="metric-foot">Repository metadata</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Open PRs</div><div class="metric-value">' + (repoActivity ? esc(repoActivity.open_pull_requests) : "Unknown") + '</div><div class="metric-foot">GitHub Search API</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Merged PRs</div><div class="metric-value">' + (repoActivity ? esc(repoActivity.merged_pull_requests) : "Unknown") + '</div><div class="metric-foot">GitHub Search API count · may be capped</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Queued workflow runs</div><div class="metric-value">' + (workflowSummary ? esc(workflowSummary.queued) : "Unknown") + '</div><div class="metric-foot">Latest 100 workflow runs</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">In-progress runs</div><div class="metric-value">' + (workflowSummary ? esc(workflowSummary.in_progress) : "Unknown") + '</div><div class="metric-foot">Workflow-run count, not job count</div></article>' +
      '<article class="panel metric-card"><div class="metric-top">Failed runs</div><div class="metric-value">' + (workflowSummary ? esc(workflowSummary.failed) : "Unknown") + '</div><div class="metric-foot">Latest 100 workflow runs</div></article>' +
    '</div>' +
    '<div class="notice warn"><div><strong>Job history is not exposed yet</strong>Runner status and busy state come from a cached GitHub snapshot. Job queues, run history and per-job resource usage require additional telemetry sources.</div></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Runner inventory</h2><span class="muted">' + (runnerSnapshot.repositories_checked === runnerSnapshot.repositories_total && runnerSnapshot.repositories_total > 0 ? repoRunners.length + ' observed' : 'Partial / unavailable') + '</span></div>' +
      (runnerRows ? '<div class="table-wrap"><table><thead><tr><th>Runner</th><th>Status</th><th>Load</th><th>Type</th><th>Container</th><th>Settings</th></tr></thead><tbody>' + runnerRows + '</tbody></table></div>' : empty("Runner status unavailable","GitHub API authentication or runner discovery may have failed. An empty partial snapshot is not a zero count.")) +
      ((runnerSnapshot.warnings || []).length ? '<div class="notice warn" style="margin-top:12px"><div><strong>Snapshot warnings</strong>' + runnerSnapshot.warnings.map((warning) => {
        const text = String(warning);
        const missingRunnerPermission = text.includes("Resource not accessible by integration") && text.includes("/actions/runners");
        const permissionHint = missingRunnerPermission
          ? '<p>Permission fix: this endpoint requires repository <strong>Administration: read</strong> for a GitHub App or fine-grained token. A classic PAT needs the <code>repo</code> scope and repository-admin access. After changing GitHub App permissions, update or reinstall the repository installation. See <a href="https://docs.github.com/en/rest/actions/self-hosted-runners#list-self-hosted-runners-for-a-repository" target="_blank" rel="noreferrer">GitHub runner API permissions</a>.</p>'
          : '';
        return '<p>' + esc(warning) + '</p>' + permissionHint;
      }).join("") + '</div></div>' : '') +
      '<p class="field-hint" style="margin-top:10px">Snapshot: ' + (runnerSnapshot.checked_at ? new Date(Number(runnerSnapshot.checked_at)*1000).toLocaleTimeString() : "not checked") + (runnerSnapshot.stale ? ' · stale cached data' : ' · shared 60-second cache') + '</p></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Logic Containers matching rules</h2><button class="btn" id="add-rule-btn">＋ Add rule</button></div><p class="section-description">These rules are currently global and select the backend and image for matching dynamic jobs.</p>' +
      (ruleList.length ? '<div class="card-list">' + ruleList.map((rule,index) => '<div class="list-row"><div class="list-row-main"><strong>' + esc(rule.name) + '</strong><small>Labels: ' + esc((rule.match_labels || []).join(", ")) + ' · Image: ' + esc(rule.image) + ' · Backend: ' + esc(rule.backend && rule.backend.Vm ? "VM " + rule.backend.Vm.vm_name : "Local Linux") + '</small></div><button class="btn btn-danger" data-delete-rule="' + index + '">Remove</button></div>').join("") + '</div>' : empty("No matching rules","Dynamic jobs use the default image until a rule matches.")) +
    '</div>' +
    '<div class="two-col section">' +
      panel("Docker execution boundary", '<p class="section-description">Direct socket access is a high-risk compatibility exception. GitDockRun should use its authorization boundary instead.</p><label class="checkbox-row"><input type="checkbox" id="repo-direct-socket" ' + (docker.direct_socket_enabled ? "checked" : "") + '><span><strong>Direct Docker socket access</strong>Allows workflows in this repository to reach the Docker daemon directly.</span></label><div class="field"><label for="repo-allowed-jobs">Allowed GitDockRun job names</label><input id="repo-allowed-jobs" value="' + esc((docker.allowed_job_names || []).join(", ")) + '" placeholder="* or specific job names"></div><div class="field"><label for="repo-allowed-containers">Allowed logical containers</label><input id="repo-allowed-containers" value="' + esc((docker.allowed_container_names || []).join(", ")) + '" placeholder="runner, build-cache"></div><label class="checkbox-row"><input type="checkbox" id="mount-enabled" ' + (mounts.enabled ? "checked" : "") + '><span><strong>Enable mount allowlist</strong>Only explicitly allowed host paths may be mounted.</span></label><div id="mount-rules-list" class="card-list">' + mountRows + '</div><button class="btn" id="add-mount-rule">＋ Add mount rule</button>') +
      panel("GitVault access scopes", '<div class="chip-row">' + ((detail.vault_groups || []).length ? detail.vault_groups.map((group) => pill(group,"info")).join("") : '<span class="muted">No GitVault groups configured</span>') + '</div><p class="section-description" style="margin-top:12px">Secret scope membership is shown here; plaintext secret values are never exposed.</p><div class="kv-grid"><div class="kv"><small>Repository scope</small><strong>' + esc(repo) + '</strong></div><div class="kv"><small>Global scope</small><strong>Shared only when explicitly authorized</strong></div></div>')
    + '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Workflow API overrides</h2><span class="pill info">Repository-scoped</span></div><p class="section-description">Inherit the global policy by default. An explicit deny can only reduce access; the effective backend policy remains authoritative.</p><div class="table-wrap"><table><thead><tr><th>API</th><th>Policy</th><th>Operation override</th></tr></thead><tbody>' + apiRows + '</tbody></table></div></div>' +
    '<div class="two-col section">' +
      panel("Logic Container capability policies", '<p class="section-description">Capabilities are independent. Execute and melt are elevated operations; grant only what the workflow needs.</p><div id="logic-policy-list" class="card-list">' + logicPolicyCards + '</div><button class="btn" id="add-logic-policy">＋ Add container policy</button>') +
      panel("GitDockRun workflow requirements", (requirements || []).length ? '<div class="table-wrap"><table><thead><tr><th>Workflow</th><th>Calling job</th><th>Target job</th><th>Operation</th></tr></thead><tbody>' + requirements.map((item) => '<tr><td class="mono">' + esc(item.file) + ':' + esc(item.line) + '</td><td>' + esc(item.calling_job || "—") + '</td><td class="mono">' + esc(item.target_job) + '</td><td>' + pill(item.operation,"info") + '</td></tr>').join("") + '</tbody></table></div>' : empty("No requirements detected","No GitDockRun job references were found in the stored report.")) +
    '</div>' +
    '<div class="toolbar section"><span class="field-hint" id="repo-save-status">Changes are not saved until you click Save changes.</span><button class="btn btn-primary" id="save-repo-policy-bottom">Save changes</button></div>';
  content.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.view)));
  document.getElementById("add-rule-btn").addEventListener("click", () => openRuleModal(ruleList, repo, navigate));
  content.querySelectorAll("[data-api-op-mode]").forEach((select) => {
    const api = select.dataset.apiOpMode;
    const updateMode = () => {
      const globalPolicy = settings.global && settings.global.apis && settings.global.apis[api] || {allowed_operations:[]};
      content.querySelectorAll("[data-api-op]").forEach((operation) => {
        if (!operation.dataset.apiOp.startsWith(api + "|")) return;
        operation.disabled = select.value === "inherit";
        if (select.value === "inherit") operation.checked = (globalPolicy.allowed_operations || []).includes(operation.dataset.apiOp.split("|")[1]);
      });
    };
    select.addEventListener("change", updateMode);
    updateMode();
  });
  content.querySelectorAll("[data-delete-rule]").forEach((button) => button.addEventListener("click", async () => {
    const index = Number(button.dataset.deleteRule);
    const rule = ruleList[index];
    if (!rule) return;
    const ok = await confirmModal("Remove Logic Containers rule?", "Remove rule “" + rule.name + "” from the global rule set? Other repositories may use it too.", "Remove rule", true);
    if (!ok) return;
    try { await invoke("save_logic_rules",{rules:ruleList.filter((_,i)=>i!==index)}); toast("Logic Containers rule removed."); await navigate("repo:"+repo); }
    catch (error) { toast("Could not remove rule: " + error,"error"); }
  }));
  document.getElementById("add-logic-policy").addEventListener("click", () => {
    const name = prompt("Logical container name:");
    if (!name || !name.trim()) return;
    const key = name.trim();
    if (logic[key]) { toast("A policy with that name already exists.","warning"); return; }
    const list = document.getElementById("logic-policy-list");
    list.insertAdjacentHTML("beforeend",logicCard(key,{connect:true,read:true,write:true,execute:false,melt:false,mountable:false,allowed_melt_targets:["runner"]}));
    const createdPolicy = list.lastElementChild;
    createdPolicy.querySelector("[data-remove-logic-policy]").addEventListener("click", () => createdPolicy.remove());
    logic[key] = {connect:true,read:true,write:true,execute:false,melt:false,mountable:false,allowed_melt_targets:["runner"]};
  });
  document.getElementById("add-mount-rule").addEventListener("click", () => {
    const list = document.getElementById("mount-rules-list");
    const index = list.querySelectorAll("[data-mount-index]").length;
    list.insertAdjacentHTML("beforeend",mountCard({source:"",recursive:false,read_only:true,allow:true},index));
    const createdMount = list.lastElementChild;
    createdMount.querySelector("[data-remove-mount]").addEventListener("click", () => createdMount.remove());
  });
  content.querySelectorAll("[data-remove-mount]").forEach((button) => button.addEventListener("click", () => button.closest("[data-mount-index]")?.remove()));
  content.querySelectorAll("[data-remove-logic-policy]").forEach((button) => button.addEventListener("click", () => button.closest("[data-logic-policy]")?.remove()));
  content.querySelectorAll("[data-runner-settings]").forEach((button) => button.addEventListener("click", () => {
    const runner = repoRunners.find((item) => item.name === button.dataset.runnerSettings);
    if (!runner) return;
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = '<section class="modal" role="dialog" aria-modal="true"><h2>Permanent runner settings</h2><p>Runner-specific override storage is not exposed by the current backend. The controls below show the observed runner identity and link to the global runner policy.</p><div class="kv-grid"><div class="kv"><small>Runner</small><strong>' + esc(runner.name) + '</strong></div><div class="kv"><small>Container state</small><strong>' + esc(runner.container_status || "Unknown") + '</strong></div><div class="kv"><small>Current state</small><strong>' + esc(runner.status) + (runner.busy ? " · busy" : " · idle") + '</strong></div><div class="kv"><small>Scope</small><strong>' + esc(repo) + '</strong></div></div><div class="modal-actions"><button class="btn" data-close>Close</button><button class="btn btn-primary" data-general>Global runner settings →</button></div></section>';
    document.body.appendChild(backdrop);
    backdrop.querySelector("[data-close]").onclick = () => backdrop.remove();
    backdrop.querySelector("[data-general]").onclick = () => { backdrop.remove(); window.dispatchEvent(new CustomEvent("gitrun:navigate",{detail:"general"})); };
  }));
  document.getElementById("save-repo-policy").addEventListener("click", savePolicy);
  document.getElementById("save-repo-policy-bottom").addEventListener("click", savePolicy);

  async function savePolicy() {
    const next = structuredClone(settings);
    next.repositories = next.repositories || {};
    const updatedRepo = next.repositories[repo] || {};
    updatedRepo.api_overrides = updatedRepo.api_overrides || {};
    updatedRepo.docker = updatedRepo.docker || {};
    updatedRepo.docker.direct_socket_enabled = document.getElementById("repo-direct-socket").checked;
    updatedRepo.docker.allowed_job_names = document.getElementById("repo-allowed-jobs").value.split(",").map((value)=>value.trim()).filter(Boolean);
    updatedRepo.docker.allowed_container_names = document.getElementById("repo-allowed-containers").value.split(",").map((value)=>value.trim()).filter(Boolean);
    updatedRepo.docker.logic_containers = {};
    content.querySelectorAll("[data-logic-policy]").forEach((card) => {
      const name = card.dataset.logicPolicy;
      const policy = {};
      logicOps.forEach((op) => { policy[op] = Boolean(card.querySelector('[data-logic-op="' + CSS.escape(op) + '"]')?.checked); });
      policy.allowed_melt_targets = (card.querySelector("[data-logic-targets]")?.value || "").split(",").map((value)=>value.trim()).filter(Boolean);
      updatedRepo.docker.logic_containers[name] = policy;
    });
    updatedRepo.docker.allowed_mounts = {
      enabled: document.getElementById("mount-enabled").checked,
      rules: Array.from(content.querySelectorAll("[data-mount-index]")).map((card) => ({
        source: card.querySelector("[data-mount-source]").value.trim(),
        recursive: card.querySelector("[data-mount-recursive]").checked,
        read_only: card.querySelector("[data-mount-readonly]").checked,
        allow: card.querySelector("[data-mount-allow]").checked
      }))
    };
    for (const api of Object.keys(apiOps)) {
      const value = content.querySelector('[data-api-enabled="' + CSS.escape(api) + '"]').value;
      const opMode = content.querySelector('[data-api-op-mode="' + CSS.escape(api) + '"]').value;
      const operations = Array.from(content.querySelectorAll("[data-api-op]")).filter((item) => item.dataset.apiOp.startsWith(api+"|") && item.checked).map((item)=>item.dataset.apiOp.split("|")[1]);
      updatedRepo.api_overrides[api] = {enabled:value==="inherit"?null:value==="allow",allowed_operations:opMode==="inherit"?null:operations};
    }
    next.repositories[repo] = updatedRepo;
    const enablingSocket = updatedRepo.docker.direct_socket_enabled && !(repoSettings.docker && repoSettings.docker.direct_socket_enabled);
    let confirmSocketOptOut = false;
    if (enablingSocket) {
      confirmSocketOptOut = await confirmModal("Enable direct Docker socket access?", "This gives workflows in " + repo + " direct Docker daemon access and can bypass isolation boundaries.", "Enable socket access", true);
      if (!confirmSocketOptOut) return;
    }
    const status = document.getElementById("repo-save-status");
    status.textContent = "Validating and saving…";
    try {
      await invoke("save_dashboard_settings",{updated:config,settings:next,confirmSocketOptOut});
      toast("Repository policy saved.");
      status.textContent = "Saved successfully · " + new Date().toLocaleTimeString();
    } catch (error) {
      status.textContent = "Save failed: " + error;
      toast("Could not save repository policy: " + error,"error");
    }
  }
}

function logicCard(name, policy) {
  return '<div class="panel panel-pad" data-logic-policy="' + esc(name) + '"><div class="section-heading"><strong class="mono">' + esc(name) + '</strong><button class="btn btn-danger" data-remove-logic-policy>Remove</button></div><div class="chip-row">' + logicOps.map((op) => '<label class="checkbox-row"><input type="checkbox" data-logic-op="' + op + '" ' + (policy[op] ? "checked" : "") + '><span>' + op + '</span></label>').join("") + '</div><div class="field" style="margin-top:10px"><label>Allowed melt targets</label><input data-logic-targets value="' + esc((policy.allowed_melt_targets || []).join(", ")) + '" placeholder="runner, build-cache"></div></div>';
}
function mountCard(rule,index) {
  return '<div class="panel panel-pad" data-mount-index="' + index + '"><div class="field"><label>Host source path</label><input data-mount-source value="' + esc(rule.source || "") + '" placeholder="/srv/build-cache"></div><div class="chip-row"><label class="checkbox-row"><input type="checkbox" data-mount-recursive ' + (rule.recursive ? "checked" : "") + '><span>Recursive</span></label><label class="checkbox-row"><input type="checkbox" data-mount-readonly ' + (rule.read_only ? "checked" : "") + '><span>Read-only</span></label><label class="checkbox-row"><input type="checkbox" data-mount-allow ' + (rule.allow ? "checked" : "") + '><span>Allow</span></label><button class="btn btn-danger" data-remove-mount>Remove</button></div></div>';
}

function openRuleModal(existingRules, repo, navigate) {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="logic-rule-title"><h2 id="logic-rule-title">New Logic Containers rule</h2><p>Rules are global: a label match can affect any configured repository that schedules a matching dynamic job.</p><div class="field"><label for="rule-name">Rule name</label><input id="rule-name" placeholder="windows-jobs"></div><div class="field"><label for="rule-labels">Match labels (all required)</label><input id="rule-labels" placeholder="windows, x64"></div><div class="field"><label for="rule-backend">Execution backend</label><select id="rule-backend"><option value="local">Local Linux host</option><option value="vm">VM by name</option></select></div><div class="field" id="rule-vm-wrap" hidden><label for="rule-vm-name">VM name</label><input id="rule-vm-name" placeholder="win-host"></div><div class="field"><label for="rule-image">Runner image</label><input id="rule-image" placeholder="gitrun-runner:latest"></div><div class="modal-actions"><button class="btn" data-cancel>Cancel</button><button class="btn btn-primary" data-save>Save rule</button></div></section>';
  document.body.appendChild(backdrop);
  const backend = backdrop.querySelector("#rule-backend");
  backend.addEventListener("change", () => { backdrop.querySelector("#rule-vm-wrap").hidden = backend.value !== "vm"; });
  backdrop.querySelector("[data-cancel]").addEventListener("click", () => backdrop.remove());
  backdrop.addEventListener("click", (event) => { if (event.target === backdrop) backdrop.remove(); });
  backdrop.querySelector("[data-save]").addEventListener("click", async () => {
    const name = backdrop.querySelector("#rule-name").value.trim();
    const labels = backdrop.querySelector("#rule-labels").value.split(",").map((value)=>value.trim()).filter(Boolean);
    const image = backdrop.querySelector("#rule-image").value.trim();
    const vmName = backdrop.querySelector("#rule-vm-name").value.trim();
    if (!name || !labels.length || !image || (backend.value === "vm" && !vmName)) { toast("Enter a rule name, at least one label, an image and a VM name when applicable.","warning"); return; }
    if (existingRules.some((rule)=>rule.name.toLowerCase()===name.toLowerCase())) { toast("A rule with this name already exists.","warning"); return; }
    const selectedBackend = backend.value === "vm" ? {Vm:{vm_name:vmName}} : "LocalLinux";
    try {
      await invoke("save_logic_rules",{rules:[...existingRules,{name,match_labels:labels,backend:selectedBackend,image}]});
      backdrop.remove();
      toast("Logic Containers rule saved.");
      await navigate("repo:"+repo);
    } catch (error) { toast("Could not save rule: "+error,"error"); }
  });
  backdrop.querySelector("#rule-name").focus();
}
