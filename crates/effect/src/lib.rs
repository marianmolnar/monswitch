//! Edge transition effect for the software KVM.
//!
//! When the cursor crosses from one machine to the other, the machine it
//! leaves shows a short burst at the exit point and the machine it enters a
//! smaller one at the entry point.
//!
//! - The Deskflow **server** sees every switch in deskflow-core's log
//!   ([`kvm::CoreEvent::Switch`]). It shows its own burst and tells the client
//!   over a tiny UDP message ([`proto`]).
//! - The **client** says hello to the server every few seconds so the server
//!   knows where to send; the server address comes from the client core's log
//!   (or `[kvm.effect].peer`).
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

/// Displays a burst. The tray app forwards it to the thread that owns the
/// overlay (the main thread on macOS).
pub type ShowFn = Arc<dyn Fn(Burst) + Send + Sync>;

/// How often the client says hello.
const HELLO_EVERY: Duration = Duration::from_secs(3);
/// The client's cursor is warped to the entry point right after the switch.
const ENTRY_SETTLE: Duration = Duration::from_millis(25);

struct State {
    role: KvmRole,
    cfg: EffectConfig,
    /// `[kvm].screen_name` (explicit) – also filters messages on the client.
    configured_name: Option<String>,
    /// This machine's Deskflow screen name, if known.
    my_name: Option<String>,
    /// Client: server address from deskflow-core's log.
    server: Option<String>,
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
        let edge = edge_at(x, y);
        log::debug!("effect: {kind:?} burst at {x:.0},{y:.0} ({edge:?})");
        (self.show)(Burst { x, y, kind, look, edge });
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
                std::thread::sleep(ENTRY_SETTLE);
                let (px, py) = cursor_position().unwrap_or((f64::from(x), f64::from(y)));
                self.burst(px, py, Kind::Entry, look);
            }
            Msg::Leave { name, .. } if for_me(&name) => {
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
}
