//! The extra step before `cloudflared tunnel run` for a named tunnel: creating the Cloudflare
//! tunnel and its DNS record when needed (see `named.rs`), while the card shows it starting.

use std::path::Path;

use tauri::{AppHandle, Emitter, Runtime as TauriRuntime};

use super::{LogEvent, RuntimeView, Status, Tunnels, EVENT_LOG};
use crate::{
    named::{self, NamedTunnel},
    settings::TunnelConfig,
};

impl Tunnels {
    /// Runs while the tunnel is claimed as launching. Returns the arguments and command line
    /// for `cloudflared tunnel run`, or `None` when Stop was pressed during the setup.
    pub(super) async fn prepare_named<R: TauriRuntime>(
        &self,
        app: &AppHandle<R>,
        cfg: &TunnelConfig,
        named: &NamedTunnel,
        bin: &Path,
        run_id: u64,
        quick_args: Vec<String>,
    ) -> Result<Option<(Vec<String>, String)>, String> {
        let id = cfg.id.as_str();
        {
            let mut map = self.map();
            if let Some(rt) = map.get_mut(id).filter(|rt| rt.run_id == run_id) {
                rt.view.public_url = Some(format!("https://{}", named.hostname));
                self.emit(app, id, rt);
            }
        }

        let log = |line: String| {
            let mut map = self.map();
            if let Some(rt) = map.get_mut(id).filter(|rt| rt.run_id == run_id) {
                let _ = app.emit(EVENT_LOG, LogEvent { id, line: &line });
                rt.log(line);
            }
        };
        let stopped = || !self.map().get(id).is_some_and(|rt| rt.run_id == run_id && !rt.stopping);
        let result = named::provision(app, bin, id, named, &log, &stopped).await;

        let mut map = self.map();
        let Some(rt) = map.get_mut(id).filter(|rt| rt.run_id == run_id) else {
            return Err("The tunnel was removed while it was starting.".into());
        };
        match result {
            Ok(Some(prepared)) if !rt.stopping => {
                let args = named::run_args(quick_args, &prepared);
                let command_line = format!("{} {}", bin.display(), args.join(" "));
                let line = format!("$ {command_line}");
                let _ = app.emit(EVENT_LOG, LogEvent { id, line: &line });
                rt.log(line);
                Ok(Some((args, command_line)))
            }
            Ok(_) => {
                rt.launching = false;
                rt.view = RuntimeView::default();
                rt.log("[cloudfairy] Stopped before cloudflared started.".into());
                self.emit(app, id, rt);
                Ok(None)
            }
            Err(msg) => {
                rt.launching = false;
                rt.view = RuntimeView {
                    status: Status::Error,
                    error: Some(msg.clone()),
                    ..Default::default()
                };
                rt.log(format!("[cloudfairy] {msg}"));
                self.emit(app, id, rt);
                Err(msg)
            }
        }
    }
}
