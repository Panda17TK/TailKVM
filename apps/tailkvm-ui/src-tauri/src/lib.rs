use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tailkvm_net::protocol::{decode_line, encode_line, WireMessage};
use tailkvm_win32::monitor::MonitorTopology;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, State, WindowEvent,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::{self, Duration},
};

const DEFAULT_TAILKVM_PORT: u16 = 47110;

#[derive(Debug, Serialize)]
struct TailnetStatus {
    backend_state: String,
    self_node: Option<TailnetNode>,
    peers: Vec<TailnetNode>,
    raw_peer_count: usize,
}

#[derive(Debug, Serialize)]
struct TailnetNode {
    id: String,
    host_name: String,
    dns_name: Option<String>,
    os: Option<String>,
    online: bool,
    active: Option<bool>,
    tailscale_ips: Vec<String>,
    user: Option<String>,
    relay: Option<String>,
    cur_addr: Option<String>,
    last_seen: Option<String>,
    tx_bytes: Option<u64>,
    rx_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct TcpSessionSnapshot {
    role: String,
    listening: bool,
    listen_addr: Option<String>,
    connected: bool,
    peer_addr: Option<String>,
    peer_name: Option<String>,
    heartbeat_seq: u64,
    last_heartbeat_ms: Option<u64>,
    last_event: String,
    local_keyboard_layout: Option<String>,
    peer_keyboard_layout: Option<String>,
    keyboard_layout_warning: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteControlState {
    active: bool,
    switch_edge: String,
    remote_width: i32,
    remote_height: i32,
    edge_margin: i32,
    /// When the seamless absolute-cursor engine is driving, the legacy
    /// return-edge detection in the controller session is disabled (return is
    /// decided locally by the combined-space model).
    seamless: bool,
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
struct ScreenSession {
    should_run: Arc<AtomicBool>,
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
}

#[derive(Clone)]
struct AppState {
    tcp: Arc<Mutex<TcpSessionSnapshot>>,
    receiver_running: Arc<AtomicBool>,
    /// True while a controller session should stay connected (drives
    /// auto-reconnect); cleared by an explicit disconnect.
    controller_should_run: Arc<AtomicBool>,
    /// Bumped on every connect_tcp_peer so a stale 1:1 supervisor (e.g. from a
    /// double-click) exits instead of fighting the new one for the same peer —
    /// which would churn the receiver's newest-wins slot and look like frequent
    /// disconnects.
    controller_generation: Arc<AtomicU64>,
    /// Whether the receiver accepts incoming controller connections (G1).
    accept_incoming: Arc<AtomicBool>,
    /// Named multi-screen controller sessions, keyed by screen name (B1.2).
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    /// Real virtual-screen size reported by each peer via ScreenInfo (B1.7),
    /// keyed by screen name. Used by the router to size remote screens.
    screen_sizes: Arc<Mutex<HashMap<String, (i32, i32)>>>,
    /// True while the multi-screen router is active (B1.4).
    router_running: Arc<AtomicBool>,
    /// The live screen space the router reads each tick; swapped atomically by
    /// reconfigure_router without restarting the router (issue 1).
    router_space: Arc<Mutex<Option<Arc<tailkvm_win32::layout_graph::MultiScreenSpace>>>>,
    /// The router's fixed local screen name (set while running).
    router_local_name: Arc<Mutex<Option<String>>>,
    capture_running: Arc<AtomicBool>,
    mouse_hook_running: Arc<AtomicBool>,
    keyboard_hook_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    controller_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    /// Outbound channel for the receiver session, so this side can also push
    /// (e.g. clipboard) back to the controller — enables bidirectional sync.
    receiver_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    clipboard_sync_running: Arc<AtomicBool>,
    clipboard_watch: Arc<Mutex<Option<tailkvm_win32::clipboard_watch::ClipboardWatchHandle>>>,
    raw_mouse_running: Arc<AtomicBool>,
    raw_mouse: Arc<Mutex<Option<tailkvm_win32::raw_input_mouse::RawMouseHandle>>>,
    /// When set, the keyboard forwarder resolves printable keys to Unicode on
    /// the controller's layout (JIS/US bridge) and drops IME-toggle keys.
    resolve_characters: Arc<AtomicBool>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            tcp: Arc::new(Mutex::new(TcpSessionSnapshot::default())),
            receiver_running: Arc::new(AtomicBool::new(false)),
            controller_should_run: Arc::new(AtomicBool::new(false)),
            controller_generation: Arc::new(AtomicU64::new(0)),
            accept_incoming: Arc::new(AtomicBool::new(true)),
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
struct KeyboardForwardingContext {
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    capture_running: Arc<AtomicBool>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    resolve_characters: Arc<AtomicBool>,
}

impl AppState {
    fn keyboard_forwarding_context(&self) -> KeyboardForwardingContext {
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

#[tauri::command]
fn get_app_status() -> String {
    format!("TailKVM v{} backend running.", env!("CARGO_PKG_VERSION"))
}

/// Toggle character-resolution mode for keyboard forwarding. When on, printable
/// keys are resolved to the controller's layout character and sent as Unicode
/// (JIS/US bridge), control/modifier/Win/Alt+Tab keys go through the physical
/// path, and IME-toggle keys (半角/全角 等) are dropped. Read live by the
/// forwarding loop, so it can be toggled during a session.
#[tauri::command]
async fn set_resolve_characters(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.resolve_characters.store(enabled, Ordering::SeqCst);
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = if enabled {
            "Character resolution ON (JIS/US bridge; IME toggle keys dropped).".to_string()
        } else {
            "Character resolution OFF (physical scan/vk forwarding).".to_string()
        };
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Raw Input mouse capture diagnostic (phase-A PoC). Observe-only: it counts
/// local HID-level relative deltas and reports them, but does NOT move the
/// cursor or inject anything. Used to validate the WM_INPUT pipeline on real
/// hardware before wiring raw deltas into remote-mode movement.
#[tauri::command]
async fn start_raw_mouse_diagnostic(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    if state.raw_mouse_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Raw mouse diagnostic is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let (delta_tx, delta_rx) = std::sync::mpsc::channel::<(i32, i32)>();

    let handle = match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(delta_tx) {
        Ok(handle) => handle,
        Err(err) => {
            state.raw_mouse_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = state
            .raw_mouse
            .lock()
            .map_err(|_| "raw mouse mutex poisoned".to_string())?;
        *guard = Some(handle);
    }

    let tcp_state = state.tcp.clone();
    let running = state.raw_mouse_running.clone();

    tauri::async_runtime::spawn(async move {
        let mut count: u64 = 0;
        let (mut sum_x, mut sum_y): (i64, i64) = (0, 0);

        while running.load(Ordering::SeqCst) {
            while let Ok((dx, dy)) = delta_rx.try_recv() {
                count += 1;
                sum_x += dx as i64;
                sum_y += dy as i64;

                if count.is_multiple_of(20) {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Raw mouse diagnostic: {count} relative events, sum=({sum_x}, {sum_y}). Observe-only, no injection."
                        );
                    });
                }
            }

            time::sleep(Duration::from_millis(5)).await;
        }

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event =
                format!("Raw mouse diagnostic stopped. {count} events, sum=({sum_x}, {sum_y}).");
        });
    });

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event =
            "Raw mouse diagnostic started (observe-only PoC; move the mouse).".to_string();
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn stop_raw_mouse_diagnostic(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.raw_mouse_running.store(false, Ordering::SeqCst);

    {
        let mut guard = state
            .raw_mouse
            .lock()
            .map_err(|_| "raw mouse mutex poisoned".to_string())?;
        *guard = None;
    }

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Raw mouse diagnostic stop requested.".to_string();
    });

    Ok(tcp_snapshot(&state.tcp))
}

/// Async + spawn_blocking so the win32 monitor enumeration runs on a worker
/// thread instead of Tauri's main/event-loop thread. EnumDisplayMonitors is
/// fast, but keeping OS calls off the UI thread avoids any chance of stalling
/// the event loop during startup. The win32 calls are thread-safe.
#[tauri::command]
async fn get_windows_monitor_topology() -> Result<MonitorTopology, String> {
    tokio::task::spawn_blocking(tailkvm_win32::monitor::get_monitor_topology)
        .await
        .map_err(|e| format!("monitor topology task failed: {e}"))?
}

#[tauri::command]
fn get_keyboard_layout() -> Result<tailkvm_win32::keyboard_layout::KeyboardLayoutInfo, String> {
    Ok(tailkvm_win32::keyboard_layout::current_keyboard_layout())
}

#[tauri::command]
async fn get_tcp_session_state(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
fn install_firewall_rule(
    port: Option<u16>,
    remote_address: Option<String>,
) -> Result<String, String> {
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    tailkvm_win32::firewall::install_firewall_rule(port, remote_address)
}

#[tauri::command]
async fn start_mouse_hook_capture(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let snapshot = tcp_snapshot(&state.tcp);

    if !snapshot.connected {
        return Err("No active TCP connection. Connect to a peer first.".to_string());
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller channel. Connect to a peer first.".to_string());
    };

    start_mouse_hook_forwarding(
        SenderTarget::Fixed(sender),
        state.tcp.clone(),
        state.mouse_hook_running.clone(),
        state.mouse_hook.clone(),
        "manual",
    )?;

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn stop_mouse_hook_capture(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    stop_mouse_hook_forwarding(
        state.mouse_hook_running.clone(),
        state.mouse_hook.clone(),
        state.tcp.clone(),
        "manual",
    )?;

    Ok(tcp_snapshot(&state.tcp))
}

/// Where hook-forwarded input is sent. `Fixed` targets one session (1:1);
/// `Active` resolves the current target at send time so the multi-screen router
/// can switch screens without restarting the hooks (roadmap B1.3). A missing
/// active target drops the event without erroring (the hook keeps running).
#[derive(Clone)]
enum SenderTarget {
    Fixed(mpsc::UnboundedSender<WireMessage>),
    /// Resolved at send time by the multi-screen router (roadmap B1.4).
    Active(Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>),
}

impl SenderTarget {
    fn send(&self, message: WireMessage) -> Result<(), ()> {
        match self {
            SenderTarget::Fixed(sender) => sender.send(message).map_err(|_| ()),
            SenderTarget::Active(slot) => {
                if let Ok(guard) = slot.lock() {
                    if let Some(sender) = guard.as_ref() {
                        return sender.send(message).map_err(|_| ());
                    }
                }
                Ok(())
            }
        }
    }
}

fn start_mouse_hook_forwarding(
    sender: SenderTarget,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    label: &'static str,
) -> Result<(), String> {
    if mouse_hook_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!("Mouse hook capture is already running. mode={label}");
        });
        return Ok(());
    }

    let (event_tx, event_rx) =
        std::sync::mpsc::channel::<tailkvm_win32::mouse_hook::MouseHookEvent>();

    let hook = match tailkvm_win32::mouse_hook::start_mouse_hook(event_tx) {
        Ok(hook) => hook,
        Err(err) => {
            mouse_hook_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = mouse_hook
            .lock()
            .map_err(|_| "mouse hook mutex poisoned".to_string())?;
        *guard = Some(hook);
    }

    let tcp_state_for_task = tcp_state.clone();
    let mouse_hook_running_for_task = mouse_hook_running.clone();
    let mouse_hook_for_task = mouse_hook.clone();

    std::thread::spawn(move || {
        let mut event_count: u64 = 0;
        let mut pressed_buttons: Vec<String> = Vec::new();

        while mouse_hook_running_for_task.load(Ordering::SeqCst) {
            // Block until an event arrives (≈0ms added latency); the timeout
            // only bounds how long we wait before re-checking the stop flag.
            let event = match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            let message = match event {
                tailkvm_win32::mouse_hook::MouseHookEvent::Button { button, down } => {
                    track_button_press(&mut pressed_buttons, &button, down);
                    WireMessage::MouseButton { button, down }
                }
                tailkvm_win32::mouse_hook::MouseHookEvent::Wheel { delta, horizontal } => {
                    WireMessage::MouseWheel { delta, horizontal }
                }
            };

            if sender.send(message.clone()).is_err() {
                mouse_hook_running_for_task.store(false, Ordering::SeqCst);
                update_tcp_state(&tcp_state_for_task, |snapshot| {
                    snapshot.connected = false;
                    snapshot.last_event =
                        "Mouse hook capture stopped: controller channel closed.".to_string();
                });
                break;
            }

            event_count += 1;

            update_tcp_state(&tcp_state_for_task, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = true;
                snapshot.last_event = format!(
                    "Mouse hook event forwarded. mode={label}, count={}, event={message:?}",
                    event_count
                );
            });
        }

        // Always uninstall the hook when the loop ends (failsafe, peer
        // disconnect, or manual stop) so local click/wheel input is no longer
        // suppressed. Without this, an internal exit (e.g. controller channel
        // closed) would leave the low-level hook installed and the local mouse
        // buttons captured — a lockout.
        if let Ok(mut guard) = mouse_hook_for_task.lock() {
            *guard = None;
        }

        for button in pressed_buttons.drain(..) {
            let _ = sender.send(WireMessage::MouseButton {
                button,
                down: false,
            });
        }

        update_tcp_state(&tcp_state_for_task, |snapshot| {
            snapshot.last_event =
                format!("Mouse hook capture stopped. mode={label}, events={event_count}. Released stuck buttons.");
        });
    });

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Mouse hook capture started. mode={label}. Local click/wheel events are suppressed."
        );
    });

    Ok(())
}

fn stop_mouse_hook_forwarding(
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    label: &'static str,
) -> Result<(), String> {
    mouse_hook_running.store(false, Ordering::SeqCst);

    {
        let mut guard = mouse_hook
            .lock()
            .map_err(|_| "mouse hook mutex poisoned".to_string())?;
        *guard = None;
    }

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!("Mouse hook capture stop requested. mode={label}");
    });

    Ok(())
}

