//! Named tunnels on the user's own domain, an optional mode next to quick tunnels.
//!
//! Signing in runs `cloudflared tunnel login`, which saves an origin certificate (`cert.pem`).
//! The first start of a named tunnel runs `cloudflared tunnel create` and
//! `cloudflared tunnel route dns`; every start then runs `cloudflared tunnel run` by UUID.
//! Every command gets Cloudfairy's own `--config` file, so a `~/.cloudflared/config.yml`
//! written for other tunnels doesn't change these ones.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::Notify,
};

use crate::{cloudflared, tunnel::hide_console, tunnel::TunnelView, AppState};

pub const EVENT_LOGIN: &str = "cloudflare-login";
const TUNNEL_PREFIX: &str = "cloudfairy-";
/// `cloudflared tunnel login` waits for the browser; give up after this long.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Limit for `create`, `route dns` and `delete`, which each make one API call.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
const CONFIG_FILE: &str = "cloudflared-named.yml";
const CONFIG_TEXT: &str = "# Written by Cloudfairy. Named tunnels started by Cloudfairy read this file\n\
                           # instead of ~/.cloudflared/config.yml.\n\
                           no-autoupdate: true\n";
const CREDENTIALS_DIR: &str = "named-tunnels";

type CmdResult<T> = Result<T, String>;

/// A tunnel on the user's own domain. Stored in `TunnelConfig::named`; `None` there means a
/// quick tunnel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NamedTunnel {
    /// Public hostname, like `app.example.com`.
    pub hostname: String,
    /// UUID of the Cloudflare tunnel, once Cloudfairy has created it.
    pub tunnel_id: Option<String>,
    /// Name it was created with, like `cloudfairy-app-example-com`.
    pub tunnel_name: Option<String>,
    /// The tunnel's credentials file, written by `cloudflared tunnel create`.
    pub credentials_file: Option<String>,
    /// The hostname whose DNS record points at the tunnel.
    pub routed_hostname: Option<String>,
}

// ---------------------------------------------------------------- hostnames and names

/// Accepts `app.example.com`, also with `https://`, a trailing slash or a trailing dot.
pub fn normalize_hostname(input: &str) -> Result<String, String> {
    let mut s = input.trim().to_ascii_lowercase();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
        }
    }
    let s = s.trim_end_matches('/').trim_end_matches('.');
    if s.is_empty() {
        return Err("Enter a hostname on your domain, like app.example.com.".into());
    }
    if !s.is_ascii() {
        return Err("Use the ASCII form of the hostname (the one that starts with xn--).".into());
    }
    if s.contains(['/', ':', '?', '#', '@']) {
        return Err("Enter only the hostname, like app.example.com, without a path or port.".into());
    }
    let labels: Vec<&str> = s.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let tld_ok = labels.last().is_some_and(|t| t.chars().any(|c| c.is_ascii_alphabetic()));
    if labels.len() < 2 || s.len() > 253 || !labels.iter().all(label_ok) || !tld_ok {
        return Err(format!("\"{s}\" isn't a valid hostname. Use something like app.example.com."));
    }
    if ["trycloudflare.com", "cfargotunnel.com"]
        .iter()
        .any(|d| s == *d || s.ends_with(&format!(".{d}")))
    {
        return Err("Use a hostname on your own domain. For a trycloudflare.com link, choose a random link instead.".into());
    }
    Ok(s.to_string())
}

/// Name for the Cloudflare tunnel: `app.example.com` becomes `cloudfairy-app-example-com`.
pub fn tunnel_name(hostname: &str) -> String {
    format!("{TUNNEL_PREFIX}{}", hostname.replace('.', "-"))
}

pub fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------- origin certificate

/// The user's home folder as cloudflared sees it: `HOME` first, then `USERPROFILE`.
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where cloudflared looks for `cert.pem`, in its order. `tunnel login` writes the first one.
pub fn cert_candidates(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join(".cloudflared"),
        home.join(".cloudflare-warp"),
        home.join("cloudflare-warp"),
    ];
    if cfg!(unix) {
        dirs.push(PathBuf::from("/etc/cloudflared"));
        dirs.push(PathBuf::from("/usr/local/etc/cloudflared"));
    }
    dirs.into_iter().map(|d| d.join("cert.pem")).collect()
}

/// The origin certificate to use: the path saved after signing in, else the first one
/// cloudflared would find.
pub fn find_cert(saved: Option<&str>) -> Option<PathBuf> {
    let saved = saved.map(PathBuf::from).filter(|p| p.is_file());
    saved.or_else(|| cert_candidates(&home_dir()?).into_iter().find(|p| p.is_file()))
}

fn default_cert() -> Option<PathBuf> {
    home_dir().and_then(|h| cert_candidates(&h).into_iter().next())
}

// ---------------------------------------------------------------- parsing cloudflared output

/// The sign-in link from `cloudflared tunnel login` output.
pub fn parse_login_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url = line[start..].split_whitespace().next()?;
    let url = url.trim_end_matches(['.', ',', ')', '"', '\'']);
    url.contains("cloudflare").then(|| url.to_string())
}

