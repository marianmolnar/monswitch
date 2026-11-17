//! Recent cursor positions, to find where the cursor left the screen.
//!
//! When the Deskflow server hands the cursor to the other machine it warps the
//! local (hidden) cursor to the middle of the screen, so "where is the cursor
//! now" is useless for the exit point. The last sample that touched a screen
//! edge is the exit point.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{sys, Edge};

const SAMPLE_EVERY: Duration = Duration::from_millis(10);
const KEEP: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Rect {
    /// The nearest edge to (x, y) and the distance to it (right/bottom are exclusive).
    fn nearest_edge(&self, x: f64, y: f64) -> (f64, Edge) {
        [
            (x - self.left, Edge::Left),
            (self.right - 1.0 - x, Edge::Right),
            (y - self.top, Edge::Top),
            (self.bottom - 1.0 - y, Edge::Bottom),
        ]
        .into_iter()
        .fold((f64::MAX, Edge::Left), |best, d| if d.0 < best.0 { d } else { best })
    }

    /// The screen edge (x, y) lies on, if it is within `tol` of one.
    pub fn edge_of(&self, x: f64, y: f64, tol: f64) -> Option<Edge> {
        let (dist, edge) = self.nearest_edge(x, y);
        (dist <= tol).then_some(edge)
    }

    /// (x, y) moved straight onto the nearest edge, with that edge, if the
    /// edge is at most `max` away.
    pub fn snap_to_edge(&self, x: f64, y: f64, max: f64) -> Option<(f64, f64, Edge)> {
        let (dist, edge) = self.nearest_edge(x, y);
        if dist > max {
            return None;
        }
        Some(match edge {
            Edge::Left => (self.left, y, edge),
            Edge::Right => (self.right - 1.0, y, edge),
            Edge::Top => (x, self.top, edge),
            Edge::Bottom => (x, self.bottom - 1.0, edge),
        })
    }

    /// Distance to the nearest edge (right/bottom are exclusive).
    pub fn edge_distance(&self, x: f64, y: f64) -> f64 {
        (x - self.left)
            .min(self.right - 1.0 - x)
            .min(y - self.top)
            .min(self.bottom - 1.0 - y)
            .max(0.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    x: f64,
    y: f64,
}

/// Picks the exit point from samples (oldest first): the newest one on an
/// edge, else the one closest to an edge (never the warped-to centre), else
/// the newest one.
fn exit_point(samples: &[Sample], before: Instant, bounds: impl Fn(f64, f64) -> Option<Rect>) -> Option<(f64, f64)> {
    let recent: Vec<(&Sample, Option<f64>)> = samples
        .iter()
        .filter(|s| s.at <= before)
        .map(|s| (s, bounds(s.x, s.y).map(|r| r.edge_distance(s.x, s.y))))
        .collect();
    let on_edge = recent.iter().rev().find(|(_, d)| d.is_some_and(|d| d <= 2.0));
    let closest = recent
        .iter()
        .rev()
        .filter(|(_, d)| d.is_some())
        .min_by(|a, b| a.1.unwrap_or(f64::MAX).total_cmp(&b.1.unwrap_or(f64::MAX)));
    on_edge
        .or(closest)
        .or(recent.last())
        .map(|(s, _)| (s.x, s.y))
}

/// Samples the cursor on a background thread while `active`.
pub struct Trail {
    samples: Arc<Mutex<VecDeque<Sample>>>,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Trail {
    pub fn start() -> Self {
        let samples = Arc::new(Mutex::new(VecDeque::new()));
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, a, st) = (samples.clone(), active.clone(), stop.clone());
        let spawned = std::thread::Builder::new().name("cursor-trail".into()).spawn(move || {
            sys::dpi_aware_thread();
            while !st.load(Ordering::Relaxed) {
                if !a.load(Ordering::Relaxed) {
                    if let Ok(mut q) = s.lock() {
                        q.clear();
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                if let Some((x, y)) = sys::cursor_position() {
                    let now = Instant::now();
                    if let Ok(mut q) = s.lock() {
                        q.push_back(Sample { at: now, x, y });
                        while q.front().is_some_and(|f| now.duration_since(f.at) > KEEP) {
                            q.pop_front();
                        }
                    }
                }
                std::thread::sleep(SAMPLE_EVERY);
            }
        });
        if let Err(e) = spawned {
            log::warn!("effect: cannot start cursor sampling: {e}");
        }
        Self { samples, active, stop }
    }

    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    /// Where the cursor most likely left the screen just before `at`.
    pub fn exit_point(&self, at: Instant) -> Option<(f64, f64)> {
        let samples: Vec<Sample> = self.samples.lock().ok()?.iter().copied().collect();
        exit_point(&samples, at, sys::display_bounds_at).or_else(sys::cursor_position)
    }
}

impl Drop for Trail {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_point_prefers_last_edge_sample() {
        let t0 = Instant::now();
        let at = |ms: u64, x: f64, y: f64| Sample { at: t0 + Duration::from_millis(ms), x, y };
        let screen = Rect { left: 0.0, top: 0.0, right: 3840.0, bottom: 2160.0 };
        let bounds = |_: f64, _: f64| Some(screen);
        // moving right, touching the edge, then warped to the centre
        let s = [at(0, 3700.0, 500.0), at(10, 3839.0, 520.0), at(20, 1920.0, 1080.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(30), bounds), Some((3839.0, 520.0)));
        // no edge sample: the one closest to an edge, not the warped centre
        let s = [at(0, 3800.0, 500.0), at(10, 3825.0, 510.0), at(20, 1920.0, 1080.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(30), bounds), Some((3825.0, 510.0)));
        // no bounds known: newest one
        let s = [at(0, 100.0, 100.0), at(10, 200.0, 200.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(30), |_, _| None), Some((200.0, 200.0)));
        // samples after the event are ignored
        let s = [at(0, 0.0, 300.0), at(50, 3839.0, 10.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(20), bounds), Some((0.0, 300.0)));
        assert_eq!(exit_point(&[], t0, bounds), None);
    }

    #[test]
    fn finds_the_edge() {
        let r = Rect { left: 0.0, top: 0.0, right: 1920.0, bottom: 2160.0 };
        assert_eq!(r.edge_of(1918.0, 800.0, 40.0), Some(Edge::Right));
        assert_eq!(r.edge_of(0.0, 5.0, 40.0), Some(Edge::Left));
        assert_eq!(r.edge_of(900.0, 2150.0, 40.0), Some(Edge::Bottom));
        assert_eq!(r.edge_of(900.0, 900.0, 40.0), None);
    }

    #[test]
    fn snaps_onto_the_nearest_edge() {
        let r = Rect { left: -1920.0, top: 0.0, right: 0.0, bottom: 2160.0 };
        // a fast move left the point ~2 cm inside
        assert_eq!(r.snap_to_edge(-1800.0, 700.0, 300.0), Some((-1920.0, 700.0, Edge::Left)));
        assert_eq!(r.snap_to_edge(-40.0, 1000.0, 300.0), Some((-1.0, 1000.0, Edge::Right)));
        assert_eq!(r.snap_to_edge(-900.0, 2100.0, 300.0), Some((-900.0, 2159.0, Edge::Bottom)));
        // already on the edge: unchanged
        assert_eq!(r.snap_to_edge(-1920.0, 5.5, 300.0), Some((-1920.0, 5.5, Edge::Left)));
        // too far from any edge (e.g. the warped-to centre): left alone
        assert_eq!(r.snap_to_edge(-960.0, 1080.0, 300.0), None);
    }
}
