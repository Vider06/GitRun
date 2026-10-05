// GitRun dashboard frontend logic.
//
// No framework (React/Vue/etc): the view surface here is small enough
// (6 views, no complex client-side state beyond "which repo is selected")
// that a framework would add build tooling and bundle size without solving
// a real problem. Plain DOM + template strings, one render function per
// view, all wired to real Tauri commands from `../src-tauri/src/lib.rs` —
// nothing here is mocked.

const { invoke } = window.__TAURI__.core;

const content = document.getElementById("content");
const repoListEl = document.getElementById("repo-list");
const navButtons = () => document.querySelectorAll(".nav-item");

let state = {
  view: "overview",
  selectedRepo: null,
  overview: null,
  firstRun: false,
  navigationGeneration: 0,
};

async function refreshOverview(generation = state.navigationGeneration) {
  try {
    const overview = await invoke("get_overview");
    if (generation !== state.navigationGeneration) return false;
    state.overview = overview;
  } catch (error) {
    if (generation !== state.navigationGeneration) return false;
    state.overview = { error: String(error) };
  }
  return true;
}

function renderRepoNav() {
  const repos = state.overview?.repositories || [];
  repoListEl.innerHTML = repos
    .map(
      (repo) => `<li><button class="nav-item ${state.view === "repo:" + repo ? "active" : ""}" data-view="repo:${repo}">${escapeHtml(repo)}</button></li>`
    )
    .join("");
}

function setActiveNav() {
  navButtons().forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.view === state.view);
  });
}

let modalTitleCounter = 0;

function escapeHtml(value) {
  const div = document.createElement("div");
  div.textContent = value ?? "";
  return div.innerHTML;
}

function enhanceModalAccessibility(backdrop, { dismissOnEscape = true } = {}) {
  const modal = backdrop.querySelector(".modal");
  if (!modal) return;

  const title = modal.querySelector(".modal-title");
  if (title) {
    if (!title.id) title.id = `modal-title-${++modalTitleCounter}`;
    modal.setAttribute("aria-labelledby", title.id);
  }
  modal.setAttribute("role", "dialog");
  modal.setAttribute("aria-modal", "true");

  const focusTarget = modal.querySelector(
    "input, select, textarea, button:not([disabled]), a[href]"
  );
  queueMicrotask(() => focusTarget?.focus());

  if (dismissOnEscape) {
    backdrop.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        backdrop.remove();
      }
    });
  }
}

function statusPill(ok, trueLabel, falseLabel) {
  return ok
    ? `<span class="pill pill-success">${trueLabel}</span>`
    : `<span class="pill pill-muted">${falseLabel}</span>`;
}

// ---------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------