#[tauri::command]
async fn start_keyboard_hook_capture(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let snapshot = tcp_snapshot(&state.tcp);

    if !snapshot.connected {
        return Err("No active TCP connection. Connect to a peer first.".to_string());
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller channel. Connect to a peer first.".to_string());
    };

    start_keyboard_hook_forwarding(
        &state.keyboard_forwarding_context(),
        SenderTarget::Fixed(sender),
        "manual",
    )?;

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn stop_keyboard_hook_capture(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    stop_keyboard_hook_forwarding(
        state.keyboard_hook_running.clone(),
        state.keyboard_hook.clone(),
        state.tcp.clone(),
        "manual",
    )?;

    Ok(tcp_snapshot(&state.tcp))
}

/// Generation counter for the keyboard-forward thread. Each successful call to
/// `start_keyboard_hook_forwarding` claims a new generation; the spawned thread
/// only resets the shared running flag / hook (and keeps looping) while it is
/// still the current generation. This prevents a superseded thread — e.g. after
/// a quick return-then-cross, which the multi-edge crossing makes more frequent
/// — from clearing the flag/hook that a newer thread owns, which silently
/// dropped keyboard forwarding (keys then typed locally instead of the peer).
static KEYBOARD_HOOK_GENERATION: AtomicU64 = AtomicU64::new(0);

fn start_keyboard_hook_forwarding(
    ctx: &KeyboardForwardingContext,
    sender: SenderTarget,
    label: &'static str,
) -> Result<(), String> {
    let tcp_state = ctx.tcp_state.clone();
    let keyboard_hook_running = ctx.keyboard_hook_running.clone();
    let keyboard_hook = ctx.keyboard_hook.clone();
    let capture_running = ctx.capture_running.clone();
    let mouse_hook_running = ctx.mouse_hook_running.clone();
    let mouse_hook = ctx.mouse_hook.clone();
    let remote_control = ctx.remote_control.clone();
    let resolve_characters = ctx.resolve_characters.clone();

    if keyboard_hook_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!("Keyboard hook capture is already running. mode={label}");
        });
        return Ok(());
    }

    // Claim a generation. The spawned thread owns the shared flag/hook only
    // while this stays the current generation (see KEYBOARD_HOOK_GENERATION).
    let my_gen = KEYBOARD_HOOK_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);

    let (event_tx, event_rx) =
        std::sync::mpsc::channel::<tailkvm_win32::keyboard_hook::KeyboardHookEvent>();

    let hook = match tailkvm_win32::keyboard_hook::start_keyboard_hook(event_tx) {
        Ok(hook) => hook,
        Err(err) => {
            keyboard_hook_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = keyboard_hook
            .lock()
            .map_err(|_| "keyboard hook mutex poisoned".to_string())?;
        *guard = Some(hook);
    }

    let tcp_state_for_task = tcp_state.clone();
    let keyboard_hook_running_for_task = keyboard_hook_running.clone();
    let keyboard_hook_for_task = keyboard_hook.clone();

    std::thread::spawn(move || {
        let mut event_count: u64 = 0;
        let mut pressed_keys: Vec<(u16, u16, bool)> = Vec::new();
        // Command-modifier state tracked from the event stream, used to route
        // keys when character resolution is enabled. Shift is folded into
        // character resolution rather than treated as a command modifier.
        let mut ctrl_down = false;
        let mut alt_down = false;
        let mut win_down = false;
        let mut shift_down = false;

        while keyboard_hook_running_for_task.load(Ordering::SeqCst)
            && KEYBOARD_HOOK_GENERATION.load(Ordering::SeqCst) == my_gen
        {
            // Block until an event arrives (≈0ms added latency); the timeout
            // only bounds how long we wait before re-checking the stop flag.
            let event = match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            match event {
                tailkvm_win32::keyboard_hook::KeyboardHookEvent::Failsafe => {
                    keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
                    capture_running.store(false, Ordering::SeqCst);

                    let _ = stop_mouse_hook_forwarding(
                        mouse_hook_running.clone(),
                        mouse_hook.clone(),
                        tcp_state_for_task.clone(),
                        "failsafe-keyboard",
                    );

                    if let Ok(mut remote_state) = remote_control.lock() {
                        remote_state.active = false;
                    }

                    update_tcp_state(&tcp_state_for_task, |snapshot| {
                        snapshot.last_event =
                            "Keyboard failsafe Ctrl+Alt+Pause received. All captures stopping."
                                .to_string();
                    });

                    break;
                }
                tailkvm_win32::keyboard_hook::KeyboardHookEvent::Key {
                    vk,
                    scan_code,
                    down,
                    extended,
                } => {
                    // Track modifier state from the stream.
                    if let Some(modifier) = tailkvm_win32::key_class::modifier_kind(vk) {
                        match modifier {
                            tailkvm_win32::key_class::Modifier::Ctrl => ctrl_down = down,
                            tailkvm_win32::key_class::Modifier::Alt => alt_down = down,
                            tailkvm_win32::key_class::Modifier::Win => win_down = down,
                            tailkvm_win32::key_class::Modifier::Shift => shift_down = down,
                        }
                    }

                    // Helper to forward a physical key event and track it for
                    // stuck-key release.
                    let physical = |pressed: &mut Vec<(u16, u16, bool)>| {
                        track_key_press(pressed, vk, scan_code, extended, down);
                        WireMessage::KeyboardKey {
                            vk,
                            scan_code,
                            down,
                            extended,
                        }
                    };

                    let message: Option<WireMessage> =
                        if resolve_characters.load(Ordering::SeqCst) {
                            match tailkvm_win32::key_class::classify_key(
                                vk, ctrl_down, alt_down, win_down,
                            ) {
                                // IME toggle/conversion keys are handled locally,
                                // never forwarded (receiver stays direct-input).
                                tailkvm_win32::key_class::KeyRoute::ImeLocal => None,
                                tailkvm_win32::key_class::KeyRoute::Physical => {
                                    Some(physical(&mut pressed_keys))
                                }
                                tailkvm_win32::key_class::KeyRoute::Character => {
                                    if down {
                                        match tailkvm_win32::keyboard::resolve_key_text(
                                            vk, scan_code, shift_down, false,
                                        ) {
                                            Some(text) => Some(WireMessage::KeyboardText { text }),
                                            // Dead key / unresolved: fall back to
                                            // the physical key (tracked for release).
                                            None => Some(physical(&mut pressed_keys)),
                                        }
                                    } else if pressed_keys.iter().any(|(k, s, e)| {
                                        *k == vk && *s == scan_code && *e == extended
                                    }) {
                                        // Release a physical-fallback key-down.
                                        Some(physical(&mut pressed_keys))
                                    } else {
                                        // Character key-up: Unicode was self-contained.
                                        None
                                    }
                                }
                            }
                        } else {
                            // Legacy behavior: always reproduce the physical key.
                            Some(physical(&mut pressed_keys))
                        };

                    let Some(message) = message else {
                        continue;
                    };

                    if sender.send(message.clone()).is_err() {
                        keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
                        update_tcp_state(&tcp_state_for_task, |snapshot| {
                            snapshot.connected = false;
                            snapshot.last_event =
                                "Keyboard hook capture stopped: controller channel closed."
                                    .to_string();
                        });
                        break;
                    }

                    event_count += 1;

                    update_tcp_state(&tcp_state_for_task, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Keyboard hook event forwarded. mode={label}, count={}, event={message:?}",
                            event_count
                        );
                    });
                }
            }
        }

        // Reset the shared flag and uninstall the hook ONLY if this thread is
        // still the current generation. A superseded thread (a newer cross has
        // already started its own keyboard thread) must NOT clear the flag/hook
        // it no longer owns — doing so silently dropped keyboard forwarding
        // (keys typed locally). When still current, clearing the flag also fixes
        // the original stuck-true case (the `Disconnected` break) so the next
        // crossing can re-install the hook; uninstalling stops local keyboard
        // suppression after a failsafe/disconnect.
        if KEYBOARD_HOOK_GENERATION.load(Ordering::SeqCst) == my_gen {
            keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
            if let Ok(mut guard) = keyboard_hook_for_task.lock() {
                *guard = None;
            }
        }

        for (vk, scan_code, extended) in pressed_keys.drain(..) {
            let _ = sender.send(WireMessage::KeyboardKey {
                vk,
                scan_code,
                down: false,
                extended,
            });
        }

        update_tcp_state(&tcp_state_for_task, |snapshot| {
            snapshot.last_event = format!(
                "Keyboard hook capture stopped. mode={label}, events={event_count}. Released stuck keys."
            );
        });
    });

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Keyboard hook capture started. mode={label}. Local keyboard input is suppressed. Ctrl+Alt+Pause stops all capture."
        );
    });

    Ok(())
}

fn stop_keyboard_hook_forwarding(
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    label: &'static str,
) -> Result<(), String> {
    keyboard_hook_running.store(false, Ordering::SeqCst);

    {
        let mut guard = keyboard_hook
            .lock()
            .map_err(|_| "keyboard hook mutex poisoned".to_string())?;
        *guard = None;
    }

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!("Keyboard hook capture stop requested. mode={label}");
    });

    Ok(())
}

#[tauri::command]
async fn send_test_keyboard_text(
    text: String,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let text = text.chars().take(200).collect::<String>();

    if text.is_empty() {
        return Err("keyboard text is empty.".to_string());
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    sender
        .send(WireMessage::KeyboardText { text: text.clone() })
        .map_err(|e| format!("failed to queue keyboard text: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued KeyboardText: {text}");
    });

    Ok(tcp_snapshot(&state.tcp))
}

