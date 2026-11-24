use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use http_body_util::StreamBody;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

use super::*;
use crate::capture::BodyView;

// ---------------------------------------------------------------- unit tests

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
    }
    h
}

#[test]
fn strips_hop_by_hop_headers() {
    let mut h = headers(&[
        ("connection", "keep-alive, x-private"),
        ("keep-alive", "timeout=5"),
        ("x-private", "1"),
        ("transfer-encoding", "chunked"),
        ("content-type", "text/plain"),
    ]);
    strip_hop_by_hop(&mut h, false);
    assert_eq!(h.len(), 1);
    assert!(h.contains_key("content-type"));

    let mut h = headers(&[("connection", "Upgrade"), ("upgrade", "websocket"), ("sec-websocket-key", "k")]);
    assert!(is_upgrade(&h));
    strip_hop_by_hop(&mut h, true);
    assert_eq!(h["connection"], "upgrade");
    assert_eq!(h["upgrade"], "websocket");
    assert_eq!(h["sec-websocket-key"], "k");

    assert!(!is_upgrade(&headers(&[("upgrade", "websocket")])), "Upgrade alone isn't a switch");
}

#[test]
fn lists_headers_with_credentials_hidden() {
    let h = headers(&[
        ("x-b", "2"),
        ("cookie", "s=abc"),
        ("set-cookie", "a=1"),
        ("set-cookie", "b=2"),
        ("cf-connecting-ip", "203.0.113.7"),
    ]);
    let listed = list_headers(&h);
    assert_eq!(
        listed,
        vec![
            ("Cf-Connecting-Ip".to_string(), "203.0.113.7".to_string()),
            ("Cookie".into(), "(hidden, 5 characters)".into()),
            ("Set-Cookie".into(), "(hidden, 8 characters)".into()),
            ("X-B".into(), "2".into()),
        ]
    );
}

#[test]
fn checks_basic_auth() {
    let expected = b"me:s3cret";
    let basic = |s: &str| headers(&[("authorization", &format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(s)))]);
    assert!(auth_matches(&basic("me:s3cret"), expected));
    assert!(auth_matches(&headers(&[("authorization", "basic bWU6czNjcmV0")]), expected), "the scheme is case-insensitive");
    assert!(!auth_matches(&basic("me:wrong"), expected));
    assert!(!auth_matches(&basic("me:s3cret2"), expected));
    assert!(!auth_matches(&basic("you:s3cret"), expected));
    assert!(!auth_matches(&headers(&[("authorization", "Bearer bWU6czNjcmV0")]), expected));
    assert!(!auth_matches(&headers(&[("authorization", "Basic !!!")]), expected));
    assert!(!auth_matches(&HeaderMap::new(), expected));
    // A password may contain colons; only the first one splits the user from it.
    assert!(auth_matches(&basic(":a:b"), b":a:b"));
}

#[test]
fn validates_extra_headers() {
    let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
    let ok = parse_extra_headers(&[pair("X-Frame-Options", "DENY"), pair("", ""), pair(" X-Robots-Tag ", "noindex")]).unwrap();
    assert_eq!(ok.len(), 2);
    assert_eq!(ok[1].0, "x-robots-tag");
    assert!(parse_extra_headers(&[pair("", "value")]).unwrap_err().contains("needs a name"));
    assert!(parse_extra_headers(&[pair("Bad Name", "1")]).is_err());
    assert!(parse_extra_headers(&[pair("Content-Length", "1")]).is_err());
    assert!(parse_extra_headers(&[pair("X-Ok", "line\nbreak")]).is_err());
}

