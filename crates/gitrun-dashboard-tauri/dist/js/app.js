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
  let detail, dockRequirements;
  try {
    [detail, dockRequirements] = await Promise.all([
      invoke("get_repo_detail", { repo }),
      invoke("get_dock_requirements", { repo }),
    ]);
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "repo:" + repo) return;

  const dockerPolicy = detail.docker_policy || {};
  const logicPolicies = dockerPolicy.logic_containers || {};
  const mountPolicy = dockerPolicy.allowed_mounts || { enabled: false, rules: [] };

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title mono">${escapeHtml(repo)}</h1>
      <p class="view-subtitle">GitVault groups: ${detail.vault_groups.length ? detail.vault_groups.map((g) => `<span class="pill pill-info">${escapeHtml(g)}</span>`).join(" ") : '<span class="text-muted">none</span>'}</p>
    </div>

    <div class="section">
      <div class="toolbar">
        <h2 class="section-title" style="margin:0">Logic Containers rules</h2>
        <button class="btn btn-primary btn-sm" id="add-rule-btn">+ Add rule</button>
      </div>
      <p class="field-hint" style="margin-bottom:12px">Rules choose the backend and image for matching dynamic jobs. GitDockRun requirements are detected separately from workflow files.</p>
      ${detail.logic_rules.length === 0 ? `<div class="empty-state">No Logic Containers rules configured. Dynamic runners use the default image.</div>` : `<div class="table-scroll"><table class="data-table"><thead><tr><th>Rule</th><th>Match labels</th><th>Backend</th><th>Image</th><th></th></tr></thead><tbody>${detail.logic_rules.map((rule, i) => `<tr><td>${escapeHtml(rule.name)}</td><td>${rule.match_labels.map((l) => `<span class="pill pill-muted">${escapeHtml(l)}</span>`).join(" ")}</td><td>${formatBackend(rule.backend)}</td><td class="mono">${escapeHtml(rule.image)}</td><td><button class="btn btn-sm btn-danger" data-delete-rule="${i}">Remove</button></td></tr>`).join("")}</tbody></table></div>`}
    </div>

    <div class="section">
      <h2 class="section-title">GitDockRun workflow requirements</h2>
      <p class="field-hint">Pre-flight validation marks the logical job names used by <span class="mono">GitDockRun --job</span>. The real Docker container is resolved only after GitHub confirms the run/job/runner binding.</p>
      ${dockRequirements.length === 0 ? `<div class="empty-state">No GitDockRun job references detected.</div>` : `<div class="table-scroll"><table class="data-table"><thead><tr><th>Workflow</th><th>Calling job</th><th>Target job</th><th>Operation</th></tr></thead><tbody>${dockRequirements.map((r) => `<tr><td class="mono">${escapeHtml(r.file)}:${r.line}</td><td>${escapeHtml(r.calling_job || "—")}</td><td class="mono">${escapeHtml(r.target_job)}</td><td><span class="pill pill-info">${escapeHtml(r.operation)}</span></td></tr>`).join("")}</tbody></table></div>`}
    </div>

    <div class="section" style="max-width:860px">
      <h2 class="section-title">Docker resource policy</h2>
      <label class="field-checkbox"><input type="checkbox" id="repo-direct-socket" ${dockerPolicy.direct_socket_enabled ? "checked" : ""} /><span><strong>Direct Docker socket compatibility</strong></span></label>
      <p class="field-hint">Dangerous compatibility opt-out. GitDockRun does not require this switch.</p>
      <div class="field">
        <label>Allowed GitDockRun job names <span class="text-muted">(comma-separated, * for all)</span></label>
        <input id="repo-allowed-jobs" value="${escapeHtml((dockerPolicy.allowed_job_names || []).join(","))}" />
      </div>
      <div class="field">
        <label>Allowed Logic Container names <span class="text-muted">(comma-separated, * for all)</span></label>
        <input id="repo-allowed-containers" value="${escapeHtml((dockerPolicy.allowed_container_names || []).join(","))}" />
      </div>

      <h3 class="section-title" style="font-size:16px">Logic Container capabilities</h3>
      <p class="field-hint">A container may be connected/read/written/executed/melted independently of the global API enablement.</p>
      <div id="logic-policy-list">
        ${Object.entries(logicPolicies).map(([name, policy]) => `<div class="card" style="margin-bottom:10px" data-logic-policy="${escapeHtml(name)}"><div class="toolbar"><strong class="mono">${escapeHtml(name)}</strong><button class="btn btn-sm btn-danger" data-remove-logic-policy="${escapeHtml(name)}">Remove</button></div><div class="chip-row">${["connect","read","write","execute","melt","mountable"].map((op) => `<label class="field-checkbox"><input type="checkbox" data-logic-op="${escapeHtml(name)}|${op}" ${policy?.[op] ? "checked" : ""} /><span>${op}</span></label>`).join("")}</div><div class="field"><label>Allowed melt targets</label><input data-logic-targets="${escapeHtml(name)}" value="${escapeHtml((policy?.allowed_melt_targets || []).join(","))}" placeholder="runner,build-cache" /></div></div>`).join("")}
      </div>
      <button class="btn btn-sm" id="add-logic-policy">+ Add Logic Container policy</button>

      <h3 class="section-title" style="font-size:16px;margin-top:24px">Mount allowlist</h3>
      <label class="field-checkbox"><input type="checkbox" id="mount-enabled" ${mountPolicy.enabled ? "checked" : ""} /><span>Enable configured mounts</span></label>
      <div id="mount-rules-list">${(mountPolicy.rules || []).map((rule, i) => `<div class="card mount-rule" data-mount-index="${i}" style="margin-bottom:10px"><div class="field"><label>Source</label><input data-mount-source="${i}" value="${escapeHtml(rule.source || "")}" /></div><div class="chip-row"><label class="field-checkbox"><input type="checkbox" data-mount-recursive="${i}" ${rule.recursive ? "checked" : ""} /><span>recursive</span></label><label class="field-checkbox"><input type="checkbox" data-mount-readonly="${i}" ${rule.read_only ? "checked" : ""} /><span>read-only</span></label><label class="field-checkbox"><input type="checkbox" data-mount-allow="${i}" ${rule.allow ? "checked" : ""} /><span>allow</span></label><button class="btn btn-sm btn-danger" data-remove-mount="${i}">Remove</button></div></div>`).join("")}</div>
      <button class="btn btn-sm" id="add-mount-rule">+ Add mount rule</button>
      <div class="toolbar" style="margin-top:18px"><span id="repo-docker-save-status" class="field-hint"></span><button class="btn btn-primary" id="save-repo-docker-policy">Save Docker policy</button></div>
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
      } catch (error) { alert("Could not remove rule: " + error); }
    });
  });

  document.getElementById("add-logic-policy").addEventListener("click", () => {
    const name = prompt("Logical container name:");
    if (!name || !name.trim()) return;
    const key = name.trim();
    if (logicPolicies[key]) {
      alert("That Logic Container already has a policy.");
      return;
    }
    logicPolicies[key] = {
      connect: true,
      read: true,
      write: true,
      execute: false,
      melt: false,
      mountable: false,
      allowed_melt_targets: ["runner"],
    };
    const list = document.getElementById("logic-policy-list");
    const card = document.createElement("div");
    card.className = "card";
    card.style.marginBottom = "10px";
    card.dataset.logicPolicy = key;
    card.innerHTML = '<div class="toolbar"><strong class="mono">' +
      escapeHtml(key) +
      '</strong><button class="btn btn-sm btn-danger" data-remove-logic-policy="' +
      escapeHtml(key) +
      '">Remove</button></div>' +
      '<div class="chip-row">' +
      ["connect","read","write","execute","melt","mountable"].map((op) =>
        '<label class="field-checkbox"><input type="checkbox" data-logic-op="' +
        escapeHtml(key) + '|' + op + '" ' + (logicPolicies[key][op] ? "checked" : "") +
        ' /><span>' + op + '</span></label>'
      ).join("") +
      '</div><div class="field"><label>Allowed melt targets</label><input data-logic-targets="' +
      escapeHtml(key) + '" value="runner" placeholder="runner,build-cache" /></div>';
    list.appendChild(card);
    card.querySelector("[data-remove-logic-policy]").addEventListener("click", () => {
      delete logicPolicies[key];
      card.remove();
    });
  });

  content.querySelectorAll("[data-remove-logic-policy]").forEach((btn) => btn.addEventListener("click", () => {
    const key = btn.dataset.removeLogicPolicy;
    delete logicPolicies[key];
    btn.closest("[data-logic-policy]")?.remove();
  }));

  const addMountButton = document.getElementById("add-mount-rule");
  addMountButton.addEventListener("click", () => {
    const list = document.getElementById("mount-rules-list");
    const index = list.querySelectorAll(".mount-rule").length;
    const card = document.createElement("div");
    card.className = "card mount-rule";
    card.dataset.mountIndex = String(index);
    card.style.marginBottom = "10px";
    card.innerHTML = '<div class="field"><label>Source</label><input data-mount-source="' + index + '" value="/path" /></div>' +
      '<div class="chip-row"><label class="field-checkbox"><input type="checkbox" data-mount-recursive="' + index + '" /><span>recursive</span></label>' +
      '<label class="field-checkbox"><input type="checkbox" data-mount-readonly="' + index + '" checked /><span>read-only</span></label>' +
      '<label class="field-checkbox"><input type="checkbox" data-mount-allow="' + index + '" checked /><span>allow</span></label>' +
      '<button class="btn btn-sm btn-danger" data-remove-mount="' + index + '">Remove</button></div>';
    list.appendChild(card);
    card.querySelector("[data-remove-mount]").addEventListener("click", () => card.remove());
  });
  content.querySelectorAll("[data-remove-mount]").forEach((btn) => btn.addEventListener("click", () => {
    btn.closest(".mount-rule")?.remove();
  }));

  document.getElementById("save-repo-docker-policy").addEventListener("click", async () => {
    const status = document.getElementById("repo-docker-save-status");
    try {
      const socketEnabled = document.getElementById("repo-direct-socket").checked;
      if (socketEnabled && !confirm("Enable direct Docker socket access for this repository? This bypasses the normal socket isolation compatibility boundary and gives workflows Docker daemon access. GitDockRun remains available without it.")) {
        return;
      }
      const settings = await invoke("get_gitrun_settings");
      settings.repositories = settings.repositories || {};
      const repoSettings = settings.repositories[repo] || {};
      repoSettings.api_overrides = repoSettings.api_overrides || {};
      repoSettings.docker = repoSettings.docker || {};
      repoSettings.docker.direct_socket_enabled = document.getElementById("repo-direct-socket").checked;
      repoSettings.docker.allowed_job_names = document.getElementById("repo-allowed-jobs").value.split(",").map((v) => v.trim()).filter(Boolean);
      repoSettings.docker.allowed_container_names = document.getElementById("repo-allowed-containers").value.split(",").map((v) => v.trim()).filter(Boolean);
      repoSettings.docker.logic_containers = {};
      Object.keys(logicPolicies).forEach((name) => {
        const policy = logicPolicies[name];
        const result = { ...policy, allowed_melt_targets: [...(policy.allowed_melt_targets || [])] };
        ["connect","read","write","execute","melt","mountable"].forEach((op) => {
          const el = document.querySelector(`[data-logic-op="${CSS.escape(name)}|${op}"]`);
          if (el) result[op] = el.checked;
        });
        const targetEl = document.querySelector(`[data-logic-targets="${CSS.escape(name)}"]`);
        result.allowed_melt_targets = targetEl ? targetEl.value.split(",").map((v) => v.trim()).filter(Boolean) : [];
        repoSettings.docker.logic_containers[name] = result;
      });
      repoSettings.docker.allowed_mounts = {
        enabled: document.getElementById("mount-enabled").checked,
        rules: Array.from(document.querySelectorAll(".mount-rule")).map((row) => {
          const index = row.dataset.mountIndex;
          return {
            source: row.querySelector(`[data-mount-source="${index}"]`).value.trim(),
            recursive: row.querySelector(`[data-mount-recursive="${index}"]`).checked,
            read_only: row.querySelector(`[data-mount-readonly="${index}"]`).checked,
            allow: row.querySelector(`[data-mount-allow="${index}"]`).checked,
          };
        }),
      };
      settings.repositories[repo] = repoSettings;
      await invoke("save_gitrun_settings", { settings, confirmSocketOptOut: repoSettings.docker.direct_socket_enabled });
      status.textContent = "Docker policy saved.";
      status.style.color = "var(--success)";
    } catch (error) {
      status.textContent = "Error: " + error;
      status.style.color = "var(--danger)";
    }
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
  let config, settings;
  try {
    [config, settings] = await Promise.all([invoke("get_config"), invoke("get_gitrun_settings")]);
  } catch (error) {
    if (generation !== state.navigationGeneration) return;
    content.innerHTML = errorBanner(String(error));
    return;
  }
  if (generation !== state.navigationGeneration || state.view !== "settings") return;

  const apiOps = {
    GitVaultRun: ["Read", "Write", "Exists", "Delete", "List"],
    GitDockRun: ["Connect", "Disconnect", "Read", "Write", "Execute", "Melt"],
    GitSaveRun: ["File", "Logs"],
    GitRegisterRun: ["Register"],
    GitInstallRun: ["Install", "Remove", "Update"],
    GitReadRun: ["Read"],
    GitWriteRun: ["Write"],
    GitVerifyRun: ["Verify"],
    GitStatusRun: ["Status"],
  };
  const apiNames = Object.keys(apiOps);
  const globalApis = settings.global?.apis || {};

  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">Settings</h1>
      <p class="view-subtitle">GitRun security policy, API capabilities and Docker compatibility controls.</p>
    </div>

    <div class="section">
      <h2 class="section-title">Workflow APIs — global policy</h2>
      <p class="field-hint">Disabled by default. Repository settings can only restrict these capabilities further.</p>
      <div class="table-scroll"><table class="data-table">
        <thead><tr><th>API</th><th>Enabled</th><th>Operations</th></tr></thead>
        <tbody>
          ${apiNames.map((api) => {
            const policy = globalApis[api] || { enabled: false, allowed_operations: [] };
            return `<tr>
              <td class="mono">${api}</td>
              <td><label class="field-checkbox"><input type="checkbox" data-global-api="${api}" ${policy.enabled ? "checked" : ""} /><span>${policy.enabled ? "Enabled" : "Disabled"}</span></label></td>
              <td><div class="chip-row">${apiOps[api].map((op) => `<label class="field-checkbox"><input type="checkbox" data-global-op="${api}|${op}" ${(policy.allowed_operations || []).includes(op) ? "checked" : ""} ${policy.enabled ? "" : "disabled"} /><span>${op.toLowerCase()}</span></label>`).join("")}</div></td>
            </tr>`;
          }).join("")}
        </tbody>
      </table></div>
    </div>

    <div class="section" style="max-width:760px">
      <h2 class="section-title">Repository policy</h2>
      <div class="field">
        <label for="policy-repo">Repository</label>
        <select id="policy-repo">${config.repositories.map((repo) => `<option value="${escapeHtml(repo)}">${escapeHtml(repo)}</option>`).join("")}</select>
      </div>
      <div id="repo-policy-editor"></div>
    </div>

    <div class="section" style="max-width:760px">
      <h2 class="section-title">Docker Socket Protection</h2>
      <label class="field-checkbox">
        <input type="checkbox" id="global-hardening" ${config.gsr_docker_socket_hardening ? "checked" : ""} />
        <span><strong>GSR Docker socket hardening</strong></span>
      </label>
      <p class="field-hint">Hardening is the global danger gate. Turning it off requires the existing unsafe-runner gate as well.</p>
      <label class="field-checkbox">
        <input type="checkbox" id="unsafe-runner" ${config.gsr_allow_unsafe_runner ? "checked" : ""} />
        <span>Allow unsafe runner configuration</span>
      </label>
    </div>

    <div class="section" style="max-width:760px">
      <div class="toolbar">
        <span id="security-save-status" class="field-hint"></span>
        <button class="btn btn-primary" id="save-security-settings">Save security settings</button>
      </div>
    </div>
  `;

  const repoSelect = document.getElementById("policy-repo");
  const repoEditor = document.getElementById("repo-policy-editor");

  function effectiveRepoSettings(repo) {
    return settings.repositories?.[repo] || {
      api_overrides: {},
      docker: { direct_socket_enabled: false, allowed_job_names: [], allowed_container_names: [], logic_containers: {}, allowed_mounts: { enabled: false, rules: [] } },
      vault: { read: false, write: false, exists: false, delete: false, list_metadata: false, allowed_names: [] },
      storage: { enabled: false, allow_files: false, allow_logs: false, max_file_size_bytes: 67108864 },
      register: { enabled: false, allow_workflow: false, allow_permanent: false, allowed_entries: [] },
    };
  }

  function renderRepoPolicy(repo) {
    const policy = effectiveRepoSettings(repo);
    const overrides = policy.api_overrides || {};
    repoEditor.innerHTML = `
      <div class="field-row">
        <label class="field-checkbox"><input type="checkbox" id="repo-socket" ${policy.docker?.direct_socket_enabled ? "checked" : ""} /><span><strong>Direct Docker socket compatibility</strong></span></label>
      </div>
      <p class="field-hint">This is the explicit repository opt-out. GitDockRun remains separate and does not require the socket.</p>
      <div class="table-scroll"><table class="data-table">
        <thead><tr><th>API override</th><th>Enabled</th><th>Allowed operations</th></tr></thead>
        <tbody>${apiNames.map((api) => {
          const override = overrides[api] || { enabled: null, allowed_operations: null };
          const selected = override.allowed_operations || [];
          return `<tr>
            <td class="mono">${api}</td>
            <td><select data-repo-enabled="${api}"><option value="inherit" ${override.enabled === null || override.enabled === undefined ? "selected" : ""}>Inherit</option><option value="true" ${override.enabled === true ? "selected" : ""}>Allow</option><option value="false" ${override.enabled === false ? "selected" : ""}>Deny</option></select></td>
            <td><div class="chip-row">${apiOps[api].map((op) => `<label class="field-checkbox"><input type="checkbox" data-repo-op="${api}|${op}" ${selected.includes(op) ? "checked" : ""} /><span>${op.toLowerCase()}</span></label>`).join("")}</div></td>
          </tr>`;
        }).join("")}</tbody>
      </table></div>
    `;
    document.getElementById("repo-socket").addEventListener("change", () => {});
  }

  repoSelect.addEventListener("change", () => renderRepoPolicy(repoSelect.value));
  renderRepoPolicy(repoSelect.value || config.repositories[0] || "");

  apiNames.forEach((api) => {
    const checkbox = document.querySelector(`[data-global-api="${CSS.escape(api)}"]`);
    checkbox?.addEventListener("change", () => {
      document.querySelectorAll(`[data-global-op^="${CSS.escape(api)}|"]`).forEach((op) => {
        op.disabled = !checkbox.checked;
        if (!checkbox.checked) op.checked = false;
      });
    });
  });

  document.getElementById("save-security-settings").addEventListener("click", async () => {
    const next = structuredClone(settings);
    next.global = next.global || { apis: {} };
    next.global.apis = next.global.apis || {};
    for (const api of apiNames) {
      const enabled = document.querySelector(`[data-global-api="${CSS.escape(api)}"]`).checked;
      const operations = Array.from(document.querySelectorAll(`[data-global-op^="${CSS.escape(api)}|"]`))
        .filter((el) => el.checked).map((el) => el.dataset.globalOp.split("|")[1]);
      next.global.apis[api] = { enabled, allowed_operations: operations };
    }

    const repo = repoSelect.value;
    next.repositories = next.repositories || {};
    const repoSettings = effectiveRepoSettings(repo);
    repoSettings.docker = repoSettings.docker || { direct_socket_enabled: false, allowed_job_names: [], allowed_container_names: [], logic_containers: {}, allowed_mounts: { enabled: false, rules: [] } };
    repoSettings.docker.direct_socket_enabled = document.getElementById("repo-socket").checked;
    repoSettings.api_overrides = repoSettings.api_overrides || {};
    for (const api of apiNames) {
      const enabledValue = document.querySelector(`[data-repo-enabled="${CSS.escape(api)}"]`).value;
      const selected = Array.from(document.querySelectorAll(`[data-repo-op^="${CSS.escape(api)}|"]`))
        .filter((el) => el.checked).map((el) => el.dataset.repoOp.split("|")[1]);
      repoSettings.api_overrides[api] = {
        enabled: enabledValue === "inherit" ? null : enabledValue === "true",
        allowed_operations: selected.length ? selected : null,
      };
    }
    next.repositories[repo] = repoSettings;

    const nextConfigDangerGate = {
      ...config,
      gsr_docker_socket_hardening: document.getElementById("global-hardening").checked,
      gsr_allow_unsafe_runner: document.getElementById("unsafe-runner").checked,
    };

    const status = document.getElementById("security-save-status");
    try {
      if (config.gsr_docker_socket_hardening && !nextConfigDangerGate.gsr_docker_socket_hardening) {
        if (!confirm("Disable GSR Docker socket hardening? The existing unsafe-runner danger gate will be required before GitRun accepts this configuration.")) {
          return;
        }
      }
      await invoke("save_gitrun_settings", { settings: next, confirmSocketOptOut: repoSettings.docker.direct_socket_enabled });
      await invoke("save_config", { updated: nextConfigDangerGate });
      settings = next;
      status.textContent = "Security settings saved.";
      status.style.color = "var(--success)";
    } catch (error) {
      status.textContent = "Error: " + error;
      status.style.color = "var(--danger)";
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

function renderSetupProgress() {
  setDashboardShell(false);
  content.innerHTML = `
    <div class="view-header">
      <h1 class="view-title">Installing GitRun</h1>
      <p class="view-subtitle">GitRun is being installed with administrator privileges. Follow the live progress and installer output below.</p>
    </div>

    <div class="setup-progress-layout">
      <aside class="card setup-progress-panel">
        <div class="setup-panel-title">Setup progress</div>
        <div class="setup-progress-track" role="progressbar" aria-label="GitRun setup progress"
             aria-valuemin="0" aria-valuemax="8" aria-valuenow="0">
          <div id="setup-progress-fill" class="setup-progress-fill setup-progress-waiting"></div>
        </div>
        <div class="setup-progress-summary">
          <strong id="setup-progress-percent">Waiting…</strong>
          <span id="setup-progress-status">Waiting for administrator authorization.</span>
        </div>
        <ol class="setup-phase-list" id="setup-phase-list">
          <li data-setup-phase="1">Check Docker and system prerequisites</li>
          <li data-setup-phase="2">Prepare GitRun system directories</li>
          <li data-setup-phase="3">Install runner, recovery, and service resources</li>
          <li data-setup-phase="4">Write GitRun configuration</li>
          <li data-setup-phase="5">Build gitrun-runner:latest</li>
          <li data-setup-phase="6">Install GitRun binaries</li>
          <li data-setup-phase="7">Enable and start GitRun service</li>
          <li data-setup-phase="8">Finalize desktop integration</li>
        </ol>
        <button class="btn" id="setup-retry" hidden>Back to setup</button>
      </aside>

      <section class="card setup-terminal-card">
        <div class="setup-terminal-header">
          <div>
            <div class="setup-panel-title">Privileged setup log</div>
            <div class="field-hint">Live stdout/stderr from the installer.</div>
          </div>
          <span class="pill pill-muted" id="setup-terminal-status">Waiting</span>
        </div>
        <pre class="setup-terminal-output mono" id="setup-terminal-output" aria-live="polite"></pre>
      </section>
    </div>
  `;

  document.getElementById("setup-retry").addEventListener("click", renderFirstRun);
}

function setupAppendLog(message) {
  const terminal = document.getElementById("setup-terminal-output");
  if (!terminal) return;
  const timestamp = new Date().toLocaleTimeString();
  const line = `[${timestamp}] ${String(message)}`;
  const lines = (terminal.textContent || "").split("\n").filter(Boolean);
  lines.push(line);
  if (lines.length > 500) lines.splice(0, lines.length - 500);
  terminal.textContent = lines.join("\n") + "\n";
  terminal.scrollTop = terminal.scrollHeight;
}

function setupSetStatus(message, phase) {
  const status = document.getElementById("setup-progress-status");
  const percent = document.getElementById("setup-progress-percent");
  const fill = document.getElementById("setup-progress-fill");
  const track = document.querySelector(".setup-progress-track");
  if (!status || !percent || !fill || !track) return;

  const marker = /^\[GitRun setup\] \[(\d+)\/8\] (.*)$/.exec(message);
  const displayMessage = marker ? marker[2] : message;
  const safePhase = Math.max(0, Math.min(8, Number(phase) || 0));
  const value = Math.round((safePhase / 8) * 100);

  status.textContent = displayMessage;
  percent.textContent = safePhase === 0 ? "Waiting…" : `${value}% • Step ${safePhase}/8`;
  fill.classList.toggle("setup-progress-waiting", safePhase === 0);
  fill.style.width = safePhase === 0 ? "18%" : `${value}%`;
  track.setAttribute("aria-valuenow", String(safePhase));
}

function applySetupEvent(event) {
  const phase = Number(event.phase) || 0;
  const message = String(event.message || "");
  const done = Boolean(event.done);
  const success = Boolean(event.success);
  const stream = String(event.stream || "system");
  const marker = /^\[GitRun setup\] \[(\d+)\/8\] (.*)$/.exec(message);
  const displayMessage = marker ? marker[2] : message;

  setupAppendLog(stream === "stdout" || stream === "system" ? message : `[${stream}] ${message}`);
  setupSetStatus(message, phase);

  document.querySelectorAll("[data-setup-phase]").forEach((item) => {
    const step = Number(item.dataset.setupPhase);
    item.classList.toggle("active", step === phase && phase > 0 && !done);
    item.classList.toggle("complete", step < phase || (done && success && step <= phase));
  });

  const terminalStatus = document.getElementById("setup-terminal-status");
  if (terminalStatus) {
    terminalStatus.textContent = done ? (success ? "Complete" : "Failed") : (phase === 0 ? "Waiting" : `Step ${phase}/8`);
    terminalStatus.className = `pill ${done ? (success ? "pill-success" : "pill-danger") : "pill-info"}`;
    terminalStatus.title = displayMessage;
  }

  if (done && !success) {
    const retry = document.getElementById("setup-retry");
    if (retry) retry.hidden = false;
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

    const authMode = authModeEl.value;
    const token = tokenEl.value.trim();
    const appId = appIdEl.value.trim();
    const installationId = installationIdEl.value.trim();
    let setupPrivateKeyPath = privateKeyPathEl.value.trim();
    const repositories = reposEl.value.trim();

    if (authMode === "pat" && !token) {
      const statusEl = document.getElementById("setup-status");
      statusEl.textContent = "GitHub token is required.";
      statusEl.style.color = "var(--danger)";
      tokenEl.focus();
      return;
    }
    if (authMode === "app" && (!appId || !installationId || !privateKeyPath)) {
      const statusEl = document.getElementById("setup-status");
      statusEl.textContent = "App ID, installation ID, and private key path are all required.";
      statusEl.style.color = "var(--danger)";
      if (!appId) appIdEl.focus();
      else if (!installationId) installationIdEl.focus();
      else privateKeyPathEl.focus();
      return;
    }
    if (!repositories) {
      const statusEl = document.getElementById("setup-status");
      statusEl.textContent = "At least one repository is required.";
      statusEl.style.color = "var(--danger)";
      reposEl.focus();
      return;
    }

    renderSetupProgress();
    setupAppendLog("Starting first-run GitRun setup…");
    setupSetStatus("Preparing privileged setup…", 0);

    let unlisten = null;
    let setupEventDone = false;

    try {
      const { listen } = window.__TAURI__.event;
      unlisten = await listen("gitrun-setup-progress", (event) => {
        const payload = event.payload || {};
        setupEventDone = Boolean(payload.done);
        applySetupEvent(payload);
      });

      if (authMode === "app") {
        setupAppendLog("Securing the GitHub App private key to mode 0600…");
        const securedPath = await invoke("secure_private_key", { privateKeyPath: setupPrivateKeyPath });
        setupAppendLog("Private key security check completed.");
        setupPrivateKeyPath = securedPath;
      }

      await invoke("run_first_setup", {
        authMode,
        token,
        repositories,
        appId,
        installationId,
        privateKeyPath: setupPrivateKeyPath,
      });

      state.firstRun = false;
      setDashboardShell(true);
      await navigate("overview");
      pollHypervisorDecisions();
      setInterval(pollHypervisorDecisions, 5000);
    } catch (error) {
      if (!setupEventDone) {
        applySetupEvent({
          phase: 0,
          total: 8,
          message: "Setup failed: " + String(error),
          stream: "system",
          done: true,
          success: false,
        });
      }
    } finally {
      if (unlisten) {
        await unlisten();
      }
    }
  });
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
