// Requests per minute over the last 30 minutes, as a small inline SVG on each running card.
// app.js calls window.cfSparkline.render(t, card) whenever it renders a card. The counts come
// from cloudflared's metrics (t.requests, t.requestErrors), which only ever go up during a run.
(() => {
  "use strict";

  const MINUTES = 30;
  const W = 96;
  const H = 22;
  const PAD = 2.5;
  const MINUTE = 60_000;

  /** id -> { status, req, err, buckets: Map(minute -> {req, err}), recent: [{at, req, err}] } */
  const runs = new Map();

  const minuteOf = (ms) => Math.floor(ms / MINUTE);

  function prune(run, now) {
    const oldest = minuteOf(now) - MINUTES + 1;
    for (const m of run.buckets.keys()) if (m < oldest) run.buckets.delete(m);
    run.recent = run.recent.filter((r) => r.at > now - MINUTE);
  }

  function record(t) {
    const now = Date.now();
    const req = t.requests ?? 0;
    const err = t.requestErrors ?? 0;
    let run = runs.get(t.id);
    if (!run) {
      // Requests from before the window first saw this tunnel have no times, so they only
      // set the starting point.
      runs.set(t.id, { status: t.status, req, err, buckets: new Map(), recent: [] });
      return;
    }
    if (t.status === "starting" && run.status !== "starting") {
      // A new run counts from zero and starts a new graph.
      Object.assign(run, { req: 0, err: 0, buckets: new Map(), recent: [] });
    }
    run.status = t.status;
    let dReq = req - run.req;
    let dErr = err - run.err;
    if (dReq < 0 || dErr < 0) {
      // cloudflared restarted and its counters went back to zero.
      dReq = req;
      dErr = err;
    }
    run.req = req;
    run.err = err;
    if (dReq > 0 || dErr > 0) {
      const m = minuteOf(now);
      const b = run.buckets.get(m) ?? { req: 0, err: 0 };
      b.req += dReq;
      b.err += dErr;
      run.buckets.set(m, b);
      run.recent.push({ at: now, req: dReq, err: dErr });
    }
    prune(run, now);
  }

  const plural = (n, word) => `${n.toLocaleString()} ${word}${n === 1 ? "" : "s"}`;

  function draw(id, card, show) {
    let box = card.querySelector(".card-spark");
    if (!box) {
      box = document.createElement("span");
      box.className = "card-spark";
      box.setAttribute("role", "img");
      const stats = card.querySelector(".card-stats");
      if (!stats) return;
      stats.after(box);
    }
    const run = runs.get(id);
    box.hidden = !show || !run;
    if (box.hidden) return;

    const now = Date.now();
    prune(run, now);
    const last = minuteOf(now);
    const values = [];
    for (let m = last - MINUTES + 1; m <= last; m++) values.push(run.buckets.get(m) ?? { req: 0, err: 0 });
    const peak = Math.max(0, ...values.map((v) => v.req));
    const top = Math.max(1, peak);
    const x = (i) => (PAD + (i * (W - 2 * PAD)) / (MINUTES - 1)).toFixed(1);
    const y = (v) => (H - PAD - (v / top) * (H - 2 * PAD)).toFixed(1);
    const line = "M" + values.map((v, i) => `${x(i)},${y(v.req)}`).join("L");
    const area = `${line}L${x(MINUTES - 1)},${H - PAD}L${x(0)},${H - PAD}Z`;
    const marks = values
      .map((v, i) => (v.err ? `<circle class="spark-err" cx="${x(i)}" cy="${y(v.req)}" r="2.2"/>` : ""))
      .join("");

    const rate = run.recent.reduce((n, r) => n + r.req, 0);
    const failed = run.recent.reduce((n, r) => n + r.err, 0);
    const failedTotal = values.reduce((n, v) => n + v.err, 0);
    let tip = `Requests per minute over the last ${MINUTES} minutes. Last minute: ${plural(rate, "request")}`;
    tip += failed ? `, ${failed.toLocaleString()} failed.` : ".";
    if (peak) tip += ` Busiest minute: ${peak.toLocaleString()}.`;
    if (failedTotal) tip += ` Red dots mark minutes with failed requests.`;
    box.title = tip;
    box.setAttribute("aria-label", tip);
    box.innerHTML =
      `<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" aria-hidden="true">` +
      `<line class="spark-base" x1="${x(0)}" y1="${H - PAD}" x2="${x(MINUTES - 1)}" y2="${H - PAD}"/>` +
      `<path class="spark-area" d="${area}"/><path class="spark-line" d="${line}"/>${marks}</svg>` +
      `<span class="spark-rate">${rate.toLocaleString()}/min</span>`;
  }

  function render(t, card) {
    record(t);
    draw(t.id, card, t.status === "running");
  }

  // Minutes pass without new events, so move the graphs along.
  setInterval(() => {
    for (const [id, run] of runs) {
      const card = document.querySelector(`.card[data-id="${CSS.escape(id)}"]`);
      if (!card) runs.delete(id);
      else if (run.status === "running") draw(id, card, true);
    }
  }, 5000);

  window.cfSparkline = { render };
})();