#[test]
fn answers_preflight_requests() {
    let req = headers(&[
        ("origin", "https://app.example"),
        ("access-control-request-method", "PUT"),
        ("access-control-request-headers", "content-type, x-token"),
    ]);
    assert!(is_preflight(&Method::OPTIONS, &req));
    assert!(!is_preflight(&Method::GET, &req));
    assert!(!is_preflight(&Method::OPTIONS, &headers(&[("origin", "https://app.example")])));

    let resp = preflight_response(&req);
    let h = resp.headers();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(h["access-control-allow-origin"], "https://app.example");
    assert_eq!(h["access-control-allow-methods"], "PUT");
    assert_eq!(h["access-control-allow-headers"], "content-type, x-token");
    assert_eq!(h["access-control-allow-credentials"], "true");
}

#[test]
fn adds_cors_headers_to_responses() {
    let origin = HeaderValue::from_static("https://app.example");
    let mut h = headers(&[
        ("access-control-allow-origin", "*"),
        ("content-type", "application/json"),
        ("x-total", "3"),
        ("vary", "Accept-Encoding"),
    ]);
    add_cors_headers(Some(&origin), &mut h);
    assert_eq!(h.get_all("access-control-allow-origin").iter().count(), 1, "the app's value is replaced");
    assert_eq!(h["access-control-allow-origin"], "https://app.example");
    assert_eq!(h["access-control-allow-credentials"], "true");
    assert!(h["access-control-expose-headers"].to_str().unwrap().contains("x-total"));
    let vary: Vec<_> = h.get_all("vary").iter().map(|v| v.to_str().unwrap()).collect();
    assert_eq!(vary, ["Accept-Encoding", "Origin"]);

    let mut plain = headers(&[("content-type", "text/plain")]);
    add_cors_headers(None, &mut plain);
    assert_eq!(plain.len(), 1, "requests without an Origin are left alone");
}

#[tokio::test]
async fn tap_records_the_first_megabyte_and_passes_everything_on() {
    let seen: Arc<Mutex<Option<Recorded>>> = Arc::default();
    let sink = seen.clone();
    let data = vec![7u8; crate::capture::MAX_BODY + 100];
    let tap = Tap::new(full(data.clone()), move |rec| *sink.lock().unwrap() = Some(rec));
    let out = tap.collect().await.unwrap().to_bytes();
    assert_eq!(out.len(), data.len(), "the whole body goes through");
    let rec = seen.lock().unwrap().take().unwrap();
    assert!(rec.complete && rec.truncated());
    assert_eq!((rec.bytes.len(), rec.size), (crate::capture::MAX_BODY, data.len() as u64));

    let seen: Arc<Mutex<Option<Recorded>>> = Arc::default();
    let sink = seen.clone();
    drop(Tap::new(full(Bytes::new()), move |rec| *sink.lock().unwrap() = Some(rec)));
    let rec = seen.lock().unwrap().take().unwrap();
    assert!(rec.complete && rec.size == 0, "an empty body that's never read still counts as complete");
}

// ---------------------------------------------------------------- integration tests

/// A stand-in for the user's app.
async fn app_server(gate: Arc<Notify>) -> SocketAddr {
    serve_app(TcpListener::bind("127.0.0.1:0").await.unwrap(), gate)
}

fn serve_app(listener: TcpListener, gate: Arc<Notify>) -> SocketAddr {
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let gate = gate.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| app(req, gate.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades()
                    .await;
            });
        }
    });
    addr
}