function renderOverview() {
  const data = state.overview;
  if (!data) {
    content.innerHTML = `<div class="empty-state">Loading…</div>`;
    return;
  }
  if (data.error) {
    content.innerHTML = errorBanner(data.error);
    return;
  }

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">Overview</h1>
      <p class="view-subtitle">Fleet-wide status across every configured repository.</p>
    </div>

    <div class="card-grid">
      <div class="card">
        <p class="card-stat-label">Repositories</p>
        <p class="card-stat-value">${data.repositories.length}</p>
      </div>
      <div class="card">
        <p class="card-stat-label">Runner pool</p>
        <p class="card-stat-value">${data.min_runners}–${data.max_runners}</p>
      </div>
      <div class="card">
        <p class="card-stat-label">GitVault</p>
        <p class="card-stat-value">${statusPill(data.vault_enabled, "Enabled", "Disabled")}</p>
      </div>
      <div class="card">
        <p class="card-stat-label">GSR watchdog</p>
        <p class="card-stat-value">${statusPill(data.gsr_watching, "Watching", "Not running")}</p>
      </div>
      <div class="card">
        <p class="card-stat-label">Critical events</p>
        <p class="card-stat-value" style="color:${data.recent_critical_events > 0 ? "var(--danger)" : "var(--text-primary)"}">${data.recent_critical_events}</p>
      </div>
    </div>

    <div class="section">
      <h2 class="section-title">Repositories</h2>
      ${
        data.repositories.length === 0
          ? `<div class="empty-state">No repositories configured yet. Add one in Settings.</div>`
          : `<div class="table-scroll"><table class="data-table">
              <thead><tr><th>Repository</th><th></th></tr></thead>
              <tbody>
                ${data.repositories
                  .map(
                    (repo) => `<tr>
                      <td class="mono">${escapeHtml(repo)}</td>
                      <td><button class="btn btn-sm" data-open-repo="${escapeHtml(repo)}">Open →</button></td>
                    </tr>`
                  )
                  .join("")}
              </tbody>
            </table></div>`
      }
    </div>
  `;

  content.querySelectorAll("[data-open-repo]").forEach((btn) => {
    btn.addEventListener("click", () => navigate("repo:" + btn.dataset.openRepo));
  });
}

// ---------------------------------------------------------------------
// Per-repo view (Logic Containers lives here, per the operator's direction)
// ---------------------------------------------------------------------

async function renderRepoView(repo) {
  const generation = state.navigationGeneration;
  content.innerHTML = `<div class="empty-state">Loading ${escapeHtml(repo)}…</div>`;
  let detail;
  try {
    detail = await invoke("get_repo_detail", { repo });
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "repo:" + repo) return;



  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title mono">${escapeHtml(repo)}</h1>
      <p class="view-subtitle">GitVault groups: ${
        detail.vault_groups.length ? detail.vault_groups.map((g) => `<span class="pill pill-info">${escapeHtml(g)}</span>`).join(" ") : '<span class="text-muted">none</span>'
      }</p>
    </div>

    <div class="section">
      <div class="toolbar">
        <h2 class="section-title" style="margin:0">Logic Containers rules</h2>
        <button class="btn btn-primary btn-sm" id="add-rule-btn">+ Add rule</button>
      </div>
      <p class="field-hint" style="margin-bottom:12px">
        Rules decide which backend/image a dynamic runner uses, based on a queued job's labels.
        First matching rule wins. These rules are global (shared across repos) but shown here
        since this is where you'll usually want to add one.
      </p>
      ${
        detail.logic_rules.length === 0
          ? `<div class="empty-state">No Logic Containers rules configured. Dynamic runners use the default image.</div>`
          : `<div class="table-scroll"><table class="data-table">
              <thead><tr><th>Rule</th><th>Match labels</th><th>Backend</th><th>Image</th><th></th></tr></thead>
              <tbody>
                ${detail.logic_rules
                  .map(
                    (rule, i) => `<tr>
                      <td>${escapeHtml(rule.name)}</td>
                      <td>${rule.match_labels.map((l) => `<span class="pill pill-muted">${escapeHtml(l)}</span>`).join(" ")}</td>
                      <td>${formatBackend(rule.backend)}</td>
                      <td class="mono">${escapeHtml(rule.image)}</td>
                      <td><button class="btn btn-sm btn-danger" data-delete-rule="${i}">Remove</button></td>
                    </tr>`
                  )
                  .join("")}
              </tbody>
            </table></div>`
      }
    </div>
  `;

  document.getElementById("add-rule-btn").addEventListener("click", () => openRuleModal(detail.logic_rules));
  content.querySelectorAll("[data-delete-rule]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const index = Number(btn.dataset.deleteRule);
      const rule = detail.logic_rules[index];
      if (!rule) return;
      if (!confirm(`Remove Logic Containers rule "${rule.name}"? This cannot be undone.`)) return;
      try {
        const updated = detail.logic_rules.filter((_, i) => i !== index);
        await invoke("save_logic_rules", { rules: updated });
        await navigate("repo:" + repo);
      } catch (error) {
        alert("Could not remove rule: " + error);
      }
    });
  });
}

// Logic Containers rules are stored globally (one file), not per-repo — this
// is a placeholder hook in case a future revision scopes rules to a repo the
// same way GitVault secrets are scoped. Today it shows every rule under
// every repo view, which is honest about the current (global) behavior
// rather than pretending it's already per-repo.
function formatBackend(backend) {
  if (backend === "LocalLinux" || backend?.LocalLinux !== undefined) {
    return `<span class="pill pill-info">Local Linux</span>`;
  }
  const vmName = backend?.Vm?.vm_name;
  return `<span class="pill pill-warning">VM: ${escapeHtml(vmName || "?")}</span>`;
}

