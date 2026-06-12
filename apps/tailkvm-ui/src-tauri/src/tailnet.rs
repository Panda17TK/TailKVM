//! Tailscale (tailnet) integration: peer discovery via the local
//! `tailscale status --json` CLI.
//!
//! Second slice of the lib.rs decomposition: fully self-contained — no shared
//! app state, just CLI invocation and JSON shaping for the UI peer list.

use serde::Serialize;
use serde_json::Value;
use std::process::Command;

#[derive(Debug, Serialize)]
pub(crate) struct TailnetStatus {
    pub(crate) backend_state: String,
    pub(crate) self_node: Option<TailnetNode>,
    pub(crate) peers: Vec<TailnetNode>,
    pub(crate) raw_peer_count: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct TailnetNode {
    pub(crate) id: String,
    pub(crate) host_name: String,
    pub(crate) dns_name: Option<String>,
    pub(crate) os: Option<String>,
    pub(crate) online: bool,
    pub(crate) active: Option<bool>,
    pub(crate) tailscale_ips: Vec<String>,
    pub(crate) user: Option<String>,
    pub(crate) relay: Option<String>,
    pub(crate) cur_addr: Option<String>,
    pub(crate) last_seen: Option<String>,
    pub(crate) tx_bytes: Option<u64>,
    pub(crate) rx_bytes: Option<u64>,
}

#[tauri::command]
pub(crate) fn get_tailscale_status() -> Result<TailnetStatus, String> {
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
        let mut command = Command::new(&exe);
        command.args(["status", "--json"]);

        // This app runs in the windows GUI subsystem: without this flag every
        // status poll spawns a console window that flashes on screen.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        match command.output() {
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
