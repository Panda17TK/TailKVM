//! Pure combined-coordinate-space model for seamless, absolute-cursor
//! switching between the local screen and one remote screen (roadmap A1).
//!
//! Instead of warping the local cursor to a lock point and sending relative
//! deltas, the controller tracks a logical cursor that lives in *either* the
//! local or the remote screen's native coordinates. Relative deltas (from Raw
//! Input) move it; when it crosses the configured boundary it transfers to the
//! other screen at the proportionally-mapped position. The active screen is
//! then driven by absolute `MouseSetPosition`, which is drift-free and needs no
//! per-frame `GetCursorPos`/`SetCursorPos` polling.
//!
//! This module is pure and fully unit-tested; the Win32 wiring lives elsewhere.

/// Axis-aligned rectangle in a screen's native virtual coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> i32 {
        (self.right - self.left).max(1)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(1)
    }
}

/// Which screen the logical cursor currently occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Local,
    Remote,
}

/// Local screen edge that the remote screen is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn from_label(value: &str) -> Self {
        match value {
            "left" => Edge::Left,
            "top" => Edge::Top,
            "bottom" => Edge::Bottom,
            _ => Edge::Right,
        }
    }

    /// The edge on the opposite side, i.e. the side a neighbor attaches back on.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// Logical cursor position: a point in `region`'s native coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    pub region: Region,
    pub x: i32,
    pub y: i32,
}

/// The two-screen combined space and the edge that joins them.
#[derive(Debug, Clone, Copy)]
pub struct CombinedSpace {
    pub local: Rect,
    pub remote: Rect,
    pub edge: Edge,
    /// Inset applied at the entry side so the cursor does not immediately
    /// re-cross back on the next delta.
    pub inset: i32,
}

/// Map `value` from one 1-D range to another by proportion, clamped to
/// `[0,1]`. Shared by the 2-screen and N-screen models.
pub(crate) fn map_ratio(value: i32, from_lo: i32, from_len: i32, to_lo: i32, to_len: i32) -> i32 {
    let ratio = ((value - from_lo) as f64 / from_len.max(1) as f64).clamp(0.0, 1.0);
    to_lo + (ratio * (to_len - 1).max(0) as f64).round() as i32
}

impl CombinedSpace {
    pub fn new(local: Rect, remote: Rect, edge: Edge) -> Self {
        Self {
            local,
            remote,
            edge,
            inset: 2,
        }
    }

    fn map_ratio(value: i32, from_lo: i32, from_len: i32, to_lo: i32, to_len: i32) -> i32 {
        let ratio = ((value - from_lo) as f64 / from_len as f64).clamp(0.0, 1.0);
        to_lo + (ratio * (to_len - 1).max(0) as f64).round() as i32
    }