/// The certificate path `cloudflared tunnel login` reports, when it finished or when a
/// certificate was already there.
pub fn parse_login_cert_path(output: &str) -> Option<String> {
    let mut lines = output.lines().map(str::trim);
    while let Some(line) = lines.next() {
        if let Some(rest) = line.split_once("existing certificate at ").map(|(_, r)| r) {
            let path = rest.split(" which ").next().unwrap_or(rest).trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
        if line.ends_with("saved to:") {
            if let Some(path) = lines.by_ref().find(|l| !l.is_empty()) {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// The UUID from `cloudflared tunnel create` output:
/// "Created tunnel cloudfairy-app with id 6ff42ae2-765d-4adf-8112-31c55c1551ef".
pub fn parse_created_id(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    lower.match_indices("with id ").find_map(|(i, m)| {
        let token = lower[i + m.len()..].split_whitespace().next()?;
        let token = token.trim_end_matches(['.', ',']);
        is_uuid(token).then(|| token.to_string())
    })
}

/// `TunnelID` from a credentials file written by `cloudflared tunnel create`.
pub fn tunnel_id_from_credentials(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let id = value.get("TunnelID")?.as_str()?.to_ascii_lowercase();
    is_uuid(&id).then_some(id)
}

/// The DNS record `cloudflared tunnel route dns` reports, from
/// "Added CNAME app.example.com which will route to this tunnel" or
/// "app.example.com is already configured to route to your tunnel".
pub fn routed_record(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        if let Some((_, rest)) = line.split_once("Added CNAME ") {
            return rest.split_whitespace().next().map(str::to_string);
        }
        for marker in [" is already configured to route to your tunnel", " updated to route to your tunnel"] {
            if let Some((before, _)) = line.split_once(marker) {
                return before.split_whitespace().last().map(str::to_string);
            }
        }
        None
    })
}

/// What went wrong, for the failures users can fix themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// No origin certificate: the user hasn't signed in.
    NotSignedIn,
    /// Cloudflare rejected the certificate.
    SignInRejected,
    /// The hostname's domain isn't in the account the user signed in to.
    ZoneNotInAccount,
    /// Cloudflare created the record under another domain, e.g. `app.other.com.example.com`.
    WrongZone { record: String },
    /// A DNS record with that name already exists.
    DnsRecordExists,
    /// A tunnel with Cloudfairy's name already exists in the account.
    NameTaken,
    /// The credentials file is missing or unreadable.
    CredentialsMissing,
    /// The tunnel was created, but cloudflared couldn't save its credentials.
    CredentialsNotWritten,
    /// The tunnel no longer exists in the account.
    TunnelGone,
}

/// Recognizes the common failures in cloudflared's output.
pub fn classify(output: &str) -> Option<Problem> {
    let o = output.to_ascii_lowercase();
    let has = |s: &str| o.contains(s);
    Some(if has("couldn't write tunnel credentials") {
        Problem::CredentialsNotWritten
    } else if has("cannot determine default origin certificate path")
        || has("error locating origin cert")
        || has("cannot find a valid certificate")
        || has("client didn't specify origincert path")
    {
        Problem::NotSignedIn
    } else if has("tunnel credentials file") || has("couldn't read tunnel credentials") || has("contained invalid json") {
        Problem::CredentialsMissing
    } else if has("tunnel with name already exists") {
        Problem::NameTaken
    } else if has("record with that host already exists") || has("record already exists") {
        Problem::DnsRecordExists
    } else if has("zone")
        && (has("not found") || has("could not find") || has("not part of") || has("does not belong") || has("no zone"))
    {
        Problem::ZoneNotInAccount
    } else if has("tunnel not found") || has("has already been deleted") || has("invalid tunnel secret") {
        Problem::TunnelGone
    } else if has("authentication error") || has("unauthorized") || has("invalid origin certificate") {
        Problem::SignInRejected
    } else {
        return None;
    })
}

/// Details for [`Problem::message`].
pub struct Context<'a> {
    pub hostname: &'a str,
    pub tunnel_name: &'a str,
    pub credentials_file: &'a str,
    pub cert: Option<&'a str>,
}

impl Problem {
    pub fn message(&self, cx: &Context) -> String {
        let host = cx.hostname;
        let cert = cx.cert.unwrap_or("cert.pem");
        match self {
            Problem::NotSignedIn => {
                "Sign in to Cloudflare first. Open Settings and click Sign in under Cloudflare account.".into()
            }
            Problem::SignInRejected => format!(
                "Cloudflare didn't accept your sign-in. Delete {cert} and sign in again in Settings."
            ),
            Problem::ZoneNotInAccount => format!(
                "The domain of {host} isn't in your Cloudflare account. Add the domain to Cloudflare, \
                 or sign in again and pick the domain {host} belongs to."
            ),
            Problem::WrongZone { record } => format!(
                "{host} isn't in the domain you picked when you signed in, so Cloudflare added the DNS \
                 record as {record}. Delete that record in the Cloudflare dashboard. To use {host}, \
                 delete {cert}, sign in again and pick the domain it belongs to."
            ),
            Problem::DnsRecordExists => format!(
                "{host} already has a DNS record. Delete it in the Cloudflare dashboard \
                 (DNS, Records), or use another hostname."
            ),
            Problem::NameTaken => format!(
                "Your Cloudflare account already has a tunnel named {}, and its credentials file \
                 isn't on this computer. Delete that tunnel in the Cloudflare dashboard \
                 (Networks, Tunnels), then start again.",
                cx.tunnel_name
            ),
            Problem::CredentialsMissing => format!(
                "This tunnel's credentials file is missing or damaged: {}. Remove the tunnel, \
                 delete it from your Cloudflare account when asked, and add it again.",
                cx.credentials_file
            ),
            Problem::CredentialsNotWritten => format!(
                "Cloudflare created the tunnel {}, but cloudflared couldn't save its credentials to {}. \
                 Delete the tunnel in the Cloudflare dashboard (Networks, Tunnels), then start again.",
                cx.tunnel_name, cx.credentials_file
            ),
            Problem::TunnelGone => "This tunnel no longer exists in your Cloudflare account. Remove it, \
                 delete it from your Cloudflare account when asked, and add it again."
                .into(),
        }
    }
}

/// A readable error for a failed `create`, `route dns` or `delete`.
pub fn explain(output: &str, cx: &Context) -> String {
    match classify(output) {
        Some(problem) => problem.message(cx),
        None => last_error_line(output).unwrap_or_else(|| "cloudflared failed without saying why.".into()),
    }
}

/// The line that best explains a failure: the last one that looks like an error, without
/// cloudflared's timestamp and level.
fn last_error_line(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let looks_bad = |l: &&&str| {
        let l = l.to_ascii_lowercase();
        l.contains(" err ") || l.contains("error") || l.contains("failed")
    };
    let line = lines.iter().rev().find(looks_bad).or(lines.last())?;
    let msg = line.split_once(" ERR ").map_or(*line, |(_, m)| m).trim();
    let mut short: String = msg.chars().take(300).collect();
    if short.len() < msg.len() {
        short.push('…');
    }
    Some(short)
}

/// Turns an `ERR` line from a running named tunnel into advice, when it's a known problem.
pub fn friendly_error(msg: &str) -> String {
    match classify(msg) {
        Some(p @ (Problem::TunnelGone | Problem::CredentialsMissing)) => p.message(&Context {
            hostname: "",
            tunnel_name: "",
            credentials_file: "the credentials file",
            cert: None,
        }),
        _ => msg.to_string(),
    }
}

// ---------------------------------------------------------------- arguments

fn base_args(config: &Path, cert: &Path) -> Vec<String> {
    vec![
        "tunnel".into(),
        "--no-autoupdate".into(),
        "--config".into(),
        config.display().to_string(),
        "--origincert".into(),
        cert.display().to_string(),
    ]
}

pub fn create_args(config: &Path, cert: &Path, credentials: &Path, name: &str) -> Vec<String> {
    let mut args = base_args(config, cert);
    args.extend(["create".into(), "--credentials-file".into(), credentials.display().to_string(), name.into()]);
    args
}

pub fn route_args(config: &Path, cert: &Path, tunnel_id: &str, hostname: &str) -> Vec<String> {
    let mut args = base_args(config, cert);
    args.extend(["route".into(), "dns".into(), tunnel_id.into(), hostname.into()]);
    args
}

/// `-f` because Cloudfairy has just stopped its own connector, and Cloudflare can take a
/// moment to notice the connections are gone.
pub fn delete_args(config: &Path, cert: &Path, tunnel_id: &str) -> Vec<String> {
    let mut args = base_args(config, cert);
    args.extend(["delete".into(), "-f".into(), tunnel_id.into()]);
    args
}

/// What `cloudflared tunnel run` needs, found or created by [`provision`].
#[derive(Clone, Debug)]
pub struct Prepared {
    pub tunnel_id: String,
    pub credentials_file: PathBuf,
    pub config_file: PathBuf,
    pub cert: Option<PathBuf>,
}

/// Turns the quick-tunnel arguments into `cloudflared tunnel … run … <uuid>`.
///
/// Options of `tunnel` itself (metrics, log level, protocol) stay before `run`. The origin
/// options (`--url`, `--http-host-header`, `--no-tls-verify`) move after it, because `run`
/// defines its own copies and cloudflared reads those.
pub fn run_args(quick: Vec<String>, p: &Prepared) -> Vec<String> {
    let split = quick
        .iter()
        .position(|a| matches!(a.as_str(), "--url" | "--http-host-header" | "--no-tls-verify"))
        .unwrap_or(quick.len());
    let mut head = quick;
    let tail = head.split_off(split);
    head.push("--config".into());
    head.push(p.config_file.display().to_string());
    if let Some(cert) = &p.cert {
        head.push("--origincert".into());
        head.push(cert.display().to_string());
    }
    head.push("run".into());
    head.push("--credentials-file".into());
    head.push(p.credentials_file.display().to_string());
    head.extend(tail);
    head.push(p.tunnel_id.clone());
    head
}

// ---------------------------------------------------------------- running cloudflared

/// Runs a short cloudflared command. Returns whether it succeeded and its combined output.
async fn run_cli(bin: &Path, args: &[String]) -> Result<(bool, String), String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut cmd);
    let out = tokio::time::timeout(COMMAND_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "cloudflared didn't answer within 90 seconds.".to_string())?
        .map_err(|e| format!("Couldn't run cloudflared at {}: {e}", bin.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    Ok((out.status.success(), text))
}

fn command_line(bin: &Path, args: &[String]) -> String {
    format!("{} {}", bin.display(), args.join(" "))
}

/// Writes Cloudfairy's cloudflared config file. An empty file makes cloudflared log an
/// error, so it holds one harmless setting.
fn config_file<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let path = dir.join(CONFIG_FILE);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(CONFIG_TEXT) {
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
        std::fs::write(&path, CONFIG_TEXT).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    }
    Ok(path)
}

