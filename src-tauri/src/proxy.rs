//! A local reverse proxy between cloudflared and the user's app.
//!
//! With "Route tunnels through Cloudfairy" on, each running tunnel gets a listener on
//! `127.0.0.1:<random port>`. cloudflared's `--url` points at it, and the proxy forwards every
//! request to the real target. Bodies stream both ways, and only the first 1 MB of each is
//! copied for the request list (see `capture.rs`). WebSocket and other `Upgrade` requests are
//! joined end to end once the app agrees to switch protocols.
//!
//! On the way through, the proxy can ask for a password, answer CORS preflight requests and add
//! response headers. Like cloudflared, it only uses the target's scheme and host: the request
//! path is passed on as the visitor sent it.
//!
//! Dropping the [`ProxyHandle`] closes the listener and every open connection.

use std::{
    convert::Infallible,
    error::Error as StdError,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::{Body, Frame, Incoming, SizeHint},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    http::request::Parts,
    service::service_fn,
    upgrade::OnUpgrade,
    Method, Request, Response, StatusCode, Uri, Version,
};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use subtle::ConstantTimeEq;
use tokio::sync::watch;

use crate::{
    capture::{Capture, Recorded, Side, Template, MAX_BODY},
    requests::{hidden, is_secret, RequestEntry},
    settings::TunnelConfig,
    tunnel,
};

type BoxError = Box<dyn StdError + Send + Sync>;
pub type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;
type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

/// What the request list says when nothing answers at the target. `requests.rs` uses the same.
const UNREACHABLE: &str = "Your server didn't answer";

/// What a tunnel asks of its proxy.
#[derive(Clone, Debug, Default)]
pub struct ProxyConfig {
    /// The normalized target, like `http://localhost:3000`.
    pub target: String,
    pub rewrite_host: bool,
    pub auth_user: String,
    /// Empty means no password.
    pub auth_password: String,
    pub cors: bool,
    pub extra_headers: Vec<(String, String)>,
}

impl ProxyConfig {
    pub fn from_tunnel(cfg: &TunnelConfig) -> Self {
        Self {
            target: cfg.target.clone(),
            rewrite_host: cfg.rewrite_host,
            auth_user: cfg.auth_user.clone(),
            auth_password: cfg.auth_password.clone(),
            cors: cfg.cors,
            extra_headers: cfg
                .extra_headers
                .iter()
                .map(|h| (h.name.clone(), h.value.clone()))
                .collect(),
        }
    }
}

/// What the proxy tells its tunnel.
#[derive(Clone, Debug)]
pub enum Event {
    /// A request was added to the list, or changed.
    Request(Box<RequestEntry>),
    /// Nothing accepted a connection at the target.
    Unreachable,
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// A running proxy. Dropping it stops it.
pub struct ProxyHandle {
    addr: SocketAddr,
    shared: Arc<Shared>,
    /// Never sent on. Dropping it wakes every task waiting on `Shared::stop`.
    _stop: watch::Sender<()>,
}

impl ProxyHandle {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn capture(&self) -> Arc<Capture> {
        self.shared.capture.clone()
    }

    /// Sends recorded request `id` to the target again. The future doesn't borrow the handle,
    /// so it can be awaited after a lock around the handle is released.
    pub fn replay(&self, id: u64) -> impl Future<Output = Result<RequestEntry, String>> + Send + 'static {
        replay(self.shared.clone(), id)
    }
}

struct Target {
    scheme: &'static str,
    /// `host[:port]`, as a browser sends it in the `Host` header.
    authority: String,
    host_header: HeaderValue,
    /// `host:port`, for messages.
    label: String,
}

impl Target {
    fn parse(url: &str) -> Result<(Self, String), String> {
        let scheme = if url.starts_with("https://") {
            "https"
        } else if url.starts_with("http://") {
            "http"
        } else {
            return Err("Only http:// and https:// addresses are supported.".into());
        };
        let bad = || format!("{url} isn't a valid address.");
        let authority = tunnel::host_header(url).ok_or_else(bad)?;
        let (host, port) = tunnel::host_port(url).ok_or_else(bad)?;
        let host_header = HeaderValue::from_str(&authority).map_err(|_| bad())?;
        let target = Self {
            scheme,
            authority,
            host_header,
            label: format!("{host}:{port}"),
        };
        Ok((target, host))
    }

