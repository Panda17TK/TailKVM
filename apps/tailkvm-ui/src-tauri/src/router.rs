//! Multi-screen router (roadmap B1.4).
//!
//! Fourth slice of the lib.rs decomposition (#17): `RouterConfig`/`RouterArgs`,
//! `run_router`, `build_multi_screen_space`, and the router Tauri commands.
//! Everything is `pub(crate)`: internal plumbing, not crate API.

use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tailkvm_net::protocol::WireMessage;
use tauri::State;
use tokio::{
    sync::mpsc,
    time::{self, Duration},
};

use crate::forwarding::{
    start_keyboard_hook_forwarding, start_mouse_hook_forwarding, stop_keyboard_hook_forwarding,
    stop_mouse_hook_forwarding, SenderTarget,
};
use crate::state::*;
#[derive(Debug, Deserialize)]
pub(crate) struct RouterScreen {
    name: String,
    width: i32,
    height: i32,
    is_local: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RouterLink {
    from: String,
    edge: String,
    to: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RouterConfig {
    screens: Vec<RouterScreen>,
    links: Vec<RouterLink>,
}

/// Owned inputs for the multi-screen router (roadmap B1.4).
struct RouterArgs {
    tcp_state: Arc<Mutex<TcpSessionSnapshot>>,
    router_running: Arc<AtomicBool>,
    sessions: Arc<Mutex<HashMap<String, ScreenSession>>>,
    /// Peer screen geometry (ScreenInfo) keyed by screen/machine name, used to
    /// clamp the logical cursor onto each peer's real monitors (#7).
    screen_sizes: PeerScreenMap,
    remote_control: Arc<Mutex<RemoteControlState>>,
    resolve_characters: Arc<AtomicBool>,
    /// Japanese-IME settings, forwarded into the keyboard context.
    ime_settings: Arc<Mutex<ImeSettings>>,
    /// Candidate-anchor slot: while a remote screen is controlled the router
    /// publishes the remote cursor projected onto the local screen rect
    /// (IME-POS-011); cleared on every return-to-local path.
    ime_anchor: ImeAnchorSlot,
    mouse_hook_running: Arc<AtomicBool>,
    mouse_hook: Arc<Mutex<Option<tailkvm_win32::mouse_hook::MouseHookHandle>>>,
    keyboard_hook_running: Arc<AtomicBool>,
    keyboard_hook: Arc<Mutex<Option<tailkvm_win32::keyboard_hook::KeyboardHookHandle>>>,
    router_space: Arc<Mutex<Option<Arc<tailkvm_win32::layout_graph::MultiScreenSpace>>>>,
    local_name: String,
    lock_x: i32,
    lock_y: i32,
    interval_ms: u64,
    edge_margin: i32,
    edge_dwell_ms: u64,
    dead_corner_px: i32,
}

/// Resolve the current outbound sender for a named screen session.
fn screen_sender(
    sessions: &Arc<Mutex<HashMap<String, ScreenSession>>>,
    name: &str,
) -> Option<mpsc::UnboundedSender<WireMessage>> {
    let map = sessions.lock().ok()?;
    let session = map.get(name)?;
    let tx = session.tx.lock().ok()?;
    tx.clone()
}

/// Multi-screen router (roadmap B1.4). Owns the logical cursor across N screens
/// via `MultiScreenSpace`; in the local screen it follows the real cursor and
/// watches edges, and on a remote screen it integrates Raw Input deltas and
/// sends absolute `MouseSetPosition` to that screen's session. Hooks
/// (click/wheel/key) are installed only while controlling a remote and target
/// the active session via `SenderTarget::Active`, so remote->remote switches do
/// not restart them. Opt-in; runtime-unvalidated PoC (needs 3 machines).
async fn run_router(args: RouterArgs) {
    use tailkvm_win32::layout_graph::ScreenCursor;
    use tailkvm_win32::screen_space::{Edge, SwitchGuard};

    let active_slot: Arc<Mutex<Option<mpsc::UnboundedSender<WireMessage>>>> =
        Arc::new(Mutex::new(None));
    let mut switch_guard = SwitchGuard::new(args.edge_dwell_ms, args.interval_ms);

    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<(i32, i32)>();
    let _raw_handle = match tailkvm_win32::raw_input_mouse::start_raw_mouse_capture(raw_tx) {
        Ok(handle) => handle,
        Err(err) => {
            args.router_running.store(false, Ordering::SeqCst);
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event = format!("Router needs Raw Input, unavailable: {err}");
            });
            return;
        }
    };

