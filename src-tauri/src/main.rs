// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_update;
mod background;
mod child_guard;
mod cloudflared;
mod named;
mod requests;
mod settings;
mod tunnel;

use std::time::{SystemTime, UNIX_EPOCH};

use app_update::{AppUpdate, PendingUpdate};
use serde::Serialize;
use settings::{SettingsStore, TunnelConfig};
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;
use tunnel::{Change, TunnelView, Tunnels};

/// Passed by the login item, so Cloudfairy starts in the tray instead of opening its window.
const MINIMIZED_ARG: &str = "--minimized";

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
fn add_tunnel(app: AppHandle, state: State<AppState>, name: String, target: String) -> CmdResult<TunnelView> {
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
        start_on_open: false,
        named: None,
    };
    state.settings.update(|s| s.tunnels.push(cfg.clone()))?;
    background::refresh_tray(&app);
    Ok(state.tunnels.view(cfg))
}

#[tauri::command]
fn update_tunnel(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    name: String,
    target: String,
    rewrite_host: bool,
    start_on_open: bool,
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
            t.start_on_open = start_on_open;
            t.clone()
        })
    })?;
    background::refresh_tray(&app);
    cfg.map(|c| state.tunnels.view(c))
        .ok_or_else(|| "Tunnel not found.".into())
}

#[tauri::command]
fn remove_tunnel(app: AppHandle, state: State<AppState>, id: String) -> CmdResult<()> {
    state.tunnels.remove(&id);
    state.settings.update(|s| s.tunnels.retain(|t| t.id != id))?;
    background::refresh_tray(&app);
    Ok(())
}

#[tauri::command]
async fn start_tunnel(app: AppHandle, id: String) -> CmdResult<TunnelView> {
    start_one(&app, &id).await
}

/// Starts one saved tunnel. Used by the window, the tray and "Start when Cloudfairy opens".
async fn start_one(app: &AppHandle, id: &str) -> CmdResult<TunnelView> {
    let state = app.state::<AppState>();
    let settings = state.settings.get();
    let cfg = settings
        .tunnels
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or("Tunnel not found.")?;
    let (bin, _) = cloudflared::resolve(app, settings.cloudflared_path.as_deref())?;
    state
        .tunnels
        .start(app, &cfg, &bin, &settings.protocol)
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

#[tauri::command]
fn tunnel_requests(state: State<AppState>, id: String) -> Vec<requests::RequestEntry> {
    state.tunnels.requests(&id)
}

#[tauri::command]
fn clear_tunnel_requests(state: State<AppState>, id: String) {
    state.tunnels.clear_requests(&id);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    cloudflared_path: Option<String>,
    protocol: String,
    close_to_tray: bool,
    notify: bool,
    copy_on_live: bool,
    launch_at_login: bool,
    /// False where the OS has no tray, so the window hides that option.
    tray_available: bool,
    /// The running build's version, from tauri.conf.json.
    app_version: String,
}

#[tauri::command]
fn get_settings(app: AppHandle, state: State<AppState>) -> SettingsView {
    let s = state.settings.get();
    SettingsView {
        cloudflared_path: s.cloudflared_path,
        protocol: s.protocol,
        close_to_tray: s.close_to_tray,
        notify: s.notify,
        copy_on_live: s.copy_on_live,
        launch_at_login: app.autolaunch().is_enabled().unwrap_or(false),
        tray_available: background::has_tray(&app),
        app_version: app.package_info().version.to_string(),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInput {
    cloudflared_path: Option<String>,
    protocol: String,
    close_to_tray: bool,
    notify: bool,
    copy_on_live: bool,
    launch_at_login: bool,
}

#[tauri::command]
fn save_settings(app: AppHandle, state: State<AppState>, input: SettingsInput) -> CmdResult<()> {
    if !matches!(input.protocol.as_str(), "auto" | "quic" | "http2") {
        return Err("Unknown protocol.".into());
    }
    let autolaunch = app.autolaunch();
    if autolaunch.is_enabled().unwrap_or(false) != input.launch_at_login {
        let result = if input.launch_at_login {
            autolaunch.enable()
        } else {
            autolaunch.disable()
        };
        result.map_err(|e| format!("Couldn't change the login item: {e}"))?;
    }
    let path = input
        .cloudflared_path
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    state.settings.update(|s| {
        s.cloudflared_path = path;
        s.protocol = input.protocol;
        s.close_to_tray = input.close_to_tray;
        s.notify = input.notify;
        s.copy_on_live = input.copy_on_live;
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
            background::show_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![MINIMIZED_ARG]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingUpdate::default())
        .setup(|app| {
            // Kills cloudflared left running by a session that crashed.
            child_guard::init(&app.path().app_data_dir()?);
            let config_dir = app.path().app_config_dir()?;
            let tunnels = Tunnels::default();
            // Tunnels reports changes while its lock is held; handle them on a separate task.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Change>();
            tunnels.set_listener(move |change| {
                let _ = tx.send(change);
            });
            app.manage(AppState {
                settings: SettingsStore::load(config_dir.join("settings.json")),
                tunnels,
            });
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(change) = rx.recv().await {
                    background::on_change(&handle, change);
                }
            });

            let tray = background::init_tray(app.handle());
            // The window starts hidden (see tauri.conf.json). The login item opens
            // Cloudfairy in the tray, unless there is no tray to open it from.
            let minimized = std::env::args().any(|a| a == MINIMIZED_ARG);
            if !minimized || !tray {
                background::show_window(app.handle());
            }
            background::start_on_open(app.handle());
            Ok(())
        })
        .on_window_event(|window, event| {
            let WindowEvent::CloseRequested { api, .. } = event else { return };
            let app = window.app_handle();
            let state = app.state::<AppState>();
            // With nothing running, closing the window quits as before.
            let keep = state.settings.get().close_to_tray && state.tunnels.any_active();
            if keep && background::has_tray(app) {
                api.prevent_close();
                let _ = window.hide();
                background::hidden_to_tray(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_tunnels,
            add_tunnel,
            update_tunnel,
            remove_tunnel,
            start_tunnel,
            stop_tunnel,
            tunnel_logs,
            tunnel_requests,
            clear_tunnel_requests,
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
            named::cloudflare_account,
            named::cloudflare_login,
            named::cloudflare_login_cancel,
            named::add_named_tunnel,
            named::set_tunnel_hostname,
            named::remove_named_tunnel,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Cloudfairy");

    app.run(|handle, event| match event {
        RunEvent::Exit => {
            if let Some(state) = handle.try_state::<AppState>() {
                state.tunnels.kill_all();
            }
        }
        // Clicking the Dock icon while the window is hidden in the tray.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => background::show_window(handle),
        _ => {}
    });
}
