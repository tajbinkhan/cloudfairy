//! Requests from outside the window: `cloudfairy://` links and the command line.
//!
//! A second launch (`cloudfairy start 3000`, or a link on Windows and Linux) reaches the
//! running app through the single-instance plugin. On macOS links arrive through the
//! deep-link plugin instead. Commands typed in a terminal run right away. Any web page can
//! open a link, so a link that would share a port only queues a question for the window,
//! and nothing starts until the user clicks Start.
//!
//! The window may not be loaded yet (on a cold start), so results wait in the `Inbox` until
//! the window takes them with `take_remote_items`. `EVENT_INBOX` tells a loaded window to look.

use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard},
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    background,
    cli::{self, Command},
    deep_link::{self, LinkAction},
    settings::TunnelConfig,
    transfer, AppState,
};

pub const EVENT_INBOX: &str = "remote-inbox";
/// Links waiting for an answer. Older ones are dropped, so a page can't pile them up.
const MAX_WAITING: usize = 5;
/// Items the window hasn't picked up yet.
const MAX_ITEMS: usize = 20;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Item {
    /// A link asked to start a tunnel. The window asks the user, then calls `answer_link_request`.
    #[serde(rename_all = "camelCase")]
    Confirm {
        id: u64,
        /// The local address without the scheme, e.g. `localhost:3000`.
        address: String,
        name: String,
        /// A saved tunnel already has this target, so nothing new is saved.
        existing: bool,
    },
    /// A result or error to show the user.
    Message { text: String },
}

struct Waiting {
    id: u64,
    target: String,
    name: Option<String>,
}

#[derive(Default)]
struct Queues {
    items: Vec<Item>,
    waiting: Vec<Waiting>,
    next_id: u64,
}

#[derive(Default)]
pub struct Inbox(Mutex<Queues>);

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn push(app: &AppHandle, item: Item) {
    {
        let inbox = app.state::<Inbox>();
        let mut q = inbox.lock();
        if q.items.len() >= MAX_ITEMS {
            q.items.remove(0);
        }
        q.items.push(item);
    }
    ping(app);
}

/// Tells the window to take new items and reload the tunnel list.
fn ping(app: &AppHandle) {
    let _ = app.emit(EVENT_INBOX, ());
}

fn say(app: &AppHandle, text: impl Into<String>) {
    push(app, Item::Message { text: text.into() });
}

/// Called once from setup: registers the link scheme where installers don't, listens for
/// links on macOS, and handles this launch's own arguments.
pub fn init(app: &AppHandle) {
    #[cfg(all(windows, debug_assertions))]
    {
        use tauri_plugin_deep_link::DeepLinkExt;
        let _ = app.deep_link().register_all();
    }
    // The .deb and .rpm packages register the scheme. An AppImage has to do it itself.
    #[cfg(target_os = "linux")]
    if cfg!(debug_assertions) || app.env().appimage.is_some() {
        use tauri_plugin_deep_link::DeepLinkExt;
        let _ = app.deep_link().register_all();
    }
    // Windows and Linux pass links as arguments (see `handle_args`); macOS sends an event.
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_deep_link::DeepLinkExt;
        let handle = app.clone();
        app.deep_link().on_open_url(move |event| {
            for url in event.urls() {
                handle_link(&handle, url.as_str());
            }
        });
        if let Ok(Some(urls)) = app.deep_link().get_current() {
            for url in urls {
                handle_link(app, url.as_str());
            }
        }
    }
    let args: Vec<String> = std::env::args().collect();
    handle_args(app, &args);
}

/// Handles a full argument list, program name first: this launch's, or a second launch's
/// forwarded by the single-instance plugin.
pub fn handle_args(app: &AppHandle, args: &[String]) {
    match cli::parse(args.get(1..).unwrap_or_default()) {
        Ok(None) => {}
        Ok(Some(command)) => run(app, command),
        Err(e) => {
            background::show_window(app);
            say(app, e);
        }
    }
}

fn run(app: &AppHandle, command: Command) {
    background::show_window(app);
    match command {
        Command::Link(link) => handle_link(app, &link),
        Command::Open => {}
        // Typed in the user's own terminal, so it starts without asking.
        Command::Start { target, name } => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let text = start_target(&app, &target, name).await;
                say(&app, text);
            });
        }
        Command::Stop { target } => say(app, stop_target(app, &target)),
        Command::StopAll => say(app, stop_all(app)),
    }
}

pub fn handle_link(app: &AppHandle, link: &str) {
    background::show_window(app);
    match deep_link::parse(link) {
        Ok(LinkAction::Open) => {}
        Ok(LinkAction::Start { port, name }) => ask_to_start(app, &format!("http://localhost:{port}"), name),
        Ok(LinkAction::Stop { port }) => say(app, stop_target(app, &format!("http://localhost:{port}"))),
        Err(e) => say(app, format!("Cloudfairy ignored a link. {e}")),
    }
}

fn saved_with_target(app: &AppHandle, target: &str) -> Option<TunnelConfig> {
    let state = app.state::<AppState>();
    let tunnels = state.settings.get().tunnels;
    tunnels.into_iter().find(|t| t.target == target)
}

