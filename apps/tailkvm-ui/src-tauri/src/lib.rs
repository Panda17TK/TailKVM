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
mod legacy_capture;
mod router;
mod seamless;
mod session;
mod state;
mod tailnet;

use forwarding::*;
use legacy_capture::*;
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
            legacy_capture::emergency_reset,
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
            session::set_auth_token,
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
            legacy_capture::start_mouse_capture,
            legacy_capture::stop_mouse_capture,
            legacy_capture::start_raw_mouse_diagnostic,
            legacy_capture::stop_raw_mouse_diagnostic,
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
