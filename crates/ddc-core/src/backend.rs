//! ddc-hi backend (Windows: dxva2/nvapi, macOS: IOAVService).
//!
//! All DDC I/O runs on one dedicated worker thread that owns the `Display`
//! handles. Callers talk to it over a channel and wait with a timeout, so a
//! hung DDC transaction can never block the caller forever. The worker also
//! enforces the minimum pause between commands and caches capabilities.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ddc_hi::{Ddc, Display};

use crate::caps::Capabilities;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::{DisplayControl, MonitorInfo, Selector, VcpReading};

enum Cmd {
    Enumerate,
    CapsString(usize),
    Get(usize, u8),
    Set(usize, u8, u16),
}

enum Reply {
    Monitors(Vec<MonitorInfo>),
    Caps(String),
    Value(VcpReading),
    Done,
}

type JobResult = std::result::Result<Reply, String>;

struct Job {
    cmd: Cmd,
    reply: Sender<JobResult>,
}

fn worker(rx: Receiver<Job>, min_delay: Duration) {
    let mut displays: Vec<Display> = Display::enumerate();
    let mut caps_cache: HashMap<usize, String> = HashMap::new();
    let mut last_op: Option<Instant> = None;

    for job in rx {
        let result = match job.cmd {
            Cmd::Enumerate => Ok(Reply::Monitors(
                displays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| info_of(i, d))
                    .collect(),
            )),
            Cmd::CapsString(idx) => match caps_cache.get(&idx) {
                Some(s) => Ok(Reply::Caps(s.clone())),
                None => with_display(&mut displays, idx, &mut last_op, min_delay, |d| {
                    d.handle
                        .capabilities_string()
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .map_err(|e| e.to_string())
                })
                .map(|s| {
                    caps_cache.insert(idx, s.clone());
                    Reply::Caps(s)
                }),
            },
            Cmd::Get(idx, code) => with_display(&mut displays, idx, &mut last_op, min_delay, |d| {
                d.handle
                    .get_vcp_feature(code)
                    .map(|v| {
                        Reply::Value(VcpReading {
                            code,
                            ty: v.ty,
                            current: v.value(),
                            maximum: v.maximum(),
                        })
                    })
                    .map_err(|e| e.to_string())
            }),
            Cmd::Set(idx, code, value) => {
                with_display(&mut displays, idx, &mut last_op, min_delay, |d| {
                    d.handle
                        .set_vcp_feature(code, value)
                        .map(|_| Reply::Done)
                        .map_err(|e| e.to_string())
                })
            }
        };
        // The caller may have given up (timeout); that's fine.
        let _ = job.reply.send(result);
    }
}

fn with_display<T>(
    displays: &mut [Display],
    idx: usize,
    last_op: &mut Option<Instant>,
    min_delay: Duration,
    f: impl FnOnce(&mut Display) -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let d = displays
        .get_mut(idx)
        .ok_or_else(|| format!("monitor index {idx} out of range"))?;
    if let Some(t) = *last_op {
        let elapsed = t.elapsed();
        if elapsed < min_delay {
            thread::sleep(min_delay - elapsed);
        }
    }
    // Backend crates can panic on malformed replies (seen: ddc-i2c index out
    // of bounds via nvapi in PbP mode). Contain it so the worker survives and
    // only this operation fails.
    let r = panic::catch_unwind(AssertUnwindSafe(|| f(d))).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Err(format!("DDC backend panicked: {msg}"))
    });
    *last_op = Some(Instant::now());
    r
}

/// Name of the DDC worker thread; the CLI uses it to keep backend panics
/// (which are converted into errors) out of the console.
pub const WORKER_THREAD_NAME: &str = "ddc-worker";

fn info_of(index: usize, d: &Display) -> MonitorInfo {
    let i = &d.info;
    MonitorInfo {
        index,
        backend: i.backend.to_string(),
        id: i.id.clone(),
        manufacturer: i.manufacturer_id.clone(),
        model_name: i.model_name.clone(),
        serial: i
            .serial_number
            .clone()
            .or_else(|| i.serial.map(|s| s.to_string())),
        caps_model: None,
    }
}

