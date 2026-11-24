//! Tauri commands for Cloudfairy's proxy: request bodies, replay, per-tunnel options and the
//! "Route tunnels through Cloudfairy" setting.

use serde::Serialize;
use tauri::State;

use crate::{
    capture::{BodyView, Side},
    proxy,
    requests::RequestEntry,
    settings::HeaderPair,
    tunnel::TunnelView,
    AppState, CmdResult,
};

#[tauri::command]
pub fn request_body(state: State<AppState>, id: String, entry_id: u64, side: String) -> CmdResult<Option<BodyView>> {
    let side = Side::parse(&side).ok_or("Unknown body.")?;
    Ok(state.tunnels.request_body(&id, entry_id, side))
}

#[tauri::command]
pub async fn replay_request(state: State<'_, AppState>, id: String, entry_id: u64) -> CmdResult<RequestEntry> {
    state.tunnels.replay(&id, entry_id).await
}

/// Saves a tunnel's password, CORS and extra header options.
#[tauri::command]
pub fn update_tunnel_proxy(
    state: State<AppState>,
    id: String,
    auth_user: String,
    auth_password: String,
    cors: bool,
    extra_headers: Vec<HeaderPair>,
) -> CmdResult<TunnelView> {
    if state.tunnels.is_active(&id) {
        return Err("Stop the tunnel before editing it.".into());
    }
    let auth_user = auth_user.trim().to_string();
    if auth_user.contains(':') {
        return Err("The username can't contain a colon.".into());
    }
    if !auth_user.is_empty() && auth_password.is_empty() {
        return Err("Enter a password, or clear the username.".into());
    }
    let extra_headers: Vec<HeaderPair> = extra_headers
        .into_iter()
        .map(|h| HeaderPair {
            name: h.name.trim().to_string(),
            value: h.value.trim().to_string(),
        })
        .filter(|h| !h.name.is_empty() || !h.value.is_empty())
        .collect();
    let pairs: Vec<(String, String)> = extra_headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect();
    proxy::parse_extra_headers(&pairs)?;

    let cfg = state.settings.update(|s| {
        s.tunnels.iter_mut().find(|t| t.id == id).map(|t| {
            t.auth_user = auth_user;
            t.auth_password = auth_password;
            t.cors = cors;
            t.extra_headers = extra_headers;
            t.clone()
        })
    })?;
    cfg.map(|c| state.tunnels.view(c))
        .ok_or_else(|| "Tunnel not found.".into())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettings {
    route_through_proxy: bool,
}

#[tauri::command]
pub fn get_proxy_settings(state: State<AppState>) -> ProxySettings {
    ProxySettings {
        route_through_proxy: state.settings.get().route_through_proxy,
    }
}

#[tauri::command]
pub fn save_proxy_settings(state: State<AppState>, route_through_proxy: bool) -> CmdResult<()> {
    state.settings.update(|s| s.route_through_proxy = route_through_proxy)
}
