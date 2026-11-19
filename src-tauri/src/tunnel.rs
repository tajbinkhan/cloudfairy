//! Runs `cloudflared tunnel --url …` processes and turns their log output into state.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime as TauriRuntime};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
};

use crate::{child_guard, settings::TunnelConfig};

const MAX_LOG_LINES: usize = 1000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub const EVENT_UPDATE: &str = "tunnel-update";
pub const EVENT_LOG: &str = "tunnel-log";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    pub status: Status,
    pub public_url: Option<String>,
    pub error: Option<String>,
    pub warning: Option<String>,
    /// Unix time in milliseconds.
    pub started_at: Option<u64>,
    pub connections: u32,
}

#[derive(Clone, Serialize)]
pub struct TunnelView {
    #[serde(flatten)]
    pub config: TunnelConfig,
    #[serde(flatten)]
    pub runtime: RuntimeView,
}

#[derive(Clone, Serialize)]
struct UpdateEvent<'a> {
    id: &'a str,
    #[serde(flatten)]
    runtime: &'a RuntimeView,
}

#[derive(Clone, Serialize)]
struct LogEvent<'a> {
    id: &'a str,
    line: &'a str,
}

#[derive(Default)]
struct Runtime {
    view: RuntimeView,
    logs: VecDeque<String>,
    child: Option<Child>,
    /// PID of `child`, kept because tokio forgets it once the process has been reaped.
    pid: Option<u32>,
    /// Set while cloudflared is being spawned, so a second Start can't launch another copy.
    launching: bool,
    /// cloudflared's `connIndex` of each connection to Cloudflare that is up right now.
    conns: HashSet<u32>,
    /// Identifies one launch, so output from an old process can't touch a newer one.
    run_id: u64,
    stopping: bool,
}

impl Runtime {
    fn active(&self) -> bool {
        self.launching || self.child.is_some()
    }

    fn log(&mut self, line: String) {
        if self.logs.len() >= MAX_LOG_LINES {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }
}

#[derive(Default)]
struct Inner {
    map: Mutex<HashMap<String, Runtime>>,
    next_run: AtomicU64,
}

#[derive(Clone, Default)]
pub struct Tunnels {
    inner: Arc<Inner>,
}

impl Tunnels {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Runtime>> {
        self.inner.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn view(&self, config: TunnelConfig) -> TunnelView {
        let runtime = self
            .map()
            .get(&config.id)
            .map(|r| r.view.clone())
            .unwrap_or_default();
        TunnelView { config, runtime }
    }