#[derive(Clone)]
struct Client {
    tx: Sender<Job>,
    timeout: Duration,
    caps_timeout: Duration,
    retries: u32,
    retry_pause: Duration,
}

impl Client {
    fn call(&self, op: &str, timeout: Duration, retries: u32, make: impl Fn() -> Cmd) -> Result<Reply> {
        let mut last_err = Error::WorkerGone;
        for attempt in 0..=retries {
            if attempt > 0 {
                log::debug!("retrying {op} (attempt {})", attempt + 1);
                thread::sleep(self.retry_pause);
            }
            let (rtx, rrx) = mpsc::channel();
            self.tx
                .send(Job { cmd: make(), reply: rtx })
                .map_err(|_| Error::WorkerGone)?;
            match rrx.recv_timeout(timeout) {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(msg)) => {
                    log::debug!("{op} failed: {msg}");
                    last_err = Error::Ddc { op: op.to_string(), msg };
                }
                Err(RecvTimeoutError::Timeout) => {
                    log::debug!("{op} timed out");
                    last_err = Error::Timeout {
                        op: op.to_string(),
                        ms: timeout.as_millis(),
                    };
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Error::WorkerGone),
            }
        }
        Err(last_err)
    }

    fn caps_string(&self, idx: usize) -> Result<String> {
        match self.call("capabilities", self.caps_timeout, self.retries, || Cmd::CapsString(idx))? {
            Reply::Caps(s) => Ok(s),
            _ => Err(unexpected("capabilities")),
        }
    }

    fn get(&self, idx: usize, code: u8) -> Result<VcpReading> {
        let op = format!("get VCP 0x{code:02X}");
        match self.call(&op, self.timeout, self.retries, || Cmd::Get(idx, code))? {
            Reply::Value(v) => Ok(v),
            _ => Err(unexpected(&op)),
        }
    }

    fn set(&self, idx: usize, code: u8, value: u16) -> Result<()> {
        let op = format!("set VCP 0x{code:02X}={value}");
        match self.call(&op, self.timeout, self.retries, || Cmd::Set(idx, code, value))? {
            Reply::Done => Ok(()),
            _ => Err(unexpected(&op)),
        }
    }
}

fn unexpected(op: &str) -> Error {
    Error::Ddc {
        op: op.to_string(),
        msg: "unexpected reply from worker".into(),
    }
}

/// A connection to all DDC-capable monitors.
pub struct DdcSession {
    client: Client,
    monitors: Vec<MonitorInfo>,
    preferred_backend: String,
    input_source: u8,
}

impl DdcSession {
    pub fn connect(cfg: &Config) -> Result<Self> {
        let d = &cfg.ddc;
        let (tx, rx) = mpsc::channel();
        let min_delay = Duration::from_millis(d.min_delay_ms);
        thread::Builder::new()
            .name(WORKER_THREAD_NAME.into())
            .spawn(move || worker(rx, min_delay))
            .map_err(|e| Error::Ddc {
                op: "spawn worker".into(),
                msg: e.to_string(),
            })?;

        let client = Client {
            tx,
            timeout: Duration::from_millis(d.timeout_ms),
            caps_timeout: Duration::from_millis(d.caps_timeout_ms),
            retries: d.retries,
            retry_pause: min_delay,
        };

        // Enumeration happens once when the worker starts; retrying would
        // only return the same list, so no retries here.
        let enum_timeout = Duration::from_millis(d.enumerate_timeout_ms);
        let monitors = match client.call("enumerate", enum_timeout, 0, || Cmd::Enumerate)? {
            Reply::Monitors(m) => m,
            _ => return Err(unexpected("enumerate")),
        };
        if monitors.is_empty() {
            return Err(Error::NoMonitors);
        }
        Ok(Self {
            client,
            monitors,
            preferred_backend: cfg.monitor.backend.clone(),
            input_source: cfg.vcp.input_source,
        })
    }

    pub fn monitors(&self) -> &[MonitorInfo] {
        &self.monitors
    }

