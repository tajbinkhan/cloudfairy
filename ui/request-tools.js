"use strict";

// Search, filter, Copy as cURL and Export HAR for each tunnel's request list.
// Loaded before app.js. It calls app.js helpers ($, $$, invoke, toast, requestPath, plural,
// escapeHtml, errText) only from functions that run after app.js has loaded.

/** tunnel id -> {text, method, status}. Lasts while the app is open. */
const reqFilterState = new Map();
/** <li> in a request list -> the entry it shows */
const reqFilterEntries = new WeakMap();

const REQ_STATUS_OPTIONS = [
  ["", "All statuses"],
  ["2", "2xx"],
  ["3", "3xx"],
  ["4", "4xx"],
  ["5", "5xx"],
  ["none", "No response yet"],
  ["failed", "Failed"],
];

function reqFilterFor(id) {
  let f = reqFilterState.get(id);
  if (!f) {
    f = { text: "", method: "", status: "" };
    reqFilterState.set(id, f);
  }
  return f;
}

const reqFilterActive = (f) => Boolean(f.text || f.method || f.status);

/** The filter row inside a card's request panel. app.js puts it in the card template. */
function requestFilterHtml() {
  const statuses = REQ_STATUS_OPTIONS.map(([v, label]) => `<option value="${v}">${label}</option>`).join("");
  return `
    <div class="req-filter">
      <input class="req-filter-text" type="search" placeholder="Filter by path" aria-label="Filter by path" spellcheck="false" autocomplete="off" />
      <select class="req-filter-method" aria-label="Method"><option value="">All methods</option></select>
      <select class="req-filter-status" aria-label="Status">${statuses}</select>
      <span class="req-filter-count" aria-live="polite"></span>
      <button class="btn ghost small" type="button" data-req-tool="clear-filter" hidden>Clear filters</button>
      <button class="btn small" type="button" data-req-tool="export-har" title="Save the requests shown here as a HAR file">Export HAR</button>
    </div>
    <p class="req-no-match" hidden>No requests match these filters.</p>`;
}

function reqFilterMatches(f, e) {
  if (f.method && e.method !== f.method) return false;
  if (f.status === "failed") {
    if (!e.error) return false;
  } else if (f.status === "none") {
    if (e.error || e.status != null) return false;
  } else if (f.status) {
    if (e.status == null || String(Math.floor(e.status / 100)) !== f.status) return false;
  }
  return !f.text || requestPath(e.url).toLowerCase().includes(f.text.toLowerCase());
}

/** Rebuilds the method list from the methods in the list, keeping the current choice. */
function reqFilterSyncMethods(el, rows, f) {
  const select = $(".req-filter-method", el);
  const methods = new Set(rows.map((li) => reqFilterEntries.get(li)?.method).filter(Boolean));
  if (f.method) methods.add(f.method);
  const sorted = [...methods].sort();
  const current = $$("option", select).slice(1).map((o) => o.value);
  if (sorted.join() !== current.join()) {
    select.innerHTML =
      `<option value="">All methods</option>` + sorted.map((m) => `<option value="${escapeHtml(m)}">${escapeHtml(m)}</option>`).join("");
  }
  select.value = f.method;
}

/**
 * Remembers `entries` for their rows, then hides the rows the tunnel's filter doesn't match and
 * updates the count. app.js calls it after it loads, adds, updates or clears rows.
 */
function applyRequestFilter(id, el, entries = []) {
  if (!el || !$(".req-filter", el)) return;
  const list = $(".req-list", el);
  for (const e of entries) {
    const li = $(`li[data-req="${e.id}"]`, list);
    if (li) reqFilterEntries.set(li, e);
  }
  const f = reqFilterFor(id);
  const rows = [...list.children];
  let shown = 0;
  for (const li of rows) {
    const e = reqFilterEntries.get(li);
    li.hidden = e ? !reqFilterMatches(f, e) : false;
    if (!li.hidden) shown++;
  }
  reqFilterSyncMethods(el, rows, f);
  $(".req-filter-text", el).value = f.text;
  $(".req-filter-status", el).value = f.status;

  const active = reqFilterActive(f);
  const count = $(".req-filter-count", el);
  count.hidden = rows.length === 0;
  count.textContent = active ? `${shown.toLocaleString()} of ${plural(rows.length, "request")}` : plural(rows.length, "request");
  $('[data-req-tool="clear-filter"]', el).hidden = !active;
  $('[data-req-tool="export-har"]', el).disabled = shown === 0;
  $(".req-no-match", el).hidden = !(rows.length > 0 && shown === 0);
}

function reqFilterChanged(target) {
  const el = target.closest(".card");
  if (!el) return;
  const f = reqFilterFor(el.dataset.id);
  if (target.matches(".req-filter-text")) f.text = target.value.trim();
  else if (target.matches(".req-filter-method")) f.method = target.value;
  else if (target.matches(".req-filter-status")) f.status = target.value;
  else return;
  applyRequestFilter(el.dataset.id, el);
}