async fn app(mut req: Request<Incoming>, gate: Arc<Notify>) -> Result<Response<ProxyBody>, Infallible> {
    let text = |s: String| {
        let mut r = Response::new(full(s));
        r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain"));
        r
    };
    let resp = match req.uri().path() {
        "/hello" => {
            let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let auth = if req.headers().contains_key("authorization") { "yes" } else { "no" };
            let mut r = text(format!("{} {} host={host} auth={auth}", req.method(), req.uri()));
            r.headers_mut().insert("set-cookie", HeaderValue::from_static("session=abc"));
            r
        }
        "/echo" => {
            let ct = req.headers().get("content-type").cloned();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let mut r = Response::new(full(body));
            if let Some(ct) = ct {
                r.headers_mut().insert("content-type", ct);
            }
            r
        }
        "/stream" => {
            let frames = futures_util::stream::unfold(0u8, move |step| {
                let gate = gate.clone();
                async move {
                    let chunk = match step {
                        0 => "data: one\n\n",
                        1 => {
                            gate.notified().await;
                            "data: two\n\n"
                        }
                        _ => return None,
                    };
                    Some((Ok::<_, BoxError>(Frame::data(Bytes::from(chunk))), step + 1))
                }
            });
            let mut r = Response::new(StreamBody::new(frames).boxed_unsync());
            r.headers_mut().insert("content-type", HeaderValue::from_static("text/event-stream"));
            r
        }
        "/ws" => {
            let on_upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                let Ok(upgraded) = on_upgrade.await else { return };
                let mut io = TokioIo::new(upgraded);
                let mut buf = [0u8; 1024];
                while let Ok(n) = io.read(&mut buf).await {
                    if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            let mut r = Response::new(full(Bytes::new()));
            *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            r.headers_mut().insert("upgrade", HeaderValue::from_static("websocket"));
            r.headers_mut().insert("connection", HeaderValue::from_static("Upgrade"));
            r
        }
        "/big" => {
            let mut r = Response::new(full(vec![b'x'; 2 * 1024 * 1024]));
            r.headers_mut().insert("content-type", HeaderValue::from_static("application/octet-stream"));
            r
        }
        "/cors" => {
            let mut r = text("ok".into());
            r.headers_mut().insert("access-control-allow-origin", HeaderValue::from_static("*"));
            r
        }
        _ => {
            let mut r = text("not found".into());
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        }
    };
    Ok(resp)
}

struct Harness {
    proxy: ProxyHandle,
    events: Arc<Mutex<Vec<Event>>>,
    gate: Arc<Notify>,
    app: SocketAddr,
    client: reqwest::Client,
}

async fn harness(configure: impl FnOnce(&mut ProxyConfig)) -> Harness {
    let gate = Arc::new(Notify::new());
    let app = app_server(gate.clone()).await;
    let mut config = ProxyConfig {
        target: format!("http://127.0.0.1:{}", app.port()),
        ..Default::default()
    };
    configure(&mut config);
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let sink = events.clone();
    let proxy = start(&config, Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    Harness {
        proxy,
        events,
        gate,
        app,
        client,
    }
}

impl Harness {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.proxy.url())
    }

    /// A request as cloudflared would send it.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(path))
            .header("host", "demo.trycloudflare.com")
            .header("x-forwarded-proto", "https")
            .header("cf-connecting-ip", "203.0.113.7")
            .header("cf-ipcountry", "BD")
    }

    async fn entry(&self, done: impl Fn(&RequestEntry) -> bool) -> RequestEntry {
        for _ in 0..100 {
            if let Some(e) = self.proxy.capture().entries().into_iter().rev().find(|e| done(e)) {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no matching entry in {:#?}", self.proxy.capture().entries());
    }

    fn body(&self, id: u64, side: crate::capture::Side) -> BodyView {
        self.proxy.capture().body_view(id, side).unwrap()
    }
}

#[tokio::test]
async fn forwards_requests_and_records_exact_pairs() {
    let h = harness(|_| {}).await;
    let resp = h.get("/hello?x=1").header("cookie", "s=private").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "GET /hello?x=1 host=demo.trycloudflare.com auth=no");

    let e = h.entry(|e| e.response_body.is_some()).await;
    assert_eq!(e.url, "https://demo.trycloudflare.com/hello?x=1");
    assert_eq!((e.method.as_str(), e.status, e.guessed), ("GET", Some(200), false));
    assert_eq!(e.status_text.as_deref(), Some("OK"));
    assert_eq!((e.ip.as_deref(), e.country.as_deref()), (Some("203.0.113.7"), Some("BD")));
    assert_eq!(e.proxied, Some(true));
    assert!(e.headers.contains(&("Cookie".into(), "(hidden, 9 characters)".into())));
    let response_headers = e.response_headers.clone().unwrap();
    assert!(response_headers.contains(&("Set-Cookie".into(), "(hidden, 11 characters)".into())));
    assert!(e.request_body.is_none(), "a GET has no body to show");
    let body = h.body(e.id, crate::capture::Side::Response);
    assert_eq!(body.text.as_deref(), Some("GET /hello?x=1 host=demo.trycloudflare.com auth=no"));

    let json = r#"{"name":"fairy","n":1}"#;
    let resp = h
        .client
        .post(h.url("/echo"))
        .header("content-type", "application/json")
        .body(json)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), json);
    let e = h.entry(|e| e.method == "POST" && e.response_body.is_some() && e.request_body.is_some()).await;
    assert_eq!(e.size, Some(json.len() as u64));
    let req_body = h.body(e.id, crate::capture::Side::Request);
    assert_eq!(req_body.text.as_deref(), Some(json));
    assert_eq!(req_body.info.content_type.as_deref(), Some("application/json"));
    assert!(req_body.info.complete && !req_body.info.truncated);

    // Bodies over 1 MB stream through whole; the list keeps the first 1 MB.
    let big = h.get("/big").send().await.unwrap().bytes().await.unwrap();
    assert_eq!(big.len(), 2 * 1024 * 1024);
    let e = h.entry(|e| e.url.ends_with("/big") && e.response_body.is_some()).await;
    let info = e.response_body.unwrap();
    assert!(info.truncated && info.complete);
    assert_eq!(info.size, 2 * 1024 * 1024);
    assert_eq!(h.body(e.id, crate::capture::Side::Response).text, None, "binary bodies show as type and size");
}

