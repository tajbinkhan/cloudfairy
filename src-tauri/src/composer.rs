//! Commands behind adding and editing tunnels that aren't plain addresses: port
//! suggestions and shared folders.

use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::{
    background, ports, share,
    settings::{TunnelConfig, TunnelKind},
    tunnel::{self, TunnelView},
    AppState, CmdResult,
};

/// Ports something listens on, minus saved tunnels' ports and Cloudfairy's own sockets.
#[tauri::command]
pub async fn suggest_ports(state: State<'_, AppState>) -> CmdResult<Vec<ports::Suggestion>> {
    let (pids, mut exclude) = state.tunnels.own_listeners();
    exclude.extend(share::ports());
    exclude.extend(
        state
            .settings
            .get()
            .tunnels
            .iter()
            .filter_map(|t| tunnel::host_port(&t.target))
            .filter(|(host, _)| tunnel::is_loopback(host))
            .map(|(_, port)| port),
    );
    Ok(ports::suggest(pids, exclude).await)
}

/// Opens the system folder picker. Returns `None` when the user cancels.
#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app.dialog().file().set_title("Choose a folder to share");
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    dialog.pick_folder(move |path| {
        let _ = tx.send(path);
    });
    let path = rx.await.ok()??.into_path().ok()?;
    Some(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn add_folder_tunnel(app: AppHandle, state: State<AppState>, name: String, folder: String) -> CmdResult<TunnelView> {
    let folder = share::validate_folder(&folder)?;
    let name = match name.trim() {
        "" => share::folder_name(&folder),
        n => n.to_string(),
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let cfg = TunnelConfig {
        id: format!("t{nanos:x}"),
        name,
        target: String::new(),
        rewrite_host: false,
        start_on_open: false,
        kind: TunnelKind::Folder,
        folder: Some(folder),
        lifecycle: Default::default(),
        named: None,
    };
    state.settings.update(|s| s.tunnels.push(cfg.clone()))?;
    background::refresh_tray(&app);
    Ok(state.tunnels.view(cfg))
}

#[tauri::command]
pub fn update_folder_tunnel(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    name: String,
    folder: String,
    start_on_open: bool,
) -> CmdResult<TunnelView> {
    if state.tunnels.is_active(&id) {
        return Err("Stop the tunnel before editing it.".into());
    }
    let folder = share::validate_folder(&folder)?;
    let name = name.trim().to_string();
    let cfg = state.settings.update(|s| {
        s.tunnels
            .iter_mut()
            .find(|t| t.id == id && t.kind == TunnelKind::Folder)
            .map(|t| {
                if !name.is_empty() {
                    t.name = name;
                }
                t.folder = Some(folder);
                t.start_on_open = start_on_open;
                t.clone()
            })
    })?;
    background::refresh_tray(&app);
    cfg.map(|c| state.tunnels.view(c))
        .ok_or_else(|| "Tunnel not found.".into())
}