/// Queues the question "A link asked to share … publicly. Start it?" for the window.
fn ask_to_start(app: &AppHandle, target: &str, name: Option<String>) {
    let saved = saved_with_target(app, target);
    if let Some(t) = &saved {
        if app.state::<AppState>().tunnels.is_active(&t.id) {
            return say(app, format!("{} is already running.", t.name));
        }
    }
    let address = transfer::default_name(target);
    {
        let inbox = app.state::<Inbox>();
        let mut q = inbox.lock();
        // The same link opened twice before the window looked asks once.
        let queued: HashSet<u64> = q
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Confirm { id, .. } => Some(*id),
                Item::Message { .. } => None,
            })
            .collect();
        if q.waiting.iter().any(|w| w.target == target && queued.contains(&w.id)) {
            return;
        }
        q.next_id += 1;
        let id = q.next_id;
        if q.waiting.len() >= MAX_WAITING {
            q.waiting.remove(0);
        }
        q.waiting.push(Waiting {
            id,
            target: target.to_string(),
            name: name.clone(),
        });
        let existing = saved.is_some();
        let name = saved.map(|t| t.name).or(name).unwrap_or_else(|| address.clone());
        if q.items.len() >= MAX_ITEMS {
            q.items.remove(0);
        }
        q.items.push(Item::Confirm { id, address, name, existing });
    }
    ping(app);
}

/// Everything the window hasn't shown yet.
#[tauri::command]
pub fn take_remote_items(inbox: State<Inbox>) -> Vec<Item> {
    std::mem::take(&mut inbox.lock().items)
}

/// The user's answer to a link's question. Only the request saved in Rust is acted on, so
/// the window can't change what a link asked for.
#[tauri::command]
pub async fn answer_link_request(app: AppHandle, id: u64, start: bool) {
    let request = {
        let inbox = app.state::<Inbox>();
        let mut q = inbox.lock();
        q.waiting
            .iter()
            .position(|w| w.id == id)
            .map(|i| q.waiting.remove(i))
    };
    // Unknown ids were dropped for being too old.
    let Some(request) = request else { return };
    if start {
        let text = start_target(&app, &request.target, request.name).await;
        say(&app, text);
    }
}

/// Starts the saved tunnel for `target`, saving a new one first if none has that target.
/// Returns what to tell the user. Start errors also show on the tunnel's card.
async fn start_target(app: &AppHandle, target: &str, name: Option<String>) -> String {
    let cfg = match find_or_add(app, target, name) {
        Ok(cfg) => cfg,
        Err(e) => return e,
    };
    let state = app.state::<AppState>();
    if state.tunnels.is_active(&cfg.id) {
        return format!("{} is already running.", cfg.name);
    }
    match crate::start_one(app, &cfg.id).await {
        Ok(_) => format!("Starting {}.", cfg.name),
        Err(e) => {
            if !state.tunnels.is_active(&cfg.id) {
                state.tunnels.fail(app, &cfg.id, e.clone());
            }
            format!("Couldn't start {}. {e}", cfg.name)
        }
    }
}

fn find_or_add(app: &AppHandle, target: &str, name: Option<String>) -> Result<TunnelConfig, String> {
    if let Some(cfg) = saved_with_target(app, target) {
        return Ok(cfg);
    }
    let state = app.state::<AppState>();
    let cfg = state.settings.update(|s| -> Result<TunnelConfig, String> {
        if let Some(t) = s.tunnels.iter().find(|t| t.target == target) {
            return Ok(t.clone());
        }
        let ids = s.tunnels.iter().map(|t| t.id.clone()).collect();
        let cfg = transfer::new_config(transfer::new_id(&ids), target, name)?;
        s.tunnels.push(cfg.clone());
        Ok(cfg)
    })??;
    background::refresh_tray(app);
    // The window loads the new card when it hears this.
    ping(app);
    Ok(cfg)
}

fn stopped_text(stopped: &[TunnelConfig]) -> Option<String> {
    match stopped {
        [] => None,
        [one] => Some(format!("Stopped {}.", one.name)),
        many => Some(format!("Stopped {} tunnels.", many.len())),
    }
}

fn stop_matching(app: &AppHandle, matches: impl Fn(&TunnelConfig) -> bool) -> Vec<TunnelConfig> {
    let state = app.state::<AppState>();
    let running: Vec<TunnelConfig> = state
        .settings
        .get()
        .tunnels
        .into_iter()
        .filter(|t| matches(t) && state.tunnels.is_active(&t.id))
        .collect();
    for t in &running {
        state.tunnels.stop(&t.id);
    }
    running
}

fn stop_target(app: &AppHandle, target: &str) -> String {
    let stopped = stop_matching(app, |t| t.target == target);
    stopped_text(&stopped)
        .unwrap_or_else(|| format!("No tunnel for {} is running.", transfer::default_name(target)))
}

fn stop_all(app: &AppHandle) -> String {
    stopped_text(&stop_matching(app, |_| true)).unwrap_or_else(|| "No tunnels are running.".into())
}
