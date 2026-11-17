//! Core of monswitch: DDC/CI access, capabilities parsing, config and probing.

mod backend;
pub mod caps;
pub mod config;
pub mod controller;
pub mod error;
pub mod probe;

use serde::{Deserialize, Serialize};

pub use backend::{DdcSession, Monitor, WORKER_THREAD_NAME};
pub use caps::Capabilities;
pub use config::Config;
pub use controller::{connect_monitor, connect_session, Controller};
pub use error::{Error, Result};

/// One monitor as seen by the DDC backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub backend: String,
    pub id: String,
    pub manufacturer: Option<String>,
    /// Model name from EDID / the OS (may be generic on Windows).
    pub model_name: Option<String>,
    pub serial: Option<String>,
    /// `model(...)` from the capabilities string, once it has been read.
    pub caps_model: Option<String>,
}

impl MonitorInfo {
    /// Best available model name.
    pub fn model(&self) -> &str {
        self.caps_model
            .as_deref()
            .or(self.model_name.as_deref())
            .unwrap_or("?")
    }

    pub fn matches_model(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        [self.caps_model.as_deref(), self.model_name.as_deref()]
            .into_iter()
            .flatten()
            .any(|n| n.to_lowercase().contains(&needle))
    }
}

/// A VCP reading. `current`/`maximum` are the 16-bit MCCS values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcpReading {
    pub code: u8,
    /// 0 = set parameter, 1 = momentary.
    pub ty: u8,
    pub current: u16,
    pub maximum: u16,
}

/// How to pick the monitor to control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Index(usize),
    Model(String),
}

impl Selector {
    /// A plain number is an index from `monswitch list`, anything else a model name.
    pub fn parse(s: &str) -> Self {
        s.trim()
            .parse::<usize>()
            .map(Selector::Index)
            .unwrap_or_else(|_| Selector::Model(s.trim().to_string()))
    }
}

/// Control of one selected monitor.
pub trait DisplayControl {
    /// All monitors visible to the backend.
    fn list(&self) -> Vec<MonitorInfo>;
    /// The monitor this handle controls.
    fn info(&self) -> &MonitorInfo;
    fn capabilities(&self) -> Result<Capabilities>;
    fn get_vcp(&self, code: u8) -> Result<VcpReading>;
    fn set_vcp(&self, code: u8, value: u16) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_parse() {
        assert_eq!(Selector::parse("1"), Selector::Index(1));
        assert_eq!(Selector::parse("PG27"), Selector::Model("PG27".into()));
    }
}
