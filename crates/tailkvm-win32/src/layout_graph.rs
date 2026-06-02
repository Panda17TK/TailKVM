//! Named-screen adjacency graph for multi-machine seamless switching
//! (roadmap B2). Pure, unit-tested data model: it records which screen sits on
//! each edge of each screen and answers "which neighbor lies across this edge".
//!
//! This is the foundation the multi-client runtime (roadmap B1) will use to
//! route control to the correct peer when the logical cursor crosses an edge.
//! The runtime wiring (concurrent peer sessions, per-screen send routing, a
//! layout-graph editor UI) is intentionally out of scope here.

use crate::screen_space::Edge;
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
}