/// Pick the active outbound channel: the controller channel if connected as a
/// controller, otherwise the receiver channel. Used so clipboard sync works in
/// either role (bidirectional).
/// Broadcast a clipboard text to every connected peer: all named multi-screen
/// sessions plus the legacy 1:1 controller/receiver channels (roadmap B1.5).
/// Returns how many peers it was sent to.
fn broadcast_clipboard(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    controller_tx: &Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    receiver_tx: &Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    text: &str,
) -> usize {
    let message = WireMessage::ClipboardText {
        text: text.to_string(),
    };
    let mut sent = 0;

    if let Ok(map) = sessions.lock() {
        for session in map.values() {
            if let Ok(tx) = session.tx.lock() {
                if let Some(sender) = tx.as_ref() {
                    if sender.send(message.clone()).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
    }

    for slot in [controller_tx, receiver_tx] {
        if let Ok(guard) = slot.lock() {
            if let Some(sender) = guard.as_ref() {
                if sender.send(message.clone()).is_ok() {
                    sent += 1;
                }
            }
        }
    }

    sent
}

/// Relay a clipboard text received from `origin` to every *other* named session
/// (roadmap B1.5 client->sibling relay), making the server a clipboard hub.
/// Returns how many siblings it was sent to.
fn relay_clipboard(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    origin: &str,
    text: &str,
) -> usize {
    let message = WireMessage::ClipboardText {
        text: text.to_string(),
    };
    let mut sent = 0;
    if let Ok(map) = sessions.lock() {
        for (name, session) in map.iter() {
            if name == origin {
                continue;
            }
            if let Ok(tx) = session.tx.lock() {
                if let Some(sender) = tx.as_ref() {
                    if sender.send(message.clone()).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
    }
    sent
}

/// Enable/disable automatic bidirectional clipboard sync (roadmap D1). When on,
/// a clipboard-change watcher forwards local text changes to the peer; the echo
/// guard suppresses re-broadcasting content we just applied. Default off.
#[tauri::command]
async fn set_clipboard_sync(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    if !enabled {
        state.clipboard_sync_running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = state.clipboard_watch.lock() {
            *guard = None;
        }
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard sync OFF.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    if state.clipboard_sync_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard sync is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let (change_tx, change_rx) = std::sync::mpsc::channel::<()>();
    let handle = match tailkvm_win32::clipboard_watch::start_clipboard_watch(change_tx) {
        Ok(handle) => handle,
        Err(err) => {
            state.clipboard_sync_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = state
            .clipboard_watch
            .lock()
            .map_err(|_| "clipboard watch mutex poisoned".to_string())?;
        *guard = Some(handle);
    }

    let controller_tx = state.controller_tx.clone();
    let receiver_tx = state.receiver_tx.clone();
    let sessions = state.sessions.clone();
    let clipboard_guard = state.clipboard_guard.clone();
    let tcp_state = state.tcp.clone();
    let running = state.clipboard_sync_running.clone();

    std::thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            match change_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => {
                    let text = match tailkvm_win32::clipboard::get_clipboard_text() {
                        Ok(Some(text)) if !text.is_empty() => text,
                        _ => continue,
                    };
                    let text = text.chars().take(100_000).collect::<String>();

                    let should_send = {
                        match clipboard_guard.lock() {
                            Ok(mut guard) => guard.should_broadcast(&text),
                            Err(_) => false,
                        }
                    };
                    if !should_send {
                        continue;
                    }

                    let sent = broadcast_clipboard(&sessions, &controller_tx, &receiver_tx, &text);
                    if sent > 0 {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!(
                                "Clipboard change auto-synced ({} chars) to {sent} peer(s).",
                                text.chars().count()
                            );
                        });
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Clipboard sync ON (bidirectional auto).".to_string();
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn send_clipboard_text(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    let text = match tailkvm_win32::clipboard::get_clipboard_text()? {
        Some(text) if !text.is_empty() => text,
        _ => return Err("Local clipboard has no text to send.".to_string()),
    };

    // Cap to a sane size so a huge paste can't flood the control link.
    let text = text.chars().take(100_000).collect::<String>();

    // Skip resending content identical to what we last sent/applied (echo guard).
    let should_send = {
        let mut guard = state
            .clipboard_guard
            .lock()
            .map_err(|_| "clipboard guard mutex poisoned".to_string())?;
        guard.should_broadcast(&text)
    };

    if !should_send {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard unchanged since last send; skipped.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    sender
        .send(WireMessage::ClipboardText { text: text.clone() })
        .map_err(|e| format!("failed to queue clipboard text: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued ClipboardText: {} chars", text.chars().count());
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn send_test_key_tap(
    key: String,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let key = key.trim().to_lowercase();

    let Some((vk, scan_code, extended, label)) = key_to_test_key(&key) else {
        return Err(format!("unsupported test key: {key}"));
    };

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    sender
        .send(WireMessage::KeyboardKey {
            vk,
            scan_code,
            down: true,
            extended,
        })
        .map_err(|e| format!("failed to queue key down: {e}"))?;

    time::sleep(Duration::from_millis(25)).await;

    sender
        .send(WireMessage::KeyboardKey {
            vk,
            scan_code,
            down: false,
            extended,
        })
        .map_err(|e| format!("failed to queue key up: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued KeyboardKey tap: {label}");
    });

    Ok(tcp_snapshot(&state.tcp))
}

/// Track a mouse button down/up in `pressed`, de-duplicating repeated `down`
/// events so each held button is recorded exactly once. When capture stops the
/// caller drains `pressed` to release every still-held button on the receiver,
/// preventing a stuck button. Releasing an unpressed button is a no-op.
fn track_button_press(pressed: &mut Vec<String>, button: &str, down: bool) {
    if down {
        if !pressed.iter().any(|value| value == button) {
            pressed.push(button.to_string());
        }
    } else {
        pressed.retain(|value| value != button);
    }
}

/// Track a keyboard key down/up in `pressed`, keyed by `(vk, scan_code,
/// extended)` and de-duplicating repeated `down` events. Mirrors
/// [`track_button_press`] so still-held keys can be released exactly once when
/// capture stops, preventing a stuck key.
fn track_key_press(
    pressed: &mut Vec<(u16, u16, bool)>,
    vk: u16,
    scan_code: u16,
    extended: bool,
    down: bool,
) {
    let key = (vk, scan_code, extended);
    if down {
        if !pressed.contains(&key) {
            pressed.push(key);
        }
    } else {
        pressed.retain(|entry| entry != &key);
    }
}

fn key_to_test_key(key: &str) -> Option<(u16, u16, bool, &'static str)> {
    match key {
        "enter" | "return" => Some((0x0D, 0, false, "Enter")),
        "backspace" | "bs" => Some((0x08, 0, false, "Backspace")),
        "tab" => Some((0x09, 0, false, "Tab")),
        "escape" | "esc" => Some((0x1B, 0, false, "Escape")),
        "space" => Some((0x20, 0, false, "Space")),
        "left" => Some((0x25, 0, true, "ArrowLeft")),
        "up" => Some((0x26, 0, true, "ArrowUp")),
        "right" => Some((0x27, 0, true, "ArrowRight")),
        "down" => Some((0x28, 0, true, "ArrowDown")),
        "delete" | "del" => Some((0x2E, 0, true, "Delete")),
        _ => None,
    }
}

#[tauri::command]
async fn send_test_mouse_double_click(
    button: String,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let button = button.trim().to_lowercase();

    if !matches!(button.as_str(), "left" | "right" | "middle" | "x1" | "x2") {
        return Err(format!("unsupported mouse button: {button}"));
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    for click_index in 1..=2 {
        sender
            .send(WireMessage::MouseButton {
                button: button.clone(),
                down: true,
            })
            .map_err(|e| format!("failed to queue double click down: {e}"))?;

        time::sleep(Duration::from_millis(35)).await;

        sender
            .send(WireMessage::MouseButton {
                button: button.clone(),
                down: false,
            })
            .map_err(|e| format!("failed to queue double click up: {e}"))?;

        if click_index == 1 {
            time::sleep(Duration::from_millis(70)).await;
        }
    }

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued MouseButton double click: {button}");
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn send_test_mouse_click(
    button: String,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let button = button.trim().to_lowercase();

    if !matches!(button.as_str(), "left" | "right" | "middle" | "x1" | "x2") {
        return Err(format!("unsupported mouse button: {button}"));
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    sender
        .send(WireMessage::MouseButton {
            button: button.clone(),
            down: true,
        })
        .map_err(|e| format!("failed to queue mouse button down: {e}"))?;

    sender
        .send(WireMessage::MouseButton {
            button: button.clone(),
            down: false,
        })
        .map_err(|e| format!("failed to queue mouse button up: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued MouseButton click: {button}");
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn send_test_mouse_move(
    dx: Option<i32>,
    dy: Option<i32>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let dx = dx.unwrap_or(80);
    let dy = dy.unwrap_or(0);

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    sender
        .send(WireMessage::MouseMove { dx, dy })
        .map_err(|e| format!("failed to queue mouse move message: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued MouseMove dx={dx}, dy={dy}");
    });

    Ok(tcp_snapshot(&state.tcp))
}

/// Owned inputs for the seamless absolute-cursor capture engine (roadmap A1).
struct SeamlessArgs {
    sender: mpsc::UnboundedSender<WireMessage>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    resolve_characters: Arc<AtomicBool>,
    /// Remote virtual-screen sizes keyed by peer machine name (populated from
    /// ScreenInfo). Used to map the cursor onto the peer's real screen.
    screen_sizes: Arc<Mutex<HashMap<String, (i32, i32)>>>,
    /// Pointer-speed multiplier applied to raw HID deltas while controlling the
    /// remote. Raw input is otherwise integrated 1:1 into remote pixels, which
    /// feels slow next to the local cursor (which has OS pointer ballistics).
    gain: f64,
    /// Physical-pixel rect (l, t, r, b) of the local monitor the peer is pinned
    /// to in the position editor. When set, only that monitor's edge crosses.
    /// None = any monitor the cursor is on may cross.
    attach_monitor: Option<(i32, i32, i32, i32)>,
    /// Physical-pixel rect (l, t, r, b) of the peer's screen as positioned in the
    /// virtual layout by the position editor. When set, the cursor crosses on ANY
    /// local-monitor edge this rect is flush against — so a corner placement that
    /// touches both a vertical and a horizontal monitor edge crosses on either.
    peer_rect: Option<(i32, i32, i32, i32)>,
    /// All local monitor rects, for the outer-edge check (never cross at an
    /// interior boundary where a neighbouring local monitor is).
    local_monitors: Vec<(i32, i32, i32, i32)>,
    local_rect: tailkvm_win32::screen_space::Rect,
    lock_x: i32,
    lock_y: i32,
    switch_edge: String,
    edge_margin: i32,
    remote_width: i32,
    remote_height: i32,
    interval_ms: u64,
    edge_dwell_ms: u64,
    dead_corner_px: i32,
}

/// Whether `edge` of monitor `mon` faces the outer boundary — i.e. no other
/// local monitor is adjacent along that edge. Only outer edges should cross to
/// the remote; an interior edge means the cursor flows into the neighbour.
/// Whether the peer rectangle `peer` is flush-adjacent to monitor `mon` on
/// `edge` (touching that side, with overlap along it). Mirrors `is_outer_edge`
/// but tests adjacency to the peer rect instead of to neighbouring monitors.
fn peer_adjacent(
    mon: (i32, i32, i32, i32),
    edge: tailkvm_win32::screen_space::Edge,
    peer: (i32, i32, i32, i32),
) -> bool {
    use tailkvm_win32::screen_space::Edge;
    let (ml, mt, mr, mb) = mon;
    let (pl, pt, pr, pb) = peer;
    let tol = 6;
    let x_overlap = mr.min(pr) - ml.max(pl);
    let y_overlap = mb.min(pb) - mt.max(pt);
    match edge {
        Edge::Bottom => (pt - mb).abs() <= tol && x_overlap > 0,
        Edge::Top => (mt - pb).abs() <= tol && x_overlap > 0,
        Edge::Right => (pl - mr).abs() <= tol && y_overlap > 0,
        Edge::Left => (ml - pr).abs() <= tol && y_overlap > 0,
    }
}

fn is_outer_edge(
    mon: (i32, i32, i32, i32),
    edge: tailkvm_win32::screen_space::Edge,
    monitors: &[(i32, i32, i32, i32)],
) -> bool {
    use tailkvm_win32::screen_space::Edge;
    let (ml, mt, mr, mb) = mon;
    let tol = 2;
    !monitors.iter().any(|&(nl, nt, nr, nb)| {
        if (nl, nt, nr, nb) == mon {
            return false;
        }
        match edge {
            Edge::Bottom => (nt - mb).abs() <= tol && nl < mr && nr > ml,
            Edge::Top => (nb - mt).abs() <= tol && nl < mr && nr > ml,
            Edge::Right => (nl - mr).abs() <= tol && nt < mb && nb > mt,
            Edge::Left => (nr - ml).abs() <= tol && nt < mb && nb > mt,
        }
    })
}

/// Seamless absolute-cursor capture (roadmap A1/E1). In the local region the
/// real cursor is followed and the configured edge is watched; on crossing,
/// control transfers to the remote and HID relative deltas (Raw Input) drive a
/// logical cursor in the combined space, sent to the receiver as absolute
/// `MouseSetPosition`. Returning is decided locally by the model (no receiver
/// echo), so there is no warp-feedback or drift. Opt-in; legacy modes untouched.
///
/// NOTE: runtime-unvalidated PoC — needs two-machine verification.
async fn run_seamless_capture(a: SeamlessArgs) {
    use tailkvm_win32::screen_space::{
        CombinedSpace, CursorState, Edge, Rect as SsRect, Region, SwitchGuard,
    };

    let edge = Edge::from_label(&a.switch_edge);
    // `combined` is rebuilt on each local->remote crossing with the monitor the
    // cursor is actually on and the peer's latest real screen size; this initial
    // value is only a placeholder until the first crossing.
    let mut combined = CombinedSpace::new(
        a.local_rect,
        SsRect::new(0, 0, a.remote_width, a.remote_height),
        edge,
    );
    let mut switch_guard = SwitchGuard::new(a.edge_dwell_ms, a.interval_ms);

    // Raw Input is required: the remote region integrates HID deltas without
    // moving the local cursor.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(i32, i32)>();
    let _raw_handle = match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(raw_tx) {
        Ok(handle) => handle,
        Err(err) => {
            a.capture_running.store(false, Ordering::SeqCst);
            update_tcp_state(&a.tcp_state, |snapshot| {
                snapshot.last_event =
                    format!("Seamless mode requires Raw Input, unavailable: {err}");
            });
            return;
        }
    };

    let keyboard_ctx = KeyboardForwardingContext {
        tcp_state: a.tcp_state.clone(),
        keyboard_hook_running: a.keyboard_hook_running.clone(),
        keyboard_hook: a.keyboard_hook.clone(),
        capture_running: a.capture_running.clone(),
        mouse_hook_running: a.mouse_hook_running.clone(),
        mouse_hook: a.mouse_hook.clone(),
        remote_control: a.remote_control.clone(),
        resolve_characters: a.resolve_characters.clone(),
    };

    let mut state = CursorState {
        region: Region::Local,
        x: a.lock_x,
        y: a.lock_y,
    };
    let mut remote_active = false;
    let mut sent_count: u64 = 0;
    // Carry sub-pixel remainder of the gain-scaled deltas so slow movements are
    // not lost to rounding (keeps the remote cursor smooth at low speed).
    let mut frac_x = 0.0f64;
    let mut frac_y = 0.0f64;
    let gain = if a.gain.is_finite() && a.gain > 0.0 {
        a.gain
    } else {
        1.0
    };

    update_tcp_state(&a.tcp_state, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!(
            "Seamless mode armed (gain {:.2}). Cross the {} edge to control the remote ({}x{}).",
            gain, a.switch_edge, a.remote_width, a.remote_height
        );
    });

    while a.capture_running.load(Ordering::SeqCst) {
        if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
            update_tcp_state(&a.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Seamless capture stopped by Ctrl+Alt+Pause failsafe.".to_string();
            });
            break;
        }

        if !remote_active {
            // Local region: follow the real cursor and watch the switch edge.
            let cur = match tailkvm_win32::cursor::get_cursor_position() {
                Ok(position) => position,
                Err(_) => {
                    time::sleep(Duration::from_millis(a.interval_ms)).await;
                    continue;
                }
            };
            while raw_rx.try_recv().is_ok() {} // discard deltas while local

            // Detect the switch edge against the monitor the cursor is currently
            // on, not the whole virtual screen. In a mixed multi-monitor layout
            // the virtual-screen edge is unreachable on shorter monitors, so the
            // crossing would never fire there.
            let (m_left, m_top, m_right, m_bottom) =
                tailkvm_win32::monitor::monitor_rect_at_point(cur.x, cur.y);

            // Multi-edge crossing. The peer occupies a virtual rectangle; cross
            // on ANY edge of the current monitor the peer rect is flush against,
            // so a corner placement touching both a vertical and a horizontal
            // monitor edge crosses on either. Without a peer rect, fall back to
            // the single configured edge on its attach monitor (legacy path).
            let cur_mon = (m_left, m_top, m_right, m_bottom);
            let pressing = |e: Edge| match e {
                Edge::Right => cur.x >= m_right - 1 - a.edge_margin,
                Edge::Left => cur.x <= m_left + a.edge_margin,
                Edge::Top => cur.y <= m_top + a.edge_margin,
                Edge::Bottom => cur.y >= m_bottom - 1 - a.edge_margin,
            };
            // Dead corner: suppress switching near the perpendicular extremes so
            // a diagonal flick to a corner does not switch.
            let near_corner_for = |e: Edge| {
                a.dead_corner_px > 0
                    && match e {
                        Edge::Right | Edge::Left => {
                            cur.y <= m_top + a.dead_corner_px
                                || cur.y >= m_bottom - 1 - a.dead_corner_px
                        }
                        Edge::Top | Edge::Bottom => {
                            cur.x <= m_left + a.dead_corner_px
                                || cur.x >= m_right - 1 - a.dead_corner_px
                        }
                    }
            };
            let edge_allowed = |e: Edge| match a.peer_rect {
                Some(pr) => peer_adjacent(cur_mon, e, pr),
                None => {
                    let on_attach = match a.attach_monitor {
                        Some(m) => cur_mon == m,
                        None => true,
                    };
                    e == edge && on_attach && is_outer_edge(cur_mon, e, &a.local_monitors)
                }
            };
            let cross_edge = [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom]
                .into_iter()
                .find(|&e| pressing(e) && edge_allowed(e) && !near_corner_for(e));

            if switch_guard.update(cross_edge.is_some(), false) {
                let cross = cross_edge.unwrap_or(edge);
                // Rebuild the combined space with the current monitor as the
                // local rect and the peer's latest real screen size, so the entry
                // mapping (and later the return placement) are correct.
                let (rw, rh) = {
                    let peer = a.tcp_state.lock().ok().and_then(|s| s.peer_name.clone());
                    peer.as_deref()
                        .and_then(|name| {
                            a.screen_sizes
                                .lock()
                                .ok()
                                .and_then(|m| m.get(name).copied())
                        })
                        .filter(|&(w, h)| w > 320 && h > 240)
                        .unwrap_or((a.remote_width, a.remote_height))
                };
                combined = CombinedSpace::new(
                    SsRect::new(m_left, m_top, m_right, m_bottom),
                    SsRect::new(0, 0, rw, rh),
                    cross,
                );
                state = combined.enter_remote_at(cur.x, cur.y);
                remote_active = true;
                if let Ok(mut remote_state) = a.remote_control.lock() {
                    remote_state.active = true;
                }

                let _ = start_mouse_hook_forwarding(
                    SenderTarget::Fixed(a.sender.clone()),
                    a.tcp_state.clone(),
                    a.mouse_hook_running.clone(),
                    a.mouse_hook.clone(),
                    "auto",
                );
                if let Err(err) = start_keyboard_hook_forwarding(
                    &keyboard_ctx,
                    SenderTarget::Fixed(a.sender.clone()),
                    "auto",
                ) {
                    update_tcp_state(&a.tcp_state, |snapshot| {
                        snapshot.last_event = format!("Keyboard forwarding failed to start: {err}");
                    });
                }

                let _ = a.sender.send(WireMessage::MouseSetPosition {
                    x: state.x,
                    y: state.y,
                });
                // Park and confine the local cursor so it cannot touch local UI
                // while the remote is controlled (released on every stop path).
                let _ = tailkvm_win32::cursor::set_cursor_position(a.lock_x, a.lock_y);
                let _ = tailkvm_win32::cursor::confine_cursor(a.lock_x, a.lock_y);

                update_tcp_state(&a.tcp_state, |snapshot| {
                    snapshot.last_event =
                        format!("Seamless: entered remote at x={}, y={}.", state.x, state.y);
                });
            }

            time::sleep(Duration::from_millis(a.interval_ms)).await;
            continue;
        }

        // Remote region: integrate raw deltas into the combined space.
        let mut acc_x = 0i32;
        let mut acc_y = 0i32;
        while let Ok((dx, dy)) = raw_rx.try_recv() {
            acc_x = acc_x.saturating_add(dx);
            acc_y = acc_y.saturating_add(dy);
        }

        // Scale raw HID deltas by the pointer-speed gain (with sub-pixel carry)
        // so controlling the remote feels as fast as the local cursor instead of
        // the raw 1:1 mapping, which is noticeably slow on a high-res local.
        let scaled_x = acc_x as f64 * gain + frac_x;
        let scaled_y = acc_y as f64 * gain + frac_y;
        let gain_x = scaled_x.trunc() as i32;
        let gain_y = scaled_y.trunc() as i32;
        frac_x = scaled_x - gain_x as f64;
        frac_y = scaled_y - gain_y as f64;

        if gain_x != 0 || gain_y != 0 {
            let (next, switched) = combined.apply_delta(state, gain_x, gain_y);
            state = next;

            if switched {
                // Returned to local: stop forwarding and place the real cursor.
                remote_active = false;
                if let Ok(mut remote_state) = a.remote_control.lock() {
                    remote_state.active = false;
                }
                let _ = stop_mouse_hook_forwarding(
                    a.mouse_hook_running.clone(),
                    a.mouse_hook.clone(),
                    a.tcp_state.clone(),
                    "auto",
                );
                let _ = stop_keyboard_hook_forwarding(
                    a.keyboard_hook_running.clone(),
                    a.keyboard_hook.clone(),
                    a.tcp_state.clone(),
                    "auto",
                );
                tailkvm_win32::cursor::release_cursor_confine();
                let _ = tailkvm_win32::cursor::set_cursor_position(state.x, state.y);

                update_tcp_state(&a.tcp_state, |snapshot| {
                    snapshot.last_event = format!(
                        "Seamless: returned to local at x={}, y={}.",
                        state.x, state.y
                    );
                });
            } else {
                let _ = a.sender.send(WireMessage::MouseSetPosition {
                    x: state.x,
                    y: state.y,
                });
                let _ = tailkvm_win32::cursor::set_cursor_position(a.lock_x, a.lock_y);
                sent_count += 1;
                if sent_count.is_multiple_of(30) {
                    update_tcp_state(&a.tcp_state, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Seamless remote active. sent={sent_count}, pos=({}, {}).",
                            state.x, state.y
                        );
                    });
                }
            }
        }

        time::sleep(Duration::from_millis(a.interval_ms)).await;
    }

    a.capture_running.store(false, Ordering::SeqCst);
    // Always release the cursor clip so the local cursor is never stranded,
    // regardless of why the loop ended (failsafe, return, stop).
    tailkvm_win32::cursor::release_cursor_confine();
    let _ = stop_mouse_hook_forwarding(
        a.mouse_hook_running.clone(),
        a.mouse_hook.clone(),
        a.tcp_state.clone(),
        "auto",
    );
    let _ = stop_keyboard_hook_forwarding(
        a.keyboard_hook_running.clone(),
        a.keyboard_hook.clone(),
        a.tcp_state.clone(),
        "auto",
    );
    if let Ok(mut remote_state) = a.remote_control.lock() {
        remote_state.active = false;
    }

    update_tcp_state(&a.tcp_state, |snapshot| {
        snapshot.last_event = "Seamless capture stopped.".to_string();
    });
}

// Parameters here are the Tauri IPC contract (the frontend invokes with these
// named args), so they cannot be bundled into a struct without breaking the
// command signature. The argument count is intentional at this boundary.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn start_mouse_capture(
    gain: Option<f64>,
    interval_ms: Option<u64>,
    max_delta: Option<i32>,
    remote_mode: Option<bool>,
    switch_edge: Option<String>,
    edge_margin: Option<i32>,
    remote_width: Option<i32>,
    remote_height: Option<i32>,
    use_raw_input: Option<bool>,
    seamless: Option<bool>,
    edge_dwell_ms: Option<u64>,
    dead_corner_px: Option<i32>,
    // Physical-pixel rect of the local monitor the peer is pinned to (from the
    // position editor). All four present = pin crossing to that monitor's edge.
    attach_left: Option<i32>,
    attach_top: Option<i32>,
    attach_right: Option<i32>,
    attach_bottom: Option<i32>,
    // Physical-pixel rect of the peer's screen as placed in the virtual layout
    // (position + real resolution). All four present = cross on every local
    // monitor edge this rect is flush against (multi-edge).
    peer_left: Option<i32>,
    peer_top: Option<i32>,
    peer_right: Option<i32>,
    peer_bottom: Option<i32>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let snapshot = tcp_snapshot(&state.tcp);

    if !snapshot.connected {
        return Err("No active TCP connection. Connect to a peer first.".to_string());
    }

    if let Some(peer_addr) = snapshot.peer_addr.as_deref() {
        if peer_addr.starts_with("127.") || peer_addr.starts_with("localhost") {
            return Err(
                "Refusing to start capture against localhost to prevent mouse feedback loop."
                    .to_string(),
            );
        }
    }

    let gain = gain.unwrap_or(1.0).clamp(0.10, 4.00);
    let interval_ms = interval_ms.unwrap_or(33).clamp(8, 100);
    let max_delta = max_delta.unwrap_or(80).clamp(10, 500);
    let remote_mode = remote_mode.unwrap_or(true);
    let switch_edge = normalize_edge(switch_edge.unwrap_or_else(|| "right".to_string()));
    let edge_margin = edge_margin.unwrap_or(3).clamp(1, 64);
    let remote_width = remote_width.unwrap_or(1920).clamp(320, 20000);
    let remote_height = remote_height.unwrap_or(1080).clamp(240, 20000);
    let use_raw_input = use_raw_input.unwrap_or(false);
    let seamless = seamless.unwrap_or(false);
    let edge_dwell_ms = edge_dwell_ms.unwrap_or(0).min(2000);
    let dead_corner_px = dead_corner_px.unwrap_or(0).clamp(0, 1000);

    if let Ok(mut remote_control) = state.remote_control.lock() {
        remote_control.active = false;
        remote_control.switch_edge = switch_edge.clone();
        remote_control.remote_width = remote_width;
        remote_control.remote_height = remote_height;
        remote_control.edge_margin = edge_margin;
        remote_control.seamless = seamless;
    }

    if state.capture_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Mouse capture is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        state.capture_running.store(false, Ordering::SeqCst);
        return Err("No active controller channel. Connect to a peer first.".to_string());
    };

    let topology = match tailkvm_win32::monitor::get_monitor_topology() {
        Ok(topology) => topology,
        Err(err) => {
            state.capture_running.store(false, Ordering::SeqCst);
            return Err(format!("Failed to get monitor topology: {err}"));
        }
    };

    let virtual_screen = topology.virtual_screen.clone();
    // All local monitor rects, for the seamless engine's outer-edge check so it
    // never crosses at an interior boundary (where a neighbouring local monitor
    // is — the cursor should flow there, not to the remote).
    let local_monitors: Vec<(i32, i32, i32, i32)> = topology
        .monitors
        .iter()
        .map(|m| {
            (
                m.rect_physical_px.left,
                m.rect_physical_px.top,
                m.rect_physical_px.right,
                m.rect_physical_px.bottom,
            )
        })
        .collect();

    let lock_x = virtual_screen.left + (virtual_screen.width / 2);
    let lock_y = virtual_screen.top + (virtual_screen.height / 2);

    let tcp_state = state.tcp.clone();
    let capture_running = state.capture_running.clone();
    let remote_control = state.remote_control.clone();
    let mouse_hook_running = state.mouse_hook_running.clone();
    let mouse_hook = state.mouse_hook.clone();
    let keyboard_hook_running = state.keyboard_hook_running.clone();
    let keyboard_hook = state.keyboard_hook.clone();
    let resolve_characters = state.resolve_characters.clone();

    if seamless {
        // Prefer the peer's real virtual-screen size (reported via ScreenInfo,
        // stored in screen_sizes under the peer machine name) over the
        // frontend's guess, so the cursor maps onto the peer's whole screen.
        let (remote_width, remote_height) = tcp_snapshot(&state.tcp)
            .peer_name
            .as_deref()
            .and_then(|name| {
                state
                    .screen_sizes
                    .lock()
                    .ok()
                    .and_then(|sizes| sizes.get(name).copied())
            })
            .filter(|&(w, h)| w > 320 && h > 240)
            .unwrap_or((remote_width, remote_height));

        // The peer is pinned to a specific local monitor only when all four
        // edges of its rect are provided by the position editor.
        let attach_monitor = match (attach_left, attach_top, attach_right, attach_bottom) {
            (Some(l), Some(t), Some(r), Some(b)) if r > l && b > t => Some((l, t, r, b)),
            _ => None,
        };

        // The peer's virtual rectangle (position + real resolution) from the
        // editor. Enables crossing on every monitor edge it is flush against.
        let peer_rect = match (peer_left, peer_top, peer_right, peer_bottom) {
            (Some(l), Some(t), Some(r), Some(b)) if r > l && b > t => Some((l, t, r, b)),
            _ => None,
        };

        let args = SeamlessArgs {
            sender,
            tcp_state,
            capture_running,
            remote_control,
            mouse_hook_running,
            mouse_hook,
            keyboard_hook_running,
            keyboard_hook,
            resolve_characters,
            screen_sizes: state.screen_sizes.clone(),
            gain,
            attach_monitor,
            peer_rect,
            local_monitors,
            local_rect: tailkvm_win32::screen_space::Rect::new(
                virtual_screen.left,
                virtual_screen.top,
                virtual_screen.right,
                virtual_screen.bottom,
            ),
            lock_x,
            lock_y,
            switch_edge,
            edge_margin,
            remote_width,
            remote_height,
            interval_ms,
            edge_dwell_ms,
            dead_corner_px,
        };
        tauri::async_runtime::spawn(run_seamless_capture(args));
        return Ok(tcp_snapshot(&state.tcp));
    }

    tauri::async_runtime::spawn(async move {
        let mut remote_active = !remote_mode;
        let mut sent_count: u64 = 0;
        let mut skipped_count: u64 = 0;
        let mut ignored_warp_frames: u8 = 0;
        let mut last_mirror_pos: Option<tailkvm_win32::cursor::CursorPosition> = None;

        // Optional Raw Input source: HID relative deltas (no pointer accel /
        // warp feedback). Falls back to the cursor-warp method on failure. The
        // handle is kept alive for the task and stops raw capture on drop.
        let (raw_delta_rx, _raw_handle) = if use_raw_input {
            let (tx, rx) = std::sync::mpsc::channel::<(i32, i32)>();
            match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(tx) {
                Ok(handle) => (Some(rx), Some(handle)),
                Err(err) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            format!("Raw input unavailable, using cursor warp instead: {err}");
                    });
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let mut return_x = lock_x;
        let mut return_y = lock_y;

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.role = "controller".to_string();
            snapshot.connected = true;
            snapshot.last_event = if remote_mode {
                format!(
                    "Remote mode armed. Move cursor to {} edge. remote={}x{}, gain={gain:.2}, interval={}ms, max_delta={}, margin={}px.",
                    switch_edge, remote_width, remote_height, interval_ms, max_delta, edge_margin
                )
            } else {
                format!(
                    "Mirror capture started. gain={gain:.2}, interval={}ms, max_delta={}",
                    interval_ms, max_delta
                )
            };
        });

        while capture_running.load(Ordering::SeqCst) {
            if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event =
                        "Mouse capture stopped by Ctrl+Alt+Pause failsafe.".to_string();
                });
                break;
            }

            let current = match tailkvm_win32::cursor::get_cursor_position() {
                Ok(position) => position,
                Err(err) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Mouse capture read error: {err}");
                    });
                    time::sleep(Duration::from_millis(interval_ms)).await;
                    continue;
                }
            };

            // While not yet controlling the remote, discard buffered raw deltas
            // so local movement toward the edge does not jump the remote cursor
            // on activation.
            if let Some(rx) = &raw_delta_rx {
                if !remote_active {
                    while rx.try_recv().is_ok() {}
                }
            }

            if remote_mode && !remote_active {
                if is_cursor_at_edge(&current, &virtual_screen, &switch_edge, edge_margin) {
                    let return_pos =
                        local_return_position(&current, &virtual_screen, &switch_edge, edge_margin);
                    return_x = return_pos.x;
                    return_y = return_pos.y;

                    let remote_entry = remote_entry_position(
                        &current,
                        &virtual_screen,
                        &switch_edge,
                        remote_width,
                        remote_height,
                    );

                    if sender
                        .send(WireMessage::MouseSetPosition {
                            x: remote_entry.x,
                            y: remote_entry.y,
                        })
                        .is_err()
                    {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.connected = false;
                            snapshot.last_event =
                                "Remote mode failed: controller channel closed.".to_string();
                        });
                        break;
                    }

                    remote_active = true;

                    if let Ok(mut remote_state) = remote_control.lock() {
                        remote_state.active = true;
                    }

                    if let Err(err) = start_mouse_hook_forwarding(
                        SenderTarget::Fixed(sender.clone()),
                        tcp_state.clone(),
                        mouse_hook_running.clone(),
                        mouse_hook.clone(),
                        "auto",
                    ) {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Auto click/wheel capture failed: {err}");
                        });
                    }

                    let keyboard_ctx = KeyboardForwardingContext {
                        tcp_state: tcp_state.clone(),
                        keyboard_hook_running: keyboard_hook_running.clone(),
                        keyboard_hook: keyboard_hook.clone(),
                        capture_running: capture_running.clone(),
                        mouse_hook_running: mouse_hook_running.clone(),
                        mouse_hook: mouse_hook.clone(),
                        remote_control: remote_control.clone(),
                        resolve_characters: resolve_characters.clone(),
                    };
                    if let Err(err) = start_keyboard_hook_forwarding(
                        &keyboard_ctx,
                        SenderTarget::Fixed(sender.clone()),
                        "auto",
                    ) {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Auto keyboard capture failed: {err}");
                        });
                    }

                    ignored_warp_frames = 3;

                    let _ = tailkvm_win32::cursor::set_cursor_position(lock_x, lock_y);

                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Remote mode active via {} edge. Remote entry x={}, y={} for remote {}x{}. Local lock x={}, y={}. Return target x={}, y={}.",
                            switch_edge,
                            remote_entry.x,
                            remote_entry.y,
                            remote_width,
                            remote_height,
                            lock_x,
                            lock_y,
                            return_x,
                            return_y
                        );
                    });

                    time::sleep(Duration::from_millis(interval_ms)).await;
                    continue;
                }

                skipped_count += 1;

                if skipped_count.is_multiple_of(60) {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Remote mode waiting for {} edge. current x={}, y={}",
                            switch_edge, current.x, current.y
                        );
                    });
                }

                time::sleep(Duration::from_millis(interval_ms)).await;
                continue;
            }

            let raw_dx;
            let raw_dy;

            if remote_mode {
                if let Some(rx) = &raw_delta_rx {
                    // Raw Input mode: sum HID relative deltas since the last tick.
                    // The local cursor is pinned (deltas come from the device, not
                    // the cursor position) so the warp-feedback heuristics below
                    // are unnecessary.
                    let mut acc_x = 0i32;
                    let mut acc_y = 0i32;
                    while let Ok((dx, dy)) = rx.try_recv() {
                        acc_x = acc_x.saturating_add(dx);
                        acc_y = acc_y.saturating_add(dy);
                    }
                    raw_dx = acc_x;
                    raw_dy = acc_y;

                    let _ = tailkvm_win32::cursor::set_cursor_position(lock_x, lock_y);
                } else {
                    raw_dx = current.x - lock_x;
                    raw_dy = current.y - lock_y;

                    if ignored_warp_frames > 0 {
                        ignored_warp_frames -= 1;
                        let _ = tailkvm_win32::cursor::set_cursor_position(lock_x, lock_y);
                        skipped_count += 1;
                        time::sleep(Duration::from_millis(interval_ms)).await;
                        continue;
                    }

                    let _ = tailkvm_win32::cursor::set_cursor_position(lock_x, lock_y);

                    let warp_threshold = (max_delta * 8).max(800);
                    if raw_dx.abs() > warp_threshold || raw_dy.abs() > warp_threshold {
                        skipped_count += 1;

                        if skipped_count.is_multiple_of(20) {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!(
                                    "Ignored possible local cursor warp. raw=({}, {}), threshold={}",
                                    raw_dx, raw_dy, warp_threshold
                                );
                            });
                        }

                        time::sleep(Duration::from_millis(interval_ms)).await;
                        continue;
                    }
                }
            } else {
                let last = last_mirror_pos.unwrap_or(current);
                last_mirror_pos = Some(current);

                raw_dx = current.x - last.x;
                raw_dy = current.y - last.y;
            }

            let dx = ((raw_dx as f64) * gain).round() as i32;
            let dy = ((raw_dy as f64) * gain).round() as i32;

            let dx = dx.clamp(-max_delta, max_delta);
            let dy = dy.clamp(-max_delta, max_delta);

            if dx != 0 || dy != 0 {
                if sender.send(WireMessage::MouseMove { dx, dy }).is_err() {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.connected = false;
                        snapshot.last_event =
                            "Mouse capture stopped: controller channel closed.".to_string();
                    });
                    break;
                }

                sent_count += 1;

                if sent_count.is_multiple_of(15) {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Remote capture active. sent={}, skipped={}, raw=({}, {}), sent=({}, {}), gain={gain:.2}, interval={}ms",
                            sent_count, skipped_count, raw_dx, raw_dy, dx, dy, interval_ms
                        );
                    });
                }
            } else {
                skipped_count += 1;
            }

            time::sleep(Duration::from_millis(interval_ms)).await;
        }

        capture_running.store(false, Ordering::SeqCst);

        if remote_mode && remote_active {
            let _ = tailkvm_win32::cursor::set_cursor_position(return_x, return_y);
        }

        let _ = stop_mouse_hook_forwarding(
            mouse_hook_running.clone(),
            mouse_hook.clone(),
            tcp_state.clone(),
            "auto",
        );

        let _ = stop_keyboard_hook_forwarding(
            keyboard_hook_running.clone(),
            keyboard_hook.clone(),
            tcp_state.clone(),
            "auto",
        );

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!(
                "Mouse capture stopped. sent={}, skipped={}, remote_mode={}, edge={}, local cursor returned to x={}, y={}",
                sent_count, skipped_count, remote_mode, switch_edge, return_x, return_y
            );
        });
    });

    Ok(tcp_snapshot(&state.tcp))
}

