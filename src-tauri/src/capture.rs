//! Requests recorded by Cloudfairy's proxy (see `proxy.rs`): entries for the request list,
//! the first 1 MB of each body, and what is needed to send a request again.
//!
//! Bodies of one tunnel share a memory budget. When a new body doesn't fit, the oldest kept
//! bodies are dropped first; their entries stay in the list with `dropped` set.

use std::{
    collections::VecDeque,
    io::Read,
    sync::{Mutex, MutexGuard},
};

use bytes::Bytes;
use hyper::{HeaderMap, Method};
use serde::Serialize;

use crate::requests::{RequestEntry, MAX_ENTRIES};

/// The part of each request and response body that is kept.
pub const MAX_BODY: usize = 1024 * 1024;
/// Kept bodies of one tunnel, in bytes.
pub const MAX_MEMORY: usize = 50 * 1024 * 1024;
/// Decompressed bodies are cut here before they're shown.
const MAX_DECODED: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Request,
    Response,
}

impl Side {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "request" => Some(Self::Request),
            "response" => Some(Self::Response),
            _ => None,
        }
    }
}

/// What the request list shows about a body. The bytes themselves are fetched separately.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyInfo {
    /// Bytes that went through the proxy.
    pub size: u64,
    /// Only the first 1 MB was kept.
    pub truncated: bool,
    /// False when the connection closed before the body ended.
    pub complete: bool,
    /// The kept bytes were dropped to stay under the tunnel's memory limit.
    pub dropped: bool,
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
}

/// A body as the request list's detail view gets it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyView {
    pub info: BodyInfo,
    /// The body as text, when it is text. Compressed bodies are decompressed first.
    pub text: Option<String>,
    /// The `Content-Encoding` that was undone to get `text`.
    pub decoded_from: Option<String>,
}

/// The first bytes of a body as it streams through, and how much went by in all.
#[derive(Debug, Default)]
pub struct Recorded {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub complete: bool,
}

impl Recorded {
    pub fn push(&mut self, chunk: &[u8]) {
        self.size += chunk.len() as u64;
        let room = MAX_BODY.saturating_sub(self.bytes.len());
        self.bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }

    pub fn truncated(&self) -> bool {
        self.size > self.bytes.len() as u64
    }
}

/// A request as the proxy forwarded it, credentials included, so it can be sent again.
#[derive(Clone, Debug)]
pub struct Template {
    pub method: Method,
    /// Path and query.
    pub path: String,
    pub headers: HeaderMap,
    /// WebSocket and other `Upgrade` requests can't be replayed.
    pub upgrade: bool,
}

struct Item {
    entry: RequestEntry,
    request: Option<Vec<u8>>,
    response: Option<Vec<u8>>,
    template: Template,
}

impl Item {
    fn body_slot(&mut self, side: Side) -> (&mut Option<Vec<u8>>, &mut Option<BodyInfo>) {
        match side {
            Side::Request => (&mut self.request, &mut self.entry.request_body),
            Side::Response => (&mut self.response, &mut self.entry.response_body),
        }
    }
}

struct Inner {
    items: VecDeque<Item>,
    next_id: u64,
    /// Sum of kept body bytes.
    bytes: usize,
    limit: usize,
}

pub struct Capture {
    inner: Mutex<Inner>,
}

impl Default for Capture {
    fn default() -> Self {
        Self::with_limit(MAX_MEMORY)
    }
}

impl Capture {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                items: VecDeque::new(),
                next_id: 0,
                bytes: 0,
                limit,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds a request and gives it an id. The oldest entry goes once there are 500.
    pub fn begin(&self, mut entry: RequestEntry, template: Template) -> RequestEntry {
        let mut inner = self.lock();
        inner.next_id += 1;
        entry.id = inner.next_id;
        if inner.items.len() >= MAX_ENTRIES {
            if let Some(old) = inner.items.pop_front() {
                let freed = old.request.map_or(0, |b| b.len()) + old.response.map_or(0, |b| b.len());
                inner.bytes -= freed;
            }
        }
        inner.items.push_back(Item {
            entry: entry.clone(),
            request: None,
            response: None,
            template,
        });
        entry
    }

    pub fn update(&self, id: u64, f: impl FnOnce(&mut RequestEntry)) -> Option<RequestEntry> {
        let mut inner = self.lock();
        let item = inner.items.iter_mut().find(|i| i.entry.id == id)?;
        f(&mut item.entry);
        Some(item.entry.clone())
    }