    fn uri(&self, path: &str) -> Option<Uri> {
        format!("{}://{}{}", self.scheme, self.authority, path).parse().ok()
    }
}

struct Shared {
    target: Target,
    rewrite_host: bool,
    /// `user:password`, as Basic auth sends it.
    auth: Option<Vec<u8>>,
    cors: bool,
    extra_headers: Vec<(HeaderName, HeaderValue)>,
    client: Client<Connector, ProxyBody>,
    capture: Arc<Capture>,
    sink: Sink,
    stop: watch::Receiver<()>,
}

impl Shared {
    fn emit(&self, entry: RequestEntry) {
        (self.sink)(Event::Request(Box::new(entry)));
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut RequestEntry)) {
        if let Some(entry) = self.capture.update(id, f) {
            self.emit(entry);
        }
    }

    fn body_done(&self, id: u64, side: Side, rec: Recorded, headers: &HeaderMap) {
        // Most requests have no body; an update for each would only add noise.
        if side == Side::Request && rec.size == 0 && rec.complete {
            return;
        }
        if let Some(entry) = self.capture.store_body(id, side, rec, headers) {
            self.emit(entry);
        }
    }

    /// CORS and extra headers, on every response the visitor gets.
    fn decorate(&self, origin: Option<&HeaderValue>, headers: &mut HeaderMap) {
        if self.cors {
            add_cors_headers(origin, headers);
        }
        for (name, value) in &self.extra_headers {
            headers.insert(name.clone(), value.clone());
        }
    }
}

/// Starts a proxy for `config` on a free loopback port. Must run inside a tokio runtime.
pub fn start(config: &ProxyConfig, sink: Sink) -> Result<ProxyHandle, String> {
    let (target, host) = Target::parse(&config.target)?;
    let client = build_client(target.scheme, &host)?;
    let extra_headers = parse_extra_headers(&config.extra_headers)?;
    let auth = (!config.auth_password.is_empty())
        .then(|| format!("{}:{}", config.auth_user, config.auth_password).into_bytes());

    let fail = |e: std::io::Error| format!("Couldn't open a local port for Cloudfairy's proxy: {e}");
    let std_listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(fail)?;
    std_listener.set_nonblocking(true).map_err(fail)?;
    let addr = std_listener.local_addr().map_err(fail)?;
    let listener = tokio::net::TcpListener::from_std(std_listener).map_err(fail)?;

    let (stop_tx, stop) = watch::channel(());
    let shared = Arc::new(Shared {
        target,
        rewrite_host: config.rewrite_host,
        auth,
        cors: config.cors,
        extra_headers,
        client,
        capture: Arc::new(Capture::default()),
        sink,
        stop,
    });
    tokio::spawn(accept_loop(listener, shared.clone()));
    Ok(ProxyHandle {
        addr,
        shared,
        _stop: stop_tx,
    })
}

fn build_client(scheme: &str, host: &str) -> Result<Client<Connector, ProxyBody>, String> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config(scheme == "https" && !tunnel::is_loopback(host))?)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_timer(TokioTimer::new())
        .build(https))
}

/// cloudflared runs with `--no-tls-verify` for https targets on this computer, because local
/// dev servers almost always use self-signed certificates. Other hosts are checked against
/// the system's trusted certificates, as cloudflared does.
fn tls_config(verify: bool) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    if verify {
        use rustls_platform_verifier::BuilderVerifierExt;
        let builder = builder.with_platform_verifier().map_err(|e| e.to_string())?;
        Ok(builder.with_no_client_auth())
    } else {
        Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth())
    }
}

/// Accepts any certificate but still checks the handshake signatures.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn accept_loop(listener: tokio::net::TcpListener, shared: Arc<Shared>) {
    let mut stop = shared.stop.clone();
    loop {
        let stream = tokio::select! {
            _ = stop.changed() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => {
                    // Out of file descriptors, say. Don't spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        tokio::spawn(serve_connection(stream, shared.clone()));
    }
}

async fn serve_connection(stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let mut stop = shared.stop.clone();
    let service = service_fn(move |req| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handle(shared, req).await) }
    });
    // Only cloudflared connects here, so idle keep-alive connections are left open for it.
    let conn = hyper::server::conn::http1::Builder::new()
        .header_read_timeout(None::<Duration>)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::select! {
        _ = conn => {}
        _ = stop.changed() => {}
    }
}

