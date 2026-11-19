// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_update;
mod child_guard;
mod cloudflared;
mod settings;
mod tunnel;

use std::time::{SystemTime, UNIX_EPOCH};

use app_update::{AppUpdate, PendingUpdate};
use serde::Serialize;
use settings::{SettingsStore, TunnelConfig};
use tauri::{AppHandle, Manager, RunEvent, State};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tunnel::{TunnelView, Tunnels};

struct AppState {
    settings: SettingsStore,
    tunnels: Tunnels,
}

type CmdResult<T> = Result<T, String>;

#[tauri::command]
fn list_tunnels(state: State<AppState>) -> Vec<TunnelView> {
    state
        .settings
        .get()
        .tunnels
        .into_iter()
        .map(|cfg| state.tunnels.view(cfg))
        .collect()
}

#[tauri::command]
fn add_tunnel(state: State<AppState>, name: String, target: String) -> CmdResult<TunnelView> {
    let target = tunnel::normalize_target(&target)?;
    let name = match name.trim() {
        "" => target
            .split_once("://")
            .map_or(target.as_str(), |(_, rest)| rest)
            .to_string(),
        n => n.to_string(),
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let cfg = TunnelConfig {
        id: format!("t{nanos:x}"),
        name,
        target,
        rewrite_host: false,
    };
    state.settings.update(|s| s.tunnels.push(cfg.clone()))?;
    Ok(state.tunnels.view(cfg))
}

#[tauri::command]
fn update_tunnel(
    state: State<AppState>,
    id: String,
    name: String,
    target: String,
    rewrite_host: bool,
) -> CmdResult<TunnelView> {
    if state.tunnels.is_active(&id) {
        return Err("Stop the tunnel before editing it.".into());
    }
    let target = tunnel::normalize_target(&target)?;
    let name = name.trim().to_string();
    let cfg = state.settings.update(|s| {
        s.tunnels.iter_mut().find(|t| t.id == id).map(|t| {
            if !name.is_empty() {
                t.name = name;
            }
            t.target = target;
            t.rewrite_host = rewrite_host;
            t.clone()
        })
    })?;
    cfg.map(|c| state.tunnels.view(c))
        .ok_or_else(|| "Tunnel not found.".into())
}

#[tauri::command]
fn remove_tunnel(state: State<AppState>, id: String) -> CmdResult<()> {
    state.tunnels.remove(&id);
    state.settings.update(|s| s.tunnels.retain(|t| t.id != id))
}

#[tauri::command]
async fn start_tunnel(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<TunnelView> {
    let settings = state.settings.get();
    let cfg = settings
        .tunnels
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or("Tunnel not found.")?;
    let (bin, _) = cloudflared::resolve(&app, settings.cloudflared_path.as_deref())?;
    state
        .tunnels
        .start(&app, &cfg, &bin, &settings.protocol)
        .await?;
    Ok(state.tunnels.view(cfg))
}

#[tauri::command]
fn stop_tunnel(state: State<AppState>, id: String) {
    state.tunnels.stop(&id);
}

#[tauri::command]
fn tunnel_logs(state: State<AppState>, id: String) -> Vec<String> {
    state.tunnels.logs(&id)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    cloudflared_path: Option<String>,
    protocol: String,
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> SettingsView {
    let s = state.settings.get();
    SettingsView {
        cloudflared_path: s.cloudflared_path,
        protocol: s.protocol,
    }
}

#[tauri::command]
fn save_settings(state: State<AppState>, cloudflared_path: Option<String>, protocol: String) -> CmdResult<()> {
    if !matches!(protocol.as_str(), "auto" | "quic" | "http2") {
        return Err("Unknown protocol.".into());
    }
    let path = cloudflared_path
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    state.settings.update(|s| {
        s.cloudflared_path = path;
        s.protocol = protocol;
    })
}

#[tauri::command]
async fn cloudflared_info(app: AppHandle, state: State<'_, AppState>) -> CmdResult<cloudflared::Info> {
    let custom = state.settings.get().cloudflared_path;
    Ok(cloudflared::info(&app, custom.as_deref()).await)
}

#[tauri::command]
async fn latest_cloudflared() -> CmdResult<String> {
    cloudflared::latest_version().await
}

#[tauri::command]
async fn download_cloudflared(app: AppHandle, state: State<'_, AppState>) -> CmdResult<cloudflared::Info> {
    if cfg!(windows) && state.tunnels.any_active() {
        return Err("Stop all tunnels before updating cloudflared.".into());
    }
    cloudflared::download(&app).await?;
    // Use the freshly downloaded copy rather than an old custom path.
    state.settings.update(|s| s.cloudflared_path = None)?;
    Ok(cloudflared::info(&app, None).await)
}

#[tauri::command]
async fn check_app_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> CmdResult<Option<AppUpdate>> {
    app_update::check(&app, &pending).await
}

#[tauri::command]
async fn install_app_update(
    app: AppHandle,
    state: State<'_, AppState>,
    pending: State<'_, PendingUpdate>,
) -> CmdResult<()> {
    let (update, bytes) = app_update::download(&app, &pending).await?;
    // Installing replaces the running app (on Windows it exits right away), so stop
    // cloudflared now or its processes would outlive Cloudfairy.
    state.tunnels.kill_all();
    update
        .install(bytes)
        .map_err(|e| format!("Couldn't install the update: {e}"))?;
    app.restart();
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> CmdResult<()> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Only web links can be opened.".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn copy_text(app: AppHandle, text: String) -> CmdResult<()> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn qr_svg(text: String) -> CmdResult<String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#111111"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

fn main() {
    let app = tauri::Builder::default()
        // Must be first. A second copy would keep its own list of tunnels and overwrite
        // settings.json, so it exits and brings this window to the front instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingUpdate::default())
        .setup(|app| {
            // Kills cloudflared left running by a session that crashed.
            child_guard::init(&app.path().app_data_dir()?);
            let config_dir = app.path().app_config_dir()?;
            app.manage(AppState {
                settings: SettingsStore::load(config_dir.join("settings.json")),
                tunnels: Tunnels::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_tunnels,
            add_tunnel,
            update_tunnel,
            remove_tunnel,
            start_tunnel,
            stop_tunnel,
            tunnel_logs,
            get_settings,
            save_settings,
            cloudflared_info,
            latest_cloudflared,
            download_cloudflared,
            check_app_update,
            install_app_update,
            open_url,
            copy_text,
            qr_svg,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Cloudfairy");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            if let Some(state) = handle.try_state::<AppState>() {
                state.tunnels.kill_all();
            }
        }
    });
}