function openRuleModal(existingRules) {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = `
    <div class="modal">
      <h3 class="modal-title">New Logic Containers rule</h3>
      <div class="field">
        <label>Rule name</label>
        <input type="text" id="rule-name" placeholder="windows-jobs" />
      </div>
      <div class="field">
        <label>Match labels (comma-separated, ALL must be present)</label>
        <input type="text" id="rule-labels" placeholder="windows" />
      </div>
      <div class="field">
        <label>Backend</label>
        <select id="rule-backend">
          <option value="local">Local Linux host</option>
          <option value="vm">VM (by name)</option>
        </select>
      </div>
      <div class="field" id="vm-name-field" style="display:none">
        <label>VM name</label>
        <input type="text" id="rule-vm-name" placeholder="win-host" />
      </div>
      <div class="field">
        <label>Runner image</label>
        <input type="text" id="rule-image" placeholder="gitrun-runner:windows" />
      </div>
      <div class="modal-actions">
        <button class="btn" id="rule-cancel">Cancel</button>
        <button class="btn btn-primary" id="rule-save">Save rule</button>
      </div>
    </div>
  `;
  document.body.appendChild(backdrop);
  enhanceModalAccessibility(backdrop);

  const backendSelect = backdrop.querySelector("#rule-backend");
  const vmField = backdrop.querySelector("#vm-name-field");
  backendSelect.addEventListener("change", () => {
    vmField.style.display = backendSelect.value === "vm" ? "block" : "none";
  });

  backdrop.querySelector("#rule-cancel").addEventListener("click", () => backdrop.remove());
  backdrop.querySelector("#rule-save").addEventListener("click", async () => {
    const name = backdrop.querySelector("#rule-name").value.trim();
    const labels = backdrop
      .querySelector("#rule-labels")
      .value.split(",")
      .map((l) => l.trim())
      .filter(Boolean);
    const image = backdrop.querySelector("#rule-image").value.trim();
    const backend =
      backendSelect.value === "vm"
        ? { Vm: { vm_name: backdrop.querySelector("#rule-vm-name").value.trim() } }
        : "LocalLinux";

    if (!name || labels.length === 0 || !image) {
      alert("Rule name, at least one match label, and an image are all required.");
      return;
    }

    const updated = [...existingRules, { name, match_labels: labels, backend, image }];
    try {
      await invoke("save_logic_rules", { rules: updated });
      backdrop.remove();
      renderRepoView(state.selectedRepo);
    } catch (error) {
      alert("Could not save rule: " + error);
    }
  });
}

// ---------------------------------------------------------------------
// GitVault
// ---------------------------------------------------------------------

