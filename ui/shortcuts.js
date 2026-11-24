"use strict";

// Keyboard shortcuts. Uses tunnels, toast and $ from app.js, and drives the cards by clicking
// their buttons, so every shortcut goes through the same code as the mouse.
(() => {
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const MOD = isMac ? "Cmd" : "Ctrl";
  const modPressed = (e) => (isMac ? e.metaKey : e.ctrlKey);

  const SHORTCUTS = [
    [[MOD, "N"], "Add a tunnel"],
    [["Tab"], "Move to the next tunnel or button"],
    [["↑", "↓"], "Move between tunnels"],
    [["Space"], "Start or stop the tunnel"],
    [[MOD, "C"], "Copy its public link"],
    [["O"], "Open its link in your browser"],
    [["L"], "Show or hide its logs"],
    [["R"], "Show or hide its requests"],
    [["?"], "Show these shortcuts"],
    [["Esc"], "Close a dialog"],
  ];

  function buildHelp() {
    const dialog = document.createElement("dialog");
    dialog.id = "shortcuts-dialog";
    dialog.className = "dialog";
    // Modifier combinations read "Ctrl + N", alternatives read "↑ ↓".
    const kbd = (keys) => keys.map((k) => `<kbd>${escapeHtml(k)}</kbd>`).join(keys[0] === MOD ? " + " : " ");
    dialog.innerHTML = `
      <form method="dialog">
        <h2>Keyboard shortcuts</h2>
        <p class="hint">Select a tunnel with Tab or a click, then use its keys.</p>
        <dl class="shortcut-list">
          ${SHORTCUTS.map(([keys, what]) => `<dt>${kbd(keys)}</dt><dd>${escapeHtml(what)}</dd>`).join("")}
        </dl>
        <div class="dialog-actions">
          <button class="btn primary" value="close" type="submit">Done</button>
        </div>
      </form>`;
    document.body.append(dialog);
    return dialog;
  }

  let help = null;
  function openHelp() {
    help ??= buildHelp();
    help.showModal();
  }

  function focusComposer() {
    const input = $("#target");
    input.scrollIntoView({ block: "nearest" });
    input.focus();
    input.select();
  }

  // Cards take focus, so Tab stops on them and the keys below know which tunnel is meant.
  function makeFocusable(card) {
    if (card.tabIndex !== 0) card.tabIndex = 0;
  }
  const list = $("#list");
  for (const card of list.children) makeFocusable(card);
  new MutationObserver((records) => {
    for (const r of records) for (const node of r.addedNodes) if (node.classList?.contains("card")) makeFocusable(node);
  }).observe(list, { childList: true });

  function isTyping(el) {
    return !!el && (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName));
  }

  function press(card, selector) {
    $(selector, card)?.click();
  }

  function moveFocus(card, step) {
    const cards = [...list.querySelectorAll(":scope > .card")];
    const next = cards[cards.indexOf(card) + step];
    if (next) {
      next.focus();
      next.scrollIntoView({ block: "nearest" });
    }
  }

  function withLink(card, selector) {
    const t = tunnels.get(card.dataset.id);
    if (!t?.publicUrl) {
      toast("Start the tunnel to get a link");
      return;
    }
    press(card, selector);
  }

  document.addEventListener("keydown", (e) => {
    if (e.defaultPrevented || e.isComposing) return;
    const mod = modPressed(e) && !e.altKey;
    const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
    // Keep the webview from opening a new window, even when the shortcut can't run.
    if (mod && !e.shiftKey && key === "n") e.preventDefault();
    if (document.querySelector("dialog[open]") || isTyping(e.target)) return;

    if (mod && !e.shiftKey && key === "n") {
      focusComposer();
      return;
    }
    if (e.key === "?" && !modPressed(e) && !e.altKey) {
      e.preventDefault();
      openHelp();
      return;
    }

    const card = e.target.closest?.("#list > .card");
    if (!card) return;
    if (mod && !e.shiftKey && key === "c") {
      // Selected text copies as usual.
      if (window.getSelection()?.toString()) return;
      e.preventDefault();
      withLink(card, '.url-actions [data-action="copy"]');
      return;
    }
    if (modPressed(e) || e.altKey || e.shiftKey) return;
    switch (key) {
      case "ArrowDown":
      case "ArrowUp":
        e.preventDefault();
        moveFocus(card, key === "ArrowDown" ? 1 : -1);
        break;
      case " ":
        // On a button inside the card, Space presses that button instead.
        if (e.target !== card) return;
        e.preventDefault();
        press(card, '[data-action="toggle"]');
        break;
      case "o":
        e.preventDefault();
        withLink(card, '.url-actions [data-action="open"]');
        break;
      case "l":
        e.preventDefault();
        press(card, '[data-action="logs"]');
        break;
      case "r":
        e.preventDefault();
        press(card, '[data-action="requests"]');
        break;
    }
  });
})();
