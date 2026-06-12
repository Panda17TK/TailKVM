//! Hook-based input forwarding (controller side).
//!
//! Second slice of the lib.rs decomposition (#17): the low-level mouse and
//! keyboard hook forwarding loops, including the hook health watchdog (#12),
//! IME composition wiring (#10), and pressed-key/button tracking used to
//! release stuck input when capture stops. Everything is `pub(crate)`:
//! internal plumbing, not crate API.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tailkvm_net::protocol::WireMessage;
use tokio::sync::mpsc;

use crate::state::{update_tcp_state, KeyboardForwardingContext, TcpSessionSnapshot};

/// Where hook-forwarded input is sent. `Fixed` targets one session (1:1);
/// `Active` resolves the current target at send time so the multi-screen router
/// can switch screens without restarting the hooks (roadmap B1.3). A missing
/// active target drops the event without erroring (the hook keeps running).
#[derive(Clone)]
pub(crate) enum SenderTarget {
    Fixed(mpsc::UnboundedSender<WireMessage>),
    /// Resolved at send time by the multi-screen router (roadmap B1.4).
    Active(Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>),
}

impl SenderTarget {
    fn send(&self, message: WireMessage) -> Result<(), ()> {
        match self {
            SenderTarget::Fixed(sender) => sender.send(message).map_err(|_| ()),
            SenderTarget::Active(slot) => {
                if let Ok(guard) = slot.lock() {
                    if let Some(sender) = guard.as_ref() {
                        return sender.send(message).map_err(|_| ());
                    }
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn start_mouse_hook_forwarding(
    sender: SenderTarget,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    label: &'static str,
) -> Result<(), String> {
    if mouse_hook_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!("Mouse hook capture is already running. mode={label}");
        });
        return Ok(());
    }

    let (event_tx, event_rx) =
        std::sync::mpsc::channel::<tailkvm_win32::mouse_hook::MouseHookEvent>();

    let hook = match tailkvm_win32::mouse_hook::start_mouse_hook(event_tx) {
        Ok(hook) => hook,
        Err(err) => {
            mouse_hook_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = mouse_hook
            .lock()
            .map_err(|_| "mouse hook mutex poisoned".to_string())?;
        *guard = Some(hook);
    }

    let tcp_state_for_task = tcp_state.clone();
    let mouse_hook_running_for_task = mouse_hook_running.clone();
    let mouse_hook_for_task = mouse_hook.clone();

    std::thread::spawn(move || {
        let mut event_count: u64 = 0;
        let mut pressed_buttons: Vec<String> = Vec::new();

        // Hook health-check state (#12): rebindable receiver so a silently
        // removed hook can be reinstalled on a fresh channel in place.
        let mut event_rx = event_rx;
        let mut last_marker = Instant::now();
        let mut marker_baseline = tailkvm_win32::mouse_hook::health_marker_seen();
        let mut marker_pending = false;
        let mut missed_markers: u8 = 0;

        while mouse_hook_running_for_task.load(Ordering::SeqCst) {
            // Hook health check (#12): see the keyboard twin. A removed mouse
            // hook means local clicks leak while forwarding continues.
            if last_marker.elapsed() >= Duration::from_secs(2) {
                if marker_pending {
                    let seen = tailkvm_win32::mouse_hook::health_marker_seen();
                    if seen == marker_baseline {
                        missed_markers += 1;
                    } else {
                        missed_markers = 0;
                    }
                    marker_baseline = seen;
                }
                if missed_markers >= 2 {
                    missed_markers = 0;
                    if let Ok(mut guard) = mouse_hook_for_task.lock() {
                        *guard = None; // joins the dead hook's pump thread
                    }
                    let (new_tx, new_rx) = std::sync::mpsc::channel();
                    match tailkvm_win32::mouse_hook::start_mouse_hook(new_tx) {
                        Ok(new_hook) => {
                            if let Ok(mut guard) = mouse_hook_for_task.lock() {
                                *guard = Some(new_hook);
                            }
                            event_rx = new_rx;
                            marker_baseline = tailkvm_win32::mouse_hook::health_marker_seen();
                            update_tcp_state(&tcp_state_for_task, |snapshot| {
                                snapshot.last_event =
                                    "Mouse hook was silently removed by the OS; reinstalled."
                                        .to_string();
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state_for_task, |snapshot| {
                                snapshot.last_event = format!(
                                    "Mouse hook lost and reinstall failed: {err}. Stopping forwarding."
                                );
                            });
                            mouse_hook_running_for_task.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                marker_pending = tailkvm_win32::mouse::send_hook_health_marker().is_ok();
                last_marker = Instant::now();
            }

            // Block until an event arrives (≈0ms added latency); the timeout
            // only bounds how long we wait before re-checking the stop flag.
            let event = match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            let message = match event {
                tailkvm_win32::mouse_hook::MouseHookEvent::Button { button, down } => {
                    track_button_press(&mut pressed_buttons, &button, down);
                    WireMessage::MouseButton { button, down }
                }
                tailkvm_win32::mouse_hook::MouseHookEvent::Wheel { delta, horizontal } => {
                    WireMessage::MouseWheel { delta, horizontal }
                }
            };

            if sender.send(message.clone()).is_err() {
                mouse_hook_running_for_task.store(false, Ordering::SeqCst);
                update_tcp_state(&tcp_state_for_task, |snapshot| {
                    snapshot.connected = false;
                    snapshot.last_event =
                        "Mouse hook capture stopped: controller channel closed.".to_string();
                });
                break;
            }

            event_count += 1;

            update_tcp_state(&tcp_state_for_task, |snapshot| {
                snapshot.role = "controller".to_string();
                snapshot.connected = true;
                snapshot.last_event = format!(
                    "Mouse hook event forwarded. mode={label}, count={}, event={message:?}",
                    event_count
                );
            });
        }

        // Always uninstall the hook when the loop ends (failsafe, peer
        // disconnect, or manual stop) so local click/wheel input is no longer
        // suppressed. Without this, an internal exit (e.g. controller channel
        // closed) would leave the low-level hook installed and the local mouse
        // buttons captured — a lockout.
        if let Ok(mut guard) = mouse_hook_for_task.lock() {
            *guard = None;
        }

        for button in pressed_buttons.drain(..) {
            let _ = sender.send(WireMessage::MouseButton {
                button,
                down: false,
            });
        }

        update_tcp_state(&tcp_state_for_task, |snapshot| {
            snapshot.last_event =
                format!("Mouse hook capture stopped. mode={label}, events={event_count}. Released stuck buttons.");
        });
    });

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Mouse hook capture started. mode={label}. Local click/wheel events are suppressed."
        );
    });

    Ok(())
}

pub(crate) fn stop_mouse_hook_forwarding(
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    label: &'static str,
) -> Result<(), String> {
    mouse_hook_running.store(false, Ordering::SeqCst);

    {
        let mut guard = mouse_hook
            .lock()
            .map_err(|_| "mouse hook mutex poisoned".to_string())?;
        *guard = None;
    }

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!("Mouse hook capture stop requested. mode={label}");
    });

    Ok(())
}

/// Generation counter for the keyboard-forward thread. Each successful call to
/// `start_keyboard_hook_forwarding` claims a new generation; the spawned thread
/// only resets the shared running flag / hook (and keeps looping) while it is
/// still the current generation. This prevents a superseded thread — e.g. after
/// a quick return-then-cross, which the multi-edge crossing makes more frequent
/// — from clearing the flag/hook that a newer thread owns, which silently
/// dropped keyboard forwarding (keys then typed locally instead of the peer).
static KEYBOARD_HOOK_GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn start_keyboard_hook_forwarding(
    ctx: &KeyboardForwardingContext,
    sender: SenderTarget,
    label: &'static str,
) -> Result<(), String> {
    let tcp_state = ctx.tcp_state.clone();
    let keyboard_hook_running = ctx.keyboard_hook_running.clone();
    let keyboard_hook = ctx.keyboard_hook.clone();
    let capture_running = ctx.capture_running.clone();
    let mouse_hook_running = ctx.mouse_hook_running.clone();
    let mouse_hook = ctx.mouse_hook.clone();
    let remote_control = ctx.remote_control.clone();
    let resolve_characters = ctx.resolve_characters.clone();

    if keyboard_hook_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&tcp_state, |snapshot| {
            snapshot.last_event = format!("Keyboard hook capture is already running. mode={label}");
        });
        return Ok(());
    }

