//! Software KVM for PbP mode, built on Deskflow.
//!
//! Deskflow itself is configured once in its own GUI (screens, layout,
//! clipboard, TLS trust). This crate only decides *when* `deskflow-core`
//! runs:
//! - **server** (the machine that gets the monitor's USB keyboard/mouse in
//!   PbP): runs only while PiP/PbP is on – otherwise the cursor would leave
//!   into a screen that is not visible;
//! - **client**: runs all the time and simply waits for the server.
//!
//! The core's log output is read line by line and turned into
//! [`CoreEvent`]s (screen switches for the edge effect).

pub mod events;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub use events::{parse_line, screen_name_from_settings, CoreEvent};

use ddc_core::config::{KvmConfig, KvmRole};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("deskflow-core not found (install Deskflow or set [kvm].deskflow_core)")]
    CoreNotFound,
    #[error("cannot start deskflow-core: {0}")]
    Spawn(std::io::Error),
}

/// Default install locations of `deskflow-core`.
fn default_core_paths() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut v = Vec::new();
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(p) = std::env::var_os(var) {
                v.push(PathBuf::from(p).join("Deskflow").join("deskflow-core.exe"));
            }
        }
        v
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/Applications/Deskflow.app/Contents/MacOS/deskflow-core"),
            PathBuf::from("/opt/homebrew/bin/deskflow-core"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/bin/deskflow-core"),
            PathBuf::from("/usr/local/bin/deskflow-core"),
        ]
    }
}

/// `[kvm].deskflow_core` if set, otherwise the first default path that exists.
pub fn find_core(cfg: &KvmConfig) -> Option<PathBuf> {
    let configured = cfg.deskflow_core.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.exists().then_some(p);
    }
    default_core_paths().into_iter().find(|p| p.exists())
}

