//! Folder sharing. While a folder tunnel runs, Cloudfairy serves the folder read-only on a
//! random `127.0.0.1` port and points cloudflared at that port.
//!
//! The link is public, so the server only answers GET and HEAD, never leaves the folder
//! (`..`, encoded separators and symlinks that point out are refused), and hides dotfiles.

use std::{
    collections::HashMap,
    convert::Infallible,
    ffi::OsStr,
    fs, io,
    net::Ipv4Addr,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::Duration,
};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Empty, Full, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    header::{self, HeaderName, HeaderValue},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tauri::{AppHandle, Runtime as TauriRuntime};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    net::TcpListener,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

use crate::{
    settings::{TunnelConfig, TunnelKind},
    tunnel::Tunnels,
};

type Body = UnsyncBoxBody<Bytes, io::Error>;

/// A folder listing stops after this many entries.
const MAX_LISTING: usize = 5000;
/// How often a running share checks whether its tunnel stopped.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Characters left as they are in links on the listing page.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

struct Server {
    /// Tells this server apart from a later one for the same tunnel.
    generation: u64,
    port: u16,
    cancel: CancellationToken,
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn servers() -> MutexGuard<'static, HashMap<String, Server>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, Server>>> = OnceLock::new();
    SERVERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// What the card and tray show as a tunnel's target: the folder, or the local address.
pub fn label(cfg: &TunnelConfig) -> &str {
    match (cfg.kind, cfg.folder.as_deref()) {
        (TunnelKind::Folder, Some(folder)) => folder,
        _ => &cfg.target,
    }
}

/// Checks a folder chosen in the window and returns it as it will be saved.
pub fn validate_folder(input: &str) -> Result<String, String> {
    let path = PathBuf::from(input.trim());
    if input.trim().is_empty() || !path.is_absolute() {
        return Err("Choose a folder to share.".into());
    }
    if !path.is_dir() {
        return Err(format!("{} isn't a folder.", path.display()));
    }
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "Cloudfairy can't share a folder whose path has characters it can't read.".into())
}

/// The folder's own name, for a tunnel's default name. A drive root keeps its full path.
pub fn folder_name(folder: &str) -> String {
    Path::new(folder)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(folder)
        .to_string()
}

/// Ports the running folder servers listen on.
pub fn ports() -> Vec<u16> {
    servers().values().map(|s| s.port).collect()
}

/// Starts serving the tunnel's folder, then starts cloudflared pointed at it. The server
/// stops when the tunnel stops.
pub async fn start<R: TauriRuntime>(
    app: &AppHandle<R>,
    tunnels: &Tunnels,
    cfg: &TunnelConfig,
    bin: &Path,
    protocol: &str,
) -> Result<(), String> {
    let folder = cfg.folder.as_deref().ok_or("This tunnel has no folder to share.")?;
    if tunnels.is_active(&cfg.id) {
        return Err("This tunnel is already running.".into());
    }
    let root = fs::canonicalize(folder)
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| format!("Couldn't open {folder}. It may have been moved or deleted."))?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| format!("Couldn't start the file server: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Couldn't start the file server: {e}"))?
        .port();
    let cancel = CancellationToken::new();
    tauri::async_runtime::spawn(serve(listener, Arc::new(root), cancel.clone()));

    let local = TunnelConfig {
        target: format!("http://127.0.0.1:{port}"),
        rewrite_host: false,
        ..cfg.clone()
    };
    if let Err(e) = tunnels.start(app, &local, bin, protocol).await {
        cancel.cancel();
        return Err(e);
    }
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    let old = servers().insert(cfg.id.clone(), Server { generation, port, cancel });
    if let Some(old) = old {
        old.cancel.cancel();
    }
    tauri::async_runtime::spawn(watch(tunnels.clone(), cfg.id.clone(), generation));
    Ok(())
}

