//! Seamless absolute-cursor capture engine and edge-crossing helpers.
//!
//! Third slice of the lib.rs decomposition (#17): `SeamlessArgs` +
//! `run_seamless_capture` (roadmap A1/E1) plus the edge helpers shared with
//! the legacy remote mode (`normalize_edge`, `is_cursor_at_edge`,
//! `remote_entry_position`, `local_return_position`, `is_remote_return_edge`).
//! Everything is `pub(crate)`: internal plumbing, not crate API.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Instant;
use tailkvm_net::protocol::WireMessage;
use tokio::{
    sync::mpsc,
    time::{self, Duration},
};

use crate::forwarding::{
    start_keyboard_hook_forwarding, start_mouse_hook_forwarding, stop_keyboard_hook_forwarding,
    stop_mouse_hook_forwarding, SenderTarget,
};
use crate::state::{
    update_tcp_state, ImeAnchorSlot, ImeSettings, KeyboardForwardingContext, PeerScreenMap,
    RemoteControlState, TcpSessionSnapshot,
};

/// Owned inputs for the seamless absolute-cursor capture engine (roadmap A1).
pub(crate) struct SeamlessArgs {
    /// Live outbound slot (`AppState.controller_tx`): re-resolved on every
    /// send so the engine survives TCP reconnects — the supervisor swaps a
    /// fresh sender in on reconnect and forwarding resumes automatically.
    pub(crate) sender_slot: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>>,
    pub(crate) tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    pub(crate) capture_running: Arc<AtomicBool>,
    pub(crate) remote_control: Arc<Mutex<RemoteControlState>>,
    pub(crate) mouse_hook_running: Arc<AtomicBool>,
    pub(crate) mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    pub(crate) keyboard_hook_running: Arc<AtomicBool>,
    pub(crate) keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    pub(crate) resolve_characters: Arc<AtomicBool>,
    /// Japanese-IME settings, forwarded into the keyboard context.
    pub(crate) ime_settings: Arc<Mutex<ImeSettings>>,
    /// Candidate-anchor slot: while a remote is controlled this engine
    /// publishes the remote cursor projected onto the controller screen
    /// (IME-POS-010); cleared on every return-to-local path.
    pub(crate) ime_anchor: ImeAnchorSlot,
    /// Remote screen geometry keyed by peer machine name (populated from
    /// ScreenInfo). Used to map the cursor onto the peer's real screen.
    pub(crate) screen_sizes: PeerScreenMap,
    /// Pointer-speed multiplier applied to raw HID deltas while controlling the
    /// remote. Raw input is otherwise integrated 1:1 into remote pixels, which
    /// feels slow next to the local cursor (which has OS pointer ballistics).
    pub(crate) gain: f64,
    /// Physical-pixel rect (l, t, r, b) of the local monitor the peer is pinned
    /// to in the position editor. When set, only that monitor's edge crosses.
    /// None = any monitor the cursor is on may cross.
    pub(crate) attach_monitor: Option<(i32, i32, i32, i32)>,
    /// Physical-pixel rect (l, t, r, b) of the peer's screen as positioned in the
    /// virtual layout by the position editor. When set, the cursor crosses on ANY
    /// local-monitor edge this rect is flush against — so a corner placement that
    /// touches both a vertical and a horizontal monitor edge crosses on either.
    pub(crate) peer_rect: Option<(i32, i32, i32, i32)>,
    /// All local monitor rects, for the outer-edge check (never cross at an
    /// interior boundary where a neighbouring local monitor is).
    pub(crate) local_monitors: Vec<(i32, i32, i32, i32)>,
    pub(crate) local_rect: tailkvm_win32::screen_space::Rect,
    pub(crate) lock_x: i32,
    pub(crate) lock_y: i32,
    pub(crate) switch_edge: String,
    pub(crate) edge_margin: i32,
    pub(crate) remote_width: i32,
    pub(crate) remote_height: i32,
    pub(crate) interval_ms: u64,
    pub(crate) edge_dwell_ms: u64,
    pub(crate) dead_corner_px: i32,
}

