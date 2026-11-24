//! Per-tunnel lifecycle options, set in the Edit dialog: restart after a failure, stop after a
//! set time, and hand each new link to other tools (see `link_hooks`).
//!
//! It reacts to the [`Change`]s that `Tunnels` reports, and tells the window about pending
//! restarts and stop times with the `tunnel-lifecycle` event.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    background, link_hooks,
    settings::TunnelConfig,
    tunnel::{Change, Status, TunnelView},
    AppState,
};

pub const EVENT: &str = "tunnel-lifecycle";

/// "Stop after" choices in minutes. 0 means never.
const STOP_AFTER_CHOICES: [u32; 6] = [0, 15, 30, 60, 120, 480];
const FIRST_RETRY_SECS: u64 = 5;
const MAX_RETRY_SECS: u64 = 300;
/// Cloudflare limits how often quick tunnels can be created, so a 429 waits at least this long.
const RATE_LIMIT_RETRY_SECS: u64 = 60;
/// A run that stayed live this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LifecycleConfig {
    /// Start the tunnel again, with backoff, when it ends in an error.
    pub restart_on_failure: bool,
    /// Stop the tunnel this many minutes after it goes live. 0 means never.
    pub stop_after_minutes: u32,
    /// Full path of a file that gets each new link. Empty means off.
    pub url_file: String,
    /// When set, the file gets a `NAME=<link>` line instead of only the link.
    pub url_file_var: String,
    /// Shell command run for each new link. `{url}` is replaced by the link.
    pub url_command: String,
}

impl LifecycleConfig {
    fn has_hooks(&self) -> bool {
        !self.url_file.trim().is_empty() || !self.url_command.trim().is_empty()
    }

    /// Trims the text fields and checks them, with messages for the Edit dialog.
    fn normalized(mut self) -> Result<Self, String> {
        if !STOP_AFTER_CHOICES.contains(&self.stop_after_minutes) {
            return Err("Pick a stop time from the list.".into());
        }
        self.url_file = self.url_file.trim().to_string();
        self.url_file_var = self.url_file_var.trim().to_string();
        self.url_command = self.url_command.trim().to_string();
        if !self.url_file.is_empty() && !Path::new(&self.url_file).is_absolute() {
            return Err(if cfg!(windows) {
                "Enter the file's full path, like C:\\Projects\\app\\.env.".into()
            } else {
                "Enter the file's full path, like /home/me/app/.env.".into()
            });
        }
        if !self.url_file_var.is_empty() {
            if self.url_file.is_empty() {
                return Err("Enter a file for the variable, or clear the variable name.".into());
            }
            if !link_hooks::is_var_name(&self.url_file_var) {
                return Err("Variable names use letters, digits and _, and don't start with a digit.".into());
            }
        }
        if self.url_command.contains(['\r', '\n']) {
            return Err("Put the command on one line.".into());
        }
        Ok(self)
    }
}

/// Wait before restart number `attempt` (0 for the first): 5 s, 10 s, 20 s … up to 5 min.
fn retry_delay(attempt: u32, rate_limited: bool) -> Duration {
    let secs = FIRST_RETRY_SECS
        .saturating_mul(1u64 << attempt.min(16))
        .min(MAX_RETRY_SECS);
    let secs = if rate_limited { secs.max(RATE_LIMIT_RETRY_SECS) } else { secs };
    Duration::from_secs(secs)
}

fn is_rate_limited(error: &str) -> bool {
    error.contains("429") || error.to_ascii_lowercase().contains("too many requests")
}

/// "15 min", "1 h", "8 h".
fn minutes_label(minutes: u32) -> String {
    if minutes >= 60 && minutes % 60 == 0 {
        format!("{} h", minutes / 60)
    } else {
        format!("{minutes} min")
    }
}

