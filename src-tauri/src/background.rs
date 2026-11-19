//! What Cloudfairy does outside its window: the tray icon and menu, desktop notifications,
//! copying new links, and starting tunnels when the app opens.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::atomic::{AtomicBool, Ordering},
};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

use crate::{
    tunnel::{Change, Status, TunnelView},
    AppState,
};

pub const TRAY_ID: &str = "main";

/// Set once the "still running in the tray" notification has been shown this session.
static HIDE_HINT_SHOWN: AtomicBool = AtomicBool::new(false);

pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn window_focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").is_some_and(|w| {
        w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false)
    })
}

pub fn has_tray(app: &AppHandle) -> bool {
    app.tray_by_id(TRAY_ID).is_some()
}

/// Creates the tray icon. Returns false where the OS has no tray: some Linux desktops
/// lack the AppIndicator library, which the tray code loads at runtime and panics without.
pub fn init_tray(app: &AppHandle) -> bool {
    match catch_unwind(AssertUnwindSafe(|| build_tray(app))) {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            eprintln!("Couldn't create the tray icon: {e}");
            false
        }
        Err(_) => {
            eprintln!("Couldn't create the tray icon: the system tray isn't available.");
            false
        }
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip(tooltip(app))
        // Left click opens the window; the menu is on right click (Linux always shows the menu).
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn views(app: &AppHandle) -> Vec<TunnelView> {
    let state = app.state::<AppState>();
    let configs = state.settings.get().tunnels;
    configs.into_iter().map(|c| state.tunnels.view(c)).collect()
}

fn is_active(v: &TunnelView) -> bool {
    matches!(v.runtime.status, Status::Starting | Status::Running)
}

fn status_label(v: &TunnelView) -> &'static str {
    match v.runtime.status {
        Status::Running if v.runtime.connections == 0 => "Reconnecting",
        Status::Running => "Live",
        Status::Starting => "Starting",
        Status::Error => "Failed",
        Status::Stopped => "Stopped",
    }
}

/// Menu labels treat `&` as a mnemonic marker, so a literal one is doubled.
fn menu_text(s: &str) -> String {
    s.replace('&', "&&")
}

fn tooltip(app: &AppHandle) -> String {
    let live = views(app)
        .iter()
        .filter(|v| v.runtime.status == Status::Running)
        .count();
    match live {
        0 => "Cloudfairy".into(),
        1 => "Cloudfairy · 1 tunnel live".into(),
        n => format!("Cloudfairy · {n} tunnels live"),
    }
}

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let views = views(app);
    let menu = Menu::new(app)?;
    if views.is_empty() {
        menu.append(&MenuItem::with_id(app, "none", "No tunnels yet", false, None::<&str>)?)?;
    }
    // Newest first, like the window.
    for v in views.iter().rev() {
        let id = &v.config.id;
        let has_url = v.runtime.public_url.is_some();
        let label = format!("{} · {}", menu_text(&v.config.name), status_label(v));
        let copy = MenuItem::with_id(app, format!("copy:{id}"), "Copy link", has_url, None::<&str>)?;
        let open = MenuItem::with_id(app, format!("open:{id}"), "Open in browser", has_url, None::<&str>)?;
        let toggle_text = if is_active(v) { "Stop" } else { "Start" };
        let toggle = MenuItem::with_id(app, format!("toggle:{id}"), toggle_text, true, None::<&str>)?;
        menu.append(&Submenu::with_items(app, label, true, &[&copy, &open, &toggle])?)?;
    }
    let any_active = views.iter().any(is_active);
    let any_idle = views.iter().any(|v| !is_active(v));
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "start-all", "Start all", any_idle, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "stop-all", "Stop all", any_active, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "show", "Show Cloudfairy", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit Cloudfairy", true, None::<&str>)?)?;
    Ok(menu)
}

/// Rebuilds the tray menu and tooltip from the current tunnels.
pub fn refresh_tray(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else { return };
    match build_menu(app) {
        Ok(menu) => {
            let _ = tray.set_menu(Some(menu));
        }
        Err(e) => eprintln!("Couldn't update the tray menu: {e}"),
    }
    let _ = tray.set_tooltip(Some(tooltip(app)));
}

fn on_menu(app: &AppHandle, item: &str) {
    match item {
        "show" => show_window(app),
        "quit" => app.exit(0),
        "start-all" => {
            for v in views(app).into_iter().filter(|v| !is_active(v)) {
                spawn_start(app, v.config.id);
            }
        }
        "stop-all" => {
            let state = app.state::<AppState>();
            for v in views(app).into_iter().filter(is_active) {
                state.tunnels.stop(&v.config.id);
            }
        }
        _ => {
            let Some((action, id)) = item.split_once(':') else { return };
            let Some(v) = views(app).into_iter().find(|v| v.config.id == id) else { return };
            let url = v.runtime.public_url.clone();
            match action {
                "copy" => {
                    if let Some(url) = url {
                        let _ = app.clipboard().write_text(url);
                    }
                }
                "open" => {
                    if let Some(url) = url {
                        let _ = app.opener().open_url(url, None::<&str>);
                    }
                }
                "toggle" if is_active(&v) => app.state::<AppState>().tunnels.stop(id),
                "toggle" => spawn_start(app, v.config.id),
                _ => {}
            }
        }
    }
}

/// Starts a tunnel from outside the window (tray or app launch). Errors land on the card.
fn spawn_start(app: &AppHandle, id: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::start_one(&app, &id).await {
            let state = app.state::<AppState>();
            if !state.tunnels.is_active(&id) {
                state.tunnels.fail(&app, &id, e);
            }
        }
    });
}

/// Starts every tunnel marked "Start when Cloudfairy opens".
pub fn start_on_open(app: &AppHandle) {
    let configs = app.state::<AppState>().settings.get().tunnels;
    for cfg in configs.into_iter().filter(|c| c.start_on_open) {
        spawn_start(app, cfg.id);
    }
}

/// Handles a change reported by `Tunnels`. Runs on its own task, never under the tunnel lock.
pub fn on_change(app: &AppHandle, change: Change) {
    match change {
        Change::Updated => refresh_tray(app),
        Change::Live { id, url } => {
            let settings = app.state::<AppState>().settings.get();
            let Some(cfg) = settings.tunnels.iter().find(|t| t.id == id) else { return };
            let copied = settings.copy_on_live && app.clipboard().write_text(url.clone()).is_ok();
            if settings.notify && !window_focused(app) {
                let body = if copied { format!("{url}\nLink copied.") } else { url };
                notify(app, &format!("{} is live", cfg.name), &body);
            }
        }
        Change::Failed { id, error } => {
            let settings = app.state::<AppState>().settings.get();
            let Some(cfg) = settings.tunnels.iter().find(|t| t.id == id) else { return };
            if settings.notify && !window_focused(app) {
                notify(app, &format!("{} stopped", cfg.name), &error);
            }
        }
    }
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    if let Err(e) = app.notification().builder().title(title).body(body).show() {
        eprintln!("Couldn't show a notification: {e}");
    }
}

/// Called when the window is hidden to the tray. Says so once per session, so closing the
/// window doesn't look like it stopped the tunnels.
pub fn hidden_to_tray(app: &AppHandle) {
    if HIDE_HINT_SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let active = views(app).iter().filter(|v| is_active(v)).count();
    let tunnels = if active == 1 { "1 tunnel is".to_string() } else { format!("{active} tunnels are") };
    notify(
        app,
        "Cloudfairy is still running",
        &format!("{tunnels} still running. Use the tray icon to open Cloudfairy or quit."),
    );
}
