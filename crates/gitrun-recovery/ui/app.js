const invoke = window.__TAURI__.core.invoke;
const health = document.querySelector("#health");
const summary = document.querySelector("#summary");
const alertDetail = document.querySelector("#alert-detail");
const issues = document.querySelector("#issues");
const meta = document.querySelector("#meta");
const terminal = document.querySelector("#terminal");
const buttons = Array.from(document.querySelectorAll(".action"));
let busyDepth = 0;

health.setAttribute("role", "status");
health.setAttribute("aria-live", "polite");

function esc(value) {
  return String(value == null ? "" : value)
    .replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;")
    .replaceAll('"',"&quot;").replaceAll("'","&#039;");
}
function log(message, kind) {
  const prefix = new Date().toLocaleTimeString();
  const line = document.createElement("div");
  if (kind) line.className = kind;
  line.textContent = "[" + prefix + "] " + message;
  terminal.appendChild(line);
  while (terminal.childElementCount > 300) terminal.firstElementChild.remove();
  terminal.scrollTop = terminal.scrollHeight;
}
function setBusy(busy, label) {
  busyDepth = Math.max(0, busyDepth + (busy ? 1 : -1));
  const isBusy = busyDepth > 0;
  buttons.forEach((button) => { button.disabled = isBusy; });
  if (busy) log(label || "Running recovery action…");
}
function setMetric(id, value, detail, kind) {
  const valueEl = document.getElementById(id);
  valueEl.textContent = value;
  valueEl.parentElement.classList.toggle("bad", kind === "bad");
  valueEl.parentElement.classList.toggle("good", kind === "good");
  const detailId = id + "-detail";
  const detailEl = document.getElementById(detailId);
  if (detailEl) detailEl.textContent = detail || "";
}
function render(report) {
  if (!report || typeof report !== "object") {
    throw new Error("Recovery returned an invalid diagnostic report.");
  }
  const found = Array.isArray(report.issues)
    ? report.issues.filter((issue) => issue && typeof issue === "object")
    : [];
  const critical = found.filter((issue) => issue.severity === "Critical");
  const warnings = found.filter((issue) => issue.severity === "Warning");
  const ready = critical.length === 0 && warnings.length === 0;
  health.className = "health-state " + (critical.length ? "" : warnings.length ? "warn" : "ready");
  health.querySelector("span:last-child").textContent = critical.length ? "Repair required" : warnings.length ? "Warnings detected" : "Checks passed";
  document.getElementById("alert").className = "alert " + (critical.length ? "" : warnings.length ? "checking" : "healthy");
  summary.textContent = critical.length ? critical.length + " critical issue(s) block normal startup" : warnings.length ? warnings.length + " warning(s) need review" : "No blocking startup issues detected";
  alertDetail.textContent = critical.length ? "Review the critical findings below and choose a targeted recovery action." : warnings.length ? "GitRun may still start, but review these warnings before running untrusted workflows." : "The available checks passed. This is not a guarantee that every runner protection is active.";
  setMetric("check-config", report.config_ok ? "Valid" : "Invalid", report.config_path || "No persistent config path resolved", report.config_ok ? "good" : "bad");
  setMetric("check-docker", report.docker_ok ? "Reachable" : "Unavailable", "Docker daemon access", report.docker_ok ? "good" : "bad");
  setMetric("check-service", report.service_unit_ok ? "Valid" : "Missing / stale", "systemd unit validation", report.service_unit_ok ? "good" : "bad");
  setMetric("check-binary", report.gitrun_binary ? "Found" : "Missing", report.gitrun_binary || "GitRun CLI was not located", report.gitrun_binary ? "good" : "bad");
  document.getElementById("issue-count").textContent = found.length + " finding(s)";
  issues.innerHTML = found.length ? found.map((issue) => {
    const severity = String(issue.severity || "Info").toLowerCase();
    return '<article class="issue ' + esc(severity) + '"><div class="issue-top"><strong>' + esc(issue.title) + '</strong><span class="severity">' + esc(issue.severity) + '</span></div><p>' + esc(issue.detail) + '</p>' + (issue.repairable ? '<p class="repair-note">Recovery path available — choose the appropriate action on the right.</p>' : '<p class="repair-note">Manual review may be required.</p>') + '</article>';
  }).join("") : '<article class="issue info"><div class="issue-top"><strong>All inspected checks passed</strong><span class="severity">Info</span></div><p>No blocking issues were reported by the current diagnostic set.</p></article>';
  const update = report.update && typeof report.update === "object" ? report.update : {};
  let updateTitle = "Update check not completed";
  let updateDetail = "Run GTUU to check GitRun and permanent runner updates.";
  let updateKind = "";
  if (update.checked && update.error && String(update.error).startsWith("GTUU completed:")) {
    updateTitle = "GTUU completed";
    updateDetail = String(update.error).slice("GTUU completed:".length).trim();
  } else if (update.checked && update.error) {
    updateTitle = "Update operation needs review";
    updateDetail = update.error;
    updateKind = "available";
  } else if (update.checked && update.available) {
    updateTitle = "Update available: " + (update.current_version || report.version) + " → " + (update.target_version || "latest");
    updateDetail = update.applied ? "The update was applied." : "GTUU reported a newer target version.";
    updateKind = "available";
  } else if (update.checked && update.applied) {
    updateTitle = "Update operation completed";
    updateDetail = "GitRun version: " + (update.current_version || report.version) + (update.target_version ? " · target: " + update.target_version : "");
  } else if (update.checked) {
    updateTitle = "GitRun " + (update.current_version || report.version || "version unknown");
    updateDetail = update.target_version ? "Target version: " + update.target_version : "GTUU check completed; no structured target version was returned.";
  }
  document.getElementById("update-title").textContent = updateTitle;
  document.getElementById("update-detail").textContent = updateDetail;
  document.getElementById("update-state").textContent = update.checked ? (update.available ? "Available" : "Checked") : "Not checked";
  document.getElementById("update-card").className = "update-card " + updateKind;
  meta.textContent = "GitRun " + (report.version || "unknown") + " · Config: " + (report.config_path || "not configured") + " · Binary: " + (report.gitrun_binary || "not found");
}
async function refresh() {
  setBusy(true, "Running read-only diagnostics…");
  summary.textContent = "Inspecting installation…";
  try {
    const report = await invoke("get_report");
    render(report);
    log("Diagnostic report refreshed.", "success");
  } catch (error) {
    health.className = "health-state";
    health.querySelector("span:last-child").textContent = "Recovery error";
    summary.textContent = "Recovery inspection failed";
    alertDetail.textContent = String(error);
    log("Inspection failed: " + error, "error");
  } finally { setBusy(false); }
}
document.querySelector("#refresh").addEventListener("click", refresh);
document.querySelector("#clear-log").addEventListener("click", () => { terminal.textContent = ""; log("Activity log cleared."); });
document.querySelector("#gtuu").addEventListener("click", async () => {
  const accepted = window.confirm("Run GTUU now? GitRun may update the installed CLI, runner image and eligible idle permanent runner containers. Active workloads should be checked first.");
  if (!accepted) return;
  setBusy(true, "Running GTUU; eligible permanent runner containers may be replaced.");
  try { render(await invoke("run_gtuu")); log("GTUU operation returned. Review the update status and findings.", "success"); }
  catch (error) { log("GTUU failed: " + error, "error"); summary.textContent = "GTUU operation failed"; alertDetail.textContent = String(error); }
  finally { setBusy(false); }
});
document.querySelector("#service").addEventListener("click", async () => {
  const accepted = window.confirm("Repair the GitRun system service? Recovery may rewrite the systemd unit and restart the service. This can interrupt runner reconciliation.");
  if (!accepted) return;
  setBusy(true, "Repairing systemd unit and restarting service…");
  try { await invoke("repair_service"); log("Service repair completed; refreshing diagnostics.", "success"); await refresh(); }
  catch (error) { log("Service repair failed: " + error, "error"); summary.textContent = "Service repair failed"; alertDetail.textContent = String(error); }
  finally { setBusy(false); }
});
async function runJsonRecovery(action, title, confirmation) {
  if (!window.confirm(confirmation)) return;
  setBusy(true, title + "…");
  try {
    const report = await invoke("json_recovery", {action});
    const errors = Array.isArray(report.errors) ? report.errors : [];
    const details = Array.isArray(report.details) ? report.details : [];
    log(title + ": " + Number(report.changed || 0) + " changed, " + Number(report.skipped || 0) + " skipped, " + errors.length + " issue(s).", errors.length ? "error" : "success");
    details.slice(0, 20).forEach((detail) => log(String(detail)));
    errors.slice(0, 20).forEach((detail) => log(String(detail), "error"));
    render(await invoke("get_report"));
  } catch (error) {
    log(title + " failed: " + error, "error");
    summary.textContent = title + " failed";
    alertDetail.textContent = String(error);
  } finally {
    setBusy(false);
  }
}
document.querySelector("#json-repair").addEventListener("click", () => runJsonRecovery(
  "repair",
  "JSON repair attempt",
  "Attempt only conservative normalization: remove a BOM or trailing commas and format valid JSON. Files that remain ambiguous or disagree with their .Back reference will be left unchanged."
));
document.querySelector("#json-restore-backups").addEventListener("click", () => runJsonRecovery(
  "restore-backups",
  "Restore JSON from .Back",
  "This overwrites current JSON configuration files with available sibling .json.Back copies. Changes made since those backups were created may be lost. Continue?"
));
document.querySelector("#json-restore-library").addEventListener("click", () => runJsonRecovery(
  "restore-main-library",
  "Restore Main Library",
  "Download and verify the signed release manifest and library.zip, then replace only files inside /usr/share/gitrun/library. Active configuration files will not be overwritten. Continue?"
));
document.querySelector("#dashboard").addEventListener("click", async () => {
  setBusy(true, "Opening GitRun dashboard…");
  try { await invoke("open_dashboard"); log("Dashboard launch requested.", "success"); }
  catch (error) { log("Dashboard launch failed: " + error, "error"); alertDetail.textContent = String(error); }
  finally { setBusy(false); }
});
refresh();