fn is_remote_return_edge(x: i32, y: i32, remote: &RemoteControlState) -> bool {
    let margin = remote.edge_margin.max(8);
    let width = remote.remote_width.max(1);
    let height = remote.remote_height.max(1);

    match remote.switch_edge.as_str() {
        // Local right -> remote enters from left, so remote left edge returns local.
        "right" => x <= margin,
        // Local left -> remote enters from right, so remote right edge returns local.
        "left" => x >= width - 1 - margin,
        // Local top -> remote enters from bottom, so remote bottom edge returns local.
        "top" => y >= height - 1 - margin,
        // Local bottom -> remote enters from top, so remote top edge returns local.
        "bottom" => y <= margin,
        _ => x <= margin,
    }
}

fn normalize_edge(edge: String) -> String {
    match edge.trim().to_lowercase().as_str() {
        "left" => "left".to_string(),
        "right" => "right".to_string(),
        "top" => "top".to_string(),
        "bottom" => "bottom".to_string(),
        _ => "right".to_string(),
    }
}

fn is_cursor_at_edge(
    position: &tailkvm_win32::cursor::CursorPosition,
    rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    margin: i32,
) -> bool {
    match edge {
        "left" => position.x <= rect.left + margin,
        "right" => position.x >= rect.right - 1 - margin,
        "top" => position.y <= rect.top + margin,
        "bottom" => position.y >= rect.bottom - 1 - margin,
        _ => position.x >= rect.right - 1 - margin,
    }
}

