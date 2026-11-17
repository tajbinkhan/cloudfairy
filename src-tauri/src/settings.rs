//! Persistent settings: the cloudflared location, protocol choice and saved tunnels.

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Explicit path to the cloudflared binary. `None` means auto-detect.
    pub cloudflared_path: Option<String>,
    /// `auto`, `quic` or `http2`.
    pub protocol: String,
    pub tunnels: Vec<TunnelConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cloudflared_path: None,
            protocol: "auto".into(),
            tunnels: Vec::new(),
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