    let keyboard_ctx = KeyboardForwardingContext {
        tcp_state: args.tcp_state.clone(),
        keyboard_hook_running: args.keyboard_hook_running.clone(),
        keyboard_hook: args.keyboard_hook.clone(),
        // The keyboard failsafe clears this; point it at the router so
        // Ctrl+Alt+Pause stops the router loop too.
        capture_running: args.router_running.clone(),
        mouse_hook_running: args.mouse_hook_running.clone(),
        mouse_hook: args.mouse_hook.clone(),
        remote_control: args.remote_control.clone(),
        resolve_characters: args.resolve_characters.clone(),
        ime_settings: args.ime_settings.clone(),
        ime_anchor: args.ime_anchor.clone(),
    };

    let mut active = args.local_name.clone();
    let mut cursor = ScreenCursor {
        screen: args.local_name.clone(),
        x: args.lock_x,
        y: args.lock_y,
    };

    // IME candidate anchor (IME-POS-011): while a remote screen is active,
    // publish the cursor projected onto the local screen rect; None returns
    // the forwarding loop to its lock_near fallback.
    let set_anchor = |value: Option<(i32, i32)>| {
        if let Ok(mut slot) = args.ime_anchor.lock() {
            *slot = value;
        }
    };

    let start_hooks = || {
        let _ = start_mouse_hook_forwarding(
            SenderTarget::Active(active_slot.clone()),
            args.tcp_state.clone(),
            args.mouse_hook_running.clone(),
            args.mouse_hook.clone(),
            "router",
        );
        let _ = start_keyboard_hook_forwarding(
            &keyboard_ctx,
            SenderTarget::Active(active_slot.clone()),
            "router",
        );
    };
    let stop_hooks = || {
        let _ = stop_mouse_hook_forwarding(
            args.mouse_hook_running.clone(),
            args.mouse_hook.clone(),
            args.tcp_state.clone(),
            "router",
        );
        let _ = stop_keyboard_hook_forwarding(
            args.keyboard_hook_running.clone(),
            args.keyboard_hook.clone(),
            args.tcp_state.clone(),
            "router",
        );
    };

    update_tcp_state(&args.tcp_state, |snapshot| {
        snapshot.last_event = format!(
            "Multi-screen router armed. Local screen '{}'.",
            args.local_name
        );
    });

    // Recovery-route state (#7: parity with the 1:1 seamless engine): the
    // physical-push gate (crossing requires fresh relative HID deltas, so a
    // peer controlling THIS machine cannot false-trigger our edges) and the
    // link watchdog (a dead session returns control to local input).
    let mut last_push: [Option<Instant>; 4] = [None; 4]; // Right, Left, Top, Bottom
    const PUSH_FRESH_MS: u128 = 250;
    let mut link_down_since: Option<Instant> = None;
    const LINK_LOST_RETURN: Duration = Duration::from_millis(1500);
    let peer_monitors_for = |name: &str| -> Vec<(i32, i32, i32, i32)> {
        args.screen_sizes
            .lock()
            .ok()
            .and_then(|m| m.get(name).map(|peer| peer.monitors.clone()))
            .unwrap_or_default()
    };