fn remote_entry_position(
    position: &tailkvm_win32::cursor::CursorPosition,
    local_rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    remote_width: i32,
    remote_height: i32,
) -> tailkvm_win32::cursor::CursorPosition {
    let inset = 4;

    match edge {
        "left" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: remote_width - 1 - inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "right" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "top" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: remote_height - 1 - inset,
            }
        }
        "bottom" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: inset,
            }
        }
        _ => tailkvm_win32::cursor::CursorPosition {
            x: inset,
            y: remote_height / 2,
        },
    }
}

fn local_return_position(
    position: &tailkvm_win32::cursor::CursorPosition,
    rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    margin: i32,
) -> tailkvm_win32::cursor::CursorPosition {
    let safe_margin = margin.max(8);

    match edge {
        "left" => tailkvm_win32::cursor::CursorPosition {
            x: rect.left + safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "right" => tailkvm_win32::cursor::CursorPosition {
            x: rect.right - 1 - safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "top" => tailkvm_win32::cursor::CursorPosition {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.top + safe_margin,
        },
        "bottom" => tailkvm_win32::cursor::CursorPosition {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.bottom - 1 - safe_margin,
        },
        _ => tailkvm_win32::cursor::CursorPosition {
            x: rect.right - 1 - safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
    }
}

/// Stop all input forwarding: mouse-move capture, mouse hook, and keyboard
/// hook, and clear remote-control state. The forwarding loops release any stuck
/// keys/buttons as they wind down. Shared by the Stop button and the tray
/// "Pause" item; complements (does not replace) the Ctrl+Alt+Pause failsafe.
fn pause_all_capture(state: &AppState) {
    state.capture_running.store(false, Ordering::SeqCst);

    if let Ok(mut remote_state) = state.remote_control.lock() {
        remote_state.active = false;
    }

    let _ = stop_mouse_hook_forwarding(
        state.mouse_hook_running.clone(),
        state.mouse_hook.clone(),
        state.tcp.clone(),
        "auto",
    );

    let _ = stop_keyboard_hook_forwarding(
        state.keyboard_hook_running.clone(),
        state.keyboard_hook.clone(),
        state.tcp.clone(),
        "auto",
    );
}

#[tauri::command]
async fn stop_mouse_capture(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    pause_all_capture(&state);

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Mouse capture stop requested.".to_string();
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn start_tcp_receiver(
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);

    if state.receiver_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Receiver is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let tcp_state = state.tcp.clone();
    let receiver_running = state.receiver_running.clone();
    let receiver_tx = state.receiver_tx.clone();
    let clipboard_guard = state.clipboard_guard.clone();
    let accept_incoming = state.accept_incoming.clone();

    tauri::async_runtime::spawn(async move {
        let listen_addr = format!("0.0.0.0:{port}");

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.role = "receiver".to_string();
            snapshot.listening = false;
            snapshot.listen_addr = Some(listen_addr.clone());
            snapshot.connected = false;
            snapshot.last_event = format!("Starting receiver on {listen_addr}...");
        });

        match TcpListener::bind(&listen_addr).await {
            Ok(listener) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.listening = true;
                    snapshot.last_event = format!("Receiver listening on {listen_addr}.");
                });

                // Single active session, newest wins: when a new controller
                // connects, signal the previous handler to stop so a crashed /
                // zombie connection self-heals on reconnect. The displaced
                // handler still runs its stuck-input release on the way out.
                let mut active_cancel: Option<tokio::sync::oneshot::Sender<()>> = None;

                loop {
                    match listener.accept().await {
                        Ok((stream, peer_addr)) => {
                            // Disable Nagle so each injected input event is sent
                            // immediately instead of being coalesced (KVM latency).
                            let _ = stream.set_nodelay(true);
                            let peer_addr_text = peer_addr.to_string();
                            let tcp_state_for_client = tcp_state.clone();
                            let receiver_tx_for_client = receiver_tx.clone();
                            let clipboard_guard_for_client = clipboard_guard.clone();
                            let accept_incoming_for_client = accept_incoming.clone();

                            // Displace any existing session.
                            if let Some(old_cancel) = active_cancel.take() {
                                let _ = old_cancel.send(());
                            }
                            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                            active_cancel = Some(cancel_tx);

                            tauri::async_runtime::spawn(async move {
                                handle_receiver_stream(
                                    stream,
                                    peer_addr_text,
                                    tcp_state_for_client,
                                    cancel_rx,
                                    receiver_tx_for_client,
                                    clipboard_guard_for_client,
                                    accept_incoming_for_client,
                                )
                                .await;
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Receiver accept failed: {err}");
                            });
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.role = "receiver".to_string();
                    snapshot.listening = false;
                    snapshot.connected = false;
                    snapshot.last_event =
                        format!("Failed to bind receiver on {listen_addr}: {err}");
                });
            }
        }

        receiver_running.store(false, Ordering::SeqCst);

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.listening = false;
        });
    });

    time::sleep(Duration::from_millis(150)).await;
    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
async fn connect_tcp_peer(
    host: String,
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let host = host.trim().to_string();

    if host.is_empty() {
        return Err("host is empty. Enter a Tailscale IP such as 100.x.y.z.".to_string());
    }

    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    let addr = format!("{host}:{port}");
    let tcp_state = state.tcp.clone();

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = false;
        snapshot.peer_addr = Some(addr.clone());
        snapshot.peer_name = None;
        snapshot.last_event = format!("Connecting to {addr}...");
    });

    // Supersede any existing 1:1 controller supervisor before starting a new
    // one. Bumping the generation makes the old supervisor exit; clearing the
    // command channel ends its in-flight session immediately. Without this, a
    // second connect (e.g. a double-click) leaves two supervisors dialing the
    // same peer and churning the receiver's single session slot.
    let my_gen = state.controller_generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut tx_guard) = state.controller_tx.lock() {
        *tx_guard = None;
    }

    let should_run = state.controller_should_run.clone();
    should_run.store(true, Ordering::SeqCst);

    spawn_controller_supervisor(
        addr,
        state.tcp.clone(),
        state.capture_running.clone(),
        state.remote_control.clone(),
        state.clipboard_guard.clone(),
        state.screen_sizes.clone(),
        state.sessions.clone(),
        state.controller_tx.clone(),
        should_run,
        "controller".to_string(),
        Some((state.controller_generation.clone(), my_gen)),
    );

    time::sleep(Duration::from_millis(200)).await;
    Ok(tcp_snapshot(&state.tcp))
}

/// Run a (re)connecting controller session in the background until `should_run`
/// is cleared. Each attempt rebuilds the command channel and stores its sender
/// into `tx_slot`. Shared by the single 1:1 controller and named multi-screen
/// sessions (roadmap B1.2 / F2).
#[allow(clippy::too_many_arguments)]
fn spawn_controller_supervisor(
    addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    screen_sizes: Arc<Mutex<HashMap<String, (i32, i32)>>>,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    tx_slot: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    should_run: Arc<AtomicBool>,
    screen_label: String,
    // For the 1:1 controller: (shared counter, this supervisor's generation).
    // The loop exits if the shared counter moves past our generation, so a newer
    // connect supersedes us. None for named sessions (they dedupe via their own
    // per-session should_run flag).
    generation: Option<(Arc<AtomicU64>, u64)>,
) {
    let is_current = move || {
        generation
            .as_ref()
            .is_none_or(|(counter, my_gen)| counter.load(Ordering::SeqCst) == *my_gen)
    };
    tauri::async_runtime::spawn(async move {
        let mut backoff_secs: u64 = 1;
        while should_run.load(Ordering::SeqCst) && is_current() {
            let (command_tx, command_rx) = mpsc::unbounded_channel::<WireMessage>();
            if let Ok(mut tx_guard) = tx_slot.lock() {
                *tx_guard = Some(command_tx);
            }

            let session_start = Instant::now();
            run_controller_session(
                addr.clone(),
                tcp_state.clone(),
                command_rx,
                capture_running.clone(),
                remote_control.clone(),
                clipboard_guard.clone(),
                screen_sizes.clone(),
                sessions.clone(),
                screen_label.clone(),
            )
            .await;
            let session_secs = session_start.elapsed().as_secs();

            if let Ok(mut tx_guard) = tx_slot.lock() {
                *tx_guard = None;
            }

            if !should_run.load(Ordering::SeqCst) || !is_current() {
                break;
            }

            // A session that stayed up for a while was healthy — reset the
            // backoff so a one-off drop reconnects fast (instead of inheriting a
            // 10s wait from earlier failures).
            if session_secs >= 15 {
                backoff_secs = 1;
            }

            // Preserve WHY the session ended (run_controller_session left the
            // reason in last_event) instead of clobbering it with a generic
            // "reconnecting" note — otherwise the actual cause is invisible.
            let reason = tcp_state
                .lock()
                .map(|s| s.last_event.clone())
                .unwrap_or_default();
            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.connected = false;
                snapshot.last_event = format!(
                    "[{screen_label}] dropped after {session_secs}s ({reason}). Reconnecting in {backoff_secs}s..."
                );
            });

            let mut waited = 0;
            while waited < backoff_secs && should_run.load(Ordering::SeqCst) && is_current() {
                time::sleep(Duration::from_secs(1)).await;
                waited += 1;
            }
            backoff_secs = (backoff_secs * 2).min(10);
        }

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.connected = false;
            snapshot.last_event = format!("[{screen_label}] session ended.");
        });
    });
}