#[derive(Default)]
struct Entry {
    /// Restarts since the tunnel last stayed live for [`STABLE_AFTER`].
    attempts: u32,
    live_since: Option<Instant>,
    /// Changing a token cancels the timer that holds the old one.
    restart_token: u64,
    /// Unix ms of the pending restart.
    restart_at: Option<u64>,
    stop_token: u64,
    /// Unix ms when the tunnel stops, for the run whose link is `stop_url`.
    stop_at: Option<u64>,
    stop_url: Option<String>,
    /// The stop time of a run that failed, so an automatic restart keeps the same limit.
    carried_stop_at: Option<u64>,
    /// Set while an automatic restart is starting the tunnel.
    auto_restarting: bool,
    /// The last time the tunnel went live came from an automatic restart.
    restarted: bool,
    last_url: Option<String>,
    hook_error: Option<String>,
    /// Shown on a stopped card, e.g. "Stopped after 30 min."
    notice: Option<String>,
    /// Runs the file and command steps for one link at a time.
    hook_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleView {
    id: String,
    restart_at: Option<u64>,
    stop_at: Option<u64>,
    stop_url: Option<String>,
    hook_error: Option<String>,
    notice: Option<String>,
}

fn view(id: &str, e: &Entry) -> LifecycleView {
    LifecycleView {
        id: id.into(),
        restart_at: e.restart_at,
        stop_at: e.stop_at,
        stop_url: e.stop_url.clone(),
        hook_error: e.hook_error.clone(),
        notice: e.notice.clone(),
    }
}

/// Lifecycle state of every tunnel. Lives in Tauri's managed state.
#[derive(Default)]
pub struct Lifecycles {
    map: Mutex<HashMap<String, Entry>>,
    next_token: AtomicU64,
}

impl Lifecycles {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed) + 1
    }
}

fn lifecycles(app: &AppHandle) -> State<'_, Lifecycles> {
    app.state::<Lifecycles>()
}

