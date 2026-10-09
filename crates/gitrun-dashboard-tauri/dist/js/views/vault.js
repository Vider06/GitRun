import { invoke, content, esc, heading, panel, pill, empty, errorView, toast, confirmModal, formatTime } from "../lib.js";

export async function renderVault() {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Checking GitVault metadata…</p></div>';
  let secrets;
  try { secrets = await invoke("list_vault_secrets"); }
  catch (error) {
    if (generation !== window.__gitrunNavigationGeneration) return;
    content.innerHTML = heading("PRIVACY", "GitVault", "Secret scopes and metadata. Plaintext secret values are never returned to the dashboard.") + errorView(error);
    return;
  }
  if (generation !== window.__gitrunNavigationGeneration) return;
  content.innerHTML = heading("PRIVACY", "GitVault", "Manage secret metadata and scopes without exposing plaintext values.", '<button class="btn btn-primary" id="add-secret">＋ Add secret</button>') +
    '<div class="notice success"><div><strong>Secret values stay out of the webview</strong>The dashboard only receives names, scopes and timestamps. To rotate or replace a value, enter a new one; it cannot be read back here.</div></div>' +
    '<div class="section">' + panel("Stored secret metadata", (secrets || []).length ? '<div class="table-wrap"><table><thead><tr><th>Secret name</th><th>Scope</th><th>Last updated</th><th></th></tr></thead><tbody>' + secrets.map((secret) => '<tr><td class="mono">' + esc(secret.name) + '</td><td>' + pill(scopeText(secret.scope), "info") + '</td><td>' + esc(formatTime(secret.updated_at)) + '</td><td><button class="btn btn-danger" data-delete-secret="' + esc(secret.name) + '" data-scope="' + esc(JSON.stringify(secret.scope)) + '">Delete</button></td></tr>').join("") + '</tbody></table></div>' : empty("No secrets stored", "Add a secret and choose whether it belongs to a repository, group or global scope.")) + '</div>';
  document.getElementById("add-secret").addEventListener("click", () => openSecretModal());
  content.querySelectorAll("[data-delete-secret]").forEach((button) => button.addEventListener("click", async () => {
    const ok = await confirmModal("Delete secret metadata?", "This permanently deletes " + button.dataset.deleteSecret + " from the selected scope. Workflows that depend on it may fail.", "Delete secret", true);
    if (!ok) return;
    try {
      await invoke("delete_vault_secret", { name: button.dataset.deleteSecret, scope: JSON.parse(button.dataset.scope) });
      toast("Secret deleted.");
      await renderVault();
    } catch (error) { toast("Could not delete secret: " + error, "error"); }
  }));
}
function scopeText(scope) {
  if (!scope) return "Unknown";
  if (typeof scope === "string") return scope;
  const kind = scope.kind || scope.Kind;
  const value = scope.value;
  if (kind === "Global") return "Global";
  if (kind === "Repo") return "Repository: " + (value || "unknown");
  if (kind === "Group") return "Group: " + (value || "unknown");
  return JSON.stringify(scope);
}
function openSecretModal() {
  const backdrop = document.createElement("div");
  backdrop.className = "modal-backdrop";
  backdrop.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="secret-title"><h2 id="secret-title">Add or replace a secret</h2><p>Values are sent directly to the local backend and are never read back into the UI.</p><div class="field"><label for="secret-name">Secret name</label><input id="secret-name" autocomplete="off" placeholder="DEPLOY_TOKEN"></div><div class="field"><label for="secret-value">Secret value</label><input id="secret-value" type="password" autocomplete="new-password" placeholder="Enter secret value"></div><div class="field"><label for="secret-scope">Scope type</label><select id="secret-scope"><option value="global">Global</option><option value="repo">Repository</option><option value="group">Group</option></select></div><div class="field" id="secret-scope-name-wrap" hidden><label for="secret-scope-name">Repository or group name</label><input id="secret-scope-name" placeholder="owner/repository or group name"></div><div class="modal-actions"><button class="btn" data-cancel>Cancel</button><button class="btn btn-primary" data-save>Save secret</button></div></section>';
  document.body.appendChild(backdrop);
  const type = backdrop.querySelector("#secret-scope");
  const wrap = backdrop.querySelector("#secret-scope-name-wrap");
  type.addEventListener("change", () => { wrap.hidden = type.value === "global"; });
  backdrop.querySelector("[data-cancel]").addEventListener("click", () => backdrop.remove());
  backdrop.addEventListener("click", (event) => { if (event.target === backdrop) backdrop.remove(); });
  backdrop.querySelector("[data-save]").addEventListener("click", async () => {
    const name = backdrop.querySelector("#secret-name").value.trim();
    const value = backdrop.querySelector("#secret-value").value;
    const scopeName = backdrop.querySelector("#secret-scope-name").value.trim();
    if (!name || !value || (type.value !== "global" && !scopeName)) { toast("Complete every required field.", "warning"); return; }
    let scope = { kind: "Global" };
    if (type.value === "repo") scope = { kind: "Repo", value: scopeName };
    if (type.value === "group") scope = { kind: "Group", value: scopeName };
    try {
      await invoke("set_vault_secret", { name, value, scope });
      backdrop.remove();
      toast("Secret saved. Its value will not be shown again.");
      await renderVault();
    } catch (error) { toast("Could not save secret: " + error, "error"); }
  });
  backdrop.querySelector("#secret-name").focus();
}
