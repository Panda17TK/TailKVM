use serde::Serialize;
use serde_json::Value;
use std::process::Command;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
};

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

#[tauri::command]
fn get_app_status() -> String {
    "TailKVM tray app is running. Task 1 OK.".to_string()
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
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_app_status,
            get_tailscale_status
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
                    "show" => {
                        show_main_window(app);
                    }
                    "pause" => {
                        println!("TailKVM pause requested. Input engine is not implemented yet.");
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {
                        println!("unhandled tray menu event: {:?}", event.id);
                    }
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