function clearRequestFilter(el) {
  reqFilterState.set(el.dataset.id, { text: "", method: "", status: "" });
  applyRequestFilter(el.dataset.id, el);
}

async function exportHar(el, btn) {
  const entryIds = $$(".req-list > li", el)
    .filter((li) => !li.hidden)
    .map((li) => Number(li.dataset.req));
  if (!entryIds.length) return;
  btn.disabled = true;
  try {
    const path = await invoke("export_har", { id: el.dataset.id, entryIds });
    if (path) toast(`Saved ${plural(entryIds.length, "request")} to ${path.split(/[\\/]/).pop()}`);
  } catch (e) {
    toast(errText(e));
  } finally {
    btn.disabled = false;
  }
}

// ---------------------------------------------------------------- Copy as cURL

/** Mirrors is_secret in requests.rs. */
function reqIsSecretHeader(name) {
  const n = name.toLowerCase();
  return ["authorization", "proxy-authorization", "cookie"].includes(n) || ["token", "secret", "password", "api-key", "apikey"].some((w) => n.includes(w));
}

/** Headers Cloudflare or cloudflared add on the way in, and ones curl sets itself. */
function reqIsAddedHeader(name) {
  const n = name.toLowerCase();
  return n.startsWith("cf-") || ["cdn-loop", "x-forwarded-for", "x-forwarded-proto", "host", "content-length", "accept-encoding"].includes(n);
}

/** POSIX shell single-quoting: 'it'\''s'. */
function shellQuote(s) {
  return `'${String(s).replace(/'/g, `'\\''`)}'`;
}

/** {command, left: credential header names that were left out}. */
function buildCurl(e) {
  // One option per line so long commands stay readable.
  const lines = [`curl ${shellQuote(e.url)}`];
  if (e.method === "HEAD") lines.push("--head");
  else if (e.method !== "GET") lines.push(`-X ${/^[A-Z]+$/.test(e.method) ? e.method : shellQuote(e.method)}`);
  const left = [];
  let compressed = false;
  for (const [name, value] of e.headers) {
    if (reqIsSecretHeader(name) || /^\(hidden, \d+ characters?\)$/.test(value)) {
      left.push(name);
      continue;
    }
    if (name.toLowerCase() === "accept-encoding") compressed = true;
    if (reqIsAddedHeader(name)) continue;
    lines.push(`-H ${shellQuote(`${name}: ${value}`)}`);
  }
  if (compressed) lines.push("--compressed");
  return { command: lines.join(" \\\n  "), left };
}

function joinNames(names) {
  if (names.length < 2) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** The Copy as cURL button and its note, for a request's detail view. */
function requestCurlHtml(e) {
  const { left } = buildCurl(e);
  const notes = [
    left.length
      ? `Leaves out ${joinNames(left)} because ${left.length === 1 ? "it holds" : "they hold"} credentials.`
      : "Leaves out credential headers such as Authorization and Cookie.",
  ];
  if (e.size) notes.push("The body isn't recorded, so the command sends none.");
  return `
    <div class="req-curl">
      <button class="btn small" type="button" data-req-tool="copy-curl">Copy as cURL</button>
      <span class="req-curl-note">${escapeHtml(notes.join(" "))}</span>
    </div>`;
}

async function copyCurl(btn) {
  const e = reqFilterEntries.get(btn.closest("li"));
  if (!e) return;
  try {
    await invoke("copy_text", { text: buildCurl(e).command });
    toast("cURL command copied");
  } catch (err) {
    toast(errText(err));
  }
}

// ---------------------------------------------------------------- events

(function initRequestTools() {
  const list = typeof document === "undefined" ? null : document.getElementById("list");
  if (!list) return;
  list.addEventListener("input", (ev) => {
    if (ev.target.matches(".req-filter-text")) reqFilterChanged(ev.target);
  });
  list.addEventListener("change", (ev) => {
    if (ev.target.matches(".req-filter select")) reqFilterChanged(ev.target);
  });
  list.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape" && ev.target.matches(".req-filter-text") && ev.target.value) {
      ev.preventDefault();
      ev.target.value = "";
      reqFilterChanged(ev.target);
    }
  });
  list.addEventListener("click", (ev) => {
    const btn = ev.target.closest("[data-req-tool]");
    if (!btn) return;
    const el = btn.closest(".card");
    if (!el) return;
    switch (btn.dataset.reqTool) {
      case "clear-filter":
        clearRequestFilter(el);
        break;
      case "export-har":
        exportHar(el, btn);
        break;
      case "copy-curl":
        copyCurl(btn);
        break;
    }
  });
})();