    pub fn logs(&self, id: &str) -> Vec<String> {
        self.map()
            .get(id)
            .map(|r| r.logs.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.map().get(id).is_some_and(Runtime::active)
    }

    pub fn any_active(&self) -> bool {
        self.map().values().any(Runtime::active)
    }

    pub async fn start<R: TauriRuntime>(
        &self,
        app: &AppHandle<R>,
        cfg: &TunnelConfig,
        bin: &Path,
        protocol: &str,
    ) -> Result<(), String> {
        let args = build_args(&cfg.target, protocol, cfg.rewrite_host);
        let command_line = format!("{} {}", bin.display(), args.join(" "));

        // Claim the tunnel before the first await, so two quick clicks can't both launch it.
        let run_id = {
            let mut map = self.map();
            let rt = map.entry(cfg.id.clone()).or_default();
            if rt.active() {
                return Err("This tunnel is already running.".into());
            }
            let run_id = self.inner.next_run.fetch_add(1, Ordering::Relaxed) + 1;
            rt.view = RuntimeView {
                status: Status::Starting,
                started_at: Some(now_ms()),
                ..Default::default()
            };
            rt.launching = true;
            rt.run_id = run_id;
            rt.stopping = false;
            rt.conns.clear();
            rt.logs.clear();
            rt.log(format!("$ {command_line}"));
            emit_update(app, &cfg.id, &rt.view);
            run_id
        };

        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        hide_console(&mut cmd);
        let spawned = child_guard::spawn(cmd, command_line).await;

        let (stdout, stderr) = {
            let mut map = self.map();
            // Removed, or stopped and started again, while it was launching: dropping
            // `spawned` kills the process.
            let Some(rt) = map.get_mut(&cfg.id).filter(|rt| rt.run_id == run_id) else {
                if let Ok(child) = &spawned {
                    child_guard::exited(child.id());
                }
                return Err("The tunnel was removed while it was starting.".into());
            };
            rt.launching = false;
            let mut child = match spawned {
                Ok(child) => child,
                Err(e) => {
                    let msg = format!("Couldn't launch cloudflared at {}: {e}", bin.display());
                    rt.view = RuntimeView {
                        status: Status::Error,
                        error: Some(msg.clone()),
                        ..Default::default()
                    };
                    rt.log(format!("[cloudfairy] {msg}"));
                    emit_update(app, &cfg.id, &rt.view);
                    return Err(msg);
                }
            };
            if rt.stopping {
                // Stop was pressed while it was launching; `watch` finishes the cleanup.
                let _ = child.start_kill();
            }
            rt.pid = child.id();
            let streams = (child.stdout.take(), child.stderr.take());
            rt.child = Some(child);
            streams
        };

        if let Some(out) = stdout {
            tauri::async_runtime::spawn(self.clone().pump(app.clone(), cfg.id.clone(), run_id, out));
        }
        if let Some(err) = stderr {
            tauri::async_runtime::spawn(self.clone().pump(app.clone(), cfg.id.clone(), run_id, err));
        }
        tauri::async_runtime::spawn(self.clone().watch(
            app.clone(),
            cfg.id.clone(),
            cfg.target.clone(),
            run_id,
        ));

        if let Some(warning) = probe_local(&cfg.target).await {
            let mut map = self.map();
            if let Some(rt) = map.get_mut(&cfg.id).filter(|rt| rt.run_id == run_id && !rt.stopping) {
                rt.view.warning = Some(warning.clone());
                rt.log(format!("[cloudfairy] {warning}"));
                emit_update(app, &cfg.id, &rt.view);
            }
        }
        Ok(())
    }

    pub fn stop(&self, id: &str) {
        let mut map = self.map();
        if let Some(rt) = map.get_mut(id).filter(|rt| rt.active()) {
            rt.stopping = true;
            // While it's still launching, `start` kills it as soon as the process exists.
            if let Some(child) = rt.child.as_mut() {
                let _ = child.start_kill();
            }
            rt.log("[cloudfairy] Stopping…".into());
        }
    }

    pub fn remove(&self, id: &str) {
        if let Some(mut rt) = self.map().remove(id) {
            if let Some(mut child) = rt.child.take() {
                let _ = child.start_kill();
                child_guard::exited(rt.pid);
            }
        }
    }

    /// Called when the app exits so no cloudflared process is left behind.
    pub fn kill_all(&self) {
        for rt in self.map().values_mut() {
            rt.stopping = true;
            if let Some(child) = rt.child.as_mut() {
                let _ = child.start_kill();
            }
        }
        child_guard::all_stopped();
    }

    async fn pump<R: TauriRuntime, Rd>(self, app: AppHandle<R>, id: String, run_id: u64, reader: Rd)
    where
        Rd: AsyncRead + Unpin + Send + 'static,
    {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            self.handle_line(&app, &id, run_id, line);
        }
    }

    fn handle_line<R: TauriRuntime>(&self, app: &AppHandle<R>, id: &str, run_id: u64, line: String) {
        let mut map = self.map();
        let Some(rt) = map.get_mut(id) else { return };
        if rt.run_id != run_id {
            return;
        }

        let view = &mut rt.view;
        let mut changed = false;
        if view.public_url.is_none() {
            if let Some(url) = find_quick_url(&line) {
                view.public_url = Some(url);
                changed = true;
            }
        }
        // cloudflared keeps up to four connections, numbered by connIndex. One drop can log both
        // "Connection terminated" and "Unregistered", so track the set rather than a counter.
        let index = conn_index(&line);
        if line.contains("Registered tunnel connection") {
            let fallback = u32::MAX - rt.conns.len() as u32;
            rt.conns.insert(index.unwrap_or(fallback));
            view.status = Status::Running;
            view.error = None;
            changed = true;
        } else if line.contains("Unregistered tunnel connection") || line.contains("Connection terminated") {
            match index {
                Some(i) => {
                    rt.conns.remove(&i);
                }
                None => {
                    if let Some(&any) = rt.conns.iter().next() {
                        rt.conns.remove(&any);
                    }
                }
            }
            changed = true;
        }
        view.connections = rt.conns.len() as u32;
        if line.contains("Unable to reach the origin service") {
            view.warning = Some(
                "Cloudflare reached your computer, but nothing answered at the local address. \
                 Is your server running?"
                    .into(),
            );
            changed = true;
        } else if let Some(msg) = error_message(&line) {
            if !rt.stopping && view.status != Status::Running {
                view.error = Some(msg);
                changed = true;
            }
        }

        let _ = app.emit(EVENT_LOG, LogEvent { id, line: &line });
        if changed {
            emit_update(app, id, &rt.view);
        }
        rt.log(line);
    }