async function renderGitVault() {
  const generation = state.navigationGeneration;
  content.innerHTML = `<div class="empty-state">Loading…</div>`;
  let secrets;
  try {
    secrets = await invoke("list_vault_secrets");
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "gitvault") return;

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">GitVault</h1>
      <p class="view-subtitle">Encrypted secrets available to runner containers. Values are never shown once saved.</p>
    </div>

    <div class="toolbar">
      <span></span>
      <button class="btn btn-primary btn-sm" id="add-secret-btn">+ Add secret</button>
    </div>

    ${
      secrets.length === 0
        ? `<div class="empty-state">No secrets stored yet.</div>`
        : `<div class="table-scroll"><table class="data-table">
            <thead><tr><th>Name</th><th>Scope</th><th>Last updated</th><th></th></tr></thead>
            <tbody>
              ${secrets
                .map(
                  (s, i) => `<tr>
                    <td class="mono">${escapeHtml(s.name)}</td>
                    <td>${formatScope(s.scope)}</td>
                    <td class="text-muted">${formatTimestamp(s.updated_at)}</td>
                    <td><button class="btn btn-sm btn-danger" data-delete-secret-index="${i}">Delete</button></td>
                  </tr>`
                )
                .join("")}
            </tbody>
          </table></div>`
    }
  `;

  document.getElementById("add-secret-btn").addEventListener("click", () => openSecretModal());
  content.querySelectorAll("[data-delete-secret-index]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const index = Number(btn.dataset.deleteSecretIndex);
      const s = secrets[index];
      if (!s) return;
      if (!confirm(`Delete secret "${s.name}"? This cannot be undone.`)) return;
      try {
        await invoke("delete_vault_secret", { name: s.name, scope: s.scope });
        await navigate("gitvault");
      } catch (error) {
        alert("Could not delete secret: " + error);
      }
    });
  });
}

function formatScope(scope) {
  if (scope.kind === "Global") return `<span class="pill pill-info">Global</span>`;
  if (scope.kind === "Group") return `<span class="pill pill-warning">Group: ${escapeHtml(scope.value)}</span>`;
  return `<span class="pill pill-muted">Repo: ${escapeHtml(scope.value)}</span>`;
}

function formatTimestamp(unixSeconds) {
  if (!unixSeconds) return "—";
  return new Date(unixSeconds * 1000).toLocaleString();
}

function openSecretModal() {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = `
    <div class="modal">
      <h3 class="modal-title">New secret</h3>
      <div class="field">
        <label>Name (environment variable style, e.g. DEPLOY_KEY)</label>
        <input type="text" id="secret-name" placeholder="DEPLOY_KEY" />
      </div>
      <div class="field">
        <label>Value</label>
        <input type="password" id="secret-value" placeholder="••••••••" />
      </div>
      <div class="field">
        <label>Scope</label>
        <select id="secret-scope">
          <option value="Global">Global — every repo</option>
          <option value="Group">Group — repos sharing a group name</option>
          <option value="Repo">Repo — one specific repository</option>
        </select>
      </div>
      <div class="field" id="scope-value-field" style="display:none">
        <label id="scope-value-label">Group name</label>
        <input type="text" id="scope-value" />
      </div>
      <div class="modal-actions">
        <button class="btn" id="secret-cancel">Cancel</button>
        <button class="btn btn-primary" id="secret-save">Save secret</button>
      </div>
    </div>
  `;
  document.body.appendChild(backdrop);
  enhanceModalAccessibility(backdrop);

  const scopeSelect = backdrop.querySelector("#secret-scope");
  const scopeField = backdrop.querySelector("#scope-value-field");
  const scopeLabel = backdrop.querySelector("#scope-value-label");
  scopeSelect.addEventListener("change", () => {
    scopeField.style.display = scopeSelect.value === "Global" ? "none" : "block";
    scopeLabel.textContent = scopeSelect.value === "Group" ? "Group name" : "Repository (owner/repo)";
  });

  backdrop.querySelector("#secret-cancel").addEventListener("click", () => backdrop.remove());
  backdrop.querySelector("#secret-save").addEventListener("click", async () => {
    const name = backdrop.querySelector("#secret-name").value.trim();
    const value = backdrop.querySelector("#secret-value").value;
    const scopeKind = scopeSelect.value;
    const scopeValue = backdrop.querySelector("#scope-value").value.trim();

    if (!name || !value) {
      alert("Name and value are both required.");
      return;
    }
    if (scopeKind !== "Global" && !scopeValue) {
      alert("A group/repo name is required for this scope.");
      return;
    }

    const scope = scopeKind === "Global" ? { kind: "Global" } : { kind: scopeKind, value: scopeValue };
    try {
      await invoke("set_vault_secret", { name, value, scope });
      backdrop.remove();
      renderGitVault();
    } catch (error) {
      alert("Could not save secret: " + error);
    }
  });
}

// ---------------------------------------------------------------------
// GSR
// ---------------------------------------------------------------------

async function renderGsr() {
  const generation = state.navigationGeneration;
  content.innerHTML = `<div class="empty-state">Loading…</div>`;
  let status, events, zizmor;
  try {
    [status, events, zizmor] = await Promise.all([
      invoke("get_gsr_status"),
      invoke("list_gsr_events", { limit: 50 }),
      invoke("get_zizmor_info"),
    ]);
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "gsr") return;

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">GSR</h1>
      <p class="view-subtitle">GitSecureRun watchdog: external process supervision and security events.</p>
    </div>

    <div class="card-grid">
      <div class="card">
        <p class="card-stat-label">Watchdog status</p>
        <p class="card-stat-value">${statusPill(status.watching, "Watching", "Not running")}</p>
      </div>
      <div class="card">
        <p class="card-stat-label">Watched PID</p>
        <p class="card-stat-value">${status.watched_pid ?? "—"}</p>
      </div>
    </div>

    <div class="section" style="max-width:640px">
      <h2 class="section-title">Workflow validation</h2>
      <label class="field-checkbox">
        <input type="checkbox" id="zizmor-toggle" ${zizmor.enabled ? "checked" : ""} />
        <span><strong>Validate YML using a deeper parser by ${escapeHtml(zizmor.info.author)}</strong></span>
      </label>
      <p class="field-hint" style="margin:6px 0 0 26px">
        Runs the third-party <span class="mono">zizmor</span> static analyzer against workflow files
        before a job starts, in addition to GitRun's own built-in checks.
        <a href="#" id="zizmor-learn-more">Learn more</a>
      </p>
    </div>

    <div class="section">
      <h2 class="section-title">Recent security events</h2>
      ${
        events.length === 0
          ? `<div class="empty-state">No events recorded.</div>`
          : `<div class="table-scroll"><table class="data-table">
              <thead><tr><th>Time</th><th>Severity</th><th>Source</th><th>Message</th></tr></thead>
              <tbody>
                ${events
                  .map(
                    (e) => `<tr>
                      <td class="text-muted">${formatTimestamp(e.timestamp)}</td>
                      <td>${severityPill(e.severity)}</td>
                      <td class="mono">${escapeHtml(e.source)}</td>
                      <td>${escapeHtml(e.message)}</td>
                    </tr>`
                  )
                  .join("")}
              </tbody>
            </table></div>`
      }
    </div>
  `;

  document.getElementById("zizmor-learn-more").addEventListener("click", (event) => {
    event.preventDefault();
    navigate("zizmor-info");
  });

  document.getElementById("zizmor-toggle").addEventListener("change", async (event) => {
    const wantsOn = event.target.checked;
    if (!wantsOn) {
      // Turning off never needs consent - only enabling does.
      try {
        await invoke("disable_zizmor");
      } catch (error) {
        alert("Could not disable zizmor: " + error);
        event.target.checked = true;
      }
      return;
    }
    // Revert the checkbox visually until the consent flow actually
    // completes — enabling isn't instantaneous (a real install may run),
    // and the checkbox shouldn't look "on" before that succeeds.
    event.target.checked = false;
    if (zizmor.license_accepted) {
      await enableZizmorWithInstallFeedback(event.target);
    } else {
      openZizmorLicenseModal(zizmor.info, event.target);
    }
  });
}