/// Whether `edge` of monitor `mon` faces the outer boundary — i.e. no other
/// local monitor is adjacent along that edge. Only outer edges should cross to
/// the remote; an interior edge means the cursor flows into the neighbour.
/// Whether the peer rectangle `peer` is flush-adjacent to monitor `mon` on
/// `edge` (touching that side, with overlap along it). Mirrors `is_outer_edge`
/// but tests adjacency to the peer rect instead of to neighbouring monitors.
fn peer_adjacent(
    mon: (i32, i32, i32, i32),
    edge: tailkvm_win32::screen_space::Edge,
    peer: (i32, i32, i32, i32),
) -> bool {
    use tailkvm_win32::screen_space::Edge;
    let (ml, mt, mr, mb) = mon;
    let (pl, pt, pr, pb) = peer;
    let tol = 6;
    let x_overlap = mr.min(pr) - ml.max(pl);
    let y_overlap = mb.min(pb) - mt.max(pt);
    match edge {
        Edge::Bottom => (pt - mb).abs() <= tol && x_overlap > 0,
        Edge::Top => (mt - pb).abs() <= tol && x_overlap > 0,
        Edge::Right => (pl - mr).abs() <= tol && y_overlap > 0,
        Edge::Left => (ml - pr).abs() <= tol && y_overlap > 0,
    }
}

fn is_outer_edge(
    mon: (i32, i32, i32, i32),
    edge: tailkvm_win32::screen_space::Edge,
    monitors: &[(i32, i32, i32, i32)],
) -> bool {
    use tailkvm_win32::screen_space::Edge;
    let (ml, mt, mr, mb) = mon;
    let tol = 2;
    !monitors.iter().any(|&(nl, nt, nr, nb)| {
        if (nl, nt, nr, nb) == mon {
            return false;
        }
        match edge {
            Edge::Bottom => (nt - mb).abs() <= tol && nl < mr && nr > ml,
            Edge::Top => (nb - mt).abs() <= tol && nl < mr && nr > ml,
            Edge::Right => (nl - mr).abs() <= tol && nt < mb && nb > mt,
            Edge::Left => (nr - ml).abs() <= tol && nt < mb && nb > mt,
        }
    })
}

