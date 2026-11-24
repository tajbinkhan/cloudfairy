//! Builds a per-tunnel request list from cloudflared's debug log.
//!
//! At `--loglevel debug` cloudflared logs two lines per proxied request:
//!
//! ```text
//! 2026-11-19T09:13:07Z DBG POST https://x.trycloudflare.com/api HTTP/1.1 connIndex=0 content-length=6 event=1 headers={"Accept":["*/*"],…} host=x.trycloudflare.com ingressRule=0 originService=http://127.0.0.1:3000 path=/api
//! 2026-11-19T09:13:07Z DBG 200 OK connIndex=0 content-length=-1 event=1 ingressRule=0 originService=http://127.0.0.1:3000
//! ```
//!
//! The response line doesn't say which request it answers, so it is matched to the oldest
//! unanswered request on the same connection. When several were waiting the match is a guess,
//! and the entry says so. A request that fails or that the visitor cancels logs
//! `ERR Request failed error="…" connIndex=0 dest=<url>` instead.

use std::collections::VecDeque;

use serde::Serialize;

const MAX_ENTRIES: usize = 500;
/// A request still unanswered after this long is no longer offered responses.
const PENDING_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestEntry {
    pub id: u64,
    /// Unix time in milliseconds, when Cloudfairy read the request line.
    pub at: u64,
    pub method: String,
    /// The full public URL, query string included.
    pub url: String,
    pub status: Option<u16>,
    pub status_text: Option<String>,
    /// The response was matched by order while other requests were waiting on the same connection.
    pub guessed: bool,
    /// Why the request failed, when cloudflared logged `Request failed` for it.
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    /// Request body size, when the visitor sent a Content-Length.
    pub size: Option<u64>,
    pub ip: Option<String>,
    pub country: Option<String>,
    pub user_agent: Option<String>,
    /// Request headers as cloudflared received them, with credentials hidden.
    pub headers: Vec<(String, String)>,
}

#[derive(Default)]
pub struct RequestLog {
    entries: VecDeque<RequestEntry>,
    /// Unanswered requests, oldest first: (connIndex, entry id).
    pending: Vec<(u32, u64)>,
    next_id: u64,
}

impl RequestLog {
    pub fn entries(&self) -> Vec<RequestEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.pending.clear();
    }

    /// cloudflared stopped, so nothing still waiting will get an answer.
    pub fn end_run(&mut self) {
        self.pending.clear();
    }

    /// Feeds one log line. Returns the entry it added or changed.
    pub fn handle_line(&mut self, line: &str, now: u64) -> Option<RequestEntry> {
        if let Some(req) = parse_request(line) {
            return Some(self.add(req, now));
        }
        if let Some((conn, status, text)) = parse_response(line) {
            return self.answer(conn, now, |e| {
                e.status = Some(status);
                e.status_text = Some(text);
            });
        }
        if let Some((conn, url, error)) = parse_failure(line) {
            return self.fail(conn, &url, error, now);
        }
        None
    }

    fn add(&mut self, req: ParsedRequest, now: u64) -> RequestEntry {
        self.next_id += 1;
        let entry = RequestEntry {
            id: self.next_id,
            at: now,
            method: req.method,
            url: req.url,
            size: req.size.filter(|&n| n > 0),
            ip: header(&req.headers, "Cf-Connecting-Ip"),
            country: header(&req.headers, "Cf-Ipcountry").filter(|c| c != "XX"),
            user_agent: header(&req.headers, "User-Agent"),
            headers: req
                .headers
                .into_iter()
                .map(|(name, value)| {
                    let value = if is_secret(&name) { hidden(&value) } else { value };
                    (name, value)
                })
                .collect(),
            ..Default::default()
        };
        if self.entries.len() >= MAX_ENTRIES {
            if let Some(old) = self.entries.pop_front() {
                self.pending.retain(|&(_, id)| id != old.id);
            }
        }
        self.entries.push_back(entry.clone());
        self.pending.push((req.conn, entry.id));
        entry
    }

    /// Gives a response to the oldest unanswered request on `conn`.
    fn answer(&mut self, conn: u32, now: u64, set: impl FnOnce(&mut RequestEntry)) -> Option<RequestEntry> {
        self.expire(now);
        let waiting = self.pending.iter().filter(|&&(c, _)| c == conn).count();
        let pos = self.pending.iter().position(|&(c, _)| c == conn)?;
        let (_, id) = self.pending.remove(pos);
        let entry = self.entries.iter_mut().find(|e| e.id == id)?;
        set(entry);
        entry.guessed = waiting > 1;
        entry.duration_ms = Some(now.saturating_sub(entry.at));
        Some(entry.clone())
    }

    /// Marks the oldest unanswered request for `url` as failed, preferring one on `conn`.
    fn fail(&mut self, conn: u32, url: &str, error: String, now: u64) -> Option<RequestEntry> {
        let matches = |&(_, id): &(u32, u64), entries: &VecDeque<RequestEntry>| {
            entries.iter().any(|e| e.id == id && e.url == url)
        };
        let pos = self
            .pending
            .iter()
            .position(|p| p.0 == conn && matches(p, &self.entries))
            .or_else(|| self.pending.iter().position(|p| matches(p, &self.entries)))?;
        let (_, id) = self.pending.remove(pos);
        let entry = self.entries.iter_mut().find(|e| e.id == id)?;
        entry.error = Some(error);
        entry.duration_ms = Some(now.saturating_sub(entry.at));
        Some(entry.clone())
    }

    fn expire(&mut self, now: u64) {
        let entries = &self.entries;
        self.pending.retain(|&(_, id)| {
            entries
                .iter()
                .find(|e| e.id == id)
                .is_some_and(|e| now.saturating_sub(e.at) < PENDING_TIMEOUT_MS)
        });
    }
}

