//! Pure edge-crossing and coordinate geometry for seamless/multi-screen KVM.
//!
//! This is the shared, platform-independent home for the edge/return math that
//! the seamless engine (and, in future, the multi-screen router) needs. It has
//! no Win32 dependency: coordinates are plain [`Point`]/[`Rect`] values, and the
//! app layer adapts its `tailkvm_win32` cursor/monitor types at the boundary.
//! Keeping this logic here removes the duplicate edge math the engines otherwise
//! reimplement and makes it directly unit-testable.

/// A screen coordinate (device pixels, virtual-screen space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A screen rectangle with cached `width`/`height`, mirroring the fields of
/// `tailkvm_win32::monitor::RectI32` so the app can convert with a field copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
            width: right - left,
            height: bottom - top,
        }
    }
}

/// Canonicalize an edge label to one of `left|right|top|bottom`, defaulting to
/// `right` for trimmed/case-insensitive-unknown input.
pub fn normalize_edge(edge: &str) -> String {
    match edge.trim().to_lowercase().as_str() {
        "left" => "left".to_string(),
        "right" => "right".to_string(),
        "top" => "top".to_string(),
        "bottom" => "bottom".to_string(),
        _ => "right".to_string(),
    }
}

/// Whether the remote-side cursor at `(x, y)` sits on the edge that returns
/// control to the local machine, given the switch edge and the remote screen
/// size. The margin is floored at 8px so a too-small configured margin can't
/// make the return edge unreachable.
pub fn is_remote_return_edge(
    x: i32,
    y: i32,
    switch_edge: &str,
    edge_margin: i32,
    remote_width: i32,
    remote_height: i32,
) -> bool {
    let margin = edge_margin.max(8);
    let width = remote_width.max(1);
    let height = remote_height.max(1);

    match switch_edge {
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

/// Whether `position` is within `margin` of the given `edge` of `rect`.
pub fn is_cursor_at_edge(position: Point, rect: Rect, edge: &str, margin: i32) -> bool {
    match edge {
        "left" => position.x <= rect.left + margin,
        "right" => position.x >= rect.right - 1 - margin,
        "top" => position.y <= rect.top + margin,
        "bottom" => position.y >= rect.bottom - 1 - margin,
        _ => position.x >= rect.right - 1 - margin,
    }
}

/// Map a local exit position to the entry position on the opposite edge of the
/// remote screen, preserving the along-edge ratio (aspect-correct crossing).
pub fn remote_entry_position(
    position: Point,
    local_rect: Rect,
    edge: &str,
    remote_width: i32,
    remote_height: i32,
) -> Point {
    let inset = 4;

    match edge {
        "left" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            Point {
                x: remote_width - 1 - inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "right" => {
            let ratio = ((position.y - local_rect.top) as f64 / local_rect.height.max(1) as f64)
                .clamp(0.0, 1.0);
            Point {
                x: inset,
                y: ((remote_height - 1) as f64 * ratio).round() as i32,
            }
        }
        "top" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            Point {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: remote_height - 1 - inset,
            }
        }
        "bottom" => {
            let ratio = ((position.x - local_rect.left) as f64 / local_rect.width.max(1) as f64)
                .clamp(0.0, 1.0);
            Point {
                x: ((remote_width - 1) as f64 * ratio).round() as i32,
                y: inset,
            }
        }
        _ => Point {
            x: inset,
            y: remote_height / 2,
        },
    }
}

/// Position to place the local cursor when control returns from the remote,
/// just inside the given `edge` of `rect`. The margin is floored at 8px so the
/// cursor lands clear of the edge (and won't immediately re-cross).
pub fn local_return_position(position: Point, rect: Rect, edge: &str, margin: i32) -> Point {
    let safe_margin = margin.max(8);

    match edge {
        "left" => Point {
            x: rect.left + safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "right" => Point {
            x: rect.right - 1 - safe_margin,
            y: position
                .y
                .clamp(rect.top + safe_margin, rect.bottom - 1 - safe_margin),
        },
        "top" => Point {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.top + safe_margin,
        },
        "bottom" => Point {
            x: position
                .x
                .clamp(rect.left + safe_margin, rect.right - 1 - safe_margin),
            y: rect.bottom - 1 - safe_margin,
        },
        _ => Point {
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

    #[test]
    fn normalize_edge_keeps_valid_and_defaults_to_right() {
        assert_eq!(normalize_edge("left"), "left");
        assert_eq!(normalize_edge("right"), "right");
        assert_eq!(normalize_edge("top"), "top");
        assert_eq!(normalize_edge("bottom"), "bottom");
        // Trimmed + case-insensitive.
        assert_eq!(normalize_edge("  RIGHT "), "right");
        assert_eq!(normalize_edge("Top"), "top");
        // Unknown falls back to the default edge.
        assert_eq!(normalize_edge("diagonal"), "right");
        assert_eq!(normalize_edge(""), "right");
    }

    #[test]
    fn is_cursor_at_edge_respects_margin_on_each_side() {
        let r = Rect::new(0, 0, 1920, 1080);
        let margin = 3;

        // right edge: x >= right - 1 - margin = 1916
        assert!(is_cursor_at_edge(Point::new(1916, 500), r, "right", margin));
        assert!(!is_cursor_at_edge(
            Point::new(1915, 500),
            r,
            "right",
            margin
        ));

        // left edge: x <= left + margin = 3
        assert!(is_cursor_at_edge(Point::new(3, 500), r, "left", margin));
        assert!(!is_cursor_at_edge(Point::new(4, 500), r, "left", margin));

        // top edge: y <= top + margin = 3
        assert!(is_cursor_at_edge(Point::new(500, 3), r, "top", margin));
        assert!(!is_cursor_at_edge(Point::new(500, 4), r, "top", margin));

        // bottom edge: y >= bottom - 1 - margin = 1076
        assert!(is_cursor_at_edge(
            Point::new(500, 1076),
            r,
            "bottom",
            margin
        ));
        assert!(!is_cursor_at_edge(
            Point::new(500, 1075),
            r,
            "bottom",
            margin
        ));
    }

    #[test]
    fn is_cursor_at_edge_handles_negative_origin_virtual_screen() {
        // Multi-monitor virtual screen whose primary is not at (0,0).
        let r = Rect::new(-1920, -200, 1920, 1080);
        let margin = 3;

        // right edge: x >= 1920 - 1 - 3 = 1916
        assert!(is_cursor_at_edge(Point::new(1916, 0), r, "right", margin));
        assert!(!is_cursor_at_edge(Point::new(1900, 0), r, "right", margin));

        // left edge: x <= -1920 + 3 = -1917
        assert!(is_cursor_at_edge(Point::new(-1917, 0), r, "left", margin));
        assert!(!is_cursor_at_edge(Point::new(-1916, 0), r, "left", margin));
    }

    #[test]
    fn remote_entry_position_enters_opposite_edge_with_aspect_mapping() {
        let local = Rect::new(0, 0, 1920, 1080);
        let (rw, rh) = (1280, 720);
        let inset = 4;

        // Exit local RIGHT -> enter remote LEFT (small x), y mapped by ratio.
        let entry = remote_entry_position(Point::new(1919, 540), local, "right", rw, rh);
        assert_eq!(entry.x, inset);
        // ratio = 540/1080 = 0.5 -> y = (720-1)*0.5 = 359.5 -> 360
        assert_eq!(entry.y, 360);

        // Exit local LEFT -> enter remote RIGHT (large x).
        let entry = remote_entry_position(Point::new(0, 0), local, "left", rw, rh);
        assert_eq!(entry.x, rw - 1 - inset);
        assert_eq!(entry.y, 0);

        // Exit local TOP -> enter remote BOTTOM (large y), x mapped by ratio.
        let entry = remote_entry_position(Point::new(960, 0), local, "top", rw, rh);
        assert_eq!(entry.y, rh - 1 - inset);
        // ratio = 960/1920 = 0.5 -> x = (1280-1)*0.5 = 639.5 -> 640
        assert_eq!(entry.x, 640);

        // Exit local BOTTOM -> enter remote TOP (small y).
        let entry = remote_entry_position(Point::new(960, 1079), local, "bottom", rw, rh);
        assert_eq!(entry.y, inset);
    }

    #[test]
    fn remote_entry_position_clamps_ratio_within_bounds() {
        // Cursor far below the rect should still map within [0, rh-1].
        let local = Rect::new(0, 0, 1920, 1080);
        let entry = remote_entry_position(Point::new(1919, 100_000), local, "right", 1280, 720);
        assert!(
            entry.y >= 0 && entry.y <= 719,
            "y out of range: {}",
            entry.y
        );
    }

    #[test]
    fn local_return_position_uses_safe_margin_floor_of_8() {
        let r = Rect::new(0, 0, 1920, 1080);

        // margin below 8 is bumped to 8.
        let ret = local_return_position(Point::new(1919, 540), r, "right", 3);
        assert_eq!(ret.x, 1920 - 1 - 8);
        assert!(ret.y >= 8 && ret.y <= 1080 - 1 - 8);

        let ret = local_return_position(Point::new(0, 540), r, "left", 3);
        assert_eq!(ret.x, 8);

        let ret = local_return_position(Point::new(960, 0), r, "top", 3);
        assert_eq!(ret.y, 8);

        let ret = local_return_position(Point::new(960, 1079), r, "bottom", 3);
        assert_eq!(ret.y, 1080 - 1 - 8);
    }

    #[test]
    fn is_remote_return_edge_mirrors_switch_edge() {
        // margin floor is 8 inside the function (edge_margin passed as 3).
        // Switch right -> entered remote from left -> return at remote LEFT edge.
        assert!(is_remote_return_edge(8, 500, "right", 3, 1920, 1080));
        assert!(!is_remote_return_edge(9, 500, "right", 3, 1920, 1080));

        // Switch left -> return at remote RIGHT edge: x >= width-1-8 = 1911.
        assert!(is_remote_return_edge(1911, 500, "left", 3, 1920, 1080));
        assert!(!is_remote_return_edge(1910, 500, "left", 3, 1920, 1080));

        // Switch top -> return at remote BOTTOM edge: y >= height-1-8 = 1071.
        assert!(is_remote_return_edge(500, 1071, "top", 3, 1920, 1080));
        assert!(!is_remote_return_edge(500, 1070, "top", 3, 1920, 1080));

        // Switch bottom -> return at remote TOP edge: y <= 8.
        assert!(is_remote_return_edge(500, 8, "bottom", 3, 1920, 1080));
        assert!(!is_remote_return_edge(500, 9, "bottom", 3, 1920, 1080));
    }
}
