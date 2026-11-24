//! Exports a tunnel's request list as a HAR 1.2 file (http://www.softwareishard.com/blog/har-12-spec/).
//!
//! The request list doesn't record bodies or response headers yet. `post_data`, `response_headers`
//! and `response_content` are where they go once `RequestEntry` has them; the rest of the file
//! stays the same.

use std::collections::HashSet;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::capture::{BodyView, Side};
use crate::requests::{self, RequestEntry};
use crate::AppState;

#[derive(Serialize)]
pub struct Har {
    pub log: Log,
}

#[derive(Serialize)]
pub struct Log {
    pub version: &'static str,
    pub creator: Creator,
    pub entries: Vec<Entry>,
    pub comment: String,
}

#[derive(Serialize)]
pub struct Creator {
    pub name: &'static str,
    pub version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub started_date_time: String,
    pub time: u64,
    pub request: Request,
    pub response: Response,
    pub cache: Cache,
    pub timings: Timings,
    /// Why the request failed, when cloudflared logged a failure for it.
    #[serde(rename = "_error", skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The response was matched by order and may belong to a neighbouring request.
    #[serde(rename = "_guessed", skip_serializing_if = "std::ops::Not::not")]
    pub guessed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub method: String,
    pub url: String,
    pub http_version: &'static str,
    pub cookies: Vec<Header>,
    pub headers: Vec<Header>,
    pub query_string: Vec<Header>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_data: Option<PostData>,
    pub headers_size: i64,
    pub body_size: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostData {
    pub mime_type: String,
    pub text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub http_version: &'static str,
    pub cookies: Vec<Header>,
    pub headers: Vec<Header>,
    pub content: Content,
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,
    pub headers_size: i64,
    pub body_size: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub size: i64,
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `base64` when `text` holds binary data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

/// A name and value pair. HAR uses the same shape for headers, cookies and query parameters.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct Cache {}

#[derive(Serialize)]
pub struct Timings {
    pub send: u64,
    pub wait: u64,
    pub receive: u64,
}

/// cloudflared logs every proxied request as HTTP/1.1.
const HTTP_VERSION: &str = "HTTP/1.1";
const UNKNOWN_MIME: &str = "x-unknown";
const COMMENT: &str = "Exported by Cloudfairy from cloudflared's debug log. Credential headers \
    (Authorization, Cookie and similar) are left out. Bodies and response headers aren't recorded.";

/// Builds a HAR log from request list entries, oldest first.
/// Looks up a recorded body by request id. The proxy keeps bodies apart from the list.
pub type Bodies<'a> = &'a dyn Fn(u64, Side) -> Option<BodyView>;

#[cfg(test)]
pub fn build(entries: &[RequestEntry], app_version: &str) -> Har {
    build_with(entries, app_version, &|_, _| None)
}

pub fn build_with(entries: &[RequestEntry], app_version: &str, bodies: Bodies) -> Har {
    let mut entries: Vec<&RequestEntry> = entries.iter().collect();
    entries.sort_by_key(|e| (e.at, e.id));
    Har {
        log: Log {
            version: "1.2",
            creator: Creator { name: "Cloudfairy", version: app_version.into() },
            entries: entries.into_iter().map(|e| entry(e, bodies)).collect(),
            comment: COMMENT.into(),
        },
    }
}

fn entry(e: &RequestEntry, bodies: Bodies) -> Entry {
    let time = e.duration_ms.unwrap_or(0);
    Entry {
        started_date_time: iso_time(e.at),
        time,
        request: Request {
            method: e.method.clone(),
            url: e.url.clone(),
            http_version: HTTP_VERSION,
            cookies: Vec::new(),
            headers: request_headers(e),
            query_string: query_string(&e.url),
            post_data: post_data(e, bodies),
            headers_size: -1,
            body_size: e.size.map_or(-1, |n| n as i64),
        },
        response: Response {
            // HAR tools read status 0 as "no response", the same as browsers write for failures.
            status: e.status.unwrap_or(0),
            status_text: e.status_text.clone().unwrap_or_default(),
            http_version: if e.status.is_some() { HTTP_VERSION } else { "" },
            cookies: Vec::new(),
            headers: response_headers(e),
            content: response_content(e, bodies),
            redirect_url: String::new(),
            headers_size: -1,
            body_size: -1,
        },
        cache: Cache {},
        // Only the total is known, so it all counts as waiting for the server.
        timings: Timings { send: 0, wait: time, receive: 0 },
        error: e.error.clone(),
        guessed: e.guessed,
    }
}

/// Request headers without credentials. The request list only has their hidden placeholders.
fn request_headers(e: &RequestEntry) -> Vec<Header> {
    e.headers
        .iter()
        .filter(|(name, _)| !requests::is_secret(name))
        .map(|(name, value)| Header { name: name.clone(), value: value.clone() })
        .collect()
}

/// The request body, when the proxy recorded it as text.
fn post_data(e: &RequestEntry, bodies: Bodies) -> Option<PostData> {
    e.request_body.as_ref()?;
    let view = bodies(e.id, Side::Request)?;
    Some(PostData { mime_type: mime(&view), text: view.text? })
}

/// Response headers the proxy recorded, without credentials or cookies.
fn response_headers(e: &RequestEntry) -> Vec<Header> {
    e.response_headers
        .iter()
        .flatten()
        .filter(|(name, _)| !requests::is_secret(name) && !name.eq_ignore_ascii_case("set-cookie"))
        .map(|(name, value)| Header { name: name.clone(), value: value.clone() })
        .collect()
}

/// The response body. Text bodies are included; binary ones only give their size.
fn response_content(e: &RequestEntry, bodies: Bodies) -> Content {
    let unknown = Content { size: -1, mime_type: UNKNOWN_MIME.into(), text: None, encoding: None };
    if e.response_body.is_none() {
        return unknown;
    }
    let Some(view) = bodies(e.id, Side::Response) else { return unknown };
    Content {
        size: view.info.size as i64,
        mime_type: mime(&view),
        text: view.text,
        encoding: None,
    }
}

fn mime(view: &BodyView) -> String {
    view.info.content_type.clone().unwrap_or_else(|| UNKNOWN_MIME.into())
}

/// The decoded `name=value` pairs after `?` in `url`.
fn query_string(url: &str) -> Vec<Header> {
    let Some((_, query)) = url.split_once('?') else { return Vec::new() };
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Header { name: decode(name), value: decode(value) }
        })
        .collect()
}