/// Explicitly disconnect the controller session and stop auto-reconnect.
#[tauri::command]
async fn disconnect_tcp_peer(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    state.controller_should_run.store(false, Ordering::SeqCst);
    // Dropping the command sender ends the current session's select loop.
    if let Ok(mut tx_guard) = state.controller_tx.lock() {
        *tx_guard = None;
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.connected = false;
        snapshot.last_event = "Disconnect requested; auto-reconnect stopped.".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Toggle whether the receiver accepts incoming controller connections (G1).
#[tauri::command]
async fn set_accept_incoming(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.accept_incoming.store(enabled, Ordering::SeqCst);
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = if enabled {
            "Accepting incoming controller connections.".to_string()
        } else {
            "Rejecting incoming controller connections.".to_string()
        };
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Connect (or reconnect) a named screen for multi-machine control (B1.2).
/// Re-connecting an existing name replaces the previous session.
#[tauri::command]
async fn connect_screen(
    name: String,
    host: String,
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let name = name.trim().to_string();
    let host = host.trim().to_string();
    if name.is_empty() || host.is_empty() {
        return Err("screen name and host are required.".to_string());
    }
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    let addr = format!("{host}:{port}");

    start_named_session(&state, &name, &addr)?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = format!("Connecting screen '{name}' to {addr}...");
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Start (or replace) a named reconnecting session to `addr`. Sync, so it can
/// be called from a command or from app startup (B1.2 / B1.6 auto-connect).
fn start_named_session(state: &AppState, name: &str, addr: &str) -> Result<(), String> {
    let mut map = state
        .sessions
        .lock()
        .map_err(|_| "sessions mutex poisoned".to_string())?;

    if let Some(old) = map.remove(name) {
        old.should_run.store(false, Ordering::SeqCst);
        if let Ok(mut tx) = old.tx.lock() {
            *tx = None;
        }
    }

    let should_run = Arc::new(AtomicBool::new(true));
    let tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> = Arc::new(Mutex::new(None));
    map.insert(
        name.to_string(),
        ScreenSession {
            should_run: should_run.clone(),
            tx: tx.clone(),
        },
    );

    spawn_controller_supervisor(
        addr.to_string(),
        state.tcp.clone(),
        state.capture_running.clone(),
        state.remote_control.clone(),
        state.clipboard_guard.clone(),
        state.screen_sizes.clone(),
        state.sessions.clone(),
        tx,
        should_run,
        name.to_string(),
        None,
    );

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SavedScreen {
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
    #[serde(default)]
    is_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SavedLink {
    from: String,
    edge: String,
    to: String,
}

/// Persisted multi-screen layout (roadmap B1.6 / F3).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SavedLayout {
    #[serde(default)]
    screens: Vec<SavedScreen>,
    #[serde(default)]
    links: Vec<SavedLink>,
    /// Connect the configured screens automatically on app startup.
    #[serde(default)]
    auto_connect: bool,
}

fn layout_file_path() -> Result<std::path::PathBuf, String> {
    let base = std::env::var("APPDATA").map_err(|_| "APPDATA env not set".to_string())?;
    let dir = std::path::Path::new(&base).join("TailKVM");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    Ok(dir.join("layout.json"))
}

fn read_saved_layout() -> SavedLayout {
    layout_file_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist the multi-screen layout to `%APPDATA%\TailKVM\layout.json` (B1.6).
#[tauri::command]
async fn save_layout(
    layout: SavedLayout,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let path = layout_file_path()?;
    let json =
        serde_json::to_string_pretty(&layout).map_err(|e| format!("serialize layout: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("write layout: {e}"))?;
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = format!("Layout saved to {}.", path.display());
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Load the persisted multi-screen layout (B1.6).
#[tauri::command]
async fn load_layout() -> Result<SavedLayout, String> {
    Ok(read_saved_layout())
}

/// Disconnect a named screen and stop its auto-reconnect (B1.2).
#[tauri::command]
async fn disconnect_screen(
    name: String,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let name = name.trim().to_string();
    if let Ok(mut map) = state.sessions.lock() {
        if let Some(session) = map.remove(&name) {
            session.should_run.store(false, Ordering::SeqCst);
            if let Ok(mut tx) = session.tx.lock() {
                *tx = None;
            }
        }
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = format!("Disconnected screen '{name}'.");
    });
    Ok(tcp_snapshot(&state.tcp))
}

#[derive(Debug, Serialize)]
struct ScreenStatus {
    name: String,
    connected: bool,
    /// Coarse connection state for the UI (issue 3): "active" (live channel) or
    /// "reconnecting" (session up, channel rebuilding / peer unreachable).
    state: String,
}

/// List named multi-screen sessions with their connection state (B1.2 / issue 3).
#[tauri::command]
async fn list_screens(state: State<'_, AppState>) -> Result<Vec<ScreenStatus>, String> {
    let map = state
        .sessions
        .lock()
        .map_err(|_| "sessions mutex poisoned".to_string())?;
    let mut screens: Vec<ScreenStatus> = map
        .iter()
        .map(|(name, session)| {
            let connected = session.tx.lock().map(|g| g.is_some()).unwrap_or(false);
            ScreenStatus {
                name: name.clone(),
                connected,
                state: if connected { "active" } else { "reconnecting" }.to_string(),
            }
        })
        .collect();
    screens.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(screens)
}

#[derive(Debug, Serialize)]
struct LockState {
    locked: bool,
}

/// Report whether this machine is locked / on a secure desktop (issue 3), so
/// the UI can show that input sharing is currently suspended here.
#[tauri::command]
fn get_lock_state() -> LockState {
    LockState {
        locked: tailkvm_win32::desktop::is_workstation_locked(),
    }
}

#[derive(Debug, Deserialize)]
struct RouterScreen {
    name: String,
    width: i32,
    height: i32,
    is_local: bool,
}

#[derive(Debug, Deserialize)]
struct RouterLink {
    from: String,
    edge: String,
    to: String,
}

#[derive(Debug, Deserialize)]
struct RouterConfig {
    screens: Vec<RouterScreen>,
    links: Vec<RouterLink>,
}

/// Owned inputs for the multi-screen router (roadmap B1.4).
struct RouterArgs {
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    router_running: Arc<AtomicBool>,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    resolve_characters: Arc<AtomicBool>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    router_space: Arc<Mutex<Option<Arc<tailkvm_win32::layout_graph::MultiScreenSpace>>>>,
    local_name: String,
    lock_x: i32,
    lock_y: i32,
    interval_ms: u64,
    edge_margin: i32,
    edge_dwell_ms: u64,
    dead_corner_px: i32,
}

/// Resolve the current outbound sender for a named screen session.
fn screen_sender(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    name: &str,
) -> Option<mpsc::UnboundedSender<WireMessage>> {
    let map = sessions.lock().ok()?;
    let session = map.get(name)?;
    let tx = session.tx.lock().ok()?;
    tx.clone()
}

/// Multi-screen router (roadmap B1.4). Owns the logical cursor across N screens
/// via `MultiScreenSpace`; in the local screen it follows the real cursor and
/// watches edges, and on a remote screen it integrates Raw Input deltas and
/// sends absolute `MouseSetPosition` to that screen's session. Hooks
/// (click/wheel/key) are installed only while controlling a remote and target
/// the active session via `SenderTarget::Active`, so remote->remote switches do
/// not restart them. Opt-in; runtime-unvalidated PoC (needs 3 machines).
async fn run_router(args: RouterArgs) {
    use tailkvm_win32::layout_graph::ScreenCursor;
    use tailkvm_win32::screen_space::{Edge, SwitchGuard};

    let active_slot: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> =
        Arc::new(Mutex::new(None));
    let mut switch_guard = SwitchGuard::new(args.edge_dwell_ms, args.interval_ms);

    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(i32, i32)>();
    let _raw_handle = match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(raw_tx) {
        Ok(handle) => handle,
        Err(err) => {
            args.router_running.store(false, Ordering::SeqCst);
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event = format!("Router needs Raw Input, unavailable: {err}");
            });
            return;
        }
    };

    let keyboard_ctx = KeyboardForwardingContext {
        tcp_state: args.tcp_state.clone(),
        keyboard_hook_running: args.keyboard_hook_running.clone(),
        keyboard_hook: args.keyboard_hook.clone(),
        // The keyboard failsafe clears this; point it at the router so
        // Ctrl+Alt+Pause stops the router loop too.
        capture_running: args.router_running.clone(),
        mouse_hook_running: args.mouse_hook_running.clone(),
        mouse_hook: args.mouse_hook.clone(),
        remote_control: args.remote_control.clone(),
        resolve_characters: args.resolve_characters.clone(),
    };

    let mut active = args.local_name.clone();
    let mut cursor = ScreenCursor {
        screen: args.local_name.clone(),
        x: args.lock_x,
        y: args.lock_y,
    };

    let start_hooks = || {
        let _ = start_mouse_hook_forwarding(
            SenderTarget::Active(active_slot.clone()),
            args.tcp_state.clone(),
            args.mouse_hook_running.clone(),
            args.mouse_hook.clone(),
            "router",
        );
        let _ = start_keyboard_hook_forwarding(
            &keyboard_ctx,
            SenderTarget::Active(active_slot.clone()),
            "router",
        );
    };
    let stop_hooks = || {
        let _ = stop_mouse_hook_forwarding(
            args.mouse_hook_running.clone(),
            args.mouse_hook.clone(),
            args.tcp_state.clone(),
            "router",
        );
        let _ = stop_keyboard_hook_forwarding(
            args.keyboard_hook_running.clone(),
            args.keyboard_hook.clone(),
            args.tcp_state.clone(),
            "router",
        );
    };

    update_tcp_state(&args.tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Multi-screen router armed. Local screen '{}'.",
            args.local_name
        );
    });

    while args.router_running.load(Ordering::SeqCst) {
        if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event = "Router stopped by Ctrl+Alt+Pause failsafe.".to_string();
            });
            break;
        }

        // Snapshot the live screen space (issue 1: reconfigure swaps it).
        let space = match args.router_space.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        };
        let Some(space) = space else {
            // No space configured (stopped or cleared) — end the router.
            break;
        };

        // If a reconfigure removed the screen we were controlling, fall back to
        // local so we never read an inconsistent / missing screen.
        if active != args.local_name && space.rect(&active).is_none() {
            if let Ok(mut slot) = active_slot.lock() {
                *slot = None;
            }
            if let Ok(mut remote_state) = args.remote_control.lock() {
                remote_state.active = false;
            }
            stop_hooks();
            tailkvm_win32::cursor::release_cursor_confine();
            active = args.local_name.clone();
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Router: active screen removed by reconfigure; returned to local.".to_string();
            });
        }

        if active == args.local_name {
            let cur = match tailkvm_win32::cursor::get_cursor_position() {
                Ok(position) => position,
                Err(_) => {
                    time::sleep(Duration::from_millis(args.interval_ms)).await;
                    continue;
                }
            };
            while raw_rx.try_recv().is_ok() {}

            let Some(lr) = space.rect(&args.local_name).copied() else {
                break;
            };
            let m = args.edge_margin;
            let edge = [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom]
                .into_iter()
                .find(|&edge| {
                    let at = match edge {
                        Edge::Right => cur.x >= lr.right - 1 - m,
                        Edge::Left => cur.x <= lr.left + m,
                        Edge::Top => cur.y <= lr.top + m,
                        Edge::Bottom => cur.y >= lr.bottom - 1 - m,
                    };
                    at && space.neighbor(&args.local_name, edge).is_some()
                });

            // Debounce switching with dwell + dead corner (roadmap C1 applied
            // to the router).
            let near_corner = match edge {
                Some(Edge::Right) | Some(Edge::Left) => {
                    args.dead_corner_px > 0
                        && (cur.y <= lr.top + args.dead_corner_px
                            || cur.y >= lr.bottom - 1 - args.dead_corner_px)
                }
                Some(Edge::Top) | Some(Edge::Bottom) => {
                    args.dead_corner_px > 0
                        && (cur.x <= lr.left + args.dead_corner_px
                            || cur.x >= lr.right - 1 - args.dead_corner_px)
                }
                None => false,
            };
            let fire = switch_guard.update(edge.is_some(), near_corner);

            if let Some(edge) = edge.filter(|_| fire) {
                if let Some(entry) = space.enter_neighbor(&args.local_name, edge, cur.x, cur.y) {
                    active = entry.screen.clone();
                    cursor = entry;
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = screen_sender(&args.sessions, &active);
                    }
                    if let Ok(mut remote_state) = args.remote_control.lock() {
                        remote_state.active = true;
                    }
                    start_hooks();
                    let _ = tailkvm_win32::cursor::set_cursor_position(args.lock_x, args.lock_y);
                    let _ = tailkvm_win32::cursor::confine_cursor(args.lock_x, args.lock_y);
                    if let Some(sender) = screen_sender(&args.sessions, &active) {
                        let _ = sender.send(WireMessage::MouseSetPosition {
                            x: cursor.x,
                            y: cursor.y,
                        });
                    }
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Router: control moved to screen '{}' at x={}, y={}.",
                            active, cursor.x, cursor.y
                        );
                    });
                }
            }

            time::sleep(Duration::from_millis(args.interval_ms)).await;
            continue;
        }

        // Active is a remote screen: integrate raw deltas.
        let mut acc_x = 0i32;
        let mut acc_y = 0i32;
        while let Ok((dx, dy)) = raw_rx.try_recv() {
            acc_x = acc_x.saturating_add(dx);
            acc_y = acc_y.saturating_add(dy);
        }

        if acc_x != 0 || acc_y != 0 {
            let (next, switch) = space.apply_delta(cursor.clone(), acc_x, acc_y);
            cursor = next;

            if switch.is_some() {
                if cursor.screen == args.local_name {
                    active = args.local_name.clone();
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = None;
                    }
                    if let Ok(mut remote_state) = args.remote_control.lock() {
                        remote_state.active = false;
                    }
                    stop_hooks();
                    tailkvm_win32::cursor::release_cursor_confine();
                    let _ = tailkvm_win32::cursor::set_cursor_position(cursor.x, cursor.y);
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Router: returned to local at x={}, y={}.",
                            cursor.x, cursor.y
                        );
                    });
                } else {
                    // remote -> remote: swap the active target, keep hooks.
                    active = cursor.screen.clone();
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = screen_sender(&args.sessions, &active);
                    }
                    if let Some(sender) = screen_sender(&args.sessions, &active) {
                        let _ = sender.send(WireMessage::MouseSetPosition {
                            x: cursor.x,
                            y: cursor.y,
                        });
                    }
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event =
                            format!("Router: control moved to screen '{active}'.");
                    });
                }
            } else if let Some(sender) = screen_sender(&args.sessions, &active) {
                let _ = sender.send(WireMessage::MouseSetPosition {
                    x: cursor.x,
                    y: cursor.y,
                });
                let _ = tailkvm_win32::cursor::set_cursor_position(args.lock_x, args.lock_y);
            }
        }

        time::sleep(Duration::from_millis(args.interval_ms)).await;
    }

    args.router_running.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = args.router_space.lock() {
        *slot = None;
    }
    tailkvm_win32::cursor::release_cursor_confine();
    stop_hooks();
    if let Ok(mut remote_state) = args.remote_control.lock() {
        remote_state.active = false;
    }
    update_tcp_state(&args.tcp_state, |snapshot| {
        snapshot.last_event = "Multi-screen router stopped.".to_string();
    });
}

/// Start the multi-screen router from a layout config (B1.4). Screens named in
/// the config must already be connected via `connect_screen` (except the local
/// screen). Opt-in; legacy modes untouched.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn start_multi_screen_router(
    config: RouterConfig,
    interval_ms: Option<u64>,
    edge_margin: Option<i32>,
    edge_dwell_ms: Option<u64>,
    dead_corner_px: Option<i32>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let local_name = config
        .screens
        .iter()
        .find(|screen| screen.is_local)
        .map(|screen| screen.name.clone())
        .ok_or_else(|| "config must include exactly one local screen.".to_string())?;

    let (space, lock_x, lock_y) = build_multi_screen_space(&config, &state.screen_sizes)?;

    if state.router_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Multi-screen router is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    if let Ok(mut slot) = state.router_space.lock() {
        *slot = Some(Arc::new(space));
    }
    if let Ok(mut name) = state.router_local_name.lock() {
        *name = Some(local_name.clone());
    }

    let args = RouterArgs {
        tcp_state: state.tcp.clone(),
        router_running: state.router_running.clone(),
        sessions: state.sessions.clone(),
        remote_control: state.remote_control.clone(),
        resolve_characters: state.resolve_characters.clone(),
        mouse_hook_running: state.mouse_hook_running.clone(),
        mouse_hook: state.mouse_hook.clone(),
        keyboard_hook_running: state.keyboard_hook_running.clone(),
        keyboard_hook: state.keyboard_hook.clone(),
        router_space: state.router_space.clone(),
        local_name,
        lock_x,
        lock_y,
        interval_ms: interval_ms.unwrap_or(33).clamp(8, 100),
        edge_margin: edge_margin.unwrap_or(3).clamp(1, 64),
        edge_dwell_ms: edge_dwell_ms.unwrap_or(0).min(2000),
        dead_corner_px: dead_corner_px.unwrap_or(0).clamp(0, 1000),
    };

    tauri::async_runtime::spawn(run_router(args));

    Ok(tcp_snapshot(&state.tcp))
}

/// Build the `MultiScreenSpace` from a layout config, re-fetching the *live*
/// monitor topology (so a reconfigure picks up DPI / resolution / monitor
/// changes) and preferring peer-reported sizes (B1.7). Returns the space and
/// the local lock point. Pure of side effects on AppState.
fn build_multi_screen_space(
    config: &RouterConfig,
    screen_sizes: &Arc<Mutex<HashMap<String, (i32, i32)>>>,
) -> Result<(tailkvm_win32::layout_graph::MultiScreenSpace, i32, i32), String> {
    use tailkvm_win32::layout_graph::{LayoutGraph, MultiScreenSpace};
    use tailkvm_win32::screen_space::{Edge, Rect as SsRect};

    if !config.screens.iter().any(|screen| screen.is_local) {
        return Err("config must include exactly one local screen.".to_string());
    }

    let topology = tailkvm_win32::monitor::get_monitor_topology()
        .map_err(|err| format!("failed to get monitor topology: {err}"))?;
    let vs = &topology.virtual_screen;
    let lock_x = vs.left + (vs.width / 2);
    let lock_y = vs.top + (vs.height / 2);

    let reported = screen_sizes
        .lock()
        .map(|sizes| sizes.clone())
        .unwrap_or_default();

    let mut screens = HashMap::new();
    for screen in &config.screens {
        let rect = if screen.is_local {
            SsRect::new(vs.left, vs.top, vs.right, vs.bottom)
        } else if let Some(&(w, h)) = reported.get(&screen.name) {
            SsRect::new(0, 0, w.max(320), h.max(240))
        } else {
            SsRect::new(0, 0, screen.width.max(320), screen.height.max(240))
        };
        screens.insert(screen.name.clone(), rect);
    }

    let mut graph = LayoutGraph::new();
    for link in &config.links {
        graph.link(&link.from, Edge::from_label(&link.edge), &link.to);
    }

    Ok((MultiScreenSpace::new(screens, graph), lock_x, lock_y))
}