/// Shared "enable" flow used both when the license was already accepted in
/// a previous session (no dialog needed again) and right after the modal's
/// Accept button.
async function enableZizmorWithInstallFeedback(checkboxEl) {
  const hint = document.createElement("span");
  hint.className = "field-hint";
  hint.style.marginLeft = "8px";
  hint.textContent = "Installing…";
  checkboxEl.parentElement.appendChild(hint);
  try {
    const outcome = await invoke("accept_zizmor_license_and_install");
    if (outcome.status === "Failed") {
      checkboxEl.checked = false;
      hint.textContent = "Could not enable zizmor: " + outcome.detail;
      hint.style.color = "var(--danger)";
    } else {
      hint.remove();
    }
  } catch (error) {
    checkboxEl.checked = false;
    hint.textContent = "Error: " + error;
    hint.style.color = "var(--danger)";
  }
}

function openZizmorLicenseModal(info, checkboxEl) {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = `
    <div class="modal">
      <h3 class="modal-title">Enable zizmor?</h3>
      <p class="field-hint" style="margin-bottom:10px">${escapeHtml(info.description)}</p>
      <p class="field-hint" style="margin-bottom:10px">
        <strong>${escapeHtml(info.name)}</strong> by ${escapeHtml(info.author)} —
        <a href="${escapeHtml(info.repository)}" target="_blank" rel="noopener">${escapeHtml(info.repository)}</a>
      </p>
      <div class="mono" style="white-space:pre-wrap;background:var(--bg-input);border:1px solid var(--border);padding:10px;border-radius:var(--radius-sm);font-size:12px;margin-bottom:12px">${escapeHtml(info.license_name)}

${escapeHtml(info.terms_summary)}

Full license text: ${escapeHtml(info.license_url)}</div>
      <div class="modal-actions">
        <button class="btn" id="zizmor-decline">Cancel</button>
        <button class="btn btn-primary" id="zizmor-accept">I accept — enable zizmor</button>
      </div>
    </div>
  `;
  document.body.appendChild(backdrop);
  enhanceModalAccessibility(backdrop);

  backdrop.querySelector("#zizmor-decline").addEventListener("click", () => backdrop.remove());
  backdrop.querySelector("#zizmor-accept").addEventListener("click", async () => {
    backdrop.remove();
    await enableZizmorWithInstallFeedback(checkboxEl);
  });
}

