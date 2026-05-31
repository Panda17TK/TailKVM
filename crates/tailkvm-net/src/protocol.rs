use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Hello {
        machine_name: String,
        app_version: String,
    },
    HelloAck {
        receiver_machine_name: String,
        accepted: bool,
        message: String,
    },
    Heartbeat {
        seq: u64,
        unix_ms: u64,
    },
    HeartbeatAck {
        seq: u64,
        unix_ms: u64,
    },
    MouseSetPosition {
        x: i32,
        y: i32,
    },
    MousePosition {
        x: i32,
        y: i32,
    },
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: String,
        down: bool,
    },
    MouseWheel {
        delta: i32,
        horizontal: bool,
    },
    Disconnect {
        reason: String,
    },
}

pub fn encode_line(message: &WireMessage) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_string(message)
        .map_err(|e| format!("failed to encode wire message: {e}"))?;
    line.push('\n');
    Ok(line.into_bytes())
}

pub fn decode_line(line: &str) -> Result<WireMessage, String> {
    serde_json::from_str(line).map_err(|e| format!("failed to decode wire message: {e}"))
}