/// Rebuild and atomically swap the running router's screen space without
/// restarting it (issue 1). Re-fetches monitor topology, so monitor/DPI/
/// resolution/layout changes apply live. On failure the old space is kept and
/// an error is returned. The local screen name must be preserved.
#[tauri::command]
async fn reconfigure_router(
    config: RouterConfig,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    if !state.router_running.load(Ordering::SeqCst) {
        return Err("router is not running; use start instead.".to_string());
    }

    let current_local = state
        .router_local_name
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    if let Some(local) = &current_local {
        let new_local = config
            .screens
            .iter()
            .find(|screen| screen.is_local)
            .map(|screen| screen.name.clone());
        if new_local.as_deref() != Some(local.as_str()) {
            return Err(format!("reconfigure must keep the local screen '{local}'."));
        }
    }

    // Build first; only swap on success so the live router never sees a
    // half-built or failed space.
    let (space, _lock_x, _lock_y) = build_multi_screen_space(&config, &state.screen_sizes)?;
    if let Ok(mut slot) = state.router_space.lock() {
        *slot = Some(Arc::new(space));
    }

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Router reconfigured live (no restart).".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Stop the multi-screen router (B1.4).
#[tauri::command]
async fn stop_multi_screen_router(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.router_running.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = state.router_space.lock() {
        *slot = None;
    }
    if let Ok(mut name) = state.router_local_name.lock() {
        *name = None;
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Multi-screen router stop requested.".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}

#[derive(Debug, Serialize)]
struct DiscoveredPeer {
    host_name: String,
    ip: String,
    reachable: bool,
}

/// Discover Tailnet peers that appear to be running TailKVM by probing the KVM
/// port on each online peer (roadmap F1). `reachable` means the port accepted a
/// TCP connection within the timeout.
#[tauri::command]
async fn discover_tailkvm_peers(port: Option<u16>) -> Result<Vec<DiscoveredPeer>, String> {
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    let status = get_tailscale_status()?;

    let mut discovered = Vec::new();
    for peer in status.peers.iter().filter(|peer| peer.online) {
        let Some(ip) = peer.tailscale_ips.first() else {
            continue;
        };
        let addr = format!("{ip}:{port}");
        let reachable = matches!(
            time::timeout(Duration::from_millis(400), TcpStream::connect(&addr)).await,
            Ok(Ok(_))
        );
        discovered.push(DiscoveredPeer {
            host_name: peer.host_name.clone(),
            ip: ip.clone(),
            reachable,
        });
    }

    Ok(discovered)
}

#[tauri::command]
fn get_tailscale_status() -> Result<TailnetStatus, String> {
    let output = run_tailscale_status_json()?;

    if !output.status.success() {
        return Err(format!(
            "tailscale status --json failed. exit={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("tailscale stdout is not valid UTF-8: {e}"))?;

    let root: Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("failed to parse tailscale status JSON: {e}"))?;

    let backend_state = root
        .get("BackendState")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();

    let self_node = root
        .get("Self")
        .map(|value| parse_node("self".to_string(), value));

    let mut peers = Vec::new();

    if let Some(peer_map) = root.get("Peer").and_then(Value::as_object) {
        for (id, node) in peer_map {
            peers.push(parse_node(id.to_string(), node));
        }
    }

    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.host_name.to_lowercase().cmp(&b.host_name.to_lowercase()))
    });

    Ok(TailnetStatus {
        backend_state,
        self_node,
        raw_peer_count: peers.len(),
        peers,
    })
}

async fn handle_receiver_stream(
    stream: TcpStream,
    peer_addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
    receiver_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    accept_incoming: Arc<AtomicBool>,
) {
    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.role = "receiver".to_string();
        snapshot.connected = true;
        snapshot.peer_addr = Some(peer_addr.clone());
        snapshot.peer_name = None;
        snapshot.last_event = format!("Accepted connection from {peer_addr}.");
    });

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Outbound channel so this side can push unsolicited messages (clipboard)
    // back to the controller, enabling bidirectional sync.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WireMessage>();
    if let Ok(mut guard) = receiver_tx.lock() {
        *guard = Some(out_tx);
    }

    // Safety net: keys/buttons the controller pressed but has not released yet.
    // If the connection drops mid-press we release these on the way out so
    // nothing stays stuck on this receiver. Reuses the same tracking helpers as
    // the controller-side capture loop.
    let mut held_keys: Vec<(u16, u16, bool)> = Vec::new();
    let mut held_buttons: Vec<String> = Vec::new();

    // Poll for monitor hotplug / resolution change and re-send ScreenInfo so the
    // controller's router keeps the correct remote size (roadmap #4 hotplug).
    let mut topology_check = time::interval(Duration::from_secs(5));
    topology_check.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_screen_size: Option<(i32, i32)> = None;

    loop {
        let read = tokio::select! {
            read = lines.next_line() => read,
            outbound = out_rx.recv() => {
                match outbound {
                    Some(message) => {
                        if let Err(err) = write_wire(&mut write_half, &message).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Receiver failed to send outbound: {err}");
                            });
                            break;
                        }
                        continue;
                    }
                    None => break,
                }
            }
            _ = topology_check.tick() => {
                if let Ok(topology) = tailkvm_win32::monitor::get_monitor_topology() {
                    let size = (topology.virtual_screen.width, topology.virtual_screen.height);
                    if last_screen_size.is_some() && last_screen_size != Some(size) {
                        let info = WireMessage::ScreenInfo {
                            name: local_machine_name(),
                            virtual_width: size.0,
                            virtual_height: size.1,
                        };
                        if write_wire(&mut write_half, &info).await.is_ok() {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event =
                                    format!("Monitor change: re-sent ScreenInfo {}x{}.", size.0, size.1);
                            });
                        }
                    }
                    last_screen_size = Some(size);
                }
                continue;
            }
            _ = &mut cancel_rx => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event =
                        "Receiver session replaced by a newer controller connection.".to_string();
                });
                break;
            }
        };

        match read {
            Ok(Some(line)) => match decode_line(&line) {
                Ok(WireMessage::Hello {
                    machine_name,
                    app_version,
                }) => {
                    let accepted = accept_incoming.load(Ordering::SeqCst);

                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.peer_name = Some(machine_name.clone());
                        snapshot.last_event = if accepted {
                            format!("Hello from {machine_name} / app {app_version}.")
                        } else {
                            format!("Rejected connection from {machine_name} (not accepting).")
                        };
                    });

                    let ack = WireMessage::HelloAck {
                        receiver_machine_name: local_machine_name(),
                        accepted,
                        message: if accepted {
                            "accepted".to_string()
                        } else {
                            "receiver is not accepting connections".to_string()
                        },
                    };

                    if let Err(err) = write_wire(&mut write_half, &ack).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send HelloAck: {err}");
                        });
                        break;
                    }

                    if !accepted {
                        // Politely close the rejected connection.
                        break;
                    }

                    if let Err(err) = send_local_keyboard_layout(&mut write_half).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send KeyboardLayout: {err}");
                        });
                    }

                    // Report our real virtual-screen size so the controller's
                    // router can size this screen accurately (B1.7).
                    if let Ok(topology) = tailkvm_win32::monitor::get_monitor_topology() {
                        let info = WireMessage::ScreenInfo {
                            name: local_machine_name(),
                            virtual_width: topology.virtual_screen.width,
                            virtual_height: topology.virtual_screen.height,
                        };
                        if let Err(err) = write_wire(&mut write_half, &info).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Failed to send ScreenInfo: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::KeyboardLayout {
                    language_id,
                    keyboard_type,
                    is_jis_keyboard: _,
                    is_japanese_locale: _,
                    label,
                }) => {
                    apply_peer_keyboard_layout(&tcp_state, language_id, keyboard_type, &label);
                }
                Ok(WireMessage::MouseSetPosition { x, y }) => {
                    // Inject a real absolute mouse move (SendInput) instead of
                    // SetCursorPos: a suppressed/hidden cursor (no physical
                    // mouse, touch input, hide-while-typing) only becomes
                    // visible again on actual mouse input, and SetCursorPos
                    // does not count as input — the cursor moved invisibly.
                    match tailkvm_win32::mouse::send_absolute_mouse_move(x, y) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("MouseSetPosition applied. x={x}, y={y}");
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after set: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseSetPosition failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::ClipboardText { text }) => {
                    // Remember what we are about to apply so the clipboard
                    // watcher does not echo it back to the controller.
                    if let Ok(mut guard) = clipboard_guard.lock() {
                        guard.mark_applied(&text);
                    }
                    match tailkvm_win32::clipboard::set_clipboard_text(&text) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "ClipboardText applied. chars={}",
                                    text.chars().count()
                                );
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("ClipboardText failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::KeyboardText { text }) => {
                    match tailkvm_win32::keyboard::send_keyboard_text(&text) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("KeyboardText applied. chars={}", text.chars().count());
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("KeyboardText failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::KeyboardKey {
                    vk,
                    scan_code,
                    down,
                    extended,
                }) => {
                    track_key_press(&mut held_keys, vk, scan_code, extended, down);
                    match tailkvm_win32::keyboard::send_key_event(vk, scan_code, down, extended) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "KeyboardKey applied. vk=0x{vk:02x}, scan=0x{scan_code:02x}, down={down}, extended={extended}"
                                );
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("KeyboardKey failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseWheel { delta, horizontal }) => {
                    match tailkvm_win32::mouse::send_mouse_wheel(delta, horizontal) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "MouseWheel applied. delta={delta}, horizontal={horizontal}"
                                );
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after wheel: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseWheel failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseButton { button, down }) => {
                    track_button_press(&mut held_buttons, &button, down);
                    match tailkvm_win32::mouse::send_mouse_button(&button, down) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("MouseButton applied. button={button}, down={down}");
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after button: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseButton failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseMove { dx, dy }) => {
                    match tailkvm_win32::mouse::send_relative_mouse_move(dx, dy) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("MouseMove applied. dx={dx}, dy={dy}");
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after move: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseMove failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::Heartbeat { seq, unix_ms: _ }) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.role = "receiver".to_string();
                        snapshot.connected = true;
                        snapshot.heartbeat_seq = seq;
                        snapshot.last_heartbeat_ms = Some(now_unix_ms());
                        snapshot.last_event = format!("Heartbeat received. seq={seq}");
                    });

                    let ack = WireMessage::HeartbeatAck {
                        seq,
                        unix_ms: now_unix_ms(),
                    };

                    if let Err(err) = write_wire(&mut write_half, &ack).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send HeartbeatAck: {err}");
                        });
                        break;
                    }
                }
                Ok(WireMessage::Disconnect { reason }) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Peer disconnected: {reason}");
                    });
                    break;
                }
                Ok(other) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Receiver ignored message: {other:?}");
                    });
                }
                Err(err) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Receiver decode error: {err}");
                    });
                }
            },
            Ok(None) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = "Peer closed TCP connection.".to_string();
                });
                break;
            }
            Err(err) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = format!("Receiver read error: {err}");
                });
                break;
            }
        }
    }

    // Drop the outbound channel so clipboard sync stops targeting a dead session.
    if let Ok(mut guard) = receiver_tx.lock() {
        *guard = None;
    }

    // Release anything the controller left held when the session ended, so a
    // mid-press disconnect cannot leave a stuck key or button on this machine.
    let released_keys = held_keys.len();
    let released_buttons = held_buttons.len();
    for (vk, scan_code, extended) in held_keys.drain(..) {
        let _ = tailkvm_win32::keyboard::send_key_event(vk, scan_code, false, extended);
    }
    for button in held_buttons.drain(..) {
        let _ = tailkvm_win32::mouse::send_mouse_button(&button, false);
    }

    update_tcp_state(&tcp_state, |snapshot| {
        if snapshot.role == "receiver" {
            snapshot.connected = false;
        }
        if released_keys > 0 || released_buttons > 0 {
            snapshot.last_event = format!(
                "Receiver disconnected. Released {released_keys} stuck key(s), {released_buttons} stuck button(s)."
            );
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_controller_session(
    addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut command_rx: mpsc::UnboundedReceiver<WireMessage>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    screen_sizes: Arc<Mutex<HashMap<String, (i32, i32)>>>,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    origin_name: String,
) {
    match TcpStream::connect(&addr).await {
        Ok(stream) => {
            // Disable Nagle so single control messages (mouse moves, key events)
            // go out immediately rather than being batched (KVM latency).
            let _ = stream.set_nodelay(true);

            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = true;
                snapshot.peer_addr = Some(addr.clone());
                snapshot.last_event = format!("TCP connected to {addr}.");
            });

            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();

            let hello = WireMessage::Hello {
                machine_name: local_machine_name(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            };

            if let Err(err) = write_wire(&mut write_half, &hello).await {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.connected = false;
                    snapshot.last_event = format!("Failed to send Hello: {err}");
                });
                return;
            }

            if let Err(err) = send_local_keyboard_layout(&mut write_half).await {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = format!("Failed to send KeyboardLayout: {err}");
                });
            }

            let mut heartbeat_seq: u64 = 0;
            let mut interval = time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                match decode_line(&line) {
                                    Ok(WireMessage::HelloAck { receiver_machine_name, accepted, message }) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.peer_name = Some(receiver_machine_name.clone());
                                            snapshot.connected = accepted;
                                            snapshot.last_event = format!("HelloAck from {receiver_machine_name}: {message}");
                                        });
                                    }
                                    Ok(WireMessage::KeyboardLayout { language_id, keyboard_type, is_jis_keyboard: _, is_japanese_locale: _, label }) => {
                                        apply_peer_keyboard_layout(&tcp_state, language_id, keyboard_type, &label);
                                    }
                                    Ok(WireMessage::HeartbeatAck { seq, unix_ms: _ }) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.role = "controller".to_string();
                                            snapshot.connected = true;
                                            snapshot.heartbeat_seq = seq;
                                            snapshot.last_heartbeat_ms = Some(now_unix_ms());
                                            snapshot.last_event = format!("HeartbeatAck received. seq={seq}");
                                        });
                                    }
                                    Ok(WireMessage::ScreenInfo { name, virtual_width, virtual_height }) => {
                                        // Record the peer's real virtual-screen size so the
                                        // router can size this remote accurately (B1.7).
                                        if let Ok(mut sizes) = screen_sizes.lock() {
                                            sizes.insert(name.clone(), (virtual_width, virtual_height));
                                        }
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "ScreenInfo from {name}: {virtual_width}x{virtual_height}."
                                            );
                                        });
                                    }
                                    Ok(WireMessage::ClipboardText { text }) => {
                                        // Bidirectional clipboard: apply the peer's
                                        // text and mark the guard so our watcher
                                        // does not echo it back.
                                        if let Ok(mut guard) = clipboard_guard.lock() {
                                            guard.mark_applied(&text);
                                        }
                                        let chars = text.chars().count();
                                        let _ = tailkvm_win32::clipboard::set_clipboard_text(&text);
                                        // Hub relay: forward to the other screens so
                                        // all clients stay in sync (B1.5 relay).
                                        let relayed =
                                            relay_clipboard(&sessions, &origin_name, &text);
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "ClipboardText applied (chars={chars}), relayed to {relayed} sibling(s)."
                                            );
                                        });
                                    }
                                    Ok(WireMessage::MousePosition { x, y }) => {
                                        let remote_state = remote_control
                                            .lock()
                                            .map(|state| state.clone())
                                            .unwrap_or_default();

                                        if remote_state.active
                                            && !remote_state.seamless
                                            && is_remote_return_edge(x, y, &remote_state)
                                        {
                                            capture_running.store(false, Ordering::SeqCst);

                                            if let Ok(mut state) = remote_control.lock() {
                                                state.active = false;
                                            }

                                            update_tcp_state(&tcp_state, |snapshot| {
                                                snapshot.role = "controller".to_string();
                                                snapshot.connected = true;
                                                snapshot.last_event = format!(
                                                    "Remote return edge reached at x={}, y={}. Capture stop requested.",
                                                    x, y
                                                );
                                            });
                                        } else {
                                            update_tcp_state(&tcp_state, |snapshot| {
                                                snapshot.role = "controller".to_string();
                                                snapshot.connected = true;
                                                snapshot.last_event = format!(
                                                    "Remote MousePosition x={}, y={}",
                                                    x, y
                                                );
                                            });
                                        }
                                    }
                                    Ok(other) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!("Controller ignored message: {other:?}");
                                        });
                                    }
                                    Err(err) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!("Controller decode error: {err}");
                                        });
                                    }
                                }
                            }
                            Ok(None) => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = "Peer closed TCP connection.".to_string();
                                });
                                break;
                            }
                            Err(err) => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = format!("Controller read error: {err}");
                                });
                                break;
                            }
                        }
                    }
                    maybe_outbound = command_rx.recv() => {
                        match maybe_outbound {
                            Some(outbound) => {
                                if let Err(err) = write_wire(&mut write_half, &outbound).await {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.last_event = format!("Failed to send command message: {err}");
                                    });
                                    break;
                                }

                                // Skip the per-event UI update for high-rate mouse
                                // moves: it would allocate + lock ~30x/s and clobber
                                // the capture loop's throttled progress summary.
                                if !matches!(outbound, WireMessage::MouseMove { .. }) {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.role = "controller".to_string();
                                        snapshot.connected = true;
                                        snapshot.last_event =
                                            format!("Sent command message: {outbound:?}");
                                    });
                                }
                            }
                            None => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = "Controller command channel closed.".to_string();
                                });
                                break;
                            }
                        }
                    }
                    _ = interval.tick() => {
                        heartbeat_seq += 1;

                        let heartbeat = WireMessage::Heartbeat {
                            seq: heartbeat_seq,
                            unix_ms: now_unix_ms(),
                        };

                        if let Err(err) = write_wire(&mut write_half, &heartbeat).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Failed to send Heartbeat: {err}");
                            });
                            break;
                        }

                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.role = "controller".to_string();
                            snapshot.connected = true;
                            snapshot.heartbeat_seq = heartbeat_seq;
                            snapshot.last_event = format!("Heartbeat sent. seq={heartbeat_seq}");
                        });
                    }
                }
            }
        }
        Err(err) => {
            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = false;
                snapshot.peer_addr = Some(addr.clone());
                snapshot.last_event = format!("Failed to connect to {addr}: {err}");
            });
        }
    }

    update_tcp_state(&tcp_state, |snapshot| {
        if snapshot.role == "controller" {
            snapshot.connected = false;
        }
    });
}

async fn send_current_mouse_position<W>(writer: &mut W) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let position = tailkvm_win32::cursor::get_cursor_position()?;

    write_wire(
        writer,
        &WireMessage::MousePosition {
            x: position.x,
            y: position.y,
        },
    )
    .await
}

async fn send_local_keyboard_layout<W>(writer: &mut W) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let info = tailkvm_win32::keyboard_layout::current_keyboard_layout();

    write_wire(
        writer,
        &WireMessage::KeyboardLayout {
            language_id: info.language_id,
            keyboard_type: info.keyboard_type,
            is_jis_keyboard: info.is_jis_keyboard,
            is_japanese_locale: info.is_japanese_locale,
            label: info.label,
        },
    )
    .await
}

