//! The parts of [`Tunnels`] that deal with Cloudfairy's proxy (see `proxy.rs`).

use std::sync::{Arc, Weak};

use tauri::{AppHandle, Emitter, Runtime as TauriRuntime};

use super::{Inner, RequestEvent, Tunnels, EVENT_REQUEST};
use crate::{
    capture::{BodyView, Side},
    proxy::{self, Event, ProxyConfig, ProxyHandle},
    requests::RequestEntry,
    settings::TunnelConfig,
};

/// Same text as the warning cloudflared's "Unable to reach the origin service" line sets.
const UNREACHABLE_WARNING: &str =
    "Cloudflare reached your computer, but nothing answered at the local address. Is your server running?";

const NEEDS_PROXY: &str =
    "Replay needs \"Route tunnels through Cloudfairy\" in Settings. Turn it on, then restart the tunnel.";

impl Tunnels {
    /// Starts the proxy for one run. Its requests go to the window as `tunnel-request` events.
    pub(super) fn launch_proxy<R: TauriRuntime>(
        &self,
        app: &AppHandle<R>,
        cfg: &TunnelConfig,
        run_id: u64,
    ) -> Result<ProxyHandle, String> {
        // Weak, because the proxy lives inside the tunnel map it reports to.
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let app = app.clone();
        let id = cfg.id.clone();
        let sink = move |event: Event| match event {
            Event::Request(entry) => {
                let _ = app.emit(EVENT_REQUEST, RequestEvent { id: &id, entry: &entry });
            }
            Event::Unreachable => {
                if let Some(inner) = weak.upgrade() {
                    Tunnels { inner }.warn_unreachable(&app, &id, run_id);
                }
            }
        };
        proxy::start(&ProxyConfig::from_tunnel(cfg), Arc::new(sink))
    }

    /// The proxy couldn't connect to the target. `watch` clears this once the target answers.
    fn warn_unreachable<R: TauriRuntime>(&self, app: &AppHandle<R>, id: &str, run_id: u64) {
        let mut map = self.map();
        let Some(rt) = map.get_mut(id).filter(|rt| rt.run_id == run_id && !rt.stopping) else {
            return;
        };
        if rt.view.warning.is_some() {
            return;
        }
        rt.view.warning = Some(UNREACHABLE_WARNING.into());
        rt.log(format!("[cloudfairy] {UNREACHABLE_WARNING}"));
        self.emit(app, id, rt);
    }

    /// Sends a recorded request to the local target again. The result joins the list.
    pub async fn replay(&self, id: &str, entry_id: u64) -> Result<RequestEntry, String> {
        let replay = {
            let map = self.map();
            let rt = map.get(id).ok_or("Tunnel not found.")?;
            match (&rt.proxy, &rt.capture) {
                (Some(proxy), _) => proxy.replay(entry_id),
                (None, Some(_)) => return Err("Start the tunnel to replay requests.".into()),
                (None, None) => return Err(NEEDS_PROXY.into()),
            }
        };
        replay.await
    }

    pub fn request_body(&self, id: &str, entry_id: u64, side: Side) -> Option<BodyView> {
        let capture = self.map().get(id)?.capture.clone()?;
        capture.body_view(entry_id, side)
    }
}
