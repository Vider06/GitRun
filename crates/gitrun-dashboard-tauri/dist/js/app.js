import { invoke, content, esc, toast, errorView, setFooter, applyTheme } from "./lib.js";
import { renderOverview } from "./views/overview.js";

const repoList = document.getElementById("repo-list");
const names = {
  overview: "Overview", security: "Privacy & Security", vault: "GitVault",
  gsr: "GitSecureRun", logs: "Logs", general: "General settings", "app-settings": "App settings",
  recovery: "Recovery"
};
let currentView = "overview";
let overviewCache = null;
let navigationGeneration = 0;
let setupInProgress = false;
let currentCatState = "calm";

function renderLoading(message) {
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>' + esc(message || "Loading…") + '</p></div>';
}
function setConnection(kind, label, detail) {
  const dot = document.getElementById("connection-dot");
  dot.className = "connection-dot " + kind;
  document.getElementById("connection-label").textContent = label;
  document.getElementById("connection-detail").textContent = detail;
}
function setNavigation(view) {
  currentView = view;
  document.querySelectorAll(".nav-item[data-view]").forEach((item) => {
    const active = item.dataset.view === view;
    item.classList.toggle("active", active);
    if (active) item.setAttribute("aria-current", "page");
    else item.removeAttribute("aria-current");
  });
  const label = view.startsWith("repo:") ? view.slice(5) : (names[view] || view);
  document.getElementById("page-crumb").textContent = label;
}

