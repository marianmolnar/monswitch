//! Platforms without an overlay implementation (Linux builds compile, effect is off).

use crate::trail::Rect;
use crate::{Burst, Error};

pub fn dpi_aware_thread() {}

pub fn cursor_position() -> Option<(f64, f64)> {
    None
}

pub fn display_bounds_at(_x: f64, _y: f64) -> Option<Rect> {
    None
}

pub struct Overlay;

impl Overlay {
    pub fn new() -> Result<Self, Error> {
        Err(Error::Overlay("not supported on this platform".into()))
    }

    pub fn show(&self, _burst: Burst) {}
}