fn apply_peer_keyboard_layout(
    tcp_state: &Arc<Mutex<TcpSessionSnapshot>>,
    peer_language_id: u16,
    peer_keyboard_type: i32,
    peer_label: &str,
) {
    let local = tailkvm_win32::keyboard_layout::current_keyboard_layout();
    let warning = local.mismatch_with(peer_language_id, peer_keyboard_type);

    update_tcp_state(tcp_state, |snapshot| {
        snapshot.local_keyboard_layout = Some(local.label.clone());
        snapshot.peer_keyboard_layout = Some(peer_label.to_string());
        snapshot.keyboard_layout_warning = warning.clone();
        snapshot.last_event = match &warning {
            Some(message) => message.clone(),
            None => format!(
                "Keyboard layout match. local={}, peer={peer_label}",
                local.label
            ),
        };
    });
}

async fn write_wire<W>(writer: &mut W, message: &WireMessage) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let line = encode_line(message)?;
    writer
        .write_all(&line)
        .await
        .map_err(|e| format!("failed to write wire message: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("failed to flush wire message: {e}"))
}

/// The currently connected peer's reported virtual-screen size (width, height),
/// from its ScreenInfo message (stored in `screen_sizes` keyed by machine name).
/// The UI uses this to draw the remote tile at the peer's real resolution.
#[tauri::command]
fn get_peer_screen_size(state: State<'_, AppState>) -> Option<(i32, i32)> {
    let name = tcp_snapshot(&state.tcp).peer_name?;
    let sizes = state
        .screen_sizes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    sizes.get(&name).copied()
}

fn tcp_snapshot(state: &Arc<Mutex<TcpSessionSnapshot>>) -> TcpSessionSnapshot {
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

fn update_tcp_state(
    state: &Arc<Mutex<TcpSessionSnapshot>>,
    update: impl FnOnce(&mut TcpSessionSnapshot),
) {
    if let Ok(mut snapshot) = state.lock() {
        update(&mut snapshot);
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn local_machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-windows-machine".to_string())
}

fn run_tailscale_status_json() -> Result<std::process::Output, String> {
    let mut candidates = vec!["tailscale.exe".to_string(), "tailscale".to_string()];

    if cfg!(target_os = "windows") {
        candidates.push(r"C:\Program Files\Tailscale\tailscale.exe".to_string());
        candidates.push(r"C:\Program Files (x86)\Tailscale\tailscale.exe".to_string());
    }

    let mut errors = Vec::new();

    for exe in candidates {
        match Command::new(&exe).args(["status", "--json"]).output() {
            Ok(output) => return Ok(output),
            Err(err) => errors.push(format!("{exe}: {err}")),
        }
    }

    Err(format!(
        "failed to execute tailscale CLI. Tried: {}",
        errors.join(" | ")
    ))
}

fn parse_node(id: String, value: &Value) -> TailnetNode {
    TailnetNode {
        id,
        host_name: get_string(value, "HostName").unwrap_or_else(|| "(unknown)".to_string()),
        dns_name: get_string(value, "DNSName"),
        os: get_string(value, "OS"),
        online: get_bool(value, "Online").unwrap_or(false),
        active: get_bool(value, "Active"),
        tailscale_ips: get_string_array(value, "TailscaleIPs"),
        user: get_string(value, "User"),
        relay: get_string(value, "Relay"),
        cur_addr: get_string(value, "CurAddr"),
        last_seen: get_string(value, "LastSeen"),
        tx_bytes: get_u64(value, "TxBytes"),
        rx_bytes: get_u64(value, "RxBytes"),
    }
}

fn get_string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(ToString::to_string)
}

fn get_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key)?.as_bool()
}

fn get_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

fn get_string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Guarantee physical-pixel virtual-desktop coordinates across cursor,
    // monitor, and SendInput APIs regardless of the embedded manifest.
    tailkvm_win32::monitor::ensure_per_monitor_dpi_aware();

    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_app_status,
            get_tailscale_status,
            get_windows_monitor_topology,
            get_peer_screen_size,
            get_keyboard_layout,
            get_tcp_session_state,
            install_firewall_rule,
            send_test_keyboard_text,
            send_clipboard_text,
            set_clipboard_sync,
            send_test_key_tap,
            start_keyboard_hook_capture,
            stop_keyboard_hook_capture,
            send_test_mouse_double_click,
            send_test_mouse_click,
            start_mouse_hook_capture,
            stop_mouse_hook_capture,
            start_tcp_receiver,
            connect_tcp_peer,
            disconnect_tcp_peer,
            set_accept_incoming,
            discover_tailkvm_peers,
            connect_screen,
            disconnect_screen,
            list_screens,
            get_lock_state,
            save_layout,
            load_layout,
            start_multi_screen_router,
            reconfigure_router,
            stop_multi_screen_router,
            send_test_mouse_move,
            start_mouse_capture,
            stop_mouse_capture,
            start_raw_mouse_diagnostic,
            stop_raw_mouse_diagnostic,
            set_resolve_characters
        ])
        .setup(|app| {
            // Startup auto-connect (roadmap B1.6): if a saved layout opts in,
            // connect its remote screens. The router is NOT auto-started (it
            // captures input); the user starts it explicitly.
            {
                let layout = read_saved_layout();
                if layout.auto_connect {
                    let app_state = app.state::<AppState>();
                    for screen in layout.screens.iter().filter(|s| !s.is_local) {
                        if screen.host.trim().is_empty() {
                            continue;
                        }
                        let addr = format!("{}:{}", screen.host.trim(), DEFAULT_TAILKVM_PORT);
                        let _ = start_named_session(&app_state, &screen.name, &addr);
                    }
                }
            }

            let show_i = MenuItem::with_id(app, "show", "Open TailKVM", true, None::<&str>)?;
            let pause_i =
                MenuItem::with_id(app, "pause", "Pause input forwarding", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let menu = Menu::with_items(app, &[&show_i, &pause_i, &quit_i])?;

            let _tray = TrayIconBuilder::new()
                .tooltip("TailKVM")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "pause" => {
                        // Manual kill switch from the tray: stop all forwarding
                        // and release stuck input (complements Ctrl+Alt+Pause).
                        let app_state = app.state::<AppState>();
                        pause_all_capture(&app_state);
                        update_tcp_state(&app_state.tcp, |snapshot| {
                            snapshot.last_event =
                                "All input forwarding paused from tray.".to_string();
                        });
                    }
                    "quit" => app.exit(0),
                    _ => println!("unhandled tray menu event: {:?}", event.id),
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running TailKVM");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tailkvm_win32::cursor::CursorPosition;
    use tailkvm_win32::monitor::RectI32;

    /// Build a `RectI32` the same way `RectI32::new` does (its constructor is
    /// private to the monitor module, but the fields are public).
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RectI32 {
        RectI32 {
            left,
            top,
            right,
            bottom,
            width: right - left,
            height: bottom - top,
        }
    }

    fn pos(x: i32, y: i32) -> CursorPosition {
        CursorPosition { x, y }
    }

    #[test]
    fn normalize_edge_keeps_valid_and_defaults_to_right() {
        assert_eq!(normalize_edge("left".to_string()), "left");
        assert_eq!(normalize_edge("right".to_string()), "right");
        assert_eq!(normalize_edge("top".to_string()), "top");
        assert_eq!(normalize_edge("bottom".to_string()), "bottom");
        // Trimmed + case-insensitive.
        assert_eq!(normalize_edge("  RIGHT ".to_string()), "right");
        assert_eq!(normalize_edge("Top".to_string()), "top");
        // Unknown falls back to the default edge.
        assert_eq!(normalize_edge("diagonal".to_string()), "right");
        assert_eq!(normalize_edge(String::new()), "right");
    }

    #[test]
    fn is_cursor_at_edge_respects_margin_on_each_side() {
        let r = rect(0, 0, 1920, 1080);
        let margin = 3;

        // right edge: x >= right - 1 - margin = 1916
        assert!(is_cursor_at_edge(&pos(1916, 500), &r, "right", margin));
        assert!(!is_cursor_at_edge(&pos(1915, 500), &r, "right", margin));

        // left edge: x <= left + margin = 3
        assert!(is_cursor_at_edge(&pos(3, 500), &r, "left", margin));
        assert!(!is_cursor_at_edge(&pos(4, 500), &r, "left", margin));

        // top edge: y <= top + margin = 3
        assert!(is_cursor_at_edge(&pos(500, 3), &r, "top", margin));
        assert!(!is_cursor_at_edge(&pos(500, 4), &r, "top", margin));

        // bottom edge: y >= bottom - 1 - margin = 1076
        assert!(is_cursor_at_edge(&pos(500, 1076), &r, "bottom", margin));
        assert!(!is_cursor_at_edge(&pos(500, 1075), &r, "bottom", margin));
    }

    #[test]
    fn is_cursor_at_edge_handles_negative_origin_virtual_screen() {
        // Multi-monitor virtual screen whose primary is not at (0,0).
        let r = rect(-1920, -200, 1920, 1080);
        let margin = 3;

        // right edge: x >= 1920 - 1 - 3 = 1916
        assert!(is_cursor_at_edge(&pos(1916, 0), &r, "right", margin));
        assert!(!is_cursor_at_edge(&pos(1900, 0), &r, "right", margin));

        // left edge: x <= -1920 + 3 = -1917
        assert!(is_cursor_at_edge(&pos(-1917, 0), &r, "left", margin));
        assert!(!is_cursor_at_edge(&pos(-1916, 0), &r, "left", margin));
    }

    #[test]
    fn remote_entry_position_enters_opposite_edge_with_aspect_mapping() {
        let local = rect(0, 0, 1920, 1080);
        let (rw, rh) = (1280, 720);
        let inset = 4;

        // Exit local RIGHT -> enter remote LEFT (small x), y mapped by ratio.
        let entry = remote_entry_position(&pos(1919, 540), &local, "right", rw, rh);
        assert_eq!(entry.x, inset);
        // ratio = 540/1080 = 0.5 -> y = (720-1)*0.5 = 359.5 -> 360
        assert_eq!(entry.y, 360);

        // Exit local LEFT -> enter remote RIGHT (large x).
        let entry = remote_entry_position(&pos(0, 0), &local, "left", rw, rh);
        assert_eq!(entry.x, rw - 1 - inset);
        assert_eq!(entry.y, 0);

        // Exit local TOP -> enter remote BOTTOM (large y), x mapped by ratio.
        let entry = remote_entry_position(&pos(960, 0), &local, "top", rw, rh);
        assert_eq!(entry.y, rh - 1 - inset);
        // ratio = 960/1920 = 0.5 -> x = (1280-1)*0.5 = 639.5 -> 640
        assert_eq!(entry.x, 640);

        // Exit local BOTTOM -> enter remote TOP (small y).
        let entry = remote_entry_position(&pos(960, 1079), &local, "bottom", rw, rh);
        assert_eq!(entry.y, inset);
    }

    #[test]
    fn remote_entry_position_clamps_ratio_within_bounds() {
        // Cursor far below the rect should still map within [0, rh-1].
        let local = rect(0, 0, 1920, 1080);
        let entry = remote_entry_position(&pos(1919, 100_000), &local, "right", 1280, 720);
        assert!(
            entry.y >= 0 && entry.y <= 719,
            "y out of range: {}",
            entry.y
        );
    }

    #[test]
    fn local_return_position_uses_safe_margin_floor_of_8() {
        let r = rect(0, 0, 1920, 1080);

        // margin below 8 is bumped to 8.
        let ret = local_return_position(&pos(1919, 540), &r, "right", 3);
        assert_eq!(ret.x, 1920 - 1 - 8);
        assert!(ret.y >= 8 && ret.y <= 1080 - 1 - 8);

        let ret = local_return_position(&pos(0, 540), &r, "left", 3);
        assert_eq!(ret.x, 8);

        let ret = local_return_position(&pos(960, 0), &r, "top", 3);
        assert_eq!(ret.y, 8);

        let ret = local_return_position(&pos(960, 1079), &r, "bottom", 3);
        assert_eq!(ret.y, 1080 - 1 - 8);
    }

    #[test]
    fn is_remote_return_edge_mirrors_switch_edge() {
        let base = RemoteControlState {
            active: true,
            switch_edge: "right".to_string(),
            remote_width: 1920,
            remote_height: 1080,
            edge_margin: 3,
            seamless: false,
        };
        // margin floor is 8 inside the function.

        // Switch right -> entered remote from left -> return at remote LEFT edge.
        assert!(is_remote_return_edge(8, 500, &base));
        assert!(!is_remote_return_edge(9, 500, &base));

        // Switch left -> return at remote RIGHT edge: x >= width-1-8 = 1911.
        let left = RemoteControlState {
            switch_edge: "left".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(1911, 500, &left));
        assert!(!is_remote_return_edge(1910, 500, &left));

        // Switch top -> return at remote BOTTOM edge: y >= height-1-8 = 1071.
        let top = RemoteControlState {
            switch_edge: "top".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(500, 1071, &top));
        assert!(!is_remote_return_edge(500, 1070, &top));

        // Switch bottom -> return at remote TOP edge: y <= 8.
        let bottom = RemoteControlState {
            switch_edge: "bottom".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(500, 8, &bottom));
        assert!(!is_remote_return_edge(500, 9, &bottom));
    }

    #[test]
    fn key_to_test_key_maps_known_keys_and_extended_flags() {
        assert_eq!(key_to_test_key("enter"), Some((0x0D, 0, false, "Enter")));
        assert_eq!(key_to_test_key("return"), Some((0x0D, 0, false, "Enter")));
        assert_eq!(
            key_to_test_key("backspace"),
            Some((0x08, 0, false, "Backspace"))
        );
        assert_eq!(key_to_test_key("esc"), Some((0x1B, 0, false, "Escape")));
        // Arrow / navigation keys are extended.
        assert_eq!(key_to_test_key("left"), Some((0x25, 0, true, "ArrowLeft")));
        assert_eq!(key_to_test_key("delete"), Some((0x2E, 0, true, "Delete")));
        // Unknown keys return None (caller rejects them).
        assert_eq!(key_to_test_key("f13"), None);
        assert_eq!(key_to_test_key(""), None);
    }

    #[test]
    fn track_button_press_dedups_and_releases() {
        let mut pressed: Vec<String> = Vec::new();

        track_button_press(&mut pressed, "left", true);
        track_button_press(&mut pressed, "left", true); // duplicate down ignored
        assert_eq!(pressed, vec!["left".to_string()]);

        track_button_press(&mut pressed, "right", true);
        assert_eq!(pressed, vec!["left".to_string(), "right".to_string()]);

        // Releasing one button leaves the other still tracked.
        track_button_press(&mut pressed, "left", false);
        assert_eq!(pressed, vec!["right".to_string()]);

        // Releasing an unpressed button is a no-op (no underflow / phantom).
        track_button_press(&mut pressed, "middle", false);
        assert_eq!(pressed, vec!["right".to_string()]);
    }

    #[test]
    fn track_key_press_dedups_by_vk_scan_extended() {
        let mut pressed: Vec<(u16, u16, bool)> = Vec::new();

        track_key_press(&mut pressed, 0x41, 0x1E, false, true);
        track_key_press(&mut pressed, 0x41, 0x1E, false, true); // duplicate down
        assert_eq!(pressed, vec![(0x41, 0x1E, false)]);

        // Same vk/scan but extended differs -> a distinct held key.
        track_key_press(&mut pressed, 0x41, 0x1E, true, true);
        assert_eq!(pressed, vec![(0x41, 0x1E, false), (0x41, 0x1E, true)]);

        // Releasing the non-extended one keeps the extended one held.
        track_key_press(&mut pressed, 0x41, 0x1E, false, false);
        assert_eq!(pressed, vec![(0x41, 0x1E, true)]);

        // Releasing a key that was never pressed is a no-op.
        track_key_press(&mut pressed, 0x09, 0, false, false);
        assert_eq!(pressed, vec![(0x41, 0x1E, true)]);
    }
}
