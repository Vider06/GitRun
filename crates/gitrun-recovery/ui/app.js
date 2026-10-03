const invoke = window.__TAURI__.core.invoke;
const health = document.querySelector("#health");
const summary = document.querySelector("#summary");
const issues = document.querySelector("#issues");
const meta = document.querySelector("#meta");

function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function render(report) {
  const critical = report.issues.filter(function(issue) { return issue.severity === "Critical"; });
  const warnings = report.issues.filter(function(issue) { return issue.severity === "Warning"; });

  health.textContent = critical.length ? "Needs repair" : "Ready";
  health.className = "pill " + (critical.length ? "bad" : warnings.length ? "warn" : "ok");

  summary.textContent = critical.length
    ? critical.length + " blocking problem(s) detected."
    : warnings.length
      ? warnings.length + " warning(s) detected; GitRun can still start."
      : "No blocking problems detected.";

  issues.innerHTML = report.issues.length
    ? report.issues.map(function(issue) {
        return '<article class="issue ' + issue.severity.toLowerCase() + '">' +
          '<div class="issue-top"><strong>' + escapeHtml(issue.title) +
          '</strong><span>' + escapeHtml(issue.severity) +
          '</span></div><p>' + escapeHtml(issue.detail) +
          '</p></article>';
      }).join("")
    : '<article class="issue info"><p>Everything looks healthy.</p></article>';

  const update = report.update;
  const updateText = update.checked
    ? (update.available
      ? "Update: " + escapeHtml(update.current_version) + " → " +
        escapeHtml(update.target_version || "latest")
      : "GitRun " + escapeHtml(update.current_version) + " is current.")
    : "Update check not completed.";

  meta.textContent = updateText + " • Config: " +
    escapeHtml(report.config_path || "not configured");
}

async function refresh() {
  summary.textContent = "Checking…";
  try {
    render(await invoke("get_report"));
  } catch (error) {
    health.textContent = "Recovery error";
    health.className = "pill bad";
    summary.textContent = String(error);
  }
}

document.querySelector("#refresh").addEventListener("click", refresh);

document.querySelector("#gtuu").addEventListener("click", async function() {
  summary.textContent = "Running GTUU…";
  try {
    render(await invoke("run_gtuu"));
  } catch (error) {
    summary.textContent = String(error);
  }
});

document.querySelector("#service").addEventListener("click", async function() {
  summary.textContent = "Repairing systemd service…";
  try {
    await invoke("repair_service");
    await refresh();
  } catch (error) {
    summary.textContent = String(error);
  }
});

document.querySelector("#dashboard").addEventListener("click", async function() {
  try {
    await invoke("open_dashboard");
  } catch (error) {
    summary.textContent = String(error);
  }
});

refresh();
