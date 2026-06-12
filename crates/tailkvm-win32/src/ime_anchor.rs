//! IME candidate-anchor math (pure, no FFI).
//!
//! The IME candidate window anchors to the capture window, so the capture
//! window must sit where the user "feels" they are typing (IME-POS-002/010).
//! These helpers project a remote cursor position onto a controller-side
//! rect and clamp anchors into a visible monitor area. All inputs are plain
//! rects/points so the math is unit-testable.

/// A rectangle in controller-side physical pixels: (left, top, right, bottom).
pub type AnchorRect = (i32, i32, i32, i32);

/// Margin kept between the anchor and the monitor edge so the candidate UI
/// has room to open next to it.
pub const ANCHOR_EDGE_MARGIN: i32 = 16;

/// Offset used for the `lock_near` fallback: just below-right of the lock /
/// cursor position so the candidate UI does not cover it (IME-POS-012).
pub const LOCK_NEAR_OFFSET: i32 = 24;

/// Project a remote-cursor position onto a controller-side rect by
/// normalized mapping (IME-POS-010):
///
/// `anchor = local.origin + remote_pos / remote_size * local_size`
///
/// Degenerate remote sizes fall back to the local rect's center.
pub fn project_remote_to_local(
    remote_x: i32,
    remote_y: i32,
    remote_width: i32,
    remote_height: i32,
    local: AnchorRect,
) -> (i32, i32) {
    let (left, top, right, bottom) = local;
    let local_w = (right - left).max(1);
    let local_h = (bottom - top).max(1);
    if remote_width <= 0 || remote_height <= 0 {
        return (left + local_w / 2, top + local_h / 2);
    }
    let ratio_x = (remote_x as f64 / remote_width as f64).clamp(0.0, 1.0);
    let ratio_y = (remote_y as f64 / remote_height as f64).clamp(0.0, 1.0);
    (
        left + (local_w as f64 * ratio_x).round() as i32,
        top + (local_h as f64 * ratio_y).round() as i32,
    )
}

/// Clamp an anchor into `monitor` with [`ANCHOR_EDGE_MARGIN`] so the capture
/// window (and the candidate UI anchored to it) stays fully visible
/// (IME-POS-003/006). Degenerate monitors collapse to their origin.
pub fn clamp_anchor(x: i32, y: i32, monitor: AnchorRect) -> (i32, i32) {
    let (left, top, right, bottom) = monitor;
    let lo_x = left + ANCHOR_EDGE_MARGIN;
    let hi_x = (right - 1 - ANCHOR_EDGE_MARGIN).max(lo_x);
    let lo_y = top + ANCHOR_EDGE_MARGIN;
    let hi_y = (bottom - 1 - ANCHOR_EDGE_MARGIN).max(lo_y);
    (x.clamp(lo_x, hi_x), y.clamp(lo_y, hi_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_remote_center_to_local_center() {
        let local = (0, 0, 1920, 1080);
        let (x, y) = project_remote_to_local(640, 360, 1280, 720, local);
        assert_eq!((x, y), (960, 540));
    }

    #[test]
    fn projects_with_local_origin_offset() {
        // Secondary monitor left of primary.
        let local = (-1920, 200, 0, 1280);
        let (x, y) = project_remote_to_local(0, 0, 1280, 720, local);
        assert_eq!((x, y), (-1920, 200));
        let (x, y) = project_remote_to_local(1280, 720, 1280, 720, local);
        assert_eq!((x, y), (0, 1280));
    }

    #[test]
    fn projection_clamps_out_of_range_remote_positions() {
        let local = (0, 0, 1000, 1000);
        let (x, y) = project_remote_to_local(99_999, -50, 1280, 720, local);
        assert_eq!((x, y), (1000, 0));
    }

    #[test]
    fn degenerate_remote_size_falls_back_to_local_center() {
        let local = (0, 0, 1920, 1080);
        assert_eq!(project_remote_to_local(10, 10, 0, 720, local), (960, 540));
        assert_eq!(project_remote_to_local(10, 10, 1280, 0, local), (960, 540));
    }

    #[test]
    fn clamp_keeps_anchor_inside_monitor_with_margin() {
        let mon = (0, 0, 1920, 1080);
        assert_eq!(clamp_anchor(0, 0, mon), (16, 16));
        assert_eq!(
            clamp_anchor(5000, 5000, mon),
            (1920 - 1 - 16, 1080 - 1 - 16)
        );
        assert_eq!(clamp_anchor(960, 540, mon), (960, 540));
    }

    #[test]
    fn clamp_handles_negative_origin_monitors() {
        let mon = (-1920, -200, 0, 880);
        assert_eq!(clamp_anchor(-5000, -5000, mon), (-1920 + 16, -200 + 16));
        assert_eq!(clamp_anchor(0, 880, mon), (-1 - 16, 880 - 1 - 16));
    }

    #[test]
    fn clamp_survives_degenerate_monitor() {
        let mon = (10, 10, 12, 12);
        // hi < lo would panic in clamp; the helper collapses to lo instead.
        let (x, y) = clamp_anchor(0, 0, mon);
        assert_eq!((x, y), (26, 26));
    }
}
