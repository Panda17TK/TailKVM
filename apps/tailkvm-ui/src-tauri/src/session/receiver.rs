//! Inbound (being-controlled) session loop. L2 split of session.rs.

use std::sync::{
    atomic::{AtomicBool, Ordering},
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
use crate::forwarding::*;
use crate::state::*;

// Shared-state handles for one inbound session; mirrors
// spawn_controller_supervisor, which carries the same allowance.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_receiver_stream(
    stream: TcpStream,
    peer_addr: String,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
    receiver_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    clipboard_guard: Arc<Mutex<tailkvm_win32::clipboard::ClipboardLoopGuard>>,
    accept_incoming: Arc<AtomicBool>,
    receiver_abort: Arc<AtomicBool>,
    auth_token: Arc<Mutex<Option<String>>>,
) {
    // A stale abort (fired while no session was active) must not kill this
    // brand-new session on its first failsafe tick.
    receiver_abort.store(false, Ordering::SeqCst);
    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.role = "receiver".to_string();
        snapshot.connected = true;
        snapshot.peer_addr = Some(peer_addr.clone());
        snapshot.peer_name = None;
        snapshot.last_event = format!("Accepted connection from {peer_addr}.");
    });

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    // Accumulator for the capped line reader (H3): persists across cancellation
    // of the select! read branch so a partially-read line is not lost.
    let mut line_buf: Vec<u8> = Vec::new();

    // Outbound channel so this side can push unsolicited messages (clipboard)
    // back to the controller, enabling bidirectional sync.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WireMessage>();
    if let Ok(mut guard) = receiver_tx.lock() {
        *guard = Some(out_tx);
    }

    // Safety net: keys/buttons the controller pressed but has not released yet.
    // If the connection drops mid-press we release these on the way out so
    // nothing stays stuck on this receiver. Reuses the same tracking helpers as
    // the controller-side capture loop.
    let mut held_keys: Vec<(u16, u16, bool)> = Vec::new();
    let mut held_buttons: Vec<String> = Vec::new();

    // True only after a Hello has arrived AND `accept_incoming` was set (H2).
    // Until then every other message is dropped, so the "reject incoming"
    // toggle cannot be bypassed by a client that skips the handshake, and no
    // input is injected before the heartbeat watchdog is armed by Hello (M1).
    let mut accepted = false;

    // Throttle for InputInjectionFailed notices (UIPI failures arrive at event
    // rate; one notice per second is enough for the controller to surface it).
    let mut last_inject_fail_notice: Option<Instant> = None;
    // Throttles for the seamless hot path: MouseSetPosition arrives at polling
    // rate, so the diagnostic echo and state-line updates are rate-limited.
    let mut last_setpos_echo: Option<Instant> = None;
    let mut setpos_count: u64 = 0;

    // Fast failsafe tick (recovery routes while being controlled): physical
    // Ctrl+Alt+Pause on THIS machine, the emergency reset from the tray/UI,
    // and a controller-heartbeat watchdog (a controller killed mid-press never
    // sends a FIN, which would otherwise leave keys stuck for minutes). Each
    // drops the session, and the exit path releases held keys/buttons.
    let mut failsafe_check = time::interval(Duration::from_millis(300));
    failsafe_check.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_heartbeat: Option<Instant> = None;
    const HEARTBEAT_STALE: Duration = Duration::from_secs(8);

    // Poll for monitor hotplug / resolution change and re-send ScreenInfo so the
    // controller's router keeps the correct remote size (roadmap #4 hotplug).
    let mut topology_check = time::interval(Duration::from_secs(5));
    topology_check.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_screen_size: Option<(i32, i32)> = None;

    loop {
        let read = tokio::select! {
            read = read_capped_line(&mut reader, &mut line_buf, MAX_WIRE_LINE_BYTES) => read,
            outbound = out_rx.recv() => {
                match outbound {
                    Some(message) => {
                        if let Err(err) = write_wire(&mut write_half, &message).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Receiver failed to send outbound: {err}");
                            });
                            break;
                        }
                        continue;
                    }
                    None => break,
                }
            }
            _ = topology_check.tick() => {
                if let Ok(topology) = tailkvm_win32::monitor::get_monitor_topology() {
                    let size = (topology.virtual_screen.width, topology.virtual_screen.height);
                    if last_screen_size.is_some() && last_screen_size != Some(size) {
                        let info = local_screen_info(&topology);
                        if write_wire(&mut write_half, &info).await.is_ok() {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event =
                                    format!("Monitor change: re-sent ScreenInfo {}x{}.", size.0, size.1);
                            });
                        }
                    }
                    last_screen_size = Some(size);
                }
                continue;
            }
            _ = failsafe_check.tick() => {
                if receiver_abort.swap(false, Ordering::SeqCst) {
                    let bye = WireMessage::Disconnect {
                        reason: "receiver emergency reset".to_string(),
                    };
                    let _ = write_wire(&mut write_half, &bye).await;
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Receiver session aborted by emergency reset.".to_string();
                    });
                    break;
                }
                if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
                    let bye = WireMessage::Disconnect {
                        reason: "receiver failsafe (Ctrl+Alt+Pause)".to_string(),
                    };
                    let _ = write_wire(&mut write_half, &bye).await;
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Receiver failsafe Ctrl+Alt+Pause: controller session dropped."
                                .to_string();
                    });
                    break;
                }
                if last_heartbeat.is_some_and(|t| t.elapsed() >= HEARTBEAT_STALE) {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Controller heartbeat stale (>8s): dropping session, releasing held input."
                                .to_string();
                    });
                    break;
                }
                continue;
            }
            _ = &mut cancel_rx => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event =
                        "Receiver session replaced by a newer controller connection.".to_string();
                });
                break;
            }
        };

        match read {
            Ok(Some(line)) => match decode_line(&line) {
                Ok(WireMessage::Hello {
                    machine_name,
                    app_version,
                    auth_token: peer_token,
                }) => {
                    // Arm the heartbeat watchdog from the handshake: a
                    // controller that connects and then stalls without ever
                    // heartbeating is also caught.
                    last_heartbeat = Some(Instant::now());
                    let is_accepting = accept_incoming.load(Ordering::SeqCst);
                    // H1: a configured pairing token must match the one the
                    // controller presented; with no token configured the
                    // handshake stays open (tailnet trust only).
                    let required_token = auth_token.lock().ok().and_then(|guard| guard.clone());
                    let token_ok =
                        hello_authorized(required_token.as_deref(), peer_token.as_deref());
                    accepted = is_accepting && token_ok;

                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.peer_name = Some(machine_name.clone());
                        snapshot.last_event = if accepted {
                            format!("Hello from {machine_name} / app {app_version}.")
                        } else if !is_accepting {
                            format!("Rejected connection from {machine_name} (not accepting).")
                        } else {
                            format!(
                                "Rejected connection from {machine_name} (pairing token mismatch)."
                            )
                        };
                    });

                    let ack = WireMessage::HelloAck {
                        receiver_machine_name: local_machine_name(),
                        accepted,
                        message: if accepted {
                            "accepted".to_string()
                        } else if !is_accepting {
                            "receiver is not accepting connections".to_string()
                        } else {
                            "pairing token mismatch".to_string()
                        },
                    };

                    if let Err(err) = write_wire(&mut write_half, &ack).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send HelloAck: {err}");
                        });
                        break;
                    }

                    if !accepted {
                        // Politely close the rejected connection.
                        break;
                    }

                    if let Err(err) = send_local_keyboard_layout(&mut write_half).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send KeyboardLayout: {err}");
                        });
                    }

                    // Report our real virtual-screen size so the controller's
                    // router can size this screen accurately (B1.7).
                    if let Ok(topology) = tailkvm_win32::monitor::get_monitor_topology() {
                        let info = local_screen_info(&topology);
                        if let Err(err) = write_wire(&mut write_half, &info).await {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Failed to send ScreenInfo: {err}");
                            });
                        }
                    }
                }
                // Defense in depth (H2/M1): drop every non-Hello message until
                // the handshake has been accepted. A client that never sends an
                // accepted Hello can neither inject input nor apply clipboard
                // content, so the "reject incoming connections" toggle is always
                // enforced — even against a client that skips the handshake.
                Ok(_) if !accepted => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event =
                            "Ignored message before an accepted handshake.".to_string();
                    });
                }
                Ok(WireMessage::KeyboardLayout {
                    language_id,
                    keyboard_type,
                    is_jis_keyboard: _,
                    is_japanese_locale: _,
                    label,
                }) => {
                    apply_peer_keyboard_layout(&tcp_state, language_id, keyboard_type, &label);
                }
                Ok(WireMessage::MouseSetPosition { x, y }) => {
                    // Inject a real absolute mouse move (SendInput) instead of
                    // SetCursorPos: a suppressed/hidden cursor (no physical
                    // mouse, touch input, hide-while-typing) only becomes
                    // visible again on actual mouse input, and SetCursorPos
                    // does not count as input — the cursor moved invisibly.
                    match tailkvm_win32::mouse::send_absolute_mouse_move(x, y) {
                        Ok(()) => {
                            setpos_count += 1;
                            // Rate-limit the diagnostic state line: per-event
                            // formatting and mutex traffic is wasted work at
                            // polling rate.
                            if setpos_count == 1 || setpos_count.is_multiple_of(30) {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.role = "receiver".to_string();
                                    snapshot.connected = true;
                                    snapshot.last_event = format!(
                                        "MouseSetPosition applied. x={x}, y={y} (count={setpos_count})"
                                    );
                                });
                            }

                            // Seamless decides the return locally (no receiver
                            // echo needed), so this MousePosition echo is
                            // diagnostic only — throttle it instead of echoing
                            // every move back at polling rate.
                            let echo_due = last_setpos_echo
                                .is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
                            if echo_due {
                                last_setpos_echo = Some(Instant::now());
                                if let Err(err) = send_current_mouse_position(&mut write_half).await
                                {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.last_event = format!(
                                            "Failed to send MousePosition after set: {err}"
                                        );
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_set_position",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseSetPosition failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::ClipboardText { text }) => {
                    // Remember what we are about to apply so the clipboard
                    // watcher does not echo it back to the controller.
                    if let Ok(mut guard) = clipboard_guard.lock() {
                        guard.mark_applied(&text);
                    }
                    match tailkvm_win32::clipboard::set_clipboard_text(&text) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "ClipboardText applied. chars={}",
                                    text.chars().count()
                                );
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("ClipboardText failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::ClipboardImage { dib_base64 }) => {
                    // #9 phase 1: decode the peer's CF_DIB image and apply it,
                    // marking the guard so our watcher does not echo it back.
                    match decode_dib(&dib_base64) {
                        Ok(dib) => {
                            if let Ok(mut guard) = clipboard_guard.lock() {
                                guard.mark_applied_bytes(&dib);
                            }
                            match tailkvm_win32::clipboard::set_clipboard_dib(&dib) {
                                Ok(()) => {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.role = "receiver".to_string();
                                        snapshot.connected = true;
                                        snapshot.last_event =
                                            format!("ClipboardImage applied. bytes={}", dib.len());
                                    });
                                }
                                Err(err) => {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.last_event =
                                            format!("ClipboardImage failed: {err}");
                                    });
                                }
                            }
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event =
                                    format!("ClipboardImage decode failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::KeyboardText { text }) => {
                    match tailkvm_win32::keyboard::send_keyboard_text(&text) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("KeyboardText applied. chars={}", text.chars().count());
                            });
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "keyboard_text",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("KeyboardText failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::KeyboardKey {
                    vk,
                    scan_code,
                    down,
                    extended,
                }) => {
                    track_key_press(&mut held_keys, vk, scan_code, extended, down);
                    match tailkvm_win32::keyboard::send_key_event(vk, scan_code, down, extended) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "KeyboardKey applied. vk=0x{vk:02x}, scan=0x{scan_code:02x}, down={down}, extended={extended}"
                                );
                            });
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "keyboard_key",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("KeyboardKey failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseWheel { delta, horizontal }) => {
                    match tailkvm_win32::mouse::send_mouse_wheel(delta, horizontal) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event = format!(
                                    "MouseWheel applied. delta={delta}, horizontal={horizontal}"
                                );
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after wheel: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_wheel",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseWheel failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseButton { button, down }) => {
                    track_button_press(&mut held_buttons, &button, down);
                    match tailkvm_win32::mouse::send_mouse_button(&button, down) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("MouseButton applied. button={button}, down={down}");
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after button: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_button",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseButton failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::MouseMove { dx, dy }) => {
                    match tailkvm_win32::mouse::send_relative_mouse_move(dx, dy) {
                        Ok(()) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.role = "receiver".to_string();
                                snapshot.connected = true;
                                snapshot.last_event =
                                    format!("MouseMove applied. dx={dx}, dy={dy}");
                            });

                            if let Err(err) = send_current_mouse_position(&mut write_half).await {
                                update_tcp_state(&tcp_state, |snapshot| {
                                    snapshot.last_event =
                                        format!("Failed to send MousePosition after move: {err}");
                                });
                            }
                        }
                        Err(err) => {
                            notify_injection_failure(
                                &mut write_half,
                                &mut last_inject_fail_notice,
                                "mouse_move",
                                &err,
                            )
                            .await;
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("MouseMove failed: {err}");
                            });
                        }
                    }
                }
                Ok(WireMessage::Heartbeat { seq, unix_ms: _ }) => {
                    last_heartbeat = Some(Instant::now());
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.role = "receiver".to_string();
                        snapshot.connected = true;
                        snapshot.heartbeat_seq = seq;
                        snapshot.last_heartbeat_ms = Some(now_unix_ms());
                        snapshot.last_event = format!("Heartbeat received. seq={seq}");
                    });

                    let ack = WireMessage::HeartbeatAck {
                        seq,
                        unix_ms: now_unix_ms(),
                    };

                    if let Err(err) = write_wire(&mut write_half, &ack).await {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!("Failed to send HeartbeatAck: {err}");
                        });
                        break;
                    }
                }
                Ok(WireMessage::Disconnect { reason }) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Peer disconnected: {reason}");
                    });
                    break;
                }
                Ok(other) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Receiver ignored message: {other:?}");
                    });
                }
                Err(err) => {
                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.last_event = format!("Receiver decode error: {err}");
                    });
                }
            },
            Ok(None) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = "Peer closed TCP connection.".to_string();
                });
                break;
            }
            Err(err) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.last_event = format!("Receiver read error: {err}");
                });
                break;
            }
        }
    }

    // Drop the outbound channel so clipboard sync stops targeting a dead session.
    if let Ok(mut guard) = receiver_tx.lock() {
        *guard = None;
    }

    // Release anything the controller left held when the session ended, so a
    // mid-press disconnect cannot leave a stuck key or button on this machine.
    let released_keys = held_keys.len();
    let released_buttons = held_buttons.len();
    for (vk, scan_code, extended) in held_keys.drain(..) {
        let _ = tailkvm_win32::keyboard::send_key_event(vk, scan_code, false, extended);
    }
    for button in held_buttons.drain(..) {
        let _ = tailkvm_win32::mouse::send_mouse_button(&button, false);
    }

    update_tcp_state(&tcp_state, |snapshot| {
        if snapshot.role == "receiver" {
            snapshot.connected = false;
        }
        if released_keys > 0 || released_buttons > 0 {
            snapshot.last_event = format!(
                "Receiver disconnected. Released {released_keys} stuck key(s), {released_buttons} stuck button(s)."
            );
        }
    });
}
