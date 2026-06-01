use serde::Serialize;
use serde_json::Value;
use std::{
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
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
}

impl Default for RemoteControlState {
    fn default() -> Self {
        Self {
            active: false,
            switch_edge: "right".to_string(),
            remote_width: 1920,
            remote_height: 1080,
            edge_margin: 3,
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

#[derive(Clone)]
struct AppState {
    tcp: Arc<Mutex<TcpSessionSnapshot>>,
    receiver_running: Arc<AtomicBool>,
    capture_running: Arc<AtomicBool>,
    mouse_hook_running: Arc<AtomicBool>,
    keyboard_hook_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    controller_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            tcp: Arc::new(Mutex::new(TcpSessionSnapshot::default())),
            receiver_running: Arc::new(AtomicBool::new(false)),
            capture_running: Arc::new(AtomicBool::new(false)),
            mouse_hook_running: Arc::new(AtomicBool::new(false)),
            keyboard_hook_running: Arc::new(AtomicBool::new(false)),
            remote_control: Arc::new(Mutex::new(RemoteControlState::default())),
            mouse_hook: Arc::new(Mutex::new(None)),
            keyboard_hook: Arc::new(Mutex::new(None)),
            controller_tx: Arc::new(Mutex::new(None)),
        }
    }
}

#[tauri::command]
fn get_app_status() -> String {
    "TailKVM backend is running. Task 5 OK.".to_string()
}

#[tauri::command]
fn get_windows_monitor_topology() -> Result<MonitorTopology, String> {
    tailkvm_win32::monitor::get_monitor_topology()
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
        sender,
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

fn start_mouse_hook_forwarding(
    sender: mpsc::UnboundedSender<WireMessage>,
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

    tauri::async_runtime::spawn(async move {
        let mut event_count: u64 = 0;
        let mut pressed_buttons: Vec<String> = Vec::new();

        while mouse_hook_running_for_task.load(Ordering::SeqCst) {
            while let Ok(event) = event_rx.try_recv() {
                let message = match event {
                    tailkvm_win32::mouse_hook::MouseHookEvent::Button { button, down } => {
                        if down {
                            if !pressed_buttons.iter().any(|value| value == &button) {
                                pressed_buttons.push(button.clone());
                            }
                        } else {
                            pressed_buttons.retain(|value| value != &button);
                        }

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

            time::sleep(Duration::from_millis(5)).await;
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
        sender,
        state.tcp.clone(),
        state.keyboard_hook_running.clone(),
        state.keyboard_hook.clone(),
        state.capture_running.clone(),
        state.mouse_hook_running.clone(),
        state.mouse_hook.clone(),
        state.remote_control.clone(),
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

fn start_keyboard_hook_forwarding(
    sender: mpsc::UnboundedSender<WireMessage>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    capture_running: Arc<AtomicBool>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    label: &'static str,
) -> Result<(), String> {
    if keyboard_hook_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!("Keyboard hook capture is already running. mode={label}");
        });
        return Ok(());
    }

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

    tauri::async_runtime::spawn(async move {
        let mut event_count: u64 = 0;
        let mut pressed_keys: Vec<(u16, u16, bool)> = Vec::new();

        while keyboard_hook_running_for_task.load(Ordering::SeqCst) {
            while let Ok(event) = event_rx.try_recv() {
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
                        if down {
                            if !pressed_keys.iter().any(|(key_vk, key_scan, key_ext)| {
                                *key_vk == vk && *key_scan == scan_code && *key_ext == extended
                            }) {
                                pressed_keys.push((vk, scan_code, extended));
                            }
                        } else {
                            pressed_keys.retain(|(key_vk, key_scan, key_ext)| {
                                !(*key_vk == vk && *key_scan == scan_code && *key_ext == extended)
                            });
                        }

                        let message = WireMessage::KeyboardKey {
                            vk,
                            scan_code,
                            down,
                            extended,
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

            time::sleep(Duration::from_millis(5)).await;
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

    if let Ok(mut remote_control) = state.remote_control.lock() {
        remote_control.active = false;
        remote_control.switch_edge = switch_edge.clone();
        remote_control.remote_width = remote_width;
        remote_control.remote_height = remote_height;
        remote_control.edge_margin = edge_margin;
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

    let lock_x = virtual_screen.left + (virtual_screen.width / 2);
    let lock_y = virtual_screen.top + (virtual_screen.height / 2);

    let tcp_state = state.tcp.clone();
    let capture_running = state.capture_running.clone();
    let remote_control = state.remote_control.clone();
    let mouse_hook_running = state.mouse_hook_running.clone();
    let mouse_hook = state.mouse_hook.clone();
    let keyboard_hook_running = state.keyboard_hook_running.clone();
    let keyboard_hook = state.keyboard_hook.clone();

    tauri::async_runtime::spawn(async move {
        let mut remote_active = !remote_mode;
        let mut sent_count: u64 = 0;
        let mut skipped_count: u64 = 0;
        let mut ignored_warp_frames: u8 = 0;
        let mut last_mirror_pos: Option<tailkvm_win32::cursor::CursorPosition> = None;

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
                        sender.clone(),
                        tcp_state.clone(),
                        mouse_hook_running.clone(),
                        mouse_hook.clone(),
                        "auto",
                    ) {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Auto click/wheel capture failed: {err}");
                        });
                    }

                    if let Err(err) = start_keyboard_hook_forwarding(
                        sender.clone(),
                        tcp_state.clone(),
                        keyboard_hook_running.clone(),
                        keyboard_hook.clone(),
                        capture_running.clone(),
                        mouse_hook_running.clone(),
                        mouse_hook.clone(),
                        remote_control.clone(),
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

                if skipped_count % 60 == 0 {
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

                    if skipped_count % 20 == 0 {
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

                if sent_count % 15 == 0 {
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

#[tauri::command]
async fn stop_mouse_capture(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
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

                loop {
                    match listener.accept().await {
                        Ok((stream, peer_addr)) => {
                            let peer_addr_text = peer_addr.to_string();
                            let tcp_state_for_client = tcp_state.clone();

                            tauri::async_runtime::spawn(async move {
                                handle_receiver_stream(
                                    stream,
                                    peer_addr_text,
                                    tcp_state_for_client,
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
    let (command_tx, command_rx) = mpsc::unbounded_channel::<WireMessage>();

    if let Ok(mut tx_guard) = state.controller_tx.lock() {
        *tx_guard = Some(command_tx);
    }

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = false;
        snapshot.peer_addr = Some(addr.clone());
        snapshot.peer_name = None;
        snapshot.last_event = format!("Connecting to {addr}...");
    });

    let capture_running = state.capture_running.clone();
    let remote_control = state.remote_control.clone();

    tauri::async_runtime::spawn(async move {
        run_controller_session(addr, tcp_state, command_rx, capture_running, remote_control).await;
    });

    time::sleep(Duration::from_millis(200)).await;
    Ok(tcp_snapshot(&state.tcp))
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

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match decode_line(&line) {
                Ok(WireMessage::Hello {
                    machine_name,
                    app_version,
                }) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.peer_name = Some(machine_name.clone());
                        snapshot.last_event =
                            format!("Hello from {machine_name} / app {app_version}.");
                    });

                    let ack = WireMessage::HelloAck {
                        receiver_machine_name: local_machine_name(),
                        accepted: true,
                        message: "accepted".to_string(),
                    };

                    if let Err(err) = write_wire(&mut write_half, &ack).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send HelloAck: {err}");
                        });
                        break;
                    }

                    if let Err(err) = send_local_keyboard_layout(&mut write_half).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send KeyboardLayout: {err}");
                        });
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
                    match tailkvm_win32::cursor::set_cursor_position(x, y) {
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

    update_tcp_state(&tcp_state, |snapshot| {
        if snapshot.role == "receiver" {
            snapshot.connected = false;
        }
    });
}

async fn run_controller_session(
    addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut command_rx: mpsc::UnboundedReceiver<WireMessage>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
) {
    match TcpStream::connect(&addr).await {
        Ok(stream) => {
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
                                    Ok(WireMessage::MousePosition { x, y }) => {
                                        let remote_state = remote_control
                                            .lock()
                                            .map(|state| state.clone())
                                            .unwrap_or_default();

                                        if remote_state.active
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

                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.role = "controller".to_string();
                                    snapshot.connected = true;
                                    snapshot.last_event = format!("Sent command message: {outbound:?}");
                                });
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

fn tcp_snapshot(state: &Arc<Mutex<TcpSessionSnapshot>>) -> TcpSessionSnapshot {
    state.lock().expect("tcp state mutex poisoned").clone()
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
            get_keyboard_layout,
            get_tcp_session_state,
            install_firewall_rule,
            send_test_keyboard_text,
            send_test_key_tap,
            start_keyboard_hook_capture,
            stop_keyboard_hook_capture,
            send_test_mouse_double_click,
            send_test_mouse_click,
            start_mouse_hook_capture,
            stop_mouse_hook_capture,
            start_tcp_receiver,
            connect_tcp_peer,
            send_test_mouse_move,
            start_mouse_capture,
            stop_mouse_capture
        ])
        .setup(|app| {
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
                        println!("TailKVM pause requested. Input engine is not implemented yet.");
                    }
                    "quit" => app.exit(0),
                    _ => println!("unhandled tray menu event: {:?}", event.id),
                })
                .on_tray_icon_event(|tray, event| match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        let app = tray.app_handle();
                        show_main_window(&app);
                    }
                    _ => {}
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
}