async fn handle(shared: Arc<Shared>, mut req: Request<Incoming>) -> Response<ProxyBody> {
    let origin = req.headers().get(header::ORIGIN).cloned();
    if shared.cors && is_preflight(req.method(), req.headers()) {
        let mut resp = preflight_response(req.headers());
        shared.decorate(None, resp.headers_mut());
        record_local(&shared, &req, &resp);
        return resp;
    }
    if let Some(expected) = &shared.auth {
        if !auth_matches(req.headers(), expected) {
            let mut resp = unauthorized();
            shared.decorate(origin.as_ref(), resp.headers_mut());
            record_local(&shared, &req, &resp);
            return resp;
        }
        // The tunnel's password is for Cloudfairy, not for the app.
        req.headers_mut().remove(header::AUTHORIZATION);
    }
    let started = Instant::now();
    let client_upgrade = is_upgrade(req.headers()).then(|| hyper::upgrade::on(&mut req));
    let small = content_length(req.headers()).is_some_and(|n| n > 0 && n <= MAX_BODY as u64);
    let (parts, body) = req.into_parts();
    // A small body is read in full before it's sent on, so the request can be replayed even
    // when the app was down. Bigger and chunked bodies stream.
    let body = if small && client_upgrade.is_none() {
        match body.collect().await {
            Ok(collected) => Outgoing::Buffered(collected.to_bytes()),
            Err(_) => return plain(StatusCode::BAD_REQUEST, "The request body didn't arrive in full.\n"),
        }
    } else {
        Outgoing::Stream(body.map_err(BoxError::from).boxed_unsync())
    };
    forward(&shared, parts, body, client_upgrade, None, started).await.1
}

/// A request body on its way to the app.
enum Outgoing {
    Stream(ProxyBody),
    Buffered(Bytes),
}

/// Lists a request the proxy answered itself (a preflight or a 401).
fn record_local(shared: &Shared, req: &Request<Incoming>, resp: &Response<ProxyBody>) {
    let path = path_of(req.uri());
    let mut entry = new_entry(req.method(), req.headers(), &path, None);
    entry.status = Some(resp.status().as_u16());
    entry.status_text = resp.status().canonical_reason().map(str::to_string);
    entry.duration_ms = Some(0);
    entry.response_headers = Some(list_headers(resp.headers()));
    let template = Template {
        method: req.method().clone(),
        path,
        headers: req.headers().clone(),
        upgrade: false,
    };
    let entry = shared.capture.begin(entry, template);
    shared.emit(entry);
}