    while args.router_running.load(Ordering::SeqCst) {
        if tailkvm_win32::cursor::is_ctrl_alt_pause_pressed() {
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event = "Router stopped by Ctrl+Alt+Pause failsafe.".to_string();
            });
            break;
        }

        // Snapshot the live screen space (issue 1: reconfigure swaps it).
        let space = match args.router_space.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        };
        let Some(space) = space else {
            // No space configured (stopped or cleared) — end the router.
            break;
        };

        // If a reconfigure removed the screen we were controlling, fall back to
        // local so we never read an inconsistent / missing screen.
        if active != args.local_name && space.rect(&active).is_none() {
            if let Ok(mut slot) = active_slot.lock() {
                *slot = None;
            }
            if let Ok(mut remote_state) = args.remote_control.lock() {
                remote_state.active = false;
            }
            stop_hooks();
            tailkvm_win32::cursor::release_cursor_confine();
            set_anchor(None);
            active = args.local_name.clone();
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Router: active screen removed by reconfigure; returned to local.".to_string();
            });
        }

        if active == args.local_name {
            let cur = match tailkvm_win32::cursor::get_cursor_position() {
                Ok(position) => position,
                Err(_) => {
                    time::sleep(Duration::from_millis(args.interval_ms)).await;
                    continue;
                }
            };
            // Drain deltas while local, remembering when the user last
            // physically pushed in each direction (crossing gate below).
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
            let pushed = |e: Edge| {
                let idx = match e {
                    Edge::Right => 0,
                    Edge::Left => 1,
                    Edge::Top => 2,
                    Edge::Bottom => 3,
                };
                last_push[idx].is_some_and(|t| t.elapsed().as_millis() <= PUSH_FRESH_MS)
            };

            let Some(lr) = space.rect(&args.local_name).copied() else {
                break;
            };
            let m = args.edge_margin;
            let edge = [Edge::Right, Edge::Left, Edge::Top, Edge::Bottom]
                .into_iter()
                .find(|&edge| {
                    let at = match edge {
                        Edge::Right => cur.x >= lr.right - 1 - m,
                        Edge::Left => cur.x <= lr.left + m,
                        Edge::Top => cur.y <= lr.top + m,
                        Edge::Bottom => cur.y >= lr.bottom - 1 - m,
                    };
                    at && pushed(edge) && space.neighbor(&args.local_name, edge).is_some()
                });

            // Debounce switching with dwell + dead corner (roadmap C1 applied
            // to the router).
            let near_corner = match edge {
                Some(Edge::Right) | Some(Edge::Left) => {
                    args.dead_corner_px > 0
                        && (cur.y <= lr.top + args.dead_corner_px
                            || cur.y >= lr.bottom - 1 - args.dead_corner_px)
                }
                Some(Edge::Top) | Some(Edge::Bottom) => {
                    args.dead_corner_px > 0
                        && (cur.x <= lr.left + args.dead_corner_px
                            || cur.x >= lr.right - 1 - args.dead_corner_px)
                }
                None => false,
            };
            let fire = switch_guard.update(edge.is_some(), near_corner);

            if let Some(edge) = edge.filter(|_| fire) {
                if let Some(entry) = space.enter_neighbor(&args.local_name, edge, cur.x, cur.y) {
                    active = entry.screen.clone();
                    cursor = entry;
                    // Clamp the entry point onto the peer's real monitors
                    // (L-shaped layouts) and disarm any stale link-loss timer.
                    let mons = peer_monitors_for(&active);
                    let (cx, cy) =
                        tailkvm_win32::screen_space::clamp_to_rects(cursor.x, cursor.y, &mons);
                    cursor.x = cx;
                    cursor.y = cy;
                    link_down_since = None;
                    set_anchor(space.rect(&args.local_name).zip(space.rect(&active)).map(
                        |(local, remote)| {
                            tailkvm_win32::ime_anchor::project_remote_to_local(
                                cursor.x,
                                cursor.y,
                                remote.right - remote.left,
                                remote.bottom - remote.top,
                                (local.left, local.top, local.right, local.bottom),
                            )
                        },
                    ));
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = screen_sender(&args.sessions, &active);
                    }
                    if let Ok(mut remote_state) = args.remote_control.lock() {
                        remote_state.active = true;
                    }
                    start_hooks();
                    let _ = tailkvm_win32::cursor::set_cursor_position(args.lock_x, args.lock_y);
                    let _ = tailkvm_win32::cursor::confine_cursor(args.lock_x, args.lock_y);
                    if let Some(sender) = screen_sender(&args.sessions, &active) {
                        let _ = sender.send(WireMessage::MouseSetPosition {
                            x: cursor.x,
                            y: cursor.y,
                        });
                    }
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Router: control moved to screen '{}' at x={}, y={}.",
                            active, cursor.x, cursor.y
                        );
                    });
                }
            }

            time::sleep(Duration::from_millis(args.interval_ms)).await;
            continue;
        }

        // Link watchdog (#7): the active screen's session died (sender slot
        // empty). Return control to local input instead of leaving the cursor
        // parked + confined while moves go nowhere; the session supervisor
        // keeps reconnecting, so crossing works again once the peer is back.
        let link_alive = screen_sender(&args.sessions, &active).is_some();
        if link_alive {
            link_down_since = None;
        } else if link_down_since.is_none() {
            link_down_since = Some(Instant::now());
        }
        if link_down_since.is_some_and(|t| t.elapsed() >= LINK_LOST_RETURN) {
            link_down_since = None;
            active = args.local_name.clone();
            if let Ok(mut slot) = active_slot.lock() {
                *slot = None;
            }
            if let Ok(mut remote_state) = args.remote_control.lock() {
                remote_state.active = false;
            }
            stop_hooks();
            tailkvm_win32::cursor::release_cursor_confine();
            set_anchor(None);
            update_tcp_state(&args.tcp_state, |snapshot| {
                snapshot.last_event =
                    "Router: screen link lost; control returned to local input.".to_string();
            });
            time::sleep(Duration::from_millis(args.interval_ms)).await;
            continue;
        }

        // Active is a remote screen: integrate raw deltas.
        let mut acc_x = 0i32;
        let mut acc_y = 0i32;
        while let Ok((dx, dy)) = raw_rx.try_recv() {
            acc_x = acc_x.saturating_add(dx);
            acc_y = acc_y.saturating_add(dy);
        }

        if acc_x != 0 || acc_y != 0 {
            let (next, switch) = space.apply_delta(cursor.clone(), acc_x, acc_y);
            cursor = next;

            if switch.is_some() {
                if cursor.screen == args.local_name {
                    active = args.local_name.clone();
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = None;
                    }
                    if let Ok(mut remote_state) = args.remote_control.lock() {
                        remote_state.active = false;
                    }
                    stop_hooks();
                    tailkvm_win32::cursor::release_cursor_confine();
                    set_anchor(None);
                    let _ = tailkvm_win32::cursor::set_cursor_position(cursor.x, cursor.y);
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event = format!(
                            "Router: returned to local at x={}, y={}.",
                            cursor.x, cursor.y
                        );
                    });
                } else {
                    // remote -> remote: swap the active target, keep hooks.
                    active = cursor.screen.clone();
                    let mons = peer_monitors_for(&active);
                    let (cx, cy) =
                        tailkvm_win32::screen_space::clamp_to_rects(cursor.x, cursor.y, &mons);
                    cursor.x = cx;
                    cursor.y = cy;
                    if let Ok(mut slot) = active_slot.lock() {
                        *slot = screen_sender(&args.sessions, &active);
                    }
                    if let Some(sender) = screen_sender(&args.sessions, &active) {
                        let _ = sender.send(WireMessage::MouseSetPosition {
                            x: cursor.x,
                            y: cursor.y,
                        });
                    }
                    // remote -> remote switch: update the IME anchor to the
                    // new screen's projection (IME-POS-021).
                    set_anchor(space.rect(&args.local_name).zip(space.rect(&active)).map(
                        |(local, remote)| {
                            tailkvm_win32::ime_anchor::project_remote_to_local(
                                cursor.x,
                                cursor.y,
                                remote.right - remote.left,
                                remote.bottom - remote.top,
                                (local.left, local.top, local.right, local.bottom),
                            )
                        },
                    ));
                    update_tcp_state(&args.tcp_state, |snapshot| {
                        snapshot.last_event =
                            format!("Router: control moved to screen '{active}'.");
                    });
                }
            } else if let Some(sender) = screen_sender(&args.sessions, &active) {
                // Keep the logical cursor on the peer's real monitors: the
                // screen rect is a bounding box that can contain dead zones.
                let mons = peer_monitors_for(&active);
                let (cx, cy) =
                    tailkvm_win32::screen_space::clamp_to_rects(cursor.x, cursor.y, &mons);
                cursor.x = cx;
                cursor.y = cy;
                let _ = sender.send(WireMessage::MouseSetPosition {
                    x: cursor.x,
                    y: cursor.y,
                });
                set_anchor(space.rect(&args.local_name).zip(space.rect(&active)).map(
                    |(local, remote)| {
                        tailkvm_win32::ime_anchor::project_remote_to_local(
                            cursor.x,
                            cursor.y,
                            remote.right - remote.left,
                            remote.bottom - remote.top,
                            (local.left, local.top, local.right, local.bottom),
                        )
                    },
                ));
                let _ = tailkvm_win32::cursor::set_cursor_position(args.lock_x, args.lock_y);
            }
        }

        time::sleep(Duration::from_millis(args.interval_ms)).await;
    }

    args.router_running.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = args.router_space.lock() {
        *slot = None;
    }
    tailkvm_win32::cursor::release_cursor_confine();
    stop_hooks();
    set_anchor(None);
    if let Ok(mut remote_state) = args.remote_control.lock() {
        remote_state.active = false;
    }
    update_tcp_state(&args.tcp_state, |snapshot| {
        snapshot.last_event = "Multi-screen router stopped.".to_string();
    });
}