/// Seamless absolute-cursor capture (roadmap A1/E1). In the local region the
/// real cursor is followed and the configured edge is watched; on crossing,
/// control transfers to the remote and HID relative deltas (Raw Input) drive a
/// logical cursor in the combined space, sent to the receiver as absolute
/// `MouseSetPosition`. Returning is decided locally by the model (no receiver
/// echo), so there is no warp-feedback or drift. Opt-in; legacy modes untouched.
///
/// NOTE: runtime-unvalidated PoC — needs two-machine verification.
pub(crate) async fn run_seamless_capture(a: SeamlessArgs) {
    use tailkvm_win32::screen_space::{
        CombinedSpace, CursorState, Edge, Rect as SsRect, Region, SwitchGuard,
    };

    let edge = Edge::from_label(&a.switch_edge);
    // `combined` is rebuilt on each local->remote crossing with the monitor the
    // cursor is actually on and the peer's latest real screen size; this initial
    // value is only a placeholder until the first crossing.
    let mut combined = CombinedSpace::new(
        a.local_rect,
        SsRect::new(0, 0, a.remote_width, a.remote_height),
        edge,
    );
    let mut switch_guard = SwitchGuard::new(a.edge_dwell_ms, a.interval_ms);

    // Raw Input is required: the remote region integrates HID deltas without
    // moving the local cursor.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(i32, i32)>();
    let _raw_handle = match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(raw_tx) {
        Ok(handle) => handle,
        Err(err) => {
            a.capture_running.store(false, Ordering::SeqCst);
            update_tcp_state(&a.tcp_state, |snapshot| {
                snapshot.last_event =
                    format!("Seamless mode requires Raw Input, unavailable: {err}");
            });
            return;
        }
    };

    let keyboard_ctx = KeyboardForwardingContext {
        tcp_state: a.tcp_state.clone(),
        keyboard_hook_running: a.keyboard_hook_running.clone(),
        keyboard_hook: a.keyboard_hook.clone(),
        capture_running: a.capture_running.clone(),
        mouse_hook_running: a.mouse_hook_running.clone(),
        mouse_hook: a.mouse_hook.clone(),
        remote_control: a.remote_control.clone(),
        resolve_characters: a.resolve_characters.clone(),
        ime_settings: a.ime_settings.clone(),
        ime_anchor: a.ime_anchor.clone(),
    };

    let mut state = CursorState {
        region: Region::Local,
        x: a.lock_x,
        y: a.lock_y,
    };
    let mut remote_active = false;
    let mut sent_count: u64 = 0;
    // Local monitor rect + remote size of the current crossing, kept to
    // project the remote cursor back onto the controller screen as the IME
    // candidate anchor (IME-POS-010).
    let mut anchor_local_rect: Option<(i32, i32, i32, i32)> = None;
    let mut anchor_remote_size = (a.remote_width, a.remote_height);
    let publish_anchor =
        |local: Option<(i32, i32, i32, i32)>, remote_size: (i32, i32), x: i32, y: i32| {
            if let Ok(mut slot) = a.ime_anchor.lock() {
                *slot = local.map(|rect| {
                    tailkvm_win32::ime_anchor::project_remote_to_local(
                        x,
                        y,
                        remote_size.0,
                        remote_size.1,
                        rect,
                    )
                });
            }
        };
    let clear_anchor = || {
        if let Ok(mut slot) = a.ime_anchor.lock() {
            *slot = None;
        }
    };
    // Real monitor rects of the peer (origin-relative, from ScreenInfo); the
    // remote loop clamps the logical cursor onto these so it cannot wander
    // into dead zones of an L-shaped layout's bounding box.
    let mut peer_monitors: Vec<(i32, i32, i32, i32)> = Vec::new();
    // When the user last physically pushed toward each edge (Right, Left, Top,
    // Bottom), from relative HID deltas. Crossing requires a fresh push:
    // injected absolute moves (a peer controlling THIS machine) produce no
    // relative deltas, so they can no longer false-trigger our edge detection
    // in a bidirectional setup.
    let mut last_push: [Option<Instant>; 4] = [None; 4];
    const PUSH_FRESH_MS: u128 = 250;
    // Link watchdog: when the TCP session dies while the remote is being
    // controlled, return control to local input after this long instead of
    // leaving the cursor parked and confined until the failsafe hotkey.
    let mut link_down_since: Option<Instant> = None;
    const LINK_LOST_RETURN: Duration = Duration::from_millis(1500);

    // Resolve the outbound channel at send time (it is swapped on reconnect);
    // returns whether the message was actually queued to a live session.
    let send_remote = |message: WireMessage| -> bool {
        a.sender_slot
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|tx| tx.send(message).is_ok()))
            .unwrap_or(false)
    };
    // Carry sub-pixel remainder of the gain-scaled deltas so slow movements are
    // not lost to rounding (keeps the remote cursor smooth at low speed).
    let mut frac_x = 0.0f64;
    let mut frac_y = 0.0f64;
    let gain = if a.gain.is_finite() && a.gain > 0.0 {
        a.gain
    } else {
        1.0
    };

    update_tcp_state(&a.tcp_state, |snapshot| {
        snapshot.role = "controller".to_string();
        snapshot.connected = true;
        snapshot.last_event = format!(
            "Seamless mode armed (gain {:.2}). Cross the {} edge to control the remote ({}x{}).",
            gain, a.switch_edge, a.remote_width, a.remote_height
        );
    });

    while a.capture_running.load(Ordering::SeqCst) {
        if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
            update_tcp_state(&a.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Seamless capture stopped by Ctrl+Alt+Pause failsafe.".to_string();
            });
            break;
        }

        if !remote_active {
            // Local region: follow the real cursor and watch the switch edge.
            let cur = match tailkvm_win32::cursor::get_cursor_position() {
                Ok(position) => position,
                Err(_) => {
                    time::sleep(Duration::from_millis(a.interval_ms)).await;
                    continue;
                }
            };
            // Drain deltas while local, remembering when the user last
            // physically pushed in each direction (the crossing gate below
            // requires a fresh push toward the edge).
            let mut push_dx = 0i32;
            let mut push_dy = 0i32;
            while let Ok((dx, dy)) = raw_rx.try_recv() {
                push_dx = push_dx.saturating_add(dx);
                push_dy = push_dy.saturating_add(dy);
            }
            let push_now = Instant::now();
            if push_dx > 0 {
                last_push[0] = Some(push_now); // Right
            }
            if push_dx < 0 {
                last_push[1] = Some(push_now); // Left
            }
            if push_dy < 0 {
                last_push[2] = Some(push_now); // Top
            }
            if push_dy > 0 {
                last_push[3] = Some(push_now); // Bottom
            }

            // Detect the switch edge against the monitor the cursor is currently
            // on, not the whole virtual screen. In a mixed multi-monitor layout
            // the virtual-screen edge is unreachable on shorter monitors, so the
            // crossing would never fire there.
            let (m_left, m_top, m_right, m_bottom) =
                tailkvm_win32::monitor::monitor_rect_at_point(cur.x, cur.y);

            // Multi-edge crossing. The peer occupies a virtual rectangle; cross
            // on ANY edge of the current monitor the peer rect is flush against,
            // so a corner placement touching both a vertical and a horizontal
            // monitor edge crosses on either. Without a peer rect, fall back to
            // the single configured edge on its attach monitor (legacy path).
            let cur_mon = (m_left, m_top, m_right, m_bottom);
            let pressing = |e: Edge| match e {
                Edge::Right => cur.x >= m_right - 1 - a.edge_margin,
                Edge::Left => cur.x <= m_left + a.edge_margin,
                Edge::Top => cur.y <= m_top + a.edge_margin,
                Edge::Bottom => cur.y >= m_bottom - 1 - a.edge_margin,
            };
            // Dead corner: suppress switching near the perpendicular extremes so
            // a diagonal flick to a corner does not switch.
            let near_corner_for = |e: Edge| {
                a.dead_corner_px > 0
                    && match e {
                        Edge::Right | Edge::Left => {
                            cur.y <= m_top + a.dead_corner_px
                                || cur.y >= m_bottom - 1 - a.dead_corner_px
                        }
                        Edge::Top | Edge::Bottom => {
                            cur.x <= m_left + a.dead_corner_px
                                || cur.x >= m_right - 1 - a.dead_corner_px
                        }
                    }
            };
            let edge_allowed = |e: Edge| match a.peer_rect {
                Some(pr) => peer_adjacent(cur_mon, e, pr),
                None => {
                    let on_attach = match a.attach_monitor {
                        Some(m) => cur_mon == m,
                        None => true,
                    };
                    e == edge && on_attach && is_outer_edge(cur_mon, e, &a.local_monitors)
                }
            };
            // Physical-push gate: only an edge the user recently pushed toward
            // with relative HID deltas may cross (see `last_push`).
            let pushed = |e: Edge| {
                let idx = match e {
                    Edge::Right => 0,
                    Edge::Left => 1,
                    Edge::Top => 2,
                    Edge::Bottom => 3,
                };
                last_push[idx].is_some_and(|t| t.elapsed().as_millis() <= PUSH_FRESH_MS)
            };
            let cross_edge = [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom]
                .into_iter()
                .find(|&e| pressing(e) && pushed(e) && edge_allowed(e) && !near_corner_for(e));

            // Never enter the remote without a live session: with no link the
            // cursor would just park at the lock point until the link watchdog
            // returned it 1.5s later.
            let link_alive = a
                .sender_slot
                .lock()
                .ok()
                .is_some_and(|guard| guard.is_some());

            if switch_guard.update(cross_edge.is_some() && link_alive, false) {
                let cross = cross_edge.unwrap_or(edge);
                // Rebuild the combined space with the current monitor as the
                // local rect and the peer's latest real screen size, so the entry
                // mapping (and later the return placement) are correct.
                let peer_screen = {
                    let peer = a.tcp_state.lock().ok().and_then(|s| s.peer_name.clone());
                    peer.as_deref().and_then(|name| {
                        a.screen_sizes
                            .lock()
                            .ok()
                            .and_then(|m| m.get(name).cloned())
                    })
                };
                let (rw, rh) = peer_screen
                    .as_ref()
                    .map(|peer| (peer.width, peer.height))
                    .filter(|&(w, h)| w > 320 && h > 240)
                    .unwrap_or((a.remote_width, a.remote_height));
                peer_monitors = peer_screen.map(|peer| peer.monitors).unwrap_or_default();
                combined = CombinedSpace::new(
                    SsRect::new(m_left, m_top, m_right, m_bottom),
                    SsRect::new(0, 0, rw, rh),
                    cross,
                );
                state = combined.enter_remote_at(cur.x, cur.y);
                remote_active = true;
                anchor_local_rect = Some((m_left, m_top, m_right, m_bottom));
                anchor_remote_size = (rw, rh);
                // A stale link-loss timer from a previous remote period must
                // not instantly bounce this fresh entry back to local.
                link_down_since = None;
                if let Ok(mut remote_state) = a.remote_control.lock() {
                    remote_state.active = true;
                }

                let _ = start_mouse_hook_forwarding(
                    SenderTarget::Active(a.sender_slot.clone()),
                    a.tcp_state.clone(),
                    a.mouse_hook_running.clone(),
                    a.mouse_hook.clone(),
                    "auto",
                );
                if let Err(err) = start_keyboard_hook_forwarding(
                    &keyboard_ctx,
                    SenderTarget::Active(a.sender_slot.clone()),
                    "auto",
                ) {
                    update_tcp_state(&a.tcp_state, |snapshot| {
                        snapshot.last_event = format!("Keyboard forwarding failed to start: {err}");
                    });
                }

                // Clamp the entry point onto a real peer monitor before
                // announcing it (L-shaped peer layouts).
                let (entry_x, entry_y) =
                    tailkvm_win32::screen_space::clamp_to_rects(state.x, state.y, &peer_monitors);
                state.x = entry_x;
                state.y = entry_y;
                let _ = send_remote(WireMessage::MouseSetPosition {
                    x: state.x,
                    y: state.y,
                });
                publish_anchor(anchor_local_rect, anchor_remote_size, state.x, state.y);
                // Park and confine the local cursor so it cannot touch local UI
                // while the remote is controlled (released on every stop path).
                let _ = tailkvm_win32::cursor::set_cursor_position(a.lock_x, a.lock_y);
                let _ = tailkvm_win32::cursor::confine_cursor(a.lock_x, a.lock_y);

                update_tcp_state(&a.tcp_state, |snapshot| {
                    snapshot.last_event =
                        format!("Seamless: entered remote at x={}, y={}.", state.x, state.y);
                });
            }

            time::sleep(Duration::from_millis(a.interval_ms)).await;
            continue;
        }

        // Link watchdog (recovery route): the session died or was replaced
        // while controlling the remote. Hand control back to local input
        // instead of leaving the cursor parked + confined; the capture stays
        // armed, so once the supervisor reconnects, crossing works again.
        let remote_link_alive = a
            .sender_slot
            .lock()
            .ok()
            .is_some_and(|guard| guard.is_some());
        if !remote_link_alive && link_down_since.is_none() {
            link_down_since = Some(Instant::now());
        }
        if link_down_since.is_some_and(|t| t.elapsed() >= LINK_LOST_RETURN) {
            link_down_since = None;
            remote_active = false;
            clear_anchor();
            if let Ok(mut remote_state) = a.remote_control.lock() {
                remote_state.active = false;
            }
            let _ = stop_mouse_hook_forwarding(
                a.mouse_hook_running.clone(),
                a.mouse_hook.clone(),
                a.tcp_state.clone(),
                "link-lost",
            );
            let _ = stop_keyboard_hook_forwarding(
                a.keyboard_hook_running.clone(),
                a.keyboard_hook.clone(),
                a.tcp_state.clone(),
                "link-lost",
            );
            tailkvm_win32::cursor::release_cursor_confine();
            update_tcp_state(&a.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Seamless: peer link lost; control returned to local input (re-arms on reconnect)."
                        .to_string();
            });
            time::sleep(Duration::from_millis(a.interval_ms)).await;
            continue;
        }

        // Remote region: integrate raw deltas into the combined space.
        let mut acc_x = 0i32;
        let mut acc_y = 0i32;
        while let Ok((dx, dy)) = raw_rx.try_recv() {
            acc_x = acc_x.saturating_add(dx);
            acc_y = acc_y.saturating_add(dy);
        }

        // Scale raw HID deltas by the pointer-speed gain (with sub-pixel carry)
        // so controlling the remote feels as fast as the local cursor instead of
        // the raw 1:1 mapping, which is noticeably slow on a high-res local.
        let scaled_x = acc_x as f64 * gain + frac_x;
        let scaled_y = acc_y as f64 * gain + frac_y;
        let gain_x = scaled_x.trunc() as i32;
        let gain_y = scaled_y.trunc() as i32;
        frac_x = scaled_x - gain_x as f64;
        frac_y = scaled_y - gain_y as f64;

        if gain_x != 0 || gain_y != 0 {
            let (next, switched) = combined.apply_delta(state, gain_x, gain_y);
            state = next;

            if switched {
                // Returned to local: stop forwarding and place the real cursor.
                remote_active = false;
                clear_anchor();
                if let Ok(mut remote_state) = a.remote_control.lock() {
                    remote_state.active = false;
                }
                let _ = stop_mouse_hook_forwarding(
                    a.mouse_hook_running.clone(),
                    a.mouse_hook.clone(),
                    a.tcp_state.clone(),
                    "auto",
                );
                let _ = stop_keyboard_hook_forwarding(
                    a.keyboard_hook_running.clone(),
                    a.keyboard_hook.clone(),
                    a.tcp_state.clone(),
                    "auto",
                );
                tailkvm_win32::cursor::release_cursor_confine();
                let _ = tailkvm_win32::cursor::set_cursor_position(state.x, state.y);

                update_tcp_state(&a.tcp_state, |snapshot| {
                    snapshot.last_event = format!(
                        "Seamless: returned to local at x={}, y={}.",
                        state.x, state.y
                    );
                });
            } else {
                // Keep the logical cursor on a real peer monitor: the remote
                // rect is the peer's bounding box, which can contain dead
                // zones in an L-shaped layout where no cursor is visible.
                let (clamped_x, clamped_y) =
                    tailkvm_win32::screen_space::clamp_to_rects(state.x, state.y, &peer_monitors);
                state.x = clamped_x;
                state.y = clamped_y;
                publish_anchor(anchor_local_rect, anchor_remote_size, state.x, state.y);
                if send_remote(WireMessage::MouseSetPosition {
                    x: state.x,
                    y: state.y,
                }) {
                    // A successful send proves the link is live again (e.g.
                    // the supervisor reconnected and swapped in a fresh
                    // sender) — disarm the link watchdog.
                    link_down_since = None;
                } else if link_down_since.is_none() {
                    link_down_since = Some(Instant::now());
                }
                let _ = tailkvm_win32::cursor::set_cursor_position(a.lock_x, a.lock_y);
                sent_count += 1;
                if sent_count.is_multiple_of(30) {
                    update_tcp_state(&a.tcp_state, |snapshot| {
                        snapshot.role = "controller".to_string();
                        snapshot.connected = true;
                        snapshot.last_event = format!(
                            "Seamless remote active. sent={sent_count}, pos=({}, {}).",
                            state.x, state.y
                        );
                    });
                }
            }
        }

        time::sleep(Duration::from_millis(a.interval_ms)).await;
    }

    a.capture_running.store(false, Ordering::SeqCst);
    clear_anchor();
    // Always release the cursor clip so the local cursor is never stranded,
    // regardless of why the loop ended (failsafe, return, stop).
    tailkvm_win32::cursor::release_cursor_confine();
    let _ = stop_mouse_hook_forwarding(
        a.mouse_hook_running.clone(),
        a.mouse_hook.clone(),
        a.tcp_state.clone(),
        "auto",
    );
    let _ = stop_keyboard_hook_forwarding(
        a.keyboard_hook_running.clone(),
        a.keyboard_hook.clone(),
        a.tcp_state.clone(),
        "auto",
    );
    if let Ok(mut remote_state) = a.remote_control.lock() {
        remote_state.active = false;
    }

    update_tcp_state(&a.tcp_state, |snapshot| {
        snapshot.last_event = "Seamless capture stopped.".to_string();
    });
}