/// What the supervisor wants given the role and the PbP state.
pub fn should_run(role: KvmRole, pbp_on: bool) -> bool {
    match role {
        KvmRole::Off => false,
        KvmRole::Server => pbp_on,
        KvmRole::Client => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvmState {
    Off,
    Stopped,
    Running { pid: u32 },
    Failed(String),
}

/// Owns the `deskflow-core` child process.
pub struct Supervisor {
    cfg: KvmConfig,
    child: Option<Child>,
    last_error: Option<String>,
    /// Do not respawn a crashing core more often than this.
    next_start: Option<Instant>,
    /// Receives events parsed from the core's output.
    events: Option<Sender<CoreEvent>>,
}

/// Settings that require restarting deskflow-core when they change.
fn core_settings(c: &KvmConfig) -> (KvmRole, &str, &str) {
    (c.role, c.deskflow_core.trim(), c.settings.trim())
}

impl Supervisor {
    pub fn new(cfg: KvmConfig) -> Self {
        Self {
            cfg,
            child: None,
            last_error: None,
            next_start: None,
            events: None,
        }
    }

    /// Sends [`CoreEvent`]s from every core started from now on to `tx`.
    pub fn with_events(mut self, tx: Sender<CoreEvent>) -> Self {
        self.events = Some(tx);
        self
    }

    pub fn config(&self) -> &KvmConfig {
        &self.cfg
    }

    /// New config (from the settings window): restart the core only when a
    /// setting it uses changed.
    pub fn set_config(&mut self, cfg: KvmConfig) {
        if core_settings(&cfg) != core_settings(&self.cfg) {
            self.stop();
            self.last_error = None;
            self.next_start = None;
        }
        self.cfg = cfg;
    }

    pub fn state(&mut self) -> KvmState {
        self.reap();
        if self.cfg.role == KvmRole::Off {
            return KvmState::Off;
        }
        match (&self.child, &self.last_error) {
            (Some(c), _) => KvmState::Running { pid: c.id() },
            (None, Some(e)) => KvmState::Failed(e.clone()),
            (None, None) => KvmState::Stopped,
        }
    }

    /// Brings the process in line with the role and PbP state. Call it
    /// periodically and after every PbP change.
    pub fn sync(&mut self, pbp_on: bool) {
        self.reap();
        let want = should_run(self.cfg.role, pbp_on);
        match (want, self.child.is_some()) {
            (true, false) => {
                if self.next_start.is_some_and(|t| Instant::now() < t) {
                    return;
                }
                if let Err(e) = self.start() {
                    log::warn!("kvm: {e}");
                    self.last_error = Some(e.to_string());
                    self.next_start = Some(Instant::now() + self.retry_delay());
                }
            }
            (false, true) => self.stop(),
            _ => {}
        }
    }

    fn retry_delay(&self) -> Duration {
        Duration::from_millis(self.cfg.restart_delay_ms.max(1000))
    }

    fn start(&mut self) -> Result<(), Error> {
        let core = find_core(&self.cfg).ok_or(Error::CoreNotFound)?;
        let mode = match self.cfg.role {
            KvmRole::Server => "server",
            KvmRole::Client => "client",
            KvmRole::Off => return Ok(()),
        };
        let mut cmd = Command::new(&core);
        cmd.arg(mode);
        let settings = self.cfg.settings.trim();
        if !settings.is_empty() {
            cmd.arg("--settings").arg(Path::new(settings));
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(Error::Spawn)?;
        log::info!("kvm: started deskflow-core {mode} (pid {})", child.id());
        // Always drain both pipes, otherwise the core blocks once a pipe is full.
        if let Some(out) = child.stdout.take() {
            spawn_reader("deskflow-stdout", out, self.events.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader("deskflow-stderr", err, self.events.clone());
        }
        self.child = Some(child);
        self.last_error = None;
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let pid = c.id();
            if let Err(e) = c.kill() {
                log::warn!("kvm: kill pid {pid}: {e}");
            }
            let _ = c.wait();
            log::info!("kvm: stopped deskflow-core (pid {pid})");
        }
    }

    /// Notices a core that exited on its own (crash, refused to start
    /// because another instance runs, …).
    fn reap(&mut self) {
        let exited = match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(Some(status))) => Some(format!("deskflow-core exited ({status})")),
            Some(Err(e)) => Some(format!("deskflow-core: {e}")),
            _ => None,
        };
        if let Some(msg) = exited {
            log::warn!("kvm: {msg}");
            self.child = None;
            self.last_error = Some(msg);
            self.next_start = Some(Instant::now() + self.retry_delay());
        }
    }
}

/// Reads the core's output until the pipe closes (core exited or killed).
fn spawn_reader(name: &str, pipe: impl Read + Send + 'static, events: Option<Sender<CoreEvent>>) {
    let result = std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // the core writes in the console code page on Windows – lossy is fine
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            log::debug!("deskflow: {line}");
            if let (Some(tx), Some(ev)) = (&events, parse_line(line)) {
                if tx.send(ev).is_err() {
                    // receiver gone: keep draining, stop parsing
                    while matches!(reader.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                        buf.clear();
                    }
                    break;
                }
            }
        }
    });
    if let Err(e) = result {
        log::warn!("kvm: cannot start output reader: {e}");
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_runs_only_in_pbp() {
        assert!(should_run(KvmRole::Server, true));
        assert!(!should_run(KvmRole::Server, false));
        assert!(should_run(KvmRole::Client, false));
        assert!(!should_run(KvmRole::Off, true));
    }

    #[test]
    fn missing_core_reports_failure_and_backs_off() {
        let cfg = KvmConfig {
            role: KvmRole::Client,
            deskflow_core: "Z:/definitely/not/here/deskflow-core".into(),
            ..KvmConfig::default()
        };
        let mut s = Supervisor::new(cfg);
        s.sync(false);
        assert!(matches!(s.state(), KvmState::Failed(_)));
        // backoff: an immediate second sync does not retry
        let before = s.next_start;
        s.sync(false);
        assert_eq!(s.next_start, before);
    }

    #[test]
    fn off_role_never_starts() {
        let mut s = Supervisor::new(KvmConfig::default());
        s.sync(true);
        assert_eq!(s.state(), KvmState::Off);
    }
}