async function renderZizmorInfo() {
  const generation = state.navigationGeneration;
  content.innerHTML = `<div class="empty-state">Loading…</div>`;
  let zizmor;
  try {
    zizmor = await invoke("get_zizmor_info");
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "zizmor-info") return;
  const info = zizmor.info;
  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">${escapeHtml(info.name)}</h1>
      <p class="view-subtitle">Third-party workflow analyzer, optionally used by GSR's workflow validation step.</p>
    </div>
    <div class="section" style="max-width:640px">
      <p style="margin-bottom:12px">${escapeHtml(info.description)}</p>
      <div class="table-scroll"><table class="data-table" style="margin-bottom:16px">
        <tbody>
          <tr><td class="text-muted">Author</td><td>${escapeHtml(info.author)}</td></tr>
          <tr><td class="text-muted">Homepage</td><td><a href="${escapeHtml(info.homepage)}" target="_blank" rel="noopener">${escapeHtml(info.homepage)}</a></td></tr>
          <tr><td class="text-muted">Repository</td><td><a href="${escapeHtml(info.repository)}" target="_blank" rel="noopener">${escapeHtml(info.repository)}</a></td></tr>
          <tr><td class="text-muted">License</td><td>${escapeHtml(info.license_name)} — <a href="${escapeHtml(info.license_url)}" target="_blank" rel="noopener">full text</a></td></tr>
          <tr><td class="text-muted">Installed locally</td><td>${zizmor.already_installed ? "Yes" : "No — installed automatically on first enable"}</td></tr>
          <tr><td class="text-muted">Currently enabled</td><td>${zizmor.enabled ? "Yes" : "No"}</td></tr>
        </tbody>
      </table></div>
      <p class="field-hint" style="margin-bottom:16px">${escapeHtml(info.terms_summary)}</p>
      <button class="btn" data-view="gsr">Back to GSR</button>
    </div>
  `;
}

function severityPill(severity) {
  if (severity === "Critical") return `<span class="pill pill-danger">Critical</span>`;
  if (severity === "Warning") return `<span class="pill pill-warning">Warning</span>`;
  return `<span class="pill pill-info">Info</span>`;
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

async function renderSettings() {
  const generation = state.navigationGeneration;
  content.innerHTML = `<div class="empty-state">Loading…</div>`;
  let config;
  try {
    config = await invoke("get_config");
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "settings") return;

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">Settings</h1>
      <p class="view-subtitle">Core scheduling configuration.</p>
    </div>

    <div class="section" style="max-width:520px">
      <div class="field">
        <label>Repositories (comma-separated)</label>
        <input type="text" id="cfg-repositories" value="${escapeHtml(config.repositories.join(","))}" />
      </div>
      <div class="field-row">
        <div class="field">
          <label>Min runners</label>
          <input type="number" id="cfg-min" value="${config.min_runners}" min="1" />
        </div>
        <div class="field">
          <label>Max runners</label>
          <input type="number" id="cfg-max" value="${config.max_runners}" min="1" />
        </div>
      </div>
      <div class="field-row">
        <div class="field">
          <label>Idle timeout (s)</label>
          <input type="number" id="cfg-idle" value="${config.idle_timeout}" min="0" />
        </div>
        <div class="field">
          <label>Poll interval (s)</label>
          <input type="number" id="cfg-poll" value="${config.poll_interval}" min="1" />
        </div>
      </div>
      <div class="field">
        <label>Runner image</label>
        <input type="text" id="cfg-image" value="${escapeHtml(config.runner_image)}" />
      </div>
      <button class="btn btn-primary" id="save-settings-btn">Save settings</button>
      <span id="save-status" class="field-hint"></span>
    </div>
  `;

  document.getElementById("save-settings-btn").addEventListener("click", async () => {
    const updated = {
      ...config,
      repositories: document.getElementById("cfg-repositories").value.split(",").map((s) => s.trim()).filter(Boolean),
      min_runners: Number(document.getElementById("cfg-min").value),
      max_runners: Number(document.getElementById("cfg-max").value),
      idle_timeout: Number(document.getElementById("cfg-idle").value),
      poll_interval: Number(document.getElementById("cfg-poll").value),
      runner_image: document.getElementById("cfg-image").value.trim(),
    };
    const statusEl = document.getElementById("save-status");
    try {
      await invoke("save_config", { updated });
      statusEl.textContent = "Saved.";
      statusEl.style.color = "var(--success)";
    } catch (error) {
      statusEl.textContent = "Error: " + error;
      statusEl.style.color = "var(--danger)";
    }
  });
}

// ---------------------------------------------------------------------
// Hypervisor decisions — global, cross-view prompt. Polled independently
// of `navigate()`/`state.view` because this is time-sensitive (the
// autoscaler's background thread only waits up to 5 minutes for an
// answer — see `gitrun-scheduler::vm_resolution` — regardless of which
// page the operator happens to be looking at) and can't wait for them to
// visit a specific screen.
// ---------------------------------------------------------------------

const openHypervisorModals = new Set(); // vm_name -> avoid opening a duplicate modal on the next poll tick while one is already up

async function pollHypervisorDecisions() {
  let pending;
  try {
    pending = await invoke("list_pending_hypervisor_decisions");
  } catch (error) {
    // Non-fatal and easy to hit before GITRUN_STATE_DIR/config is fully
    // set up (e.g. during first-run setup) — stay quiet rather than
    // interrupting whatever view the operator is on with an error banner
    // for a background poll they didn't initiate.
    console.warn("could not poll hypervisor decisions:", error);
    return;
  }
  for (const decision of pending) {
    if (openHypervisorModals.has(decision.vm_name)) continue;
    openHypervisorDecisionModal(decision);
  }
}

function openHypervisorDecisionModal(decision) {
  openHypervisorModals.add(decision.vm_name);

  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = `
    <div class="modal">
      <h3 class="modal-title">KVM isn't available for VM "${escapeHtml(decision.vm_name)}"</h3>
      <p class="field-hint" style="margin-bottom:12px">
        The autoscaler tried to bring this VM up under KVM and it failed:
      </p>
      <pre class="mono" style="white-space:pre-wrap;background:var(--bg-input);border:1px solid var(--border);padding:10px;border-radius:var(--radius-sm);font-size:12px;margin-bottom:12px">${escapeHtml(decision.error)}</pre>
      <p class="field-hint" style="margin-bottom:12px">
        You can retry KVM (e.g. if you just fixed the problem), or use VirtualBox for this VM instead.
        No response within 5 minutes of the original request and this attempt is abandoned — the
        autoscaler will try KVM again automatically the next time this VM is needed.
      </p>
      <div class="modal-actions">
        <button class="btn" id="hv-retry-kvm">Retry KVM</button>
        <button class="btn btn-primary" id="hv-use-virtualbox">Use VirtualBox</button>
      </div>
    </div>
  `;
  document.body.appendChild(backdrop);
  enhanceModalAccessibility(backdrop, { dismissOnEscape: false });

  async function respond(choice) {
    try {
      await invoke("respond_hypervisor_decision", { vmName: decision.vm_name, choice });
      backdrop.remove();
      openHypervisorModals.delete(decision.vm_name);
    } catch (error) {
      alert("Could not send decision: " + error);
    }
  }

  backdrop.querySelector("#hv-retry-kvm").addEventListener("click", () => respond("retry_kvm"));
  backdrop.querySelector("#hv-use-virtualbox").addEventListener("click", () => respond("use_virtual_box"));
  // Deliberately no "Cancel"/backdrop-click-to-dismiss here, unlike the
  // other modals in this file: dismissing without answering would leave
  // the autoscaler's background thread waiting out the full 5 minutes for
  // nothing, and the operator can still just wait for the timeout
  // themselves if they'd rather not decide right now — but that should be
  // a deliberate no-action, not an accidental click-away.
}