fn credentials_dir<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join(CREDENTIALS_DIR))
}

/// Saves what provisioning learned, unless the tunnel was removed or edited meanwhile.
fn save<R: Runtime>(app: &AppHandle<R>, id: &str, hostname: &str, f: impl FnOnce(&mut NamedTunnel)) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let _ = state.settings.update(|s| {
        let named = s.tunnels.iter_mut().find(|t| t.id == id).and_then(|t| t.named.as_mut());
        if let Some(named) = named.filter(|n| n.hostname == hostname) {
            f(named);
        }
    });
}

fn saved_cert<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    app.try_state::<AppState>().and_then(|s| s.settings.get().origin_cert)
}

/// Creates the Cloudflare tunnel and its DNS record when that hasn't happened yet, and finds
/// everything `cloudflared tunnel run` needs. `log` gets progress and cloudflared's output.
/// Returns `None` when `stopped` says Stop was pressed.
pub async fn provision<R: Runtime>(
    app: &AppHandle<R>,
    bin: &Path,
    id: &str,
    named: &NamedTunnel,
    log: &(dyn Fn(String) + Send + Sync),
    stopped: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Option<Prepared>, String> {
    let hostname = named.hostname.as_str();
    let name = named.tunnel_name.clone().unwrap_or_else(|| tunnel_name(hostname));
    let creds_dir = credentials_dir(app)?;
    let credentials_file = named
        .credentials_file
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| creds_dir.join(format!("{name}.json")));
    let config_file = config_file(app)?;
    let cert = find_cert(saved_cert(app).as_deref());
    let creds_text = credentials_file.display().to_string();
    let cert_text = cert.as_ref().map(|c| c.display().to_string());
    let cx = Context {
        hostname,
        tunnel_name: &name,
        credentials_file: &creds_text,
        cert: cert_text.as_deref(),
    };
    let need_cert = || cert.clone().ok_or_else(|| Problem::NotSignedIn.message(&cx));
    let run = |args: Vec<String>| {
        log(format!("$ {}", command_line(bin, &args)));
        async move {
            let (ok, output) = run_cli(bin, &args).await?;
            for line in output.lines().filter(|l| !l.trim().is_empty()) {
                log(line.to_string());
            }
            Ok::<_, String>((ok, output))
        }
    };

    let tunnel_id = match &named.tunnel_id {
        Some(tunnel_id) => tunnel_id.clone(),
        None => {
            // Kept from an earlier tunnel for this hostname that was removed from Cloudfairy
            // but not from the Cloudflare account.
            let reused = std::fs::read_to_string(&credentials_file)
                .ok()
                .and_then(|t| tunnel_id_from_credentials(&t));
            let tunnel_id = match reused {
                Some(tunnel_id) => {
                    log(format!("[cloudfairy] Using tunnel {name} ({tunnel_id}) created earlier."));
                    tunnel_id
                }
                None => {
                    let cert = need_cert()?;
                    std::fs::create_dir_all(&creds_dir)
                        .map_err(|e| format!("Couldn't create {}: {e}", creds_dir.display()))?;
                    log(format!("[cloudfairy] Creating tunnel {name} in your Cloudflare account…"));
                    let (ok, output) = run(create_args(&config_file, &cert, &credentials_file, &name)).await?;
                    if !ok {
                        return Err(explain(&output, &cx));
                    }
                    if !credentials_file.is_file() {
                        return Err(Problem::CredentialsNotWritten.message(&cx));
                    }
                    let created = parse_created_id(&output).or_else(|| {
                        let text = std::fs::read_to_string(&credentials_file).ok()?;
                        tunnel_id_from_credentials(&text)
                    });
                    created.ok_or("cloudflared created the tunnel but didn't report its ID. See Logs.")?
                }
            };
            save(app, id, hostname, |n| {
                n.tunnel_id = Some(tunnel_id.clone());
                n.tunnel_name = Some(name.clone());
                n.credentials_file = Some(creds_text.clone());
            });
            tunnel_id
        }
    };
    if stopped() {
        return Ok(None);
    }

    if named.routed_hostname.as_deref() != Some(hostname) {
        let cert = need_cert()?;
        log(format!("[cloudfairy] Pointing {hostname} at the tunnel…"));
        let (ok, output) = run(route_args(&config_file, &cert, &tunnel_id, hostname)).await?;
        if !ok {
            return Err(explain(&output, &cx));
        }
        if let Some(record) = routed_record(&output) {
            let record = record.trim_end_matches('.').to_ascii_lowercase();
            if record != hostname {
                return Err(Problem::WrongZone { record }.message(&cx));
            }
        }
        save(app, id, hostname, |n| n.routed_hostname = Some(hostname.to_string()));
        if stopped() {
            return Ok(None);
        }
    }

    if !credentials_file.is_file() {
        return Err(Problem::CredentialsMissing.message(&cx));
    }
    Ok(Some(Prepared {
        tunnel_id,
        credentials_file,
        config_file,
        cert,
    }))
}