    /// Waits for the process to exit. While the local server looks down, it re-checks
    /// every few seconds so the warning disappears once the server comes up.
    async fn watch<R: TauriRuntime>(self, app: AppHandle<R>, id: String, target: String, run_id: u64) {
        let mut tick: u32 = 0;
        let (success, description) = loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            tick = tick.wrapping_add(1);
            let recheck = {
                let mut map = self.map();
                let Some(rt) = map.get_mut(&id) else { return };
                if rt.run_id != run_id {
                    return;
                }
                let Some(child) = rt.child.as_mut() else { return };
                match child.try_wait() {
                    Ok(Some(status)) => break (status.success(), status.to_string()),
                    Ok(None) => {}
                    Err(e) => break (false, e.to_string()),
                }
                rt.view.warning.is_some() && tick % 12 == 0
            };
            if recheck && probe_local(&target).await.is_none() {
                let mut map = self.map();
                if let Some(rt) = map.get_mut(&id).filter(|rt| rt.run_id == run_id) {
                    rt.view.warning = None;
                    rt.log(format!("[cloudfairy] {target} is answering now."));
                    emit_update(&app, &id, &rt.view);
                }
            }
        };

        // Let the output readers drain the last lines (they often explain the failure).
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut map = self.map();
        let Some(rt) = map.get_mut(&id) else { return };
        if rt.run_id != run_id {
            return;
        }
        rt.child = None;
        child_guard::exited(rt.pid.take());
        rt.conns.clear();
        let view = &mut rt.view;
        view.connections = 0;
        view.public_url = None;
        if rt.stopping || success {
            *view = RuntimeView::default();
        } else {
            view.status = Status::Error;
            view.started_at = None;
            if view.error.is_none() {
                view.error = Some(format!("cloudflared stopped unexpectedly ({description})."));
            }
        }
        rt.log(format!("[cloudfairy] cloudflared exited ({description})"));
        emit_update(&app, &id, &rt.view);
    }
}

fn emit_update<R: TauriRuntime>(app: &AppHandle<R>, id: &str, runtime: &RuntimeView) {
    let _ = app.emit(EVENT_UPDATE, UpdateEvent { id, runtime });
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(windows)]
pub fn hide_console(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console(_cmd: &mut Command) {}

fn build_args(target: &str, protocol: &str, rewrite_host: bool) -> Vec<String> {
    let mut args: Vec<String> = vec!["tunnel".into(), "--no-autoupdate".into()];
    if matches!(protocol, "quic" | "http2") {
        args.push("--protocol".into());
        args.push(protocol.into());
    }
    // Local HTTPS dev servers almost always use self-signed certificates.
    if target.starts_with("https://") && host_port(target).is_some_and(|(h, _)| is_loopback(&h)) {
        args.push("--no-tls-verify".into());
    }
    if let Some(host) = rewrite_host.then(|| host_header(target)).flatten() {
        args.push("--http-host-header".into());
        args.push(host);
    }
    args.push("--url".into());
    args.push(target.into());
    args
}

/// Accepts `3000`, `:3000`, `localhost:3000`, `127.0.0.1:8080/app` or a full http(s) URL.
pub fn normalize_target(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    let s = s.strip_prefix(':').unwrap_or(s);
    if s.is_empty() {
        return Err("Enter a port (like 3000) or a local address.".into());
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let port = s
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or("Port must be a number from 1 to 65535.")?;
        return Ok(format!("http://localhost:{port}"));
    }

    let url = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    let scheme = url.split("://").next().unwrap_or_default().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("Only http:// and https:// addresses are supported.".into());
    }
    match host_port(&url) {
        Some((host, _)) if !host.is_empty() && !host.contains(char::is_whitespace) => Ok(url),
        _ => Err("That doesn't look like a valid address. Try something like localhost:3000.".into()),
    }
}

/// The `host[:port]` part of a URL, as a browser would send it in the `Host` header.
fn host_header(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    (!authority.is_empty()).then(|| authority.to_string())
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let default_port = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };

    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((authority.to_string(), default_port)),
    }
}

