//! Controller (sending) session plus the reconnect supervisor. L2 split of
//! session.rs.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Instant;
use tailkvm_net::protocol::{decode_line, WireMessage};
use tokio::{
    io::BufReader,
    net::TcpStream,
    sync::mpsc,
    time::{self, Duration},
};

use super::wire::*;
use crate::clipboard_sync::*;
use crate::seamless::*;
use crate::state::*;

/// Reconnect backoff policy (extracted so the supervisor's retry behavior is
/// unit-testable): start at 1s, double per failed attempt up to 10s, and treat
/// a session that stayed up ≥15s as healthy — its next retry starts over at 1s
/// instead of inheriting a long wait from earlier failures.
const BACKOFF_START_SECS: u64 = 1;
const BACKOFF_MAX_SECS: u64 = 10;
const HEALTHY_SESSION_SECS: u64 = 15;

/// The delay to wait before the next reconnect attempt, given the previous
/// delay and how long the just-ended session survived.
fn next_backoff_secs(previous_secs: u64, session_secs: u64) -> u64 {
    let base = if session_secs >= HEALTHY_SESSION_SECS {
        BACKOFF_START_SECS
    } else {
        previous_secs
    };
    (base * 2).min(BACKOFF_MAX_SECS)
}

/// The delay to *report and sleep* for this round: a healthy session reconnects
/// after the initial delay, not the inherited one.
fn current_backoff_secs(previous_secs: u64, session_secs: u64) -> u64 {
    if session_secs >= HEALTHY_SESSION_SECS {
        BACKOFF_START_SECS
    } else {
        previous_secs
    }
}

