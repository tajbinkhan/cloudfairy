// Suggests local ports that something listens on, under the address field of the Add form.
// Loaded before app.js; it reads app.js's `tunnels` map only inside event handlers.
(() => {
  "use strict";

  const { invoke } = window.__TAURI__.core;
  /** Don't ask the backend again within this many ms (focus events come in bursts). */
  const MIN_INTERVAL = 1500;

  const esc = (s) =>
    String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

  /** Loopback ports saved tunnels already point at. The backend leaves these out too, but a
   * tunnel may have been added since the list was read. */
  function savedPorts() {
    const used = new Set();
    let saved;
    try {
      saved = [...tunnels.values()];
    } catch {
      // app.js hasn't run yet.
      return used;
    }
    for (const t of saved) {
      if (!t.target) continue;
      try {
        const u = new URL(t.target);
        const host = u.hostname.replace(/^\[|\]$/g, "");
        if (host === "localhost" || host === "::1" || host === "0.0.0.0" || host.startsWith("127.")) {
          used.add(Number(u.port || (u.protocol === "https:" ? 443 : 80)));
        }
      } catch {
        // Not a URL; nothing to leave out.
      }
    }
    return used;
  }

  function attach(input, anchor) {
    const box = document.createElement("div");
    box.className = "port-suggestions";
    box.hidden = true;
    box.setAttribute("role", "group");
    box.setAttribute("aria-label", "Ports in use on this computer");
    anchor.after(box);

    let lastAsked = 0;
    let latest = 0;
    let list = [];

    function render() {
      const used = savedPorts();
      const shown = list.filter((s) => !used.has(s.port));
      box.hidden = shown.length === 0;
      box.innerHTML =
        `<span class="port-label">Running on this computer</span>` +
        shown
          .map((s) => {
            const what = s.process ? ` (${s.process})` : "";
            return `<button class="port-chip" type="button" data-value="${esc(s.value)}" title="Use localhost:${s.port}${esc(what)}">${s.port}${
              s.process ? `<span>${esc(s.process)}</span>` : ""
            }</button>`;
          })
          .join("");
    }

    async function refresh() {
      if (Date.now() - lastAsked < MIN_INTERVAL) return;
      lastAsked = Date.now();
      const ask = ++latest;
      let found = [];
      try {
        found = await invoke("suggest_ports");
      } catch {
        // Suggestions are a convenience; without them the form works as before.
      }
      if (ask !== latest) return;
      list = found;
      render();
    }

    box.addEventListener("click", (ev) => {
      const chip = ev.target.closest("button[data-value]");
      if (!chip) return;
      input.value = chip.dataset.value;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.focus();
    });
    input.addEventListener("focus", refresh);
    // Re-read when the window comes back, since a dev server may have started meanwhile.
    window.addEventListener("focus", refresh);
    return { refresh, render };
  }

  const target = document.getElementById("target");
  const fields = document.querySelector("#composer .composer-fields");
  if (target && fields) {
    const suggestions = attach(target, fields);
    // Adding or removing a tunnel card changes which ports are taken.
    const list = document.getElementById("list");
    if (list) new MutationObserver(suggestions.render).observe(list, { childList: true });
    suggestions.refresh();
  }
})();
