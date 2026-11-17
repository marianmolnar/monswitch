use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("no monitors with DDC/CI found (is DDC/CI enabled in the monitor OSD?)")]
    NoMonitors,

    #[error("no monitor matches '{0}' (run `monswitch list`)")]
    MonitorNotFound(String),

    #[error("DDC operation '{op}' timed out after {ms} ms")]
    Timeout { op: String, ms: u128 },

    #[error("DDC operation '{op}' failed: {msg}")]
    Ddc { op: String, msg: String },

    #[error("DDC worker thread is not running")]
    WorkerGone,

    #[error("unknown input '{name}' (known aliases: {known})")]
    UnknownInput { name: String, known: String },

    #[error("config error: {0}")]
    Config(String),

    #[error("invalid probe snapshot {path}: {msg}")]
    Snapshot { path: PathBuf, msg: String },

    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