/// Run a (re)connecting controller session in the background until `should_run`
/// is cleared. Each attempt rebuilds the command channel and stores its sender
/// into `tx_slot`. Shared by the single 1:1 controller and named multi-screen
/// sessions (roadmap B1.2 / F2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_controller_supervisor(
    addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    screen_sizes: PeerScreenMap,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    tx_slot: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    should_run: Arc<AtomicBool>,
    screen_label: String,
    // H1: shared pairing token sent in this controller's Hello (None = none).
    auth_token: Arc<Mutex<Option<String>>>,
    // For the 1:1 controller: (shared counter, this supervisor's generation).
    // The loop exits if the shared counter moves past our generation, so a newer
    // connect supersedes us. None for named sessions (they dedupe via their own
    // per-session should_run flag).
    generation: Option<(Arc<AtomicU64>, u64)>,
) {
    let is_current = move || {
        generation
            .as_ref()
            .is_none_or(|(counter, my_gen)| counter.load(Ordering::SeqCst) == *my_gen)
    };
    tauri::async_runtime::spawn(async move {
        let mut backoff_secs: u64 = BACKOFF_START_SECS;
        while should_run.load(Ordering::SeqCst) && is_current() {
            let (command_tx, command_rx) = mpsc::unbounded_channel::<WireMessage>();
            if let Ok(mut tx_guard) = tx_slot.lock() {
                *tx_guard = Some(command_tx);
            }

            let session_start = Instant::now();
            run_controller_session(
                addr.clone(),
                tcp_state.clone(),
                command_rx,
                capture_running.clone(),
                remote_control.clone(),
                clipboard_guard.clone(),
                screen_sizes.clone(),
                sessions.clone(),
                screen_label.clone(),
                auth_token.clone(),
            )
            .await;
            let session_secs = session_start.elapsed().as_secs();

            if let Ok(mut tx_guard) = tx_slot.lock() {
                *tx_guard = None;
            }

            if !should_run.load(Ordering::SeqCst) || !is_current() {
                break;
            }

            // Backoff policy lives in current_backoff_secs/next_backoff_secs
            // (pure, unit-tested): a healthy (≥15s) session retries fast at 1s
            // instead of inheriting a long wait from earlier failures.
            let sleep_secs = current_backoff_secs(backoff_secs, session_secs);

            // Preserve WHY the session ended (run_controller_session left the
            // reason in last_event) instead of clobbering it with a generic
            // "reconnecting" note — otherwise the actual cause is invisible.
            let reason = tcp_state
                .lock()
                .map(|s| s.last_event.clone())
                .unwrap_or_default();
            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.connected = false;
                snapshot.last_event = format!(
                    "[{screen_label}] dropped after {session_secs}s ({reason}). Reconnecting in {sleep_secs}s..."
                );
            });

            let mut waited = 0;
            while waited < sleep_secs && should_run.load(Ordering::SeqCst) && is_current() {
                time::sleep(Duration::from_secs(1)).await;
                waited += 1;
            }
            backoff_secs = next_backoff_secs(backoff_secs, session_secs);
        }

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.connected = false;
            snapshot.last_event = format!("[{screen_label}] session ended.");
        });
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_controller_session(
    addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut command_rx: mpsc::UnboundedReceiver<WireMessage>,
    capture_running: Arc<AtomicBool>,
    remote_control: Arc<Mutex<RemoteControlState>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    screen_sizes: PeerScreenMap,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    origin_name: String,
    auth_token: Arc<Mutex<Option<String>>>,
) {
    match TcpStream::connect(&addr).await {
        Ok(stream) => {
            // Disable Nagle so single control messages (mouse moves, key events)
            // go out immediately rather than being batched (KVM latency).
            let _ = stream.set_nodelay(true);

            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = true;
                snapshot.peer_addr = Some(addr.clone());
                snapshot.last_event = format!("TCP connected to {addr}.");
            });

            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            // External accumulator for the capped line reader (H3); see
            // `read_capped_line` for why it lives outside the select! branch.
            let mut line_buf: Vec<u8> = Vec::new();

            let hello = WireMessage::Hello {
                machine_name: local_machine_name(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                // H1: present the configured pairing token (if any) so a
                // token-protected receiver accepts this controller.
                auth_token: auth_token.lock().ok().and_then(|guard| guard.clone()),
                protocol_version: tailkvm_net::protocol::PROTOCOL_VERSION,
            };

            if let Err(err) = write_wire(&mut write_half, &hello).await {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.connected = false;
                    snapshot.last_event = format!("Failed to send Hello: {err}");
                });
                return;
            }

            if let Err(err) = send_local_keyboard_layout(&mut write_half).await {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = format!("Failed to send KeyboardLayout: {err}");
                });
            }

            let mut heartbeat_seq: u64 = 0;
            let mut interval = time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

            // Inbound watchdog (recovery route): heartbeats go out every 2s
            // and the receiver acks each one, so >8s with NOTHING inbound
            // means the link is dead even though TCP has not errored (e.g.
            // the peer lost power mid-session). Breaking lets the supervisor
            // reconnect with backoff; the seamless engine then resumes
            // automatically through the refreshed sender slot.
            let mut last_inbound = Instant::now();
            const INBOUND_STALE: Duration = Duration::from_secs(8);

            loop {
                tokio::select! {
                    line = read_capped_line(&mut reader, &mut line_buf, MAX_WIRE_LINE_BYTES) => {
                        match line {
                            Ok(Some(line)) => {
                                // Any inbound traffic proves the peer is alive.
                                last_inbound = Instant::now();
                                match decode_line(&line) {
                                    Ok(WireMessage::HelloAck { receiver_machine_name, accepted, message, protocol_version: peer_protocol }) => {
                                        let version_note = if tailkvm_net::protocol::protocol_compatible(peer_protocol) {
                                            String::new()
                                        } else {
                                            format!(" WARNING: receiver protocol v{peer_protocol} != local v{} (may misbehave).", tailkvm_net::protocol::PROTOCOL_VERSION)
                                        };
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.peer_name = Some(receiver_machine_name.clone());
                                            snapshot.connected = accepted;
                                            snapshot.last_event = format!("HelloAck from {receiver_machine_name}: {message}{version_note}");
                                        });
                                    }
                                    Ok(WireMessage::KeyboardLayout { language_id, keyboard_type, is_jis_keyboard: _, is_japanese_locale: _, label }) => {
                                        apply_peer_keyboard_layout(&tcp_state, language_id, keyboard_type, &label);
                                    }
                                    Ok(WireMessage::HeartbeatAck { seq, unix_ms: _ }) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.role = "controller".to_string();
                                            snapshot.connected = true;
                                            snapshot.heartbeat_seq = seq;
                                            snapshot.last_heartbeat_ms = Some(now_unix_ms());
                                            snapshot.last_event = format!("HeartbeatAck received. seq={seq}");
                                        });
                                    }
                                    Ok(WireMessage::ScreenInfo { name, virtual_width, virtual_height, monitors }) => {
                                        // Record the peer's real screen geometry so the
                                        // router can size this remote accurately (B1.7)
                                        // and the seamless engine can clamp onto its
                                        // real monitors (L-shaped layouts).
                                        if let Ok(mut sizes) = screen_sizes.lock() {
                                            sizes.insert(name.clone(), PeerScreen {
                                                width: virtual_width,
                                                height: virtual_height,
                                                monitors: monitors
                                                    .iter()
                                                    .map(|m| (m[0], m[1], m[2], m[3]))
                                                    .collect(),
                                            });
                                        }
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "ScreenInfo from {name}: {virtual_width}x{virtual_height}."
                                            );
                                        });
                                    }
                                    Ok(WireMessage::ClipboardText { text }) => {
                                        // Bidirectional clipboard: apply the peer's
                                        // text and mark the guard so our watcher
                                        // does not echo it back.
                                        if let Ok(mut guard) = clipboard_guard.lock() {
                                            guard.mark_applied(&text);
                                        }
                                        let chars = text.chars().count();
                                        let _ = tailkvm_win32::clipboard::set_clipboard_text(&text);
                                        // Hub relay: forward to the other screens so
                                        // all clients stay in sync (B1.5 relay).
                                        let relayed =
                                            relay_clipboard(&sessions, &origin_name, &text);
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "ClipboardText applied (chars={chars}), relayed to {relayed} sibling(s)."
                                            );
                                        });
                                    }
                                    Ok(WireMessage::ClipboardImage { dib_base64 }) => {
                                        // #9 phase 1: apply the peer's image and
                                        // relay it to the other screens (hub).
                                        if let Ok(dib) = decode_dib(&dib_base64) {
                                            if let Ok(mut guard) = clipboard_guard.lock() {
                                                guard.mark_applied_bytes(&dib);
                                            }
                                            let bytes = dib.len();
                                            let _ =
                                                tailkvm_win32::clipboard::set_clipboard_dib(&dib);
                                            let relayed = relay_clipboard_image(
                                                &sessions,
                                                &origin_name,
                                                &dib_base64,
                                            );
                                            update_tcp_state(&tcp_state, |snapshot| {
                                                snapshot.last_event = format!(
                                                    "ClipboardImage applied (bytes={bytes}), relayed to {relayed} sibling(s)."
                                                );
                                            });
                                        }
                                    }
                                    Ok(WireMessage::MousePosition { x, y }) => {
                                        let remote_state = remote_control
                                            .lock()
                                            .map(|state| state.clone())
                                            .unwrap_or_default();

                                        if remote_state.active
                                            && !remote_state.seamless
                                            && is_remote_return_edge(x, y, &remote_state)
                                        {
                                            capture_running.store(false, Ordering::SeqCst);

                                            if let Ok(mut state) = remote_control.lock() {
                                                state.active = false;
                                            }

                                            update_tcp_state(&tcp_state, |snapshot| {
                                                snapshot.role = "controller".to_string();
                                                snapshot.connected = true;
                                                snapshot.last_event = format!(
                                                    "Remote return edge reached at x={}, y={}. Capture stop requested.",
                                                    x, y
                                                );
                                            });
                                        } else {
                                            update_tcp_state(&tcp_state, |snapshot| {
                                                snapshot.role = "controller".to_string();
                                                snapshot.connected = true;
                                                snapshot.last_event = format!(
                                                    "Remote MousePosition x={}, y={}",
                                                    x, y
                                                );
                                            });
                                        }
                                    }
                                    Ok(WireMessage::InputInjectionFailed { kind, detail }) => {
                                        // Surface receiver-side injection failures
                                        // (typically UIPI: an elevated window has
                                        // focus on the peer) so input "going dead"
                                        // is explained instead of silent.
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!(
                                                "Peer could not inject {kind}: {detail} (an elevated window may have focus on the peer)."
                                            );
                                        });
                                    }
                                    Ok(other) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!("Controller ignored message: {other:?}");
                                        });
                                    }
                                    Err(err) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event = format!("Controller decode error: {err}");
                                        });
                                    }
                                }
                            }
                            Ok(None) => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = "Peer closed TCP connection.".to_string();
                                });
                                break;
                            }
                            Err(err) => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = format!("Controller read error: {err}");
                                });
                                break;
                            }
                        }
                    }
                    maybe_outbound = command_rx.recv() => {
                        match maybe_outbound {
                            Some(first) => {
                                // Coalesce before writing. MouseSetPosition is an
                                // absolute coordinate, so when sends back up (slow
                                // link / busy receiver) every position but the
                                // latest is pure latency. Drain whatever is already
                                // queued and collapse consecutive MouseSetPosition
                                // to the newest, preserving all other messages and
                                // their order. This bounds the wire to "latest
                                // position wins" instead of letting the unbounded
                                // channel accumulate stale positions, which showed
                                // up as input/cursor lag that grows over time at
                                // higher capture rates. MouseMove is relative, so
                                // it is never dropped (collapsing would lose
                                // motion) — only absolute positions coalesce.
                                let mut batch = vec![first];
                                while let Ok(next) = command_rx.try_recv() {
                                    push_coalesced(&mut batch, next);
                                }

                                let mut write_failed = false;
                                for outbound in &batch {
                                    if let Err(err) = write_wire(&mut write_half, outbound).await {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.last_event =
                                                format!("Failed to send command message: {err}");
                                        });
                                        write_failed = true;
                                        break;
                                    }
                                }
                                if write_failed {
                                    break;
                                }

                                // Skip the per-event UI update for high-rate mouse
                                // moves/positions: it would allocate + lock and
                                // clobber the capture loop's throttled progress
                                // summary. Report only the latest non-motion
                                // message in the batch, if any.
                                if let Some(outbound) = batch.iter().rev().find(|m| {
                                    !matches!(
                                        m,
                                        WireMessage::MouseMove { .. }
                                            | WireMessage::MouseSetPosition { .. }
                                    )
                                }) {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.role = "controller".to_string();
                                        snapshot.connected = true;
                                        snapshot.last_event =
                                            format!("Sent command message: {outbound:?}");
                                    });
                                }
                            }
                            None => {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event = "Controller command channel closed.".to_string();
                                });
                                break;
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if last_inbound.elapsed() >= INBOUND_STALE {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.connected = false;
                                snapshot.last_event =
                                    "Peer unresponsive (>8s without HeartbeatAck): reconnecting."
                                        .to_string();
                            });
                            break;
                        }

                        heartbeat_seq += 1;

                        let heartbeat = WireMessage::Heartbeat {
                            seq: heartbeat_seq,
                            unix_ms: now_unix_ms(),
                        };

                        if let Err(err) = write_wire(&mut write_half, &heartbeat).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Failed to send Heartbeat: {err}");
                            });
                            break;
                        }

                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.role = "controller".to_string();
                            snapshot.connected = true;
                            snapshot.heartbeat_seq = heartbeat_seq;
                            snapshot.last_event = format!("Heartbeat sent. seq={heartbeat_seq}");
                        });
                    }
                }
            }
        }
        Err(err) => {
            update_tcp_state(&tcp_state, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = false;
                snapshot.peer_addr = Some(addr.clone());
                snapshot.last_event = format!("Failed to connect to {addr}: {err}");
            });
        }
    }

    update_tcp_state(&tcp_state, |snapshot| {
        if snapshot.role == "controller" {
            snapshot.connected = false;
        }
    });
}

