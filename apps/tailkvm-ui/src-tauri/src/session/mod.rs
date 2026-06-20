//! TCP session management: connect/listen commands plus the named-session
//! helper. The inbound loop lives in `receiver`, the controller session and
//! reconnect supervisor in `controller`, and the wire framing/senders in
//! `wire` (L2 split of the original single-file session module).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tailkvm_net::protocol::WireMessage;
use tauri::State;
use tokio::{
    net::TcpListener,
    sync::mpsc,
    time::{self, Duration},
};

mod controller;
mod receiver;
mod wire;

use controller::spawn_controller_supervisor;
use receiver::handle_receiver_stream;

use crate::state::*;

#[tauri::command]
pub(crate) async fn start_tcp_receiver(
    port: Option<u16>,
    // H1: when true, bind only to this machine's Tailscale IP instead of
    // 0.0.0.0, so the listener is unreachable from the LAN even if the Windows
    // firewall rule is absent. Defaults to the previous 0.0.0.0 behavior.
    tailnet_only: Option<bool>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let port = port.unwrap_or(DEFAULT_TAILKVM_PORT);

    // Resolve the bind host before claiming the running flag so a failed
    // Tailscale lookup leaves the receiver cleanly stopped (fail closed: never
    // silently fall back to 0.0.0.0 when the user asked for tailnet-only).
    let bind_host = if tailnet_only.unwrap_or(false) {
        match crate::tailnet::tailscale_self_ip() {
            Some(ip) => ip,
            None => {
                update_tcp_state(&state.tcp, |snapshot| {
                    snapshot.last_event =
                        "Tailnet-only bind requested but no Tailscale IP found; is Tailscale up?"
                            .to_string();
                });
                return Ok(tcp_snapshot(&state.tcp));
            }
        }
    } else {
        "0.0.0.0".to_string()
    };

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
    let auth_token = state.auth_token.clone();

    tauri::async_runtime::spawn(async move {
        let listen_addr = format!("{bind_host}:{port}");

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
                            let auth_token_for_client = auth_token.clone();

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
                                    auth_token_for_client,
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
        state.auth_token.clone(),
        Some((state.controller_generation.clone(), my_gen)),
    );

    time::sleep(Duration::from_millis(200)).await;
    Ok(tcp_snapshot(&state.tcp))
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

/// Set or clear the shared pairing token (H1). When set, an inbound Hello must
/// carry a matching token or the receiver rejects it, and this controller sends
/// the token in its own Hello. An empty/whitespace value clears it (token
/// disabled — tailnet trust only). Persisted by the frontend and re-pushed on
/// load; read live, so it applies to the next handshake without a restart.
#[tauri::command]
pub(crate) async fn set_auth_token(
    token: Option<String>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let token = token
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    {
        let mut guard = state
            .auth_token
            .lock()
            .map_err(|_| "auth token mutex poisoned".to_string())?;
        *guard = token.clone();
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = if token.is_some() {
            "Pairing token set; peers must present the matching token.".to_string()
        } else {
            "Pairing token cleared (tailnet trust only).".to_string()
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
        state.auth_token.clone(),
        None,
    );

    Ok(())
}
