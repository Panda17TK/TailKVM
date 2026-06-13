//! Headless "fake receiver" for single-desktop verification.
//!
//! Listens on the TailKVM port and prints every decoded `WireMessage` instead
//! of injecting input. Point a real TailKVM controller at this machine and you
//! can confirm the capture → forward path end-to-end — edge crossing,
//! mouse/keyboard/clipboard forwarding — *without* a second desktop doing
//! `SendInput` (which on one machine would fight the controller's own hooks).
//!
//! This covers the controller-side half of issue #24 that does not need two
//! real desktops; actual input injection / IME / cursor confine still require
//! two machines (or two VMs).
//!
//! Run:
//!   cargo run -p tailkvm-net --example fake_receiver -- [PORT]
//! then connect a controller to this host's IP (loopback, LAN, or tailnet).

use std::time::{SystemTime, UNIX_EPOCH};
use tailkvm_net::protocol::{decode_line, encode_line, WireMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const DEFAULT_PORT: u16 = 47110;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("fake_receiver: failed to bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    println!("fake_receiver: listening on {addr} (Ctrl+C to stop)");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("fake_receiver: accept failed: {err}");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        println!("fake_receiver: controller connected from {peer}");
        // One session at a time is enough for manual verification.
        handle_session(stream).await;
        println!("fake_receiver: controller {peer} disconnected");
    }
}

/// Read newline-framed `WireMessage`s and log them. Replies to `Hello` and
/// `Heartbeat` so the controller's handshake and heartbeat watchdog stay
/// satisfied and it keeps forwarding instead of tearing the session down.
async fn handle_session(stream: TcpStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut counts: std::collections::HashMap<&'static str, u64> = std::collections::HashMap::new();

    while let Ok(Some(line)) = lines.next_line().await {
        let message = match decode_line(&line) {
            Ok(message) => message,
            Err(err) => {
                eprintln!("fake_receiver: undecodable line ({err}): {line}");
                continue;
            }
        };

        let kind = log_message(&message);
        *counts.entry(kind).or_insert(0) += 1;

        // Keep the controller happy so it does not drop the session.
        let reply = match &message {
            WireMessage::Hello { .. } => Some(WireMessage::HelloAck {
                receiver_machine_name: "fake-receiver".to_string(),
                accepted: true,
                message: "fake_receiver: accepted (no injection)".to_string(),
            }),
            WireMessage::Heartbeat { seq, .. } => Some(WireMessage::HeartbeatAck {
                seq: *seq,
                unix_ms: now_unix_ms(),
            }),
            _ => None,
        };
        if let Some(reply) = reply {
            if let Ok(bytes) = encode_line(&reply) {
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        }
    }

    let mut summary: Vec<_> = counts.into_iter().collect();
    summary.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    println!("fake_receiver: session summary {summary:?}");
}

/// Print a one-line summary of a received message and return its kind label.
fn log_message(message: &WireMessage) -> &'static str {
    match message {
        WireMessage::Hello { machine_name, .. } => {
            println!("  Hello from {machine_name}");
            "Hello"
        }
        WireMessage::MouseSetPosition { x, y } => {
            println!("  MouseSetPosition x={x} y={y}");
            "MouseSetPosition"
        }
        WireMessage::MouseMove { dx, dy } => {
            println!("  MouseMove dx={dx} dy={dy}");
            "MouseMove"
        }
        WireMessage::MouseButton { button, down } => {
            println!("  MouseButton {button} down={down}");
            "MouseButton"
        }
        WireMessage::MouseWheel { delta, horizontal } => {
            println!("  MouseWheel delta={delta} horizontal={horizontal}");
            "MouseWheel"
        }
        WireMessage::KeyboardText { text } => {
            println!("  KeyboardText {:?}", text);
            "KeyboardText"
        }
        WireMessage::KeyboardKey {
            vk, down, extended, ..
        } => {
            println!("  KeyboardKey vk=0x{vk:02X} down={down} extended={extended}");
            "KeyboardKey"
        }
        WireMessage::ClipboardText { text } => {
            println!("  ClipboardText ({} chars)", text.chars().count());
            "ClipboardText"
        }
        WireMessage::ClipboardImage { dib_base64 } => {
            println!("  ClipboardImage ({} base64 bytes)", dib_base64.len());
            "ClipboardImage"
        }
        WireMessage::Heartbeat { seq, .. } => {
            println!("  Heartbeat seq={seq}");
            "Heartbeat"
        }
        WireMessage::ScreenInfo {
            name,
            virtual_width,
            virtual_height,
            ..
        } => {
            println!("  ScreenInfo {name} {virtual_width}x{virtual_height}");
            "ScreenInfo"
        }
        other => {
            println!("  {other:?}");
            "Other"
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
