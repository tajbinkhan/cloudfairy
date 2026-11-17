//! Finding, versioning and downloading the cloudflared binary.

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::process::Command;

use crate::tunnel::hide_console;

const RELEASE_BASE: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download/";
/// Redirects to `/releases/tag/<version>`; unlike the GitHub API it isn't rate limited.
const LATEST_URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest";
const LATEST_TTL: Duration = Duration::from_secs(6 * 60 * 60);
pub const EVENT_DOWNLOAD: &str = "download-progress";

static LATEST_CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Path set by the user in Settings.
    Custom,
    /// Downloaded by Cloudfairy into its data folder.
    Managed,
    /// Found on PATH or in a common install location.
    System,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub path: Option<String>,
    pub version: Option<String>,
    pub source: Option<Source>,
    pub problem: Option<String>,
    pub can_download: bool,
}

#[derive(Clone, Serialize)]
struct Progress {
    received: u64,
    total: Option<u64>,
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

fn managed_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("bin").join(exe_name()))
}

/// Picks the binary to run: Settings path, then our download, then the system install.
pub fn resolve(app: &AppHandle, custom: Option<&str>) -> Result<(PathBuf, Source), String> {
    if let Some(custom) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        let path = PathBuf::from(custom);
        return if path.is_file() {
            Ok((path, Source::Custom))
        } else {
            Err(format!("The cloudflared path in Settings doesn't exist: {custom}"))
        };
    }
    if let Some(path) = managed_path(app).filter(|p| p.is_file()) {
        return Ok((path, Source::Managed));
    }
    if let Ok(path) = which::which("cloudflared") {
        return Ok((path, Source::System));
    }
    // GUI apps (especially on macOS) often don't inherit the shell's PATH.
    fallback_locations()
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| (p, Source::System))
        .ok_or_else(|| "cloudflared isn't installed. Download it below or set its path.".into())
}

fn fallback_locations() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        paths.push(PathBuf::from("/opt/homebrew/bin/cloudflared"));
        paths.push(PathBuf::from("/usr/local/bin/cloudflared"));
    } else if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(base) = std::env::var(var) {
                paths.push(PathBuf::from(base).join("cloudflared").join("cloudflared.exe"));
            }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("Microsoft\\WinGet\\Links\\cloudflared.exe"));
        }
    } else {
        paths.push(PathBuf::from("/usr/local/bin/cloudflared"));
        paths.push(PathBuf::from("/usr/bin/cloudflared"));
        if let Ok(home) = std::env::var("HOME") {
            paths.push(PathBuf::from(home).join(".local/bin/cloudflared"));
        }
    }
    paths
}

pub async fn version(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version").kill_on_drop(true);
    hide_console(&mut cmd);
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "cloudflared version 2026.10.7 (built …)"
    text.split_whitespace()
        .skip_while(|w| *w != "version")
        .nth(1)
        .map(str::to_string)
}

pub async fn info(app: &AppHandle, custom: Option<&str>) -> Info {
    let can_download = asset().is_some();
    match resolve(app, custom) {
        Ok((path, source)) => {
            let version = version(&path).await;
            let problem = version
                .is_none()
                .then(|| "This file didn't respond like cloudflared. Check the path.".to_string());
            Info {
                path: Some(path.display().to_string()),
                version,
                source: Some(source),
                problem,
                can_download,
            }
        }
        Err(problem) => Info {
            path: None,
            version: None,
            source: None,
            problem: Some(problem),
            can_download,
        },
    }
}

fn http_client(redirects: reqwest::redirect::Policy) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("cloudfairy/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .redirect(redirects)
        .build()
        .map_err(|e| e.to_string())
}

/// Version of the newest official release, cached for a few hours.
pub async fn latest_version() -> Result<String, String> {
    if let Some((at, v)) = LATEST_CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < LATEST_TTL {
            return Ok(v.clone());
        }
    }
    let response = http_client(reqwest::redirect::Policy::none())?
        .head(LATEST_URL)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Couldn't check for updates: {e}"))?;
    let version = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .and_then(|l| l.rsplit_once("/tag/"))
        .map(|(_, tag)| tag.trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty())
        .ok_or("Couldn't read the latest cloudflared version.")?;
    *LATEST_CACHE.lock().unwrap() = Some((Instant::now(), version.clone()));
    Ok(version)
}

/// Release asset for this OS/CPU, and whether it's a .tgz archive.
fn asset() -> Option<(&'static str, bool)> {
    use std::env::consts::{ARCH, OS};
    Some(match (OS, ARCH) {
        ("linux", "x86_64") => ("cloudflared-linux-amd64", false),
        ("linux", "aarch64") => ("cloudflared-linux-arm64", false),
        ("linux", "arm") => ("cloudflared-linux-arm", false),
        ("linux", "x86") => ("cloudflared-linux-386", false),
        // Windows on ARM runs the x64 build through emulation.
        ("windows", "x86_64" | "aarch64") => ("cloudflared-windows-amd64.exe", false),
        ("windows", "x86") => ("cloudflared-windows-386.exe", false),
        ("macos", "x86_64") => ("cloudflared-darwin-amd64.tgz", true),
        ("macos", "aarch64") => ("cloudflared-darwin-arm64.tgz", true),
        _ => return None,
    })
}

/// Downloads the latest official release from GitHub into the app's data folder.
pub async fn download(app: &AppHandle) -> Result<PathBuf, String> {
    let (asset, is_tgz) =
        asset().ok_or("Automatic download isn't available for this platform. Install cloudflared yourself.")?;
    let dest = managed_path(app).ok_or("Couldn't find the app data folder.")?;
    let dir = dest.parent().ok_or("Invalid app data folder.")?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;

    let response = http_client(reqwest::redirect::Policy::default())?
        .get(format!("{RELEASE_BASE}{asset}"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Download failed: {e}"))?;

    let total = response.content_length();
    let mut data = Vec::with_capacity(total.unwrap_or(0) as usize);
    let mut stream = response.bytes_stream();
    let mut last_emit = Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Download interrupted: {e}"))?;
        data.extend_from_slice(&chunk);
        if last_emit.elapsed() >= Duration::from_millis(100) {
            last_emit = Instant::now();
            let _ = app.emit(EVENT_DOWNLOAD, Progress { received: data.len() as u64, total });
        }
    }
    let _ = app.emit(EVENT_DOWNLOAD, Progress { received: data.len() as u64, total });

    let binary = if is_tgz { extract_from_tgz(&data)? } else { data };
    if binary.len() < 1_000_000 {
        return Err("The downloaded file is too small to be cloudflared. Please try again.".into());
    }

    let tmp = dir.join(format!("{}.download", exe_name()));
    tokio::fs::write(&tmp, &binary)
        .await
        .map_err(|e| format!("Couldn't save cloudflared: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|e| format!("Couldn't make cloudflared executable: {e}"))?;
    }
    if dest.exists() {
        tokio::fs::remove_file(&dest).await.map_err(|e| {
            format!("Couldn't replace the existing cloudflared (stop running tunnels first): {e}")
        })?;
    }
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| format!("Couldn't install cloudflared: {e}"))?;
    Ok(dest)
}

fn extract_from_tgz(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data));
    let entries = archive.entries().map_err(|e| format!("Bad archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Bad archive: {e}"))?;
        let is_binary = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n == "cloudflared"))
            .unwrap_or(false);
        if is_binary {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| format!("Bad archive: {e}"))?;
            return Ok(buf);
        }
    }
    Err("The archive didn't contain cloudflared.".into())
}
