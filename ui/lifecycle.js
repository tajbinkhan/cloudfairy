"use strict";

// Restart if it fails, stop after a set time, and what to do with each new link: the Edit
// dialog fields and what the cards show. Uses helpers and state from app.js.
(() => {
  /** id -> {restartAt, stopAt, stopUrl, hookError, notice} from the backend */
  const states = new Map();

  const STOP_LABELS = { 15: "15 min", 30: "30 min", 60: "1 h", 120: "2 h", 480: "8 h" };

  function secondsUntil(ms) {
    return Math.max(0, Math.ceil((ms - Date.now()) / 1000));
  }

  function formatLeft(s) {
    if (s < 60) return `${s} s`;
    const m = Math.ceil(s / 60);
    if (m < 60) return `${m} min`;
    const h = Math.floor(m / 60);
    return m % 60 ? `${h} h ${m % 60} min` : `${h} h`;
  }

  const isUp = (t) => t.status === "starting" || t.status === "running";

  function restartPending(id) {
    return Boolean(states.get(id)?.restartAt);
  }

  /** "Restarting in 20 s", while a failed tunnel waits for its next try. */
  function restartText(t) {
    const st = states.get(t.id);
    if (!st?.restartAt || isUp(t)) return null;
    const s = secondsUntil(st.restartAt);
    return s ? `Restarting in ${formatLeft(s)}` : "Restarting…";
  }

  /** "Stops in 29 min", for the run whose countdown this is. */
  function stopText(t) {
    const st = states.get(t.id);
    if (t.status !== "running" || !st?.stopAt || st.stopUrl !== t.publicUrl) return null;
    return `Stops in ${formatLeft(secondsUntil(st.stopAt))}`;
  }

  // The card template lives in app.js, so the extra elements are added here.
  function parts(el) {
    let timer = $(".lc-timer", el);
    if (!timer) {
      timer = document.createElement("span");
      timer.className = "lc-timer";
      timer.hidden = true;
      $(".card-foot .spacer", el).before(timer);
    }
    let note = $(".lc-note", el);
    if (!note) {
      note = document.createElement("p");
      note.className = "notice lc-note";
      note.hidden = true;
      $(".notice.error", el).after(note);
    }
    return { timer, note };
  }

  function render(t, el) {
    const st = states.get(t.id);
    const opts = t.lifecycle ?? {};
    const tags = [];
    if (opts.restartOnFailure) tags.push("Restarts if it fails");
    if (STOP_LABELS[opts.stopAfterMinutes]) tags.push(`Stops after ${STOP_LABELS[opts.stopAfterMinutes]}`);
    if (tags.length) $(".card-target", el).textContent += ` · ${tags.join(" · ")}`;

    const restart = restartText(t);
    if (restart) {
      el.dataset.status = "restarting";
      $(".status-text", el).textContent = restart;
      const toggle = $(".toggle", el);
      toggle.innerHTML = `${ICON.stop}<span>Stop</span>`;
      toggle.classList.remove("primary");
    }

    const { timer, note } = parts(el);
    const stop = stopText(t);
    timer.hidden = !stop;
    timer.textContent = stop ?? "";

    // A link step that failed shows while the tunnel runs; "Stopped after 30 min" once stopped.
    const message = isUp(t) ? st?.hookError : t.status === "stopped" && !restart ? st?.notice : null;
    note.hidden = !message;
    note.textContent = message ?? "";
    note.classList.toggle("lc-warn", Boolean(st?.hookError) && isUp(t));
  }

  function tick() {
    for (const [id, st] of states) {
      if (!st.restartAt && !st.stopAt) continue;
      const t = tunnels.get(id);
      const el = cardFor(id);
      if (!t || !el) continue;
      const restart = restartText(t);
      if (restart) $(".status-text", el).textContent = restart;
      const stop = stopText(t);
      if (stop) $(".lc-timer", el).textContent = stop;
    }
  }

  function apply(state) {
    states.set(state.id, state);
    const t = tunnels.get(state.id);
    if (t) renderCard(t);
  }

  // ------------------------------------------------------------ edit dialog

  function syncVarField() {
    $("#edit-url-var").disabled = !$("#edit-url-file").value.trim();
  }

  function fill(t) {
    const opts = t.lifecycle ?? {};
    $("#edit-restart").checked = Boolean(opts.restartOnFailure);
    $("#edit-stop-after").value = String(opts.stopAfterMinutes ?? 0);
    if (!$("#edit-stop-after").value) $("#edit-stop-after").value = "0";
    $("#edit-url-file").value = opts.urlFile ?? "";
    $("#edit-url-var").value = opts.urlFileVar ?? "";
    $("#edit-url-command").value = opts.urlCommand ?? "";
    syncVarField();
  }

  /** Saves the dialog's lifecycle fields. Throws the backend's message when one is invalid. */
  async function save(id) {
    const file = $("#edit-url-file").value.trim();
    const view = await invoke("set_tunnel_lifecycle", {
      id,
      lifecycle: {
        restartOnFailure: $("#edit-restart").checked,
        stopAfterMinutes: Number($("#edit-stop-after").value) || 0,
        urlFile: file,
        urlFileVar: file ? $("#edit-url-var").value.trim() : "",
        urlCommand: $("#edit-url-command").value.trim(),
      },
    });
    const t = tunnels.get(id);
    if (t) t.lifecycle = view.lifecycle;
    return view;
  }

  async function init() {
    $("#edit-url-file").addEventListener("input", syncVarField);
    await listen("tunnel-lifecycle", ({ payload }) => apply(payload));
    for (const st of await invoke("lifecycle_states")) states.set(st.id, st);
    // Cards drawn before the states arrived get them now.
    for (const t of tunnels.values()) if (cardFor(t.id)) renderCard(t);
    setInterval(tick, 1000);
  }

  window.lifecycleUi = { render, fill, save, restartPending };
  init().catch((e) => console.error("lifecycle", e));
})();
