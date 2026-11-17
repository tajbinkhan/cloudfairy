"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

const svg = (paths) =>
  `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;

const ICON = {
  gear: svg('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>'),
  copy: svg('<rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>'),
  open: svg('<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><path d="M15 3h6v6"/><path d="M10 14 21 3"/>'),
  qr: svg('<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><path d="M14 14h3v3h-3zM20 14v.01M14 20h.01M17 20h4v-3"/>'),
  play: svg('<path d="M6 4l14 8-14 8z" fill="currentColor" stroke="none"/>'),
  stop: svg('<rect x="6" y="6" width="12" height="12" rx="2" fill="currentColor" stroke="none"/>'),
  chevron: svg('<path d="m6 9 6 6 6-6"/>'),
  edit: svg('<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"/>'),
  trash: svg('<path d="M3 6h18"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6M14 11v6"/><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/>'),
};

/** id -> tunnel view from the backend ({id, name, target, status, publicUrl, error, warning, startedAt, connections}) */
const tunnels = new Map();
/** ids of cards whose log panel is open */
const openLogs = new Set();
let cfInfo = null;
let downloading = false;
let latestVersion = null;
let updatingApp = false;

// ---------------------------------------------------------------- helpers

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function errText(e) {
  return typeof e === "string" ? e : e?.message ?? String(e);
}

let toastTimer;
function toast(message) {
  const el = $("#toast");
  // Modal dialogs sit in the top layer, so the toast must live inside the open one to be visible.
  const host = $("dialog[open]") ?? document.body;
  if (el.parentElement !== host) host.append(el);
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 1800);
}

function formatUptime(startedAt) {
  if (!startedAt) return "";
  const s = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function statusText(t) {
  switch (t.status) {
    case "running": {
      const conns = t.connections > 1 ? ` · ${t.connections} connections` : "";
      return `Live · ${formatUptime(t.startedAt)}${conns}`;
    }
    case "starting":
      return t.publicUrl ? "Connecting…" : "Getting a URL…";
    case "error":
      return "Failed";
    default:
      return "Stopped";
  }
}

function logClass(line) {
  if (line.startsWith("[cloudfairy]") || line.startsWith("$ ")) return "app";
  if (/\sERR\s/.test(line)) return "err";
  if (/\sWRN\s/.test(line)) return "wrn";
  return "";
}

function logLineHtml(line) {
  const cls = logClass(line);
  return cls ? `<span class="${cls}">${escapeHtml(line)}</span>\n` : `${escapeHtml(line)}\n`;
}

// ---------------------------------------------------------------- tunnel cards

function cardTemplate(t) {
  const el = document.createElement("article");
  el.className = "card";
  el.dataset.id = t.id;
  el.innerHTML = `
    <div class="card-head">
      <div class="card-titles">
        <h3 class="card-name"></h3>
        <div class="card-target"></div>
      </div>
      <span class="status"><span class="dot"></span><span class="status-text"></span></span>
      <button class="btn small toggle" type="button" data-action="toggle"></button>
    </div>
    <div class="url-row">
      <a class="public-url" href="#" data-action="open"></a>
      <span class="url-placeholder"></span>
      <div class="url-actions">
        <button class="icon-btn" type="button" data-action="copy" title="Copy link" aria-label="Copy link">${ICON.copy}</button>
        <button class="icon-btn" type="button" data-action="open" title="Open in browser" aria-label="Open in browser">${ICON.open}</button>
        <button class="icon-btn" type="button" data-action="qr" title="Show QR code" aria-label="Show QR code">${ICON.qr}</button>
      </div>
    </div>
    <p class="notice warn" hidden></p>
    <p class="notice error" hidden></p>
    <div class="card-foot">
      <button class="btn ghost small" type="button" data-action="logs" aria-expanded="false">${ICON.chevron}<span>Logs</span></button>
      <span class="spacer"></span>
      <button class="btn ghost small" type="button" data-action="edit">${ICON.edit}<span>Edit</span></button>
      <button class="btn ghost small" type="button" data-action="remove">${ICON.trash}<span>Remove</span></button>
    </div>
    <pre class="logs" hidden></pre>`;
  return el;
}

function cardFor(id) {
  return $(`.card[data-id="${CSS.escape(id)}"]`);
}

function renderCard(t) {
  let el = cardFor(t.id);
  if (!el) {
    el = cardTemplate(t);
    $("#list").prepend(el);
  }
  const active = t.status === "starting" || t.status === "running";
  el.dataset.status = t.status;
  $(".card-name", el).textContent = t.name;
  $(".card-target", el).textContent = t.target;
  $(".status-text", el).textContent = statusText(t);

  const toggle = $(".toggle", el);
  toggle.innerHTML = active ? `${ICON.stop}<span>Stop</span>` : `${ICON.play}<span>${t.status === "error" ? "Retry" : "Start"}</span>`;
  toggle.classList.toggle("primary", !active);

  const link = $(".public-url", el);
  const placeholder = $(".url-placeholder", el);
  link.hidden = !t.publicUrl;
  link.textContent = t.publicUrl ?? "";
  link.href = t.publicUrl ?? "#";
  placeholder.hidden = !!t.publicUrl;
  placeholder.textContent =
    t.status === "starting" ? "Asking Cloudflare for a public URL…" : "Start the tunnel to get a public URL.";
  $(".url-actions", el).hidden = !t.publicUrl;

  const warn = $(".notice.warn", el);
  warn.hidden = !(active && t.warning);
  warn.textContent = t.warning ?? "";
  const error = $(".notice.error", el);
  error.hidden = !t.error || t.status === "running";
  error.textContent = t.error ?? "";

  $('[data-action="edit"]', el).disabled = active;
  updateEmpty();
}

function updateEmpty() {
  $("#empty").hidden = tunnels.size > 0;
}

function tickUptime() {
  for (const t of tunnels.values()) {
    if (t.status !== "running") continue;
    const el = cardFor(t.id);
    if (el) $(".status-text", el).textContent = statusText(t);
  }
}

async function toggleLogs(id, el) {
  const pre = $(".logs", el);
  const btn = $('[data-action="logs"]', el);
  if (openLogs.has(id)) {
    openLogs.delete(id);
    pre.hidden = true;
    btn.setAttribute("aria-expanded", "false");
    return;
  }
  openLogs.add(id);
  btn.setAttribute("aria-expanded", "true");
  const lines = await invoke("tunnel_logs", { id });
  pre.innerHTML = lines.length ? lines.map(logLineHtml).join("") : "No output yet.\n";
  pre.hidden = false;
  pre.scrollTop = pre.scrollHeight;
}

function appendLog(id, line) {
  if (!openLogs.has(id)) return;
  const pre = $(".logs", cardFor(id));
  if (!pre) return;
  if (pre.textContent === "No output yet.\n") pre.textContent = "";
  const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
  pre.insertAdjacentHTML("beforeend", logLineHtml(line));
  while (pre.childNodes.length > 2000) pre.removeChild(pre.firstChild);
  if (atBottom) pre.scrollTop = pre.scrollHeight;
}

async function startTunnel(id) {
  const t = tunnels.get(id);
  try {
    const view = await invoke("start_tunnel", { id });
    // Events may already have moved the tunnel further along; don't overwrite with an older snapshot.
    if (tunnels.get(id)?.status === "stopped" || tunnels.get(id)?.status === "error") Object.assign(t, view);
  } catch (e) {
    Object.assign(t, { status: "error", error: errText(e), publicUrl: null });
  }
  renderCard(t);
  if (!cfInfo?.path) refreshCloudflared();
}

const pendingRemove = new Map();
async function removeTunnel(id, btn) {
  if (!pendingRemove.has(id)) {
    $("span", btn).textContent = "Click again to remove";
    btn.classList.add("danger");
    pendingRemove.set(
      id,
      setTimeout(() => {
        pendingRemove.delete(id);
        $("span", btn).textContent = "Remove";
        btn.classList.remove("danger");
      }, 3000),
    );
    return;
  }
  clearTimeout(pendingRemove.get(id));
  pendingRemove.delete(id);
  try {
    await invoke("remove_tunnel", { id });
    tunnels.delete(id);
    openLogs.delete(id);
    cardFor(id)?.remove();
    updateEmpty();
  } catch (e) {
    toast(errText(e));
  }
}

async function onCardClick(ev) {
  const target = ev.target.closest("[data-action]");
  if (!target) return;
  ev.preventDefault();
  const el = target.closest(".card");
  const id = el.dataset.id;
  const t = tunnels.get(id);
  if (!t) return;

  switch (target.dataset.action) {
    case "toggle":
      if (t.status === "starting" || t.status === "running") {
        await invoke("stop_tunnel", { id });
      } else {
        await startTunnel(id);
      }
      break;
    case "copy":
      if (!t.publicUrl) return;
      try {
        await invoke("copy_text", { text: t.publicUrl });
        toast("Link copied");
      } catch (e) {
        toast(errText(e));
      }
      break;
    case "open":
      if (t.publicUrl) invoke("open_url", { url: t.publicUrl }).catch((e) => toast(errText(e)));
      break;
    case "qr":
      if (t.publicUrl) showQr(t.publicUrl);
      break;
    case "logs":
      toggleLogs(id, el);
      break;
    case "edit":
      openEdit(t);
      break;
    case "remove":
      removeTunnel(id, target);
      break;
  }
}

// ---------------------------------------------------------------- composer

async function onCreate(ev) {
  ev.preventDefault();
  const errorEl = $("#composer-error");
  errorEl.textContent = "";
  const target = $("#target").value;
  const name = $("#name").value;
  let view;
  try {
    view = await invoke("add_tunnel", { name, target });
  } catch (e) {
    errorEl.textContent = errText(e);
    $("#target").focus();
    return;
  }
  $("#target").value = "";
  $("#name").value = "";
  tunnels.set(view.id, view);
  renderCard(view);
  startTunnel(view.id);
}

// ---------------------------------------------------------------- edit dialog

let editingId = null;
function openEdit(t) {
  editingId = t.id;
  $("#edit-name").value = t.name;
  $("#edit-target").value = t.target;
  $("#edit-error").textContent = "";
  $("#edit-dialog").showModal();
}

async function saveEdit() {
  try {
    const view = await invoke("update_tunnel", {
      id: editingId,
      name: $("#edit-name").value,
      target: $("#edit-target").value,
    });
    Object.assign(tunnels.get(view.id), view);
    renderCard(tunnels.get(view.id));
    $("#edit-dialog").close();
  } catch (e) {
    $("#edit-error").textContent = errText(e);
  }
}

// ---------------------------------------------------------------- QR dialog

async function showQr(url) {
  try {
    $("#qr-code").innerHTML = await invoke("qr_svg", { text: url });
    $("#qr-url").textContent = url;
    $("#qr-dialog").showModal();
  } catch (e) {
    toast(errText(e));
  }
}

// ---------------------------------------------------------------- cloudflared + settings

const SOURCE_LABEL = { custom: "custom path", managed: "downloaded by Cloudfairy", system: "installed on this computer" };

async function refreshCloudflared() {
  try {
    cfInfo = await invoke("cloudflared_info");
  } catch (e) {
    cfInfo = { path: null, problem: errText(e), canDownload: false };
  }
  const ok = !!cfInfo.path && !!cfInfo.version;
  const chip = $("#cf-chip");
  chip.dataset.state = ok ? "ok" : "missing";
  $("#cf-chip-text").textContent = ok ? `cloudflared ${cfInfo.version}` : "cloudflared not found";

  $("#setup").hidden = ok;
  $("#setup-reason").innerHTML = cfInfo.problem && cfInfo.path
    ? escapeHtml(cfInfo.problem)
    : "Cloudfairy uses Cloudflare's free <code>cloudflared</code> tool to create tunnels, and it isn't installed yet.";

  $("#cf-path").textContent = cfInfo.path ?? "";
  for (const b of $$("[data-download]")) {
    b.hidden = !cfInfo.canDownload;
    b.disabled = downloading;
  }
  renderUpdate();
  checkLatest();
}

// Compares dotted versions like "2026.11.0" numerically. Returns >0 if a is newer.
function compareVersions(a, b) {
  const pa = a.split(".").map((n) => parseInt(n, 10) || 0);
  const pb = b.split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (d) return d;
  }
  return 0;
}

// Settings shows the download button only when cloudflared is missing or a newer release exists.
function renderUpdate() {
  const ok = !!cfInfo?.path && !!cfInfo?.version;
  const newer = ok && latestVersion && compareVersions(latestVersion, cfInfo.version) > 0;
  const upToDate = ok && latestVersion && !newer;
  $("#cf-detail").textContent = ok
    ? `Version ${cfInfo.version} · ${SOURCE_LABEL[cfInfo.source] ?? ""}${upToDate ? " · up to date" : ""}`
    : cfInfo?.problem ?? "Not found";
  const btn = $("#cf-update");
  btn.hidden = !cfInfo?.canDownload || (ok && !newer);
  if (!downloading) btn.textContent = ok ? `Update to ${latestVersion}` : "Download cloudflared";
  btn.dataset.label = btn.textContent;
}

async function checkLatest() {
  try {
    latestVersion = await invoke("latest_cloudflared");
  } catch {
    // Offline or GitHub unreachable: keep whatever we knew and hide the update button.
  }
  if (cfInfo) renderUpdate();
}

function showProgress({ received, total }, selector = ".progress[data-cf]") {
  const pct = total ? Math.min(100, (received / total) * 100) : null;
  const mb = (n) => (n / 1048576).toFixed(1);
  for (const p of $$(selector)) {
    p.hidden = false;
    $(".progress-bar", p).style.width = pct == null ? "100%" : `${pct}%`;
    $(".progress-text", p).textContent = total ? `${mb(received)} / ${mb(total)} MB` : `${mb(received)} MB`;
  }
}

async function downloadCloudflared() {
  if (downloading) return;
  downloading = true;
  for (const b of $$("[data-download]")) {
    b.disabled = true;
    b.dataset.label ??= b.textContent;
    b.textContent = "Downloading…";
  }
  showProgress({ received: 0, total: null });
  try {
    cfInfo = await invoke("download_cloudflared");
    $("#cf-custom").value = "";
    toast(`cloudflared ${cfInfo.version ?? ""} is ready`);
  } catch (e) {
    toast(errText(e));
    $("#settings-error").textContent = errText(e);
  } finally {
    downloading = false;
    for (const b of $$("[data-download]")) b.textContent = b.dataset.label;
    for (const p of $$(".progress[data-cf]")) p.hidden = true;
    await refreshCloudflared();
  }
}

// ---------------------------------------------------------------- Cloudfairy updates

async function checkAppUpdate() {
  let update = null;
  try {
    update = await invoke("check_app_update");
  } catch {
    // Offline, GitHub unreachable, or no update for this kind of install: say nothing.
  }
  $("#app-update").hidden = !update;
  if (!update) return;
  $("#app-update-title").textContent = `Cloudfairy ${update.version} is available`;
  $("#app-update-text").textContent =
    `You have version ${update.currentVersion}. Updating restarts Cloudfairy and stops any running tunnels.`;
}

async function installAppUpdate() {
  if (updatingApp) return;
  updatingApp = true;
  const install = $("#app-update-install");
  const later = $("#app-update-later");
  install.disabled = later.disabled = true;
  install.textContent = "Downloading…";
  showProgress({ received: 0, total: null }, ".app-progress");
  try {
    await invoke("install_app_update"); // restarts the app when it succeeds
  } catch (e) {
    $("#app-update-text").textContent = errText(e);
    install.textContent = "Try again";
    install.disabled = later.disabled = false;
    $(".app-progress").hidden = true;
    updatingApp = false;
  }
}

async function openSettings() {
  const s = await invoke("get_settings");
  $("#cf-custom").value = s.cloudflaredPath ?? "";
  $("#protocol").value = s.protocol || "auto";
  $("#settings-error").textContent = "";
  $("#settings-dialog").showModal();
  refreshCloudflared();
}

async function saveSettings() {
  try {
    await invoke("save_settings", {
      cloudflaredPath: $("#cf-custom").value,
      protocol: $("#protocol").value,
    });
    await refreshCloudflared();
    if (cfInfo.problem && $("#cf-custom").value.trim()) {
      $("#settings-error").textContent = cfInfo.problem;
      return;
    }
    $("#settings-dialog").close();
    toast("Settings saved");
  } catch (e) {
    $("#settings-error").textContent = errText(e);
  }
}

// ---------------------------------------------------------------- boot

async function init() {
  $("#open-settings").innerHTML = ICON.gear;
  $("#open-settings").addEventListener("click", openSettings);
  $("#cf-chip").addEventListener("click", openSettings);
  for (const b of $$("[data-open-settings]")) b.addEventListener("click", openSettings);
  for (const b of $$("[data-download]")) b.addEventListener("click", downloadCloudflared);
  $("#settings-save").addEventListener("click", saveSettings);
  $("#app-update-install").addEventListener("click", installAppUpdate);
  $("#app-update-later").addEventListener("click", () => ($("#app-update").hidden = true));
  $("#edit-save").addEventListener("click", saveEdit);
  $("#edit-form").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      saveEdit();
    }
  });
  $("#composer").addEventListener("submit", onCreate);
  $("#list").addEventListener("click", onCardClick);

  await listen("tunnel-update", ({ payload }) => {
    const t = tunnels.get(payload.id);
    if (!t) return;
    Object.assign(t, payload);
    renderCard(t);
  });
  await listen("tunnel-log", ({ payload }) => appendLog(payload.id, payload.line));
  await listen("download-progress", ({ payload }) => showProgress(payload));
  await listen("app-update-progress", ({ payload }) => showProgress(payload, ".app-progress"));

  const list = await invoke("list_tunnels");
  // Oldest first so prepend leaves the newest on top.
  for (const t of list) {
    tunnels.set(t.id, t);
    renderCard(t);
  }
  updateEmpty();
  setInterval(tickUptime, 1000);
  await refreshCloudflared();
  $("#target").focus();
  checkAppUpdate();
}

init().catch((e) => {
  document.body.insertAdjacentHTML("afterbegin", `<p class="notice error">Failed to start: ${escapeHtml(errText(e))}</p>`);
});
