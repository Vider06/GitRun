import { invoke, content, esc, heading, panel, pill, toast, confirmModal, applyTheme } from "../lib.js";

export async function renderAppSettings() {
  const generation = window.__gitrunNavigationGeneration;
  const savedTheme = localStorage.getItem("gitrun-theme") || "dark";
  const catEnabled = localStorage.getItem("gitrun-cat-enabled") !== "false";
  const tutorialSeen = localStorage.getItem("gitrun-tutorial-complete") === "true";
  const compact = localStorage.getItem("gitrun-compact-mode") === "true";
  let serviceState = {service_installed:false,service_active:false,checks:[]};
  let serviceLoadError = null;
  try { serviceState = await invoke("get_dashboard_health"); } catch (error) { serviceLoadError = String(error); }
  let kittyData = null;
  let kittyLoadError = null;
  try { kittyData = await invoke("get_kitty_dashboard_data"); } catch (error) { kittyLoadError = String(error); }
  if (generation !== window.__gitrunNavigationGeneration) return;
  const kittySettings = kittyData?.settings || {
    schema_version:1, enabled:false, telemetry_enabled:true, predictive_analytics_enabled:true,
    workflow_model_visible:true, predictive_warm_pool_enabled:false,
    adaptive_resource_management_enabled:false, smart_fast_stop_enabled:false, automation_mode:false,
    automation_allowed_repositories:[], fast_stop_max_runtime_seconds:0, shadow_mode:true,
    minimum_samples:576, history_buckets:2016, max_additional_warm_runners:2,
    warm_pool_min_hold_seconds:900, warm_pool_lead_seconds:300, minimum_quality_score:0.85
  };
  const kittyStatus = kittyData?.status || {repositories:[]};
  const kittyStatusError = kittyData?.status_error || null;
  const formatEpoch = (value) => value ? new Date(value * 1000).toLocaleString() : "Not recorded";
  const hiddenModelPlaceholder = '<div class="notice"><div><strong>Workflow Model details are hidden</strong><p>Historical predictions remain stored privately. Re-enable model visibility to show the summarized history and latest decision.</p></div></div>';
  const emptyModelPlaceholder = '<div class="notice"><div><strong>No Workflow Model history yet</strong><p>No observations are recorded yet. Kitty must be enabled for aggregate telemetry, and forecasts require sufficient history and evaluated outcomes.</p></div></div>';
  const kittyStatusNow = Date.now() / 1000;
  const kittyCanForecast = kittySettings.enabled &&
    kittySettings.telemetry_enabled && kittySettings.predictive_analytics_enabled;
  const kittyRows = (kittyStatus.repositories || []).map((entry) => {
    const forecast = entry.latest_forecast;
    const decision = entry.latest_decision;
    const observationAge = entry.latest_observation_epoch
      ? kittyStatusNow - entry.latest_observation_epoch
      : Number.POSITIVE_INFINITY;
    const forecastFresh = !!forecast && kittyCanForecast &&
      observationAge >= 0 && observationAge <= 600 &&
      Number.isFinite(forecast.generated_at_epoch) &&
      kittyStatusNow >= forecast.generated_at_epoch &&
      kittyStatusNow - forecast.generated_at_epoch <= 600;
    const forecastLabel = !forecast ? "Learning" : forecastFresh ? "Forecast" : "Historical";
    const readiness = forecast && Number.isFinite(forecast.confidence_score)
      ? (forecast.confidence_score * 100).toFixed(1) + "%"
      : "Not ready";
    const quality = forecast && Number.isFinite(forecast.quality_score)
      ? (forecast.quality_score * 100).toFixed(1) + "%"
      : "Not evaluated";
    const basePredicted = forecast ? String(forecast.predicted_peak_concurrency) : "Not ready";
    const predicted = forecast
      ? String(forecast.seasonal_predicted_peak_concurrency ?? forecast.predicted_peak_concurrency)
      : "Not ready";
    const error = forecast && Number.isFinite(forecast.mean_absolute_error)
      ? forecast.mean_absolute_error.toFixed(2)
      : "Not enough evaluated forecasts";
    const decisionText = decision ? String(decision.status).replaceAll("_", " ") : "No decision recorded";
    const explanation = decision ? String(decision.reason || "") : "No decision recorded yet.";
    const decisionAt = decision ? formatEpoch(decision.bucket_epoch) : "Not recorded";
    const plannedActions = Number.isInteger(decision?.planned_action_count)
      ? String(decision.planned_action_count)
      : "Not recorded";
    const outcome = decision?.execution_outcome
      ? String(decision.execution_outcome).replaceAll("_", " ")
      : "Pending reconciliation";
    const workloadRows = (entry.workload_patterns || []).slice(0, 3).map((workload) => {
      const recurring = (workload.recurring_slots || []).map((slot) =>
        String(slot.weekday) + ' ' + String(slot.hour).padStart(2, "0") +
          ':00 (' + String(slot.distinct_weeks) + ' weeks)'
      ).join(' · ') || "No recurring slot established";
      const latest = workload.latest_prediction;
      const predictionText = latest
        ? 'Latest target ' + formatEpoch(latest.target_epoch) +
          ' · predicted ' + String(latest.predicted_occurrences) +
          ' · prepared ' + String(latest.prewarm_runners_created || 0) +
          ' · observed ' + (latest.actual_occurrences == null ? "pending" : String(latest.actual_occurrences)) +
          (Number.isFinite(latest.mean_error_correction)
            ? ' · correction ' + Number(latest.mean_error_correction).toFixed(2)
            : '')
        : 'No scheduled prediction evaluated yet';
      return '<div class="field-hint mt-8"><strong>' + esc(workload.job_name) + '</strong>' +
        '<small>' + esc(String(workload.workflow_path)) + ' · workload ' +
        esc(String(workload.workload_id)) + ' · ' + esc(String(workload.occurrence_count)) +
        ' actual job starts · last seen ' + esc(formatEpoch(workload.last_seen_epoch)) +
        '</small><small>Exclusive family route (managed Linux): ' +
        esc('gitrun-job-' + String(workload.workload_id)) +
        ' · set this as the only value in runs-on; omit self-hosted/Linux/X64 labels so unrelated jobs cannot take this runner</small><small>GitHub dispatch chooses any idle runner in this family, not an exact container ID or one future run/job</small><small>Observed runner labels: ' +
        esc((workload.labels || []).slice(0, 8).join(', ')) +
        ((workload.labels || []).length > 8 ? ' …' : '') +
        '</small><small>Recurring: ' + esc(recurring) +
        '</small><small>' + esc(predictionText) + '</small></div>';
    }).join("");
    const recentJobs = (entry.recent_job_occurrences || []).slice(-3).reverse().map((job) =>
      '<small>GitRun job ' + esc(String(job.gitrun_job_id)) +
        ' · workload ' + esc(String(job.workload_id)) +
        (job.started_at_epoch ? ' · started ' + esc(formatEpoch(job.started_at_epoch)) : ' · start time unavailable') +
        '</small>'
    ).join("");
    return '<div class="list-row"><div class="list-row-main"><strong>' + esc(entry.repository) + '</strong><small>' +
      esc(String(entry.sample_count)) + ' recent five-minute samples · last observation ' + esc(formatEpoch(entry.latest_observation_epoch)) +
      '</small><small>Predicted concurrency: ' + esc(predicted) +
      (forecast && forecast.seasonal_predicted_peak_concurrency != null
        ? ' for target slot ' + esc(formatEpoch(forecast.seasonal_target_epoch)) +
          ' · one-hour baseline ' + esc(basePredicted)
        : '') + ' · readiness: ' + esc(readiness) +
      ' · forecast quality: ' + esc(quality) + ' · MAE: ' + esc(error) + '</small>' +
      (forecast ? '<small>Forecast generated ' + esc(formatEpoch(forecast.generated_at_epoch)) +
        ' · horizon target ' + esc(formatEpoch(forecast.target_epoch)) + '</small>' : '') +
      '<small>Last decision: ' + esc(decisionAt) + ' · planned actions: ' + esc(plannedActions) +
      ' · outcome: ' + esc(outcome) + '</small><small>' +
      esc(decisionText) + ': ' + esc(explanation) + '</small>' +
      (workloadRows ? '<div class="mt-10"><strong>Recurring job profiles</strong>' + workloadRows + '</div>' : '') +
      (recentJobs ? '<div class="mt-10"><strong>Recent GitRun occurrence IDs</strong>' + recentJobs + '</div>' : '') +
      '</div>' +
      pill(forecastLabel, forecastFresh ? "good" : forecast ? "neutral" : "info") + '</div>';
  }).join("") || (kittySettings.workflow_model_visible ? emptyModelPlaceholder : hiddenModelPlaceholder);
  const kittyPanel = '<div class="section">' + panel("Kitty Smart Scheduler",
    '<p class="section-description">Kitty is an optional statistics-based companion. The deterministic scheduler remains primary and is always the fallback. It is disabled by default; recommendations never authorize themselves. For a routed workload, use the generated gitrun-job-wk-… label as the only runs-on label on a managed Linux runner profile. GitRun creates that family with no generic self-hosted/platform labels and reuses its saved backend/image profile; an unknown family fails closed. GitHub will choose any idle runner in that family, not one exact container for a future run/job ID.</p>' +
    (kittyLoadError ? '<div class="notice warn"><div><strong>Kitty settings could not be loaded</strong><p>' + esc(kittyLoadError) + '</p><p>The visible fields use safe defaults and may not reflect saved values. Saving will replace the current settings with the values shown. Unsafe ownership, permissions or trusted-path failures require administrator repair.</p></div></div>' : '') +
    (kittyStatusError ? '<div class="notice warn"><div><strong>Workflow Model status is unavailable</strong><p>' + esc(kittyStatusError) + '</p><p>Kitty settings are still available, but the status snapshot was rejected. Inspect the status file ownership and permissions before retrying.</p></div></div>' : '') +
    '<label class="checkbox-row"><input id="kitty-enabled" type="checkbox" ' + (kittySettings.enabled ? "checked" : "") + '><span><strong>Enable Kitty</strong>Enable the optional observer and its statistical model. This does not enable automatic scaling.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-telemetry-enabled" type="checkbox" ' + (kittySettings.telemetry_enabled ? "checked" : "") + '><span><strong>Collect aggregate telemetry</strong>Queue and runner counts plus optional host resource measurements. No job payloads, environment values, tokens or secrets.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-analytics-enabled" type="checkbox" ' + (kittySettings.predictive_analytics_enabled ? "checked" : "") + '><span><strong>Predictive analytics</strong>Forecast aggregate busy-plus-queued demand and evaluate forecasts against later observations.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-model-visible" type="checkbox" ' + (kittySettings.workflow_model_visible ? "checked" : "") + '><span><strong>Show Workflow Model details</strong>Show the summarized forecast, sample count and latest decision in this dashboard.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-shadow-mode" type="checkbox" ' + (kittySettings.shadow_mode ? "checked" : "") + '><span><strong>Shadow mode</strong>Keep recommendations observational. This should stay on until forecasts have been reviewed against real workloads.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-warm-pool-enabled" type="checkbox" ' + (kittySettings.predictive_warm_pool_enabled ? "checked" : "") + '><span><strong>Predictive Warm Pool</strong>Opt in to a bounded forecast-based minimum floor and workload-specific prewarming. A recurring Tuesday 16:00 profile can request compatible runner capacity from 15:55, after enough history and forecast quality have been established. GSR authorizes every active capacity change; the configured floor and global/extra-runner caps remain authoritative.</span></label>' +
    '<label class="checkbox-row"><input id="kitty-automation-mode" type="checkbox" ' + (kittySettings.automation_mode ? "checked" : "") + '><span><strong>Automation mode</strong>Permit explicitly allowlisted Stop/Fast Stop actions without per-action prompts. Every action must still pass the GSR authorization gate and durable audit; this is not a security-policy bypass.</span></label>' +
    '<div class="field mt-12"><label for="kitty-automation-repositories">Automation repository allowlist</label><textarea id="kitty-automation-repositories" rows="3" placeholder="owner/repo, one per line">' + esc((kittySettings.automation_allowed_repositories || []).join("\n")) + '</textarea><small class="field-hint">Automation remains disabled for repositories not listed here. Only repositories configured in GitRun can be acted on.</small></div>' +
    '<label class="checkbox-row"><input id="kitty-fast-stop-enabled" type="checkbox" ' + (kittySettings.smart_fast_stop_enabled ? "checked" : "") + '><span><strong>Smart Fast Stop</strong>Cancel an overlong workflow run only after its configured deadline, allowlist check and GSR audit. Cancellation affects the entire run, not a single job.</span></label>' +
    '<div class="field mt-10"><label for="kitty-fast-stop-runtime">Fast Stop deadline (seconds)</label><input id="kitty-fast-stop-runtime" type="number" min="300" max="604800" step="300" value="' + esc(String(kittySettings.fast_stop_max_runtime_seconds || 14400)) + '"><small class="field-hint">Default proposal: 14400 seconds (4 hours). The deadline is inactive unless Smart Fast Stop is enabled; runs with unknown start time are never cancelled.</small></div>' +
    '<label class="checkbox-row"><input type="checkbox" disabled><span><strong>Adaptive Resource Management</strong>Unavailable in this version; reliable per-job resource attribution and bounded profiles are not ready.</span></label>' +
    '<div class="two-col mt-12">' +
      '<div class="field"><label for="kitty-min-samples">Minimum five-minute samples</label><input id="kitty-min-samples" type="number" min="24" max="2016" value="' + esc(String(kittySettings.minimum_samples)) + '"><small class="field-hint">Default 576 samples (48 hours); forecast quality needs later outcomes too.</small></div>' +
      '<div class="field"><label for="kitty-history-buckets">History buckets (maximum 2016)</label><input id="kitty-history-buckets" type="number" min="24" max="2016" value="' + esc(String(kittySettings.history_buckets)) + '"><small class="field-hint">Five-minute high-resolution history, at most seven days. Weekly, four-week, yearly and lifetime summaries plus bounded workload profiles are retained separately.</small></div>' +
      '<div class="field"><label for="kitty-max-additional">Maximum extra warm runners</label><input id="kitty-max-additional" type="number" min="0" max="100" value="' + esc(String(kittySettings.max_additional_warm_runners)) + '"><small class="field-hint">Additional capacity Kitty may request above the configured minimum.</small></div>' +
      '<div class="field"><label for="kitty-hold-seconds">Warm-pool minimum hold (seconds)</label><input id="kitty-hold-seconds" type="number" min="300" max="3600" step="300" value="' + esc(String(kittySettings.warm_pool_min_hold_seconds)) + '"><small class="field-hint">300–3600 seconds to avoid rapid scale-up/scale-down oscillation.</small></div>' +
      '<div class="field"><label for="kitty-warmup-lead">Predictive warm-up lead (seconds)</label><input id="kitty-warmup-lead" type="number" min="300" max="3600" step="300" value="' + esc(String(kittySettings.warm_pool_lead_seconds ?? 300)) + '"><small class="field-hint">Default 300 seconds: prepare for a recurring weekday/hour demand pattern five minutes before its target slot.</small></div>' +
      '<div class="field"><label for="kitty-min-quality">Minimum forecast readiness (0.5–1)</label><input id="kitty-min-quality" type="number" min="0.5" max="1" step="0.01" value="' + esc(String(kittySettings.minimum_quality_score)) + '"><small class="field-hint">A heuristic readiness score, not a calibrated probability.</small></div>' +
    '</div>' +
    '<div class="notice"><div><strong>Safety boundary</strong><p>Adaptive Resource Management remains unavailable. Smart Fast Stop only cancels an allowlisted workflow run after its configured maximum runtime and GSR authorization; GitHub run cancellation also affects every job in that run. GitRun occurrence IDs (grj-…) identify individual observed jobs, while workload IDs (wk-…) identify recurring job families. On the managed Linux bootstrap, a family-labeled runner is registered with only gitrun-job-wk-… and the workflow must use that exact label as its only runs-on requirement. This isolates a family from unrelated jobs, but GitHub selects an idle member of the family; it does not expose a supported API to force-pin one future run/job ID to a specific container. Windows family routing remains unavailable and fails closed.</p></div></div>' +
    '<div class="toolbar"><span class="field-hint">System installs request administrator authorization to save Kitty settings. Changes are read by the scheduler within 30 seconds.</span><div class="heading-actions"><button class="btn" id="refresh-kitty-status">Refresh model status</button><button class="btn btn-primary" id="save-kitty-settings">Save Kitty settings</button></div></div>' +
    '<div class="section-heading mt-12"><h3>Workflow Model status</h3>' + pill(kittySettings.enabled ? (kittySettings.shadow_mode ? "Shadow mode" : "Enabled") : "Disabled", kittySettings.enabled ? (kittySettings.shadow_mode ? "info" : "warn") : "neutral") + '</div>' +
    '<div class="card-list mt-10">' + kittyRows + '</div>'
  ) + '</div>';
  content.innerHTML = heading("APPLICATION","App settings","Preferences for the dashboard itself. Runner and security configuration lives elsewhere.") +
    '<div class="two-col">' +
      panel("Appearance", '<div class="field"><label for="theme-select">Color theme</label><select id="theme-select"><option value="dark" ' + (savedTheme === "dark" ? "selected" : "") + '>Dark · GitRun default</option><option value="light" ' + (savedTheme === "light" ? "selected" : "") + '>Light</option><option value="system" ' + (savedTheme === "system" ? "selected" : "") + '>Follow system preference</option></select></div><label class="checkbox-row"><input id="compact-mode" type="checkbox" ' + (compact ? "checked" : "") + '><span><strong>Compact data density</strong>Reduce padding in tables and cards for information-heavy screens.</span></label><div class="toolbar"><span class="field-hint">Theme is stored locally on this device.</span><button class="btn btn-primary" id="save-appearance">Save appearance</button></div>') +
      panel("GitRun cat", '<div class="mascot-inline"><div class="mascot-face"><svg viewBox="0 0 48 48" aria-hidden="true"><path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><path d="M15 26h.1M33 26h.1" stroke="#11151d" stroke-width="5" stroke-linecap="round"/><path d="M20 33q4 4 8 0" fill="none" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/></svg></div><div class="mascot-copy"><strong>GitRun companion</strong><p>A guide to setup, status and recovery — not a substitute for actual telemetry.</p></div></div><label class="checkbox-row"><input id="cat-enabled" type="checkbox" ' + (catEnabled ? "checked" : "") + '><span><strong>Show the cat companion</strong>Show the mascot in the dashboard and onboarding.</span></label><div class="toolbar"><span class="field-hint">Expression states will be tied to verified app status.</span><button class="btn" id="save-cat">Save mascot preference</button></div>') +
    '</div>' +
    kittyPanel +
    '<div class="section">' + panel("Guided tour", '<div class="list-row"><div class="list-row-main"><strong>Dashboard introduction</strong><small>' + (tutorialSeen ? "You have completed the current tour." : "The tour has not been completed on this device.") + '</small></div>' + pill(tutorialSeen ? "Completed" : "Not completed", tutorialSeen ? "good" : "info") + '</div><p class="section-description mt-12">The guided tour introduces Overview, repository policies, security boundaries, GitVault and Recovery. You can restart it at any time.</p><button class="btn btn-primary" id="restart-tour">Start guided tour</button>') + '</div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>GitRun startup and stop</h2>' + pill(serviceLoadError ? "Status unavailable" : serviceState.service_active ? "Service active" : serviceState.service_installed ? "Service stopped" : "Unit not found", serviceLoadError ? "bad" : serviceState.service_active ? "good" : "warn") + '</div><p class="section-description">These controls affect the host-level systemd service and scheduler, not just this dashboard window. Stopping or restarting it may interrupt runner reconciliation; existing workflow containers may need separate recovery.</p><div class="toolbar"><span class="field-hint">' + (serviceLoadError ? "Could not inspect service status: " + esc(serviceLoadError) : serviceState.service_installed ? "Unit is installed." : "No GitRun systemd unit was found.") + '</span><div class="heading-actions"><button class="btn btn-primary" id="service-start" ' + (serviceLoadError || !serviceState.service_installed || serviceState.service_active ? "disabled" : "") + '>Start service</button><button class="btn" id="service-restart" ' + (serviceLoadError || !serviceState.service_installed ? "disabled" : "") + '>Restart service</button><button class="btn btn-danger" id="service-stop" ' + (serviceLoadError || !serviceState.service_installed || !serviceState.service_active ? "disabled" : "") + '>Stop service</button></div></div><div id="service-operation-status" class="field-hint mt-10" role="status" aria-live="polite"></div></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Recommended security preset check</h2><span class="pill info">Read-only</span></div><p class="section-description">Checks whether the saved configuration aligns with GitRun’s recommended baseline. It does not change settings and does not prove runtime enforcement.</p><button class="btn btn-primary" id="run-security-preset-check">Run baseline check</button><div id="app-preset-results" class="mt-12"></div></div>' +
    '<div class="section">' + panel("Support", '<div class="list-row"><div class="list-row-main"><strong>Open a GitRun issue</strong><small>Include the version and a concise reproduction. Never include tokens, private keys or secret values.</small></div><a class="btn" href="https://github.com/Vider06/GitRun/issues" target="_blank" rel="noreferrer">Open issues ↗</a></div>') + '</div>' +
    '<div class="section panel panel-pad danger-zone"><div class="section-heading"><h2>Installation management</h2>' + pill("Host-level actions","bad") + '</div><p class="section-description">Reinstall downloads and verifies the latest official release while preserving your configuration. Uninstall removes GitRun&#39;s package, services and owned runtime/app data. Shared Docker and unrelated host workloads remain untouched.</p><div class="toolbar"><button class="btn" id="reinstall-gitrun">Reinstall GitRun</button><button class="btn btn-danger" id="uninstall-gitrun">Uninstall GitRun</button></div><div id="installation-status" class="field-hint mt-10" role="status" aria-live="polite"></div></div>';
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
  document.getElementById("refresh-kitty-status").addEventListener("click", async () => {
    const button = document.getElementById("refresh-kitty-status");
    if (button.disabled) return;
    button.disabled = true;
    try {
      await renderAppSettings();
      toast("Kitty status refreshed.");
    } catch (error) {
      toast("Kitty status refresh failed: " + error, "error");
    } finally {
      if (button.isConnected) button.disabled = false;
    }
  });
  document.getElementById("save-kitty-settings").addEventListener("click", async () => {
    const button = document.getElementById("save-kitty-settings");
    if (button.disabled) return;
    const updated = {
      ...kittySettings,
      enabled: document.getElementById("kitty-enabled").checked,
      telemetry_enabled: document.getElementById("kitty-telemetry-enabled").checked,
      predictive_analytics_enabled: document.getElementById("kitty-analytics-enabled").checked,
      workflow_model_visible: document.getElementById("kitty-model-visible").checked,
      shadow_mode: document.getElementById("kitty-shadow-mode").checked,
      predictive_warm_pool_enabled: document.getElementById("kitty-warm-pool-enabled").checked,
      adaptive_resource_management_enabled: false,
      automation_mode: document.getElementById("kitty-automation-mode").checked,
      automation_allowed_repositories: [...new Set(document.getElementById("kitty-automation-repositories").value
        .split(/[\n,;]/).map((repo) => repo.trim()).filter(Boolean))],
      smart_fast_stop_enabled: document.getElementById("kitty-fast-stop-enabled").checked,
      fast_stop_max_runtime_seconds: document.getElementById("kitty-fast-stop-enabled").checked
        ? Number(document.getElementById("kitty-fast-stop-runtime").value) : 0,
      minimum_samples: Number(document.getElementById("kitty-min-samples").value),
      history_buckets: Number(document.getElementById("kitty-history-buckets").value),
      max_additional_warm_runners: Number(document.getElementById("kitty-max-additional").value),
      warm_pool_min_hold_seconds: Number(document.getElementById("kitty-hold-seconds").value),
      warm_pool_lead_seconds: Number(document.getElementById("kitty-warmup-lead").value),
      minimum_quality_score: Number(document.getElementById("kitty-min-quality").value)
    };
    if (!Number.isInteger(updated.minimum_samples) || !Number.isInteger(updated.history_buckets) ||
        !Number.isInteger(updated.max_additional_warm_runners) || updated.minimum_samples < 24 ||
        updated.history_buckets < updated.minimum_samples || updated.history_buckets > 2016 ||
        updated.max_additional_warm_runners < 0 || updated.max_additional_warm_runners > 100 ||
        !Number.isInteger(updated.warm_pool_min_hold_seconds) ||
        updated.warm_pool_min_hold_seconds < 300 || updated.warm_pool_min_hold_seconds > 3600 ||
        updated.warm_pool_min_hold_seconds % 300 !== 0 ||
        !Number.isInteger(updated.warm_pool_lead_seconds) ||
        updated.warm_pool_lead_seconds < 300 || updated.warm_pool_lead_seconds > 3600 ||
        updated.warm_pool_lead_seconds % 300 !== 0 ||
        !Number.isFinite(updated.minimum_quality_score) || updated.minimum_quality_score < 0.5 ||
        updated.minimum_quality_score > 1 ||
        updated.automation_allowed_repositories.length > 64 ||
        updated.automation_allowed_repositories.some((repo) => !/^[A-Za-z0-9_.-]{1,39}\/[A-Za-z0-9_.-]{1,100}$/.test(repo)) ||
        (updated.automation_mode && !updated.enabled) ||
        (updated.automation_mode && updated.automation_allowed_repositories.length === 0) ||
        (updated.smart_fast_stop_enabled && (!updated.automation_mode ||
          updated.automation_allowed_repositories.length === 0 ||
          !Number.isInteger(updated.fast_stop_max_runtime_seconds) ||
          updated.fast_stop_max_runtime_seconds < 300 || updated.fast_stop_max_runtime_seconds > 604800)) ||
        (!updated.smart_fast_stop_enabled && updated.fast_stop_max_runtime_seconds !== 0)) {
      toast("Kitty settings are outside the supported bounds.", "error");
      return;
    }
    if (kittyLoadError) {
      const confirmed = await confirmModal(
        "Replace Kitty settings that could not be loaded?",
        "GitRun could not read the saved settings. The visible fields use safe defaults, not verified saved values. Saving will overwrite the current settings with the values shown and may reset existing options. Continue only if you intend to repair or replace them.",
        "Replace Kitty settings",
        true
      );
      if (!confirmed) return;
    }
    const automationRiskChanged = updated.automation_mode && (
      !kittySettings.automation_mode ||
      JSON.stringify(updated.automation_allowed_repositories) !== JSON.stringify(kittySettings.automation_allowed_repositories || []) ||
      (updated.smart_fast_stop_enabled && !kittySettings.smart_fast_stop_enabled) ||
      (updated.fast_stop_max_runtime_seconds > 0 &&
        updated.fast_stop_max_runtime_seconds !== (kittySettings.fast_stop_max_runtime_seconds || 0))
    );
    if (automationRiskChanged) {
      const confirmed = await confirmModal(
        "Enable allowlisted automation?",
        "Kitty may act without a prompt for every individual action, but only for the listed repositories. GSR must authorize and durably audit every action. Smart Fast Stop can cancel the entire workflow run, including other jobs in that run. Continue only if that impact is intended.",
        "Confirm Automation mode",
        true
      );
      if (!confirmed) return;
    }
    const warmPoolRiskChanged = updated.enabled && updated.telemetry_enabled &&
      updated.predictive_analytics_enabled && updated.predictive_warm_pool_enabled && !updated.shadow_mode &&
      (!kittySettings.enabled || !kittySettings.telemetry_enabled ||
        !kittySettings.predictive_analytics_enabled || !kittySettings.predictive_warm_pool_enabled ||
        kittySettings.shadow_mode ||
        updated.max_additional_warm_runners > kittySettings.max_additional_warm_runners ||
        updated.minimum_quality_score < kittySettings.minimum_quality_score ||
        updated.minimum_samples < kittySettings.minimum_samples ||
        updated.warm_pool_min_hold_seconds > kittySettings.warm_pool_min_hold_seconds ||
        updated.warm_pool_lead_seconds > (kittySettings.warm_pool_lead_seconds ?? 300));
    if (warmPoolRiskChanged) {
      const confirmed = await confirmModal(
        "Enable or relax predictive runner prewarming?",
        "Kitty may raise the minimum runner pool by at most " + updated.max_additional_warm_runners +
          " runners, only after enough recent observations and evaluated forecasts meet the quality threshold. A lower quality/sample threshold, a longer hold or a longer warm-up lead can keep extra runners active more readily or for longer. The existing runner maximum and resource-pressure gate remain in force. Kitty will not terminate runners or change security policy.",
        "Confirm Predictive Warm Pool",
        true
      );
      if (!confirmed) return;
    }
    button.disabled = true;
    try {
      await invoke("save_kitty_settings", { settings: updated });
      toast("Kitty settings saved. The scheduler will reload them shortly.");
      await renderAppSettings();
    } catch (error) {
      toast("Kitty settings save failed: " + error, "error");
    } finally {
      if (button.isConnected) button.disabled = false;
    }
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
  let serviceOperationInProgress = false;
  for (const action of ["start","stop","restart"]) {
    const button = document.getElementById("service-" + action);
    button.addEventListener("click", async () => {
      if (serviceOperationInProgress || button.disabled) return;
      if (action !== "start") {
        const title = action === "stop" ? "Stop the GitRun service?" : "Restart the GitRun service?";
        const detail = action === "stop"
          ? "The scheduler will stop reconciling runners and processing new work. Existing runner containers may continue running until separately stopped."
          : "The scheduler will restart and reinitialize its runner reconciliation state. Check active workloads before continuing.";
        const ok = await confirmModal(title, detail, action === "stop" ? "Stop service" : "Restart service", true);
        if (!ok) return;
      }
      serviceOperationInProgress = true;
      const serviceButtons = ["start", "restart", "stop"].map((name) => document.getElementById("service-" + name));
      serviceButtons.forEach((item) => { if (item) item.disabled = true; });
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
      } finally {
        serviceOperationInProgress = false;
        serviceButtons.forEach((item) => { if (item?.isConnected) item.disabled = false; });
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
      target.innerHTML = '<div class="notice ' + (passed===checks.length?"success":"warn") + '"><div><strong>' + passed + ' / ' + checks.length + ' baseline checks passed</strong><p>Policy configuration and one process-name observation only. This is not an end-to-end proof of runtime enforcement.</p></div></div><div class="card-list mt-10">' + checks.map((check)=>'<div class="list-row"><div class="list-row-main"><strong>' + esc(check.label) + '</strong><small>' + esc(check.detail) + '</small></div>' + pill(check.ok?"Pass":"Review",check.ok?"good":"warn") + '</div>').join("") + '</div>';
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
  let installationInProgress = false;
  const showInstallationProgress = async ({title, action, buttonId, prompt, successText, invokeCommand}) => {
    if (installationInProgress) return;
    const ok = await confirmModal(title, prompt, action, true);
    if (!ok || installationInProgress) return;
    installationInProgress = true;
    const installButtons = ["reinstall-gitrun", "uninstall-gitrun"].map((id) => document.getElementById(id));
    installButtons.forEach((item) => { if (item) item.disabled = true; });
    const status = document.getElementById("installation-status");
    status.textContent = action + "…";
    const modal = document.createElement("div");
    modal.className = "modal-backdrop";
    modal.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="installation-progress-title"><h2 id="installation-progress-title">' + esc(title) + '</h2><p id="installation-progress-status" role="status" aria-live="polite">Waiting for the operation…</p><div id="installation-progress-terminal" class="terminal" role="log" aria-live="polite"></div></section>';
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
      if (unlisten) {
        try { await unlisten(); } catch (error) { console.warn("Could not remove GitRun progress listener:", error); }
      }
      installationInProgress = false;
      installButtons.forEach((item) => { if (item?.isConnected) item.disabled = false; });
    }
  };
  document.getElementById("reinstall-gitrun").addEventListener("click", () => showInstallationProgress({
    title: "Reinstall GitRun?",
    action: "Reinstall",
    prompt: "GitRun will download the latest official release, verify its signed manifest and package checksum, then reinstall it. Existing configuration and credentials are preserved while runtime resources are rebuilt. Administrator authorization may be requested.",
    successText: "Reinstall completed. Closing this dashboard…",
    invokeCommand: "reinstall_gitrun"
  }));
  document.getElementById("uninstall-gitrun").addEventListener("click", () => showInstallationProgress({
    title: "Uninstall GitRun?",
    action: "Uninstall",
    prompt: "This permanently removes the GitRun package, services, configuration, logs, app preferences, runner containers, GitRun-managed cache volumes/networks and runner images. Docker itself and unrelated host workloads are preserved. This cannot be undone.",
    successText: "GitRun has been removed. Closing this dashboard…",
    invokeCommand: "uninstall_gitrun"
  }));
}
