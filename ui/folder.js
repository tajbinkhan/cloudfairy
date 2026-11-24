// Folder tunnels: the "Share a folder" button and the folder row in the Edit dialog.
// Loaded before app.js; it uses app.js's tunnels, renderCard, startTunnel and errText only
// inside event handlers. app.js calls window.cfFolder.openEdit and .saveEdit.
(() => {
  "use strict";

  const { invoke } = window.__TAURI__.core;
  const byId = (id) => document.getElementById(id);
  const message = (e) => (typeof e === "string" ? e : e?.message ?? String(e));

  /** The folder shown in the Edit dialog, or null while it edits an address tunnel. */
  let editFolder = null;

  async function shareFolder() {
    const button = byId("share-folder");
    const errorEl = byId("composer-error");
    errorEl.textContent = "";
    button.disabled = true;
    let view = null;
    try {
      const folder = await invoke("pick_folder");
      if (folder) view = await invoke("add_folder_tunnel", { name: byId("name").value, folder });
    } catch (e) {
      errorEl.textContent = message(e);
    } finally {
      button.disabled = false;
    }
    if (!view) return;
    byId("name").value = "";
    tunnels.set(view.id, view);
    renderCard(view);
    startTunnel(view.id);
  }

  function openEdit(t) {
    const isFolder = t.kind === "folder";
    editFolder = isFolder ? t.folder : null;
    byId("edit-target").closest(".field").hidden = isFolder;
    byId("edit-rewrite-host").closest(".check").hidden = isFolder;
    byId("edit-rewrite-hint").hidden = isFolder;
    byId("edit-folder-row").hidden = !isFolder;
    byId("edit-folder").textContent = t.folder ?? "";
  }

  async function changeFolder() {
    try {
      const folder = await invoke("pick_folder");
      if (!folder) return;
      editFolder = folder;
      byId("edit-folder").textContent = folder;
    } catch (e) {
      byId("edit-error").textContent = message(e);
    }
  }

  /** Saves a folder tunnel. Returns false when the dialog edits an address tunnel instead. */
  async function saveEdit(id) {
    if (editFolder == null) return false;
    try {
      const view = await invoke("update_folder_tunnel", {
        id,
        name: byId("edit-name").value,
        folder: editFolder,
        startOnOpen: byId("edit-start-on-open").checked,
      });
      Object.assign(tunnels.get(view.id), view);
      renderCard(tunnels.get(view.id));
      byId("edit-dialog").close();
    } catch (e) {
      byId("edit-error").textContent = message(e);
    }
    return true;
  }

  byId("share-folder")?.addEventListener("click", shareFolder);
  const change = byId("edit-folder-change");
  change?.addEventListener("click", changeFolder);
  // The Edit form saves on Enter; on this button Enter should press the button instead.
  change?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") e.stopPropagation();
  });
  window.cfFolder = { openEdit, saveEdit };
})();
