export const invoke = window.__TAURI__.core.invoke;
export const content = document.getElementById("content");

export function esc(value) {
  const node = document.createElement("span");
  node.textContent = value == null ? "" : String(value);
  return node.innerHTML.replaceAll('"', "&quot;").replaceAll("'", "&#039;");
}
export function toast(message, kind = "success") {
  const region = document.getElementById("toast-region");
  const item = document.createElement("div");
  item.className = "toast " + kind;
  item.textContent = String(message);
  item.addEventListener("animationend", (event) => {
    if (event.animationName === "toast-lifetime") item.remove();
  }, { once: true });
  region.appendChild(item);
}
export function heading(eyebrow, title, subtitle, actions = "") {
  return '<header class="view-heading"><div><div class="eyebrow">' + esc(eyebrow) + '</div><h1>' + esc(title) + '</h1><p class="subtitle">' + esc(subtitle) + '</p></div><div class="heading-actions">' + actions + '</div></header>';
}
export function panel(title, body, actions = "") {
  return '<section class="section panel panel-pad"><div class="section-heading"><h2>' + esc(title) + '</h2>' + actions + '</div>' + body + '</section>';
}
export function pill(label, kind = "neutral") {
  return '<span class="pill ' + esc(kind) + '">' + esc(label) + '</span>';
}
export function metric(label, value, foot, icon) {
  return '<article class="panel metric-card"><div class="metric-top"><span>' + esc(label) + '</span><span class="metric-icon">' + esc(icon || "•") + '</span></div><div class="metric-value">' + esc(value) + '</div><div class="metric-foot">' + esc(foot || "") + '</div></article>';
}
export function empty(title, detail, action = "") {
  return '<div class="empty-state"><strong>' + esc(title) + '</strong><span>' + esc(detail || "") + '</span>' + action + '</div>';
}
export function errorView(error, retryAction = "refresh-view") {
  return '<div class="notice danger"><div><strong>Could not load this view</strong><span>' + esc(error) + '</span><div style="margin-top:12px"><button class="btn" data-action="' + esc(retryAction) + '">Try again</button></div></div></div>';
}
export function confirmModal(title, detail, confirmLabel = "Confirm", danger = false) {
  return new Promise((resolve) => {
    const backdrop = document.createElement("div");
    backdrop.className = "modal-backdrop";
    backdrop.innerHTML = '<section class="modal" role="dialog" aria-modal="true" aria-labelledby="confirm-title"><h2 id="confirm-title">' + esc(title) + '</h2><p>' + esc(detail) + '</p><div class="modal-actions"><button class="btn" data-cancel>Cancel</button><button class="btn ' + (danger ? "btn-danger" : "btn-primary") + '" data-confirm>' + esc(confirmLabel) + '</button></div></section>';
    document.body.appendChild(backdrop);
    const finish = (value) => { backdrop.remove(); resolve(value); };
    backdrop.querySelector("[data-cancel]").addEventListener("click", () => finish(false));
    backdrop.querySelector("[data-confirm]").addEventListener("click", () => finish(true));
    backdrop.addEventListener("click", (event) => { if (event.target === backdrop) finish(false); });
    backdrop.addEventListener("keydown", (event) => { if (event.key === "Escape") finish(false); });
    backdrop.querySelector("[data-confirm]").focus();
  });
}
export function setFooter(message) {
  const el = document.getElementById("footer-status");
  if (el) el.textContent = message;
}
export function formatTime(value) {
  if (!value) return "Not available";
  const date = new Date(Number(value) * 1000);
  return Number.isNaN(date.getTime()) ? String(value) : date.toLocaleString();
}
export function statusKind(value) {
  return value ? "good" : "warn";
}
