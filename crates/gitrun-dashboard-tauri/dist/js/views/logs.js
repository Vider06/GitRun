import { invoke, content, esc, heading, pill, empty, formatTime } from "../lib.js";

const knownSources = ["gitvault", "gsr-watchdog", "gsr-poll", "gsr-agent", "gsr-exec-supervisor"];
let cachedEvents = [];
let eventError = null;
let cachedJournal = [];
let journalError = null;
let journalSource = "all";
let journalLoaded = false;
let journalLoading = null;
let activePane = "security";

function sourceLabel(source) {
  const labels = { gitvault: "GitVault / VaultEventSink", "gsr-watchdog": "GSR watchdog", "gsr-poll": "GSR host poller", "gsr-agent": "GSR command agent", "gsr-exec-supervisor": "GSR exec supervisor" };
  return labels[source] || source || "Unknown source";
}

function renderSecurityRows() {
  const source = document.getElementById("logs-source")?.value || "all";
  const severity = document.getElementById("logs-severity")?.value || "all";
  const query = (document.getElementById("logs-search")?.value || "").trim().toLowerCase();
  const rows = cachedEvents.filter((event) => {
    const eventSource = String(event.source || "unknown").toLowerCase();
    const level = String(event.severity || "info").toLowerCase();
    if (source !== "all" && (source === "other" ? knownSources.includes(eventSource) : eventSource !== source)) return false;
    if (severity !== "all" && level !== severity) return false;
    return !query || [event.source, event.severity, event.message, event.timestamp].some((value) => String(value ?? "").toLowerCase().includes(query));
  });
  const target = document.getElementById("security-log-rows");
  if (!target) return;
  if (eventError) {
    target.innerHTML = '<div class="notice danger" role="alert"><div><strong>Security event history unavailable</strong><p>' + esc(eventError) + '</p>The event queue could not be read; this is not an empty log.</div></div>';
    const count = document.getElementById("security-log-count");
    if (count) count.textContent = "Read failed";
    return;
  }
  target.innerHTML = rows.length ? rows.map((event) => {
    const level = String(event.severity || "info").toLowerCase();
    const kind = level === "critical" ? "bad" : level === "warning" ? "warn" : "info";
    const mark = kind === "bad" ? "bad" : kind === "warn" ? "warn" : "good";
    return '<article class="activity-item"><span class="activity-mark ' + mark + '"></span><div class="log-entry"><div class="toolbar"><strong>' + esc(sourceLabel(event.source)) + '</strong>' + pill(level, kind) + '</div><p>' + esc(event.message || "No event message recorded") + '</p><p class="subtle mono">' + esc(formatTime(event.timestamp)) + ' · epoch ' + esc(String(event.timestamp ?? "unknown")) + '</p></div></article>';
  }).join("") : empty("No matching security events", cachedEvents.length ? "Change the filters or search terms." : "The event queue is empty. This does not prove that enforcement is healthy.");
  const count = document.getElementById("security-log-count");
  if (count) count.textContent = rows.length + " shown · " + cachedEvents.length + " loaded";
}

function renderJournalRows() {
  const target = document.getElementById("journal-log-rows");
  if (!target) return;
  if (!journalLoaded) {
    target.innerHTML = empty("Journal not loaded", "Open the Service journal tab to fetch recent service output.");
    return;
  }
  if (journalError) {
    target.innerHTML = '<div class="notice danger" role="alert"><div><strong>Service journal unavailable</strong><p>' + esc(journalError) + '</p><p>On Linux, check that the dashboard user can read the systemd journal.</p></div></div>';
    return;
  }
  target.innerHTML = cachedJournal.length ? '<pre class="terminal log-terminal">' + cachedJournal.map(esc).join("\n") + '</pre>' : empty("No service output returned", "The journal query succeeded but returned no matching records.");
}

function renderPanes() {
  document.querySelectorAll("[data-log-pane]").forEach((button) => {
    const selected = button.dataset.logPane === activePane;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-selected", String(selected));
    button.tabIndex = selected ? 0 : -1;
  });
  const securityPane = document.getElementById("security-log-pane");
  const journalPane = document.getElementById("journal-log-pane");
  if (securityPane) securityPane.hidden = activePane !== "security";
  if (journalPane) journalPane.hidden = activePane !== "journal";
}

