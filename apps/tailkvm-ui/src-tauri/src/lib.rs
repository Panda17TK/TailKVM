use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
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

mod forwarding;
mod router;
mod seamless;
mod state;
mod tailnet;

use forwarding::*;
use seamless::*;
use state::*;

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
                    .and_then(|sizes| sizes.get(name).map(|peer| (peer.width, peer.height)))
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
            sender_slot: state.controller_tx.clone(),
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
                    "[DEPRECATED: use seamless KVM (#8)] Remote mode armed. Move cursor to {} edge. remote={}x{}, gain={gain:.2}, interval={}ms, max_delta={}, margin={}px.",
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

/// One-shot panic button (recovery route): stop every forwarding path on this
/// machine, release the cursor clip, stop raw-input capture, and abort any
/// session where THIS machine is being controlled (releasing every key/button
/// the controller left held). Reachable from the tray and the UI; the physical
/// equivalent is Ctrl+Alt+Pause, which now works on either machine.
fn emergency_reset_all(state: &AppState) {
    pause_all_capture(state);

    // The seamless engine confines the cursor while a remote is controlled and
    // releases it on its own stop paths — but release here too so recovery
    // does not depend on that loop still being healthy.
    tailkvm_win32::cursor::release_cursor_confine();

    // Stop the raw-input diagnostic if it is running.
    state.raw_mouse_running.store(false, Ordering::SeqCst);
    if let Ok(mut guard) = state.raw_mouse.lock() {
        *guard = None;
    }

    // Abort the inbound (being-controlled) session; its exit path releases
    // held keys/buttons and tells the controller why.
    state.receiver_abort.store(true, Ordering::SeqCst);

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event =
            "EMERGENCY RESET: forwarding stopped, cursor released, inbound control aborted."
                .to_string();
    });
}

