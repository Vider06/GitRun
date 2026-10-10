import { invoke, content, esc, heading, panel, pill, toast, confirmModal } from "../lib.js";

export async function renderAppSettings() {
  const generation = window.__gitrunNavigationGeneration;
  const savedTheme = localStorage.getItem("gitrun-theme") || "dark";
  const catEnabled = localStorage.getItem("gitrun-cat-enabled") !== "false";
  const tutorialSeen = localStorage.getItem("gitrun-tutorial-complete") === "true";
  const compact = localStorage.getItem("gitrun-compact-mode") === "true";
  let serviceState = {service_installed:false,service_active:false,checks:[]};
  let serviceLoadError = null;
  try { serviceState = await invoke("get_dashboard_health"); } catch (error) { serviceLoadError = String(error); }
  if (generation !== window.__gitrunNavigationGeneration) return;
  content.innerHTML = heading("APPLICATION","App settings","Preferences for the dashboard itself. Runner and security configuration lives elsewhere.") +
    '<div class="two-col">' +
      panel("Appearance", '<div class="field"><label for="theme-select">Color theme</label><select id="theme-select"><option value="dark" ' + (savedTheme === "dark" ? "selected" : "") + '>Dark · GitRun default</option><option value="light" ' + (savedTheme === "light" ? "selected" : "") + '>Light</option><option value="system" ' + (savedTheme === "system" ? "selected" : "") + '>Follow system preference</option></select></div><label class="checkbox-row"><input id="compact-mode" type="checkbox" ' + (compact ? "checked" : "") + '><span><strong>Compact data density</strong>Reduce padding in tables and cards for information-heavy screens.</span></label><div class="toolbar"><span class="field-hint">Theme is stored locally on this device.</span><button class="btn btn-primary" id="save-appearance">Save appearance</button></div>') +
      panel("GitRun cat", '<div class="mascot-inline"><div class="mascot-face"><svg viewBox="0 0 48 48" aria-hidden="true"><path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><path d="M15 26h.1M33 26h.1" stroke="#11151d" stroke-width="5" stroke-linecap="round"/><path d="M20 33q4 4 8 0" fill="none" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/></svg></div><div class="mascot-copy"><strong>GitRun companion</strong><p>A guide to setup, status and recovery — not a substitute for actual telemetry.</p></div></div><label class="checkbox-row"><input id="cat-enabled" type="checkbox" ' + (catEnabled ? "checked" : "") + '><span><strong>Show the cat companion</strong>Show the mascot in the dashboard and onboarding.</span></label><div class="toolbar"><span class="field-hint">Expression states will be tied to verified app status.</span><button class="btn" id="save-cat">Save mascot preference</button></div>') +
    '</div>' +
    '<div class="section">' + panel("Guided tour", '<div class="list-row"><div class="list-row-main"><strong>Dashboard introduction</strong><small>' + (tutorialSeen ? "You have completed the current tour." : "The tour has not been completed on this device.") + '</small></div>' + pill(tutorialSeen ? "Completed" : "Not completed", tutorialSeen ? "good" : "info") + '</div><p class="section-description" style="margin-top:12px">The guided tour introduces Overview, repository policies, security boundaries, GitVault and Recovery. You can restart it at any time.</p><button class="btn btn-primary" id="restart-tour">Start guided tour</button>') + '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>GitRun startup and stop</h2>' + pill(serviceLoadError ? "Status unavailable" : serviceState.service_active ? "Service active" : serviceState.service_installed ? "Service stopped" : "Unit not found", serviceLoadError ? "bad" : serviceState.service_active ? "good" : "warn") + '</div><p class="section-description">These controls affect the host-level systemd service and scheduler, not just this dashboard window. Stopping or restarting it may interrupt runner reconciliation; existing workflow containers may need separate recovery.</p><div class="toolbar"><span class="field-hint">' + (serviceLoadError ? "Could not inspect service status: " + esc(serviceLoadError) : serviceState.service_installed ? "Unit is installed." : "No GitRun systemd unit was found.") + '</span><div class="heading-actions"><button class="btn btn-primary" id="service-start" ' + (serviceLoadError || !serviceState.service_installed || serviceState.service_active ? "disabled" : "") + '>Start service</button><button class="btn" id="service-restart" ' + (serviceLoadError || !serviceState.service_installed ? "disabled" : "") + '>Restart service</button><button class="btn btn-danger" id="service-stop" ' + (serviceLoadError || !serviceState.service_installed || !serviceState.service_active ? "disabled" : "") + '>Stop service</button></div></div><div id="service-operation-status" class="field-hint" style="margin-top:10px"></div></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Recommended security preset check</h2><span class="pill info">Read-only</span></div><p class="section-description">Checks whether the saved configuration aligns with GitRun’s recommended baseline. It does not change settings and does not prove runtime enforcement.</p><button class="btn btn-primary" id="run-security-preset-check">Run baseline check</button><div id="app-preset-results" style="margin-top:12px"></div></div>' +
    '<div class="section">' + panel("Support", '<div class="list-row"><div class="list-row-main"><strong>Open a GitRun issue</strong><small>Include the version and a concise reproduction. Never include tokens, private keys or secret values.</small></div><a class="btn" href="https://github.com/Vider06/GitRun/issues" target="_blank" rel="noreferrer">Open issues ↗</a></div>') + '</div>' +
    '<div class="section panel panel-pad danger-zone"><div class="section-heading"><h2>Installation management</h2>' + pill("Host-level actions","bad") + '</div><p class="section-description">Reinstall downloads and verifies the latest official release while preserving your configuration. Uninstall removes GitRun's package, services and owned runtime/app data. Shared Docker and unrelated host workloads remain untouched.</p><div class="toolbar"><button class="btn" id="reinstall-gitrun">Reinstall GitRun</button><button class="btn btn-danger" id="uninstall-gitrun">Uninstall GitRun</button></div><div id="installation-status" class="field-hint" style="margin-top:10px"></div></div>';
  document.getElementById("save-appearance").addEventListener("click", () => {
    const theme = document.getElementById("theme-select").value;
    localStorage.setItem("gitrun-theme", theme);
    localStorage.setItem("gitrun-compact-mode", document.getElementById("compact-mode").checked ? "true" : "false");
    applyTheme(theme);
    document.body.classList.toggle("compact-mode", document.getElementById("compact-mode").checked);
    toast("Appearance saved.");
  });
  document.getElementById("save-cat").addEventListener("click", () => {
    const enabled = document.getElementById("cat-enabled").checked;
    localStorage.setItem("gitrun-cat-enabled", enabled ? "true" : "false");
    document.body.classList.toggle("cat-hidden", !enabled);
    toast("Mascot preference saved.");
  });
  document.getElementById("restart-tour").addEventListener("click", async () => {
    const steps = [
      ["Overview","Your operational landing page. Only metrics with a real backend source should appear as facts."],
      ["Repositories","Each repository gets its own operational view and its own policy settings."],
      ["Privacy & Security","Review trust boundaries. Configured does not automatically mean verified at runtime."],
      ["GitVault","Manage secret metadata and scopes. Plaintext secret values are never returned to the webview."],
      ["GitSecureRun","Review execution policy and its configured protections. A saved policy is not proof of runtime enforcement."],
      ["General settings","Global runner behavior, health checks and update checks live here."],
      ["App settings","Manage appearance, the cat companion and service controls."],
      ["Recovery","Inspect startup blockers and host health. Repair actions require explicit confirmation."]
    ];
    let index = 0;
    const show = () => {
      const step = steps[index];
      const modal = document.createElement("div");
      modal.className = "modal-backdrop";
      modal.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="tour-title"><div class="eyebrow">GITRUN TOUR · ' + (index + 1) + ' / ' + steps.length + '</div><h2 id="tour-title">' + esc(step[0]) + '</h2><p>' + esc(step[1]) + '</p><div class="modal-actions"><button class="btn" data-exit>Exit tour</button><button class="btn btn-primary" data-next>' + (index === steps.length - 1 ? "Finish" : "Next") + '</button></div></section>';
      document.body.appendChild(modal);
      modal.querySelector("[data-exit]").onclick = () => { localStorage.setItem("gitrun-tutorial-dismissed","true"); modal.remove(); };
      modal.querySelector("[data-next]").onclick = () => {
        modal.remove();
        if (index === steps.length - 1) {
          localStorage.setItem("gitrun-tutorial-complete", "true");
          toast("Guided tour completed.");
          renderAppSettings();
        } else { index += 1; show(); }
      };
    };
    show();
  });
  for (const action of ["start","stop","restart"]) {
    const button = document.getElementById("service-" + action);
    button.addEventListener("click", async () => {
      if (action !== "start") {
        const title = action === "stop" ? "Stop the GitRun service?" : "Restart the GitRun service?";
        const detail = action === "stop"
          ? "The scheduler will stop reconciling runners and processing new work. Existing runner containers may continue running until separately stopped."
          : "The scheduler will restart and reinitialize its runner reconciliation state. Check active workloads before continuing.";
        const ok = await confirmModal(title, detail, action === "stop" ? "Stop service" : "Restart service", true);
        if (!ok) return;
      }
      const status = document.getElementById("service-operation-status");
      status.textContent = action.charAt(0).toUpperCase() + action.slice(1) + "ing service…";
      try {
        const result = await invoke("control_gitrun_service",{action});
        status.textContent = result;
        toast("GitRun service action completed.");
        await renderAppSettings();
      } catch (error) {
        status.textContent = "Operation failed: " + error;
        toast("GitRun service action failed: " + error,"error");
      }
    });
  }
  document.getElementById("run-security-preset-check").addEventListener("click", async () => {
    const target = document.getElementById("app-preset-results");
    target.innerHTML = '<div class="notice"><div><strong>Checking baseline…</strong>Reading saved settings and runtime process observation.</div></div>';
    try {
      const [config, settings, gsr] = await Promise.all([
        invoke("get_config"),
        invoke("get_gitrun_settings"),
        invoke("get_gsr_status")
      ]);
      const directSocketRepos = Object.entries(settings.repositories || {}).filter(([, value]) => value.docker && value.docker.direct_socket_enabled).map(([repo]) => repo);
      const checks = [
        {label:"Runner network is dedicated",ok:Boolean(config.runner_network) && config.runner_network!=="bridge" && config.runner_network!=="host" && !config.runner_network.startsWith("container:"),detail:"Configured network: " + (config.runner_network || "not set")},
        {label:"Read-only runner root filesystem",ok:Boolean(config.runner_rootfs_read_only),detail:"Configured root filesystem posture"},
        {label:"Docker socket hardening",ok:Boolean(config.gsr_docker_socket_hardening) && !Boolean(config.gsr_allow_unsafe_runner),detail:"Hardening " + (config.gsr_docker_socket_hardening?"enabled":"disabled") + "; unsafe gate " + (config.gsr_allow_unsafe_runner?"enabled":"disabled")},
        {label:"GSR command policy and baseline blacklist",ok:Boolean(config.gsr_command_policy_enabled) && Boolean(config.gsr_command_baseline_blacklist_enabled),detail:"Built-in command policy configuration"},
        {label:"Workflow validation",ok:Boolean(config.gsr_workflow_validation_enabled),detail:"Pre-admission validation configuration"},
        {label:"Resource-pressure backpressure",ok:Boolean(config.resource_pressure_enabled),detail:"Host pressure thresholds configured"},
        {label:"Cache scope is not global",ok:config.shared_cache_scope!=="global",detail:"Configured cache scope: " + config.shared_cache_scope},
        {label:"No direct socket exceptions",ok:directSocketRepos.length===0,detail:directSocketRepos.length ? directSocketRepos.join(", ") : "No repository-specific direct socket exceptions"},
        {label:"GSR process observed",ok:Boolean(gsr.watching),detail:gsr.watching?"Process-name observation succeeded; enforcement is still not verified.":"The expected GSR process name was not detected."}
      ];
      const passed = checks.filter((check)=>check.ok).length;
      target.innerHTML = '<div class="notice ' + (passed===checks.length?"success":"warn") + '"><div><strong>' + passed + ' / ' + checks.length + ' baseline checks passed</strong><p>Policy configuration and one process-name observation only. This is not an end-to-end proof of runtime enforcement.</p></div></div><div class="card-list" style="margin-top:10px">' + checks.map((check)=>'<div class="list-row"><div class="list-row-main"><strong>' + esc(check.label) + '</strong><small>' + esc(check.detail) + '</small></div>' + pill(check.ok?"Pass":"Review",check.ok?"good":"warn") + '</div>').join("") + '</div>';
    } catch (error) {
      target.innerHTML = '<div class="notice danger"><div><strong>Baseline check failed</strong>' + esc(error) + '</div></div>';
    }
  });
  const closeDashboardWindow = async () => {
    try {
      const current = window.__TAURI__?.window?.getCurrentWindow?.();
      if (current) { await current.close(); return; }
    } catch (_) {}
    window.close();
  };
  const showInstallationProgress = async ({title, action, buttonId, prompt, successText, invokeCommand}) => {
    const ok = await confirmModal(title, prompt, action, true);
    if (!ok) return;
    const status = document.getElementById("installation-status");
    status.textContent = action + "…";
    const modal = document.createElement("div");
    modal.className = "modal-backdrop";
    modal.innerHTML = '<section class="modal" role="dialog" aria-modal="true"><h2>' + esc(title) + '</h2><p id="installation-progress-status">Waiting for the operation…</p><div id="installation-progress-terminal" class="terminal" role="log" aria-live="polite"></div></section>';
    document.body.appendChild(modal);
    let unlisten = null;
    try {
      unlisten = await window.__TAURI__.event.listen("gitrun-setup-progress", (event) => {
        const item = event.payload || {};
        const terminal = modal.querySelector("#installation-progress-terminal");
        if (item.message) {
          terminal.textContent += String(item.message) + "\n";
          terminal.scrollTop = terminal.scrollHeight;
        }
        modal.querySelector("#installation-progress-status").textContent = item.message || action + "…";
      });
      await invoke(invokeCommand);
      modal.querySelector("#installation-progress-status").textContent = successText;
      status.textContent = successText;
      await new Promise((resolve) => window.setTimeout(resolve, 700));
      await closeDashboardWindow();
    } catch (error) {
      modal.querySelector("#installation-progress-status").textContent = action + " failed: " + error;
      status.textContent = action + " failed.";
      toast(action + " failed: " + error, "error");
      const close = document.createElement("button");
      close.className = "btn";
      close.textContent = "Close";
      close.onclick = () => modal.remove();
      modal.querySelector(".modal").appendChild(close);
    } finally {
      if (unlisten) await unlisten();
    }
  };
  document.getElementById("reinstall-gitrun").addEventListener("click", () => showInstallationProgress({
    title: "Reinstall GitRun?",
    action: "Reinstall",
    buttonId: "reinstall-gitrun",
    prompt: "GitRun will download the latest official release, verify its signed manifest and package checksum, then reinstall it. Existing configuration and credentials are preserved while runtime resources are rebuilt. Administrator authorization may be requested.",
    successText: "Reinstall completed. Closing this dashboard…",
    invokeCommand: "reinstall_gitrun"
  }));
  document.getElementById("uninstall-gitrun").addEventListener("click", () => showInstallationProgress({
    title: "Uninstall GitRun?",
    action: "Uninstall",
    buttonId: "uninstall-gitrun",
    prompt: "This permanently removes the GitRun package, services, configuration, logs, app preferences, runner containers, GitRun-managed cache volumes/networks and runner images. Docker itself and unrelated host workloads are preserved. This cannot be undone.",
    successText: "GitRun has been removed. Closing this dashboard…",
    invokeCommand: "uninstall_gitrun"
  }));
}
export function applyTheme(theme) {
  let effective = theme;
  if (theme === "system") effective = window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  document.documentElement.dataset.theme = effective;
}
