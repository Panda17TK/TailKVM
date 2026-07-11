use serde::{Deserialize, Serialize};

/// Wire protocol version advertised in `Hello`/`HelloAck`. Bump on any breaking
/// change to the message schema so peers can detect a mismatch instead of
/// relying solely on per-field `#[serde(default)]` compatibility. Version `0` is
/// reserved for peers that predate the field (they omit it → `serde` default),
/// and is treated as "unversioned, assume compatible" by [`protocol_compatible`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Whether a peer advertising `peer_version` is compatible with this build.
/// `0` (an older peer that never sent the field) is accepted for back-compat;
/// otherwise the major version — here the whole number, since we are pre-1.0 in
/// spirit — must match exactly. Kept intentionally simple; extend when the
/// schema grows a real major/minor split.
pub fn protocol_compatible(peer_version: u32) -> bool {
    peer_version == 0 || peer_version == PROTOCOL_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Hello {
        machine_name: String,
        app_version: String,
        /// Optional shared-secret pairing token (H1). `#[serde(default)]` keeps
        /// the wire backward-compatible: a peer that predates this field sends
        /// `None`, and a receiver with no token configured does not require it.
        #[serde(default)]
        auth_token: Option<String>,
        /// Wire protocol version (see [`PROTOCOL_VERSION`]). `default` = 0 for
        /// peers that predate the field, treated as "unversioned/compatible".
        #[serde(default)]
        protocol_version: u32,
    },
    HelloAck {
        receiver_machine_name: String,
        accepted: bool,
        message: String,
        /// The receiver's wire protocol version, so the controller can detect a
        /// mismatch. `default` = 0 keeps decoding compatible with older peers.
        #[serde(default)]
        protocol_version: u32,
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
    /// Clipboard image as raw `CF_DIB` bytes, base64-encoded (#9 phase 1).
    /// Windows↔Windows round-trips DIB losslessly with no transcoding; the
    /// sender caps the raw size. Peers that predate this variant fail to
    /// decode the line and skip it (sessions tolerate unknown lines).
    ClipboardImage {
        dib_base64: String,
    },
    ScreenInfo {
        name: String,
        virtual_width: i32,
        virtual_height: i32,
        /// Monitor rects `[left, top, right, bottom]` relative to the sender's
        /// virtual-screen origin — the same space as `MouseSetPosition`
        /// offsets. Lets the controller clamp its logical cursor onto real
        /// monitors in L-shaped layouts instead of wandering through dead
        /// zones of the bounding box. `default` keeps decoding compatible
        /// with peers that predate this field.
        #[serde(default)]
        monitors: Vec<[i32; 4]>,
    },
    /// Receiver-side input injection failed (e.g. UIPI: an elevated window has
    /// focus on the receiver, so `SendInput` is blocked). Sent throttled so the
    /// controller can surface why input silently stopped working.
    InputInjectionFailed {
        kind: String,
        detail: String,
    },
    Disconnect {
        reason: String,
    },
}

/// Typed wire framing error. Implements `From<WireError> for String`, so the
/// many session call sites that propagate `?` into a `Result<_, String>` keep
/// compiling unchanged while the error is precise at the source.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("failed to encode wire message: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("failed to decode wire message: {0}")]
    Decode(#[source] serde_json::Error),
}

impl From<WireError> for String {
    fn from(err: WireError) -> Self {
        err.to_string()
    }
}

pub fn encode_line(message: &WireMessage) -> Result<Vec<u8>, WireError> {
    let mut line = serde_json::to_string(message).map_err(WireError::Encode)?;
    line.push('\n');
    Ok(line.into_bytes())
}

pub fn decode_line(line: &str) -> Result<WireMessage, WireError> {
    serde_json::from_str(line).map_err(WireError::Decode)
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
                auth_token: Some("shared-secret".to_string()),
                protocol_version: PROTOCOL_VERSION,
            },
            WireMessage::HelloAck {
                receiver_machine_name: "peer-pc".to_string(),
                accepted: true,
                message: "accepted".to_string(),
                protocol_version: PROTOCOL_VERSION,
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
            WireMessage::ClipboardImage {
                dib_base64: "Qk06AAAAAAAAADYAAAAoAAAAAQAAAAEAAAABABgAAAAAAAQAAAA".to_string(),
            },
            WireMessage::ScreenInfo {
                name: "peer-pc".to_string(),
                virtual_width: 3840,
                virtual_height: 1080,
                monitors: vec![[0, 0, 1920, 1080], [1920, 0, 3840, 1080]],
            },
            WireMessage::InputInjectionFailed {
                kind: "keyboard_key".to_string(),
                detail: "SendInput failed.".to_string(),
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
    fn hello_without_auth_token_decodes_as_none() {
        // A peer that predates the pairing token (H1) sends a Hello with no
        // `auth_token` field; it must still decode, defaulting the token to None.
        let decoded =
            decode_line(r#"{"type":"hello","machine_name":"old-pc","app_version":"0.1.0"}"#)
                .expect("legacy hello should decode");
        match decoded {
            WireMessage::Hello {
                auth_token,
                protocol_version,
                ..
            } => {
                assert_eq!(auth_token, None);
                // A peer predating the version field decodes as 0 (unversioned).
                assert_eq!(protocol_version, 0);
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn protocol_compatible_accepts_unversioned_and_exact_match() {
        assert!(protocol_compatible(0), "unversioned peer is accepted");
        assert!(
            protocol_compatible(PROTOCOL_VERSION),
            "exact match accepted"
        );
        assert!(
            !protocol_compatible(PROTOCOL_VERSION + 1),
            "a future/newer version is flagged as incompatible"
        );
    }

    #[test]
    fn decode_line_rejects_unknown_message_type() {
        // An unknown tag value must not silently deserialize into a known variant.
        assert!(decode_line("{\"type\":\"teleport\",\"x\":1,\"y\":2}").is_err());
    }
}