#[cfg(test)]
mod tests {
    //! Backoff-policy unit tests plus a loopback-TCP behavioral test of the
    //! outbound session: the controller must open with a Hello carrying the
    //! configured pairing token and protocol version, and must end its session
    //! task when the receiver goes away.

    use super::*;
    use tailkvm_net::protocol::{encode_line, PROTOCOL_VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::net::TcpListener;

    #[test]
    fn backoff_doubles_and_caps() {
        // Failing fast (session < 15s): 1 -> 2 -> 4 -> 8 -> 10 -> 10.
        let mut b = BACKOFF_START_SECS;
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(current_backoff_secs(b, 0));
            b = next_backoff_secs(b, 0);
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 10]);
        assert_eq!(next_backoff_secs(b, 0), BACKOFF_MAX_SECS, "stays capped");
    }

    #[test]
    fn backoff_resets_after_healthy_session() {
        // Inherited long delay + a session that survived >= 15s: the next
        // retry sleeps the initial delay again, not the inherited one.
        assert_eq!(
            current_backoff_secs(10, HEALTHY_SESSION_SECS),
            BACKOFF_START_SECS
        );
        assert_eq!(
            next_backoff_secs(10, HEALTHY_SESSION_SECS),
            BACKOFF_START_SECS * 2
        );
        // Just under the threshold keeps the inherited delay.
        assert_eq!(current_backoff_secs(10, HEALTHY_SESSION_SECS - 1), 10);
    }

    #[tokio::test]
    async fn controller_sends_hello_with_token_and_version_then_ends_on_server_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let (_command_tx, command_rx) = mpsc::unbounded_channel::<WireMessage>();
        let session = tokio::spawn(run_controller_session(
            addr,
            Arc::new(Mutex::new(TcpSessionSnapshot::default())),
            command_rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(RemoteControlState::default())),
            Arc::new(Mutex::new(
                tailkvm_win32::clipboard::ClipboardLoopGuard::new(),
            )),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(HashMap::new())),
            "test-screen".to_string(),
            Arc::new(Mutex::new(Some("secret".to_string()))),
        ));

        let (server, _) = listener.accept().await.unwrap();
        let mut reader = TokioBufReader::new(server);

        // First line on the wire must be the Hello, carrying the pairing token
        // and this build's protocol version.
        let mut line = String::new();
        time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("timed out waiting for Hello")
            .unwrap();
        match decode_line(line.trim_end()).expect("undecodable first line") {
            WireMessage::Hello {
                auth_token,
                protocol_version,
                ..
            } => {
                assert_eq!(auth_token.as_deref(), Some("secret"));
                assert_eq!(protocol_version, PROTOCOL_VERSION);
            }
            other => panic!("expected Hello first, got {other:?}"),
        }

        // Accept the handshake, then vanish: the controller session task must
        // end (EOF path) instead of lingering as a zombie.
        let ack = encode_line(&WireMessage::HelloAck {
            receiver_machine_name: "fake-receiver".to_string(),
            accepted: true,
            message: "accepted".to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
        reader.get_mut().write_all(&ack).await.unwrap();
        drop(reader);

        time::timeout(Duration::from_secs(5), session)
            .await
            .expect("controller session must end when the receiver drops")
            .unwrap();
    }
}