    /// Stores a finished body and returns the updated entry.
    pub fn store_body(&self, id: u64, side: Side, rec: Recorded, headers: &HeaderMap) -> Option<RequestEntry> {
        let mut info = BodyInfo {
            size: rec.size,
            truncated: rec.truncated(),
            complete: rec.complete,
            dropped: false,
            content_type: header_text(headers, "content-type"),
            content_encoding: header_text(headers, "content-encoding"),
        };
        let mut inner = self.lock();
        let inner = &mut *inner;
        let pos = inner.items.iter().position(|i| i.entry.id == id)?;
        let mut bytes = rec.bytes;
        if bytes.len() > inner.limit {
            bytes.clear();
            info.dropped = true;
        }
        // Make room by dropping the oldest kept bodies.
        let mut oldest = 0;
        while inner.bytes + bytes.len() > inner.limit && oldest < inner.items.len() {
            let item = &mut inner.items[oldest];
            for s in [Side::Request, Side::Response] {
                let (kept, body_info) = item.body_slot(s);
                if let Some(old) = kept.take() {
                    inner.bytes -= old.len();
                    if let Some(i) = body_info {
                        i.dropped = true;
                    }
                }
            }
            oldest += 1;
        }
        inner.bytes += bytes.len();
        let item = &mut inner.items[pos];
        if side == Side::Request && rec.size > 0 {
            item.entry.size = Some(rec.size);
        }
        let (kept, body_info) = item.body_slot(side);
        if let Some(old) = kept.take() {
            inner.bytes -= old.len();
        }
        *kept = (!bytes.is_empty()).then_some(bytes);
        *body_info = Some(info);
        Some(item.entry.clone())
    }

    pub fn entries(&self) -> Vec<RequestEntry> {
        self.lock().items.iter().map(|i| i.entry.clone()).collect()
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.items.clear();
        inner.bytes = 0;
    }

    /// Kept body bytes, for tests and the memory limit.
    #[cfg(test)]
    pub fn kept_bytes(&self) -> usize {
        self.lock().bytes
    }

    /// The body as the detail view shows it. `None` when the entry or body is unknown.
    pub fn body_view(&self, id: u64, side: Side) -> Option<BodyView> {
        let inner = self.lock();
        let item = inner.items.iter().find(|i| i.entry.id == id)?;
        let (info, bytes) = match side {
            Side::Request => (item.entry.request_body.clone()?, item.request.as_deref()),
            Side::Response => (item.entry.response_body.clone()?, item.response.as_deref()),
        };
        Some(view_body(info, bytes.unwrap_or_default()))
    }

    /// What's needed to send entry `id` again: the request and its full body.
    pub fn replay_parts(&self, id: u64) -> Result<(Template, Bytes, u64), String> {
        let inner = self.lock();
        let item = inner
            .items
            .iter()
            .find(|i| i.entry.id == id)
            .ok_or("That request is no longer in the list.")?;
        if item.template.upgrade {
            return Err("WebSocket and other upgrade requests can't be replayed.".into());
        }
        let body = match &item.entry.request_body {
            None if item.entry.size.unwrap_or(0) > 0 => {
                return Err("The request body is still arriving. Try again when it has finished.".into())
            }
            None => Bytes::new(),
            Some(info) if info.truncated => {
                return Err("The request body is over 1 MB, so Cloudfairy didn't keep all of it.".into())
            }
            Some(info) if !info.complete => {
                return Err("The visitor's connection closed before the request body ended.".into())
            }
            Some(info) if info.dropped => {
                return Err("The request body was dropped to save memory.".into())
            }
            Some(_) => Bytes::from(item.request.clone().unwrap_or_default()),
        };
        Ok((item.template.clone(), body, item.entry.id))
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let v = headers.get(name)?.to_str().ok()?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn view_body(info: BodyInfo, bytes: &[u8]) -> BodyView {
    let encoding = info
        .content_encoding
        .as_deref()
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e != "identity");
    let (decoded, decoded_from) = match encoding.as_deref() {
        None => (Some(bytes.to_vec()), None),
        Some("gzip" | "x-gzip") => (Some(read_some(flate2::read::GzDecoder::new(bytes))), encoding.clone()),
        Some("deflate") => {
            // "deflate" is meant to be zlib-wrapped, but some servers send raw deflate.
            let zlib = read_some(flate2::read::ZlibDecoder::new(bytes));
            let out = if zlib.is_empty() && !bytes.is_empty() {
                read_some(flate2::read::DeflateDecoder::new(bytes))
            } else {
                zlib
            };
            (Some(out), encoding.clone())
        }
        // Brotli and zstd aren't decoded; they're shown as type and size.
        Some(_) => (None, None),
    };
    let text = decoded.and_then(|d| as_text(info.content_type.as_deref(), &d));
    BodyView {
        decoded_from: decoded_from.filter(|_| text.is_some()),
        info,
        text,
    }
}

/// Reads as much as decompresses cleanly. A truncated body still shows its start.
fn read_some(mut reader: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    while out.len() < MAX_DECODED {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    out.truncate(MAX_DECODED);
    out
}

fn is_text_type(content_type: &str) -> bool {
    let ct = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    ct.starts_with("text/")
        || ct.ends_with("+json")
        || ct.ends_with("+xml")
        || matches!(
            ct.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-www-form-urlencoded"
                | "application/graphql"
                | "application/x-ndjson"
                | "application/manifest+json"
                | "image/svg+xml"
        )
}

/// The bytes as text, when the type says text, or when no type is given and they are UTF-8.
fn as_text(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    // A body cut at 1 MB can end partway through a character.
    let valid = match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&bytes[..e.valid_up_to()]).ok(),
        Err(_) => None,
    };
    match content_type {
        Some(ct) if is_text_type(ct) => Some(valid.map_or_else(|| String::from_utf8_lossy(bytes).into_owned(), str::to_string)),
        Some(_) => None,
        None => valid
            .filter(|s| !s.chars().any(|c| c.is_control() && !c.is_whitespace()))
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn template() -> Template {
        Template {
            method: Method::POST,
            path: "/api".into(),
            headers: HeaderMap::new(),
            upgrade: false,
        }
    }

    fn rec(len: usize) -> Recorded {
        let mut r = Recorded::default();
        r.push(&vec![b'a'; len]);
        r.complete = true;
        r
    }

    fn json_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("content-type", "application/json".parse().unwrap());
        h
    }