pub(crate) fn is_remote_return_edge(x: i32, y: i32, remote: &RemoteControlState) -> bool {
    let margin = remote.edge_margin.max(8);
    let width = remote.remote_width.max(1);
    let height = remote.remote_height.max(1);

    match remote.switch_edge.as_str() {
        // Local right -> remote enters from left, so remote left edge returns local.
        "right" => x <= margin,
        // Local left -> remote enters from right, so remote right edge returns local.
        "left" => x >= width - 1 - margin,
        // Local top -> remote enters from bottom, so remote bottom edge returns local.
        "top" => y >= height - 1 - margin,
        // Local bottom -> remote enters from top, so remote top edge returns local.
        "bottom" => y <= margin,
        _ => x <= margin,
    }
}

pub(crate) fn normalize_edge(edge: String) -> String {
    match edge.trim().to_lowercase().as_str() {
        "left" => "left".to_string(),
        "right" => "right".to_string(),
        "top" => "top".to_string(),
        "bottom" => "bottom".to_string(),
        _ => "right".to_string(),
    }
}

pub(crate) fn is_cursor_at_edge(
    position: &tailkvm_win32::cursor::CursorPosition,
    rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    margin: i32,
) -> bool {
    match edge {
        "left" => position.x <= rect.left + margin,
        "right" => position.x >= rect.right - 1 - margin,
        "top" => position.y <= rect.top + margin,
        "bottom" => position.y >= rect.bottom - 1 - margin,
        _ => position.x >= rect.right - 1 - margin,
    }
}