fn emit(app: &AppHandle, id: &str, e: &Entry) {
    let _ = app.emit(EVENT, view(id, e));
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn config(app: &AppHandle, id: &str) -> Option<TunnelConfig> {
    let state = app.state::<AppState>();
    let tunnels = state.settings.get().tunnels;
    tunnels.into_iter().find(|t| t.id == id)
}

fn tunnel_view(app: &AppHandle, id: &str) -> Option<TunnelView> {
    config(app, id).map(|c| app.state::<AppState>().tunnels.view(c))
}

fn log(app: &AppHandle, id: &str, line: String) {
    app.state::<AppState>().tunnels.log_line(app, id, line);
}

/// Handles a change reported by `Tunnels`. Runs on the change task, before `background`.
pub fn on_change(app: &AppHandle, change: &Change) {
    match change {
        Change::Updated => {}
        Change::Live { id, url } => on_live(app, id, url),
        Change::Failed { id, error } => on_failed(app, id, error),
    }
}

fn on_live(app: &AppHandle, id: &str, url: &str) {
    let Some(cfg) = config(app, id) else { return };
    let opts = &cfg.lifecycle;
    let lc = lifecycles(app);
    let mut map = lc.lock();
    let e = map.entry(id.to_string()).or_default();
    e.live_since = Some(Instant::now());
    e.restart_token = lc.token();
    e.restart_at = None;
    e.notice = None;
    e.restarted = std::mem::take(&mut e.auto_restarting);
    let carried = e.carried_stop_at.take();

    e.stop_token = lc.token();
    e.stop_at = None;
    e.stop_url = None;
    if opts.stop_after_minutes > 0 {
        let fresh = now_ms() + u64::from(opts.stop_after_minutes) * 60_000;
        // A manual start gets a fresh countdown; an automatic restart keeps the old one.
        let at = if e.restarted { carried.unwrap_or(fresh) } else { fresh };
        e.stop_at = Some(at);
        e.stop_url = Some(url.to_string());
        tauri::async_runtime::spawn(stop_when_due(
            app.clone(),
            id.to_string(),
            e.stop_token,
            url.to_string(),
            at,
            opts.stop_after_minutes,
        ));
    }

    // Reconnects keep the link and don't come here, but a repeat is skipped all the same.
    let new_link = e.last_url.as_deref() != Some(url);
    if new_link {
        e.last_url = Some(url.to_string());
        e.hook_error = None;
    }
    let hook_lock = (new_link && opts.has_hooks()).then(|| e.hook_lock.clone());
    emit(app, id, e);
    drop(map);

    if let Some(lock) = hook_lock {
        let app = app.clone();
        let url = url.to_string();
        tauri::async_runtime::spawn(async move {
            let _one_at_a_time = lock.lock().await;
            // A newer link may have arrived while an older command was still running.
            let current = lifecycles(&app).lock().get(&cfg.id).and_then(|e| e.last_url.clone());
            if current.as_deref() != Some(url.as_str()) {
                return;
            }
            let result = link_hooks::run(&app, &cfg, &url).await;
            let lc = lifecycles(&app);
            let mut map = lc.lock();
            if let Some(e) = map.get_mut(&cfg.id).filter(|e| e.last_url.as_deref() == Some(url.as_str())) {
                e.hook_error = result.err();
                emit(&app, &cfg.id, e);
            }
        });
    }
}

fn on_failed(app: &AppHandle, id: &str, error: &str) {
    let Some(cfg) = config(app, id) else { return };
    let lc = lifecycles(app);
    let mut map = lc.lock();
    let e = map.entry(id.to_string()).or_default();
    // A failure can be reported twice (see `Tunnels::fail`); one restart is enough.
    if e.restart_at.is_some() {
        return;
    }
    if e.live_since.take().is_some_and(|t| t.elapsed() >= STABLE_AFTER) {
        e.attempts = 0;
    }
    e.stop_token = lc.token();
    let deadline = e.stop_at.take();
    e.stop_url = None;
    e.auto_restarting = false;
    e.notice = None;
    if !cfg.lifecycle.restart_on_failure {
        e.attempts = 0;
        e.carried_stop_at = None;
        emit(app, id, e);
        return;
    }
    if deadline.is_some() {
        e.carried_stop_at = deadline;
    }
    let delay = retry_delay(e.attempts, is_rate_limited(error));
    e.attempts = e.attempts.saturating_add(1);
    e.restart_token = lc.token();
    e.restart_at = Some(now_ms() + delay.as_millis() as u64);
    let token = e.restart_token;
    emit(app, id, e);
    drop(map);

    log(app, id, format!("[cloudfairy] Restarting in {} s.", delay.as_secs()));
    tauri::async_runtime::spawn(restart_when_due(app.clone(), id.to_string(), token, delay));
}

async fn restart_when_due(app: AppHandle, id: String, token: u64, delay: Duration) {
    tokio::time::sleep(delay).await;
    // Only a tunnel that is still failed restarts. A manual Start or Stop wins.
    let still_failed = tunnel_view(&app, &id).is_some_and(|v| v.runtime.status == Status::Error);
    {
        let lc = lifecycles(&app);
        let mut map = lc.lock();
        let Some(e) = map.get_mut(&id).filter(|e| e.restart_token == token) else { return };
        e.restart_at = None;
        e.auto_restarting = still_failed;
        emit(&app, &id, e);
    }
    if !still_failed {
        return;
    }
    if let Err(err) = crate::start_one(&app, &id).await {
        let state = app.state::<AppState>();
        if !state.tunnels.is_active(&id) {
            // Reports the failure again, which schedules the next try.
            state.tunnels.fail(&app, &id, err);
        }
    }
}

async fn stop_when_due(app: AppHandle, id: String, token: u64, url: String, at: u64, minutes: u32) {
    tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()))).await;
    {
        let lc = lifecycles(&app);
        let mut map = lc.lock();
        let Some(e) = map.get_mut(&id).filter(|e| e.stop_token == token) else { return };
        e.stop_at = None;
        e.stop_url = None;
        emit(&app, &id, e);
    }
    let Some(v) = tunnel_view(&app, &id) else { return };
    let same_run = v.runtime.public_url.as_deref() == Some(url.as_str());
    if !same_run || !matches!(v.runtime.status, Status::Starting | Status::Running) {
        return;
    }
    let label = minutes_label(minutes);
    log(&app, &id, format!("[cloudfairy] The {label} limit is up."));
    let state = app.state::<AppState>();
    state.tunnels.stop(&id);
    {
        let lc = lifecycles(&app);
        let mut map = lc.lock();
        if let Some(e) = map.get_mut(&id) {
            e.notice = Some(format!("Stopped after {label}, as set in Edit."));
            emit(&app, &id, e);
        }
    }
    let settings = state.settings.get();
    if settings.notify && !background::window_focused(&app) {
        background::notify(&app, &format!("{} stopped", v.config.name), &format!("It ran for {label}, the limit set in Edit."));
    }
}

