import { invoke, content, esc, heading, metric, panel, pill, empty, errorView, toast, setFooter } from "../lib.js";

export async function renderOverview(navigate) {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Reading the local control plane…</p></div>';
  let data, runnerSnapshot, activitySnapshot, resourceSnapshot;
  const forceRefresh = Boolean(window.__gitrunForceRefresh);
  try {
    [data, runnerSnapshot, activitySnapshot, resourceSnapshot] = await Promise.all([
      invoke("get_overview"),
      invoke("get_runner_snapshot", {forceRefresh}).catch((error) => ({checked_at:0,stale:true,complete:false,repositories_checked:0,repositories_total:0,workflow_repositories_checked:0,workflow_runs:[],runners:[],warnings:[String(error)]})),
      invoke("get_repository_activity", {forceRefresh}).catch((error) => ({checked_at:0,stale:true,complete:false,repositories_checked:0,repositories_total:0,repositories:[],warnings:[String(error)]})),
      invoke("get_host_resource_snapshot", {forceRefresh}).catch((error) => ({sampled_at:0,stale:true,error:String(error)}))
    ]);
  } catch (error) { if (generation !== window.__gitrunNavigationGeneration) return; content.innerHTML = errorView(error); return; }
  if (generation !== window.__gitrunNavigationGeneration) return;
  if (data.error) { content.innerHTML = errorView(data.error); return; }
  window.dispatchEvent(new CustomEvent("gitrun:mascot-state",{detail:Number(data.recent_critical_events)>0?"alert":!data.gsr_watching?"serious":"calm"}));
  const repos = Array.isArray(data.repositories) ? data.repositories : [];
  const activity = Array.isArray(activitySnapshot.repositories) ? activitySnapshot.repositories : [];
  const activityKnown = activitySnapshot.repositories_checked === repos.length && repos.length === activitySnapshot.repositories_total;
  const activityByRepo = new Map(activity.map((item) => [item.repository,item]));
  const totalStars = activity.reduce((sum,item)=>sum+Number(item.stars||0),0);
  const totalOpenPrs = activity.reduce((sum,item)=>sum+Number(item.open_pull_requests||0),0);
  const totalMergedPrs = activity.reduce((sum,item)=>sum+Number(item.merged_pull_requests||0),0);
  const runners = Array.isArray(runnerSnapshot.runners) ? runnerSnapshot.runners : [];
  const runnerCountsKnown = repos.length === 0 || (runnerSnapshot.repositories_total === repos.length && runnerSnapshot.repositories_checked === runnerSnapshot.repositories_total);
  const workflowRuns = Array.isArray(runnerSnapshot.workflow_runs) ? runnerSnapshot.workflow_runs : [];
  const workflowCountsKnown = runnerSnapshot.workflow_repositories_checked === repos.length;
  const workflowQueued = workflowRuns.reduce((sum,item)=>sum+Number(item.queued||0),0);
  const workflowActive = workflowRuns.reduce((sum,item)=>sum+Number(item.in_progress||0),0);
  const workflowSucceeded = workflowRuns.reduce((sum,item)=>sum+Number(item.succeeded||0),0);
  const workflowFailed = workflowRuns.reduce((sum,item)=>sum+Number(item.failed||0),0);
  const onlineRunners = runners.filter((runner) => runner.online).length;
  const busyRunners = runners.filter((runner) => runner.busy).length;
  const runnerFreshness = runnerSnapshot.checked_at ? new Date(Number(runnerSnapshot.checked_at) * 1000).toLocaleTimeString() : "Not checked";
  const resourceBar = (label, value) => {
    const known = resourceSnapshot && Number.isFinite(Number(value));
    const number = known ? Math.max(0,Math.min(100,Number(value))) : 0;
    return '<div class="resource-row"><div class="resource-label"><span>' + esc(label) + '</span><strong>' + (known ? number.toFixed(1) + "%" : "Unknown") + '</strong></div><div class="resource-track"><span style="width:' + (known ? number : 0) + '%"></span></div></div>';
  };
  const resourcePanel = '<div class="section panel panel-pad"><div class="section-heading"><h2>Host resource pressure</h2>' + pill(resourceSnapshot && !resourceSnapshot.stale ? "Sampled" : "Unknown / stale", resourceSnapshot && !resourceSnapshot.stale ? "good" : "warn") + '</div><div class="resource-grid">' + resourceBar("CPU",resourceSnapshot && resourceSnapshot.cpu_percent) + resourceBar("Memory",resourceSnapshot && resourceSnapshot.memory_percent) + resourceBar("Critical filesystem",resourceSnapshot && resourceSnapshot.disk_percent) + '</div><p class="field-hint" style="margin-top:12px">Host-wide snapshot only; this is not per-job resource usage. ' + esc(resourceSnapshot && resourceSnapshot.sampled_at ? "Sampled at " + new Date(Number(resourceSnapshot.sampled_at)*1000).toLocaleTimeString() : "No successful resource sample yet.") + (resourceSnapshot && resourceSnapshot.error ? " · " + esc(resourceSnapshot.error) : "") + '</p></div>';
  const runnerRows = runners.map((runner) => '<tr><td>' + esc(runner.repository) + '</td><td><strong>' + esc(runner.name) + '</strong><div class="subtle mono">ID ' + esc(runner.id) + '</div></td><td>' + pill(runner.online ? "Online" : runner.status, runner.online ? "good" : "bad") + '</td><td>' + pill(runner.busy ? "Busy" : "Idle", runner.busy ? "warn" : "neutral") + '</td><td>' + pill(runner.kind, runner.kind === "Permanent" ? "info" : runner.kind === "Dynamic" ? "good" : "warn") + '</td><td>' + esc(runner.container_status || "Unknown") + '</td></tr>').join("");
  const cards = repos.map((repo) => {
    const stats = activityByRepo.get(repo) || {};
    const stars = stats.stars == null ? "Unknown" : stats.stars;
    const openPrs = stats.open_pull_requests == null ? "Unknown" : stats.open_pull_requests;
    const mergedPrs = stats.merged_pull_requests == null ? "Unknown" : stats.merged_pull_requests;
    return \`<article class="panel repo-card"><div class="repo-card-top"><div class="repo-avatar">\${esc(repo.slice(0, 1).toUpperCase())}</div><div class="repo-title"><strong>\${esc(repo)}</strong><small>Configured repository · operational snapshot</small></div>\${pill("Configured", "good")}</div><div class="repo-card-foot"><span class="pill neutral">★ \${esc(stars)}</span><span class="pill neutral">Open PRs \${esc(openPrs)}</span><span class="pill neutral">Merged \${esc(mergedPrs)}</span><span class="pill neutral">Runner pool \${esc(data.min_runners)}–\${esc(data.max_runners)}</span><button class="btn" data-open-repo="\${esc(repo)}">Open repository →</button></div></article>\`;
  }).join("");
  content.innerHTML = heading("CONTROL PLANE", "Overview", "A clear view of what GitRun knows right now. Unknown runtime metrics stay unknown instead of being presented as zero.", '<button class="btn" data-action="refresh-view">↻ Refresh</button>') +
    '<div class="grid metrics-grid">' +
      metric("Repositories", repos.length, "Configured in GitRun", "⌘") +
      metric("Runner pool target", String(data.min_runners) + "–" + String(data.max_runners), "Configured limits, not live capacity", "⇄") +
      metric("GitVault", data.vault_enabled ? "Configured" : "Not configured", "Directory/configuration signal only", "▣") +
      metric("Critical events", data.recent_critical_events, "Recorded in the last 24 hours", "!") +
      metric("Online runners", runnerCountsKnown ? onlineRunners : "Unknown", runnerSnapshot.stale ? "Cached snapshot · " + runnerFreshness : "GitHub snapshot · " + runnerFreshness, "●") +
      metric("Busy runners", runnerCountsKnown ? busyRunners : "Unknown", runnerCountsKnown ? "Observed GitHub runner occupancy" : "Some repositories could not be queried", "↻") +
      metric("Stars", activityKnown ? totalStars : "Unknown", activitySnapshot.stale ? "Cached / stale repository metadata" : "Across configured repositories", "★") +
      metric("Open pull requests", activityKnown ? totalOpenPrs : "Unknown", activityKnown ? "GitHub Search API count" : "Partial repository snapshot", "↗") +
      metric("Merged pull requests", activityKnown ? totalMergedPrs : "Unknown", "GitHub Search API count · may be capped", "✓") +
      metric("Queued workflow runs", workflowCountsKnown ? workflowQueued : "Unknown", "Latest 100 workflow runs per repository", "◷") +
      metric("In-progress runs", workflowCountsKnown ? workflowActive : "Unknown", "Workflow-run status, not individual job count", "↻") +
      metric("Successful runs", workflowCountsKnown ? workflowSucceeded : "Unknown", "Latest 100 workflow runs per repository", "✓") +
      metric("Failed runs", workflowCountsKnown ? workflowFailed : "Unknown", "Latest 100 workflow runs per repository", "!") +
    '</div>' +
    '<div class="notice warn"><div><strong>Telemetry freshness</strong>Runner state is cached for 60 seconds; repository stars and pull-request totals are cached for 5 minutes. Individual job queues, per-job duration and resource usage are still unavailable. The workflow-run metrics below count recent workflow runs, not individual jobs.</div></div>' +
    resourcePanel +
    '<div class="section"><div class="section-heading"><h2>Repositories</h2><span class="muted">' + repos.length + ' configured</span></div>' +
      (cards ? '<div class="grid" style="grid-template-columns:repeat(auto-fit,minmax(270px,1fr))">' + cards + '</div>' : empty("No repositories yet", "Run setup to connect your first repository.")) +
    '</div>' +
    ((runnerSnapshot.warnings && runnerSnapshot.warnings.length) || (activitySnapshot.warnings && activitySnapshot.warnings.length) ? '<div class="notice warn section"><div><strong>Snapshot warnings</strong>' + (runnerSnapshot.warnings || []).concat(activitySnapshot.warnings || []).map((warning) => '<p>' + esc(warning) + '</p>').join("") + '</div></div>' : '') +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Observed runners</h2><span class="muted">' + (runnerCountsKnown ? runners.length + ' runner(s) · complete GitHub query' : 'Partial / unavailable snapshot') + '</span></div>' +
      (runnerRows ? '<div class="table-wrap"><table><thead><tr><th>Repository</th><th>Runner</th><th>Status</th><th>Load</th><th>Type</th><th>Container</th></tr></thead><tbody>' + runnerRows + '</tbody></table></div>' : runnerCountsKnown ? empty("No registered runners found","GitHub returned no registered runners for the configured repositories.") : empty("Runner inventory unavailable","GitHub runner data is unknown until authentication and API access succeed.")) +
      '<p class="field-hint" style="margin-top:10px">Snapshot time: ' + esc(runnerFreshness) + (runnerSnapshot.stale ? ' · stale cached data' : ' · shared 60-second cache') + '</p></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Recent workflow runs</h2><span class="muted">' + (workflowCountsKnown ? 'Latest 100 runs per repository' : 'Partial / unavailable') + '</span></div>' +
      (workflowRuns.length ? '<div class="table-wrap"><table><thead><tr><th>Repository</th><th>Total</th><th>Queued</th><th>In progress</th><th>Success</th><th>Failed</th><th>Cancelled</th></tr></thead><tbody>' + workflowRuns.map((item) => '<tr><td>' + esc(item.repository) + '</td><td>' + esc(item.total) + '</td><td>' + esc(item.queued) + '</td><td>' + esc(item.in_progress) + '</td><td>' + esc(item.succeeded) + '</td><td>' + pill(item.failed, Number(item.failed)>0?"bad":"good") + '</td><td>' + esc(item.cancelled) + '</td></tr>').join("") + '</tbody></table></div>' : empty("Workflow run data unavailable","GitHub Actions permissions or API access may be missing.")) +
      '<p class="field-hint" style="margin-top:10px">Workflow run snapshots share the 60-second cache with runner inventory.</p></div>' +
    '<div class="two-col section">' +
      panel("Runner pool", '<div class="kv-grid"><div class="kv"><small>Minimum target</small><strong>' + esc(data.min_runners) + '</strong></div><div class="kv"><small>Maximum target</small><strong>' + esc(data.max_runners) + '</strong></div><div class="kv"><small>Observed runners</small><strong>' + (runnerCountsKnown ? esc(runners.length) : "Unknown") + '</strong></div><div class="kv"><small>Busy runners</small><strong>' + (runnerCountsKnown ? esc(busyRunners) : "Unknown") + '</strong></div></div>') +
      panel("System signals", '<div class="activity-item"><span class="activity-mark ' + (data.gsr_watching ? "good" : "bad") + '"></span><div><strong>GSR watchdog</strong><p>' + (data.gsr_watching ? "Process detected; enforcement still requires verification." : "Watchdog process was not detected.") + '</p></div></div><div class="activity-item"><span class="activity-mark ' + (data.vault_enabled ? "good" : "") + '"></span><div><strong>GitVault</strong><p>' + (data.vault_enabled ? "A vault directory is configured; storage health is not yet confirmed." : "Vault directory is not configured.") + '</p></div></div><div class="activity-item"><span class="activity-mark ' + (Number(data.recent_critical_events) > 0 ? "bad" : "good") + '"></span><div><strong>Security events</strong><p>' + esc(data.recent_critical_events) + ' critical event(s) recorded in the last 24 hours.</p></div></div>')
    + '</div>';
  content.querySelectorAll("[data-open-repo]").forEach((button) => button.addEventListener("click", () => navigate("repo:" + button.dataset.openRepo)));
  setFooter("Overview refreshed · " + new Date().toLocaleTimeString());
}