#[tokio::test]
async fn rewrites_the_host_header_when_asked() {
    let h = harness(|c| c.rewrite_host = true).await;
    let text = h.get("/hello").send().await.unwrap().text().await.unwrap();
    assert_eq!(text, format!("GET /hello host=127.0.0.1:{} auth=no", h.app.port()));
    let e = h.entry(|e| e.status.is_some()).await;
    assert_eq!(e.url, "https://demo.trycloudflare.com/hello", "the list shows the public URL");
}

#[tokio::test]
async fn streams_responses_as_they_arrive() {
    let h = harness(|_| {}).await;
    let mut resp = h.get("/stream").send().await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), resp.chunk()).await.expect("first chunk arrives before the stream ends");
    assert_eq!(first.unwrap().unwrap(), "data: one\n\n");
    let e = h.entry(|e| e.status == Some(200)).await;
    assert!(e.response_body.is_none(), "the stream is still open");

    h.gate.notify_one();
    let second = tokio::time::timeout(Duration::from_secs(5), resp.chunk()).await.unwrap();
    assert_eq!(second.unwrap().unwrap(), "data: two\n\n");
    assert!(resp.chunk().await.unwrap().is_none());
    let e = h.entry(|e| e.response_body.is_some()).await;
    assert_eq!(h.body(e.id, crate::capture::Side::Response).text.as_deref(), Some("data: one\n\ndata: two\n\n"));
}

#[tokio::test]
async fn bridges_websocket_upgrades() {
    let h = harness(|_| {}).await;
    let addr = h.proxy.url().trim_start_matches("http://").to_string();
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        b"GET /ws HTTP/1.1\r\nHost: demo.trycloudflare.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
          Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(head.contains("upgrade: websocket"));

    for msg in [&b"ping"[..], b"second message"] {
        sock.write_all(msg).await.unwrap();
        let mut back = vec![0u8; msg.len()];
        tokio::time::timeout(Duration::from_secs(5), sock.read_exact(&mut back)).await.unwrap().unwrap();
        assert_eq!(back, msg);
    }
    let e = h.entry(|e| e.status == Some(101)).await;
    assert!(h.proxy.capture().replay_parts(e.id).is_err(), "upgrades can't be replayed");
}

