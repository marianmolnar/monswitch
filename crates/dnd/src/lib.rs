//! Drag-and-drop of files across the KVM edge.
//!
//! Deskflow locks the cursor to the current screen while a mouse button is
//! held, so a file drag can never cross the edge by itself. Instead:
//!
//! 1. While a mouse button is held near an edge that leads to the other
//!    machine, an invisible strip appears along that edge ([`EdgeDrop`]). It is
//!    an OS drop target, so a file drag that reaches the edge lands on it
//!    ([`Callbacks::on_hover`] → show "release to send").
//! 2. When the files are dropped on it ([`Callbacks::on_drop`]), the caller
//!    offers them to the other machine and calls [`cross_edge`]: the button is
//!    up now, so Deskflow lets the cursor over.
//! 3. The other machine then shows the files (shelf / simulated drag).
//!
//! [`layout`] finds the edges that lead to the other machine from Deskflow's
//! own screen layout.

pub mod layout;

#[cfg(windows)]
#[path = "sys_windows.rs"]
mod sys;
#[cfg(not(windows))]
#[path = "sys_other.rs"]
mod sys;

use std::path::PathBuf;

use thiserror::Error;

pub use sys::{cross_edge, cursor_position, EdgeDrop};

#[derive(Debug, Error)]
pub enum Error {
    #[error("edge drop: {0}")]
    Sys(String),
    #[error("edge drop is not supported on this platform yet")]
    Unsupported,
}

/// Screen edge (of the whole desktop) that leads to the other machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// Unit step pointing out of the screen through this edge (y down).
    pub fn outward(self) -> (i32, i32) {
        match self {
            Edge::Left => (-1, 0),
            Edge::Right => (1, 0),
            Edge::Top => (0, -1),
            Edge::Bottom => (0, 1),
        }
    }

    /// "left" / "right" / "up" / "top" / "down" / "bottom" (Deskflow uses up/down).
    pub fn parse(s: &str) -> Option<Edge> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            "up" | "top" => Some(Edge::Top),
            "down" | "bottom" => Some(Edge::Bottom),
            _ => None,
        }
    }
}

/// Files dropped on an edge strip.
#[derive(Debug, Clone, PartialEq)]
pub struct Dropped {
    pub files: Vec<PathBuf>,
    pub edge: Edge,
    /// Where they were dropped (global screen coordinates).
    pub x: i32,
    pub y: i32,
}

/// Called on the edge-drop thread; keep them short (hand work to another thread).
pub struct Callbacks {
    /// A file drag entered the strip on this edge (`Some`) or left it (`None`).
    pub on_hover: Box<dyn Fn(Option<Edge>) + Send + Sync>,
    /// Files were dropped on the strip. Call [`cross_edge`] afterwards (from
    /// another thread, once the drop has finished) to hand the cursor over.
    pub on_drop: Box<dyn Fn(Dropped) + Send + Sync>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges() {
        assert_eq!(Edge::parse("up"), Some(Edge::Top));
        assert_eq!(Edge::parse(" Right "), Some(Edge::Right));
        assert_eq!(Edge::parse("diagonal"), None);
        for e in Edge::ALL {
            assert_eq!(e.opposite().opposite(), e);
            let (x, y) = e.outward();
            let (ox, oy) = e.opposite().outward();
            assert_eq!((x + ox, y + oy), (0, 0));
        }
    }
}