#[derive(Debug, PartialEq)]
struct ParsedRequest {
    method: String,
    url: String,
    conn: u32,
    size: Option<u64>,
    headers: Vec<(String, String)>,
}

/// The text after cloudflared's `DBG ` level marker.
fn debug_message(line: &str) -> Option<&str> {
    line.split_once(" DBG ").map(|(_, rest)| rest)
}

/// `GET https://x.trycloudflare.com/path?q=1 HTTP/1.1 connIndex=0 content-length=0 … headers={…} …`
fn parse_request(line: &str) -> Option<ParsedRequest> {
    let msg = debug_message(line)?;
    let mut parts = msg.splitn(4, ' ');
    let method = parts.next()?;
    let url = parts.next()?;
    let proto = parts.next()?;
    let fields = parts.next()?;
    let is_method = !method.is_empty() && method.bytes().all(|b| b.is_ascii_uppercase());
    let is_url = url.starts_with("https://") || url.starts_with("http://");
    if !(is_method && is_url && proto.starts_with("HTTP/") && fields.contains("originService=")) {
        return None;
    }
    // Header values are JSON-escaped, so a visitor can't fake the fields that follow them.
    let (before, headers) = match fields.split_once(" headers=") {
        Some((before, rest)) => (before, parse_headers(rest)),
        None => (fields, Vec::new()),
    };
    Some(ParsedRequest {
        method: method.into(),
        url: url.into(),
        conn: field(before, "connIndex")?.parse().ok()?,
        size: field(before, "content-length").and_then(|n| n.parse().ok()),
        headers,
    })
}

/// `{"Accept":["*/*"],"User-Agent":["curl/8.0"]}` at the start of `text`, in name order.
fn parse_headers(text: &str) -> Vec<(String, String)> {
    let mut stream = serde_json::Deserializer::from_str(text)
        .into_iter::<std::collections::BTreeMap<String, Vec<String>>>();
    match stream.next() {
        Some(Ok(map)) => map.into_iter().map(|(name, values)| (name, values.join(", "))).collect(),
        _ => Vec::new(),
    }
}

/// `200 OK connIndex=0 content-length=-1 event=1 ingressRule=0 originService=…`
fn parse_response(line: &str) -> Option<(u32, u16, String)> {
    let msg = debug_message(line)?;
    let (code, rest) = msg.split_once(' ')?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The reason phrase comes from the local server, so read the fields from the right.
    let (text, fields) = rest.rsplit_once(" connIndex=")?;
    if !fields.contains("originService=") || fields.contains("headers=") {
        return None;
    }
    let conn = fields.split(' ').next()?.parse().ok()?;
    Some((conn, code.parse().ok()?, text.trim().to_string()))
}

/// `ERR Request failed error="Unable to reach the origin service…" connIndex=0 dest=https://… event=0 …`
fn parse_failure(line: &str) -> Option<(u32, String, String)> {
    let (_, rest) = line.split_once(" ERR Request failed ")?;
    let quoted = rest.strip_prefix("error=\"")?;
    let (error, fields) = split_quoted(quoted)?;
    let conn = field(fields, "connIndex")?.parse().ok()?;
    let url = field(fields, "dest")?.to_string();
    Some((conn, url, failure_text(&error)))
}