/// Start the multi-screen router from a layout config (B1.4). Screens named in
/// the config must already be connected via `connect_screen` (except the local
/// screen). Opt-in; legacy modes untouched.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) async fn start_multi_screen_router(
    config: RouterConfig,
    interval_ms: Option<u64>,
    edge_margin: Option<i32>,
    edge_dwell_ms: Option<u64>,
    dead_corner_px: Option<i32>,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    let local_name = config
        .screens
        .iter()
        .find(|screen| screen.is_local)
        .map(|screen| screen.name.clone())
        .ok_or_else(|| "config must include exactly one local screen.".to_string())?;

    let (space, lock_x, lock_y) = build_multi_screen_space(&config, &state.screen_sizes)?;

    if state.router_running.swap(true, Ordering::SeqCst) {
        update_tcp_state(&state.tcp, |snapshot| {
            snapshot.last_event = "Multi-screen router is already running.".to_string();
        });
        return Ok(tcp_snapshot(&state.tcp));
    }

    if let Ok(mut slot) = state.router_space.lock() {
        *slot = Some(Arc::new(space));
    }
    if let Ok(mut name) = state.router_local_name.lock() {
        *name = Some(local_name.clone());
    }

    let args = RouterArgs {
        tcp_state: state.tcp.clone(),
        router_running: state.router_running.clone(),
        sessions: state.sessions.clone(),
        screen_sizes: state.screen_sizes.clone(),
        remote_control: state.remote_control.clone(),
        resolve_characters: state.resolve_characters.clone(),
        ime_settings: state.ime_settings.clone(),
        ime_anchor: state.ime_anchor.clone(),
        mouse_hook_running: state.mouse_hook_running.clone(),
        mouse_hook: state.mouse_hook.clone(),
        keyboard_hook_running: state.keyboard_hook_running.clone(),
        keyboard_hook: state.keyboard_hook.clone(),
        router_space: state.router_space.clone(),
        local_name,
        lock_x,
        lock_y,
        interval_ms: interval_ms.unwrap_or(33).clamp(8, 100),
        edge_margin: edge_margin.unwrap_or(3).clamp(1, 64),
        edge_dwell_ms: edge_dwell_ms.unwrap_or(0).min(2000),
        dead_corner_px: dead_corner_px.unwrap_or(0).clamp(0, 1000),
    };

    tauri::async_runtime::spawn(run_router(args));

    Ok(tcp_snapshot(&state.tcp))
}

