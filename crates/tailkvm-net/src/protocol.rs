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
    KeyboardText {
        text: String,
    },
    KeyboardKey {
        vk: u16,
        scan_code: u16,
        down: bool,
        extended: bool,
    },
    KeyboardLayout {
        language_id: u16,
        keyboard_type: i32,
        is_jis_keyboard: bool,
        is_japanese_locale: bool,
        label: String,
    },
    ClipboardText {
        text: String,
    },
    ScreenInfo {
        name: String,
        virtual_width: i32,
        virtual_height: i32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Encode a message to a wire line, then decode it back, asserting the
    /// round-trip preserves every field and honors the line framing contract.
    ///
    /// `WireMessage` does not derive `PartialEq`, so equality is checked via
    /// canonical JSON (`serde_json::to_value`) rather than direct comparison.
    fn assert_roundtrip(message: WireMessage) {
        let bytes = encode_line(&message).expect("encode_line should succeed");

        // Framing contract: exactly one trailing '\n' and no embedded newlines,
        // because the receiver splits the stream on line boundaries.
        assert_eq!(
            *bytes.last().expect("encoded line must not be empty"),
            b'\n',
            "wire line must end with a newline: {message:?}"
        );
        let text = String::from_utf8(bytes).expect("wire line must be valid UTF-8");
        assert_eq!(
            text.matches('\n').count(),
            1,
            "wire line must contain exactly one newline: {message:?}"
        );

        // The receiver reads with `lines()`, which strips the trailing newline,
        // so decoding must succeed on the newline-stripped content.
        let line = text.trim_end_matches('\n');
        let decoded = decode_line(line).expect("decode_line should succeed");

        let original_json = serde_json::to_value(&message).expect("serialize original");
        let decoded_json = serde_json::to_value(&decoded).expect("serialize decoded");
        assert_eq!(
            original_json, decoded_json,
            "round-trip must preserve all fields: {message:?}"
        );
    }

    #[test]
    fn roundtrip_all_variants() {
        let messages = vec![
            WireMessage::Hello {
                machine_name: "alice-pc".to_string(),
                app_version: "0.1.0".to_string(),
            },
            WireMessage::HelloAck {
                receiver_machine_name: "bob-note".to_string(),
                accepted: true,
                message: "accepted".to_string(),
            },
            WireMessage::Heartbeat {
                seq: 42,
                unix_ms: 1_700_000_000_000,
            },
            WireMessage::HeartbeatAck {
                seq: 42,
                unix_ms: 1_700_000_000_001,
            },
            WireMessage::MouseSetPosition { x: -1920, y: 1080 },
            WireMessage::MousePosition { x: 0, y: 0 },
            WireMessage::MouseMove { dx: -5, dy: 7 },
            WireMessage::MouseButton {
                button: "left".to_string(),
                down: true,
            },
            WireMessage::MouseWheel {
                delta: -120,
                horizontal: false,
            },
            WireMessage::KeyboardText {
                // Includes an astral-plane emoji (surrogate pair) and JIS text.
                text: "abc123 日本語 😀".to_string(),
            },
            WireMessage::KeyboardKey {
                vk: 0x41,
                scan_code: 0x1E,
                down: true,
                extended: false,
            },
            WireMessage::KeyboardLayout {
                language_id: 0x0411,
                keyboard_type: 7,
                is_jis_keyboard: true,
                is_japanese_locale: true,
                label: "locale=0x0411 (Japanese), keyboard_type=7 (JIS)".to_string(),
            },
            WireMessage::ClipboardText {
                text: "copied text 日本語 🚀".to_string(),
            },
            WireMessage::ScreenInfo {
                name: "bob-note".to_string(),
                virtual_width: 3840,
                virtual_height: 1080,
            },
            WireMessage::Disconnect {
                reason: "user requested".to_string(),
            },
        ];

        for message in messages {
            assert_roundtrip(message);
        }
    }

    #[test]
    fn mouse_move_uses_snake_case_tag_and_fields() {
        let bytes = encode_line(&WireMessage::MouseMove { dx: 3, dy: -4 }).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "mouse_move");
        assert_eq!(value["dx"], 3);
        assert_eq!(value["dy"], -4);
    }

    #[test]
    fn keyboard_key_uses_snake_case_tag_and_fields() {
        let bytes = encode_line(&WireMessage::KeyboardKey {
            vk: 0x0D,
            scan_code: 0,
            down: false,
            extended: true,
        })
        .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "keyboard_key");
        assert_eq!(value["vk"], 0x0D);
        assert_eq!(value["scan_code"], 0);
        assert_eq!(value["down"], false);
        assert_eq!(value["extended"], true);
    }

    #[test]
    fn keyboard_text_preserves_surrogate_pairs() {
        // Receiver injection re-encodes text to UTF-16 units, so the wire layer
        // must carry astral-plane characters intact.
        let original = "🚀あ";
        let bytes = encode_line(&WireMessage::KeyboardText {
            text: original.to_string(),
        })
        .unwrap();
        let line = String::from_utf8(bytes).unwrap();
        let decoded = decode_line(line.trim_end()).unwrap();
        match decoded {
            WireMessage::KeyboardText { text } => assert_eq!(text, original),
            other => panic!("expected KeyboardText, got {other:?}"),
        }
    }

    #[test]
    fn decode_line_rejects_invalid_json() {
        assert!(decode_line("not json at all").is_err());
        assert!(decode_line("{\"type\": ").is_err());
        assert!(decode_line("").is_err());
    }

    #[test]
    fn decode_line_rejects_unknown_message_type() {
        // An unknown tag value must not silently deserialize into a known variant.
        assert!(decode_line("{\"type\":\"teleport\",\"x\":1,\"y\":2}").is_err());
    }
}
