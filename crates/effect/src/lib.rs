//! Edge transition effect for the software KVM.
//!
//! When the cursor crosses from one machine to the other, the machine it
//! leaves shows a short burst at the exit point and the machine it enters a
//! smaller one at the entry point.
//!
//! - The Deskflow **server** sees every switch in deskflow-core's log
//!   ([`kvm::CoreEvent::Switch`]). It shows its own burst and tells the client
//!   over a tiny UDP message ([`proto`]).
//! - The **client** does not get that line. It logs `entering computer` /
//!   `leaving computer` ([`kvm::CoreEvent::Presence`]) and shows the burst
//!   from those, at the cursor (entry) or the last edge sample (exit). A UDP
//!   message for the same crossing is ignored, so the burst plays once even
//!   when the server also sends one. The client still says hello every few
//!   seconds; the server address comes from the client core's log (or
//!   `[kvm.effect].peer`).
//! - The overlay ([`Overlay`]) is a native transparent, click-through, topmost
//!   window created once at startup; it never takes input.

pub mod proto;

mod link;
// The software renderer draws the overlay on Windows and macOS; other
// platforms have no overlay.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub mod render;
mod trail;

#[cfg(target_os = "macos")]
#[path = "sys_macos.rs"]
mod sys;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "sys_other.rs"]
mod sys;
#[cfg(windows)]
#[path = "sys_windows.rs"]
mod sys;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ddc_core::config::{EffectConfig, EffectStyle, KvmConfig, KvmRole};
use kvm::CoreEvent;
use thiserror::Error;

pub use sys::{cursor_position, dpi_aware_thread, Overlay};

use crate::link::Link;
use crate::proto::Msg;
use crate::trail::Trail;

#[derive(Debug, Error)]
pub enum Error {
    #[error("overlay: {0}")]
    Overlay(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The cursor left this screen here (bigger burst).
    Exit,
    /// The cursor entered this screen here (smaller burst).
    Entry,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub style: EffectStyle,
    pub duration: Duration,
    pub rgb: (u8, u8, u8),
}

impl Look {
    pub fn from_config(c: &EffectConfig) -> Self {
        Self {
            style: c.style,
            duration: Duration::from_millis(u64::from(c.duration_ms.clamp(50, 3000))),
            rgb: c.rgb().unwrap_or((0x38, 0xBD, 0xF8)),
        }
    }
}

/// Screen edge a burst sits on (the light runs along it, the mist drifts away from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// Unit vector pointing into the screen (screen coordinates, y down).
    pub fn inward(self) -> (f32, f32) {
        match self {
            Edge::Left => (1.0, 0.0),
            Edge::Right => (-1.0, 0.0),
            Edge::Top => (0.0, 1.0),
            Edge::Bottom => (0.0, -1.0),
        }
    }

    /// The edge runs vertically (left/right edge).
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// One burst, in the platform's global screen coordinates
/// (Windows: physical pixels; macOS: points, origin top-left).
#[derive(Debug, Clone, PartialEq)]
pub struct Burst {
    pub x: f64,
    pub y: f64,
    pub kind: Kind,
    pub look: Look,
    /// Screen edge at (x, y), if it is close to one.
    pub edge: Option<Edge>,
}

/// How close to an edge a burst must be to count as "on" it.
const EDGE_TOLERANCE: f64 = 48.0;

/// Screen edge at (x, y) on this machine's displays.
pub fn edge_at(x: f64, y: f64) -> Option<Edge> {
    sys::display_bounds_at(x, y).and_then(|r| r.edge_of(x, y, EDGE_TOLERANCE))
}

/// Bursts this close to an edge are moved onto it (platform units: physical
/// pixels on Windows, points on macOS). On a fast move the last trail sample
/// (exit) or the settled cursor (entry) can be a few centimetres inside the
/// screen, but the burst belongs on the edge the cursor crossed.
const SNAP_DISTANCE: f64 = 300.0;

/// Display height assumed when the display under a burst is unknown.
const FALLBACK_DISPLAY_HEIGHT: f64 = 1440.0;

/// Burst radius at (x, y), in the platform's units: a fixed fraction of the
/// height of the display there (see [`render::RADIUS_OF_HEIGHT`]).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn burst_radius(x: f64, y: f64) -> f64 {
    let height = sys::display_bounds_at(x, y).map_or(FALLBACK_DISPLAY_HEIGHT, |r| r.bottom - r.top);
    render::radius_for(height)
}

