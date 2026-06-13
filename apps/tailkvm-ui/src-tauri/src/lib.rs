use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tailkvm_net::protocol::WireMessage;
use tailkvm_win32::monitor::MonitorTopology;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, State, WindowEvent,
};
use tokio::{
    net::TcpStream,
    time::{self, Duration},
};

mod clipboard_sync;
mod forwarding;
mod ime_mode;
mod router;
mod seamless;
mod session;
mod state;
mod tailnet;

use forwarding::*;
use seamless::*;
use session::*;
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

/// Update the Japanese-IME settings (candidate position mode, IME open /
/// conversion policies, focus-failure behavior). Persisted by the frontend
/// under `tailkvm.imeSettings.v1` and pushed here on load and on change
/// (IME-CONF-001); read live by the keyboard forwarding loop at every
/// composition-mode entry.
#[tauri::command]
async fn set_ime_settings(
    settings: ImeSettings,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    {
        let mut guard = state
            .ime_settings
            .lock()
            .map_err(|_| "ime settings mutex poisoned".to_string())?;
        *guard = settings;
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "IME settings updated.".to_string();
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
    let ime_settings = state.ime_settings.clone();
    let ime_anchor = state.ime_anchor.clone();

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
            ime_settings: state.ime_settings.clone(),
            ime_anchor: state.ime_anchor.clone(),
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
                    "[Compatibility fallback] Remote mode armed. Move cursor to {} edge. remote={}x{}, gain={gain:.2}, interval={}ms, max_delta={}, margin={}px.",
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
                        ime_settings: ime_settings.clone(),
                        ime_anchor: ime_anchor.clone(),
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
            set_ime_settings,
            tailnet::get_tailscale_status,
            get_windows_monitor_topology,
            get_peer_screen_size,
            get_keyboard_layout,
            get_tcp_session_state,
            install_firewall_rule,
            emergency_reset,
            send_test_keyboard_text,
            clipboard_sync::send_clipboard_text,
            clipboard_sync::send_clipboard_image,
            clipboard_sync::set_clipboard_sync,
            send_test_key_tap,
            start_keyboard_hook_capture,
            stop_keyboard_hook_capture,
            send_test_mouse_double_click,
            send_test_mouse_click,
            start_mouse_hook_capture,
            stop_mouse_hook_capture,
            session::start_tcp_receiver,
            session::connect_tcp_peer,
            session::disconnect_tcp_peer,
            session::set_accept_incoming,
            discover_tailkvm_peers,
            session::connect_screen,
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