/// Stops the server once its tunnel stops or is removed.
async fn watch(tunnels: Tunnels, id: String, generation: u64) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let active = tunnels.is_active(&id);
        let mut map = servers();
        if !matches!(map.get(&id), Some(s) if s.generation == generation) {
            return;
        }
        if !active {
            if let Some(server) = map.remove(&id) {
                server.cancel.cancel();
            }
            return;
        }
    }
}

async fn serve(listener: TcpListener, root: Arc<PathBuf>, cancel: CancellationToken) {
    loop {
        let stream = tokio::select! {
            _ = cancel.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => {
                    // Out of file handles, say. Wait instead of spinning.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let root = root.clone();
        let cancel = cancel.clone();
        tauri::async_runtime::spawn(async move {
            let service = service_fn(move |req| {
                let root = root.clone();
                async move { Ok::<_, Infallible>(handle(req, root).await) }
            });
            let mut builder = http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(30));
            let conn = builder.serve_connection(TokioIo::new(stream), service);
            tokio::select! {
                _ = conn => {}
                _ = cancel.cancelled() => {}
            }
        });
    }
}

/// What a request path maps to.
#[derive(Debug, PartialEq)]
enum Resolved {
    File(PathBuf),
    Listing(String),
    Redirect(String),
    NotFound,
}

async fn handle(req: Request<Incoming>, root: Arc<PathBuf>) -> Response<Body> {
    let head = req.method() == Method::HEAD;
    let mut response = if req.method() != Method::GET && !head {
        let mut r = text(StatusCode::METHOD_NOT_ALLOWED, "Only GET and HEAD requests are allowed.\n");
        set(&mut r, header::ALLOW, "GET, HEAD");
        r
    } else {
        let path = req.uri().path().to_string();
        let range = req.headers().get(header::RANGE).and_then(|v| v.to_str().ok()).map(str::to_string);
        let resolved = tauri::async_runtime::spawn_blocking(move || resolve(&root, &path))
            .await
            .unwrap_or(Resolved::NotFound);
        match resolved {
            Resolved::File(path) => serve_file(&path, range.as_deref(), head).await,
            Resolved::Listing(html) => {
                let mut r = sized(StatusCode::OK, Bytes::from(html));
                set(&mut r, header::CONTENT_TYPE, "text/html; charset=utf-8");
                set(&mut r, header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'");
                r
            }
            Resolved::Redirect(location) => {
                let mut r = text(StatusCode::MOVED_PERMANENTLY, "Moved.\n");
                set(&mut r, header::LOCATION, &location);
                r
            }
            Resolved::NotFound => text(StatusCode::NOT_FOUND, "Not found.\n"),
        }
    };
    set(&mut response, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(&mut response, header::REFERRER_POLICY, "no-referrer");
    if head {
        // Content-Length stays; hyper sends no body for HEAD.
        *response.body_mut() = empty();
    }
    response
}

/// Maps a request path to something inside `root`, which must be canonical.
fn resolve(root: &Path, url_path: &str) -> Resolved {
    let Some((path, segments)) = locate(root, url_path) else { return Resolved::NotFound };
    let Ok(meta) = fs::metadata(&path) else { return Resolved::NotFound };
    if meta.is_file() {
        return Resolved::File(path);
    }
    if !meta.is_dir() {
        return Resolved::NotFound;
    }
    // Relative links on the page only work when a folder's URL ends with a slash.
    if !segments.is_empty() && !url_path.ends_with('/') {
        return Resolved::Redirect(href(&segments, true));
    }
    if let Some(index) = visible_inside(root, &path.join("index.html")).filter(|p| p.is_file()) {
        return Resolved::File(index);
    }
    match listing(root, &path, &segments) {
        Ok(html) => Resolved::Listing(html),
        Err(_) => Resolved::NotFound,
    }
}

/// Decodes the path and checks each segment, then follows symlinks and checks that the
/// result is still inside `root` and not hidden. Returns the real path and the segments.
fn locate(root: &Path, url_path: &str) -> Option<(PathBuf, Vec<String>)> {
    if !url_path.starts_with('/') {
        return None;
    }
    let decoded = percent_decode_str(url_path).decode_utf8().ok()?;
    let mut segments = Vec::new();
    for seg in decoded.split('/').filter(|s| !s.is_empty()) {
        if !safe_segment(seg) {
            return None;
        }
        segments.push(seg.to_string());
    }
    let mut path = root.to_path_buf();
    path.extend(&segments);
    let real = visible_inside(root, &path)?;
    Some((real, segments))
}

/// A path segment that names a visible entry in the current folder and nothing else.
fn safe_segment(seg: &str) -> bool {
    // A leading dot covers `.`, `..` and dotfiles.
    !seg.starts_with('.')
        // Other separators, drive letters and NTFS streams.
        && !seg.contains(['\\', ':'])
        && !seg.chars().any(char::is_control)
        && !(cfg!(windows) && windows_alias(seg))
}

/// Names Windows treats as something else: devices such as `CON` or `NUL.txt`, and names
/// with trailing dots or spaces, which Windows drops.
fn windows_alias(seg: &str) -> bool {
    if seg.ends_with(['.', ' ']) {
        return true;
    }
    let stem = seg.split('.').next().unwrap_or(seg).trim_end().to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        s if s.starts_with("COM") || s.starts_with("LPT") => {
            matches!(&s[3..], "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
        }
        _ => false,
    }
}

/// Resolves symlinks in `path` and returns the real path if it lies inside `root` and no
/// part of it below `root` is hidden.
fn visible_inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(path).ok()?;
    let rel = real.strip_prefix(root).ok()?;
    let mut at = root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(name) = part else { return None };
        at.push(name);
        if hidden(name, &at) {
            return None;
        }
    }
    Some(real)
}

/// Dotfiles everywhere, plus files Windows marks hidden or system (desktop.ini, Thumbs.db).
fn hidden(name: &OsStr, path: &Path) -> bool {
    if name.to_string_lossy().starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        if let Ok(meta) = fs::metadata(path) {
            return meta.file_attributes() & (HIDDEN | SYSTEM) != 0;
        }
    }
    #[cfg(not(windows))]
    let _ = path;
    false
}