/// (x, y) moved onto the nearest screen edge if it is within
/// [`SNAP_DISTANCE`], with that edge; otherwise unchanged with no edge.
pub fn snap_to_edge(x: f64, y: f64) -> (f64, f64, Option<Edge>) {
    match sys::display_bounds_at(x, y).and_then(|r| r.snap_to_edge(x, y, SNAP_DISTANCE)) {
        Some((sx, sy, edge)) => (sx, sy, Some(edge)),
        None => (x, y, None),
    }
}

/// Displays a burst. The tray app forwards it to the thread that owns the
/// overlay (the main thread on macOS).
pub type ShowFn = Arc<dyn Fn(Burst) + Send + Sync>;

/// How often the client says hello.
const HELLO_EVERY: Duration = Duration::from_secs(3);
/// The client's cursor is warped to the entry point right after the switch.
const ENTRY_SETTLE: Duration = Duration::from_millis(25);
/// One crossing can be seen twice (the client's own log and the server's UDP
/// message). The second of the same kind inside this window is dropped.
const BURST_COALESCE: Duration = Duration::from_millis(400);

struct State {
    role: KvmRole,
    cfg: EffectConfig,
    /// `[kvm].screen_name` (explicit) – also filters messages on the client.
    configured_name: Option<String>,
    /// This machine's Deskflow screen name, if known.
    my_name: Option<String>,
    /// Client: server address from deskflow-core's log.
    server: Option<String>,
    /// Last burst we committed to showing.
    last_shown: Option<(Kind, Instant)>,
}

impl State {
    fn active(&self) -> bool {
        self.cfg.enabled && self.role != KvmRole::Off
    }
}

struct Inner {
    state: Mutex<State>,
    link: Option<Link>,
    trail: Trail,
    show: ShowFn,
    stop: AtomicBool,
}

/// Runs the effect logic; feed it deskflow-core events with [`Effects::on_core_event`].
pub struct Effects {
    inner: Arc<Inner>,
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().and_then(|s| non_empty(&s))
}

impl Effects {
    pub fn start(kvm: &KvmConfig, show: ShowFn) -> Self {
        // port 0 (rejected by config validation) = no network, used by tests
        let link = match (kvm.effect.port != 0).then(|| Link::bind(kvm.effect.port)) {
            None => None,
            Some(Ok(l)) => Some(l),
            Some(Err(e)) => {
                log::warn!("effect: cannot use UDP port {}: {e}", kvm.effect.port);
                None
            }
        };
        let configured_name = non_empty(&kvm.screen_name);
        let state = State {
            role: kvm.role,
            cfg: kvm.effect.clone(),
            my_name: configured_name.clone().or_else(hostname),
            configured_name,
            server: None,
            last_shown: None,
        };
        let trail = Trail::start();
        trail.set_active(state.active());
        let inner = Arc::new(Inner { state: Mutex::new(state), link, trail, show, stop: AtomicBool::new(false) });

        if inner.link.is_some() {
            let i = inner.clone();
            let r = std::thread::Builder::new().name("effect-recv".into()).spawn(move || {
                while !i.stop.load(Ordering::Relaxed) {
                    if let Some((msg, _)) = i.link.as_ref().and_then(Link::recv) {
                        i.on_msg(msg);
                    }
                }
            });
            if let Err(e) = r {
                log::warn!("effect: cannot start receiver: {e}");
            }
            let i = inner.clone();
            let r = std::thread::Builder::new().name("effect-hello".into()).spawn(move || {
                while !i.stop.load(Ordering::Relaxed) {
                    i.say_hello();
                    std::thread::sleep(HELLO_EVERY);
                }
            });
            if let Err(e) = r {
                log::warn!("effect: cannot start hello sender: {e}");
            }
        }
        Self { inner }
    }