// ---------------------------------------------------------------- signing in

struct Login {
    cancel: Arc<Notify>,
    url: Option<String>,
}

static LOGIN: Mutex<Option<Login>> = Mutex::new(None);

fn login_state() -> MutexGuard<'static, Option<Login>> {
    LOGIN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clears the running sign-in however `cloudflare_login` returns.
struct LoginGuard;

impl Drop for LoginGuard {
    fn drop(&mut self) {
        *login_state() = None;
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    signed_in: bool,
    /// The certificate in use, or where signing in will save it.
    cert_path: Option<String>,
    signing_in: bool,
    login_url: Option<String>,
}

fn account_view(saved: Option<&str>) -> AccountView {
    let cert = find_cert(saved);
    let login = login_state();
    AccountView {
        signed_in: cert.is_some(),
        cert_path: cert.or_else(default_cert).map(|p| p.display().to_string()),
        signing_in: login.is_some(),
        login_url: login.as_ref().and_then(|l| l.url.clone()),
    }
}

#[derive(Clone, Serialize)]
struct LoginEvent {
    url: String,
}

#[tauri::command]
pub fn cloudflare_account(state: State<AppState>) -> AccountView {
    account_view(state.settings.get().origin_cert.as_deref())
}

/// Runs `cloudflared tunnel login` and waits for it. cloudflared opens the browser itself;
/// the link is also sent to the window in a `cloudflare-login` event.
#[tauri::command]
pub async fn cloudflare_login(app: AppHandle, state: State<'_, AppState>) -> CmdResult<AccountView> {
    let settings = state.settings.get();
    if find_cert(settings.origin_cert.as_deref()).is_some() {
        return Ok(account_view(settings.origin_cert.as_deref()));
    }
    let (bin, _) = cloudflared::resolve(&app, settings.cloudflared_path.as_deref())?;
    let cancel = Arc::new(Notify::new());
    {
        let mut login = login_state();
        if login.is_some() {
            return Err("Sign-in is already running. Finish it in your browser.".into());
        }
        *login = Some(Login { cancel: cancel.clone(), url: None });
    }
    let _guard = LoginGuard;

    let mut cmd = Command::new(&bin);
    cmd.args(["tunnel", "login"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    hide_console(&mut cmd);
    // Not spawned through child_guard: on Windows its job object would also hold a browser
    // that cloudflared starts, and close it when Cloudfairy quits.
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Couldn't launch cloudflared at {}: {e}", bin.display()))?;
    let output = Arc::new(Mutex::new(String::new()));
    let readers: Vec<_> = [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| tauri::async_runtime::spawn(read_login(app.clone(), stream, output.clone())))
    .collect();

    let outcome = tokio::select! {
        status = child.wait() => status.map(|s| s.success()).map_err(|e| e.to_string()),
        _ = cancel.notified() => Err("Sign-in canceled.".to_string()),
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err("Sign-in timed out. Try again.".to_string()),
    };
    let _ = child.start_kill();
    for reader in readers {
        let _ = tokio::time::timeout(Duration::from_secs(2), reader).await;
    }
    let text = output.lock().unwrap_or_else(|e| e.into_inner()).clone();

    if let Some(path) = parse_login_cert_path(&text).filter(|p| Path::new(p).is_file()) {
        state.settings.update(|s| s.origin_cert = Some(path))?;
    }
    let settings = state.settings.get();
    if find_cert(settings.origin_cert.as_deref()).is_some() {
        drop(_guard);
        return Ok(account_view(settings.origin_cert.as_deref()));
    }
    match outcome {
        Err(e) => Err(e),
        Ok(_) => Err(match last_error_line(&text) {
            Some(line) => format!("Sign-in didn't finish: {line}"),
            None => "Sign-in didn't finish. Try again.".into(),
        }),
    }
}

async fn read_login<R: Runtime>(app: AppHandle<R>, stream: Box<dyn AsyncRead + Unpin + Send>, output: Arc<Mutex<String>>) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = parse_login_url(&line) {
            let mut login = login_state();
            if let Some(l) = login.as_mut().filter(|l| l.url.is_none()) {
                l.url = Some(url.clone());
                let _ = app.emit(EVENT_LOGIN, LoginEvent { url });
            }
        }
        let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
        out.push_str(&line);
        out.push('\n');
    }
}

#[tauri::command]
pub fn cloudflare_login_cancel() {
    if let Some(login) = login_state().as_ref() {
        login.cancel.notify_one();
    }
}

// ---------------------------------------------------------------- tunnel commands

/// Like `add_tunnel`, for a tunnel on the user's own domain.
#[tauri::command]
pub fn add_named_tunnel(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    target: String,
    hostname: String,
) -> CmdResult<TunnelView> {
    let hostname = normalize_hostname(&hostname)?;
    let view = crate::add_tunnel(app, state.clone(), name, target)?;
    set_named(&state, &view.config.id, Some(hostname))
}

/// Switches a tunnel between a random link (`hostname` empty) and a hostname on the user's
/// domain.
#[tauri::command]
pub fn set_tunnel_hostname(state: State<AppState>, id: String, hostname: Option<String>) -> CmdResult<TunnelView> {
    if state.tunnels.is_active(&id) {
        return Err("Stop the tunnel before editing it.".into());
    }
    let hostname = match hostname.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(h) => Some(normalize_hostname(h)?),
    };
    set_named(&state, &id, hostname)
}