/// `/a%20b/c/` from the segments `["a b", "c"]`.
fn href(segments: &[String], dir: bool) -> String {
    let mut out = String::from("/");
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.extend(utf8_percent_encode(seg, SEGMENT));
    }
    if dir && !segments.is_empty() {
        out.push('/');
    }
    out
}

fn listing(root: &Path, dir: &Path, segments: &[String]) -> io::Result<String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)?.flatten().take(MAX_LISTING) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !safe_segment(name) {
            continue;
        }
        // Symlinks are listed only when they point somewhere inside the folder.
        let Some(real) = visible_inside(root, &entry.path()) else { continue };
        let Ok(meta) = fs::metadata(&real) else { continue };
        if meta.is_dir() || meta.is_file() {
            entries.push((meta.is_dir(), name.to_string(), meta.len()));
        }
    }
    entries.sort_by_cached_key(|(is_dir, name, _)| (!is_dir, name.to_lowercase()));

    let root_name = root.file_name().and_then(OsStr::to_str).unwrap_or("Shared folder");
    let title = std::iter::once(root_name)
        .chain(segments.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" / ");
    let mut rows = String::new();
    if !segments.is_empty() {
        rows.push_str("<li><a href=\"../\">..</a><span></span></li>\n");
    }
    for (is_dir, name, size) in &entries {
        let link: String = utf8_percent_encode(name, SEGMENT).collect();
        let (slash, size) = if *is_dir { ("/", String::new()) } else { ("", format_size(*size)) };
        rows.push_str(&format!(
            "<li><a href=\"{link}{slash}\">{}{slash}</a><span>{size}</span></li>\n",
            escape_html(name)
        ));
    }
    if entries.is_empty() {
        rows.push_str("<li class=\"empty\">This folder is empty.</li>\n");
    }
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; --bg: #f6f5f3; --text: #1d1b19; --muted: #6b665f; --line: #e4e1dc; --link: #c2185b; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #141312; --text: #f1eeea; --muted: #a39d95; --line: #33302d; --link: #ff7aa8; }} }}
body {{ margin: 0; background: var(--bg); color: var(--text); font: 15px/1.5 system-ui, -apple-system, "Segoe UI", Roboto, sans-serif; }}
main {{ max-width: 760px; margin: 0 auto; padding: 24px 16px 40px; }}
h1 {{ font-size: 18px; margin: 0 0 16px; overflow-wrap: anywhere; }}
ul {{ list-style: none; margin: 0; padding: 0; border-top: 1px solid var(--line); }}
li {{ display: flex; gap: 16px; padding: 8px 2px; border-bottom: 1px solid var(--line); }}
li a {{ flex: 1; min-width: 0; color: var(--link); text-decoration: none; overflow-wrap: anywhere; }}
li a:hover {{ text-decoration: underline; }}
li span {{ color: var(--muted); font-variant-numeric: tabular-nums; white-space: nowrap; }}
li.empty {{ color: var(--muted); }}
p {{ color: var(--muted); font-size: 12.5px; margin-top: 16px; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
<ul>
{rows}</ul>
<p>Shared from Cloudfairy. Read-only.</p>
</main>
</body>
</html>
"#,
        title = escape_html(&title),
    ))
}