    /// New settings from the settings window. A different port needs a restart.
    pub fn set_config(&self, kvm: &KvmConfig) {
        let mut s = self.inner.lock();
        if kvm.effect.port != s.cfg.port {
            log::warn!("effect: port change takes effect after restarting the app");
        }
        s.role = kvm.role;
        s.cfg = kvm.effect.clone();
        let configured = non_empty(&kvm.screen_name);
        if configured.is_some() {
            s.my_name = configured.clone();
        }
        s.configured_name = configured;
        self.inner.trail.set_active(s.active());
    }

    pub fn on_core_event(&self, ev: &CoreEvent) {
        self.inner.on_core_event(ev);
    }
}

impl Drop for Effects {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn burst(&self, x: f64, y: f64, kind: Kind, look: Look) {
        let (x, y, edge) = snap_to_edge(x, y);
        log::info!("effect: {kind:?} burst at {x:.0},{y:.0} ({edge:?})");
        (self.show)(Burst { x, y, kind, look, edge });
    }

    /// False when this kind was already shown for the current crossing.
    fn claim(&self, kind: Kind) -> bool {
        let mut s = self.lock();
        let now = Instant::now();
        if let Some((prev, at)) = s.last_shown {
            if prev == kind && now.saturating_duration_since(at) < BURST_COALESCE {
                return false;
            }
        }
        s.last_shown = Some((kind, now));
        true
    }

    fn say_hello(&self) {
        let (target, name) = {
            let s = self.lock();
            if s.role != KvmRole::Client || !s.cfg.enabled {
                return;
            }
            let target = non_empty(&s.cfg.peer).or_else(|| s.server.clone());
            (target, s.my_name.clone().unwrap_or_else(|| "client".into()))
        };
        if let (Some(link), Some(t)) = (&self.link, target) {
            link.hello(&t, &name);
        }
    }

    fn on_core_event(&self, ev: &CoreEvent) {
        match ev {
            CoreEvent::SettingsFile(path) => {
                let name = std::fs::read_to_string(path).ok().and_then(|t| kvm::screen_name_from_settings(&t));
                let mut s = self.lock();
                if s.configured_name.is_none() && name.is_some() {
                    log::info!("effect: this screen is {:?} (from {path})", name.as_deref().unwrap_or(""));
                    s.my_name = name;
                }
            }
            CoreEvent::ServerAddress(addr) => {
                self.lock().server = Some(addr.clone());
                self.say_hello();
            }
            CoreEvent::Switch { from, to, x, y } => self.on_switch(from, to, *x, *y),
            CoreEvent::Presence { entered } => self.on_presence(*entered),
        }
    }

    /// Client side: our own core logged that the cursor entered or left.
    fn on_presence(&self, entered: bool) {
        let look = {
            let s = self.lock();
            if s.role != KvmRole::Client || !s.cfg.enabled {
                return;
            }
            Look::from_config(&s.cfg)
        };
        let kind = if entered { Kind::Entry } else { Kind::Exit };
        if !self.claim(kind) {
            return;
        }
        let point = if entered {
            std::thread::sleep(ENTRY_SETTLE);
            cursor_position()
        } else {
            self.trail.exit_point(Instant::now())
        };
        if let Some((x, y)) = point {
            self.burst(x, y, kind, look);
        } else {
            log::debug!("effect: no cursor position for {kind:?}");
        }
    }

    /// Server side: a switch seen in our own core's log.
    fn on_switch(&self, from: &str, to: &str, x: i32, y: i32) {
        let now = Instant::now();
        let (my_name, look, peer) = {
            let s = self.lock();
            if s.role != KvmRole::Server || !s.cfg.enabled {
                return;
            }
            (s.my_name.clone(), Look::from_config(&s.cfg), s.cfg.peer.clone())
        };
        let clients = self.link.as_ref().map(Link::client_names).unwrap_or_default();
        let is_me = |n: &str| match &my_name {
            Some(m) => m.eq_ignore_ascii_case(n),
            // unknown own name: every screen that is not a known client is us
            None => !clients.iter().any(|c| c.eq_ignore_ascii_case(n)),
        };
        let send = |m: Msg| {
            if let Some(l) = &self.link {
                l.to_clients(&m, &peer);
            }
        };
        if is_me(from) {
            if let Some((ex, ey)) = self.trail.exit_point(now) {
                self.burst(ex, ey, Kind::Exit, look);
            }
            send(Msg::Enter { x, y, name: to.to_string() });
        } else if is_me(to) {
            self.burst(f64::from(x), f64::from(y), Kind::Entry, look);
            send(Msg::Leave { x, y, name: from.to_string() });
        } else {
            send(Msg::Leave { x, y, name: from.to_string() });
            send(Msg::Enter { x, y, name: to.to_string() });
        }
    }