/// Build the `MultiScreenSpace` from a layout config, re-fetching the *live*
/// monitor topology (so a reconfigure picks up DPI / resolution / monitor
/// changes) and preferring peer-reported sizes (B1.7). Returns the space and
/// the local lock point. Pure of side effects on AppState.
fn build_multi_screen_space(
    config: &RouterConfig,
    screen_sizes: &PeerScreenMap,
) -> Result<(tailkvm_win32::layout_graph::MultiScreenSpace, i32, i32), String> {
    use tailkvm_win32::layout_graph::{LayoutGraph, MultiScreenSpace};
    use tailkvm_win32::screen_space::{Edge, Rect as SsRect};

    if !config.screens.iter().any(|screen| screen.is_local) {
        return Err("config must include exactly one local screen.".to_string());
    }

    let topology = tailkvm_win32::monitor::get_monitor_topology()
        .map_err(|err| format!("failed to get monitor topology: {err}"))?;
    let vs = &topology.virtual_screen;
    let lock_x = vs.left + (vs.width / 2);
    let lock_y = vs.top + (vs.height / 2);

    let reported = screen_sizes
        .lock()
        .map(|sizes| sizes.clone())
        .unwrap_or_default();

    let mut screens = HashMap::new();
    for screen in &config.screens {
        let rect = if screen.is_local {
            SsRect::new(vs.left, vs.top, vs.right, vs.bottom)
        } else if let Some(peer) = reported.get(&screen.name) {
            SsRect::new(0, 0, peer.width.max(320), peer.height.max(240))
        } else {
            SsRect::new(0, 0, screen.width.max(320), screen.height.max(240))
        };
        screens.insert(screen.name.clone(), rect);
    }

    let mut graph = LayoutGraph::new();
    for link in &config.links {
        graph.link(&link.from, Edge::from_label(&link.edge), &link.to);
    }

    Ok((MultiScreenSpace::new(screens, graph), lock_x, lock_y))
}