async fn serve_file(path: &Path, range: Option<&str>, head: bool) -> Response<Body> {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let Ok(meta) = file.metadata().await else {
        return text(StatusCode::NOT_FOUND, "Not found.\n");
    };
    let len = meta.len();
    let (status, start, count) = match range.map(|r| parse_range(r, len)) {
        None | Some(ByteRange::Whole) => (StatusCode::OK, 0, len),
        Some(ByteRange::Part(start, end)) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        Some(ByteRange::Unsatisfiable) => {
            let mut r = text(StatusCode::RANGE_NOT_SATISFIABLE, "Range not satisfiable.\n");
            set(&mut r, header::CONTENT_RANGE, &format!("bytes */{len}"));
            return r;
        }
    };
    if start > 0 && file.seek(io::SeekFrom::Start(start)).await.is_err() {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Couldn't read the file.\n");
    }
    let body = if head {
        empty()
    } else {
        StreamBody::new(ReaderStream::with_capacity(file.take(count), 64 * 1024).map_ok(Frame::data)).boxed_unsync()
    };
    let mut r = Response::new(body);
    *r.status_mut() = status;
    set(&mut r, header::CONTENT_TYPE, content_type(path));
    set(&mut r, header::CONTENT_LENGTH, &count.to_string());
    set(&mut r, header::ACCEPT_RANGES, "bytes");
    set(&mut r, header::CACHE_CONTROL, "no-cache");
    if status == StatusCode::PARTIAL_CONTENT {
        set(&mut r, header::CONTENT_RANGE, &format!("bytes {start}-{}/{len}", start + count - 1));
    }
    r
}

#[derive(Debug, PartialEq)]
enum ByteRange {
    Whole,
    /// First and last byte, inclusive.
    Part(u64, u64),
    Unsatisfiable,
}

/// Reads a `Range: bytes=…` header. Several ranges, other units and malformed values get the
/// whole file, which HTTP allows.
fn parse_range(value: &str, len: u64) -> ByteRange {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return ByteRange::Whole };
    if spec.contains(',') {
        return ByteRange::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else { return ByteRange::Whole };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last n bytes.
        let Ok(n) = last.parse::<u64>() else { return ByteRange::Whole };
        if n == 0 || len == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Part(len - n.min(len), len - 1);
    }
    let Ok(start) = first.parse::<u64>() else { return ByteRange::Whole };
    let end = match last {
        "" => u64::MAX,
        l => match l.parse::<u64>() {
            Ok(end) if end >= start => end,
            _ => return ByteRange::Whole,
        },
    };
    if start >= len {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Part(start, end.min(len - 1))
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" | "webmanifest" => "application/json",
        "txt" | "md" | "markdown" | "log" | "csv" | "tsv" | "yaml" | "yml" | "toml" | "ini" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        _ => "application/octet-stream",
    }
}

