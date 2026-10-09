import { invoke, content, esc, heading, panel, pill, empty, errorView, toast, setFooter, confirmModal } from "../lib.js";

export async function renderGeneral() {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Loading runner settings and health checks…</p></div>';
  let config, health, vmConfigs, decisions;
  try {
    [config, health, vmConfigs, decisions] = await Promise.all([invoke("get_config"), invoke("get_dashboard_health"), invoke("list_vm_configs"), invoke("list_pending_hypervisor_decisions")]);
  } catch (error) { if (generation !== window.__gitrunNavigationGeneration) return; content.innerHTML = heading("OPERATIONS","General settings","Global runner behavior, version and diagnostics.") + errorView(error); return; }
  if (generation !== window.__gitrunNavigationGeneration) return;
  content.innerHTML = heading("OPERATIONS","General settings","Global runner configuration, diagnostics and update checks.", '<button class="btn" id="run-health-check">Run health check</button><button class="btn btn-primary" id="check-updates">Check for updates</button>') +
    '<div class="grid metrics-grid"><article class="panel metric-card"><div class="metric-top">Installed version</div><div class="metric-value" style="font-size:22px">' + esc(health.version || "Unknown") + '</div><div class="metric-foot">Resolved by local backend</div></article><article class="panel metric-card"><div class="metric-top">Configuration</div><div class="metric-value">' + (health.config_ok ? "Valid" : "Issue") + '</div><div class="metric-foot">' + esc(health.config_path || "Path unavailable") + '</div></article><article class="panel metric-card"><div class="metric-top">Docker</div><div class="metric-value">' + (health.docker_ok ? "Reachable" : "Unavailable") + '</div><div class="metric-foot">Local daemon check</div></article><article class="panel metric-card"><div class="metric-top">GSR process</div><div class="metric-value">' + (health.gsr_process_seen ? "Seen" : "Missing") + '</div><div class="metric-foot">Presence check only</div></article></div>' +
    '<div id="update-result" class="section"></div>' +
    '<div class="two-col section">' +
      '<section class="panel panel-pad"><div class="section-heading"><h2>Runner pool</h2>' + pill("Global","info") + '</div><p class="section-description">These are desired limits, not a live count of online or busy runners.</p><div class="form-grid"><div class="field"><label for="min-runners">Minimum runners</label><input id="min-runners" type="number" min="0" max="100" value="' + esc(config.min_runners) + '"></div><div class="field"><label for="max-runners">Maximum runners</label><input id="max-runners" type="number" min="1" max="500" value="' + esc(config.max_runners) + '"></div><div class="field"><label for="poll-interval">Scheduler poll interval (seconds)</label><input id="poll-interval" type="number" min="1" max="3600" value="' + esc(config.poll_interval) + '"></div><div class="field"><label for="idle-timeout">Idle timeout (seconds)</label><input id="idle-timeout" type="number" min="0" max="86400" value="' + esc(config.idle_timeout) + '"></div></div><div class="toolbar"><span class="field-hint">Changes affect scheduler behavior.</span><button class="btn btn-primary" id="save-general">Save runner settings</button></div></section>' +
      '<section class="panel panel-pad"><div class="section-heading"><h2>Update policy</h2>' + pill(config.auto_container_update ? "Scheduled" : "Manual","info") + '</div><label class="checkbox-row"><input type="checkbox" id="auto-update" ' + (config.auto_container_update ? "checked" : "") + '><span><strong>Automatic runner container updates</strong>Allow GTUU to update eligible permanent runner containers on its configured schedule.</span></label><div class="field"><label for="update-time">Scheduled update time</label><input id="update-time" type="time" value="' + esc(config.container_update_time || "03:00") + '"></div><label class="checkbox-row"><input type="checkbox" id="auto-recovery" ' + (config.auto_container_recovery ? "checked" : "") + '><span><strong>Automatic container recovery</strong>Allow the configured recovery policy to restart eligible containers.</span></label><div class="field"><label>Runner image</label><div class="kv"><small>Configured image</small><strong class="mono">' + esc(config.runner_image || "Unknown") + '</strong></div></div></section>' +
    '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Runner capacity and runtime</h2>' + pill("Global","info") + '</div><p class="section-description">These settings shape new runner containers and host scheduling. They do not modify already-running containers unless a supported reconciliation/update operation applies them.</p><div class="form-grid"><div class="field"><label for="host-profile">Host sizing profile</label><select id="host-profile"><option value="small" ' + (config.host_profile==="small"?"selected":"") + '>Small</option><option value="standard" ' + (config.host_profile==="standard"?"selected":"") + '>Standard</option><option value="large" ' + (config.host_profile==="large"?"selected":"") + '>Large</option></select></div><div class="field"><label for="runner-labels">Runner labels (comma-separated)</label><input id="runner-labels" value="' + esc(config.runner_labels) + '"></div><div class="field"><label for="container-cpus">Container CPU limit</label><input id="container-cpus" value="' + esc(config.container_cpus) + '" placeholder="2"></div><div class="field"><label for="container-memory">Container memory limit</label><input id="container-memory" value="' + esc(config.container_memory) + '" placeholder="4g"></div><div class="field"><label for="container-pids-limit">Container process limit</label><input id="container-pids-limit" value="' + esc(config.container_pids_limit) + '" placeholder="512"></div><div class="field"><label for="runner-home-size">Runner home size</label><input id="runner-home-size" value="' + esc(config.runner_home_size) + '" placeholder="8g"></div><div class="field"><label for="runner-home-backend">Runner home storage</label><select id="runner-home-backend"><option value="volume" ' + (config.runner_home_backend==="volume"?"selected":"") + '>Docker volume (isolated)</option><option value="tmpfs" ' + (config.runner_home_backend==="tmpfs"?"selected":"") + '>tmpfs</option></select></div><div class="field"><label for="gtuu-timezone">Scheduled update timezone</label><select id="gtuu-timezone"><option value="utc" ' + (config.gtuu_schedule_timezone==="utc"?"selected":"") + '>UTC</option><option value="local" ' + (config.gtuu_schedule_timezone==="local"?"selected":"") + '>Host local time</option></select></div><div class="field"><label for="recovery-cooldown">Container recovery cooldown (seconds)</label><input id="recovery-cooldown" type="number" min="0" value="' + esc(config.container_recovery_cooldown) + '"></div></div><label class="checkbox-row"><input id="runner-ephemeral" type="checkbox" ' + (config.ephemeral?"checked":"") + '><span><strong>Prefer ephemeral runner containers</strong>Changes affect subsequent runner creation and reconciliation.</span></label><label class="checkbox-row"><input id="runner-disable-update" type="checkbox" ' + (config.runner_disable_update?"checked":"") + '><span><strong>Disable the runner's self-update mechanism</strong>GitRun's GTUU remains responsible for updating permanent runner images.</span></label><div class="toolbar"><span class="field-hint">Backend validation remains authoritative.</span><button class="btn btn-primary" id="save-capacity-settings">Save capacity settings</button></div></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Pending hypervisor decisions</h2><span class="pill ' + ((decisions || []).length ? "warn" : "good") + '">' + (decisions || []).length + ' pending</span></div><p class="section-description">When KVM is unavailable, the scheduler can pause a VM setup and wait for an operator to choose whether to retry KVM or use VirtualBox.</p>' + ((decisions || []).length ? '<div class="card-list">' + decisions.map((decision) => '<div class="list-row"><div class="list-row-main"><strong>' + esc(decision.vm_name) + '</strong><small>' + esc(decision.error) + ' · requested ' + esc(new Date(Number(decision.requested_at)*1000).toLocaleString()) + '</small></div><div class="heading-actions"><button class="btn" data-vm-choice="retry_kvm" data-vm-name="' + esc(decision.vm_name) + '">Retry KVM</button><button class="btn btn-primary" data-vm-choice="use_virtual_box" data-vm-name="' + esc(decision.vm_name) + '">Use VirtualBox</button></div></div>').join("") + '</div>' : empty("No pending decisions","The scheduler has no unresolved hypervisor prompts.")) + '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>VM lifecycle</h2><button class="btn btn-primary" id="add-vm-config">＋ Add VM</button></div><p class="section-description">VM definitions are used by Logic Containers rules. Windows guests require a correctly configured Docker TLS endpoint.</p><div id="vm-management"></div></div>' +
    '<div class="section">' + panel("Health check results", '<div id="health-results">' + (health.checks || []).map((check) => '<div class="list-row"><div class="list-row-main"><strong>' + esc(check.name) + '</strong><small>' + esc(check.detail) + '</small></div>' + pill(check.ok ? "OK" : "Needs attention", check.ok ? "good" : "warn") + '</div>').join("") + '</div>') + '</div>';

  renderVmManagement(vmConfigs);
  content.querySelectorAll("[data-vm-choice]").forEach((button) => button.addEventListener("click", async () => {
    const vmName = button.dataset.vmName;
    const choice = button.dataset.vmChoice;
    const ok = await confirmModal("Confirm hypervisor choice?", "The scheduler will retry setup for " + vmName + " using " + (choice === "retry_kvm" ? "KVM" : "VirtualBox") + ". This only records your choice; the scheduler performs the actual operation.", "Confirm choice");
    if (!ok) return;
    try { await invoke("respond_hypervisor_decision",{vmName,choice}); toast("Hypervisor decision recorded."); await renderGeneral(); }
    catch (error) { toast("Could not record decision: " + error,"error"); }
  }));
  document.getElementById("add-vm-config").addEventListener("click", () => openVmModal(null, vmConfigs));
    document.getElementById("save-capacity-settings").addEventListener("click", async () => {
    const cooldown = Number(document.getElementById("recovery-cooldown").value);
    const cpus = document.getElementById("container-cpus").value.trim();
    const memory = document.getElementById("container-memory").value.trim();
    const pids = document.getElementById("container-pids-limit").value.trim();
    const homeSize = document.getElementById("runner-home-size").value.trim();
    if (!Number.isInteger(cooldown) || cooldown < 0 || !cpus || !memory || !pids || !homeSize) {
      toast("Enter valid resource limits, runner home size and recovery cooldown.", "warning"); return;
    }
    const next = {
      ...config,
      host_profile: document.getElementById("host-profile").value,
      runner_labels: document.getElementById("runner-labels").value.trim(),
      container_cpus: cpus,
      container_memory: memory,
      container_pids_limit: pids,
      runner_home_size: homeSize,
      runner_home_backend: document.getElementById("runner-home-backend").value,
      gtuu_schedule_timezone: document.getElementById("gtuu-timezone").value,
      container_recovery_cooldown: cooldown,
      ephemeral: document.getElementById("runner-ephemeral").checked,
      runner_disable_update: document.getElementById("runner-disable-update").checked
    };
    try { await invoke("save_config", {updated:next}); toast("Capacity settings saved."); }
    catch (error) { toast("Could not save capacity settings: " + error, "error"); }
  });
  document.getElementById("save-general").addEventListener("click", async () => {
    const min = Number(document.getElementById("min-runners").value);
    const max = Number(document.getElementById("max-runners").value);
    const poll = Number(document.getElementById("poll-interval").value);
    const idle = Number(document.getElementById("idle-timeout").value);
    if (!Number.isInteger(min) || !Number.isInteger(max) || min < 0 || max < 1 || min > max || !Number.isInteger(poll) || poll < 1 || !Number.isInteger(idle) || idle < 0) {
      toast("Check the numeric values: runner limits must be valid and minimum cannot exceed maximum.", "warning"); return;
    }
    const next = {...config, min_runners:min, max_runners:max, poll_interval:poll, idle_timeout:idle, auto_container_update:document.getElementById("auto-update").checked, container_update_time:document.getElementById("update-time").value || "03:00", auto_container_recovery:document.getElementById("auto-recovery").checked};
    try { await invoke("save_config", {updated:next}); toast("General settings saved."); }
    catch (error) { toast("Could not save settings: " + error, "error"); }
  });
  document.getElementById("run-health-check").addEventListener("click", async () => {
    try {
      const report = await invoke("get_dashboard_health");
      document.getElementById("health-results").innerHTML = (report.checks || []).map((check) => '<div class="list-row"><div class="list-row-main"><strong>' + esc(check.name) + '</strong><small>' + esc(check.detail) + '</small></div>' + pill(check.ok ? "OK" : "Needs attention", check.ok ? "good" : "warn") + '</div>').join("");
      toast("Health check completed.");
    } catch (error) { toast("Health check failed: " + error, "error"); }
  });
  document.getElementById("check-updates").addEventListener("click", async () => {
    const target = document.getElementById("update-result");
    target.innerHTML = '<div class="notice"><div><strong>Checking updates…</strong>Running the non-installing GitRun update check.</div></div>';
    try {
      const result = await invoke("check_gitrun_updates");
      const runnerUpdateAvailable = result.runner_image_update_available === true;
      target.innerHTML = '<div class="notice ' + (result.available || runnerUpdateAvailable ? "warn" : result.checked ? "success" : "danger") + '"><div><strong>' + esc(result.title || (result.available ? "Update available" : result.checked ? "GitRun is up to date" : "Update check failed")) + '</strong><p>' + esc(result.detail || result.output || result.error || "No additional details were returned.") + '</p>' +
        (result.runner_image_status ? '<p><strong>Runner image:</strong> ' + esc(result.runner_image_status) + '</p>' : '') +
        '<p class="field-hint">Checking is read-only. No downloads or installations occur until an update action is confirmed.</p>' +
        (runnerUpdateAvailable ? '<button class="btn btn-primary" id="update-permanent-runners">Update eligible permanent runners</button><p class="field-hint">GitRun itself is not replaced by this action. Busy permanent runners are left untouched and can be retried during a later GTUU run.</p>' : '') +
        '</div></div>';
      if (!result.checked) toast("Update check did not complete.", "warning");
      if (runnerUpdateAvailable) {
        document.getElementById("update-permanent-runners").addEventListener("click", async () => {
          const accepted = await confirmModal(
            "Update the permanent runner image?",
            "GitRun will fetch and verify the signed release manifest, download the matching runner image digest, update the configured image, and reconcile eligible permanent containers. Busy runners are not stopped; they remain on their current image until a later GTUU run. GitRun itself is not updated by this operation.",
            "Update eligible runners",
            true
          );
          if (!accepted) return;
          const action = document.getElementById("update-permanent-runners");
          action.disabled = true;
          const terminal = document.createElement("div");
          terminal.className = "terminal";
          terminal.textContent = "Requesting privileged runner update…\n";
          target.appendChild(terminal);
          let unlisten = null;
          try {
            unlisten = await window.__TAURI__.event.listen("gitrun-setup-progress", (event) => {
              const item = event.payload || {};
              if (item.message) {
                terminal.textContent += String(item.message) + "\n";
                terminal.scrollTop = terminal.scrollHeight;
              }
            });
            await invoke("update_permanent_runners");
            toast("Runner update/reconciliation completed.");
            terminal.textContent += "\nOperation completed. Review the output above for per-container results.";
          } catch (error) {
            terminal.textContent += "\nERROR: " + String(error);
            toast("Runner update failed: " + error, "error");
          } finally {
            if (unlisten) await unlisten();
            action.disabled = false;
          }
        });
      }
    } catch (error) { target.innerHTML = '<div class="notice danger"><div><strong>Update check failed</strong>' + esc(error) + '</div></div>'; }
  });
  setFooter("Settings loaded · " + new Date().toLocaleTimeString());
}
