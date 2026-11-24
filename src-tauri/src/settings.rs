//! Persistent settings: the cloudflared location, protocol choice, background behavior and
//! saved tunnels.

use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelConfig {
    pub id: String,
    pub name: String,
    /// Normalised local origin, e.g. `http://localhost:3000`.
    pub target: String,
    /// Send the local address as the `Host` header instead of the trycloudflare.com name.
    /// Dev servers such as Vite reject requests for hostnames they don't know.
    #[serde(default)]
    pub rewrite_host: bool,
    /// Start this tunnel when Cloudfairy opens.
    #[serde(default)]
    pub start_on_open: bool,
    /// Set for a tunnel on the user's own domain. `None` is a quick tunnel.
    #[serde(default)]
    pub named: Option<crate::named::NamedTunnel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Explicit path to the cloudflared binary. `None` means auto-detect.
    pub cloudflared_path: Option<String>,
    /// `auto`, `quic` or `http2`.
    pub protocol: String,
    pub tunnels: Vec<TunnelConfig>,
    /// Closing the window while tunnels run hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Desktop notifications when a tunnel goes live or fails while the window isn't focused.
    pub notify: bool,
    /// Copy a tunnel's link to the clipboard when it goes live.
    pub copy_on_live: bool,
    /// Origin certificate path that `cloudflared tunnel login` reported. `None` means
    /// cloudflared's default locations.
    #[serde(default)]
    pub origin_cert: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cloudflared_path: None,
            protocol: "auto".into(),
            tunnels: Vec::new(),
            close_to_tray: true,
            notify: true,
            copy_on_live: false,
            origin_cert: None,
        }
    }
}

pub struct SettingsStore {
    path: PathBuf,
    data: Mutex<Settings>,
}

impl SettingsStore {
    pub fn load(path: PathBuf) -> Self {
        let data = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| {
                // Keep the unreadable file around instead of silently overwriting it.
                let _ = fs::rename(&path, path.with_extension("json.bak"));
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Settings> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self) -> Settings {
        self.lock().clone()
    }

    /// Mutate the settings and write them to disk.
    pub fn update<R>(&self, f: impl FnOnce(&mut Settings) -> R) -> Result<R, String> {
        let mut data = self.lock();
        let result = f(&mut data);
        let json = serde_json::to_string_pretty(&*data).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("Couldn't create settings folder: {e}"))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| format!("Couldn't save settings: {e}"))?;
        fs::rename(&tmp, &self.path).map_err(|e| format!("Couldn't save settings: {e}"))?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_get_new_defaults() {
        let old = r#"{"cloudflaredPath":null,"protocol":"http2",
            "tunnels":[{"id":"t1","name":"App","target":"http://localhost:3000"}]}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.protocol, "http2");
        assert!(s.close_to_tray && s.notify && !s.copy_on_live);
        let t = &s.tunnels[0];
        assert!(!t.rewrite_host && !t.start_on_open);
        assert!(t.named.is_none() && s.origin_cert.is_none());
    }
}
