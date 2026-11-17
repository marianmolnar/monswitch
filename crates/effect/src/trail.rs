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

/// Following the cursor's motion to the edge is trusted up to this far
/// (platform units); further out the plain sample is used.
const MAX_EXTRAPOLATION: f64 = 400.0;

/// Where the ray from `p` (inside `r`) along `dir` reaches the edge of `r`,
/// if that is at most [`MAX_EXTRAPOLATION`] away. On a diagonal move this is
/// where the cursor really crossed; a perpendicular snap would be off along
/// the edge.
fn ray_to_edge(r: &Rect, p: (f64, f64), dir: (f64, f64)) -> Option<(f64, f64)> {
    let ((x, y), (dx, dy)) = (p, dir);
    let len = dx.hypot(dy);
    if len < 0.5 {
        return None;
    }
    let along = |v: f64, d: f64, lo: f64, hi: f64| {
        if d > 0.0 {
            (hi - v) / d
        } else if d < 0.0 {
            (lo - v) / d
        } else {
            f64::INFINITY
        }
    };
    let t = along(x, dx, r.left, r.right - 1.0).min(along(y, dy, r.top, r.bottom - 1.0));
    if !(0.0..=MAX_EXTRAPOLATION / len).contains(&t) {
        return None;
    }
    Some(((x + t * dx).clamp(r.left, r.right - 1.0), (y + t * dy).clamp(r.top, r.bottom - 1.0)))
}

/// Picks the exit point from samples (oldest first): the newest one on an
/// edge, else the one closest to an edge (never the warped-to centre), else
/// the newest one. A sample short of the edge is carried on along the motion
/// since the sample before it, to where it meets the edge.
fn exit_point(samples: &[Sample], before: Instant, bounds: impl Fn(f64, f64) -> Option<Rect>) -> Option<(f64, f64)> {
    let recent: Vec<(&Sample, Option<f64>)> = samples
        .iter()
        .filter(|s| s.at <= before)
        .map(|s| (s, bounds(s.x, s.y).map(|r| r.edge_distance(s.x, s.y))))
        .collect();
    let on_edge = recent.iter().rposition(|(_, d)| d.is_some_and(|d| d <= 2.0));
    let closest = recent
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, (_, d))| d.is_some())
        .min_by(|a, b| a.1 .1.unwrap_or(f64::MAX).total_cmp(&b.1 .1.unwrap_or(f64::MAX)))
        .map(|(i, _)| i);
    let i = on_edge.or(closest).or(recent.len().checked_sub(1))?;
    let s = recent[i].0;
    if i > 0 && recent[i].1.is_some_and(|d| d > 2.0) {
        let prev = recent[i - 1].0;
        let followed = bounds(s.x, s.y).and_then(|r| ray_to_edge(&r, (s.x, s.y), (s.x - prev.x, s.y - prev.y)));
        if followed.is_some() {
            return followed;
        }
    }
    Some((s.x, s.y))
}

/// Picks the entry point from samples (oldest first) taken since the cursor
/// came onto the screen: the first of them, followed back against the motion
/// (towards the newest sample) to the edge. Without motion, the newest one.
fn entry_point(samples: &[Sample], since: Instant, bounds: impl Fn(f64, f64) -> Option<Rect>) -> Option<(f64, f64)> {
    let after: Vec<&Sample> = samples.iter().filter(|s| s.at >= since).collect();
    let (first, newest) = (*after.first()?, *after.last()?);
    let back = (first.x - newest.x, first.y - newest.y);
    bounds(first.x, first.y)
        .and_then(|r| ray_to_edge(&r, (first.x, first.y), back))
        .or(Some((newest.x, newest.y)))
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

    /// Where the cursor most likely came onto the screen at `since`. Call it a
    /// little later, once a few samples on this screen exist.
    pub fn entry_point(&self, since: Instant) -> Option<(f64, f64)> {
        let samples: Vec<Sample> = self.samples.lock().ok()?.iter().copied().collect();
        entry_point(&samples, since, sys::display_bounds_at).or_else(sys::cursor_position)
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

    fn close(p: Option<(f64, f64)>, x: f64, y: f64) -> bool {
        p.is_some_and(|(px, py)| (px - x).abs() < 1e-6 && (py - y).abs() < 1e-6)
    }

    #[test]
    fn exit_point_prefers_last_edge_sample() {
        let t0 = Instant::now();
        let at = |ms: u64, x: f64, y: f64| Sample { at: t0 + Duration::from_millis(ms), x, y };
        let screen = Rect { left: 0.0, top: 0.0, right: 3840.0, bottom: 2160.0 };
        let bounds = |_: f64, _: f64| Some(screen);
        // moving right, touching the edge, then warped to the centre
        let s = [at(0, 3700.0, 500.0), at(10, 3839.0, 520.0), at(20, 1920.0, 1080.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(30), bounds), Some((3839.0, 520.0)));
        // no edge sample: the one closest to an edge (not the warped centre),
        // carried on along its motion (25, 10) to the edge
        let s = [at(0, 3800.0, 500.0), at(10, 3825.0, 510.0), at(20, 1920.0, 1080.0)];
        assert!(close(exit_point(&s, t0 + Duration::from_millis(30), bounds), 3839.0, 515.6));
        // no bounds known: newest one
        let s = [at(0, 100.0, 100.0), at(10, 200.0, 200.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(30), |_, _| None), Some((200.0, 200.0)));
        // samples after the event are ignored
        let s = [at(0, 0.0, 300.0), at(50, 3839.0, 10.0)];
        assert_eq!(exit_point(&s, t0 + Duration::from_millis(20), bounds), Some((0.0, 300.0)));
        assert_eq!(exit_point(&[], t0, bounds), None);
    }

    #[test]
    fn diagonal_moves_meet_the_edge_where_they_cross_it() {
        let t0 = Instant::now();
        let at = |ms: u64, x: f64, y: f64| Sample { at: t0 + Duration::from_millis(ms), x, y };
        let screen = Rect { left: 0.0, top: 0.0, right: 1920.0, bottom: 2160.0 };
        let bounds = |_: f64, _: f64| Some(screen);
        // leaving through the left edge at 45° downwards: last sample 60 inside
        let s = [at(0, 120.0, 1000.0), at(10, 60.0, 1060.0), at(20, 960.0, 1080.0)];
        assert!(close(exit_point(&s, t0 + Duration::from_millis(15), bounds), 0.0, 1120.0));
        // entering through the left edge moving right and up: followed back
        let s = [at(0, 1500.0, 900.0), at(30, 40.0, 1200.0), at(40, 80.0, 1180.0), at(50, 120.0, 1160.0)];
        assert!(close(entry_point(&s, t0 + Duration::from_millis(25), bounds), 0.0, 1220.0));
        // not moving: the newest sample (snapped onto the edge later)
        let s = [at(30, 20.0, 700.0), at(40, 20.0, 700.0)];
        assert!(close(entry_point(&s, t0 + Duration::from_millis(25), bounds), 20.0, 700.0));
        // implausibly far back: not trusted
        let s = [at(30, 900.0, 700.0), at(40, 905.0, 700.0)];
        assert!(close(entry_point(&s, t0 + Duration::from_millis(25), bounds), 905.0, 700.0));
        assert_eq!(entry_point(&[], t0, bounds), None);
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
