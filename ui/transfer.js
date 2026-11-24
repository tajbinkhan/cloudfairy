"use strict";

// Export and import of saved tunnels, in the Settings dialog (see transfer.rs).
// Uses invoke, tunnels, renderCard, updateEmpty, plural, toast, errText and $ from app.js.
(() => {
  const section = document.createElement("div");
  section.className = "field-group";
  section.innerHTML = `
    <div class="label">Saved tunnels</div>
    <div class="transfer-actions">
      <button id="export-tunnels" class="btn small" type="button">Export to a file…</button>
      <button id="import-tunnels" class="btn small" type="button">Import from a file…</button>
    </div>
    <p class="hint">Passwords, link commands and file actions aren't exported. Imported tunnels don't start until you start them.</p>`;
  $("#settings-form .dialog-actions").before(section);

  const exportBtn = $("#export-tunnels");
  const importBtn = $("#import-tunnels");

  async function run(btn, task) {
    $("#settings-error").textContent = "";
    exportBtn.disabled = importBtn.disabled = true;
    try {
      await task();
    } catch (e) {
      $("#settings-error").textContent = errText(e);
    } finally {
      exportBtn.disabled = importBtn.disabled = false;
      btn.focus();
    }
  }

  exportBtn.addEventListener("click", () =>
    run(exportBtn, async () => {
      const count = await invoke("export_tunnels");
      if (count != null) toast(`Exported ${plural(count, "tunnel")}`);
    }),
  );

  importBtn.addEventListener("click", () =>
    run(importBtn, async () => {
      const result = await invoke("import_tunnels");
      if (!result) return;
      for (const t of result.tunnels) {
        tunnels.set(t.id, t);
        renderCard(t);
      }
      updateEmpty();
      toast(`Imported ${plural(result.imported, "tunnel")}, skipped ${result.skipped.toLocaleString()}`);
    }),
  );
})();
