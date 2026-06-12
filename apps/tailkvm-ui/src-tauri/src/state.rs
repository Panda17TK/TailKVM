//! Shared application state and session snapshots.
//!
//! First slice of the lib.rs decomposition: every long-lived handle the
//! Tauri commands and session loops share lives here, together with the
//! poison-tolerant snapshot helpers. Everything is `pub(crate)`: this is
//! internal plumbing, not crate API.

use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tailkvm_net::protocol::WireMessage;
use tokio::sync::mpsc;

pub(crate) const DEFAULT_TAILKVM_PORT: u16 = 47110;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TcpSessionSnapshot {
    pub(crate) role: String,
    pub(crate) listening: bool,
    pub(crate) listen_addr: Option<String>,
    pub(crate) connected: bool,
    pub(crate) peer_addr: Option<String>,
    pub(crate) peer_name: Option<String>,
    pub(crate) heartbeat_seq: u64,
    pub(crate) last_heartbeat_ms: Option<u64>,
    pub(crate) last_event: String,
    pub(crate) local_keyboard_layout: Option<String>,
    pub(crate) peer_keyboard_layout: Option<String>,
    pub(crate) keyboard_layout_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteControlState {
    pub(crate) active: bool,
    pub(crate) switch_edge: String,
    pub(crate) remote_width: i32,
    pub(crate) remote_height: i32,
    pub(crate) edge_margin: i32,
    /// When the seamless absolute-cursor engine is driving, the legacy
    /// return-edge detection in the controller session is disabled (return is
    /// decided locally by the combined-space model).
    pub(crate) seamless: bool,
}

impl Default for RemoteControlState {
    fn default() -> Self {
        Self {
            active: false,
            switch_edge: "right".to_string(),
            remote_width: 1920,
            remote_height: 1080,
            edge_margin: 3,
            seamless: false,
        }
    }
}

impl Default for TcpSessionSnapshot {
    fn default() -> Self {
        Self {
            role: "idle".to_string(),
            listening: false,
            listen_addr: None,
            connected: false,
            peer_addr: None,
            peer_name: None,
            heartbeat_seq: 0,
            last_heartbeat_ms: None,
            last_event: "Not started.".to_string(),
            local_keyboard_layout: None,
            peer_keyboard_layout: None,
            keyboard_layout_warning: None,
        }
    }
}

/// A named multi-screen controller session (roadmap B1.2): its reconnect flag
/// and the current outbound channel (rebuilt on each reconnect).
#[derive(Clone)]
pub(crate) struct ScreenSession {
    pub(crate) should_run: Arc<AtomicBool>,
    pub(crate) tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
}

/// Screen geometry a peer reported via `ScreenInfo`: virtual-screen size plus
/// monitor rects relative to the peer's virtual origin (the coordinate space
/// of `MouseSetPosition` offsets). `monitors` is empty for peers that predate
/// the field — clamping then degrades gracefully to the bounding box.
#[derive(Clone, Debug, Default)]
pub(crate) struct PeerScreen {
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) monitors: Vec<(i32, i32, i32, i32)>,
}

