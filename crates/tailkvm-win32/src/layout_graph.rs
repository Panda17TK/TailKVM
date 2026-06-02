//! Named-screen adjacency graph for multi-machine seamless switching
//! (roadmap B2). Pure, unit-tested data model: it records which screen sits on
//! each edge of each screen and answers "which neighbor lies across this edge".
//!
//! This is the foundation the multi-client runtime (roadmap B1) will use to
//! route control to the correct peer when the logical cursor crosses an edge.
//! The runtime wiring (concurrent peer sessions, per-screen send routing, a
//! layout-graph editor UI) is intentionally out of scope here.

use crate::screen_space::{map_ratio, Edge, Rect};
use std::collections::HashMap;

/// Adjacency between named screens.
#[derive(Debug, Default, Clone)]
pub struct LayoutGraph {
    links: HashMap<(String, Edge), String>,
}

impl LayoutGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Link screen `a`'s `edge` to screen `b`, and (bidirectionally) `b`'s
    /// opposite edge back to `a`. Re-linking overwrites the previous neighbor.
    pub fn link(&mut self, a: &str, edge: Edge, b: &str) {
        self.links.insert((a.to_string(), edge), b.to_string());
        self.links
            .insert((b.to_string(), edge.opposite()), a.to_string());
    }

    /// The screen across `edge` from `screen`, if any.
    pub fn neighbor(&self, screen: &str, edge: Edge) -> Option<&str> {
        self.links
            .get(&(screen.to_string(), edge))
            .map(String::as_str)
    }

    /// Number of directed links recorded (two per `link` call).
    pub fn link_count(&self) -> usize {
        self.links.len()
    }
}

/// Logical cursor in the multi-screen space: a point in `screen`'s native
/// coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenCursor {
    pub screen: String,
    pub x: i32,
    pub y: i32,
}

/// A screen transition produced by [`MultiScreenSpace::apply_delta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSwitch {
    pub from: String,
    pub to: String,
}

/// N-screen combined coordinate space (roadmap B1.1). Generalizes the 2-screen
/// `CombinedSpace`: a logical cursor lives in one named screen; relative deltas
/// move it, and crossing an edge transfers it to the adjacent screen named by
/// the [`LayoutGraph`], entering the opposite edge at the proportionally-mapped
/// perpendicular position. Pure and unit-tested; no Win32.
#[derive(Debug, Clone)]
pub struct MultiScreenSpace {
    screens: HashMap<String, Rect>,
    graph: LayoutGraph,
    inset: i32,
}

impl MultiScreenSpace {
    pub fn new(screens: HashMap<String, Rect>, graph: LayoutGraph) -> Self {
        Self {
            screens,
            graph,
            inset: 2,
        }
    }

    pub fn rect(&self, screen: &str) -> Option<&Rect> {
        self.screens.get(screen)
    }

    /// Apply a relative delta to the cursor. Returns the new cursor and, when it
    /// crossed into an adjacent screen, the switch. Crossing an edge with no
    /// neighbor (or an unknown one) clamps within the current screen.
    pub fn apply_delta(
        &self,
        cursor: ScreenCursor,
        dx: i32,
        dy: i32,
    ) -> (ScreenCursor, Option<ScreenSwitch>) {
        let Some(rect) = self.screens.get(&cursor.screen).copied() else {
            return (cursor, None);
        };

        let nx = cursor.x + dx;
        let ny = cursor.y + dy;

        // Determine the crossed edge (priority right>left>top>bottom for the
        // ambiguous corner case).
        let crossed = if nx >= rect.right {
            Some(Edge::Right)
        } else if nx < rect.left {
            Some(Edge::Left)
        } else if ny < rect.top {
            Some(Edge::Top)
        } else if ny >= rect.bottom {
            Some(Edge::Bottom)
        } else {
            None
        };

        if let Some(edge) = crossed {
            if let Some(neighbor) = self.graph.neighbor(&cursor.screen, edge) {
                if let Some(&nb_rect) = self.screens.get(neighbor) {
                    let entered = self.enter(&rect, &nb_rect, edge, nx, ny);
                    return (
                        ScreenCursor {
                            screen: neighbor.to_string(),
                            x: entered.0,
                            y: entered.1,
                        },
                        Some(ScreenSwitch {
                            from: cursor.screen,
                            to: neighbor.to_string(),
                        }),
                    );
                }
            }
        }

        // No crossing (or no neighbor): clamp within the current screen.
        (
            ScreenCursor {
                screen: cursor.screen,
                x: nx.clamp(rect.left, rect.right - 1),
                y: ny.clamp(rect.top, rect.bottom - 1),
            },
            None,
        )
    }