    /// Client side: a message from the server.
    fn on_msg(&self, msg: Msg) {
        let (look, filter) = {
            let s = self.lock();
            if s.role != KvmRole::Client || !s.cfg.enabled {
                return;
            }
            (Look::from_config(&s.cfg), s.configured_name.clone())
        };
        let for_me = |n: &str| filter.as_deref().map_or(true, |f| f.eq_ignore_ascii_case(n));
        match msg {
            Msg::Hello { .. } => {}
            Msg::Enter { x, y, name } if for_me(&name) => {
                if !self.claim(Kind::Entry) {
                    return;
                }
                std::thread::sleep(ENTRY_SETTLE);
                let (px, py) = cursor_position().unwrap_or((f64::from(x), f64::from(y)));
                self.burst(px, py, Kind::Entry, look);
            }
            Msg::Leave { name, .. } if for_me(&name) => {
                if !self.claim(Kind::Exit) {
                    return;
                }
                if let Some((px, py)) = self.trail.exit_point(Instant::now()) {
                    self.burst(px, py, Kind::Exit, look);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_from_config_clamps_and_falls_back() {
        let mut c = EffectConfig { duration_ms: 10, color: "bad".into(), ..EffectConfig::default() };
        let l = Look::from_config(&c);
        assert_eq!(l.duration, Duration::from_millis(50));
        assert_eq!(l.rgb, (0x38, 0xBD, 0xF8));
        c.color = "#FF0000".into();
        c.duration_ms = 350;
        assert_eq!(Look::from_config(&c).rgb, (255, 0, 0));
    }

    #[test]
    fn server_shows_entry_burst_for_its_own_screen() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let kvm = KvmConfig {
            role: KvmRole::Server,
            screen_name: "PC".into(),
            effect: EffectConfig { port: 0, ..EffectConfig::default() },
            ..KvmConfig::default()
        };
        let fx = Effects::start(&kvm, Arc::new(move |b| s2.lock().unwrap().push(b)));
        fx.on_core_event(&CoreEvent::Switch { from: "Mac".into(), to: "PC".into(), x: 3829, y: 260 });
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].x, got[0].y, got[0].kind), (3829.0, 260.0, Kind::Entry));

        // disabled: nothing
        let mut off = kvm.clone();
        off.effect.enabled = false;
        fx.set_config(&off);
        fx.on_core_event(&CoreEvent::Switch { from: "Mac".into(), to: "PC".into(), x: 1, y: 1 });
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn server_ignores_client_presence_lines() {
        let seen = Arc::new(Mutex::new(0u32));
        let s2 = seen.clone();
        let kvm = KvmConfig {
            role: KvmRole::Server,
            effect: EffectConfig { port: 0, ..EffectConfig::default() },
            ..KvmConfig::default()
        };
        let fx = Effects::start(&kvm, Arc::new(move |_| *s2.lock().unwrap() += 1));
        fx.on_core_event(&CoreEvent::Presence { entered: true });
        fx.on_core_event(&CoreEvent::Presence { entered: false });
        assert_eq!(*seen.lock().unwrap(), 0);
    }

    #[test]
    fn client_plays_its_own_enter_and_coalesces_the_repeat() {
        if cursor_position().is_none() {
            return;
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let kvm = KvmConfig {
            role: KvmRole::Client,
            effect: EffectConfig { port: 0, ..EffectConfig::default() },
            ..KvmConfig::default()
        };
        let fx = Effects::start(&kvm, Arc::new(move |b| s2.lock().unwrap().push(b.kind)));
        fx.on_core_event(&CoreEvent::Presence { entered: true });
        fx.on_core_event(&CoreEvent::Presence { entered: true });
        let got = seen.lock().unwrap().clone();
        assert_eq!(got, vec![Kind::Entry]);

        let mut off = kvm.clone();
        off.effect.enabled = false;
        fx.set_config(&off);
        fx.on_core_event(&CoreEvent::Presence { entered: false });
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