/// Rebuild and atomically swap the running router's screen space without
/// restarting it (issue 1). Re-fetches monitor topology, so monitor/DPI/
/// resolution/layout changes apply live. On failure the old space is kept and
/// an error is returned. The local screen name must be preserved.
#[tauri::command]
pub(crate) async fn reconfigure_router(
    config: RouterConfig,
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    if !state.router_running.load(Ordering::SeqCst) {
        return Err("router is not running; use start instead.".to_string());
    }

    let current_local = state
        .router_local_name
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    if let Some(local) = &current_local {
        let new_local = config
            .screens
            .iter()
            .find(|screen| screen.is_local)
            .map(|screen| screen.name.clone());
        if new_local.as_deref() != Some(local.as_str()) {
            return Err(format!("reconfigure must keep the local screen '{local}'."));
        }
    }

    // Build first; only swap on success so the live router never sees a
    // half-built or failed space.
    let (space, _lock_x, _lock_y) = build_multi_screen_space(&config, &state.screen_sizes)?;
    if let Ok(mut slot) = state.router_space.lock() {
        *slot = Some(Arc::new(space));
    }

    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Router reconfigured live (no restart).".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}

/// Stop the multi-screen router (B1.4).
#[tauri::command]
pub(crate) async fn stop_multi_screen_router(
    state: State<'_, AppState>,
) -> Result<TcpSessionSnapshot, String> {
    state.router_running.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = state.router_space.lock() {
        *slot = None;
    }
    if let Ok(mut name) = state.router_local_name.lock() {
        *name = None;
    }
    update_tcp_state(&state.tcp, |snapshot| {
        snapshot.last_event = "Multi-screen router stop requested.".to_string();
    });
    Ok(tcp_snapshot(&state.tcp))
}