function setSidebarCollapsed(collapsed) {
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  const toggle = document.getElementById("toggle-sidebar");
  if (!toggle) return;
  const expanded = !collapsed;
  const label = expanded ? "Collapse sidebar" : "Expand sidebar";
  toggle.setAttribute("aria-expanded", String(expanded));
  toggle.setAttribute("aria-label", label);
  toggle.title = label;
  toggle.textContent = expanded ? "⇤" : "⇥";
}
async function loadRepoNav() {
  try {
    overviewCache = await invoke("get_overview");
    const repos = overviewCache.repositories || [];
    repoList.innerHTML = repos.length ? repos.map((repo) => '<button class="nav-item" data-view="repo:' + esc(repo) + '" title="' + esc(repo) + '"><span class="repo-glyph">' + esc(repo.slice(0,1).toUpperCase()) + '</span><span>' + esc(repo) + '</span></button>').join("") : '<div class="nav-placeholder">No repositories connected</div>';
    setConnection("ok", "Local backend connected", "Tauri IPC");
    document.getElementById("overview-indicator").innerHTML = '<span class="status-led"></span>';
    setFooter("Local data source connected");
    return overviewCache;
  } catch (error) {
    repoList.innerHTML = '<div class="nav-placeholder">Repository data unavailable</div>';
    setConnection("bad", "Backend unavailable", "IPC error");
    document.getElementById("overview-indicator").textContent = "!";
    setFooter("Backend error: " + error);
    throw error;
  }
}
async function navigate(view, options = {}) {
  const generation = ++navigationGeneration;
  window.__gitrunNavigationGeneration = generation;
  window.__gitrunForceRefresh = Boolean(options.forceRefresh);
  setNavigation(view);
  if (view.startsWith("repo:")) {
    try {
      const { renderRepository } = await import("./views/repositories.js");
      if (generation !== navigationGeneration) return;
      await renderRepository(view.slice(5), navigate);
      if (generation === navigationGeneration) setCatExpression(currentCatState);
    } catch (error) {
      if (generation === navigationGeneration) content.innerHTML = errorView(error);
    } finally {
      if (generation === navigationGeneration) window.__gitrunForceRefresh = false;
    }
    return;
  }
  try {
    if (view === "overview") {
      const cachedOverview = overviewCache;
      overviewCache = null;
      await renderOverview(navigate, cachedOverview);
    }
    else if (view === "security") {
      const { renderSecurity } = await import("./views/security.js");
      if (generation !== navigationGeneration) return;
      await renderSecurity();
    }
    else if (view === "vault") {
      const { renderVault } = await import("./views/vault.js");
      if (generation !== navigationGeneration) return;
      await renderVault();
    }
    else if (view === "gsr") {
      const { renderGsr } = await import("./views/gsr.js");
      if (generation !== navigationGeneration) return;
      await renderGsr();
    }
    else if (view === "logs") {
      const { renderLogs } = await import("./views/logs.js");
      if (generation !== navigationGeneration) return;
      await renderLogs();
    }
    else if (view === "general") {
      const { renderGeneral } = await import("./views/settings.js");
      if (generation !== navigationGeneration) return;
      await renderGeneral();
    }
    else if (view === "app-settings") {
      const { renderAppSettings } = await import("./views/app-settings.js");
      if (generation !== navigationGeneration) return;
      await renderAppSettings();
    }
    else if (view === "recovery") await renderRecovery();
    else await renderOverview(navigate);
    if (generation === navigationGeneration) setCatExpression(currentCatState);
  } catch (error) {
    if (generation === navigationGeneration) content.innerHTML = errorView(error);
  } finally {
    if (generation === navigationGeneration) window.__gitrunForceRefresh = false;
  }
}
async function renderRecovery() {
  const generation = window.__gitrunNavigationGeneration;
  renderLoading("Collecting recovery diagnostics…");
  let report;
  try { report = await invoke("get_dashboard_health", {forceRefresh:Boolean(window.__gitrunForceRefresh)}); }
  catch (error) { if (generation !== window.__gitrunNavigationGeneration) return; content.innerHTML = errorView(error); return; }
  if (generation !== window.__gitrunNavigationGeneration) return;
  const checks = report.checks || [];
  content.innerHTML = '<header class="view-heading"><div><div class="eyebrow">DIAGNOSTICS</div><h1>Recovery</h1><p class="subtitle">A focused view of startup blockers and host health. This screen does not automatically modify the host.</p></div><div class="heading-actions"><button class="btn" data-action="refresh-view">↻ Re-check</button><button class="btn btn-primary" id="recovery-check-updates">Check updates</button></div></header>' +
    '<div class="notice ' + (checks.some((item) => !item.ok) ? "warn" : "success") + '"><div><strong>' + (checks.some((item) => !item.ok) ? "Attention required" : "No issues detected by these checks") + '</strong>These checks are limited to the signals the local backend can currently observe. Use the standalone Recovery utility for privileged repair actions.</div></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Health checks</h2><span class="pill info">' + checks.length + ' checks</span></div><div class="card-list">' + checks.map((item) => '<div class="list-row"><div class="list-row-main"><strong>' + esc(item.name) + '</strong><small>' + esc(item.detail) + '</small></div><span class="pill ' + (item.ok ? "good" : "warn") + '">' + (item.ok ? "OK" : "Check") + '</span></div>').join("") + '</div></div>' +
    '<div id="recovery-update-result" class="section"></div>' +
    '<div class="section panel panel-pad"><div class="section-heading"><h2>Standalone Recovery utility</h2><span class="pill neutral">Privileged actions</span></div><p class="section-description">The recovery launcher can inspect startup state, run GTUU and repair the systemd service with explicit privilege elevation. This dashboard intentionally does not fake a repair command when the backend has not exposed one.</p><div class="kv-grid"><div class="kv"><small>Config</small><strong>' + esc(report.config_path || "Unavailable") + '</strong></div><div class="kv"><small>Version</small><strong>' + esc(report.version || "Unknown") + '</strong></div></div></div>';
  content.querySelector("#recovery-check-updates").addEventListener("click", async () => {
    const target = document.getElementById("recovery-update-result");
    target.innerHTML = '<div class="notice"><div><strong>Checking…</strong></div></div>';
    try { const result = await invoke("check_gitrun_updates"); target.innerHTML = '<div class="notice ' + (result.available ? "warn" : result.checked ? "success" : "danger") + '"><div><strong>' + esc(result.title || "Update check") + '</strong><p>' + esc(result.detail || result.output || result.error || "") + '</p></div></div>'; }
    catch (error) { target.innerHTML = '<div class="notice danger"><div><strong>Update check failed</strong>' + esc(error) + '</div></div>'; }
  });
}
function firstRunComplete() {
  setupInProgress = false;
  document.body.classList.remove("setup-mode");
  navigate("overview");
  if (localStorage.getItem("gitrun-tutorial-complete") !== "true" && localStorage.getItem("gitrun-tutorial-dismissed") !== "true") {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="welcome-tour-title"><div class="mascot-inline"><div class="mascot-face"><svg viewBox="0 0 48 48" aria-hidden="true"><path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><path d="M15 26h.1M33 26h.1" stroke="#11151d" stroke-width="5" stroke-linecap="round"/><path d="M20 33q4 4 8 0" fill="none" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/></svg></div><div class="mascot-copy"><strong id="welcome-tour-title">Welcome to GitRun</strong><p>Your control plane is set up. Want a quick guided tour?</p></div></div><div class="modal-actions"><button class="btn" data-later>Later</button><button class="btn btn-primary" data-start>Start tour</button></div></section>';
    document.body.appendChild(backdrop);
    backdrop.querySelector("[data-later]").onclick = () => { localStorage.setItem("gitrun-tutorial-dismissed","true"); backdrop.remove(); };
    backdrop.querySelector("[data-start]").onclick = () => { localStorage.removeItem("gitrun-tutorial-dismissed"); backdrop.remove(); navigate("app-settings").then(() => document.getElementById("restart-tour")?.click()); };
  }
}
function setCatExpression(state) {
  currentCatState = state || "calm";
  const expressions = {
    calm: '<path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><path d="M15 26h.1M33 26h.1" stroke="#11151d" stroke-width="5" stroke-linecap="round"/><path d="M20 33q4 4 8 0" fill="none" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/>',
    alert: '<path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><circle cx="15" cy="26" r="3" fill="#11151d"/><circle cx="33" cy="26" r="3" fill="#11151d"/><path d="M19 35q5-6 10 0" fill="none" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/>',
    serious: '<path d="M9 19 7 5l13 8a19 19 0 0 1 8 0l13-8-2 14c3 4 4 8 3 13-2 8-9 12-18 12S8 40 6 32c-1-5 0-9 3-13Z" fill="currentColor"/><path d="m11 23 8 3M29 26l8-3" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/><path d="M15 28h.1M33 28h.1" stroke="#11151d" stroke-width="5" stroke-linecap="round"/><path d="M20 35h8" stroke="#11151d" stroke-width="2.5" stroke-linecap="round"/>'
  };
  document.querySelectorAll(".brand-cat svg,.mascot-face svg").forEach((svg) => { svg.innerHTML = expressions[state] || expressions.calm; });
  document.body.classList.remove("cat-alert","cat-serious");
  if (state === "alert") document.body.classList.add("cat-alert");
  if (state === "serious") document.body.classList.add("cat-serious");
}
function applySavedPreferences() {
  const theme = localStorage.getItem("gitrun-theme") || "dark";
  applyTheme(theme);
  document.body.classList.toggle("compact-mode", localStorage.getItem("gitrun-compact-mode") === "true");
  document.body.classList.toggle("cat-hidden", localStorage.getItem("gitrun-cat-enabled") === "false");
}
document.addEventListener("click", async (event) => {
  const nav = event.target.closest("[data-view]");
  if (nav) {
    event.preventDefault();
    if (setupInProgress) return;
    await navigate(nav.dataset.view);
    return;
  }
  const action = event.target.closest("[data-action]");
  if (action) {
    if (action.dataset.action === "refresh-view") {
      await loadRepoNav().catch(() => {});
      await navigate(currentView,{forceRefresh:true});
    } else if (action.dataset.action === "check-updates") {
      await navigate("general");
      document.getElementById("check-updates")?.click();
    }
  }
});
document.getElementById("refresh-view").addEventListener("click", async () => {
  await loadRepoNav().catch(() => {});
  await navigate(currentView,{forceRefresh:true});
});
document.getElementById("refresh-repositories").addEventListener("click", async () => {
  await loadRepoNav().catch((error) => toast("Could not refresh repositories: " + error, "error"));
  toast("Repository list refreshed.");
});
document.getElementById("toggle-sidebar").addEventListener("click", () => {
  const collapsed = !document.body.classList.contains("sidebar-collapsed");
  setSidebarCollapsed(collapsed);
  localStorage.setItem("gitrun-sidebar-collapsed", String(collapsed));
});
window.addEventListener("gitrun:navigate", (event) => navigate(event.detail));
window.addEventListener("gitrun:mascot-state", (event) => setCatExpression(event.detail));
window.addEventListener("gitrun:setup-complete", firstRunComplete);
window.addEventListener("gitrun:config-saved", () => {
  overviewCache = null;
  void loadRepoNav().catch(() => {});
});
window.addEventListener("gitrun:setup-required", async () => {
  setupInProgress = true;
  try {
    const { renderSetup } = await import("./setup.js");
    renderSetup(false, async () => { setupInProgress = false; await loadRepoNav().catch(() => {}); firstRunComplete(); });
  } catch (error) {
    setupInProgress = false;
    content.innerHTML = errorView("Could not load setup: " + error);
  }
});

applySavedPreferences();
setSidebarCollapsed(localStorage.getItem("gitrun-sidebar-collapsed") === "true");
(async () => {
  try {
    const firstRun = await invoke("is_first_run");
    if (firstRun) {
      setupInProgress = true;
      const { renderSetup } = await import("./setup.js");
      renderSetup(false, async () => { setupInProgress = false; await loadRepoNav().catch(() => {}); firstRunComplete(); });
    } else {
      await loadRepoNav();
      firstRunComplete();
      // Version/health probes may need to wait for Docker or systemd. They
      // should update the footer asynchronously, not block the first view.
      void invoke("get_dashboard_health").then((health) => {
        const footerVersion = document.getElementById("footer-version");
        if (footerVersion) {
          const version = health.version == null ? "" : String(health.version).trim();
          footerVersion.textContent = version ? "Version " + version : "Version unavailable";
        }
      }).catch(() => {
        const footerVersion = document.getElementById("footer-version");
        if (footerVersion) footerVersion.textContent = "Version unavailable";
      });
    }
  } catch (error) {
    content.innerHTML = errorView("Could not initialize GitRun: " + error);
    setConnection("bad", "Initialization failed", "Check the local backend");
  }
})();
