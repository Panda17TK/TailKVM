//! TCP session management: receiver loop, controller session/supervisor,
//! wire helpers and heartbeats.
//!
//! Final slice of the lib.rs decomposition (#17). Everything is
//! `pub(crate)`: internal plumbing, not crate API.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tailkvm_net::protocol::{decode_line, encode_line, WireMessage};
use tailkvm_win32::monitor::MonitorTopology;
use tauri::State;
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::{self, Duration},
};

use crate::clipboard_sync::*;
use crate::forwarding::*;
use crate::seamless::*;
use crate::state::*;
#[tauri::command]
pub(crate) async fn start_tcp_receiver(
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);

    if state.receiver_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Receiver is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let tcp_state = state.tcp.clone();
    let receiver_running = state.receiver_running.clone();
    let receiver_tx = state.receiver_tx.clone();
    let clipboard_guard = state.clipboard_guard.clone();
    let accept_incoming = state.accept_incoming.clone();
    let receiver_abort = state.receiver_abort.clone();

    tauri::async_runtime::spawn(async move {
        let listen_addr = format!("0.0.0.0:{port}");

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.role = "receiver".to_string();
            snapshot.listening = false;
            snapshot.listen_addr = Some(listen_addr.clone());
            snapshot.connected = false;
            snapshot.last_event = format!("Starting receiver on {listen_addr}...");
        });

        match TcpListener::bind(&listen_addr).await {
            Ok(listener) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.listening = true;
                    snapshot.last_event = format!("Receiver listening on {listen_addr}.");
                });

                // Single active session, newest wins: when a new controller
                // connects, signal the previous handler to stop so a crashed /
                // zombie connection self-heals on reconnect. The displaced
                // handler still runs its stuck-input release on the way out.
                let mut active_cancel: Option<tokio::sync::oneshot::Sender<()>> = None;

                loop {
                    match listener.accept().await {
                        Ok((stream, peer_addr)) => {
                            // Disable Nagle so each injected input event is sent
                            // immediately instead of being coalesced (KVM latency).
                            let _ = stream.set_nodelay(true);
                            let peer_addr_text = peer_addr.to_string();
                            let tcp_state_for_client = tcp_state.clone();
                            let receiver_tx_for_client = receiver_tx.clone();
                            let clipboard_guard_for_client = clipboard_guard.clone();
                            let accept_incoming_for_client = accept_incoming.clone();
                            let receiver_abort_for_client = receiver_abort.clone();

                            // Displace any existing session.
                            if let Some(old_cancel) = active_cancel.take() {
                                let _ = old_cancel.send(());
                            }
                            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
                            active_cancel = Some(cancel_tx);

                            tauri::async_runtime::spawn(async move {
                                handle_receiver_stream(
                                    stream,
                                    peer_addr_text,
                                    tcp_state_for_client,
                                    cancel_rx,
                                    receiver_tx_for_client,
                                    clipboard_guard_for_client,
                                    accept_incoming_for_client,
                                    receiver_abort_for_client,
                                )
                                .await;
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state, |snapshot| {
                                snapshot.last_event = format!("Receiver accept failed: {err}");
                            });
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                update_tcp_state(&tcp_state, |snapshot| {
                    snapshot.role = "receiver".to_string();
                    snapshot.listening = false;
                    snapshot.connected = false;
                    snapshot.last_event =
                        format!("Failed to bind receiver on {listen_addr}: {err}");
                });
            }
        }

        receiver_running.store(false, Ordering::SeqCst);

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.listening = false;
        });
    });

    time::sleep(Duration::from_millis(150)).await;
    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
pub(crate) async fn connect_tcp_peer(
    host: String,
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let host = host.trim().to_string();

    if host.is_empty() {
        return Err("host is empty. Enter a Tailscale IP such as 100.x.y.z.".to_string());
    }

    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    let addr = format!("{host}:{port}");
    let tcp_state = state.tcp.clone();

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = false;
        snapshot.peer_addr = Some(addr.clone());
        snapshot.peer_name = None;
        snapshot.last_event = format!("Connecting to {addr}...");
    });

    // Supersede any existing 1:1 controller supervisor before starting a new
    // one. Bumping the generation makes the old supervisor exit; clearing the
    // command channel ends its in-flight session immediately. Without this, a
    // second connect (e.g. a double-click) leaves two supervisors dialing the
    // same peer and churning the receiver's single session slot.
    let my_gen = state.controller_generation.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut tx_guard) = state.controller_tx.lock() {
        *tx_guard = None;
    }

    let should_run = state.controller_should_run.clone();
    should_run.store(true, Ordering::SeqCst);

    spawn_controller_supervisor(
        addr,
        state.tcp.clone(),
        state.capture_running.clone(),
        state.remote_control.clone(),
        state.clipboard_guard.clone(),
        state.screen_sizes.clone(),
        state.sessions.clone(),
        state.controller_tx.clone(),
        should_run,
        "controller".to_string(),
        Some((state.controller_generation.clone(), my_gen)),
    );

    time::sleep(Duration::from_millis(200)).await;
    Ok(tcp_snapshot(&state.tcp))
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
        let mut backoff_secs: u64 = 1;
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
            )
            .await;
            let session_secs = session_start.elapsed().as_secs();

            if let Ok(mut tx_guard) = tx_slot.lock() {
                *tx_guard = None;
            }

            if !should_run.load(Ordering::SeqCst) || !is_current() {
                break;
            }

            // A session that stayed up for a while was healthy — reset the
            // backoff so a one-off drop reconnects fast (instead of inheriting a
            // 10s wait from earlier failures).
            if session_secs >= 15 {
                backoff_secs = 1;
            }

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
                    "[{screen_label}] dropped after {session_secs}s ({reason}). Reconnecting in {backoff_secs}s..."
                );
            });

            let mut waited = 0;
            while waited < backoff_secs && should_run.load(Ordering::SeqCst) && is_current() {
                time::sleep(Duration::from_secs(1)).await;
                waited += 1;
            }
            backoff_secs = (backoff_secs * 2).min(10);
        }

        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.connected = false;
            snapshot.last_event = format!("[{screen_label}] session ended.");
        });
    });
}

