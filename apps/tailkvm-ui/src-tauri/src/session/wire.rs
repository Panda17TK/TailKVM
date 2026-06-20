//! Wire framing, the line-length cap (H3), the pairing-token check (H1), the
//! heartbeat/position/layout senders, and the MouseSetPosition coalescing rule.
//! L2 split of session.rs. Everything is `pub(crate)`: internal plumbing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailkvm_net::protocol::{encode_line, WireMessage};
use tailkvm_win32::monitor::MonitorTopology;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::state::{local_machine_name, update_tcp_state, TcpSessionSnapshot};

/// Upper bound on a single decoded wire line (H3). A legitimate message is
/// small; the largest is a base64 `CF_DIB` clipboard image — raw cap
/// [`tailkvm_win32::clipboard::MAX_CLIPBOARD_IMAGE_BYTES`] (≈ 8 MiB) expands to
/// ~10.7 MiB of base64 plus JSON framing. Bounding the line length stops a peer
/// from exhausting memory by streaming a line that never terminates:
/// `AsyncBufReadExt::lines()` buffers a line without any limit.
pub(crate) const MAX_WIRE_LINE_BYTES: usize = 12 * 1024 * 1024;

fn wire_line_too_long(max: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("wire line exceeded {max} bytes"),
    )
}

/// Read one `\n`-terminated line, failing with `InvalidData` once the
/// accumulated bytes exceed `max` instead of buffering an unbounded line (H3).
///
/// Mirrors `AsyncBufReadExt::next_line`: returns `Ok(None)` at clean EOF and
/// strips the trailing `\n` (and a preceding `\r`). `buf` is an external
/// accumulator owned by the caller so a partially-read line survives the
/// `select!` read branch being cancelled (the only `.await` is `fill_buf`, which
/// is cancel-safe, and bytes are appended only after they are consumed).
pub(crate) async fn read_capped_line<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // Clean EOF with nothing buffered ends the stream; a buffered
            // remainder is surfaced as a final unterminated line.
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let over = buf.len() + pos > max;
                if !over {
                    buf.extend_from_slice(&chunk[..pos]);
                }
                reader.consume(pos + 1);
                if over {
                    buf.clear();
                    return Err(wire_line_too_long(max));
                }
                break;
            }
            None => {
                let len = chunk.len();
                let over = buf.len() + len > max;
                if !over {
                    buf.extend_from_slice(chunk);
                }
                reader.consume(len);
                if over {
                    buf.clear();
                    return Err(wire_line_too_long(max));
                }
            }
        }
    }
    let mut line = std::mem::take(buf);
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Whether a Hello clears the pairing-token check (H1). With no token
/// configured (`required == None`) any Hello is accepted (tailnet trust only);
/// with a token configured the controller must present exactly that token.
pub(crate) fn hello_authorized(required: Option<&str>, presented: Option<&str>) -> bool {
    match required {
        None => true,
        Some(secret) => presented == Some(secret),
    }
}