fn set_named(state: &AppState, id: &str, hostname: Option<String>) -> CmdResult<TunnelView> {
    let cfg = state.settings.update(|s| {
        let t = s.tunnels.iter_mut().find(|t| t.id == id)?;
        t.named = match (hostname, t.named.take()) {
            (None, _) => None,
            // Same Cloudflare tunnel, new hostname: the next start adds its DNS record.
            (Some(hostname), Some(old)) => Some(NamedTunnel { hostname, ..old }),
            (Some(hostname), None) => Some(NamedTunnel { hostname, ..Default::default() }),
        };
        Some(t.clone())
    })?;
    cfg.map(|c| state.tunnels.view(c)).ok_or_else(|| "Tunnel not found.".into())
}

/// Removes a tunnel from Cloudfairy and, if asked, deletes it from the Cloudflare account
/// with its credentials file. If deleting fails, the tunnel stays in Cloudfairy.
#[tauri::command]
pub async fn remove_named_tunnel(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    delete_from_account: bool,
) -> CmdResult<()> {
    let settings = state.settings.get();
    let target = settings
        .tunnels
        .iter()
        .find(|t| t.id == id)
        .and_then(|t| t.named.clone())
        .filter(|_| delete_from_account)
        .and_then(|n| Some((n.tunnel_id.clone()?, n)));
    if let Some((tunnel_id, named)) = target {
        state.tunnels.stop(&id);
        for _ in 0..40 {
            if !state.tunnels.is_active(&id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let creds = named.credentials_file.clone().unwrap_or_default();
        let name = named.tunnel_name.clone().unwrap_or_else(|| tunnel_name(&named.hostname));
        let cert = find_cert(settings.origin_cert.as_deref());
        let cert_text = cert.as_ref().map(|c| c.display().to_string());
        let cx = Context {
            hostname: &named.hostname,
            tunnel_name: &name,
            credentials_file: &creds,
            cert: cert_text.as_deref(),
        };
        let cert = cert.ok_or_else(|| Problem::NotSignedIn.message(&cx))?;
        let (bin, _) = cloudflared::resolve(&app, settings.cloudflared_path.as_deref())?;
        let (ok, output) = run_cli(&bin, &delete_args(&config_file(&app)?, &cert, &tunnel_id)).await?;
        if !ok && classify(&output) != Some(Problem::TunnelGone) {
            return Err(format!("Couldn't delete the tunnel from Cloudflare. {}", explain(&output, &cx)));
        }
        // Only a file Cloudfairy wrote itself.
        let creds = PathBuf::from(creds);
        if credentials_dir(&app).is_ok_and(|dir| creds.parent() == Some(dir.as_path())) {
            let _ = std::fs::remove_file(&creds);
        }
    }
    crate::remove_tunnel(app, state, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hostnames() {
        assert_eq!(normalize_hostname("app.example.com").unwrap(), "app.example.com");
        assert_eq!(normalize_hostname(" https://App.Example.com/ ").unwrap(), "app.example.com");
        assert_eq!(normalize_hostname("example.com.").unwrap(), "example.com");
        assert_eq!(normalize_hostname("a-b.c-d.example.co.uk").unwrap(), "a-b.c-d.example.co.uk");
        assert_eq!(normalize_hostname("xn--bcher-kva.example").unwrap(), "xn--bcher-kva.example");
        for bad in [
            "",
            "localhost",
            "app..example.com",
            "-app.example.com",
            "app-.example.com",
            "app_1.example.com",
            "app.example.com:8080",
            "app.example.com/path",
            "*.example.com",
            "bücher.example",
            "10.0.0.1",
            "x.trycloudflare.com",
            "trycloudflare.com",
            "abc.cfargotunnel.com",
        ] {
            assert!(normalize_hostname(bad).is_err(), "{bad:?} should be rejected");
        }
        let long = format!("{}.example.com", "a".repeat(64));
        assert!(normalize_hostname(&long).is_err());
    }

    #[test]
    fn names_tunnels_after_hostnames() {
        assert_eq!(tunnel_name("app.example.com"), "cloudfairy-app-example-com");
    }

    #[test]
    fn checks_uuids() {
        assert!(is_uuid("6ff42ae2-765d-4adf-8112-31c55c1551ef"));
        assert!(!is_uuid("6ff42ae2-765d-4adf-8112-31c55c1551e"));
        assert!(!is_uuid("6ff42ae2765d4adf811231c55c1551ef"));
        assert!(!is_uuid("zff42ae2-765d-4adf-8112-31c55c1551ef"));
    }

    #[test]
    fn parses_created_tunnel_id() {
        let out = "Tunnel credentials written to /home/u/.local/share/dev.cloudfairy.desktop/named-tunnels/cloudfairy-app-example-com.json. \
                   Keep this file secret. To revoke these credentials, delete the tunnel.\n\n\
                   Created tunnel cloudfairy-app-example-com with id 6ff42ae2-765d-4adf-8112-31c55c1551ef\n";
        assert_eq!(parse_created_id(out).as_deref(), Some("6ff42ae2-765d-4adf-8112-31c55c1551ef"));
        // cloudflared's message when it created the tunnel but couldn't save the credentials.
        let partial = "Your tunnel 'cloudfairy-x' was created with ID 6FF42AE2-765D-4ADF-8112-31C55C1551EF. \
                       However, cloudflared couldn't write tunnel credentials to C:\\x.json.";
        assert_eq!(parse_created_id(partial).as_deref(), Some("6ff42ae2-765d-4adf-8112-31c55c1551ef"));
        assert_eq!(parse_created_id("Created tunnel x with id not-a-uuid"), None);
        assert_eq!(parse_created_id("failed to create tunnel: something"), None);
    }

    #[test]
    fn reads_tunnel_id_from_credentials() {
        let json = r#"{"AccountTag":"abc","TunnelSecret":"c2VjcmV0","TunnelID":"6ff42ae2-765d-4adf-8112-31c55c1551ef","Endpoint":""}"#;
        assert_eq!(tunnel_id_from_credentials(json).as_deref(), Some("6ff42ae2-765d-4adf-8112-31c55c1551ef"));
        assert_eq!(tunnel_id_from_credentials(r#"{"TunnelID":"nope"}"#), None);
        assert_eq!(tunnel_id_from_credentials("not json"), None);
    }

    #[test]
    fn parses_login_output() {
        let line = "https://dash.cloudflare.com/argotunnel?aud=&callback=https%3A%2F%2Flogin.cloudflareaccess.org%2Fabc";
        assert_eq!(parse_login_url(line).as_deref(), Some(line));
        assert_eq!(
            parse_login_url("Please open the following URL and log in with your Cloudflare account: https://dash.cloudflare.com/argotunnel?x=1.").as_deref(),
            Some("https://dash.cloudflare.com/argotunnel?x=1")
        );
        assert_eq!(parse_login_url("Leave cloudflared running to download the cert automatically."), None);
        assert_eq!(parse_login_url("see https://example.com/docs"), None);

        let done = "You have successfully logged in.\n\
                    If you wish to copy your credentials to a server, they have been saved to:\n\
                    /home/u/.cloudflared/cert.pem\n";
        assert_eq!(parse_login_cert_path(done).as_deref(), Some("/home/u/.cloudflared/cert.pem"));
        let existing = "You have an existing certificate at C:\\Users\\u\\.cloudflared\\cert.pem which login would overwrite.\n\
                        If this is intentional, please move or delete that file then run this command again.";
        assert_eq!(parse_login_cert_path(existing).as_deref(), Some("C:\\Users\\u\\.cloudflared\\cert.pem"));
        assert_eq!(parse_login_cert_path("Failed to fetch resource"), None);
    }

    #[test]
    fn lists_cert_locations_in_cloudflared_order() {
        let home = Path::new("/home/u");
        let found = cert_candidates(home);
        assert_eq!(found[0], home.join(".cloudflared").join("cert.pem"));
        assert_eq!(found[1], home.join(".cloudflare-warp").join("cert.pem"));
    }

    #[test]
    fn reads_routed_record() {
        let added = "2026-11-24T10:00:00Z INF Added CNAME app.example.com which will route to this tunnel tunnelID=6ff42ae2-765d-4adf-8112-31c55c1551ef";
        assert_eq!(routed_record(added).as_deref(), Some("app.example.com"));
        let wrong_zone = "2026-11-24T10:00:00Z INF Added CNAME app.other.org.example.com which will route to this tunnel tunnelID=x";
        assert_eq!(routed_record(wrong_zone).as_deref(), Some("app.other.org.example.com"));
        let existing = "2026-11-24T10:00:00Z INF app.example.com is already configured to route to your tunnel tunnelID=x";
        assert_eq!(routed_record(existing).as_deref(), Some("app.example.com"));
        assert_eq!(routed_record("nothing here"), None);
    }

    #[test]
    fn classifies_common_failures() {
        // Captured from cloudflared 2026.5.2 on a machine without a certificate.
        let no_default_cert = "2026-11-24T12:20:31Z ERR Cannot determine default origin certificate path. No file cert.pem in \
            [~/.cloudflared ~/.cloudflare-warp ~/cloudflare-warp]. You need to specify the origin certificate path by \
            specifying the origincert option in the configuration file, or set TUNNEL_ORIGIN_CERT environment variable originCertPath=\n\
            failed to create tunnel: couldn't create client to talk to Cloudflare Tunnel backend: Error locating origin cert: \
            client didn't specify origincert path";
        assert_eq!(classify(no_default_cert), Some(Problem::NotSignedIn));
        let missing_cert = "If you don't have a certificate signed by Cloudflare, run the command:\n\n\tcloudflared login\n \
            originCertPath=C:/x/cert.pem\n\
            Error locating origin cert: cannot find a valid certificate at the path C:/x/cert.pem";
        assert_eq!(classify(missing_cert), Some(Problem::NotSignedIn));
        let missing_creds = "Tunnel credentials file 'C:/x/creds.json' doesn't exist or is not a file";
        assert_eq!(classify(missing_creds), Some(Problem::CredentialsMissing));
        let bad_json = "ERR The credentials file at /x.json contained invalid JSON. This is probably caused by passing the wrong filepath.";
        assert_eq!(classify(bad_json), Some(Problem::CredentialsMissing));

        let dns_exists = "Failed to add route: code: 1003, reason: Failed to create record app.example.com with err An A, AAAA, \
            or CNAME record with that host already exists. For more details, refer to \
            <https://developers.cloudflare.com/dns/manage-dns-records/troubleshooting/records-with-same-name/>.";
        assert_eq!(classify(dns_exists), Some(Problem::DnsRecordExists));
        assert_eq!(classify("Failed to add route: code: 1001, reason: zone not found"), Some(Problem::ZoneNotInAccount));
        assert_eq!(
            classify("Failed to add route: Could not find zone for app.other.org"),
            Some(Problem::ZoneNotInAccount)
        );
        assert_eq!(
            classify("failed to create tunnel: Create Tunnel API call failed: tunnel with name already exists"),
            Some(Problem::NameTaken)
        );
        let partial = "Your tunnel 'cloudfairy-x' was created with ID 6ff42ae2-765d-4adf-8112-31c55c1551ef. \
            However, cloudflared couldn't write tunnel credentials to /x.json.";
        assert_eq!(classify(partial), Some(Problem::CredentialsNotWritten));
        assert_eq!(
            classify(r#"ERR Register tunnel error from server side error="Unauthorized: Tunnel not found" connIndex=0"#),
            Some(Problem::TunnelGone)
        );
        assert_eq!(
            classify("Tunnel 6ff42ae2-765d-4adf-8112-31c55c1551ef has already been deleted"),
            Some(Problem::TunnelGone)
        );
        assert_eq!(classify("REST request failed: Authentication error (10000)"), Some(Problem::SignInRejected));
        assert_eq!(classify("INF Starting tunnel"), None);
    }

    #[test]
    fn explains_failures_with_details() {
        let cx = Context {
            hostname: "app.example.com",
            tunnel_name: "cloudfairy-app-example-com",
            credentials_file: "/data/named-tunnels/cloudfairy-app-example-com.json",
            cert: Some("/home/u/.cloudflared/cert.pem"),
        };
        let dns = explain("Failed to add route: code: 1003, reason: An A, AAAA, or CNAME record with that host already exists.", &cx);
        assert!(dns.starts_with("app.example.com already has a DNS record."), "{dns}");
        let gone = explain("Tunnel credentials file '/x' doesn't exist or is not a file", &cx);
        assert!(gone.contains("/data/named-tunnels/cloudfairy-app-example-com.json"), "{gone}");
        let wrong = Problem::WrongZone { record: "app.example.com.other.org".into() }.message(&cx);
        assert!(wrong.contains("app.example.com.other.org") && wrong.contains("/home/u/.cloudflared/cert.pem"), "{wrong}");
        // Unknown failures show cloudflared's own last error, without the timestamp.
        let other = explain("2026-11-24T10:00:00Z INF Starting\n2026-11-24T10:00:01Z ERR something odd happened\n", &cx);
        assert_eq!(other, "something odd happened");
        assert_eq!(explain("", &cx), "cloudflared failed without saying why.");
    }

    #[test]
    fn friendly_errors_only_replace_known_problems() {
        assert!(friendly_error("Unauthorized: Tunnel not found").starts_with("This tunnel no longer exists"));
        assert_eq!(friendly_error("no more connections active and exiting"), "no more connections active and exiting");
    }

    #[test]
    fn builds_setup_args() {
        let cfg = Path::new("/c/cloudflared-named.yml");
        let cert = Path::new("/h/.cloudflared/cert.pem");
        let creds = Path::new("/d/named-tunnels/cloudfairy-app-example-com.json");
        assert_eq!(
            create_args(cfg, cert, creds, "cloudfairy-app-example-com"),
            [
                "tunnel", "--no-autoupdate", "--config", "/c/cloudflared-named.yml", "--origincert",
                "/h/.cloudflared/cert.pem", "create", "--credentials-file",
                "/d/named-tunnels/cloudfairy-app-example-com.json", "cloudfairy-app-example-com"
            ]
        );
        let id = "6ff42ae2-765d-4adf-8112-31c55c1551ef";
        assert_eq!(
            route_args(cfg, cert, id, "app.example.com"),
            [
                "tunnel", "--no-autoupdate", "--config", "/c/cloudflared-named.yml", "--origincert",
                "/h/.cloudflared/cert.pem", "route", "dns", id, "app.example.com"
            ]
        );
        assert_eq!(
            delete_args(cfg, cert, id),
            [
                "tunnel", "--no-autoupdate", "--config", "/c/cloudflared-named.yml", "--origincert",
                "/h/.cloudflared/cert.pem", "delete", "-f", id
            ]
        );
    }

    #[test]
    fn builds_run_args_from_quick_args() {
        let p = Prepared {
            tunnel_id: "6ff42ae2-765d-4adf-8112-31c55c1551ef".into(),
            credentials_file: PathBuf::from("/d/creds.json"),
            config_file: PathBuf::from("/c/cloudflared-named.yml"),
            cert: Some(PathBuf::from("/h/cert.pem")),
        };
        let quick: Vec<String> = [
            "tunnel", "--no-autoupdate", "--metrics", "127.0.0.1:0", "--loglevel", "debug", "--protocol", "http2",
            "--no-tls-verify", "--http-host-header", "localhost:8443", "--url", "https://localhost:8443",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            run_args(quick, &p),
            [
                "tunnel", "--no-autoupdate", "--metrics", "127.0.0.1:0", "--loglevel", "debug", "--protocol", "http2",
                "--config", "/c/cloudflared-named.yml", "--origincert", "/h/cert.pem",
                "run", "--credentials-file", "/d/creds.json",
                "--no-tls-verify", "--http-host-header", "localhost:8443", "--url", "https://localhost:8443",
                "6ff42ae2-765d-4adf-8112-31c55c1551ef"
            ]
        );
        let quick: Vec<String> = ["tunnel", "--no-autoupdate", "--url", "http://localhost:3000"].map(String::from).into();
        let p = Prepared { cert: None, ..p };
        assert_eq!(
            run_args(quick, &p),
            [
                "tunnel", "--no-autoupdate", "--config", "/c/cloudflared-named.yml",
                "run", "--credentials-file", "/d/creds.json", "--url", "http://localhost:3000",
                "6ff42ae2-765d-4adf-8112-31c55c1551ef"
            ]
        );
    }

    #[test]
    fn old_tunnels_load_without_named() {
        let t: crate::settings::TunnelConfig =
            serde_json::from_str(r#"{"id":"t1","name":"App","target":"http://localhost:3000"}"#).unwrap();
        assert!(t.named.is_none());
        let n: NamedTunnel = serde_json::from_str(r#"{"hostname":"app.example.com"}"#).unwrap();
        assert_eq!(n, NamedTunnel { hostname: "app.example.com".into(), ..Default::default() });
    }
}