pub(crate) fn remote_entry_position(
    position: &tailkvm_win32::cursor::CursorPosition,
    local_rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    remote_width: i32,
    remote_height: i32,
) -> tailkvm_win32::cursor::CursorPosition {
    let inset = 4;

    match edge {
        "left" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: remote_width - 1 - inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "right" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "top" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: remote_height - 1 - inset,
            }
        }
        "bottom" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            tailkvm_win32::cursor::CursorPosition {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: inset,
            }
        }
        _ => tailkvm_win32::cursor::CursorPosition {
            x: inset,
            y: remote_height / 2,
        },
    }
}

pub(crate) fn local_return_position(
    position: &tailkvm_win32::cursor::CursorPosition,
    rect: &tailkvm_win32::monitor::RectI32,
    edge: &str,
    margin: i32,
) -> tailkvm_win32::cursor::CursorPosition {
    let safe_margin = margin.max(8);

    match edge {
        "left" => tailkvm_win32::cursor::CursorPosition {
            x: rect.left + safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "right" => tailkvm_win32::cursor::CursorPosition {
            x: rect.right - 1 - safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "top" => tailkvm_win32::cursor::CursorPosition {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.top + safe_margin,
        },
        "bottom" => tailkvm_win32::cursor::CursorPosition {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.bottom - 1 - safe_margin,
        },
        _ => tailkvm_win32::cursor::CursorPosition {
            x: rect.right - 1 - safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tailkvm_win32::cursor::CursorPosition;
    use tailkvm_win32::monitor::RectI32;

    /// Build a `RectI32` the same way `RectI32::new` does (its constructor is
    /// private to the monitor module, but the fields are public).
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RectI32 {
        RectI32 {
            left,
            top,
            right,
            bottom,
            width: right - left,
            height: bottom - top,
        }
    }

    fn pos(x: i32, y: i32) -> CursorPosition {
        CursorPosition { x, y }
    }

    #[test]
    fn normalize_edge_keeps_valid_and_defaults_to_right() {
        assert_eq!(normalize_edge("left".to_string()), "left");
        assert_eq!(normalize_edge("right".to_string()), "right");
        assert_eq!(normalize_edge("top".to_string()), "top");
        assert_eq!(normalize_edge("bottom".to_string()), "bottom");
        // Trimmed + case-insensitive.
        assert_eq!(normalize_edge("  RIGHT ".to_string()), "right");
        assert_eq!(normalize_edge("Top".to_string()), "top");
        // Unknown falls back to the default edge.
        assert_eq!(normalize_edge("diagonal".to_string()), "right");
        assert_eq!(normalize_edge(String::new()), "right");
    }

    #[test]
    fn is_cursor_at_edge_respects_margin_on_each_side() {
        let r = rect(0, 0, 1920, 1080);
        let margin = 3;

        // right edge: x >= right - 1 - margin = 1916
        assert!(is_cursor_at_edge(&pos(1916, 500), &r, "right", margin));
        assert!(!is_cursor_at_edge(&pos(1915, 500), &r, "right", margin));

        // left edge: x <= left + margin = 3
        assert!(is_cursor_at_edge(&pos(3, 500), &r, "left", margin));
        assert!(!is_cursor_at_edge(&pos(4, 500), &r, "left", margin));

        // top edge: y <= top + margin = 3
        assert!(is_cursor_at_edge(&pos(500, 3), &r, "top", margin));
        assert!(!is_cursor_at_edge(&pos(500, 4), &r, "top", margin));

        // bottom edge: y >= bottom - 1 - margin = 1076
        assert!(is_cursor_at_edge(&pos(500, 1076), &r, "bottom", margin));
        assert!(!is_cursor_at_edge(&pos(500, 1075), &r, "bottom", margin));
    }

    #[test]
    fn is_cursor_at_edge_handles_negative_origin_virtual_screen() {
        // Multi-monitor virtual screen whose primary is not at (0,0).
        let r = rect(-1920, -200, 1920, 1080);
        let margin = 3;

        // right edge: x >= 1920 - 1 - 3 = 1916
        assert!(is_cursor_at_edge(&pos(1916, 0), &r, "right", margin));
        assert!(!is_cursor_at_edge(&pos(1900, 0), &r, "right", margin));

        // left edge: x <= -1920 + 3 = -1917
        assert!(is_cursor_at_edge(&pos(-1917, 0), &r, "left", margin));
        assert!(!is_cursor_at_edge(&pos(-1916, 0), &r, "left", margin));
    }

    #[test]
    fn remote_entry_position_enters_opposite_edge_with_aspect_mapping() {
        let local = rect(0, 0, 1920, 1080);
        let (rw, rh) = (1280, 720);
        let inset = 4;

        // Exit local RIGHT -> enter remote LEFT (small x), y mapped by ratio.
        let entry = remote_entry_position(&pos(1919, 540), &local, "right", rw, rh);
        assert_eq!(entry.x, inset);
        // ratio = 540/1080 = 0.5 -> y = (720-1)*0.5 = 359.5 -> 360
        assert_eq!(entry.y, 360);

        // Exit local LEFT -> enter remote RIGHT (large x).
        let entry = remote_entry_position(&pos(0, 0), &local, "left", rw, rh);
        assert_eq!(entry.x, rw - 1 - inset);
        assert_eq!(entry.y, 0);

        // Exit local TOP -> enter remote BOTTOM (large y), x mapped by ratio.
        let entry = remote_entry_position(&pos(960, 0), &local, "top", rw, rh);
        assert_eq!(entry.y, rh - 1 - inset);
        // ratio = 960/1920 = 0.5 -> x = (1280-1)*0.5 = 639.5 -> 640
        assert_eq!(entry.x, 640);

        // Exit local BOTTOM -> enter remote TOP (small y).
        let entry = remote_entry_position(&pos(960, 1079), &local, "bottom", rw, rh);
        assert_eq!(entry.y, inset);
    }

    #[test]
    fn remote_entry_position_clamps_ratio_within_bounds() {
        // Cursor far below the rect should still map within [0, rh-1].
        let local = rect(0, 0, 1920, 1080);
        let entry = remote_entry_position(&pos(1919, 100_000), &local, "right", 1280, 720);
        assert!(
            entry.y >= 0 && entry.y <= 719,
            "y out of range: {}",
            entry.y
        );
    }

    #[test]
    fn local_return_position_uses_safe_margin_floor_of_8() {
        let r = rect(0, 0, 1920, 1080);

        // margin below 8 is bumped to 8.
        let ret = local_return_position(&pos(1919, 540), &r, "right", 3);
        assert_eq!(ret.x, 1920 - 1 - 8);
        assert!(ret.y >= 8 && ret.y <= 1080 - 1 - 8);

        let ret = local_return_position(&pos(0, 540), &r, "left", 3);
        assert_eq!(ret.x, 8);

        let ret = local_return_position(&pos(960, 0), &r, "top", 3);
        assert_eq!(ret.y, 8);

        let ret = local_return_position(&pos(960, 1079), &r, "bottom", 3);
        assert_eq!(ret.y, 1080 - 1 - 8);
    }

    #[test]
    fn is_remote_return_edge_mirrors_switch_edge() {
        let base = RemoteControlState {
            active: true,
            switch_edge: "right".to_string(),
            remote_width: 1920,
            remote_height: 1080,
            edge_margin: 3,
            seamless: false,
        };
        // margin floor is 8 inside the function.

        // Switch right -> entered remote from left -> return at remote LEFT edge.
        assert!(is_remote_return_edge(8, 500, &base));
        assert!(!is_remote_return_edge(9, 500, &base));

        // Switch left -> return at remote RIGHT edge: x >= width-1-8 = 1911.
        let left = RemoteControlState {
            switch_edge: "left".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(1911, 500, &left));
        assert!(!is_remote_return_edge(1910, 500, &left));

        // Switch top -> return at remote BOTTOM edge: y >= height-1-8 = 1071.
        let top = RemoteControlState {
            switch_edge: "top".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(500, 1071, &top));
        assert!(!is_remote_return_edge(500, 1070, &top));

        // Switch bottom -> return at remote TOP edge: y <= 8.
        let bottom = RemoteControlState {
            switch_edge: "bottom".to_string(),
            ..base.clone()
        };
        assert!(is_remote_return_edge(500, 8, &bottom));
        assert!(!is_remote_return_edge(500, 9, &bottom));
    }
}