// ---------------------------------------------------------------------
// First-run setup
// ---------------------------------------------------------------------

function setDashboardShell(visible) {
  const sidebar = document.querySelector(".sidebar");
  if (sidebar) sidebar.style.display = visible ? "" : "none";
  if (content) {
    content.style.gridColumn = visible ? "" : "1 / -1";
    content.style.padding = visible ? "" : "48px";
  }
}

function renderFirstRun() {
  setDashboardShell(false);
  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">Welcome to GitRun</h1>
      <p class="view-subtitle">Complete the one-time setup before the dashboard can manage your runners.</p>
    </div>

    <div class="section" style="max-width:640px">
      <div class="card">
        <div class="field">
          <label for="setup-auth-mode">GitHub authentication</label>
          <select id="setup-auth-mode">
            <option value="pat">Personal Access Token</option>
            <option value="app">GitHub App</option>
          </select>
          <p class="field-hint">Choose the credential source GitRun will use for GitHub API access.</p>
        </div>

        <div id="setup-pat-fields">
          <div class="field">
            <label for="setup-token">GitHub Personal Access Token</label>
            <input type="password" id="setup-token" autocomplete="off" spellcheck="false" placeholder="github_pat_…" />
            <p class="field-hint">The token is sent only to the local privileged setup step and is not persisted by the dashboard itself.</p>
          </div>
        </div>

        <div id="setup-app-fields" style="display:none">
          <div class="field">
            <label for="setup-app-id">GitHub App ID</label>
            <input type="text" id="setup-app-id" inputmode="numeric" autocomplete="off" placeholder="123456" />
          </div>
          <div class="field">
            <label for="setup-installation-id">GitHub App Installation ID</label>
            <input type="text" id="setup-installation-id" inputmode="numeric" autocomplete="off" placeholder="12345678" />
          </div>
          <div class="field">
            <label for="setup-private-key-path">Private key PEM file</label>
            <input type="text" id="setup-private-key-path" autocomplete="off" spellcheck="false"
                   placeholder="/home/user/.config/gitrun/github-app.pem" />
            <p class="field-hint" id="setup-private-key-hint">GitRun will automatically secure this PEM to mode 0600 before setup; the private key itself is never copied into the setup request.</p>
          </div>
        </div>

        <div class="field">
          <label for="setup-repositories">Repositories</label>
          <input type="text" id="setup-repositories" autocomplete="off"
                 placeholder="owner/repository, owner/another-repository" />
          <p class="field-hint">Enter one or more repositories in owner/repository form, separated by commas.</p>
        </div>

        <div class="toolbar" style="margin-top:20px">
          <span id="setup-status" class="field-hint"></span>
          <button class="btn btn-primary" id="setup-submit">Install and start GitRun</button>
        </div>

        <p class="field-hint" style="margin-top:16px">
          GitHub App mode requires the PEM file to already exist on this machine and be readable by the privileged setup process.
        </p>
      </div>
    </div>
  `;

  const authModeEl = document.getElementById("setup-auth-mode");
  const patFields = document.getElementById("setup-pat-fields");
  const appFields = document.getElementById("setup-app-fields");
  const privateKeyPathEl = document.getElementById("setup-private-key-path");

  function updateAuthFields() {
    const appMode = authModeEl.value === "app";
    patFields.style.display = appMode ? "none" : "block";
    appFields.style.display = appMode ? "block" : "none";
  }

  authModeEl.addEventListener("change", updateAuthFields);
  updateAuthFields();

  privateKeyPathEl.addEventListener("blur", async () => {
    if (authModeEl.value !== "app") return;
    const path = privateKeyPathEl.value.trim();
    const hint = document.getElementById("setup-private-key-hint");
    if (!path) return;

    try {
      const securedPath = await invoke("secure_private_key", { privateKeyPath: path });
      privateKeyPathEl.value = securedPath;
      hint.textContent = "Private key secured with mode 0600.";
      hint.style.color = "var(--success)";
    } catch (error) {
      hint.textContent = "Private key security check: " + error;
      hint.style.color = "var(--danger)";
    }
  });

  document.getElementById("setup-submit").addEventListener("click", async () => {
    const tokenEl = document.getElementById("setup-token");
    const appIdEl = document.getElementById("setup-app-id");
    const installationIdEl = document.getElementById("setup-installation-id");
    const privateKeyPathEl = document.getElementById("setup-private-key-path");
    const reposEl = document.getElementById("setup-repositories");
    const statusEl = document.getElementById("setup-status");
    const button = document.getElementById("setup-submit");

    const authMode = authModeEl.value;
    const token = tokenEl.value.trim();
    const appId = appIdEl.value.trim();
    const installationId = installationIdEl.value.trim();
    const privateKeyPath = privateKeyPathEl.value.trim();
    const repositories = reposEl.value.trim();

    if (authMode === "pat" && !token) {
      statusEl.textContent = "GitHub token is required.";
      statusEl.style.color = "var(--danger)";
      tokenEl.focus();
      return;
    }
    if (authMode === "app" && (!appId || !installationId || !privateKeyPath)) {
      statusEl.textContent = "App ID, installation ID, and private key path are all required.";
      statusEl.style.color = "var(--danger)";
      if (!appId) appIdEl.focus();
      else if (!installationId) installationIdEl.focus();
      else privateKeyPathEl.focus();
      return;
    }
    if (!repositories) {
      statusEl.textContent = "At least one repository is required.";
      statusEl.style.color = "var(--danger)";
      reposEl.focus();
      return;
    }

    button.disabled = true;
    tokenEl.disabled = true;
    appIdEl.disabled = true;
    installationIdEl.disabled = true;
    privateKeyPathEl.disabled = true;
    reposEl.disabled = true;
    authModeEl.disabled = true;
    statusEl.textContent = "Authorizing the privileged GitRun setup…";
    statusEl.style.color = "var(--text-secondary)";

    try {
      if (authMode === "app") {
        const securedPath = await invoke("secure_private_key", { privateKeyPath });
        privateKeyPathEl.value = securedPath;
      }
      await invoke("run_first_setup", {
        authMode,
        token,
        repositories,
        appId,
        installationId,
        privateKeyPath,
      });
      tokenEl.value = "";
      state.firstRun = false;
      setDashboardShell(true);
      await navigate("overview");
      pollHypervisorDecisions();
      setInterval(pollHypervisorDecisions, 5000);
    } catch (error) {
      statusEl.textContent = "Setup failed: " + error;
      statusEl.style.color = "var(--danger)";
      button.disabled = false;
      tokenEl.disabled = false;
      appIdEl.disabled = false;
      installationIdEl.disabled = false;
      privateKeyPathEl.disabled = false;
      reposEl.disabled = false;
      authModeEl.disabled = false;
    }
  });
}

// ---------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------

function errorBanner(message) {
  return `<div class="empty-state" style="color:var(--danger)">Error: ${escapeHtml(message)}</div>`;
}

async function navigate(view) {
  const generation = ++state.navigationGeneration;
  state.view = view;
  setActiveNav();

  if (view === "overview") {
    if (!(await refreshOverview(generation))) return;
    renderRepoNav();
    renderOverview();
  } else if (view.startsWith("repo:")) {
    const repo = view.slice("repo:".length);
    state.selectedRepo = repo;
    await renderRepoView(repo);
  } else if (view === "gitvault") {
    await renderGitVault();
  } else if (view === "gsr") {
    await renderGsr();
  } else if (view === "zizmor-info") {
    await renderZizmorInfo();
  } else if (view === "settings") {
    await renderSettings();
  }
}

document.addEventListener("click", (event) => {
  const btn = event.target.closest("[data-view]");
  if (btn) navigate(btn.dataset.view);
});

// Initial load
(async () => {
  try {
    state.firstRun = await invoke("is_first_run");
  } catch (error) {
    content.innerHTML = errorBanner("Unable to determine GitRun setup state: " + error);
    return;
  }

  if (state.firstRun) {
    renderFirstRun();
    return;
  }

  await navigate("overview");

  // Hypervisor decision prompts are global and time-sensitive (see the
  // section above) — poll independently of navigation, starting
  // immediately rather than waiting for the first 5s tick, since a
  // request could already be pending from before the dashboard was
  // opened.
  pollHypervisorDecisions();
  setInterval(pollHypervisorDecisions, 5000);
})();