/// Cancels a pending restart and the stop timer. Called when the user stops the tunnel.
pub fn cancel(app: &AppHandle, id: &str) {
    let lc = lifecycles(app);
    let mut map = lc.lock();
    let Some(e) = map.get_mut(id) else { return };
    e.restart_token = lc.token();
    e.stop_token = lc.token();
    e.attempts = 0;
    e.carried_stop_at = None;
    e.auto_restarting = false;
    e.notice = None;
    if e.restart_at.take().is_some() {
        log_later(app, id, "[cloudfairy] Restart canceled.");
    }
    e.stop_at = None;
    e.stop_url = None;
    emit(app, id, e);
}

/// Logs after the lifecycle lock is released.
fn log_later(app: &AppHandle, id: &str, line: &str) {
    let (app, id, line) = (app.clone(), id.to_string(), line.to_string());
    tauri::async_runtime::spawn(async move { log(&app, &id, line) });
}

/// True when the tunnel's last trip to Live came from an automatic restart.
pub fn restarted(app: &AppHandle, id: &str) -> bool {
    lifecycles(app).lock().get(id).is_some_and(|e| e.restarted)
}

/// Seconds until a pending restart.
pub fn restart_in(app: &AppHandle, id: &str) -> Option<u64> {
    let at = lifecycles(app).lock().get(id)?.restart_at?;
    Some(at.saturating_sub(now_ms()).div_ceil(1000))
}

#[tauri::command]
pub fn set_tunnel_lifecycle(
    app: AppHandle,
    state: State<AppState>,
    id: String,
    lifecycle: LifecycleConfig,
) -> Result<TunnelView, String> {
    if state.tunnels.is_active(&id) {
        return Err("Stop the tunnel before editing it.".into());
    }
    let lifecycle = lifecycle.normalized()?;
    let restart = lifecycle.restart_on_failure;
    let cfg = state.settings.update(|s| {
        s.tunnels.iter_mut().find(|t| t.id == id).map(|t| {
            t.lifecycle = lifecycle;
            t.clone()
        })
    })?;
    if !restart {
        cancel(&app, &id);
    }
    cfg.map(|c| state.tunnels.view(c))
        .ok_or_else(|| "Tunnel not found.".into())
}

/// Pending restarts, stop times and link errors, for the window when it loads.
#[tauri::command]
pub fn lifecycle_states(app: AppHandle) -> Vec<LifecycleView> {
    lifecycles(&app).lock().iter().map(|(id, e)| view(id, e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_and_caps() {
        let secs: Vec<u64> = (0..8).map(|n| retry_delay(n, false).as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300]);
        assert_eq!(retry_delay(u32::MAX, false).as_secs(), 300);
        assert_eq!(retry_delay(0, true).as_secs(), 60, "a rate limit waits at least a minute");
        assert_eq!(retry_delay(5, true).as_secs(), 160);
    }

    #[test]
    fn spots_rate_limits() {
        assert!(is_rate_limited("Error unmarshaling QuickTunnel response: 429 Too Many Requests"));
        assert!(is_rate_limited("too many requests"));
        assert!(!is_rate_limited("cloudflared stopped unexpectedly (exit code: 1)."));
    }

    #[test]
    fn labels_minutes() {
        assert_eq!(minutes_label(15), "15 min");
        assert_eq!(minutes_label(60), "1 h");
        assert_eq!(minutes_label(480), "8 h");
    }

    #[test]
    fn checks_options() {
        let ok = LifecycleConfig {
            restart_on_failure: true,
            stop_after_minutes: 30,
            url_file: if cfg!(windows) { " C:\\app\\.env " } else { " /app/.env " }.into(),
            url_file_var: " PUBLIC_URL ".into(),
            url_command: " echo {url} ".into(),
        };
        let n = ok.clone().normalized().unwrap();
        assert_eq!((n.url_file_var.as_str(), n.url_command.as_str()), ("PUBLIC_URL", "echo {url}"));
        assert!(!n.url_file.starts_with(' '));

        let bad = |f: fn(&mut LifecycleConfig)| {
            let mut c = ok.clone();
            f(&mut c);
            c.normalized().is_err()
        };
        assert!(bad(|c| c.stop_after_minutes = 7));
        assert!(bad(|c| c.url_file = ".env".into()), "relative paths are rejected");
        assert!(bad(|c| c.url_file_var = "PUBLIC-URL".into()));
        assert!(bad(|c| c.url_file = String::new()), "a variable needs a file");
        assert!(bad(|c| c.url_command = "echo a\necho b".into()));
        assert_eq!(LifecycleConfig::default().normalized().unwrap(), LifecycleConfig::default());
    }
}
