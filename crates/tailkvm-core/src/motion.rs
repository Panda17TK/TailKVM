//! Shared motion-gating logic for the capture engines.
//!
//! The 1:1 seamless engine and the multi-screen router independently reimplement
//! the *physical-push freshness gate*: an edge crossing is allowed only if the
//! user physically pushed the mouse toward that edge very recently (a raw HID
//! delta), so a peer-injected absolute `MouseSetPosition` — which produces no
//! relative delta — cannot false-trigger a local edge crossing while this
//! machine is itself being controlled. Extracting it here gives both engines one
//! tested implementation instead of two hand-kept-in-sync copies.

use std::time::Instant;

/// How recently a push toward an edge must have happened for a crossing on that
/// edge to be allowed. Matches the value both engines previously hard-coded.
pub const PUSH_FRESH_MS: u128 = 250;

/// The four edge directions, in the fixed index order both engines used
/// (`0=Right, 1=Left, 2=Top, 3=Bottom`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushDir {
    Right = 0,
    Left = 1,
    Top = 2,
    Bottom = 3,
}

/// Tracks, per direction, the last time the user physically pushed that way.
#[derive(Debug, Clone, Default)]
pub struct PushGate {
    last: [Option<Instant>; 4],
}

impl PushGate {
    pub fn new() -> Self {
        Self { last: [None; 4] }
    }

    /// Record an accumulated raw delta as a push at `now`. A positive `dx` is a
    /// push right, negative left; positive `dy` is down (Bottom), negative up
    /// (Top). A direction with no motion is left untouched (it expires by time,
    /// it is never actively cleared here) — identical to the engines' behavior.
    pub fn record_delta(&mut self, dx: i32, dy: i32, now: Instant) {
        if dx > 0 {
            self.last[PushDir::Right as usize] = Some(now);
        }
        if dx < 0 {
            self.last[PushDir::Left as usize] = Some(now);
        }
        if dy < 0 {
            self.last[PushDir::Top as usize] = Some(now);
        }
        if dy > 0 {
            self.last[PushDir::Bottom as usize] = Some(now);
        }
    }

    /// Whether the last push toward `dir` is still fresh at `now` (within
    /// [`PUSH_FRESH_MS`]). Uses saturating subtraction so a clock that appears
    /// to go backwards never panics.
    pub fn is_fresh(&self, dir: PushDir, now: Instant) -> bool {
        self.last[dir as usize]
            .is_some_and(|t| now.saturating_duration_since(t).as_millis() <= PUSH_FRESH_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn dir_indices_match_engine_order() {
        assert_eq!(PushDir::Right as usize, 0);
        assert_eq!(PushDir::Left as usize, 1);
        assert_eq!(PushDir::Top as usize, 2);
        assert_eq!(PushDir::Bottom as usize, 3);
    }

    #[test]
    fn record_delta_sets_only_the_pushed_directions() {
        let mut gate = PushGate::new();
        let t0 = Instant::now();

        gate.record_delta(5, 0, t0); // push right
        assert!(gate.is_fresh(PushDir::Right, t0));
        assert!(!gate.is_fresh(PushDir::Left, t0));
        assert!(!gate.is_fresh(PushDir::Top, t0));
        assert!(!gate.is_fresh(PushDir::Bottom, t0));

        gate.record_delta(0, -3, t0); // push up (Top)
        assert!(gate.is_fresh(PushDir::Top, t0));
        // Right is still remembered from before.
        assert!(gate.is_fresh(PushDir::Right, t0));
    }

    #[test]
    fn diagonal_delta_marks_both_axes() {
        let mut gate = PushGate::new();
        let t0 = Instant::now();
        gate.record_delta(-2, 4, t0); // left + down
        assert!(gate.is_fresh(PushDir::Left, t0));
        assert!(gate.is_fresh(PushDir::Bottom, t0));
        assert!(!gate.is_fresh(PushDir::Right, t0));
        assert!(!gate.is_fresh(PushDir::Top, t0));
    }

    #[test]
    fn freshness_expires_after_the_window() {
        let mut gate = PushGate::new();
        let t0 = Instant::now();
        gate.record_delta(1, 0, t0);

        let within = t0 + Duration::from_millis(PUSH_FRESH_MS as u64);
        assert!(gate.is_fresh(PushDir::Right, within), "still fresh at the boundary");

        let after = t0 + Duration::from_millis(PUSH_FRESH_MS as u64 + 1);
        assert!(!gate.is_fresh(PushDir::Right, after), "stale past the window");
    }

    #[test]
    fn never_pushed_is_never_fresh() {
        let gate = PushGate::new();
        assert!(!gate.is_fresh(PushDir::Right, Instant::now()));
    }
}