    fn clamp_local(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.local.left, self.local.right - 1),
            y.clamp(self.local.top, self.local.bottom - 1),
        )
    }

    fn clamp_remote(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.remote.left, self.remote.right - 1),
            y.clamp(self.remote.top, self.remote.bottom - 1),
        )
    }

    /// Compute the remote entry state when the local cursor crosses at
    /// `(local_x, local_y)` (used when the local region follows the real cursor
    /// and detects the edge).
    pub fn enter_remote_at(&self, local_x: i32, local_y: i32) -> CursorState {
        let (x, y) = self.enter_remote(local_y, local_x);
        CursorState {
            region: Region::Remote,
            x,
            y,
        }
    }

    /// Compute the local entry state when returning from the remote at
    /// `(remote_x, remote_y)`.
    pub fn enter_local_at(&self, remote_x: i32, remote_y: i32) -> CursorState {
        let (x, y) = self.enter_local(remote_y, remote_x);
        CursorState {
            region: Region::Local,
            x,
            y,
        }
    }

    /// Apply a relative delta. Returns the new state and `true` if the cursor
    /// crossed to the other screen (a switch).
    pub fn apply_delta(&self, state: CursorState, dx: i32, dy: i32) -> (CursorState, bool) {
        let nx = state.x + dx;
        let ny = state.y + dy;

        match state.region {
            Region::Local => {
                if self.crosses_local_to_remote(nx, ny) {
                    let (rx, ry) = self.enter_remote(ny, nx);
                    (
                        CursorState {
                            region: Region::Remote,
                            x: rx,
                            y: ry,
                        },
                        true,
                    )
                } else {
                    let (cx, cy) = self.clamp_local(nx, ny);
                    (
                        CursorState {
                            region: Region::Local,
                            x: cx,
                            y: cy,
                        },
                        false,
                    )
                }
            }
            Region::Remote => {
                if self.crosses_remote_to_local(nx, ny) {
                    let (lx, ly) = self.enter_local(ny, nx);
                    (
                        CursorState {
                            region: Region::Local,
                            x: lx,
                            y: ly,
                        },
                        true,
                    )
                } else {
                    let (cx, cy) = self.clamp_remote(nx, ny);
                    (
                        CursorState {
                            region: Region::Remote,
                            x: cx,
                            y: cy,
                        },
                        false,
                    )
                }
            }
        }
    }

    fn crosses_local_to_remote(&self, nx: i32, ny: i32) -> bool {
        match self.edge {
            Edge::Right => nx >= self.local.right,
            Edge::Left => nx < self.local.left,
            Edge::Top => ny < self.local.top,
            Edge::Bottom => ny >= self.local.bottom,
        }
    }

    fn crosses_remote_to_local(&self, nx: i32, ny: i32) -> bool {
        // The remote was entered from the side opposite the local edge, so the
        // return boundary is that same entry side.
        match self.edge {
            Edge::Right => nx < self.remote.left,
            Edge::Left => nx >= self.remote.right,
            Edge::Top => ny >= self.remote.bottom,
            Edge::Bottom => ny < self.remote.top,
        }
    }

    /// Position to enter the remote screen at, given the local perpendicular
    /// coordinate at the crossing.
    fn enter_remote(&self, local_y: i32, local_x: i32) -> (i32, i32) {
        match self.edge {
            Edge::Right => (
                self.remote.left + self.inset,
                Self::map_ratio(
                    local_y,
                    self.local.top,
                    self.local.height(),
                    self.remote.top,
                    self.remote.height(),
                ),
            ),
            Edge::Left => (
                self.remote.right - 1 - self.inset,
                Self::map_ratio(
                    local_y,
                    self.local.top,
                    self.local.height(),
                    self.remote.top,
                    self.remote.height(),
                ),
            ),
            Edge::Top => (
                Self::map_ratio(
                    local_x,
                    self.local.left,
                    self.local.width(),
                    self.remote.left,
                    self.remote.width(),
                ),
                self.remote.bottom - 1 - self.inset,
            ),
            Edge::Bottom => (
                Self::map_ratio(
                    local_x,
                    self.local.left,
                    self.local.width(),
                    self.remote.left,
                    self.remote.width(),
                ),
                self.remote.top + self.inset,
            ),
        }
    }

    /// Position to return to the local screen at, given the remote perpendicular
    /// coordinate at the crossing.
    fn enter_local(&self, remote_y: i32, remote_x: i32) -> (i32, i32) {
        match self.edge {
            Edge::Right => (
                self.local.right - 1 - self.inset,
                Self::map_ratio(
                    remote_y,
                    self.remote.top,
                    self.remote.height(),
                    self.local.top,
                    self.local.height(),
                ),
            ),
            Edge::Left => (
                self.local.left + self.inset,
                Self::map_ratio(
                    remote_y,
                    self.remote.top,
                    self.remote.height(),
                    self.local.top,
                    self.local.height(),
                ),
            ),
            Edge::Top => (
                Self::map_ratio(
                    remote_x,
                    self.remote.left,
                    self.remote.width(),
                    self.local.left,
                    self.local.width(),
                ),
                self.local.top + self.inset,
            ),
            Edge::Bottom => (
                Self::map_ratio(
                    remote_x,
                    self.remote.left,
                    self.remote.width(),
                    self.local.left,
                    self.local.width(),
                ),
                self.local.bottom - 1 - self.inset,
            ),
        }
    }
}

/// Debounces edge switching to avoid accidental crossings (roadmap C1).
///
/// Driven once per capture tick. A switch fires only after the cursor has dwelt
/// at the switch edge for `dwell_ms` (accumulated across ticks), and never while
/// it is within a dead corner. `dwell_ms == 0` fires on the first at-edge tick
/// (instant, the legacy behavior).
#[derive(Debug, Clone, Copy)]
pub struct SwitchGuard {
    dwell_ms: u64,
    interval_ms: u64,
    accumulated_ms: u64,
}

impl SwitchGuard {
    pub fn new(dwell_ms: u64, interval_ms: u64) -> Self {
        Self {
            dwell_ms,
            interval_ms: interval_ms.max(1),
            accumulated_ms: 0,
        }
    }

