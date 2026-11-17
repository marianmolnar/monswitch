//! Other platforms: no clipboard integration (transfers still work via the CLI).

use std::io;

use super::{Callbacks, RemoteFiles, RunOnMain};

pub struct Clipboard;

impl Clipboard {
    pub fn start(_callbacks: Callbacks, _run_on_main: Option<RunOnMain>) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clipboard file transfer is not supported on this platform",
        ))
    }

    pub fn offer_remote(&self, _files: RemoteFiles) {}

    pub fn forget_remote(&self, _id: u64) {}
}