/// Percent-decodes a query component, with `+` as a space. Bad escapes are kept as they are.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes.get(i + 1..i + 3).and_then(hex_byte) {
                Some(b) => {
                    out.push(b);
                    i += 3;
                    continue;
                }
                None => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_byte(pair: &[u8]) -> Option<u8> {
    let digit = |b: u8| (b as char).to_digit(16);
    Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
}

/// Unix milliseconds as ISO 8601 in UTC, e.g. `2026-11-19T09:13:07.123Z`.
fn iso_time(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A file name from the tunnel name, with characters that some systems reject replaced.
fn file_name(tunnel_name: &str) -> String {
    let safe: String = tunnel_name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let safe = safe.trim_matches(['-', '.']);
    format!("{}.har", if safe.is_empty() { "requests" } else { safe })
}

/// Asks where to save, then writes the tunnel's requests with the given ids as a HAR file.
/// Returns the saved path, or `None` when the dialog was canceled.
#[tauri::command]
pub async fn export_har(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    entry_ids: Vec<u64>,
) -> Result<Option<String>, String> {
    let wanted: HashSet<u64> = entry_ids.into_iter().collect();
    let entries: Vec<RequestEntry> = state
        .tunnels
        .requests(&id)
        .into_iter()
        .filter(|e| wanted.contains(&e.id))
        .collect();
    if entries.is_empty() {
        return Err("No requests to export.".into());
    }
    let name = state
        .settings
        .get()
        .tunnels
        .into_iter()
        .find(|t| t.id == id)
        .map(|t| t.name)
        .unwrap_or_default();
    let tunnels = &state.tunnels;
    let bodies = |entry_id: u64, side: Side| tunnels.request_body(&id, entry_id, side);
    let json = serde_json::to_vec_pretty(&build_with(&entries, &app.package_info().version.to_string(), &bodies))
        .map_err(|e| e.to_string())?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Export requests as HAR")
        .set_file_name(file_name(&name))
        .add_filter("HAR file", &["har"]);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    dialog.save_file(move |path| {
        let _ = tx.send(path);
    });
    let Some(path) = rx.await.ok().flatten() else { return Ok(None) };
    let path = path.into_path().map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json)
        .await
        .map_err(|e| format!("Couldn't save {}: {e}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<RequestEntry> {
        vec![
            RequestEntry {
                id: 2,
                at: 1_763_543_587_250,
                method: "POST".into(),
                url: "https://a.trycloudflare.com/api/thing".into(),
                status: Some(201),
                status_text: Some("Created".into()),
                guessed: true,
                duration_ms: Some(40),
                size: Some(6),
                headers: vec![
                    ("Authorization".into(), "(hidden, 16 characters)".into()),
                    ("Content-Type".into(), "application/json".into()),
                    ("X-Api-Key".into(), "(hidden, 1 characters)".into()),
                ],
                ..Default::default()
            },
            RequestEntry {
                id: 1,
                at: 1_763_543_586_123,
                method: "GET".into(),
                url: "https://a.trycloudflare.com/hello?x=1&name=a%20b+c&flag#top".into(),
                error: Some("Your server didn't answer".into()),
                duration_ms: Some(12),
                headers: vec![
                    ("Accept".into(), "*/*".into()),
                    ("Cookie".into(), "(hidden, 5 characters)".into()),
                ],
                ..Default::default()
            },
        ]
    }

    #[test]
    fn formats_times_as_iso_8601() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_763_543_587_250), "2025-11-19T09:13:07.250Z");
        assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso_time(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
    }

    #[test]
    fn decodes_query_strings() {
        let q = query_string("https://x/p?x=1&name=a%20b+c&flag&bad=%zz&&end=%2");
        let pairs: Vec<(&str, &str)> = q.iter().map(|h| (h.name.as_str(), h.value.as_str())).collect();
        assert_eq!(pairs, [("x", "1"), ("name", "a b c"), ("flag", ""), ("bad", "%zz"), ("end", "%2")]);
        assert!(query_string("https://x/p").is_empty());
        assert_eq!(query_string("https://x/p?a=%C3%A9#frag")[0].value, "é");
    }

    #[test]
    fn builds_a_har_log_oldest_first_without_credentials() {
        let har = serde_json::to_value(build(&sample(), "0.1.8")).unwrap();
        let log = &har["log"];
        assert_eq!(log["version"], "1.2");
        assert_eq!(log["creator"]["name"], "Cloudfairy");
        assert_eq!(log["creator"]["version"], "0.1.8");

        let entries = log["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let (get, post) = (&entries[0], &entries[1]);

        assert_eq!(get["startedDateTime"], "2025-11-19T09:13:06.123Z");
        assert_eq!(get["request"]["method"], "GET");
        assert_eq!(get["request"]["headers"], serde_json::json!([{"name": "Accept", "value": "*/*"}]));
        assert_eq!(get["request"]["queryString"][1], serde_json::json!({"name": "name", "value": "a b c"}));
        assert_eq!(get["request"]["bodySize"], -1);
        assert!(get["request"].get("postData").is_none());
        assert_eq!(get["response"]["status"], 0);
        assert_eq!(get["response"]["httpVersion"], "");
        assert_eq!(get["_error"], "Your server didn't answer");
        assert!(get.get("_guessed").is_none());
        assert_eq!(get["time"], 12);
        assert_eq!(get["timings"], serde_json::json!({"send": 0, "wait": 12, "receive": 0}));

        assert_eq!(post["request"]["headers"], serde_json::json!([{"name": "Content-Type", "value": "application/json"}]));
        assert_eq!(post["request"]["bodySize"], 6);
        assert_eq!(post["response"]["status"], 201);
        assert_eq!(post["response"]["statusText"], "Created");
        assert_eq!(post["response"]["httpVersion"], "HTTP/1.1");
        assert_eq!(post["response"]["redirectURL"], "");
        assert_eq!(post["response"]["content"], serde_json::json!({"size": -1, "mimeType": "x-unknown"}));
        assert_eq!(post["_guessed"], true);
        assert!(post.get("_error").is_none());

        // Fields every HAR 1.2 entry must have.
        for e in entries {
            for key in ["startedDateTime", "time", "request", "response", "cache", "timings"] {
                assert!(e.get(key).is_some(), "entry is missing {key}");
            }
            for key in ["method", "url", "httpVersion", "cookies", "headers", "queryString", "headersSize", "bodySize"] {
                assert!(e["request"].get(key).is_some(), "request is missing {key}");
            }
            for key in ["status", "statusText", "httpVersion", "cookies", "headers", "content", "redirectURL", "headersSize", "bodySize"] {
                assert!(e["response"].get(key).is_some(), "response is missing {key}");
            }
        }
    }

    #[test]
    fn empty_list_is_still_a_valid_log() {
        let har = serde_json::to_value(build(&[], "1.0.0")).unwrap();
        assert_eq!(har["log"]["entries"], serde_json::json!([]));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("My app"), "My-app.har");
        assert_eq!(file_name("localhost:3000/api"), "localhost-3000-api.har");
        assert_eq!(file_name("..."), "requests.har");
        assert_eq!(file_name(""), "requests.har");
    }

    #[test]
    fn proxied_bodies_and_response_headers() {
        use crate::capture::BodyInfo;
        let info = |t: &str| BodyInfo { size: 7, content_type: Some(t.into()), ..Default::default() };
        let e = RequestEntry {
            id: 9,
            at: 1,
            method: "POST".into(),
            url: "https://a.trycloudflare.com/hook".into(),
            status: Some(200),
            proxied: Some(true),
            request_body: Some(info("application/json")),
            response_body: Some(info("text/plain")),
            response_headers: Some(vec![
                ("Content-Type".into(), "text/plain".into()),
                ("Set-Cookie".into(), "(hidden, 9 characters)".into()),
            ]),
            ..Default::default()
        };
        let bodies = |id: u64, side: Side| {
            assert_eq!(id, 9);
            let (t, text) = match side {
                Side::Request => ("application/json", r#"{"a":1}"#),
                Side::Response => ("text/plain", "thanks!"),
            };
            Some(BodyView { info: info(t), text: Some(text.into()), decoded_from: None })
        };
        let har = serde_json::to_value(build_with(&[e], "1", &bodies)).unwrap();
        let entry = &har["log"]["entries"][0];
        assert_eq!(entry["request"]["postData"]["text"], r#"{"a":1}"#);
        assert_eq!(entry["request"]["postData"]["mimeType"], "application/json");
        assert_eq!(entry["response"]["content"]["text"], "thanks!");
        assert_eq!(entry["response"]["content"]["size"], 7);
        let headers = entry["response"]["headers"].as_array().unwrap();
        assert_eq!(headers.len(), 1, "Set-Cookie is left out");
    }
}