    // Claim a generation. The spawned thread owns the shared flag/hook only
    // while this stays the current generation (see KEYBOARD_HOOK_GENERATION).
    let my_gen = KEYBOARD_HOOK_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);

    let (event_tx, event_rx) =
        std::sync::mpsc::channel::<tailkvm_win32::keyboard_hook::KeyboardHookEvent>();

    let hook = match tailkvm_win32::keyboard_hook::start_keyboard_hook(event_tx) {
        Ok(hook) => hook,
        Err(err) => {
            keyboard_hook_running.store(false, Ordering::SeqCst);
            return Err(err);
        }
    };

    {
        let mut guard = keyboard_hook
            .lock()
            .map_err(|_| "keyboard hook mutex poisoned".to_string())?;
        *guard = Some(hook);
    }

    let tcp_state_for_task = tcp_state.clone();
    let keyboard_hook_running_for_task = keyboard_hook_running.clone();
    let keyboard_hook_for_task = keyboard_hook.clone();

    std::thread::spawn(move || {
        let mut event_count: u64 = 0;
        let mut pressed_keys: Vec<(u16, u16, bool)> = Vec::new();
        // Command-modifier state tracked from the event stream, used to route
        // keys when character resolution is enabled. Shift is folded into
        // character resolution rather than treated as a command modifier.
        // Seeded from the async key state: keys already held when the hook was
        // installed (e.g. a Ctrl+drag edge crossing) never appear in the event
        // stream, so starting from `false` would misclassify them.
        let mut ctrl_down = tailkvm_win32::cursor::is_vk_down(0x11); // VK_CONTROL
        let mut alt_down = tailkvm_win32::cursor::is_vk_down(0x12); // VK_MENU
        let mut win_down = tailkvm_win32::cursor::is_vk_down(0x5B) // VK_LWIN
            || tailkvm_win32::cursor::is_vk_down(0x5C); // VK_RWIN
        let mut shift_down = tailkvm_win32::cursor::is_vk_down(0x10); // VK_SHIFT

        // Hook health-check state (#12): rebindable receiver so a silently
        // removed hook can be reinstalled on a fresh channel in place.
        let mut event_rx = event_rx;
        let mut last_marker = Instant::now();
        let mut marker_baseline = tailkvm_win32::keyboard_hook::health_marker_seen();
        let mut marker_pending = false;
        let mut missed_markers: u8 = 0;

        // IME composition mode (#10): toggled by the IME key while character
        // resolution is on. The hook passes keys through to the local capture
        // window (the real IME composes there) and committed text arrives on
        // `ime_commit_rx` to be forwarded as KeyboardText.
        let (ime_commit_tx, ime_commit_rx) = std::sync::mpsc::channel::<String>();
        let mut ime_capture: Option<tailkvm_win32::ime_capture::ImeCaptureHandle> = None;
        // Never inherit pass-through from a previous forwarding generation.
        tailkvm_win32::keyboard_hook::set_passthrough(false);

        while keyboard_hook_running_for_task.load(Ordering::SeqCst)
            && KEYBOARD_HOOK_GENERATION.load(Ordering::SeqCst) == my_gen
        {
            // Forward committed IME text (composition mode) to the peer as
            // layout-independent Unicode (flushes at least every 100ms via
            // the recv timeout below).
            while let Ok(text) = ime_commit_rx.try_recv() {
                if !text.is_empty() {
                    let _ = sender.send(WireMessage::KeyboardText { text });
                }
            }

            // Hook health check (#12): Windows silently removes a low-level
            // hook whose callback overran LowLevelHooksTimeout. Inject a
            // marker every 2s; two consecutive unseen markers mean the hook
            // is gone (local typing would leak while forwarding continues),
            // so reinstall it in place.
            if last_marker.elapsed() >= Duration::from_secs(2) {
                if marker_pending {
                    let seen = tailkvm_win32::keyboard_hook::health_marker_seen();
                    if seen == marker_baseline {
                        missed_markers += 1;
                    } else {
                        missed_markers = 0;
                    }
                    marker_baseline = seen;
                }
                if missed_markers >= 2 {
                    missed_markers = 0;
                    if let Ok(mut guard) = keyboard_hook_for_task.lock() {
                        *guard = None; // joins the dead hook's pump thread
                    }
                    let (new_tx, new_rx) = std::sync::mpsc::channel();
                    match tailkvm_win32::keyboard_hook::start_keyboard_hook(new_tx) {
                        Ok(new_hook) => {
                            if let Ok(mut guard) = keyboard_hook_for_task.lock() {
                                *guard = Some(new_hook);
                            }
                            event_rx = new_rx;
                            marker_baseline = tailkvm_win32::keyboard_hook::health_marker_seen();
                            update_tcp_state(&tcp_state_for_task, |snapshot| {
                                snapshot.last_event =
                                    "Keyboard hook was silently removed by the OS; reinstalled."
                                        .to_string();
                            });
                        }
                        Err(err) => {
                            update_tcp_state(&tcp_state_for_task, |snapshot| {
                                snapshot.last_event = format!(
                                    "Keyboard hook lost and reinstall failed: {err}. Stopping forwarding."
                                );
                            });
                            keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                marker_pending = tailkvm_win32::keyboard::send_hook_health_marker().is_ok();
                last_marker = Instant::now();
            }

            // Block until an event arrives (≈0ms added latency); the timeout
            // only bounds how long we wait before re-checking the stop flag.
            let event = match event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            match event {
                tailkvm_win32::keyboard_hook::KeyboardHookEvent::Failsafe => {
                    keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
                    capture_running.store(false, Ordering::SeqCst);

                    let _ = stop_mouse_hook_forwarding(
                        mouse_hook_running.clone(),
                        mouse_hook.clone(),
                        tcp_state_for_task.clone(),
                        "failsafe-keyboard",
                    );

                    if let Ok(mut remote_state) = remote_control.lock() {
                        remote_state.active = false;
                    }

                    update_tcp_state(&tcp_state_for_task, |snapshot| {
                        snapshot.last_event =
                            "Keyboard failsafe Ctrl+Alt+Pause received. All captures stopping."
                                .to_string();
                    });

                    break;
                }
                tailkvm_win32::keyboard_hook::KeyboardHookEvent::Key {
                    vk,
                    scan_code,
                    down,
                    extended,
                } => {
                    // Track modifier state from the stream.
                    if let Some(modifier) = tailkvm_win32::key_class::modifier_kind(vk) {
                        match modifier {
                            tailkvm_win32::key_class::Modifier::Ctrl => ctrl_down = down,
                            tailkvm_win32::key_class::Modifier::Alt => alt_down = down,
                            tailkvm_win32::key_class::Modifier::Win => win_down = down,
                            tailkvm_win32::key_class::Modifier::Shift => shift_down = down,
                        }
                    }

                    // Helper to forward a physical key event and track it for
                    // stuck-key release.
                    let physical = |pressed: &mut Vec<(u16, u16, bool)>| {
                        track_key_press(pressed, vk, scan_code, extended, down);
                        WireMessage::KeyboardKey {
                            vk,
                            scan_code,
                            down,
                            extended,
                        }
                    };

                    let message: Option<WireMessage> = if resolve_characters.load(Ordering::SeqCst)
                    {
                        match tailkvm_win32::key_class::classify_key(
                            vk, ctrl_down, alt_down, win_down,
                        ) {
                            // IME toggle (半角/全角 等): switch composition
                            // mode (#10). The capture window hosts the
                            // local IME; keys pass through to it and only
                            // committed text is forwarded as KeyboardText.
                            tailkvm_win32::key_class::KeyRoute::ImeLocal => {
                                if down {
                                    if ime_capture.is_none() {
                                        match tailkvm_win32::ime_capture::start_ime_capture(
                                            ime_commit_tx.clone(),
                                        ) {
                                            Ok(handle) => {
                                                ime_capture = Some(handle);
                                                tailkvm_win32::keyboard_hook::set_passthrough(true);
                                                update_tcp_state(&tcp_state_for_task, |snapshot| {
                                                    snapshot.last_event =
                                                                "IME composition mode ON: compose locally; committed text is forwarded. Toggle again to exit."
                                                                    .to_string();
                                                });
                                            }
                                            Err(err) => {
                                                update_tcp_state(&tcp_state_for_task, |snapshot| {
                                                    snapshot.last_event = format!(
                                                        "IME capture failed to start: {err}"
                                                    );
                                                });
                                            }
                                        }
                                    } else {
                                        // Drop destroys the capture window
                                        // and restores the previous focus.
                                        ime_capture = None;
                                        tailkvm_win32::keyboard_hook::set_passthrough(false);
                                        update_tcp_state(&tcp_state_for_task, |snapshot| {
                                            snapshot.last_event =
                                                "IME composition mode OFF.".to_string();
                                        });
                                    }
                                }
                                None
                            }
                            // While composing, every other key flows to the
                            // local IME window (hook is in pass-through):
                            // neither forward nor track it.
                            _ if ime_capture.is_some() => None,
                            tailkvm_win32::key_class::KeyRoute::Physical => {
                                Some(physical(&mut pressed_keys))
                            }
                            tailkvm_win32::key_class::KeyRoute::Character => {
                                if down {
                                    // Fold the live CapsLock toggle into
                                    // resolution so A↔a follow the
                                    // controller's lock state (was always
                                    // resolved as caps-off).
                                    let caps = tailkvm_win32::cursor::is_vk_toggled(0x14);
                                    match tailkvm_win32::keyboard::resolve_key_text(
                                        vk, scan_code, shift_down, caps,
                                    ) {
                                        Some(text) => Some(WireMessage::KeyboardText { text }),
                                        // Dead key / unresolved: fall back to
                                        // the physical key (tracked for release).
                                        None => Some(physical(&mut pressed_keys)),
                                    }
                                } else if pressed_keys
                                    .iter()
                                    .any(|(k, s, e)| *k == vk && *s == scan_code && *e == extended)
                                {
                                    // Release a physical-fallback key-down.
                                    Some(physical(&mut pressed_keys))
                                } else {
                                    // Character key-up: Unicode was self-contained.
                                    None
                                }
                            }
                        }
                    } else {
                        // Legacy behavior: always reproduce the physical key.
                        Some(physical(&mut pressed_keys))
                    };

                    let Some(message) = message else {
                        continue;
                    };

                    if sender.send(message.clone()).is_err() {
                        keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
                        update_tcp_state(&tcp_state_for_task, |snapshot| {
                            snapshot.connected = false;
                            snapshot.last_event =
                                "Keyboard hook capture stopped: controller channel closed."
                                    .to_string();
                        });
                        break;
                    }

                    event_count += 1;

                    update_tcp_state(&tcp_state_for_task, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Keyboard hook event forwarded. mode={label}, count={}, event={message:?}",
                            event_count
                        );
                    });
                }
            }
        }

        // Reset the shared flag and uninstall the hook ONLY if this thread is
        // still the current generation. A superseded thread (a newer cross has
        // already started its own keyboard thread) must NOT clear the flag/hook
        // it no longer owns — doing so silently dropped keyboard forwarding
        // (keys typed locally). When still current, clearing the flag also fixes
        // the original stuck-true case (the `Disconnected` break) so the next
        // crossing can re-install the hook; uninstalling stops local keyboard
        // suppression after a failsafe/disconnect.
        // Always leave IME pass-through disabled when this thread ends; the
        // capture window (if any) is destroyed by the handle drop.
        if ime_capture.take().is_some() {
            tailkvm_win32::keyboard_hook::set_passthrough(false);
        }

        if KEYBOARD_HOOK_GENERATION.load(Ordering::SeqCst) == my_gen {
            keyboard_hook_running_for_task.store(false, Ordering::SeqCst);
            if let Ok(mut guard) = keyboard_hook_for_task.lock() {
                *guard = None;
            }
        }

        for (vk, scan_code, extended) in pressed_keys.drain(..) {
            let _ = sender.send(WireMessage::KeyboardKey {
                vk,
                scan_code,
                down: false,
                extended,
            });
        }

        update_tcp_state(&tcp_state_for_task, |snapshot| {
            snapshot.last_event = format!(
                "Keyboard hook capture stopped. mode={label}, events={event_count}. Released stuck keys."
            );
        });
    });

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Keyboard hook capture started. mode={label}. Local keyboard input is suppressed. Ctrl+Alt+Pause stops all capture."
        );
    });

    Ok(())
}