#[tauri::command]
async fn emergency_reset(state: State<'_, AppState>) -> Result<TcpSessionSnapshot, String> {
    emergency_reset_all(&state);
    Ok(tcp_snapshot(&state.tcp))
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
    let receiver_abort = state.receiver_abort.clone();

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
                            let receiver_abort_for_client = receiver_abort.clone();

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
                                    receiver_abort_for_client,
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
    screen_sizes: PeerScreenMap,
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
    let status = tailnet::get_tailscale_status()?;

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

/// Build this machine's `ScreenInfo`: virtual-screen size plus monitor rects
/// relative to the virtual origin (the coordinate space of `MouseSetPosition`
/// offsets), so the controller can clamp onto real monitors.
fn local_screen_info(topology: &MonitorTopology) -> WireMessage {
    let vs = &topology.virtual_screen;
    let monitors = topology
        .monitors
        .iter()
        .map(|monitor| {
            let r = &monitor.rect_physical_px;
            [
                r.left - vs.left,
                r.top - vs.top,
                r.right - vs.left,
                r.bottom - vs.top,
            ]
        })
        .collect();
    WireMessage::ScreenInfo {
        name: local_machine_name(),
        virtual_width: vs.width,
        virtual_height: vs.height,
        monitors,
    }
}

/// Report an input-injection failure (e.g. UIPI: an elevated window has focus,
/// so `SendInput` is blocked) back to the controller, throttled to one notice
/// per second so per-event failures cannot flood the control link. Without
/// this, injection silently stops working from the controller's point of view.
async fn notify_injection_failure<W: AsyncWrite + Unpin>(
    write_half: &mut W,
    last_notice: &mut Option<Instant>,
    kind: &str,
    detail: &str,
) {
    let due = (*last_notice).is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
    if !due {
        return;
    }
    *last_notice = Some(Instant::now());
    let notice = WireMessage::InputInjectionFailed {
        kind: kind.to_string(),
        detail: detail.to_string(),
    };
    let _ = write_wire(write_half, &notice).await;
}

// Shared-state handles for one inbound session; mirrors
// spawn_controller_supervisor, which carries the same allowance.
#[allow(clippy::too_many_arguments)]
async fn handle_receiver_stream(
    stream: TcpStream,
    peer_addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
    receiver_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    accept_incoming: Arc<AtomicBool>,
    receiver_abort: Arc<AtomicBool>,
) {
    // A stale abort (fired while no session was active) must not kill this
    // brand-new session on its first failsafe tick.
    receiver_abort.store(false, Ordering::SeqCst);
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

    // Throttle for InputInjectionFailed notices (UIPI failures arrive at event
    // rate; one notice per second is enough for the controller to surface it).
    let mut last_inject_fail_notice: Option<Instant> = None;
    // Throttles for the seamless hot path: MouseSetPosition arrives at polling
    // rate, so the diagnostic echo and state-line updates are rate-limited.
    let mut last_setpos_echo: Option<Instant> = None;
    let mut setpos_count: u64 = 0;

    // Fast failsafe tick (recovery routes while being controlled): physical
    // Ctrl+Alt+Pause on THIS machine, the emergency reset from the tray/UI,
    // and a controller-heartbeat watchdog (a controller killed mid-press never
    // sends a FIN, which would otherwise leave keys stuck for minutes). Each
    // drops the session, and the exit path releases held keys/buttons.
    let mut failsafe_check = time::interval(Duration::from_millis(300));
    failsafe_check.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_heartbeat: Option<Instant> = None;
    const HEARTBEAT_STALE: Duration = Duration::from_secs(8);

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
                        let info = local_screen_info(&topology);
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
            _ = failsafe_check.tick() => {
                if receiver_abort.swap(false, Ordering::SeqCst) {
                    let bye = WireMessage::Disconnect {
                        reason: "receiver emergency reset".to_string(),
                    };
                    let _ = write_wire(&mut write_half, &bye).await;
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Receiver session aborted by emergency reset.".to_string();
                    });
                    break;
                }
                if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
                    let bye = WireMessage::Disconnect {
                        reason: "receiver failsafe (Ctrl+Alt+Pause)".to_string(),
                    };
                    let _ = write_wire(&mut write_half, &bye).await;
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Receiver failsafe Ctrl+Alt+Pause: controller session dropped."
                                .to_string();
                    });
                    break;
                }
                if last_heartbeat.is_some_and(|t| t.elapsed() >= HEARTBEAT_STALE) {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Controller heartbeat stale (>8s): dropping session, releasing held input."
                                .to_string();
                    });
                    break;
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
                    // Arm the heartbeat watchdog from the handshake: a
                    // controller that connects and then stalls without ever
                    // heartbeating is also caught.
                    last_heartbeat = Some(Instant::now());
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
                        let info = local_screen_info(&topology);
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
                            setpos_count += 1;
                            // Rate-limit the diagnostic state line: per-event
                            // formatting and mutex traffic is wasted work at
                            // polling rate.
                            if setpos_count == 1 || setpos_count.is_multiple_of(30) {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.role = "receiver".to_string();
                                    snapshot.connected = true;
                                    snapshot.last_event = format!(
                                        "MouseSetPosition applied. x={x}, y={y} (count={setpos_count})"
                                    );
                                });
                            }

                            // Seamless decides the return locally (no receiver
                            // echo needed), so this MousePosition echo is
                            // diagnostic only — throttle it instead of echoing
                            // every move back at polling rate.
                            let echo_due = last_setpos_echo
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
                            if echo_due {
                                last_setpos_echo = Some(Instant::now());
                                if let Err(err) = send_current_mouse_position(&mut write_half).await
                                {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.last_event = format!(
                                            "Failed to send MousePosition after set: {err}"
                                        );
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_set_position",
                                &err,
                            )
                            .await;
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
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "keyboard_text",
                                &err,
                            )
                            .await;
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
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "keyboard_key",
                                &err,
                            )
                            .await;
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
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_wheel",
                                &err,
                            )
                            .await;
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
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_button",
                                &err,
                            )
                            .await;
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
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_move",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseMove failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::Heartbeat { seq, unix_ms: _ }) => {
                    last_heartbeat = Some(Instant::now());
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
    screen_sizes: PeerScreenMap,
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

            // Inbound watchdog (recovery route): heartbeats go out every 2s
            // and the receiver acks each one, so >8s with NOTHING inbound
            // means the link is dead even though TCP has not errored (e.g.
            // the peer lost power mid-session). Breaking lets the supervisor
            // reconnect with backoff; the seamless engine then resumes
            // automatically through the refreshed sender slot.
            let mut last_inbound = Instant::now();
            const INBOUND_STALE: Duration = Duration::from_secs(8);

            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                // Any inbound traffic proves the peer is alive.
                                last_inbound = Instant::now();
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
                                    Ok(WireMessage::ScreenInfo { name, virtual_width, virtual_height, monitors }) => {
                                        // Record the peer's real screen geometry so the
                                        // router can size this remote accurately (B1.7)
                                        // and the seamless engine can clamp onto its
                                        // real monitors (L-shaped layouts).
                                        if let Ok(mut sizes) = screen_sizes.lock() {
                                            sizes.insert(name.clone(), PeerScreen {
                                                width: virtual_width,
                                                height: virtual_height,
                                                monitors: monitors
                                                    .iter()
                                                    .map(|m| (m[0], m[1], m[2], m[3]))
                                                    .collect(),
                                            });
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
                                    Ok(WireMessage::InputInjectionFailed { kind, detail }) => {
                                        // Surface receiver-side injection failures
                                        // (typically UIPI: an elevated window has
                                        // focus on the peer) so input "going dead"
                                        // is explained instead of silent.
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "Peer could not inject {kind}: {detail} (an elevated window may have focus on the peer)."
                                            );
                                        });
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
                        if last_inbound.elapsed() >= INBOUND_STALE {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.connected = false;
                                snapshot.last_event =
                                    "Peer unresponsive (>8s without HeartbeatAck): reconnecting."
                                        .to_string();
                            });
                            break;
                        }

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
    sizes.get(&name).map(|peer| (peer.width, peer.height))
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
            tailnet::get_tailscale_status,
            get_windows_monitor_topology,
            get_peer_screen_size,
            get_keyboard_layout,
            get_tcp_session_state,
            install_firewall_rule,
            emergency_reset,
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
            router::start_multi_screen_router,
            router::reconfigure_router,
            router::stop_multi_screen_router,
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
            let reset_i = MenuItem::with_id(
                app,
                "emergency_reset",
                "Emergency reset (release all input)",
                true,
                None::<&str>,
            )?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;

            let menu = Menu::with_items(app, &[&show_i, &pause_i, &reset_i, &quit_i])?;

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
                    "emergency_reset" => {
                        // Strongest tray recovery: also frees the cursor clip
                        // and aborts an inbound (being-controlled) session.
                        let app_state = app.state::<AppState>();
                        emergency_reset_all(&app_state);
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