/// Explicitly disconnect the controller session and stop auto-reconnect.
#[tauri::command]
pub(crate) async fn disconnect_tcp_peer(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.controller_should_run.store(false, Ordering::SeqCst);
    // Dropping the command sender ends the current session's select loop.
    if let Ok(mut tx_guard) = state.controller_tx.lock() {
        *tx_guard = None;
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.connected = false;
        snapshot.last_event = "Disconnect requested; auto-reconnect stopped.".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Toggle whether the receiver accepts incoming controller connections (G1).
#[tauri::command]
pub(crate) async fn set_accept_incoming(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.accept_incoming.store(enabled, Ordering::SeqCst);
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = if enabled {
            "Accepting incoming controller connections.".to_string()
        } else {
            "Rejecting incoming controller connections.".to_string()
        };
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Connect (or reconnect) a named screen for multi-machine control (B1.2).
/// Re-connecting an existing name replaces the previous session.
#[tauri::command]
pub(crate) async fn connect_screen(
    name: String,
    host: String,
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let name = name.trim().to_string();
    let host = host.trim().to_string();
    if name.is_empty() || host.is_empty() {
        return Err("screen name and host are required.".to_string());
    }
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);
    let addr = format!("{host}:{port}");

    start_named_session(&state, &name, &addr)?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = format!("Connecting screen '{name}' to {addr}...");
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Start (or replace) a named reconnecting session to `addr`. Sync, so it can
/// be called from a command or from app startup (B1.2 / B1.6 auto-connect).
pub(crate) fn start_named_session(state: &AppState, name: &str, addr: &str) -> Result<(), String> {
    let mut map = state
        .sessions
        .lock()
        .map_err(|_| "sessions mutex poisoned".to_string())?;

    if let Some(old) = map.remove(name) {
        old.should_run.store(false, Ordering::SeqCst);
        if let Ok(mut tx) = old.tx.lock() {
            *tx = None;
        }
    }

    let should_run = Arc::new(AtomicBool::new(true));
    let tx: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> = Arc::new(Mutex::new(None));
    map.insert(
        name.to_string(),
        ScreenSession {
            should_run: should_run.clone(),
            tx: tx.clone(),
        },
    );

    spawn_controller_supervisor(
        addr.to_string(),
        state.tcp.clone(),
        state.capture_running.clone(),
        state.remote_control.clone(),
        state.clipboard_guard.clone(),
        state.screen_sizes.clone(),
        state.sessions.clone(),
        tx,
        should_run,
        name.to_string(),
        None,
    );

    Ok(())
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
    let mut lines = BufReader::new(read_half).lines();

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
            read = lines.next_line() => read,
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
                }) => {
                    // Arm the heartbeat watchdog from the handshake: a
                    // controller that connects and then stalls without ever
                    // heartbeating is also caught.
                    last_heartbeat = Some(Instant::now());
                    let accepted = accept_incoming.load(Ordering::SeqCst);

                    update_tcp_state(&tcp_state, |snapshot| {
                        snapshot.peer_name = Some(machine_name.clone());
                        snapshot.last_event = if accepted {
                            format!("Hello from {machine_name} / app {app_version}.")
                        } else {
                            format!("Rejected connection from {machine_name} (not accepting).")
                        };
                    });

                    let ack = WireMessage::HelloAck {
                        receiver_machine_name: local_machine_name(),
                        accepted,
                        message: if accepted {
                            "accepted".to_string()
                        } else {
                            "receiver is not accepting connections".to_string()
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
            let mut lines = BufReader::new(read_half).lines();

            let hello = WireMessage::Hello {
                machine_name: local_machine_name(),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
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
                    line = lines.next_line() => {
                        match line {
                            Ok(Some(line)) => {
                                // Any inbound traffic proves the peer is alive.
                                last_inbound = Instant::now();
                                match decode_line(&line) {
                                    Ok(WireMessage::HelloAck { receiver_machine_name, accepted, message }) => {
                                        update_tcp_state(&tcp_state, |snapshot| {
                                            snapshot.peer_name = Some(receiver_machine_name.clone());
                                            snapshot.connected = accepted;
                                            snapshot.last_event = format!("HelloAck from {receiver_machine_name}: {message}");
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
                            Some(outbound) => {
                                if let Err(err) = write_wire(&mut write_half, &outbound).await {
                                    update_tcp_state(&tcp_state, |snapshot| {
                                        snapshot.last_event = format!("Failed to send command message: {err}");
                                    });
                                    break;
                                }

                                // Skip the per-event UI update for high-rate mouse
                                // moves: it would allocate + lock ~30x/s and clobber
                                // the capture loop's throttled progress summary.
                                if !matches!(outbound, WireMessage::MouseMove { .. }) {
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
