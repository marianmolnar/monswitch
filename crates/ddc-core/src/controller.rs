//! Resilient access to the configured monitor.
//!
//! Switching PiP/PbP (and sometimes inputs) makes the OS re-detect the
//! monitor: on macOS it vanishes for ~3 s after `pbp off`, on Windows the
//! dxva2 handle gets invalidated. A single retry is not enough, so
//! discovery keeps trying for `[ddc].rediscover_timeout_ms`.

use std::thread;
use std::time::{Duration, Instant};

use crate::backend::{DdcSession, Monitor};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::{DisplayControl, Selector};

/// Errors that can be caused by the monitor briefly disappearing.
fn is_transient(e: &Error) -> bool {
    matches!(
        e,
        Error::NoMonitors
            | Error::MonitorNotFound(_)
            | Error::Timeout { .. }
            | Error::Ddc { .. }
            | Error::WorkerGone
    )
}

/// Retries `attempt` until it succeeds, fails permanently, or the
/// rediscovery window from config runs out.
fn with_rediscovery<T>(cfg: &Config, what: &str, mut attempt: impl FnMut() -> Result<T>) -> Result<T> {
    let window = Duration::from_millis(cfg.ddc.rediscover_timeout_ms);
    let interval = Duration::from_millis(cfg.ddc.rediscover_interval_ms.max(1));
    let deadline = Instant::now() + window;
    let mut tries = 0u32;
    loop {
        tries += 1;
        match attempt() {
            Ok(v) => {
                if tries > 1 {
                    log::info!("{what}: succeeded after {tries} attempts");
                }
                return Ok(v);
            }
            Err(e) if is_transient(&e) && Instant::now() + interval < deadline => {
                log::info!("{what}: {e} – retrying");
                thread::sleep(interval);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Connects to all monitors, waiting while none are visible.
pub fn connect_session(cfg: &Config) -> Result<DdcSession> {
    with_rediscovery(cfg, "connect", || DdcSession::connect(cfg))
}

/// Connects and selects one monitor, waiting while it is not visible.
pub fn connect_monitor(cfg: &Config, selector: &Selector) -> Result<Monitor> {
    with_rediscovery(cfg, "find monitor", || DdcSession::connect(cfg)?.select(selector))
}

/// Long-lived handle for apps (tray): keeps a monitor connection and
/// transparently reconnects when an operation fails.
pub struct Controller {
    cfg: Config,
    selector: Selector,
    monitor: Option<Monitor>,
}

impl Controller {
    pub fn new(cfg: Config, selector: Selector) -> Self {
        Self {
            cfg,
            selector,
            monitor: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Replaces the config (e.g. after the settings window saved it) and
    /// forces a reconnect.
    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
        self.monitor = None;
    }

    /// Drops the connection; the next operation reconnects.
    pub fn reset(&mut self) {
        self.monitor = None;
    }

    /// The connected monitor, connecting (with rediscovery) if needed.
    pub fn monitor(&mut self) -> Result<&Monitor> {
        if self.monitor.is_none() {
            self.monitor = Some(connect_monitor(&self.cfg, &self.selector)?);
        }
        self.monitor.as_ref().ok_or(Error::WorkerGone)
    }

    /// Runs `op`; on a transient failure reconnects (waiting for the
    /// monitor to come back) and runs it once more.
    pub fn run<T>(&mut self, mut op: impl FnMut(&Monitor) -> Result<T>) -> Result<T> {
        let first = op(self.monitor()?);
        match first {
            Err(e) if is_transient(&e) => {
                log::info!("operation failed ({e}); reconnecting");
                self.monitor = None;
                op(self.monitor()?)
            }
            other => other,
        }
    }

    pub fn get_vcp(&mut self, code: u8) -> Result<crate::VcpReading> {
        self.run(|m| m.get_vcp(code))
    }

    pub fn set_vcp(&mut self, code: u8, value: u16) -> Result<()> {
        self.run(|m| m.set_vcp(code, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(timeout_ms: u64) -> Config {
        let mut c = Config::defaults().unwrap();
        c.ddc.rediscover_timeout_ms = timeout_ms;
        c.ddc.rediscover_interval_ms = 10;
        c
    }

    #[test]
    fn retries_transient_until_success() {
        let mut n = 0;
        let r = with_rediscovery(&cfg(1000), "t", || {
            n += 1;
            if n < 3 {
                Err(Error::MonitorNotFound("x".into()))
            } else {
                Ok(n)
            }
        });
        assert_eq!(r.unwrap(), 3);
    }

    #[test]
    fn gives_up_after_window() {
        let start = Instant::now();
        let r: Result<()> = with_rediscovery(&cfg(100), "t", || Err(Error::NoMonitors));
        assert!(matches!(r, Err(Error::NoMonitors)));
        assert!(start.elapsed() < Duration::from_millis(1000));
    }

    #[test]
    fn permanent_errors_fail_immediately() {
        let mut n = 0;
        let r: Result<()> = with_rediscovery(&cfg(1000), "t", || {
            n += 1;
            Err(Error::Config("bad".into()))
        });
        assert!(r.is_err());
        assert_eq!(n, 1);
    }
}