/// Sends one request to the target and records it. Returns the entry id and the response.
async fn forward(
    shared: &Arc<Shared>,
    mut parts: Parts,
    body: Outgoing,
    client_upgrade: Option<OnUpgrade>,
    replay_of: Option<u64>,
    started: Instant,
) -> (u64, Response<ProxyBody>) {
    let upgrade = client_upgrade.is_some();
    let origin = parts.headers.get(header::ORIGIN).cloned();
    let path = path_of(&parts.uri);
    let entry = new_entry(&parts.method, &parts.headers, &path, replay_of);
    let template = Template {
        method: parts.method.clone(),
        path: path.clone(),
        headers: parts.headers.clone(),
        upgrade,
    };
    let entry = shared.capture.begin(entry, template);
    let id = entry.id;
    shared.emit(entry);

    let body_headers = content_headers(&parts.headers);
    let body = match body {
        Outgoing::Buffered(bytes) => {
            let mut rec = Recorded::default();
            rec.push(&bytes);
            rec.complete = true;
            shared.body_done(id, Side::Request, rec, &body_headers);
            full(bytes)
        }
        Outgoing::Stream(body) => {
            let shared = shared.clone();
            Tap::new(body, move |rec| shared.body_done(id, Side::Request, rec, &body_headers)).boxed_unsync()
        }
    };
    let Some(uri) = shared.target.uri(&path) else {
        shared.update(id, |e| e.error = Some("Cloudfairy couldn't build the address for this request".into()));
        return (id, bad_gateway(shared, origin.as_ref(), false));
    };
    strip_hop_by_hop(&mut parts.headers, upgrade);
    if shared.rewrite_host {
        parts.headers.insert(header::HOST, shared.target.host_header.clone());
    }
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    let request = Request::from_parts(parts, body);

    // If the visitor goes away before the app answers, hyper drops this future.
    let mut pending = Pending {
        shared: shared.clone(),
        id,
        started,
        armed: true,
    };
    let result = shared.client.request(request).await;
    pending.armed = false;

    let mut resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            let unreachable = e.is_connect();
            if unreachable {
                (shared.sink)(Event::Unreachable);
            }
            let error = if unreachable {
                UNREACHABLE.to_string()
            } else {
                format!("Your server didn't send a response ({})", root_cause(&e))
            };
            shared.update(id, |e| {
                e.error = Some(error);
                e.duration_ms = Some(elapsed_ms(started));
            });
            return (id, bad_gateway(shared, origin.as_ref(), unreachable));
        }
    };

    let switching = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
    if let (true, Some(client)) = (switching, client_upgrade) {
        let upstream = hyper::upgrade::on(&mut resp);
        tokio::spawn(bridge(client, upstream, shared.stop.clone()));
    }
    let status_text = resp
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .and_then(|r| std::str::from_utf8(r.as_bytes()).ok().map(str::to_string))
        .or_else(|| resp.status().canonical_reason().map(str::to_string));
    let (mut rparts, rbody) = resp.into_parts();
    strip_hop_by_hop(&mut rparts.headers, switching && upgrade);
    shared.decorate(origin.as_ref(), &mut rparts.headers);
    let status = rparts.status.as_u16();
    let listed = list_headers(&rparts.headers);
    shared.update(id, |e| {
        e.status = Some(status);
        e.status_text = status_text;
        e.duration_ms = Some(elapsed_ms(started));
        e.response_headers = Some(listed);
    });

    let body_headers = content_headers(&rparts.headers);
    let tapped = {
        let shared = shared.clone();
        Tap::new(rbody.map_err(BoxError::from).boxed_unsync(), move |rec| {
            shared.body_done(id, Side::Response, rec, &body_headers)
        })
    };
    (id, Response::from_parts(rparts, tapped.boxed_unsync()))
}

/// Marks a request as canceled when it's dropped before the app answered.
struct Pending {
    shared: Arc<Shared>,
    id: u64,
    started: Instant,
    armed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.armed {
            let ms = elapsed_ms(self.started);
            self.shared.update(self.id, |e| {
                e.error = Some("Canceled by the visitor".into());
                e.duration_ms = Some(ms);
            });
        }
    }
}

/// Copies bytes both ways once the visitor and the app have both switched protocols.
async fn bridge(client: OnUpgrade, upstream: OnUpgrade, mut stop: watch::Receiver<()>) {
    let joined = async {
        let (Ok(client), Ok(upstream)) = tokio::join!(client, upstream) else { return };
        let mut client = TokioIo::new(client);
        let mut upstream = TokioIo::new(upstream);
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    };
    tokio::select! {
        _ = joined => {}
        _ = stop.changed() => {}
    }
}