export async function renderLogs() {
  const generation = window.__gitrunNavigationGeneration;
  journalLoaded = false;
  journalLoading = null;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Loading GitRun security events…</p></div>';
  try {
    cachedEvents = (await invoke("list_gsr_events", { limit: 500 })) || [];
    eventError = null;
  } catch (error) {
    cachedEvents = [];
    eventError = String(error || "Unknown event queue error");
  }
  if (generation !== window.__gitrunNavigationGeneration) return;
  content.innerHTML = heading("OPERATIONS", "Logs", "Security event history and systemd output for GitRun. GitVault event records never contain secret values.", '<button class="btn" data-action="refresh-view">↻ Refresh</button>') +
    '<div class="tabs log-tabs" role="tablist" aria-label="Log views"><button class="tab" id="logs-security-tab" type="button" role="tab" aria-controls="security-log-pane" aria-selected="true" data-log-pane="security">Security events</button><button class="tab" id="logs-journal-tab" type="button" role="tab" aria-controls="journal-log-pane" aria-selected="false" tabindex="-1" data-log-pane="journal">Service journal</button></div>' +
    '<section id="security-log-pane" class="section" role="tabpanel" aria-labelledby="logs-security-tab"><div class="toolbar"><div><h2>GSR event queue</h2><p class="section-description">Persistent JSONL records from GitVault, the watchdog, host poller and runner-side enforcement.</p></div><span id="security-log-count" class="pill info" role="status" aria-live="polite"></span></div>' +
    '<div class="log-filters"><label class="field"><span>Source</span><select id="logs-source"><option value="all">All sources</option><option value="gitvault">GitVault / EventSink</option><option value="gsr-watchdog">GSR watchdog</option><option value="gsr-poll">GSR host poller</option><option value="gsr-agent">GSR command agent</option><option value="gsr-exec-supervisor">GSR exec supervisor</option><option value="other">Other sources</option></select></label><label class="field"><span>Severity</span><select id="logs-severity"><option value="all">All levels</option><option value="critical">Critical</option><option value="warning">Warning</option><option value="info">Info</option></select></label><label class="field log-search"><span>Search</span><input id="logs-search" type="search" placeholder="Search source or message"></label></div>' +
    '<div id="security-log-rows" class="card-list"></div></section>' +
    '<section id="journal-log-pane" class="section" role="tabpanel" aria-labelledby="logs-journal-tab" hidden><div class="toolbar"><div><h2>Systemd journal</h2><p class="section-description">Service stdout/stderr, separate from structured security events.</p></div><div class="heading-actions"><select id="journal-source" aria-label="Service journal source"><option value="all">Scheduler + GSR</option><option value="scheduler">Scheduler (gitrun.service)</option><option value="gsr">GSR (gitrun-gsr.service)</option></select><button class="btn" id="refresh-journal">↻ Reload journal</button></div></div><div id="journal-log-rows" class="card-list"></div></section>';
  activePane = "security";
  renderPanes(); renderSecurityRows(); renderJournalRows();
  document.querySelectorAll("[data-log-pane]").forEach((button) => button.addEventListener("click", () => {
    activePane = button.dataset.logPane;
    renderPanes();
    if (activePane === "journal" && !journalLoaded) void loadJournal(generation);
  }));
  document.getElementById("logs-source").addEventListener("change", renderSecurityRows);
  document.getElementById("logs-severity").addEventListener("change", renderSecurityRows);
  document.getElementById("logs-search").addEventListener("input", renderSecurityRows);
  document.getElementById("journal-source").value = journalSource;
  document.getElementById("journal-source").addEventListener("change", async (event) => { journalSource = event.target.value; journalLoaded = false; await loadJournal(generation); });
  document.getElementById("refresh-journal").addEventListener("click", () => { journalLoaded = false; void loadJournal(generation); });
}

async function loadJournal(generation) {
  if (journalLoading) return journalLoading;
  const source = journalSource;
  const target = document.getElementById("journal-log-rows");
  if (target) target.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Reading service journal…</p></div>';
  // Start in a microtask so journalLoading is assigned before request handlers run.
  const request = Promise.resolve().then(async () => {
    try {
      const rows = await invoke("list_service_logs", { source, limit: 200 });
      if (generation !== window.__gitrunNavigationGeneration || source !== journalSource) return;
      cachedJournal = rows || [];
      journalError = null;
      journalLoaded = true;
    } catch (error) {
      if (generation !== window.__gitrunNavigationGeneration || source !== journalSource) return;
      cachedJournal = [];
      journalError = String(error);
      journalLoaded = true;
    } finally {
      if (journalLoading === request) journalLoading = null;
      if (generation === window.__gitrunNavigationGeneration) {
        if (source !== journalSource) void loadJournal(generation);
        else renderJournalRows();
      }
    }
  });
  journalLoading = request;
  return request;
}