fn format_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn set(response: &mut Response<Body>, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

/// A response with a body held in memory and its Content-Length set.
fn sized(status: StatusCode, body: Bytes) -> Response<Body> {
    let len = body.len();
    let mut r = Response::new(Full::new(body).map_err(|never| match never {}).boxed_unsync());
    *r.status_mut() = status;
    set(&mut r, header::CONTENT_LENGTH, &len.to_string());
    r
}

fn text(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut r = sized(status, Bytes::from_static(message.as_bytes()));
    set(&mut r, header::CONTENT_TYPE, "text/plain; charset=utf-8");
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared folder with an index-less subfolder, a dotfile, a hidden folder, and a
    /// secret next to the shared folder (outside it).
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!("cloudfairy-share-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let root = base.join("shared");
            fs::create_dir_all(root.join("docs/sub")).unwrap();
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::create_dir_all(root.join("site")).unwrap();
            fs::write(root.join("hello.txt"), "hello").unwrap();
            fs::write(root.join("a b.txt"), "space").unwrap();
            fs::write(root.join(".env"), "SECRET=1").unwrap();
            fs::write(root.join(".git/config"), "x").unwrap();
            fs::write(root.join("docs/readme.md"), "# docs").unwrap();
            fs::write(root.join("site/index.html"), "<h1>site</h1>").unwrap();
            fs::write(base.join("secret.txt"), "outside").unwrap();
            let root = fs::canonicalize(&root).unwrap();
            Self { base, root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn file(f: &Fixture, rel: &str) -> Resolved {
        Resolved::File(fs::canonicalize(f.root.join(rel)).unwrap())
    }

    #[test]
    fn resolves_files_and_folders() {
        let f = Fixture::new("resolve");
        assert_eq!(resolve(&f.root, "/hello.txt"), file(&f, "hello.txt"));
        assert_eq!(resolve(&f.root, "/a%20b.txt"), file(&f, "a b.txt"));
        assert_eq!(resolve(&f.root, "/docs/readme.md"), file(&f, "docs/readme.md"));
        assert_eq!(resolve(&f.root, "/docs//readme.md"), file(&f, "docs/readme.md"));
        // A folder with index.html serves it; one without gets a listing.
        assert_eq!(resolve(&f.root, "/site/"), file(&f, "site/index.html"));
        assert_eq!(resolve(&f.root, "/site"), Resolved::Redirect("/site/".into()));
        let Resolved::Listing(html) = resolve(&f.root, "/") else { panic!("root lists its files") };
        assert!(html.contains("hello.txt") && html.contains("href=\"a%20b.txt\"") && html.contains("docs/"));
        assert!(!html.contains(".env") && !html.contains(".git"), "dotfiles stay out of the listing");
        assert!(!html.contains("href=\"../\""), "the root has no parent link");
        let Resolved::Listing(html) = resolve(&f.root, "/docs/") else { panic!("docs lists its files") };
        assert!(html.contains("readme.md") && html.contains("sub/") && html.contains("href=\"../\""));
        assert_eq!(resolve(&f.root, "/missing.txt"), Resolved::NotFound);
    }

    #[test]
    fn refuses_paths_outside_the_folder() {
        let f = Fixture::new("traversal");
        for path in [
            "/../secret.txt",
            "/docs/../../secret.txt",
            "/%2e%2e/secret.txt",
            "/%2E%2E%2Fsecret.txt",
            "/docs/%2e%2e/%2e%2e/secret.txt",
            "/..%5csecret.txt",
            "/docs%5c..%5c..%5csecret.txt",
            "/%252e%252e/secret.txt",
            "/hello.txt%00.png",
            "/C:/Windows/win.ini",
            "/hello.txt::$DATA",
            "/%ff",
            "relative.txt",
            "*",
        ] {
            assert_eq!(resolve(&f.root, path), Resolved::NotFound, "{path}");
        }
        // Redirects only ever point inside this site, never to //other.host.
        assert_eq!(resolve(&f.root, "//docs"), Resolved::Redirect("/docs/".into()));
    }

    #[test]
    fn hides_dotfiles() {
        let f = Fixture::new("dotfiles");
        // ENV~1 is what an 8.3 short name for .env looks like on Windows.
        for path in ["/.env", "/.git/config", "/.git/", "/%2eenv", "/./hello.txt", "/ENV~1", "/GIT~1/config"] {
            assert_eq!(resolve(&f.root, path), Resolved::NotFound, "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn refuses_windows_aliases() {
        let f = Fixture::new("aliases");
        for path in ["/CON", "/nul.txt", "/COM1", "/lpt9.log", "/hello.txt.", "/hello.txt%20"] {
            assert_eq!(resolve(&f.root, path), Resolved::NotFound, "{path}");
        }
        assert!(windows_alias("CON") && windows_alias("aux.txt") && windows_alias("com1"));
        assert!(!windows_alias("console.log") && !windows_alias("com10") && !windows_alias("index.html"));
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_only_inside_the_folder() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new("symlinks");
        symlink(f.base.join("secret.txt"), f.root.join("out.txt")).unwrap();
        symlink(&f.base, f.root.join("up")).unwrap();
        symlink(f.root.join("hello.txt"), f.root.join("in.txt")).unwrap();
        symlink(f.root.join(".env"), f.root.join("env.txt")).unwrap();
        assert_eq!(resolve(&f.root, "/out.txt"), Resolved::NotFound);
        assert_eq!(resolve(&f.root, "/up/secret.txt"), Resolved::NotFound);
        assert_eq!(resolve(&f.root, "/env.txt"), Resolved::NotFound, "a link to a dotfile is hidden too");
        assert_eq!(resolve(&f.root, "/in.txt"), file(&f, "hello.txt"));
        let Resolved::Listing(html) = resolve(&f.root, "/") else { panic!() };
        assert!(html.contains("in.txt") && !html.contains("out.txt") && !html.contains("\"up/\""));
    }

    #[cfg(windows)]
    #[test]
    fn follows_symlinks_only_inside_the_folder() {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        let f = Fixture::new("symlinks");
        // Creating symlinks needs Developer Mode or admin rights on Windows.
        if symlink_file(f.base.join("secret.txt"), f.root.join("out.txt")).is_err() {
            return;
        }
        symlink_dir(&f.base, f.root.join("up")).unwrap();
        assert_eq!(resolve(&f.root, "/out.txt"), Resolved::NotFound);
        assert_eq!(resolve(&f.root, "/up/secret.txt"), Resolved::NotFound);
    }

    #[test]
    fn parses_ranges() {
        use ByteRange::*;
        assert_eq!(parse_range("bytes=0-99", 1000), Part(0, 99));
        assert_eq!(parse_range("bytes=500-", 1000), Part(500, 999));
        assert_eq!(parse_range("bytes=-100", 1000), Part(900, 999));
        assert_eq!(parse_range("bytes=-5000", 1000), Part(0, 999));
        assert_eq!(parse_range("bytes=900-5000", 1000), Part(900, 999));
        assert_eq!(parse_range("bytes=1000-", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 1000), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), Unsatisfiable);
        assert_eq!(parse_range("bytes=5-1", 1000), Whole);
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), Whole);
        assert_eq!(parse_range("items=0-1", 1000), Whole);
        assert_eq!(parse_range("bytes=abc", 1000), Whole);
    }

    #[test]
    fn builds_links() {
        assert_eq!(href(&[], true), "/");
        assert_eq!(href(&["a b".into(), "c#?".into()], true), "/a%20b/c%23%3F/");
        assert_eq!(content_type(Path::new("x/Movie.MP4")), "video/mp4");
        assert_eq!(content_type(Path::new("noext")), "application/octet-stream");
        assert_eq!(escape_html("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    /// Shares a folder through the real cloudflared and Cloudflare's network:
    /// `cargo test -- --ignored folder_tunnel_end_to_end`
    #[test]
    #[ignore = "needs network access and cloudflared on PATH"]
    fn folder_tunnel_end_to_end() {
        let f = Fixture::new("e2e");
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        let tunnels = Tunnels::default();
        let cfg = TunnelConfig {
            id: "share-e2e".into(),
            name: "e2e".into(),
            target: String::new(),
            rewrite_host: false,
            start_on_open: false,
            kind: TunnelKind::Folder,
            folder: Some(f.root.to_string_lossy().into_owned()),
        };
        let bin = which::which("cloudflared").expect("cloudflared on PATH");
        tauri::async_runtime::block_on(async {
            start(&handle, &tunnels, &cfg, &bin, "auto").await.unwrap();
            assert!(start(&handle, &tunnels, &cfg, &bin, "auto").await.is_err(), "double start is rejected");
            let port = ports()[0];

            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            let url = loop {
                let view = tunnels.view(cfg.clone()).runtime;
                if let (crate::tunnel::Status::Running, Some(url)) = (view.status, view.public_url) {
                    break url;
                }
                assert!(std::time::Instant::now() < deadline, "tunnel never went live: {:?}", tunnels.logs(&cfg.id));
                tokio::time::sleep(Duration::from_millis(200)).await;
            };
            let client = reqwest::Client::new();
            let mut body = String::new();
            for _ in 0..30 {
                if let Ok(r) = client.get(format!("{url}/hello.txt")).send().await {
                    if r.status().is_success() {
                        body = r.text().await.unwrap_or_default();
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            assert_eq!(body, "hello", "the file comes through the public link");
            let hidden = client.get(format!("{url}/.env")).send().await.unwrap();
            assert_eq!(hidden.status(), 404, "dotfiles stay hidden through the link");

            tunnels.stop(&cfg.id);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ports().is_empty() {
                assert!(std::time::Instant::now() < deadline, "the file server keeps running");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let local = reqwest::Client::builder().no_proxy().build().unwrap();
            let refused = local.get(format!("http://127.0.0.1:{port}/hello.txt")).send().await;
            assert!(refused.is_err(), "the file server stops with the tunnel");
        });
    }

    /// Runs the real server and talks HTTP to it.
    #[test]
    fn serves_over_http() {
        let f = Fixture::new("http");
        fs::write(f.root.join("video.mp4"), (0..=255u8).cycle().take(10_000).collect::<Vec<_>>()).unwrap();
        tauri::async_runtime::block_on(async {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let cancel = CancellationToken::new();
            tauri::async_runtime::spawn(serve(listener, Arc::new(f.root.clone()), cancel.clone()));
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let url = |p: &str| format!("http://127.0.0.1:{port}{p}");

            let r = client.get(url("/hello.txt")).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
            assert_eq!(r.text().await.unwrap(), "hello");

            let r = client.get(url("/video.mp4")).header("Range", "bytes=100-199").send().await.unwrap();
            assert_eq!(r.status(), 206);
            assert_eq!(r.headers()["content-range"], "bytes 100-199/10000");
            assert_eq!(r.headers()["content-type"], "video/mp4");
            let body = r.bytes().await.unwrap();
            assert_eq!(body.len(), 100);
            assert_eq!(body[0], 100);

            let r = client.get(url("/video.mp4")).header("Range", "bytes=20000-").send().await.unwrap();
            assert_eq!(r.status(), 416);

            let r = client.head(url("/video.mp4")).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.headers()["content-length"], "10000");

            let r = client.post(url("/hello.txt")).body("x").send().await.unwrap();
            assert_eq!(r.status(), 405);
            assert_eq!(client.get(url("/.env")).send().await.unwrap().status(), 404);
            assert_eq!(client.get(url("/%2e%2e/secret.txt")).send().await.unwrap().status(), 404);

            let r = client.get(url("/")).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert!(r.text().await.unwrap().contains("video.mp4"));

            cancel.cancel();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let refused = client.get(url("/hello.txt")).timeout(Duration::from_secs(2)).send().await;
            assert!(refused.is_err(), "the server stops when cancelled");
        });
    }
}