async fn replay(shared: Arc<Shared>, of: u64) -> Result<RequestEntry, String> {
    let (template, body, _) = shared.capture.replay_parts(of)?;
    let uri: Uri = template.path.parse().map_err(|_| "That request's path can't be sent again.")?;
    let mut req = Request::new(());
    *req.method_mut() = template.method;
    *req.uri_mut() = uri;
    *req.headers_mut() = template.headers;
    let (parts, ()) = req.into_parts();
    let (id, resp) = forward(&shared, parts, Outgoing::Buffered(body), None, Some(of), Instant::now()).await;
    // Read the response so its body is recorded. Nothing waits for it.
    let mut stop = shared.stop.clone();
    tokio::spawn(async move {
        let mut body = resp.into_body();
        let drain = async {
            while let Some(frame) = body.frame().await {
                if frame.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = drain => {}
            _ = stop.changed() => {}
        }
    });
    shared
        .capture
        .update(id, |_| {})
        .ok_or_else(|| "The replayed request is no longer in the list.".into())
}

/// Passes body frames through and keeps the first 1 MB. Calls `done` once, when the body
/// ends or is dropped.
struct Tap {
    inner: ProxyBody,
    rec: Recorded,
    done: Option<Box<dyn FnOnce(Recorded) + Send>>,
}

impl Tap {
    fn new(inner: ProxyBody, done: impl FnOnce(Recorded) + Send + 'static) -> Self {
        Self {
            inner,
            rec: Recorded::default(),
            done: Some(Box::new(done)),
        }
    }

    fn finish(&mut self, complete: bool) {
        if let Some(done) = self.done.take() {
            let mut rec = std::mem::take(&mut self.rec);
            rec.complete = complete;
            done(rec);
        }
    }
}

impl Body for Tap {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.rec.push(data);
                }
                if this.inner.is_end_stream() {
                    this.finish(true);
                }
            }
            Poll::Ready(Some(Err(_))) => this.finish(false),
            Poll::Ready(None) => this.finish(true),
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // An empty body may never be polled at all.
        let ended = self.inner.is_end_stream();
        self.finish(ended);
    }
}

// ---------------------------------------------------------------- headers

fn new_entry(method: &Method, headers: &HeaderMap, path: &str, replay_of: Option<u64>) -> RequestEntry {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let host = text("host").unwrap_or_else(|| "localhost".into());
    // cloudflared says how the visitor connected. Public links are always https.
    let scheme = match text("x-forwarded-proto").as_deref().and_then(|p| p.split(',').next()) {
        Some(p) if p.trim().eq_ignore_ascii_case("http") => "http",
        _ => "https",
    };
    RequestEntry {
        at: now_ms(),
        method: method.to_string(),
        url: format!("{scheme}://{host}{path}"),
        size: text("content-length").and_then(|n| n.parse().ok()).filter(|&n| n > 0),
        ip: text("cf-connecting-ip"),
        country: text("cf-ipcountry").filter(|c| c != "XX"),
        user_agent: text("user-agent"),
        headers: list_headers(headers),
        proxied: Some(true),
        replay_of,
        ..Default::default()
    }
}

fn path_of(uri: &Uri) -> String {
    uri.path_and_query().map_or("/", |pq| pq.as_str()).to_string()
}

/// Headers for the request list: `Title-Case` names in name order, repeated headers joined,
/// and credentials hidden.
fn list_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .keys()
        .map(|name| {
            let value = headers
                .get_all(name)
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            let title = title_case(name.as_str());
            let secret = is_secret(&title) || name == header::SET_COOKIE;
            let value = if secret { hidden(&value) } else { value };
            (title, value)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn title_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// The headers `capture.rs` needs to describe a body.
fn content_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::CONTENT_ENCODING] {
        if let Some(v) = headers.get(&name) {
            out.insert(name, v.clone());
        }
    }
    out
}

fn connection_has(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim().eq_ignore_ascii_case(token))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE) && connection_has(headers, "upgrade")
}

/// Removes headers that only apply to one connection. For a protocol switch it keeps
/// `Upgrade` and sends `Connection: upgrade` on.
fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let upgrade = headers.get(header::UPGRADE).cloned();
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        headers.remove(name);
    }
    for name in ["connection", "keep-alive", "proxy-connection", "te", "trailer", "transfer-encoding", "upgrade"] {
        headers.remove(name);
    }
    if let (true, Some(upgrade)) = (keep_upgrade, upgrade) {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, upgrade);
    }
}

/// Response headers a tunnel can add. Rejects names and values HTTP doesn't allow, and
/// headers that would break the connection. Rows with no name and no value are skipped.
pub fn parse_extra_headers(pairs: &[(String, String)]) -> Result<Vec<(HeaderName, HeaderValue)>, String> {
    const RESERVED: [&str; 7] = [
        "connection",
        "content-length",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    let mut out = Vec::new();
    for (name, value) in pairs {
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            if value.is_empty() {
                continue;
            }
            return Err(format!("The header with the value \"{value}\" needs a name."));
        }
        let parsed = HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("\"{name}\" isn't a valid header name."))?;
        if RESERVED.contains(&parsed.as_str()) {
            return Err(format!("Cloudfairy can't set the {name} header."));
        }
        let value = HeaderValue::from_str(value).map_err(|_| format!("The value of {name} has characters a header can't hold."))?;
        out.push((parsed, value));
    }
    Ok(out)
}

