//! Updates Cloudfairy itself (cloudflared updates live in `cloudflared.rs`).
//!
//! The updater plugin reads `latest.json` from the newest GitHub release, which CI publishes
//! alongside installers signed with the key in `tauri.conf.json` > `plugins.updater.pubkey`.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const EVENT_PROGRESS: &str = "app-update-progress";

/// The update found by the last check, kept so installing doesn't have to check again.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

impl PendingUpdate {
    fn lock(&self) -> MutexGuard<'_, Option<Update>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdate {
    version: String,
    current_version: String,
    notes: Option<String>,
}

#[derive(Clone, Serialize)]
struct Progress {
    received: u64,
    total: Option<u64>,
}

/// Returns the newer version, if there is one.
pub async fn check(app: &AppHandle, pending: &PendingUpdate) -> Result<Option<AppUpdate>, String> {
    if !can_self_update() {
        return Ok(None);
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let view = update.as_ref().map(|u| AppUpdate {
        version: u.version.clone(),
        current_version: u.current_version.clone(),
        notes: u.body.clone(),
    });
    *pending.lock() = update;
    Ok(view)
}

/// Whether the in-app updater may replace this copy. On Linux the `.deb` and `.rpm` updates
/// run `dpkg -i` or `rpm -U`. That is wrong when another package manager installed the files,
/// such as pacman for the AUR package, which repackages the `.deb`. So the updater only runs
/// when dpkg or rpm owns the executable. Package managers update those copies instead.
#[cfg(target_os = "linux")]
fn can_self_update() -> bool {
    use std::process::{Command, Stdio};
    use tauri::utils::{config::BundleType, platform::bundle_type};

    let (tool, owns_file) = match bundle_type() {
        Some(BundleType::Deb) => ("dpkg", "-S"),
        Some(BundleType::Rpm) => ("rpm", "-qf"),
        _ => return true,
    };
    let Ok(exe) = std::env::current_exe() else { return false };
    Command::new(tool)
        .arg(owns_file)
        .arg(exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(target_os = "linux"))]
fn can_self_update() -> bool {
    true
}

/// Downloads and verifies the update found by [`check`], reporting progress to the UI.
pub async fn download(app: &AppHandle, pending: &PendingUpdate) -> Result<(Update, Vec<u8>), String> {
    let update = pending
        .lock()
        .clone()
        .ok_or("No update is waiting. Restart Cloudfairy to check again.")?;
    let mut received = 0u64;
    let mut last_emit = Instant::now();
    let bytes = update
        .download(
            |chunk, total| {
                received += chunk as u64;
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    last_emit = Instant::now();
                    let _ = app.emit(EVENT_PROGRESS, Progress { received, total });
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("Couldn't download the update: {e}"))?;
    Ok((update, bytes))
}
