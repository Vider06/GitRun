import { invoke, content, esc, heading, panel, pill, empty, errorView, toast, confirmModal } from "../lib.js";

const apiOps = {
  GitVaultRun: ["Read","Write","Exists","Delete","List"],
  GitDockRun: ["Connect","Disconnect","Read","Write","Execute","Melt"],
  GitSaveRun: ["File","Logs"],
  GitRegisterRun: ["Register"],
  GitInstallRun: ["Install","Remove","Update"],
  GitReadRun: ["Read"], GitWriteRun: ["Write"], GitVerifyRun: ["Verify"], GitStatusRun: ["Status"]
};

export async function renderSecurity() {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Inspecting configured security boundaries…</p></div>';
  let config, settings, summary;
  try {
    [config, settings, summary] = await Promise.all([invoke("get_config"), invoke("get_gitrun_settings"), invoke("get_security_runtime_summary")]);
  } catch (error) { if (generation !== window.__gitrunNavigationGeneration) return; content.innerHTML = heading("TRUST BOUNDARIES","Privacy & Security","Security configuration and runtime confidence.") + errorView(error); return; }
  if (generation !== window.__gitrunNavigationGeneration) return;
  const repoNames = config.repositories || [];
  const repoSettings = settings.repositories || {};
  const socketRepos = repoNames.filter((repo) => Boolean(repoSettings[repo] && repoSettings[repo].docker && repoSettings[repo].docker.direct_socket_enabled));
  const mountRuleCount = Object.values(repoSettings).reduce((sum, item) => sum + (((item.docker || {}).allowed_mounts || {}).rules || []).length, 0);
  content.innerHTML = heading("TRUST BOUNDARIES","Privacy & Security","Understand which boundaries are configured and which still require runtime verification.", '<button class="btn" data-action="refresh-view">↻ Re-check</button>') +
    '<div class="notice warn"><div><strong>Configuration is not proof of enforcement</strong>The current backend mostly reads saved policy. The dashboard marks these controls as configured and does not claim the container runtime has verified them.</div></div>' +
    '<div class="section"><div class="section-heading"><h2>Execution risk map</h2><span class="muted">Configuration-derived · runtime verification pending</span></div><div class="risk-map">' +
      '<div class="risk-node ' + (repoNames.length ? "configured" : "unknown") + '"><strong>Workflow / repository</strong>' + pill(repoNames.length + " configured repos","info") + '<p>Global API allowlists and workflow validation: ' + (config.gsr_workflow_validation_enabled ? "enabled in config" : "disabled in config") + '. Repo-specific policy overrides can only narrow global access.</p></div><div class="risk-link">→</div>' +
      '<div class="risk-node ' + (summary.runner_network_is_dedicated && config.runner_rootfs_read_only ? "configured" : "unknown") + '"><strong>Runner container</strong>' + pill(summary.runner_network_is_dedicated ? "Dedicated network configured" : "Network boundary needs review",summary.runner_network_is_dedicated ? "good" : "warn") + '<p>Network: ' + esc(summary.runner_network || "unknown") + '<br>Root filesystem: ' + (config.runner_rootfs_read_only ? "read-only configured" : "writable configured") + '<br>seccomp: ' + esc(summary.seccomp_profile || "unknown") + '<br>AppArmor: ' + esc(summary.apparmor_profile || "default") + '<br>Windows Hyper-V requirement: ' + (config.runner_windows_hyperv_isolation ? "enabled" : "disabled") + '</p></div><div class="risk-link">→</div>' +
      '<div class="risk-node ' + (config.gsr_docker_socket_hardening && socketRepos.length === 0 ? "configured" : "unknown") + '"><strong>Host / Docker daemon</strong>' + pill(socketRepos.length ? socketRepos.length + " repo(s) expose socket" : config.gsr_docker_socket_hardening ? "Hardening configured" : "Hardening disabled",socketRepos.length || !config.gsr_docker_socket_hardening ? "bad" : "warn") + '<p>Direct socket exceptions: ' + socketRepos.length + ' · mount allowlist rules: ' + mountRuleCount + '<br>Cache scope: ' + esc(summary.shared_cache_scope || "unknown") + '<br>Resource pressure: ' + (config.resource_pressure_enabled ? "enabled" : "disabled") + '<br>Host/container enforcement still needs runtime verification.</p></div>' +
    '</div></div>' +
    '<div class="two-col section">' +
      panel("Global workflow API policy", '<p class="section-description">Global policy is the upper bound. Repository overrides may restrict capabilities further. Unchecked operations are denied by this global policy.</p><div class="card-list">' + Object.keys(apiOps).map((api) => { const p = settings.global && settings.global.apis && settings.global.apis[api] || {enabled:false,allowed_operations:[]}; return '<div class="list-row"><div class="list-row-main"><strong class="mono">' + esc(api) + '</strong><div class="chip-row">' + apiOps[api].map((op) => '<label class="checkbox-row"><input type="checkbox" data-global-op="' + esc(api + "|" + op) + '" ' + ((p.allowed_operations || []).includes(op) ? "checked" : "") + '><span>' + esc(op) + '</span></label>').join("") + '</div></div><label class="checkbox-row"><input type="checkbox" data-global-api="' + esc(api) + '" ' + (p.enabled ? "checked" : "") + '><span><strong>Enabled</strong></span></label></div>'; }).join("") + '</div><div class="toolbar" style="margin-top:12px"><span class="field-hint">Backend validation runs before persistence.</span><button class="btn btn-primary" id="save-api-policy">Save API policy</button></div>') +
      panel("Host protection posture", '<div class="kv-grid"><div class="kv"><small>Dedicated network</small><strong>' + (summary.runner_network_is_dedicated ? "Configured" : "Not dedicated") + '</strong></div><div class="kv"><small>Shared cache scope</small><strong>' + esc(summary.shared_cache_scope || "unknown") + '</strong></div><div class="kv"><small>Docker socket hardening</small><strong>' + (summary.docker_socket_hardening ? "Enabled in config" : "Disabled in config") + '</strong></div><div class="kv"><small>Resource pressure</small><strong>' + (summary.resource_pressure_enabled ? "Enabled" : "Disabled") + '</strong></div></div><p class="field-hint" style="margin-top:12px">Configured thresholds: CPU ' + esc(summary.resource_pressure_cpu_percent) + '% · memory ' + esc(summary.resource_pressure_memory_percent) + '% · disk ' + esc(summary.resource_pressure_disk_percent) + '%.</p>')
    + '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Repository boundaries</h2><span class="muted">' + repoNames.length + ' configured</span></div>' +
      (repoNames.length ? '<div class="card-list">' + repoNames.map((repo) => { const r = settings.repositories && settings.repositories[repo] || {}; const d = r.docker || {}; return '<div class="list-row"><div class="list-row-main"><strong>' + esc(repo) + '</strong><small>Direct socket: ' + (d.direct_socket_enabled ? "enabled — high risk" : "disabled") + ' · cache scope: ' + esc(summary.shared_cache_scope || "unknown") + '</small></div><button class="btn" data-open-repo="' + esc(repo) + '">Review repository</button></div>'; }).join("") + '</div>' : empty("No repositories configured","Connect a repository during setup.")) +
    '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Runner isolation and resource pressure</h2>' + pill("Global policy","info") + '</div><p class="section-description">These are configured values. They do not prove that a running container received every option.</p><div class="form-grid"><div class="field"><label for="sec-network">Runner Docker network</label><input id="sec-network" value="' + esc(config.runner_network) + '"></div><div class="field"><label for="sec-cache-scope">Shared cache scope</label><select id="sec-cache-scope"><option value="runner" ' + (config.shared_cache_scope==="runner"?"selected":"") + '>Runner isolated</option><option value="repository" ' + (config.shared_cache_scope==="repository"?"selected":"") + '>Repository</option><option value="global" ' + (config.shared_cache_scope==="global"?"selected":"") + '>Global (cross-repository)</option></select></div><div class="field"><label for="sec-seccomp">seccomp profile</label><input id="sec-seccomp" value="' + esc(config.runner_seccomp_profile) + '"></div><div class="field"><label for="sec-apparmor">AppArmor profile</label><input id="sec-apparmor" value="' + esc(config.runner_apparmor_profile) + '"></div></div><label class="checkbox-row"><input type="checkbox" id="sec-rootfs-readonly" ' + (config.runner_rootfs_read_only?"checked":"") + '><span><strong>Read-only runner root filesystem</strong>Writable locations must be explicitly mounted or provisioned.</span></label><label class="checkbox-row"><input type="checkbox" id="sec-hyperv-isolation" ' + (config.runner_windows_hyperv_isolation?"checked":"") + '><span><strong>Require Windows Hyper-V isolation</strong>Applies only where a Windows Docker daemon supports this isolation mode.</span></label><div class="section-heading" style="margin-top:20px"><h2>Resource pressure backpressure</h2></div><label class="checkbox-row"><input type="checkbox" id="sec-pressure-enabled" ' + (config.resource_pressure_enabled?"checked":"") + '><span><strong>Pause new runner creation under host pressure</strong>Configured thresholds are evaluated by the scheduler.</span></label><div class="form-grid"><div class="field"><label for="sec-pressure-cpu">CPU threshold (%)</label><input type="number" min="1" max="100" id="sec-pressure-cpu" value="' + esc(config.resource_pressure_cpu_percent) + '"></div><div class="field"><label for="sec-pressure-memory">Memory threshold (%)</label><input type="number" min="1" max="100" id="sec-pressure-memory" value="' + esc(config.resource_pressure_memory_percent) + '"></div><div class="field"><label for="sec-pressure-disk">Disk threshold (%)</label><input type="number" min="1" max="100" id="sec-pressure-disk" value="' + esc(config.resource_pressure_disk_percent) + '"></div><div class="field"><label for="sec-pressure-paths">Filesystems to monitor (semicolon-separated)</label><input id="sec-pressure-paths" value="' + esc(config.resource_pressure_paths) + '"></div></div><div class="section-heading" style="margin-top:20px"><h2>GSR command and workflow policy</h2></div><label class="checkbox-row"><input type="checkbox" id="sec-command-policy" ' + (config.gsr_command_policy_enabled?"checked":"") + '><span><strong>Enable command policy enforcement</strong>Master switch for baseline and custom command rules.</span></label><label class="checkbox-row"><input type="checkbox" id="sec-baseline-blacklist" ' + (config.gsr_command_baseline_blacklist_enabled?"checked":"") + '><span><strong>Baseline dangerous-command blacklist</strong>Use GitRun’s built-in patterns.</span></label><label class="checkbox-row"><input type="checkbox" id="sec-custom-blacklist-enabled" ' + (config.gsr_command_blacklist_enabled?"checked":"") + '><span><strong>Custom blacklist</strong></span></label><div class="field"><label for="sec-custom-blacklist">Blacklist substrings (comma-separated)</label><input id="sec-custom-blacklist" value="' + esc(config.gsr_command_blacklist) + '"></div><label class="checkbox-row"><input type="checkbox" id="sec-whitelist-enabled" ' + (config.gsr_command_whitelist_enabled?"checked":"") + '><span><strong>Strict command whitelist</strong>When enabled, only matching commands are allowed.</span></label><div class="field"><label for="sec-whitelist">Whitelist substrings (comma-separated)</label><input id="sec-whitelist" value="' + esc(config.gsr_command_whitelist) + '"></div><div class="form-grid"><div class="field"><label for="sec-violation-action">Violation action</label><select id="sec-violation-action"><option value="log_only" ' + (config.gsr_violation_action==="log_only"?"selected":"") + '>Log only</option><option value="kill" ' + (config.gsr_violation_action==="kill"?"selected":"") + '>Terminate job</option><option value="kill_and_ban" ' + (config.gsr_violation_action==="kill_and_ban"?"selected":"") + '>Terminate and ban runner</option></select></div><div class="field"><label>Workflow validation</label><label class="checkbox-row"><input type="checkbox" id="sec-workflow-validation" ' + (config.gsr_workflow_validation_enabled?"checked":"") + '><span>Validate workflow files before runner admission</span></label></div></div><div class="toolbar"><span class="field-hint">Config is validated before it is written.</span><button class="btn btn-primary" id="save-runtime-policy">Save runtime policy</button></div></div>' +
    '<div class="section panel panel-pad danger-zone"><div class="section-heading"><h2>Dangerous global controls</h2>' + pill("High impact","bad") + '</div><p class="section-description">Changes below affect every runner. Disabling socket hardening requires the independent unsafe-runner gate. Keep both safe defaults unless you understand the consequences.</p>' +
      '<label class="checkbox-row"><input type="checkbox" id="security-hardening" ' + (config.gsr_docker_socket_hardening ? "checked" : "") + '><span><strong>GSR Docker socket hardening</strong>Apply the configured hardening baseline to runners using the Docker socket.</span></label>' +
      '<label class="checkbox-row"><input type="checkbox" id="security-unsafe" ' + (config.gsr_allow_unsafe_runner ? "checked" : "") + '><span><strong>Allow unsafe runner configuration</strong>Required if socket hardening is disabled. This does not itself prove any runtime state.</span></label>' +
      '<div class="toolbar"><span class="field-hint">Changes are validated by the backend before persistence.</span><button class="btn btn-danger" id="save-danger-settings">Save high-impact settings</button></div></div>';
  Object.keys(apiOps).forEach((api) => {
    const enabled = content.querySelector('[data-global-api="' + CSS.escape(api) + '"]');
    const updateOperations = () => content.querySelectorAll('[data-global-op^="' + CSS.escape(api) + '|"]').forEach((operation) => {
      operation.disabled = !enabled.checked;
    });
    enabled.addEventListener("change", updateOperations);
    updateOperations();
  });
  content.querySelectorAll("[data-open-repo]").forEach((button) => button.addEventListener("click", () => window.dispatchEvent(new CustomEvent("gitrun:navigate",{detail:"repo:"+button.dataset.openRepo}))));
  document.getElementById("save-api-policy").addEventListener("click", async () => {
    const nextSettings = structuredClone(settings);
    nextSettings.global = nextSettings.global || { apis: {} };
    nextSettings.global.apis = nextSettings.global.apis || {};
    for (const api of Object.keys(apiOps)) {
      const enabled = document.querySelector('[data-global-api="' + CSS.escape(api) + '"]').checked;
      const operations = Array.from(document.querySelectorAll('[data-global-op^="' + CSS.escape(api) + '|"]')).filter((item) => item.checked).map((item) => item.dataset.globalOp.split("|")[1]);
      nextSettings.global.apis[api] = { enabled, allowed_operations: operations };
    }
    const socketConfigured = Object.entries(nextSettings.repositories || {}).some(([name, item]) => item.docker && item.docker.direct_socket_enabled && !(settings.repositories && settings.repositories[name] && settings.repositories[name].docker && settings.repositories[name].docker.direct_socket_enabled));
    let confirmSocketOptOut = false;
    if (socketConfigured) {
      confirmSocketOptOut = await confirmModal("Save policy with direct socket access configured?", "At least one repository has direct Docker socket access enabled. This confirmation is required by the backend danger gate.", "Confirm and save", true);
      if (!confirmSocketOptOut) return;
    }
    try {
      await invoke("save_dashboard_settings", { updated: config, settings: nextSettings, confirmSocketOptOut });
      toast("Global API policy saved.");
    } catch (error) { toast("Could not save API policy: " + error, "error"); }
  });
  document.getElementById("save-runtime-policy").addEventListener("click", async () => {
    const next = {
      ...config,
      runner_network: document.getElementById("sec-network").value.trim(),
      shared_cache_scope: document.getElementById("sec-cache-scope").value,
      runner_seccomp_profile: document.getElementById("sec-seccomp").value.trim(),
      runner_apparmor_profile: document.getElementById("sec-apparmor").value.trim(),
      runner_rootfs_read_only: document.getElementById("sec-rootfs-readonly").checked,
      runner_windows_hyperv_isolation: document.getElementById("sec-hyperv-isolation").checked,
      resource_pressure_enabled: document.getElementById("sec-pressure-enabled").checked,
      resource_pressure_cpu_percent: Number(document.getElementById("sec-pressure-cpu").value),
      resource_pressure_memory_percent: Number(document.getElementById("sec-pressure-memory").value),
      resource_pressure_disk_percent: Number(document.getElementById("sec-pressure-disk").value),
      resource_pressure_paths: document.getElementById("sec-pressure-paths").value.trim(),
      gsr_command_policy_enabled: document.getElementById("sec-command-policy").checked,
      gsr_command_baseline_blacklist_enabled: document.getElementById("sec-baseline-blacklist").checked,
      gsr_command_blacklist_enabled: document.getElementById("sec-custom-blacklist-enabled").checked,
      gsr_command_blacklist: document.getElementById("sec-custom-blacklist").value.trim(),
      gsr_command_whitelist_enabled: document.getElementById("sec-whitelist-enabled").checked,
      gsr_command_whitelist: document.getElementById("sec-whitelist").value.trim(),
      gsr_violation_action: document.getElementById("sec-violation-action").value,
      gsr_workflow_validation_enabled: document.getElementById("sec-workflow-validation").checked
    };
    try { await invoke("save_config",{updated:next}); toast("Runtime security policy saved. Active enforcement remains a separate verification."); }
    catch (error) { toast("Could not save runtime policy: " + error,"error"); }
  });
  document.getElementById("save-danger-settings").addEventListener("click", async () => {
    const next = {...config, gsr_docker_socket_hardening: document.getElementById("security-hardening").checked, gsr_allow_unsafe_runner: document.getElementById("security-unsafe").checked};
    if (config.gsr_docker_socket_hardening && !next.gsr_docker_socket_hardening) {
      const ok = await confirmModal("Disable Docker socket hardening?", "This weakens the host boundary for socket-enabled runners. The unsafe-runner gate must also be enabled. Continue only if you accept this risk.", "Disable hardening", true);
      if (!ok) return;
    }
    try { await invoke("save_config", {updated: next}); toast("Security configuration saved. Runtime enforcement remains unverified."); }
    catch (error) { toast("Save failed: " + error, "error"); }
  });
}