// ---------------------------------------------------------------- password

/// True when the request carries Basic credentials equal to `expected` (`user:password`).
/// The comparison takes the same time wherever the first difference is.
fn auth_matches(headers: &HeaderMap, expected: &[u8]) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some((scheme, token)) = value.trim().split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return false;
    }
    let Ok(given) = base64::engine::general_purpose::STANDARD.decode(token.trim()) else {
        return false;
    };
    given.ct_eq(expected).into()
}

fn unauthorized() -> Response<ProxyBody> {
    let mut resp = Response::new(full("This link is password protected.\n"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    let h = resp.headers_mut();
    h.insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Cloudfairy\", charset=\"UTF-8\""),
    );
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

// ---------------------------------------------------------------- CORS

fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Allows whatever the browser asks for, from the origin that asks.
fn preflight_response(req: &HeaderMap) -> Response<ProxyBody> {
    let mut resp = Response::new(full(Bytes::new()));
    *resp.status_mut() = StatusCode::NO_CONTENT;
    let h = resp.headers_mut();
    if let Some(origin) = req.get(header::ORIGIN) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    h.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    let methods = req
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, methods);
    if let Some(asked) = req.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, asked.clone());
    }
    if req.contains_key("access-control-request-private-network") {
        h.insert("access-control-allow-private-network", HeaderValue::from_static("true"));
    }
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    h.insert(
        header::VARY,
        HeaderValue::from_static("Origin, Access-Control-Request-Method, Access-Control-Request-Headers"),
    );
    resp
}

/// Lets the requesting origin read the response, cookies included. Replaces CORS headers
/// the app set, since two `Access-Control-Allow-Origin` values make browsers refuse it.
fn add_cors_headers(origin: Option<&HeaderValue>, headers: &mut HeaderMap) {
    let Some(origin) = origin else { return };
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    headers.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
    if !headers.contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS) {
        // "*" isn't a wildcard on requests with credentials, so name the headers.
        let names: Vec<&str> = headers
            .keys()
            .map(HeaderName::as_str)
            .filter(|n| !n.starts_with("access-control-") && *n != "set-cookie")
            .collect();
        if let Ok(v) = HeaderValue::from_str(&names.join(", ")) {
            if !names.is_empty() {
                headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, v);
            }
        }
    }
    let varies_by_origin = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| matches!(t.trim(), "*") || t.trim().eq_ignore_ascii_case("origin"));
    if !varies_by_origin {
        headers.append(header::VARY, HeaderValue::from_static("Origin"));
    }
}

// ---------------------------------------------------------------- misc

/// The page a visitor sees when the app doesn't answer.
fn bad_gateway(shared: &Shared, origin: Option<&HeaderValue>, unreachable: bool) -> Response<ProxyBody> {
    let target = &shared.target.label;
    let text = if unreachable {
        format!(
            "The app behind this link isn't answering right now.\n\n\
             Cloudfairy couldn't connect to {target} on the computer that shared it.\n"
        )
    } else {
        format!(
            "The app behind this link didn't send a response.\n\n\
             Cloudfairy reached {target} on the computer that shared it, but the request failed.\n"
        )
    };
    let mut resp = Response::new(full(text));
    *resp.status_mut() = StatusCode::BAD_GATEWAY;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    shared.decorate(origin, h);
    resp
}

fn full(body: impl Into<Bytes>) -> ProxyBody {
    Full::new(body.into()).map_err(|never| match never {}).boxed_unsync()
}

fn plain(status: StatusCode, text: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(full(text));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(header::CONTENT_LENGTH)?.to_str().ok()?.trim().parse().ok()
}

fn root_cause(e: &(dyn StdError + 'static)) -> String {
    let mut cause = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