    pub fn capabilities_string_at(&self, idx: usize) -> Result<String> {
        self.client.caps_string(idx)
    }

    pub fn get_vcp_at(&self, idx: usize, code: u8) -> Result<VcpReading> {
        self.client.get(idx, code)
    }

    pub fn set_vcp_at(&self, idx: usize, code: u8, value: u16) -> Result<()> {
        self.client.set(idx, code, value)
    }

    /// Reads the capabilities string to learn the real model name
    /// (Windows often reports "Generic PnP Monitor" via the OS).
    pub fn identify(&mut self, idx: usize) -> Result<Option<String>> {
        let caps = Capabilities::parse(&self.client.caps_string(idx)?);
        if let Some(m) = self.monitors.get_mut(idx) {
            m.caps_model = caps.model.clone();
        }
        Ok(caps.model)
    }

    /// Finds a monitor index for the selector.
    pub fn find(&mut self, sel: &Selector) -> Result<usize> {
        match sel {
            Selector::Index(i) => {
                if *i < self.monitors.len() {
                    Ok(*i)
                } else {
                    Err(Error::MonitorNotFound(i.to_string()))
                }
            }
            Selector::Model(name) => {
                // Walk monitors in backend-preference order. A monitor is a
                // candidate when its EDID name or capabilities model matches;
                // the first candidate that can actually read the input-source
                // VCP wins. This makes e.g. winapi fall back to nvapi when
                // dxva2 cannot talk DDC over the NVIDIA DisplayPort link.
                let mut first_match: Option<usize> = None;
                for i in self.preference_order() {
                    if !self.monitors[i].matches_model(name) {
                        if let Err(e) = self.identify(i) {
                            log::info!(
                                "monitor #{i} ({}): cannot read capabilities: {e}",
                                self.monitors[i].backend
                            );
                            continue;
                        }
                        if !self.monitors[i].matches_model(name) {
                            continue;
                        }
                    }
                    first_match.get_or_insert(i);
                    match self.client.get(i, self.input_source) {
                        Ok(_) => {
                            if first_match != Some(i) {
                                log::warn!(
                                    "falling back to monitor #{i} via {}",
                                    self.monitors[i].backend
                                );
                            }
                            return Ok(i);
                        }
                        Err(e) => log::warn!(
                            "monitor #{i} ({}) matches '{name}' but cannot read VCP 0x{:02X}: {e}",
                            self.monitors[i].backend,
                            self.input_source
                        ),
                    }
                }
                // Nothing answered; return the best match so the caller gets
                // the real DDC error rather than "not found".
                first_match.ok_or_else(|| Error::MonitorNotFound(name.clone()))
            }
        }
    }

    /// Monitor indices with the preferred backend first, stable otherwise.
    fn preference_order(&self) -> Vec<usize> {
        let pref = self.preferred_backend.trim().to_lowercase();
        let mut order: Vec<usize> = (0..self.monitors.len()).collect();
        order.sort_by_key(|&i| !pref.is_empty() && self.monitors[i].backend != pref);
        order
    }

    pub fn select(&mut self, sel: &Selector) -> Result<Monitor> {
        let idx = self.find(sel)?;
        Ok(Monitor {
            client: self.client.clone(),
            info: self.monitors[idx].clone(),
            all: self.monitors.clone(),
        })
    }
}

/// Handle to one selected monitor.
#[derive(Clone)]
pub struct Monitor {
    client: Client,
    info: MonitorInfo,
    all: Vec<MonitorInfo>,
}

impl DisplayControl for Monitor {
    fn list(&self) -> Vec<MonitorInfo> {
        self.all.clone()
    }

    fn info(&self) -> &MonitorInfo {
        &self.info
    }

    fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities::parse(&self.client.caps_string(self.info.index)?))
    }

    fn get_vcp(&self, code: u8) -> Result<VcpReading> {
        self.client.get(self.info.index, code)
    }

    fn set_vcp(&self, code: u8, value: u16) -> Result<()> {
        self.client.set(self.info.index, code, value)
    }
}