/// Build this machine's `ScreenInfo`: virtual-screen size plus monitor rects
/// relative to the virtual origin (the coordinate space of `MouseSetPosition`
/// offsets), so the controller can clamp onto real monitors.
pub(crate) fn local_screen_info(topology: &MonitorTopology) -> WireMessage {
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
pub(crate) async fn notify_injection_failure<W: AsyncWrite + Unpin>(
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

pub(crate) async fn send_current_mouse_position<W>(writer: &mut W) -> Result<(), String>
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

pub(crate) async fn send_local_keyboard_layout<W>(writer: &mut W) -> Result<(), String>
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

pub(crate) fn apply_peer_keyboard_layout(
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

/// Append `next` to a pending write batch, collapsing a run of absolute
/// `MouseSetPosition` messages to the newest. Absolute positions supersede one
/// another, so a queued-but-stale position is pure latency; every other message
/// — including relative `MouseMove`, whose deltas must never be dropped — is
/// preserved in arrival order. Extracted from the controller writer loop so the
/// coalescing rule is unit-testable.
pub(crate) fn push_coalesced(batch: &mut Vec<WireMessage>, next: WireMessage) {
    if matches!(next, WireMessage::MouseSetPosition { .. })
        && matches!(batch.last(), Some(WireMessage::MouseSetPosition { .. }))
    {
        if let Some(slot) = batch.last_mut() {
            *slot = next;
        }
    } else {
        batch.push(next);
    }
}

pub(crate) async fn write_wire<W>(writer: &mut W, message: &WireMessage) -> Result<(), String>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_capped_line_reads_lines_then_eof() {
        let data = b"hello\nworld\n";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024)
                .await
                .unwrap()
                .as_deref(),
            Some("hello")
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024)
                .await
                .unwrap()
                .as_deref(),
            Some("world")
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn read_capped_line_strips_trailing_cr() {
        let data = b"hello\r\n";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024)
                .await
                .unwrap()
                .as_deref(),
            Some("hello")
        );
    }

    #[tokio::test]
    async fn read_capped_line_returns_final_unterminated_line() {
        let data = b"partial";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024)
                .await
                .unwrap()
                .as_deref(),
            Some("partial")
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 1024).await.unwrap(),
            None
        );
    }

    #[test]
    fn hello_authorized_is_open_when_no_token_required() {
        // No configured token => any Hello clears the check (tailnet trust).
        assert!(hello_authorized(None, None));
        assert!(hello_authorized(None, Some("anything")));
    }

    #[test]
    fn hello_authorized_requires_exact_match_when_token_set() {
        assert!(hello_authorized(Some("secret"), Some("secret")));
        assert!(!hello_authorized(Some("secret"), Some("wrong")));
        assert!(!hello_authorized(Some("secret"), None));
    }

    #[tokio::test]
    async fn read_capped_line_rejects_oversized_line() {
        let data = [b'a'; 100];
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let err = read_capped_line(&mut reader, &mut buf, 10)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Reproduce the controller writer's drain: seed a batch with the first
    /// message, then fold the rest through `push_coalesced`.
    fn coalesce_all(messages: Vec<WireMessage>) -> Vec<WireMessage> {
        let mut iter = messages.into_iter();
        let mut batch = match iter.next() {
            Some(first) => vec![first],
            None => return Vec::new(),
        };
        for next in iter {
            push_coalesced(&mut batch, next);
        }
        batch
    }

    #[test]
    fn collapses_consecutive_set_positions_to_newest() {
        let out = coalesce_all(vec![
            WireMessage::MouseSetPosition { x: 1, y: 1 },
            WireMessage::MouseSetPosition { x: 2, y: 2 },
            WireMessage::MouseSetPosition { x: 3, y: 3 },
        ]);
        assert_eq!(out.len(), 1, "a run of positions collapses to one");
        match out[0] {
            WireMessage::MouseSetPosition { x, y } => assert_eq!((x, y), (3, 3)),
            ref other => panic!("expected MouseSetPosition, got {other:?}"),
        }
    }

    #[test]
    fn preserves_order_and_keeps_latest_position_in_each_run() {
        // setpos, setpos, <other>, setpos, setpos  =>  setpos(latest), <other>, setpos(latest)
        let out = coalesce_all(vec![
            WireMessage::MouseSetPosition { x: 1, y: 1 },
            WireMessage::MouseSetPosition { x: 2, y: 2 },
            WireMessage::Heartbeat { seq: 7, unix_ms: 0 },
            WireMessage::MouseSetPosition { x: 5, y: 5 },
            WireMessage::MouseSetPosition { x: 6, y: 6 },
        ]);
        assert_eq!(out.len(), 3);
        match out[0] {
            WireMessage::MouseSetPosition { x, y } => assert_eq!((x, y), (2, 2)),
            ref other => panic!("expected MouseSetPosition, got {other:?}"),
        }
        assert!(
            matches!(out[1], WireMessage::Heartbeat { seq: 7, .. }),
            "a non-position message must not be collapsed or reordered"
        );
        match out[2] {
            WireMessage::MouseSetPosition { x, y } => assert_eq!((x, y), (6, 6)),
            ref other => panic!("expected MouseSetPosition, got {other:?}"),
        }
    }

    #[test]
    fn never_drops_relative_mouse_moves() {
        // Relative deltas must all survive — collapsing them would lose motion.
        let out = coalesce_all(vec![
            WireMessage::MouseMove { dx: 1, dy: 0 },
            WireMessage::MouseMove { dx: 2, dy: 0 },
            WireMessage::MouseMove { dx: 3, dy: 0 },
        ]);
        assert_eq!(out.len(), 3);
        let total: i32 = out
            .iter()
            .map(|m| match m {
                WireMessage::MouseMove { dx, .. } => *dx,
                _ => 0,
            })
            .sum();
        assert_eq!(total, 6, "no relative motion is lost");
    }

    #[test]
    fn position_after_other_message_is_not_merged_backwards() {
        // A position separated from an earlier position by another message
        // starts a fresh run (no merge across the boundary).
        let out = coalesce_all(vec![
            WireMessage::MouseSetPosition { x: 1, y: 1 },
            WireMessage::MouseMove { dx: 9, dy: 9 },
            WireMessage::MouseSetPosition { x: 4, y: 4 },
        ]);
        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[0],
            WireMessage::MouseSetPosition { x: 1, y: 1 }
        ));
        assert!(matches!(out[1], WireMessage::MouseMove { dx: 9, dy: 9 }));
        assert!(matches!(
            out[2],
            WireMessage::MouseSetPosition { x: 4, y: 4 }
        ));
    }
}
