//! Single-machine loopback integration test for the wire transport.
//!
//! Connects a controller (writer) and receiver (reader) over `127.0.0.1` and
//! verifies that every `WireMessage` written with `encode_line` is read back
//! intact by the same newline-framed `BufReader::lines()` decoder the real
//! receiver uses. This exercises the actual TCP transport + framing on one
//! machine, with no input injection, so it is safe to run anywhere (incl. CI).

use tailkvm_net::protocol::{decode_line, encode_line, WireMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn sample_messages() -> Vec<WireMessage> {
    vec![
        WireMessage::Hello {
            machine_name: "alice-pc".to_string(),
            app_version: "0.1.0".to_string(),
        },
        WireMessage::HelloAck {
            receiver_machine_name: "peer-pc".to_string(),
            accepted: true,
            message: "accepted".to_string(),
        },
        WireMessage::MouseSetPosition { x: -1920, y: 540 },
        WireMessage::MouseMove { dx: 7, dy: -3 },
        WireMessage::MouseButton {
            button: "left".to_string(),
            down: true,
        },
        WireMessage::MouseButton {
            button: "left".to_string(),
            down: false,
        },
        WireMessage::MouseWheel {
            delta: -120,
            horizontal: false,
        },
        WireMessage::KeyboardKey {
            vk: 0x41,
            scan_code: 0x1E,
            down: true,
            extended: false,
        },
        WireMessage::KeyboardText {
            text: "abc123 日本語 🚀".to_string(),
        },
        WireMessage::ClipboardText {
            text: "copied 日本語".to_string(),
        },
        WireMessage::KeyboardLayout {
            language_id: 0x0411,
            keyboard_type: 7,
            is_jis_keyboard: true,
            is_japanese_locale: true,
            label: "locale=0x0411 (Japanese), keyboard_type=7 (JIS)".to_string(),
        },
        WireMessage::Heartbeat {
            seq: 1,
            unix_ms: 1_700_000_000_000,
        },
        WireMessage::Disconnect {
            reason: "bye".to_string(),
        },
    ]
}

fn as_json(messages: &[WireMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| serde_json::to_value(m).expect("serialize"))
        .collect()
}

/// Each message written individually is read back in order, intact.
#[tokio::test]
async fn loopback_preserves_all_messages_written_individually() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let messages = sample_messages();
    let expected = as_json(&messages);
    let count = messages.len();

    let server = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut received = Vec::with_capacity(count);
        while received.len() < count {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("stream closed before all messages arrived");
            received.push(decode_line(&line).expect("decode_line"));
        }
        received
    });

    let mut client = TcpStream::connect(addr).await.unwrap();
    for message in &messages {
        client
            .write_all(&encode_line(message).unwrap())
            .await
            .unwrap();
    }
    client.flush().await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(as_json(&received), expected);
}

/// All messages concatenated into a single TCP write are still split correctly
/// by the line framing (guards against newline-framing regressions under
/// packet coalescing).
#[tokio::test]
async fn loopback_framing_survives_coalesced_write() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let messages = sample_messages();
    let expected = as_json(&messages);
    let count = messages.len();

    let server = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut received = Vec::with_capacity(count);
        while received.len() < count {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("stream closed early");
            received.push(decode_line(&line).expect("decode_line"));
        }
        received
    });

    let mut buffer = Vec::new();
    for message in &messages {
        buffer.extend_from_slice(&encode_line(message).unwrap());
    }

    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&buffer).await.unwrap();
    client.flush().await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(as_json(&received), expected);
}