#[tokio::test]
async fn answers_502_when_nothing_listens() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let sink = events.clone();
    let config = ProxyConfig {
        target: format!("http://localhost:{port}"),
        ..Default::default()
    };
    let proxy = start(&config, Arc::new(move |e| sink.lock().unwrap().push(e))).unwrap();
    let resp = reqwest::Client::builder().no_proxy().build().unwrap().get(proxy.url()).send().await.unwrap();
    assert_eq!(resp.status(), 502);
    assert!(resp.text().await.unwrap().contains(&format!("couldn't connect to localhost:{port}")));
    assert!(events.lock().unwrap().iter().any(|e| matches!(e, Event::Unreachable)));
    let e = proxy.capture().entries().pop().unwrap();
    assert_eq!((e.status, e.error.as_deref()), (None, Some(UNREACHABLE)));
}

#[tokio::test]
async fn asks_for_the_password() {
    let h = harness(|c| {
        c.auth_user = "me".into();
        c.auth_password = "s3cret".into();
    })
    .await;
    let resp = h.get("/hello").send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert!(resp.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic "));
    assert_eq!(h.get("/hello").basic_auth("me", Some("nope")).send().await.unwrap().status(), 401);

    let resp = h.get("/hello").basic_auth("me", Some("s3cret")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().ends_with("auth=no"), "the tunnel's password isn't passed to the app");
    let wrong = h.entry(|e| e.status == Some(401)).await;
    assert!(wrong.headers.contains(&("Authorization".into(), "(hidden, 18 characters)".into())));
    let ok = h.entry(|e| e.status == Some(200)).await;
    assert!(ok.headers.iter().all(|(n, _)| n != "Authorization"), "the matched password isn't listed");
}

#[tokio::test]
async fn handles_cors_and_extra_headers() {
    let h = harness(|c| {
        c.cors = true;
        c.extra_headers = vec![("X-Frame-Options".into(), "DENY".into())];
    })
    .await;
    let resp = h
        .client
        .request(reqwest::Method::OPTIONS, h.url("/hello"))
        .header("origin", "https://app.example")
        .header("access-control-request-method", "DELETE")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.headers()["access-control-allow-origin"], "https://app.example");
    assert_eq!(resp.headers()["access-control-allow-methods"], "DELETE");

    let resp = h.get("/cors").header("origin", "https://app.example").send().await.unwrap();
    assert_eq!(resp.headers()["access-control-allow-origin"], "https://app.example");
    assert_eq!(resp.headers()["access-control-allow-credentials"], "true");
    assert_eq!(resp.headers()["x-frame-options"], "DENY");
}

#[tokio::test]
async fn replays_a_request() {
    let h = harness(|_| {}).await;
    h.client
        .post(h.url("/echo"))
        .header("host", "demo.trycloudflare.com")
        .header("content-type", "text/plain")
        .body("again")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let original = h.entry(|e| e.request_body.is_some() && e.response_body.is_some()).await;
    let replayed = h.proxy.replay(original.id).await.unwrap();
    assert_eq!(replayed.replay_of, Some(original.id));
    assert_eq!((replayed.method.as_str(), replayed.status), ("POST", Some(200)));
    assert_eq!(replayed.url, original.url);
    let done = h.entry(|e| e.replay_of.is_some() && e.response_body.is_some()).await;
    assert_eq!(h.body(done.id, crate::capture::Side::Response).text.as_deref(), Some("again"));
    assert!(h.events.lock().unwrap().iter().any(|e| matches!(e, Event::Request(r) if r.replay_of.is_some())));
}

