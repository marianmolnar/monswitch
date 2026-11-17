//! File transfer between the two machines – its own channel, not Deskflow.
//!
//! - **Transport** ([`Service`]): TLS over TCP on the LAN. Both machines
//!   listen on `[xfer].port`; each request is one connection. TLS reuses
//!   Deskflow's pairing (its certificate and trusted fingerprints, see
//!   [`trust`]), both sides authenticate. Files stream in chunks with no
//!   size limit, report progress, and resume after network errors.
//! - **Clipboard** ([`clip`]): copying files offers them to the other
//!   machine, which puts promises on its clipboard; the transfer starts
//!   only when they are pasted.
//! - [`Bridge`] wires both together for the tray app.

pub mod clip;
pub mod offer;
pub mod proto;
pub mod trust;

#[cfg(feature = "net")]
mod net;

use std::io;

#[cfg(feature = "net")]
pub use net::{clean_cache, load_tls, tls_configs, Client, Identity, RemoteReader, Service};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Deskflow's TLS certificate not found – enable TLS in Deskflow and pair the machines, or set [xfer].tls_dir")]
    NoTlsDir,
    #[error("TLS: {0}")]
    Tls(String),
    #[error("the other machine's address is not known yet (set [xfer].peer or connect the KVM)")]
    NoPeer,
    #[error("other machine: {0}")]
    Remote(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

/// Transfer progress of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub dir: Direction,
    pub id: u64,
    /// Entry path (`folder/file.ext`).
    pub name: String,
    pub done: u64,
    pub total: u64,
}

/// Files the other machine offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOffer {
    pub id: u64,
    /// The other machine's name.
    pub from: String,
    pub entries: Vec<proto::Entry>,
    /// Address the offer came from (`None` = the current peer).
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The other machine copied files.
    RemoteOffer(RemoteOffer),
    /// … and its clipboard no longer holds them.
    RemoteWithdrawn {
        id: u64,
    },
    Progress(Progress),
    /// A file finished (or failed).
    Done {
        dir: Direction,
        id: u64,
        name: String,
        result: Result<(), String>,
    },
    /// The other machine's address became known.
    Peer(String),
}

#[cfg(feature = "net")]
pub use bridge::{Bridge, BridgeOptions};

#[cfg(feature = "net")]
mod bridge {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use ddc_core::config::XferConfig;

    use crate::clip::{Callbacks, Clipboard, RunOnMain};
    use crate::{clean_cache, load_tls, Error, Event, Service};

    pub struct BridgeOptions<'a> {
        pub cfg: &'a XferConfig,
        /// This machine's name shown to the other one.
        pub name: &'a str,
        /// Deskflow settings file (to find its TLS folder).
        pub deskflow_settings: Option<&'a Path>,
        /// Runs closures on the UI thread (needed for the macOS clipboard).
        pub run_on_main: Option<RunOnMain>,
        /// Where pasted folders/files are downloaded first on macOS.
        pub cache_dir: PathBuf,
    }

    /// Transfer service + clipboard integration. Dropping it stops both.
    pub struct Bridge {
        service: Service,
        _clipboard: Option<Arc<Clipboard>>,
    }

    impl Bridge {
        /// Starts the service and, if `[xfer].clipboard`, the clipboard
        /// bridge. `on_event` sees every [`Event`] (progress for the UI).
        pub fn start(
            opts: BridgeOptions<'_>,
            on_event: impl Fn(&Event) + Send + 'static,
        ) -> Result<Self, Error> {
            let (identity, trust) = load_tls(opts.cfg, opts.deskflow_settings)?;
            let (tx, rx) = mpsc::channel();
            let service = Service::start(opts.cfg, opts.name, &identity, trust, tx)?;
            clean_cache(&opts.cache_dir, Duration::from_secs(24 * 3600));

            let clipboard = if opts.cfg.clipboard {
                let (copy_svc, clear_svc) = (service.client(), service.client());
                let callbacks = Callbacks {
                    on_copy: Box::new(move |paths| copy_svc.offer_local(paths)),
                    on_clear: Box::new(move || clear_svc.withdraw_local()),
                };
                match Clipboard::start(callbacks, opts.run_on_main) {
                    Ok(c) => Some(Arc::new(c)),
                    Err(e) => {
                        log::warn!("xfer: clipboard integration off: {e}");
                        None
                    }
                }
            } else {
                None
            };

            let client = service.client();
            let clip = clipboard.clone();
            let cache = opts.cache_dir;
            let spawned = std::thread::Builder::new()
                .name("xfer-events".into())
                .spawn(move || loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(ev) => {
                            if let Some(c) = &clip {
                                match &ev {
                                    Event::RemoteOffer(o) => {
                                        c.offer_remote(client.remote_files(o, cache.clone()))
                                    }
                                    Event::RemoteWithdrawn { id } => c.forget_remote(*id),
                                    _ => {}
                                }
                            }
                            on_event(&ev);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) if !client.is_stopped() => {}
                        Err(_) => break,
                    }
                });
            if let Err(e) = spawned {
                log::warn!("xfer: cannot start event thread: {e}");
            }
            Ok(Self {
                service,
                _clipboard: clipboard,
            })
        }

        pub fn service(&self) -> &Service {
            &self.service
        }

        /// The other machine's address (e.g. from Deskflow's log).
        pub fn set_peer(&self, host: &str) {
            self.service.set_peer(host);
        }
    }
}
