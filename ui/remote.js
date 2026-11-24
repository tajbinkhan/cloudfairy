"use strict";

// Results of cloudfairy:// links and command-line commands (see remote.rs). A link that would
// share a port waits here for the user to click Start.
// Uses invoke, listen, tunnels, renderCard, updateEmpty, toast, errText and $ from app.js.
(() => {
  const dialog = document.createElement("dialog");
  dialog.id = "link-dialog";
  dialog.className = "dialog";
  dialog.innerHTML = `
    <form method="dialog">
      <h2>Share a local port?</h2>
      <p id="link-question"></p>
      <p id="link-detail" class="hint"></p>
      <div class="dialog-actions">
        <button class="btn ghost" value="cancel" type="submit">Cancel</button>
        <button class="btn primary" value="start" type="submit">Start</button>
      </div>
    </form>`;
  document.body.append(dialog);

  /** Link requests waiting for an answer, oldest first. */
  const questions = [];
  let asking = null;

  function askNext() {
    if (asking || !questions.length) return;
    asking = questions.shift();
    $("#link-question").textContent = `A link asked to share ${asking.address} publicly. Start it?`;
    $("#link-detail").textContent = asking.existing
      ? `This starts your saved tunnel "${asking.name}". Only start it if you opened the link yourself.`
      : `Cloudfairy saves it as a new tunnel named "${asking.name}". Only start it if you opened the link yourself.`;
    dialog.returnValue = "";
    dialog.showModal();
  }

  // Escape and Cancel both leave returnValue as something other than "start".
  dialog.addEventListener("close", async () => {
    const question = asking;
    asking = null;
    if (!question) return;
    try {
      await invoke("answer_link_request", { id: question.id, start: dialog.returnValue === "start" });
    } catch (e) {
      toast(errText(e));
    }
    askNext();
  });

  // Adds cards for tunnels saved outside the window, by a link or the command line.
  async function addNewCards() {
    const list = await invoke("list_tunnels");
    for (const t of list) {
      if (tunnels.has(t.id)) continue;
      tunnels.set(t.id, t);
      renderCard(t);
    }
    updateEmpty();
  }

  async function takeItems() {
    try {
      const items = await invoke("take_remote_items");
      await addNewCards();
      for (const item of items) {
        if (item.kind === "message") toast(item.text);
        else if (item.kind === "confirm") questions.push(item);
      }
      askNext();
    } catch (e) {
      toast(errText(e));
    }
  }

  // Items queued before the window loaded (a cold start from a link or command) wait in Rust.
  listen("remote-inbox", takeItems).then(takeItems);
})();