fn is_loopback(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']).to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost") || h == "::1" || h == "0.0.0.0" || h.starts_with("127.")
}

/// Returns a warning when nothing is listening on the target yet.
async fn probe_local(target: &str) -> Option<String> {
    let (host, port) = host_port(target)?;
    let addr = format!("{host}:{port}");
    let connect = tokio::net::TcpStream::connect(addr.clone());
    match tokio::time::timeout(Duration::from_millis(800), connect).await {
        Ok(Ok(_)) => None,
        _ => Some(format!(
            "Nothing is answering on {addr} yet. The public URL will show an error page until your local server is running."
        )),
    }
}

fn find_quick_url(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(i) = rest.find("https://") {
        let tail = &rest[i..];
        let end = tail[8..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
            .map_or(tail.len(), |e| e + 8);
        let candidate = tail[..end].trim_end_matches('.');
        if candidate.ends_with(".trycloudflare.com") && candidate != "https://api.trycloudflare.com" {
            return Some(candidate.to_string());
        }
        rest = &tail[8..];
    }
    None
}

/// Reads `connIndex=2` from a cloudflared log line.
fn conn_index(line: &str) -> Option<u32> {
    let rest = &line[line.find("connIndex=")? + "connIndex=".len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Extracts the message from a cloudflared `ERR` log line.
fn error_message(line: &str) -> Option<String> {
    let (_, msg) = line.split_once(" ERR ")?;
    let msg = msg.trim();
    (!msg.is_empty()).then(|| msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_targets() {
        assert_eq!(normalize_target("3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_target(" :8080 ").unwrap(), "http://localhost:8080");
        assert_eq!(normalize_target("127.0.0.1:5173/").unwrap(), "http://127.0.0.1:5173");
        assert_eq!(normalize_target("https://localhost:8443").unwrap(), "https://localhost:8443");
        assert_eq!(normalize_target("http://[::1]:9000/app").unwrap(), "http://[::1]:9000/app");
        assert!(normalize_target("0").is_err());
        assert!(normalize_target("70000").is_err());
        assert!(normalize_target("ftp://localhost").is_err());
        assert!(normalize_target("").is_err());
        assert!(normalize_target("localhost:abc").is_err());
    }

    #[test]
    fn parses_host_and_port() {
        assert_eq!(host_port("http://localhost:3000").unwrap(), ("localhost".into(), 3000));
        assert_eq!(host_port("https://example.test/x").unwrap(), ("example.test".into(), 443));
        assert_eq!(host_port("http://[::1]:81").unwrap(), ("[::1]".into(), 81));
    }

    #[test]
    fn finds_quick_tunnel_url() {
        let line = "2026-11-17T09:32:19Z INF |  https://con-disposition-reveal-comparing.trycloudflare.com                                |";
        assert_eq!(
            find_quick_url(line).as_deref(),
            Some("https://con-disposition-reveal-comparing.trycloudflare.com")
        );
        assert_eq!(find_quick_url("INF Requesting new quick Tunnel on trycloudflare.com..."), None);
        assert_eq!(find_quick_url("ERR failed to reach https://api.trycloudflare.com/tunnel"), None);
    }

    #[test]
    fn extracts_errors() {
        assert_eq!(
            error_message("2026-11-17T09:32:34Z ERR no more connections active and exiting").as_deref(),
            Some("no more connections active and exiting")
        );
        assert_eq!(error_message("2026-11-17T09:32:34Z INF Tunnel server stopped"), None);
    }

    #[test]
    fn reads_conn_index() {
        assert_eq!(conn_index("INF Registered tunnel connection connIndex=2 connection=abc"), Some(2));
        assert_eq!(conn_index("ERR Connection terminated error=x connIndex=13"), Some(13));
        assert_eq!(conn_index("INF Starting tunnel"), None);
    }

    #[test]
    fn counts_connections_by_index() {
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        let tunnels = Tunnels::default();
        tunnels.map().insert(
            "t".into(),
            Runtime {
                run_id: 1,
                ..Default::default()
            },
        );
        let feed = |line: &str| tunnels.handle_line(&handle, "t", 1, line.into());
        let view = || tunnels.map()["t"].view.clone();

        feed("INF Registered tunnel connection connIndex=0 connection=a");
        feed("INF Registered tunnel connection connIndex=1 connection=b");
        assert_eq!((view().status, view().connections), (Status::Running, 2));

        // One drop logs two lines; it must only count once.
        feed("ERR Connection terminated error=\"timeout\" connIndex=0");
        feed("INF Unregistered tunnel connection connIndex=0 event=0");
        assert_eq!(view().connections, 1);

        feed("INF Unregistered tunnel connection connIndex=1 event=0");
        assert_eq!((view().status, view().connections), (Status::Running, 0));
        assert!(view().error.is_none(), "drops while running aren't shown as a failure");

        feed("INF Registered tunnel connection connIndex=1 connection=c");
        assert_eq!(view().connections, 1);
    }

    #[test]
    fn builds_args() {
        assert_eq!(
            build_args("https://localhost:8443", "http2", false),
            ["tunnel", "--no-autoupdate", "--protocol", "http2", "--no-tls-verify", "--url", "https://localhost:8443"]
        );
        assert_eq!(
            build_args("http://localhost:3000", "auto", false),
            ["tunnel", "--no-autoupdate", "--url", "http://localhost:3000"]
        );
        assert_eq!(
            build_args("http://localhost:5173/app", "auto", true),
            ["tunnel", "--no-autoupdate", "--http-host-header", "localhost:5173", "--url", "http://localhost:5173/app"]
        );
    }

    #[test]
    fn host_header_keeps_port_and_drops_path() {
        assert_eq!(host_header("http://127.0.0.1:8000/app?x=1").as_deref(), Some("127.0.0.1:8000"));
        assert_eq!(host_header("https://[::1]:8443").as_deref(), Some("[::1]:8443"));
        assert_eq!(host_header("http://localhost").as_deref(), Some("localhost"));
        assert_eq!(host_header("localhost:3000"), None);
    }
    /// Full run against the real cloudflared and Cloudflare's network:
    /// `cargo test -- --ignored quick_tunnel_end_to_end`
    #[test]
    #[ignore = "needs network access and cloudflared on PATH"]
    fn quick_tunnel_end_to_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        tauri::async_runtime::block_on(async {
            // Reserve a free port, but don't serve on it yet.
            let port = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let app = tauri::test::mock_app();
            let handle = app.handle().clone();
            let tunnels = Tunnels::default();
            let cfg = TunnelConfig {
                id: "e2e".into(),
                name: "e2e".into(),
                target: normalize_target(&format!("127.0.0.1:{port}")).unwrap(),
                rewrite_host: false,
            };
            let bin = which::which("cloudflared").expect("cloudflared on PATH");

            tunnels.start(&handle, &cfg, &bin, "auto").await.unwrap();
            assert!(tunnels.start(&handle, &cfg, &bin, "auto").await.is_err(), "double start is rejected");
            assert!(tunnels.view(cfg.clone()).runtime.warning.is_some(), "warns while nothing listens");

            let view = wait_for(&tunnels, &cfg, 60, |v| v.status == Status::Running && v.public_url.is_some()).await;
            let url = view.public_url.unwrap();
            println!("public url: {url}");

            // Now bring the local server up; the warning should clear by itself.
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            tauri::async_runtime::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    tauri::async_runtime::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let _ = sock.read(&mut buf).await;
                        let body = "hello from cloudfairy";
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
            wait_for(&tunnels, &cfg, 10, |v| v.warning.is_none()).await;

            // A brand-new hostname can take a few seconds to resolve.
            let client = reqwest::Client::new();
            let mut body = String::new();
            for _ in 0..30 {
                if let Ok(r) = client.get(&url).send().await {
                    if r.status().is_success() {
                        body = r.text().await.unwrap_or_default();
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            assert_eq!(body, "hello from cloudfairy", "request through the public URL reaches the local server");

            tunnels.stop(&cfg.id);
            let view = wait_for(&tunnels, &cfg, 10, |v| v.status == Status::Stopped).await;
            assert!(view.public_url.is_none());
            assert!(!tunnels.is_active(&cfg.id));
        });
    }

    async fn wait_for(
        tunnels: &Tunnels,
        cfg: &TunnelConfig,
        secs: u64,
        done: impl Fn(&RuntimeView) -> bool,
    ) -> RuntimeView {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let view = tunnels.view(cfg.clone()).runtime;
            if done(&view) {
                return view;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out; state {view:?}\nlogs:\n{}", tunnels.logs(&cfg.id).join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