    #[test]
    fn caps_each_body_at_one_megabyte() {
        let mut r = Recorded::default();
        r.push(&vec![1u8; MAX_BODY - 10]);
        assert!(!r.truncated());
        r.push(&[2u8; 30]);
        assert_eq!((r.bytes.len(), r.size), (MAX_BODY, MAX_BODY as u64 + 20));
        assert!(r.truncated());
        assert_eq!(r.bytes[MAX_BODY - 1], 2, "the part that fits is kept");
    }

    #[test]
    fn drops_oldest_bodies_first_when_memory_is_full() {
        let cap = Capture::with_limit(250);
        let ids: Vec<u64> = (0..3)
            .map(|_| cap.begin(RequestEntry::default(), template()).id)
            .collect();
        cap.store_body(ids[0], Side::Request, rec(100), &json_headers());
        cap.store_body(ids[1], Side::Response, rec(100), &json_headers());
        assert_eq!(cap.kept_bytes(), 200);

        let e = cap.store_body(ids[2], Side::Response, rec(100), &json_headers()).unwrap();
        assert_eq!(cap.kept_bytes(), 200);
        assert!(!e.response_body.unwrap().dropped);
        let first = cap.body_view(ids[0], Side::Request).unwrap();
        assert!(first.info.dropped && first.text.is_none(), "the oldest body went first");
        assert!(!cap.body_view(ids[1], Side::Response).unwrap().info.dropped);
    }

    #[test]
    fn keeps_the_last_500_entries_and_frees_their_bodies() {
        let cap = Capture::default();
        let first = cap.begin(RequestEntry::default(), template()).id;
        cap.store_body(first, Side::Request, rec(10), &HeaderMap::new());
        for _ in 0..MAX_ENTRIES {
            cap.begin(RequestEntry::default(), template());
        }
        let entries = cap.entries();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0].id, 2);
        assert_eq!(cap.kept_bytes(), 0);
    }

    #[test]
    fn shows_text_and_binary_bodies() {
        let text = |ct: Option<&str>, b: &[u8]| as_text(ct, b);
        assert_eq!(text(Some("application/json; charset=utf-8"), b"{\"a\":1}").as_deref(), Some("{\"a\":1}"));
        assert_eq!(text(Some("image/png"), b"\x89PNG"), None);
        assert_eq!(text(None, b"plain words").as_deref(), Some("plain words"));
        assert_eq!(text(None, b"\x00\x01\x02"), None);
        // Cut partway through "é".
        assert_eq!(text(Some("text/plain"), b"caf\xc3").as_deref(), Some("caf"));
    }

    #[test]
    fn decodes_gzip_bodies_for_display() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"{\"ok\":true}").unwrap();
        let bytes = gz.finish().unwrap();
        let info = BodyInfo {
            size: bytes.len() as u64,
            complete: true,
            content_type: Some("application/json".into()),
            content_encoding: Some("gzip".into()),
            ..Default::default()
        };
        let view = view_body(info.clone(), &bytes);
        assert_eq!(view.text.as_deref(), Some("{\"ok\":true}"));
        assert_eq!(view.decoded_from.as_deref(), Some("gzip"));

        let br = BodyInfo { content_encoding: Some("br".into()), ..info };
        assert_eq!(view_body(br, b"\x1b\x00").text, None);
    }

    #[test]
    fn refuses_to_replay_incomplete_bodies() {
        let cap = Capture::default();
        let id = cap.begin(RequestEntry::default(), template()).id;
        let mut big = rec(MAX_BODY);
        big.push(b"more");
        cap.store_body(id, Side::Request, big, &HeaderMap::new());
        assert!(cap.replay_parts(id).unwrap_err().contains("over 1 MB"));

        let id = cap.begin(RequestEntry::default(), template()).id;
        cap.store_body(id, Side::Request, rec(3), &HeaderMap::new());
        let (t, body, _) = cap.replay_parts(id).unwrap();
        assert_eq!((t.method, body.len()), (Method::POST, 3));

        let ws = cap.begin(RequestEntry::default(), Template { upgrade: true, ..template() }).id;
        assert!(cap.replay_parts(ws).is_err());
    }
}
