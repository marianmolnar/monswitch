//! Platforms without an edge drop strip yet (macOS: NSPasteboard(name: .drag)
//! based detection is still to be written).

use crate::{Callbacks, Edge, Error};

pub struct EdgeDrop;

impl EdgeDrop {
    pub fn start(_edges: Vec<Edge>, _callbacks: Callbacks) -> Result<Self, Error> {
        Err(Error::Unsupported)
    }

    pub fn set_edges(&self, _edges: Vec<Edge>) {}
}

pub fn cross_edge(_edge: Edge) -> Result<(), Error> {
    Err(Error::Unsupported)
}

pub fn cursor_position() -> Option<(i32, i32)> {
    None
}