/// A short reason for the request list, from cloudflared's error message.
fn failure_text(error: &str) -> String {
    if error.starts_with("Unable to reach the origin service") {
        return "Your server didn't answer".into();
    }
    if error.contains("context canceled") {
        return "Canceled by the visitor".into();
    }
    // First sentence, without the underlying Go error after the colon.
    let short = error.split([':', '\n']).next().unwrap_or(error);
    let short = short.split(". ").next().unwrap_or(short);
    short.trim().trim_end_matches('.').to_string()
}

/// Splits `some \"quoted\" text" rest` at the closing quote, unescaping `\"` and `\\`.
fn split_quoted(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '"' => return Some((out, &s[i + 1..])),
            _ => out.push(c),
        }
    }
    None
}

/// The value of ` name=value` in a run of space-separated fields.
fn field<'a>(fields: &'a str, name: &str) -> Option<&'a str> {
    fields
        .split(' ')
        .find_map(|f| f.strip_prefix(name)?.strip_prefix('='))
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// Headers whose values are credentials. They are hidden so a screen share doesn't leak them.
pub(crate) fn is_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "authorization" | "proxy-authorization" | "cookie")
        || ["token", "secret", "password", "api-key", "apikey"].iter().any(|w| n.contains(w))
}

fn hidden(value: &str) -> String {
    format!("(hidden, {} characters)", value.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GET: &str = r#"2026-11-19T09:13:06Z DBG GET https://a.trycloudflare.com/hello?x=1 HTTP/1.1 connIndex=0 content-length=0 event=1 headers={"Accept":["*/*"],"Cf-Connecting-Ip":["203.0.113.7"],"Cf-Ipcountry":["BD"],"Cookie":["s=abc"],"User-Agent":["curl/8.23.0"],"X-Weird":["a\"} connIndex=9 b"]} host=a.trycloudflare.com ingressRule=0 originService=http://127.0.0.1:18765 path=/hello"#;
    const POST: &str = r#"2026-11-19T09:13:07Z DBG POST https://a.trycloudflare.com/api/thing HTTP/1.1 connIndex=1 content-length=6 event=1 headers={"Authorization":["Bearer secret123"],"Content-Length":["6"],"X-Api-Key":["k"]} host=a.trycloudflare.com ingressRule=0 originService=http://127.0.0.1:18765 path=/api/thing"#;
    const OK: &str = "2026-11-19T09:13:06Z DBG 200 OK connIndex=0 content-length=-1 event=1 ingressRule=0 originService=http://127.0.0.1:18765";
    const NOT_FOUND: &str = "2026-11-19T09:13:07Z DBG 404 Not Found connIndex=0 content-length=-1 event=1 ingressRule=0 originService=http://127.0.0.1:18765";

    #[test]
    fn parses_a_request_line() {
        let req = parse_request(GET).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "https://a.trycloudflare.com/hello?x=1");
        assert_eq!(req.conn, 0, "a header value can't override connIndex");
        assert_eq!(req.size, Some(0));
        assert_eq!(req.headers.len(), 6);
        assert_eq!(req.headers[5], ("X-Weird".into(), "a\"} connIndex=9 b".into()));

        assert_eq!(parse_request(OK), None);
        assert_eq!(parse_request("2026-11-19T09:13:05Z DBG QUIC MTU updated to 1296 connIndex=0"), None);
        assert_eq!(parse_request("2026-11-19T09:13:05Z INF GET https://x HTTP/1.1 originService=y"), None);
    }

    #[test]
    fn parses_response_and_failure_lines() {
        assert_eq!(parse_response(OK), Some((0, 200, "OK".into())));
        assert_eq!(parse_response(NOT_FOUND), Some((0, 404, "Not Found".into())));
        assert_eq!(parse_response(GET), None);
        assert_eq!(parse_response("2026-11-19T09:13:05Z DBG 100 things happened"), None);

        let failed = r#"2026-11-19T09:13:08Z ERR Request failed error="Unable to reach the origin service. The service may be down or it may not be responding to traffic from cloudflared: dial tcp 127.0.0.1:18765: connectex: refused" connIndex=0 dest=https://a.trycloudflare.com/down event=0 ip=198.41.192.67 type=http"#;
        assert_eq!(
            parse_failure(failed),
            Some((0, "https://a.trycloudflare.com/down".into(), "Your server didn't answer".into()))
        );
        let canceled = r#"2026-11-19T09:15:12Z ERR Request failed error="Incoming request ended abruptly: context canceled" connIndex=2 dest=https://a.trycloudflare.com/slow event=0 type=http"#;
        assert_eq!(
            parse_failure(canceled),
            Some((2, "https://a.trycloudflare.com/slow".into(), "Canceled by the visitor".into()))
        );
        assert_eq!(parse_failure(r#"2026-11-19T09:13:08Z ERR  error="Unable to reach" connIndex=0"#), None);
    }

    #[test]
    fn records_requests_with_visitor_details_and_hidden_credentials() {
        let mut log = RequestLog::default();
        let e = log.handle_line(GET, 1_000).unwrap();
        assert_eq!((e.id, e.method.as_str(), e.status), (1, "GET", None));
        assert_eq!(e.ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(e.country.as_deref(), Some("BD"));
        assert_eq!(e.user_agent.as_deref(), Some("curl/8.23.0"));
        assert_eq!(e.size, None, "an empty body has no size");
        assert!(e.headers.contains(&("Cookie".into(), "(hidden, 5 characters)".into())));

        let e = log.handle_line(POST, 1_000).unwrap();
        assert_eq!(e.size, Some(6));
        assert!(e.headers.contains(&("Authorization".into(), "(hidden, 16 characters)".into())));
        assert!(e.headers.contains(&("X-Api-Key".into(), "(hidden, 1 characters)".into())));
        assert!(e.headers.contains(&("Content-Length".into(), "6".into())));
    }

    #[test]
    fn matches_responses_by_connection_and_order() {
        let mut log = RequestLog::default();
        log.handle_line(GET, 1_000);
        log.handle_line(POST, 1_010); // connIndex=1
        let e = log.handle_line(OK, 1_250).unwrap();
        assert_eq!((e.id, e.status, e.duration_ms, e.guessed), (1, Some(200), Some(250), false));

        // Two waiting on one connection: the first is answered, and flagged as a guess.
        log.handle_line(GET, 2_000);
        log.handle_line(GET, 2_001);
        let e = log.handle_line(NOT_FOUND, 2_100).unwrap();
        assert_eq!((e.id, e.status, e.guessed), (3, Some(404), true));
        let e = log.handle_line(OK, 2_200).unwrap();
        assert_eq!((e.id, e.status, e.guessed), (4, Some(200), false));

        // The POST on connIndex=1 is still waiting; nothing is left on connIndex=0.
        assert_eq!(log.handle_line(OK, 2_300), None);
        assert_eq!(log.entries()[1].status, None);
    }

    #[test]
    fn failed_requests_leave_the_queue() {
        let mut log = RequestLog::default();
        let slow = GET.replace("/hello?x=1", "/slow");
        log.handle_line(&slow, 1_000);
        log.handle_line(GET, 1_100);
        let canceled = r#"2026-11-19T09:15:12Z ERR Request failed error="Incoming request ended abruptly: context canceled" connIndex=0 dest=https://a.trycloudflare.com/slow event=0 type=http"#;
        let e = log.handle_line(canceled, 1_500).unwrap();
        assert_eq!((e.id, e.error.as_deref(), e.status), (1, Some("Canceled by the visitor"), None));
        // The next response goes to the request that is still waiting, not the canceled one.
        let e = log.handle_line(OK, 1_600).unwrap();
        assert_eq!((e.id, e.guessed), (2, false));
    }

    #[test]
    fn stale_requests_stop_taking_responses() {
        let mut log = RequestLog::default();
        log.handle_line(GET, 0);
        log.handle_line(GET, PENDING_TIMEOUT_MS + 10);
        let e = log.handle_line(OK, PENDING_TIMEOUT_MS + 20).unwrap();
        assert_eq!((e.id, e.guessed), (2, false));
    }

    #[test]
    fn keeps_the_newest_entries() {
        let mut log = RequestLog::default();
        for i in 0..MAX_ENTRIES + 5 {
            log.handle_line(GET, i as u64);
        }
        let entries = log.entries();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].id, 6);
        assert_eq!(log.pending.len(), MAX_ENTRIES);
        log.clear();
        assert!(log.entries().is_empty() && log.pending.is_empty());
    }
}
