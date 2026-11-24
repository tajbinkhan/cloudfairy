"use strict";

// Cloudfairy's proxy in the window: the "Route tunnels through Cloudfairy" setting, the
// password, CORS and header options in the Edit dialog, and response headers, bodies and
// Replay in the request list. app.js calls into window.proxyUi at a few points, and uses the
// helpers it defines (toast, errText, escapeHtml, formatBytes, tunnels, renderCard).

window.proxyUi = (() => {
  const { invoke } = window.__TAURI__.core;
  const q = (sel, root = document) => root.querySelector(sel);

  /** Mirrors the setting. Tunnels started while it's on record bodies and can be replayed. */
  let routeThroughProxy = true;

  const OFF_NOTE =
    'These options need "Route tunnels through Cloudfairy", which is off in Settings.';

  // ------------------------------------------------------------ settings dialog

  function injectSettings() {
    const group = document.createElement("div");
    group.className = "field-group";
    group.innerHTML = `
      <div class="label">Requests</div>
      <label class="check">
        <input type="checkbox" id="route-through-proxy" aria-describedby="route-through-proxy-hint" />
        <span>Route tunnels through Cloudfairy</span>
      </label>
      <p id="route-through-proxy-hint" class="hint check-hint">Cloudfairy passes each request on to your
        app, so the request list shows bodies and can replay requests, and tunnels can have a password,
        CORS and extra headers. Turn it off to let cloudflared reach your app directly. Applies to
        tunnels you start after saving.</p>`;
    q("#settings-form .dialog-actions").before(group);
  }

  async function loadSetting() {
    try {
      routeThroughProxy = (await invoke("get_proxy_settings")).routeThroughProxy;
    } catch {
      // Keep the default; the next save writes it anyway.
    }
    return routeThroughProxy;
  }

  async function fillSettings() {
    q("#route-through-proxy").checked = await loadSetting();
  }

  async function saveSettings() {
    const on = q("#route-through-proxy").checked;
    await invoke("save_proxy_settings", { routeThroughProxy: on });
    if (on !== routeThroughProxy) {
      routeThroughProxy = on;
      for (const t of tunnels.values()) renderCard(t);
    }
  }

  // ------------------------------------------------------------ edit dialog

  function injectEdit() {
    const box = document.createElement("div");
    box.className = "proxy-options";
    box.innerHTML = `
      <p class="notice warn proxy-off-note" hidden>${escapeHtml(OFF_NOTE)}</p>
      <div class="field-group">
        <div class="label">Password</div>
        <div class="proxy-auth">
          <label class="field">
            <span>Username</span>
            <input id="edit-auth-user" autocomplete="off" spellcheck="false" />
          </label>
          <label class="field">
            <span>Password</span>
            <input id="edit-auth-password" type="password" autocomplete="new-password" aria-describedby="edit-auth-hint" />
          </label>
        </div>
        <p id="edit-auth-hint" class="hint">Visitors must enter these to open the link. Leave the password
          empty to turn this off. The password is saved in the settings file on this computer.</p>
      </div>
      <div class="field-group">
        <label class="check">
          <input type="checkbox" id="edit-cors" aria-describedby="edit-cors-hint" />
          <span>Allow cross-origin requests</span>
        </label>
        <p id="edit-cors-hint" class="hint check-hint">Answers CORS preflight requests and adds
          Access-Control-Allow-* headers, so pages on other sites can call this link.</p>
      </div>
      <div class="field-group">
        <div class="label" id="edit-headers-label">Extra response headers</div>
        <div id="edit-extra-headers" class="header-rows" role="group" aria-labelledby="edit-headers-label"></div>
        <button id="edit-add-header" class="btn small add-header" type="button">Add header</button>
      </div>`;
    q("#edit-error").before(box);
    q("#edit-add-header").addEventListener("click", () => {
      addHeaderRow("", "");
      q("#edit-extra-headers .header-row:last-child input").focus();
    });
    q("#edit-extra-headers").addEventListener("click", (ev) => {
      const remove = ev.target.closest("[data-remove-header]");
      if (remove) remove.closest(".header-row").remove();
    });
  }

  function addHeaderRow(name, value) {
    const row = document.createElement("div");
    row.className = "header-row";
    row.innerHTML = `
      <input class="mono" data-header="name" placeholder="Name" aria-label="Header name" spellcheck="false" />
      <input class="mono" data-header="value" placeholder="Value" aria-label="Header value" spellcheck="false" />
      <button class="icon-btn" type="button" data-remove-header title="Remove header" aria-label="Remove header">${ICON.trash}</button>`;
    q('[data-header="name"]', row).value = name;
    q('[data-header="value"]', row).value = value;
    q("#edit-extra-headers").append(row);
  }

  async function fillEdit(t) {
    q("#edit-auth-user").value = t.authUser ?? "";
    q("#edit-auth-password").value = t.authPassword ?? "";
    q("#edit-cors").checked = Boolean(t.cors);
    q("#edit-extra-headers").innerHTML = "";
    for (const h of t.extraHeaders ?? []) addHeaderRow(h.name, h.value);
    const on = await loadSetting();
    q(".proxy-off-note").hidden = on;
    for (const el of q(".proxy-options").querySelectorAll("input, button")) el.disabled = !on;
  }

  /** Saves the options after app.js saved the rest. Returns the updated tunnel view. */
  async function saveEdit(id) {
    const extraHeaders = [...document.querySelectorAll("#edit-extra-headers .header-row")].map((row) => ({
      name: q('[data-header="name"]', row).value,
      value: q('[data-header="value"]', row).value,
    }));
    return invoke("update_tunnel_proxy", {
      id,
      authUser: q("#edit-auth-user").value,
      authPassword: q("#edit-auth-password").value,
      cors: q("#edit-cors").checked,
      extraHeaders,
    });
  }

  function cardTags(t) {
    if (!routeThroughProxy) return [];
    const tags = [];
    if (t.authPassword) tags.push("Password protected");
    if (t.cors) tags.push("CORS allowed");
    return tags;
  }

  // ------------------------------------------------------------ request detail

  const dl = (rows) => rows.map(([k, v]) => `<dt>${escapeHtml(k)}</dt><dd>${escapeHtml(v)}</dd>`).join("");

  function bodyHtml(title, e, side, info) {
    if (!info) return "";
    let meta;
    if (!info.size) {
      meta = ["Empty"];
    } else {
      meta = [info.contentType || "No content type", formatBytes(info.size)];
      if (info.contentEncoding) meta.push(info.contentEncoding);
      if (info.truncated) meta.push("first 1 MB kept");
      if (info.dropped) meta.push("dropped to save memory");
    }
    if (!info.complete) meta.push("the connection closed before it ended");
    const loadable = info.size > 0 && !info.dropped;
    return `
      <h4>${escapeHtml(title)}</h4>
      <p class="body-meta">${escapeHtml(meta.join(" · "))}</p>
      ${loadable ? `<pre class="body-text" data-body="${side}" data-entry="${e.id}" data-state="pending" hidden></pre>` : ""}`;
  }

  /** Extra detail for a request the proxy recorded, or a note on why there is none. */
  function detailHtml(e) {
    if (!e.proxied) {
      return `<p class="hint req-proxy-note">Bodies and Replay need "Route tunnels through Cloudfairy" in
        Settings. It applies the next time the tunnel starts.</p>`;
    }
    const parts = [];
    if (e.replayOf != null) parts.push(`<p class="req-replay" data-replay-of="${e.replayOf}">Sent again with Replay.</p>`);
    if (e.responseHeaders?.length) {
      parts.push(`<h4>Response headers</h4><dl class="req-headers">${dl(e.responseHeaders)}</dl>`);
    }
    parts.push(bodyHtml("Request body", e, "request", e.requestBody));
    if (e.responseBody) {
      parts.push(bodyHtml("Response body", e, "response", e.responseBody));
    } else if (e.status != null && e.status !== 101 && !e.error) {
      parts.push(`<h4>Response body</h4><p class="body-meta">Still arriving.</p>`);
    }
    parts.push(`
      <div class="req-actions">
        <button class="btn small" type="button" data-proxy-action="replay" data-entry="${e.id}">Replay</button>
        <span class="hint">Sends this request to your app again and adds the result to the list.</span>
      </div>`);
    return parts.join("");
  }

  function pretty(text, contentType) {
    const looksJson = /json/i.test(contentType ?? "") || /^\s*[[{]/.test(text);
    if (!looksJson) return text;
    try {
      return JSON.stringify(JSON.parse(text), null, 2);
    } catch {
      return text; // Cut at 1 MB, or not JSON after all.
    }
  }

  async function loadBody(pre) {
    pre.dataset.state = "loading";
    const id = pre.closest(".card")?.dataset.id;
    if (!id) return;
    try {
      const view = await invoke("request_body", { id, entryId: Number(pre.dataset.entry), side: pre.dataset.body });
      // Binary bodies stay as type and size.
      if (!view?.text) {
        pre.dataset.state = "none";
        return;
      }
      pre.textContent = pretty(view.text, view.info.contentType);
      if (view.decodedFrom) {
        const meta = pre.previousElementSibling;
        if (meta) meta.textContent += ` · shown after ${view.decodedFrom} decoding`;
      }
      pre.dataset.state = "done";
    } catch (e) {
      pre.textContent = errText(e);
      pre.dataset.state = "error";
    }
    pre.hidden = false;
  }

  // Bodies load when their detail opens. Rows are re-rendered as requests progress, so this
  // watches the list instead of hooking each place that opens a row.
  function watchDetails() {
    const list = q("#list");
    let queued = false;
    const scan = () => {
      queued = false;
      for (const pre of list.querySelectorAll('.req-detail:not([hidden]) pre[data-state="pending"]')) loadBody(pre);
    };
    new MutationObserver(() => {
      if (queued) return;
      queued = true;
      queueMicrotask(scan);
    }).observe(list, { subtree: true, childList: true, attributes: true, attributeFilter: ["hidden"] });

    list.addEventListener("click", async (ev) => {
      const btn = ev.target.closest('[data-proxy-action="replay"]');
      if (!btn) return;
      const id = btn.closest(".card")?.dataset.id;
      btn.disabled = true;
      try {
        await invoke("replay_request", { id, entryId: Number(btn.dataset.entry) });
        toast("Replayed. The result is at the top of the list.");
      } catch (e) {
        toast(errText(e));
      } finally {
        btn.disabled = false;
      }
    });
  }

  // ------------------------------------------------------------ boot

  injectSettings();
  injectEdit();
  watchDetails();
  loadSetting().then(() => {
    for (const t of tunnels.values()) renderCard(t);
  });

  return { fillSettings, saveSettings, fillEdit, saveEdit, cardTags, detailHtml };
})();