    /// Call once per tick with the current edge/corner state. Returns `true`
    /// when a switch should fire (and resets the accumulator).
    pub fn update(&mut self, at_edge: bool, near_corner: bool) -> bool {
        if !at_edge || near_corner {
            self.accumulated_ms = 0;
            return false;
        }
        self.accumulated_ms = self.accumulated_ms.saturating_add(self.interval_ms);
        if self.accumulated_ms >= self.dwell_ms {
            self.accumulated_ms = 0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> CombinedSpace {
        // Local 1920x1080 at origin; remote 1280x720 at its own origin; remote
        // attached to the local RIGHT edge.
        CombinedSpace::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            Edge::Right,
        )
    }

    #[test]
    fn moves_within_local_without_switching() {
        let s = space();
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Local,
                x: 100,
                y: 100,
            },
            50,
            -30,
        );
        assert!(!switched);
        assert_eq!((st.region, st.x, st.y), (Region::Local, 150, 70));
    }

    #[test]
    fn clamps_at_outer_local_edge() {
        let s = space();
        // Moving left past the local left edge does not cross (remote is on the
        // right); it clamps.
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Local,
                x: 5,
                y: 500,
            },
            -100,
            0,
        );
        assert!(!switched);
        assert_eq!((st.region, st.x), (Region::Local, 0));
    }

    #[test]
    fn crosses_right_edge_into_remote_with_mapped_y() {
        let s = space();
        // At local y=540 (mid), crossing right -> remote near left, y mid.
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Local,
                x: 1919,
                y: 540,
            },
            5,
            0,
        );
        assert!(switched);
        assert_eq!(st.region, Region::Remote);
        assert_eq!(st.x, 2); // remote.left + inset
                             // ratio 540/1080 = 0.5 -> (720-1)*0.5 = 359.5 -> 360
        assert_eq!(st.y, 360);
    }

    #[test]
    fn returns_from_remote_left_edge_to_local() {
        let s = space();
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Remote,
                x: 0,
                y: 360,
            },
            -5,
            0,
        );
        assert!(switched);
        assert_eq!(st.region, Region::Local);
        assert_eq!(st.x, 1920 - 1 - 2); // local.right - 1 - inset
    }

    #[test]
    fn remote_outer_edge_clamps_not_crosses() {
        let s = space();
        // Moving further right while already in remote clamps at remote right.
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Remote,
                x: 1279,
                y: 100,
            },
            50,
            0,
        );
        assert!(!switched);
        assert_eq!((st.region, st.x), (Region::Remote, 1279));
    }

    #[test]
    fn switch_guard_instant_when_no_dwell() {
        let mut guard = SwitchGuard::new(0, 33);
        assert!(guard.update(true, false)); // fires immediately
    }

    #[test]
    fn switch_guard_requires_dwell_and_resets() {
        let mut guard = SwitchGuard::new(100, 33);
        assert!(!guard.update(true, false)); // 33ms
        assert!(!guard.update(true, false)); // 66ms
        assert!(!guard.update(true, false)); // 99ms
        assert!(guard.update(true, false)); // 132ms >= 100 -> fire
                                            // accumulator reset; needs to dwell again
        assert!(!guard.update(true, false)); // 33ms
    }

    #[test]
    fn switch_guard_dead_corner_and_leaving_reset_dwell() {
        let mut guard = SwitchGuard::new(100, 50);
        assert!(!guard.update(true, false)); // 50ms
        assert!(!guard.update(true, true)); // near corner -> reset
        assert!(!guard.update(true, false)); // 50ms again (not 100)
        assert!(guard.update(true, false)); // 100ms -> fire
    }

    #[test]
    fn switch_guard_leaving_edge_resets() {
        let mut guard = SwitchGuard::new(100, 60);
        assert!(!guard.update(true, false)); // 60ms
        assert!(!guard.update(false, false)); // left edge -> reset
        assert!(!guard.update(true, false)); // 60ms
        assert!(guard.update(true, false)); // 120ms -> fire
    }

    #[test]
    fn top_edge_layout_crosses_vertically() {
        let s = CombinedSpace::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            Edge::Top,
        );
        let (st, switched) = s.apply_delta(
            CursorState {
                region: Region::Local,
                x: 960,
                y: 0,
            },
            0,
            -5,
        );
        assert!(switched);
        assert_eq!(st.region, Region::Remote);
        assert_eq!(st.y, 720 - 1 - 2); // enter near remote bottom
    }
}
