import { invoke, content, esc, heading, panel, pill, empty, errorView, formatTime } from "../lib.js";

export async function renderGsr() {
  const generation = window.__gitrunNavigationGeneration;
  content.innerHTML = '<div class="loading-state"><span class="loader"></span><p>Reading GSR process status and event history…</p></div>';
  let status, events, info;
  try {
    [status, events, info] = await Promise.all([invoke("get_gsr_status"), invoke("list_gsr_events",{limit:30}), invoke("get_zizmor_info")]);
  } catch (error) { if (generation !== window.__gitrunNavigationGeneration) return; content.innerHTML = heading("RUNTIME PROTECTION","GitSecureRun","Watchdog status and recorded security events.") + errorView(error); return; }
  if (generation !== window.__gitrunNavigationGeneration) return;
  window.dispatchEvent(new CustomEvent("gitrun:mascot-state",{detail:!(status.watching)?"serious":(events || []).some((event)=>/critical|high|violation|blocked/i.test(String(event.severity || event.level || "")))?"alert":"calm"}));
  content.innerHTML = heading("RUNTIME PROTECTION","GitSecureRun","Inspect watchdog presence and security events. A running process is not a complete enforcement proof.", '<button class="btn" data-action="refresh-view">↻ Refresh</button>') +
    '<div class="grid metrics-grid"><article class="panel metric-card"><div class="metric-top">GSR process</div><div class="metric-value">' + (status.watching ? "Seen" : "Missing") + '</div><div class="metric-foot">Process-name check</div></article><article class="panel metric-card"><div class="metric-top">Watched runner</div><div class="metric-value">' + (status.watched_pid || "—") + '</div><div class="metric-foot">PID validated against process name</div></article><article class="panel metric-card"><div class="metric-top">Recent events</div><div class="metric-value">' + (events || []).length + '</div><div class="metric-foot">Latest event records returned</div></article><article class="panel metric-card"><div class="metric-top">Zizmor</div><div class="metric-value">' + (info.enabled ? "Enabled" : "Optional") + '</div><div class="metric-foot">' + (info.already_installed ? "Binary installed" : "Binary not detected") + '</div></article></div>' +
    '<div class="notice ' + (status.watching ? "warn" : "danger") + '"><div><strong>' + (status.watching ? "Watchdog process detected" : "Watchdog process not detected") + '</strong>This is a process-level observation only. Command policy, socket hardening, workflow validation and container-level protections need their own runtime checks.</div></div>' +
    '<div class="section">' + panel("Security event timeline", (events || []).length ? '<div class="card-list">' + events.map((event) => { const severity = String(event.severity || event.level || "info"); const bad = /critical|high|error|violation|blocked/i.test(severity); return '<div class="activity-item"><span class="activity-mark ' + (bad ? "bad" : "good") + '"></span><div><strong>' + esc(event.title || event.kind || event.event_type || "Security event") + ' ' + pill(severity, bad ? "bad" : "info") + '</strong><p>' + esc(event.detail || event.message || event.command || "No event detail supplied") + '</p><p class="subtle">' + esc(formatTime(event.timestamp || event.created_at || event.time)) + '</p></div></div>'; }).join("") + '</div>' : empty("No security events returned","An empty event list is not proof that enforcement is healthy.")) + '</div>' +
    '<div class="section">' + panel("Workflow analyzer · Zizmor", '<div class="list-row"><div class="list-row-main"><strong>' + (info.enabled ? "Enabled" : "Not enabled") + '</strong><small>' + (info.license_accepted ? "License accepted" : "License acceptance required") + ' · ' + (info.already_installed ? "binary detected" : "binary not detected") + '</small></div>' + pill(info.enabled ? "Active in config" : "Optional", info.enabled ? "good" : "neutral") + '</div><p class="section-description" style="margin-top:12px">Optional third-party workflow analysis is distinct from GitRun’s built-in pre-flight checks.</p><button class="btn" id="zizmor-toggle">' + (info.enabled ? "Disable Zizmor" : info.license_accepted ? "Enable Zizmor" : "Review license and enable") + '</button>') + '</div>';
  document.getElementById("zizmor-toggle").addEventListener("click", async () => {
    try {
      if (info.enabled) await invoke("disable_zizmor");
      else if (info.license_accepted) await invoke("accept_zizmor_license_and_install");
      else {
        const accepted = await new Promise((resolve) => {
          const modal = document.createElement("div"); modal.className="modal-backdrop";
          modal.innerHTML='<section class="modal" role="dialog" aria-modal="true"><h2>Enable Zizmor</h2><p>Enabling Zizmor may download and install an optional third-party workflow analyzer. Review its license and project before proceeding.</p><div class="modal-actions"><button class="btn" data-no>Cancel</button><button class="btn btn-primary" data-yes>I accept and enable</button></div></section>';
          document.body.appendChild(modal); modal.querySelector("[data-no]").onclick=()=>{modal.remove();resolve(false)};modal.querySelector("[data-yes]").onclick=()=>{modal.remove();resolve(true)};
        });
        if (!accepted) return;
        await invoke("accept_zizmor_license_and_install");
      }
      await renderGsr();
    } catch (error) { content.insertAdjacentHTML("afterbegin", '<div class="notice danger"><div><strong>Operation failed</strong>' + esc(error) + '</div></div>'); }
  });
}