pub(crate) fn stop_keyboard_hook_forwarding(
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    label: &'static str,
) -> Result<(), String> {
    keyboard_hook_running.store(false, Ordering::SeqCst);

    {
        let mut guard = keyboard_hook
            .lock()
            .map_err(|_| "keyboard hook mutex poisoned".to_string())?;
        *guard = None;
    }

    update_tcp_state(&tcp_state, |snapshot| {
        snapshot.last_event = format!("Keyboard hook capture stop requested. mode={label}");
    });

    Ok(())
}

/// Track a mouse button down/up in `pressed`, de-duplicating repeated `down`
/// events so each held button is recorded exactly once. When capture stops the
/// caller drains `pressed` to release every still-held button on the receiver,
/// preventing a stuck button. Releasing an unpressed button is a no-op.
pub(crate) fn track_button_press(pressed: &mut Vec<String>, button: &str, down: bool) {
    if down {
        if !pressed.iter().any(|value| value == button) {
            pressed.push(button.to_string());
        }
    } else {
        pressed.retain(|value| value != button);
    }
}

/// Track a keyboard key down/up in `pressed`, keyed by `(vk, scan_code,
/// extended)` and de-duplicating repeated `down` events. Mirrors
/// [`track_button_press`] so still-held keys can be released exactly once when
/// capture stops, preventing a stuck key.
pub(crate) fn track_key_press(
    pressed: &mut Vec<(u16, u16, bool)>,
    vk: u16,
    scan_code: u16,
    extended: bool,
    down: bool,
) {
    let key = (vk, scan_code, extended);
    if down {
        if !pressed.contains(&key) {
            pressed.push(key);
        }
    } else {
        pressed.retain(|entry| entry != &key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_button_press_dedups_and_releases() {
        let mut pressed: Vec<String> = Vec::new();

        track_button_press(&mut pressed, "left", true);
        track_button_press(&mut pressed, "left", true); // duplicate down ignored
        assert_eq!(pressed, vec!["left".to_string()]);

        track_button_press(&mut pressed, "right", true);
        assert_eq!(pressed, vec!["left".to_string(), "right".to_string()]);

        // Releasing one button leaves the other still tracked.
        track_button_press(&mut pressed, "left", false);
        assert_eq!(pressed, vec!["right".to_string()]);

        // Releasing an unpressed button is a no-op (no underflow / phantom).
        track_button_press(&mut pressed, "middle", false);
        assert_eq!(pressed, vec!["right".to_string()]);
    }

    #[test]
    fn track_key_press_dedups_by_vk_scan_extended() {
        let mut pressed: Vec<(u16, u16, bool)> = Vec::new();

        track_key_press(&mut pressed, 0x41, 0x1E, false, true);
        track_key_press(&mut pressed, 0x41, 0x1E, false, true); // duplicate down
        assert_eq!(pressed, vec![(0x41, 0x1E, false)]);

        // Same vk/scan but extended differs -> a distinct held key.
        track_key_press(&mut pressed, 0x41, 0x1E, true, true);
        assert_eq!(pressed, vec![(0x41, 0x1E, false), (0x41, 0x1E, true)]);

        // Releasing the non-extended one keeps the extended one held.
        track_key_press(&mut pressed, 0x41, 0x1E, false, false);
        assert_eq!(pressed, vec![(0x41, 0x1E, true)]);

        // Releasing a key that was never pressed is a no-op.
        track_key_press(&mut pressed, 0x09, 0, false, false);
        assert_eq!(pressed, vec![(0x41, 0x1E, true)]);
    }
}
