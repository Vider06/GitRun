import { invoke, content, esc, heading, toast } from "./lib.js";

export function renderSetup(isReinstall, onComplete) {
  document.body.classList.add("setup-mode");
  content.innerHTML = heading("FIRST RUN","Welcome to GitRun","Connect a repository and configure a reviewed runner profile. Credentials are sent to the local privileged setup flow.") +
    '<div class="setup-shell"><section class="panel panel-pad"><div class="section-heading"><h2>1 · Runner profile</h2><span class="pill info">GSR included</span></div><div class="field"><label for="setup-runner-profile">Runner image profile</label><select id="setup-runner-profile"><option value="minimum">Minimum — lightweight tools</option><option value="workbench" selected>Workbench — full development toolchain</option></select><p class="field-hint">Both official profiles include mandatory GitSecureRun protections.</p></div></section>' +
    '<section class="panel panel-pad section"><div class="section-heading"><h2>2 · GitHub authentication</h2><span class="pill neutral">Local setup</span></div><div class="field"><label for="setup-auth-mode">Credential type</label><select id="setup-auth-mode"><option value="pat">Personal Access Token</option><option value="app">GitHub App</option></select></div><div id="setup-pat-fields"><div class="field"><label for="setup-token">GitHub token</label><input id="setup-token" type="password" autocomplete="off" spellcheck="false" placeholder="github_pat_…"><p class="field-hint">Never place a token in logs or screenshots. GitRun setup receives it through local IPC.</p></div></div><div id="setup-app-fields" hidden><div class="field"><label for="setup-app-id">GitHub App ID</label><input id="setup-app-id" inputmode="numeric" placeholder="123456"></div><div class="field"><label for="setup-installation-id">Installation ID</label><input id="setup-installation-id" inputmode="numeric" placeholder="12345678"></div><div class="field"><label for="setup-private-key-path">Private key PEM path</label><input id="setup-private-key-path" placeholder="/home/user/.config/gitrun/github-app.pem"><p class="field-hint" id="setup-private-key-hint">The private key remains in its file and will be secured to mode 0600.</p></div></div><div class="field"><label for="setup-repositories">Repositories</label><input id="setup-repositories" placeholder="owner/repository, owner/another-repository"><p class="field-hint">Use the owner/repository format. Only connect repositories you intend GitRun to manage.</p></div><div class="toolbar"><span id="setup-status" class="field-hint"></span><button class="btn btn-primary" id="setup-submit">' + (isReinstall ? "Reinstall and start GitRun" : "Install and start GitRun") + '</button></div></section>' +
    '<section id="setup-progress-panel" class="panel panel-pad section" hidden><div class="section-heading"><h2 id="setup-progress-title">Preparing setup…</h2><span id="setup-progress-count" class="pill info">0 / 9</span></div><div class="setup-progress"><span id="setup-progress-bar"></span></div><div id="setup-terminal" class="terminal" role="log" aria-live="polite"></div><div class="toolbar" style="margin-top:12px"><span id="setup-progress-status" class="field-hint">Waiting for privileged setup</span><button id="setup-retry" class="btn" hidden>Return to setup</button></div></section></div>';
  const mode = document.getElementById("setup-auth-mode");
  const pat = document.getElementById("setup-pat-fields");
  const app = document.getElementById("setup-app-fields");
  const status = document.getElementById("setup-status");
  mode.addEventListener("change", () => { pat.hidden = mode.value !== "pat"; app.hidden = mode.value !== "app"; });
  document.getElementById("setup-private-key-path").addEventListener("blur", async () => {
    if (mode.value !== "app") return;
    const path = document.getElementById("setup-private-key-path").value.trim();
    if (!path) return;
    const hint = document.getElementById("setup-private-key-hint");
    try { const secured = await invoke("secure_private_key", {privateKeyPath:path}); document.getElementById("setup-private-key-path").value = secured; hint.textContent = "Private key secured with mode 0600."; }
    catch (error) { hint.textContent = "Key check failed: " + error; }
  });
  document.getElementById("setup-submit").addEventListener("click", async () => {
    const authMode = mode.value;
    const token = document.getElementById("setup-token").value.trim();
    const appId = document.getElementById("setup-app-id").value.trim();
    const installationId = document.getElementById("setup-installation-id").value.trim();
    const privateKeyPath = document.getElementById("setup-private-key-path").value.trim();
    const repositories = document.getElementById("setup-repositories").value.trim();
    const runnerProfile = document.getElementById("setup-runner-profile").value;
    if ((authMode === "pat" && !token) || (authMode === "app" && (!appId || !installationId || !privateKeyPath)) || !repositories) {
      status.textContent = authMode === "pat" && !token ? "A GitHub token is required." : authMode === "app" && (!appId || !installationId || !privateKeyPath) ? "Complete the GitHub App fields." : "At least one repository is required.";
      status.style.color = "var(--danger)"; return;
    }
    const submit = document.getElementById("setup-submit");
    submit.disabled = true;
    status.textContent = "Starting privileged setup…";
    document.getElementById("setup-progress-panel").hidden = false;
    const log = document.getElementById("setup-terminal");
    const append = (line) => { if (line) { log.textContent += String(line) + "\n"; log.scrollTop = log.scrollHeight; } };
    let unlisten = null;
    let done = false;
    try {
      const listener = window.__TAURI__.event;
      unlisten = await listener.listen("gitrun-setup-progress", (event) => {
        const item = event.payload || {};
        const phase = Number(item.phase || 0);
        const total = Number(item.total || 9);
        document.getElementById("setup-progress-bar").style.width = Math.max(0,Math.min(100,phase / Math.max(1,total) * 100)) + "%";
        document.getElementById("setup-progress-count").textContent = phase + " / " + total;
        document.getElementById("setup-progress-title").textContent = item.message || "Working…";
        document.getElementById("setup-progress-status").textContent = item.done ? (item.success ? "Setup completed" : "Setup failed") : "Running";
        append(item.message);
        done = Boolean(item.done);
        if (item.done) document.getElementById("setup-retry").hidden = Boolean(item.success);
      });
      let safeKeyPath = privateKeyPath;
      if (authMode === "app") safeKeyPath = await invoke("secure_private_key", {privateKeyPath});
      await invoke("run_first_setup", {request:{authMode,token,repositories,appId,installationId,privateKeyPath:safeKeyPath,runnerProfile,reinstall:Boolean(isReinstall)}});
      document.getElementById("setup-progress-bar").style.width = "100%";
      document.getElementById("setup-progress-count").textContent = "9 / 9";
      document.getElementById("setup-progress-title").textContent = "GitRun is ready";
      document.getElementById("setup-progress-status").textContent = "Setup completed successfully";
      append("Setup command completed successfully.");
      toast("GitRun setup completed.");
      await onComplete();
    } catch (error) {
      append("ERROR: " + error);
      document.getElementById("setup-progress-title").textContent = "Setup failed";
      document.getElementById("setup-progress-status").textContent = "Review the logs and correct the issue before retrying.";
      document.getElementById("setup-retry").hidden = false;
      toast("Setup failed: " + error, "error");
    } finally {
      if (unlisten) await unlisten();
      if (!done) submit.disabled = false;
      const retryButton = document.getElementById("setup-retry");
      if (retryButton) retryButton.hidden = false;
    }
  });
  document.getElementById("setup-retry").addEventListener("click", () => renderSetup(isReinstall, onComplete));
}