pub(crate) type PeerScreenMap = Arc<Mutex<HashMap<String, PeerScreen>>>;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) tcp: Arc<Mutex<TcpSessionSnapshot>>,
    pub(crate) receiver_running: Arc<AtomicBool>,
    /// True while a controller session should stay connected (drives
    /// auto-reconnect); cleared by an explicit disconnect.
    pub(crate) controller_should_run: Arc<AtomicBool>,
    /// Bumped on every connect_tcp_peer so a stale 1:1 supervisor (e.g. from a
    /// double-click) exits instead of fighting the new one for the same peer —
    /// which would churn the receiver's newest-wins slot and look like frequent
    /// disconnects.
    pub(crate) controller_generation: Arc<AtomicU64>,
    /// Whether the receiver accepts incoming controller connections (G1).
    pub(crate) accept_incoming: Arc<AtomicBool>,
    /// Recovery route: set by the emergency reset to abort the active inbound
    /// (being-controlled) session. The receiver loop polls this on a fast tick
    /// and drops the session, releasing every held key/button on the way out.
    pub(crate) receiver_abort: Arc<AtomicBool>,
    /// Named multi-screen controller sessions, keyed by screen name (B1.2).
    pub(crate) sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    /// Screen geometry reported by each peer via ScreenInfo (B1.7), keyed by
    /// screen name. Used by the router to size remote screens and by the
    /// seamless engine to clamp the cursor onto the peer's real monitors.
    pub(crate) screen_sizes: PeerScreenMap,
    /// True while the multi-screen router is active (B1.4).
    pub(crate) router_running: Arc<AtomicBool>,
    /// The live screen space the router reads each tick; swapped atomically by
    /// reconfigure_router without restarting the router (issue 1).
    pub(crate) router_space: Arc<Mutex<Option<Arc<tailkvm_win32::layout_graph::MultiScreenSpace>>>>,
    /// The router's fixed local screen name (set while running).
    pub(crate) router_local_name: Arc<Mutex<Option<String>>>,
    pub(crate) capture_running: Arc<AtomicBool>,
    pub(crate) mouse_hook_running: Arc<AtomicBool>,
    pub(crate) keyboard_hook_running: Arc<AtomicBool>,
    pub(crate) remote_control: Arc<Mutex<RemoteControlState>>,
    pub(crate) mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    pub(crate) keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    pub(crate) controller_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    /// Outbound channel for the receiver session, so this side can also push
    /// (e.g. clipboard) back to the controller — enables bidirectional sync.
    pub(crate) receiver_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    pub(crate) clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    pub(crate) clipboard_sync_running: Arc<AtomicBool>,
    pub(crate) clipboard_watch:
        Arc<Mutex<Option<tailkvm_win32::clipboard_watch::ClipboardWatchHandle>>>,
    pub(crate) raw_mouse_running: Arc<AtomicBool>,
    pub(crate) raw_mouse: Arc<Mutex<Option<tailkvm_win32::raw_input_mouse::RawMouseHandle>>>,
    /// When set, the keyboard forwarder resolves printable keys to Unicode on
    /// the controller's layout (JIS/US bridge) and drops IME-toggle keys.
    pub(crate) resolve_characters: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            tcp: Arc::new(Mutex::new(TcpSessionSnapshot::default())),
            receiver_running: Arc::new(AtomicBool::new(false)),
            controller_should_run: Arc::new(AtomicBool::new(false)),
            controller_generation: Arc::new(AtomicU64::new(0)),
            accept_incoming: Arc::new(AtomicBool::new(true)),
            receiver_abort: Arc::new(AtomicBool::new(false)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            screen_sizes: Arc::new(Mutex::new(HashMap::new())),
            router_running: Arc::new(AtomicBool::new(false)),
            router_space: Arc::new(Mutex::new(None)),
            router_local_name: Arc::new(Mutex::new(None)),
            capture_running: Arc::new(AtomicBool::new(false)),
            mouse_hook_running: Arc::new(AtomicBool::new(false)),
            keyboard_hook_running: Arc::new(AtomicBool::new(false)),
            remote_control: Arc::new(Mutex::new(RemoteControlState::default())),
            mouse_hook: Arc::new(Mutex::new(None)),
            keyboard_hook: Arc::new(Mutex::new(None)),
            controller_tx: Arc::new(Mutex::new(None)),
            receiver_tx: Arc::new(Mutex::new(None)),
            clipboard_guard: Arc::new(Mutex::new(
                tailkvm_win32::clipboard::ClipboardLoopGuard::new(),
            )),
            clipboard_sync_running: Arc::new(AtomicBool::new(false)),
            clipboard_watch: Arc::new(Mutex::new(None)),
            raw_mouse_running: Arc::new(AtomicBool::new(false)),
            raw_mouse: Arc::new(Mutex::new(None)),
            resolve_characters: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Shared state needed to start keyboard-hook forwarding. Bundling these
/// `AppState`-derived handles keeps `start_keyboard_hook_forwarding` to a few
/// arguments (was 9, tripping `clippy::too_many_arguments`).
#[derive(Clone)]
pub(crate) struct KeyboardForwardingContext {
    pub(crate) tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    pub(crate) keyboard_hook_running: Arc<AtomicBool>,
    pub(crate) keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    pub(crate) capture_running: Arc<AtomicBool>,
    pub(crate) mouse_hook_running: Arc<AtomicBool>,
    pub(crate) mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    pub(crate) remote_control: Arc<Mutex<RemoteControlState>>,
    pub(crate) resolve_characters: Arc<AtomicBool>,
}

impl AppState {
    pub(crate) fn keyboard_forwarding_context(&self) -> KeyboardForwardingContext {
        KeyboardForwardingContext {
            tcp_state: self.tcp.clone(),
            keyboard_hook_running: self.keyboard_hook_running.clone(),
            keyboard_hook: self.keyboard_hook.clone(),
            capture_running: self.capture_running.clone(),
            mouse_hook_running: self.mouse_hook_running.clone(),
            mouse_hook: self.mouse_hook.clone(),
            remote_control: self.remote_control.clone(),
            resolve_characters: self.resolve_characters.clone(),
        }
    }
}

pub(crate) fn tcp_snapshot(state: &Arc<Mutex<TcpSessionSnapshot>>) -> TcpSessionSnapshot {
    // Recover from a poisoned lock instead of panicking. If a session thread
    // ever panics while holding this mutex, `.expect()` here would turn a
    // one-off failure into a permanent, app-wide "TCP session error" on every
    // 2s poll (get_tcp_session_state). The snapshot is plain data, so reading
    // through the poison is safe.
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub(crate) fn update_tcp_state(
    state: &Arc<Mutex<TcpSessionSnapshot>>,
    update: impl FnOnce(&mut TcpSessionSnapshot),
) {
    if let Ok(mut snapshot) = state.lock() {
        update(&mut snapshot);
    }
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn local_machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-windows-machine".to_string())
}
