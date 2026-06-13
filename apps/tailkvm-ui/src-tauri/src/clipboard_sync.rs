//! Clipboard synchronization (roadmap B1.5).
//!
//! Final slice of the lib.rs decomposition (#17): broadcast/relay helpers
//! and the clipboard Tauri commands (auto-sync watcher + manual send).
//! Everything is `pub(crate)`: internal plumbing, not crate API.

use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc, Mutex},
};
use tailkvm_net::protocol::WireMessage;
use tauri::State;
use tokio::{sync::mpsc, time::Duration};

use crate::state::*;
/// Pick the active outbound channel: the controller channel if connected as a
/// controller, otherwise the receiver channel. Used so clipboard sync works in
/// either role (bidirectional).
/// Broadcast a clipboard text to every connected peer: all named multi-screen
/// sessions plus the legacy 1:1 controller/receiver channels (roadmap B1.5).
/// Returns how many peers it was sent to.
pub(crate) fn broadcast_clipboard(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    controller_tx: &Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    receiver_tx: &Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    text: &str,
) -> usize {
    let message = WireMessage::ClipboardText {
        text: text.to_string(),
    };
    let mut sent = 0;

    if let Ok(map) = sessions.lock() {
        for session in map.values() {
            if let Ok(tx) = session.tx.lock() {
                if let Some(sender) = tx.as_ref() {
                    if sender.send(message.clone()).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
    }

    for slot in [controller_tx, receiver_tx] {
        if let Ok(guard) = slot.lock() {
            if let Some(sender) = guard.as_ref() {
                if sender.send(message.clone()).is_ok() {
                    sent += 1;
                }
            }
        }
    }

    sent
}

/// Relay a clipboard text received from `origin` to every *other* named session
/// (roadmap B1.5 client->sibling relay), making the server a clipboard hub.
/// Returns how many siblings it was sent to.
pub(crate) fn relay_clipboard(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    origin: &str,
    text: &str,
) -> usize {
    let message = WireMessage::ClipboardText {
        text: text.to_string(),
    };
    let mut sent = 0;
    if let Ok(map) = sessions.lock() {
        for (name, session) in map.iter() {
            if name == origin {
                continue;
            }
            if let Ok(tx) = session.tx.lock() {
                if let Some(sender) = tx.as_ref() {
                    if sender.send(message.clone()).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
    }
    sent
}

/// Enable/disable automatic bidirectional clipboard sync (roadmap D1). When on,
/// a clipboard-change watcher forwards local text changes to the peer; the echo
/// guard suppresses re-broadcasting content we just applied. Default off.
#[tauri::command]
pub(crate) async fn set_clipboard_sync(
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    if !enabled {
        state.clipboard_sync_running.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = state.clipboard_watch.lock() {
            *guard = None;
        }
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard sync OFF.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    if state.clipboard_sync_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard sync is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    let (change_tx, change_rx) = std::sync::mpsc::channel::<()>();
    let handle = match tailkvm_win32::clipboard_watch::start_clipboard_watch(change_tx) {
        Ok(handle) => handle,
        Err(err) => {
            state.clipboard_sync_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = state
            .clipboard_watch
            .lock()
            .map_err(|_| "clipboard watch mutex poisoned".to_string())?;
        *guard = Some(handle);
    }

    let controller_tx = state.controller_tx.clone();
    let receiver_tx = state.receiver_tx.clone();
    let sessions = state.sessions.clone();
    let clipboard_guard = state.clipboard_guard.clone();
    let tcp_state = state.tcp.clone();
    let running = state.clipboard_sync_running.clone();

    std::thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            match change_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => {
                    let text = match tailkvm_win32::clipboard::get_clipboard_text() {
                        Ok(Some(text)) if !text.is_empty() => text,
                        _ => continue,
                    };
                    let text = text.chars().take(100_000).collect::<String>();

                    let should_send = {
                        match clipboard_guard.lock() {
                            Ok(mut guard) => guard.should_broadcast(&text),
                            Err(_) => false,
                        }
                    };
                    if !should_send {
                        continue;
                    }

                    let sent = broadcast_clipboard(&sessions, &controller_tx, &receiver_tx, &text);
                    if sent > 0 {
                        update_tcp_state(&tcp_state, |snapshot| {
                            snapshot.last_event = format!(
                                "Clipboard change auto-synced ({} chars) to {sent} peer(s).",
                                text.chars().count()
                            );
                        });
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Clipboard sync ON (bidirectional auto).".to_string();
    });

    Ok(tcp_snapshot(&state.tcp))
}

#[tauri::command]
pub(crate) async fn send_clipboard_text(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let sender = {
        let guard = state
            .controller_tx
            .lock()
            .map_err(|_| "controller channel mutex poisoned".to_string())?;
        guard.clone()
    };

    let Some(sender) = sender else {
        return Err("No active controller session. Connect to a peer first.".to_string());
    };

    let text = match tailkvm_win32::clipboard::get_clipboard_text()? {
        Some(text) if !text.is_empty() => text,
        _ => return Err("Local clipboard has no text to send.".to_string()),
    };

    // Cap to a sane size so a huge paste can't flood the control link.
    let text = text.chars().take(100_000).collect::<String>();

    // Skip resending content identical to what we last sent/applied (echo guard).
    let should_send = {
        let mut guard = state
            .clipboard_guard
            .lock()
            .map_err(|_| "clipboard guard mutex poisoned".to_string())?;
        guard.should_broadcast(&text)
    };

    if !should_send {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Clipboard unchanged since last send; skipped.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    sender
        .send(WireMessage::ClipboardText { text: text.clone() })
        .map_err(|e| format!("failed to queue clipboard text: {e}"))?;

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!("Queued ClipboardText: {} chars", text.chars().count());
    });

    Ok(tcp_snapshot(&state.tcp))
}