#[tokio::test]
async fn replays_a_webhook_that_arrived_while_the_app_was_down() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let config = ProxyConfig {
        target: format!("http://127.0.0.1:{port}"),
        ..Default::default()
    };
    let proxy = start(&config, Arc::new(|_| {})).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let resp = client
        .post(format!("{}/echo", proxy.url()))
        .header("content-type", "application/json")
        .body(r#"{"event":"paid"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let missed = proxy.capture().entries().pop().unwrap();
    assert_eq!(missed.error.as_deref(), Some(UNREACHABLE));
    assert!(missed.request_body.as_ref().is_some_and(|b| b.complete), "the body was kept anyway");

    serve_app(TcpListener::bind(("127.0.0.1", port)).await.unwrap(), Arc::new(Notify::new()));
    let replayed = proxy.replay(missed.id).await.unwrap();
    assert_eq!(replayed.status, Some(200));
    for _ in 0..100 {
        let e = proxy.capture().entries().pop().unwrap();
        if e.response_body.is_some() {
            let body = proxy.capture().body_view(e.id, crate::capture::Side::Response).unwrap();
            assert_eq!(body.text.as_deref(), Some(r#"{"event":"paid"}"#));
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the replay's response body wasn't recorded");
}

/// A throwaway self-signed certificate for `localhost`, made for these tests only.
const TEST_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBnDCCAUGgAwIBAgIUS0BSMPPjdPuSPkYEGmkq16DVAQYwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTEyNDEyNDIyOFoYDzIxMjYxMDMx
MTI0MjI4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAT+db8+EAx6kuR7LU32Z6YtLS/3KhjXO8q8vO9s+tmaKZroVaHmmrFw
Ag+oGbkr18eWQwJZ8AX0+yOwSo3+xox3o28wbTAdBgNVHQ4EFgQURigSKZ2z+HeZ
mgB+yHP/GpOrj8UwHwYDVR0jBBgwFoAURigSKZ2z+HeZmgB+yHP/GpOrj8UwDwYD
VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI
zj0EAwIDSQAwRgIhALqXgoP7G9zDJR+9RLj+orr/UkCD61DG347ROq3V2SoqAiEA
+HyCRmSF6TiQ2MSyyXcqVYh60y2Z5G6Dh3qP/DBCjaU=
-----END CERTIFICATE-----
";
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgm/CdtoDZE97i0Xj7
+kErTP3LMGFezuTX3/TmSpFwCrShRANCAAT+db8+EAx6kuR7LU32Z6YtLS/3KhjX
O8q8vO9s+tmaKZroVaHmmrFwAg+oGbkr18eWQwJZ8AX0+yOwSo3+xox3
-----END PRIVATE KEY-----
";

#[tokio::test]
async fn reaches_https_targets_with_self_signed_certificates() {
    use rustls::pki_types::{pem::PemObject, PrivateKeyDer};

    let cert = CertificateDer::from_pem_slice(TEST_CERT.as_bytes()).unwrap();
    let key = PrivateKeyDer::from_pem_slice(TEST_KEY.as_bytes()).unwrap();
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let gate = Arc::new(Notify::new());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, gate) = (acceptor.clone(), gate.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else { return };
                let svc = service_fn(move |req| app(req, gate.clone()));
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), svc)
                    .await;
            });
        }
    });

    let config = ProxyConfig {
        target: format!("https://localhost:{port}"),
        ..Default::default()
    };
    let proxy = start(&config, Arc::new(|_| {})).unwrap();
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/hello", proxy.url()))
        .header("host", "demo.trycloudflare.com")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "GET /hello host=demo.trycloudflare.com auth=no");

    // Other hosts are checked against the system's certificates, as cloudflared does.
    assert!(tls_config(true).is_ok());
}

#[tokio::test]
async fn stops_listening_and_closes_streams_when_dropped() {
    let h = harness(|_| {}).await;
    let mut resp = h.get("/stream").send().await.unwrap();
    resp.chunk().await.unwrap();
    let addr = h.proxy.url().trim_start_matches("http://").to_string();
    drop(h.proxy);

    let next = tokio::time::timeout(Duration::from_secs(5), resp.chunk()).await.expect("the open stream closes");
    assert!(!matches!(next, Ok(Some(_))), "no more data after the proxy stops");
    let mut refused = false;
    for _ in 0..50 {
        if TcpStream::connect(&addr).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "the listener is closed");
}