    /// Compute the entry point in `nb` when leaving `from` across `edge`.
    fn enter(&self, from: &Rect, nb: &Rect, edge: Edge, exit_x: i32, exit_y: i32) -> (i32, i32) {
        match edge {
            // Exit right -> enter neighbor's left side; perpendicular axis is y.
            Edge::Right => (
                nb.left + self.inset,
                map_ratio(exit_y, from.top, from.height(), nb.top, nb.height()),
            ),
            Edge::Left => (
                nb.right - 1 - self.inset,
                map_ratio(exit_y, from.top, from.height(), nb.top, nb.height()),
            ),
            // Exit top -> enter neighbor's bottom; perpendicular axis is x.
            Edge::Top => (
                map_ratio(exit_x, from.left, from.width(), nb.left, nb.width()),
                nb.bottom - 1 - self.inset,
            ),
            Edge::Bottom => (
                map_ratio(exit_x, from.left, from.width(), nb.left, nb.width()),
                nb.top + self.inset,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_is_bidirectional() {
        let mut graph = LayoutGraph::new();
        graph.link("alice", Edge::Right, "bob");
        assert_eq!(graph.neighbor("alice", Edge::Right), Some("bob"));
        assert_eq!(graph.neighbor("bob", Edge::Left), Some("alice"));
        // No link on the unspecified edges.
        assert_eq!(graph.neighbor("alice", Edge::Left), None);
        assert_eq!(graph.neighbor("bob", Edge::Right), None);
    }

    #[test]
    fn vertical_links_use_top_bottom() {
        let mut graph = LayoutGraph::new();
        graph.link("alice", Edge::Top, "carol");
        assert_eq!(graph.neighbor("alice", Edge::Top), Some("carol"));
        assert_eq!(graph.neighbor("carol", Edge::Bottom), Some("alice"));
    }

    #[test]
    fn chains_three_screens() {
        let mut graph = LayoutGraph::new();
        graph.link("a", Edge::Right, "b");
        graph.link("b", Edge::Right, "c");
        assert_eq!(graph.neighbor("a", Edge::Right), Some("b"));
        assert_eq!(graph.neighbor("b", Edge::Right), Some("c"));
        assert_eq!(graph.neighbor("c", Edge::Left), Some("b"));
        assert_eq!(graph.neighbor("b", Edge::Left), Some("a"));
        assert_eq!(graph.neighbor("c", Edge::Right), None);
    }

    #[test]
    fn relinking_overwrites_previous_neighbor() {
        let mut graph = LayoutGraph::new();
        graph.link("a", Edge::Right, "b");
        graph.link("a", Edge::Right, "c");
        assert_eq!(graph.neighbor("a", Edge::Right), Some("c"));
        assert_eq!(graph.neighbor("c", Edge::Left), Some("a"));
    }

    #[test]
    fn unknown_screen_has_no_neighbor() {
        let graph = LayoutGraph::new();
        assert_eq!(graph.neighbor("ghost", Edge::Right), None);
    }

    fn three_screen_space() -> MultiScreenSpace {
        // a (1920x1080) -- right --> b (1280x720) -- right --> c (1024x768)
        let mut screens = HashMap::new();
        screens.insert("a".to_string(), Rect::new(0, 0, 1920, 1080));
        screens.insert("b".to_string(), Rect::new(0, 0, 1280, 720));
        screens.insert("c".to_string(), Rect::new(0, 0, 1024, 768));
        let mut graph = LayoutGraph::new();
        graph.link("a", Edge::Right, "b");
        graph.link("b", Edge::Right, "c");
        MultiScreenSpace::new(screens, graph)
    }

    #[test]
    fn moves_within_screen_without_switch() {
        let space = three_screen_space();
        let (cur, switch) = space.apply_delta(
            ScreenCursor {
                screen: "b".to_string(),
                x: 100,
                y: 100,
            },
            40,
            10,
        );
        assert_eq!(switch, None);
        assert_eq!((cur.screen.as_str(), cur.x, cur.y), ("b", 140, 110));
    }

    #[test]
    fn crosses_a_to_b_then_b_to_c() {
        let space = three_screen_space();
        // a right edge -> b (enter b left), y mid mapped.
        let (cur, switch) = space.apply_delta(
            ScreenCursor {
                screen: "a".to_string(),
                x: 1919,
                y: 540,
            },
            5,
            0,
        );
        assert_eq!(
            switch,
            Some(ScreenSwitch {
                from: "a".to_string(),
                to: "b".to_string()
            })
        );
        assert_eq!(cur.screen, "b");
        assert_eq!(cur.x, 2); // b.left + inset
                              // ratio 540/1080=0.5 -> (720-1)*0.5=359.5 -> 360
        assert_eq!(cur.y, 360);

        // continue from b right edge -> c
        let (cur2, switch2) = space.apply_delta(
            ScreenCursor {
                screen: "b".to_string(),
                x: 1279,
                y: 360,
            },
            5,
            0,
        );
        assert_eq!(switch2.map(|s| s.to), Some("c".to_string()));
        assert_eq!(cur2.screen, "c");
        assert_eq!(cur2.x, 2);
    }

    #[test]
    fn returns_c_to_b_to_a_via_left() {
        let space = three_screen_space();
        let (cur, switch) = space.apply_delta(
            ScreenCursor {
                screen: "c".to_string(),
                x: 0,
                y: 100,
            },
            -5,
            0,
        );
        assert_eq!(switch.map(|s| s.to), Some("b".to_string()));
        assert_eq!(cur.screen, "b");
        assert_eq!(cur.x, 1280 - 1 - 2); // enter b right side
    }

    #[test]
    fn edge_without_neighbor_clamps() {
        let space = three_screen_space();
        // a has no left neighbor -> clamp.
        let (cur, switch) = space.apply_delta(
            ScreenCursor {
                screen: "a".to_string(),
                x: 5,
                y: 500,
            },
            -100,
            0,
        );
        assert_eq!(switch, None);
        assert_eq!((cur.screen.as_str(), cur.x), ("a", 0));
    }
}
