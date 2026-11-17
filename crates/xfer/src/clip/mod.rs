//! Clipboard file copy/paste between the machines.
//!
//! - Files copied on this machine (Windows `CF_HDROP`, macOS file URLs) are
//!   reported through [`Callbacks::on_copy`] so they can be offered to the
//!   other machine.
//! - Files offered by the other machine are put on this clipboard as
//!   *promises*: nothing is transferred until something pastes them
//!   (Windows virtual files `FileGroupDescriptorW` + `FileContents`;
//!   macOS a lazily provided `public.file-url`).
//!
//! Deskflow syncs the clipboard too: when the cursor crosses, it replaces
//! this clipboard with the other machine's *text* (the file name, or
//! nothing, for copied files). [`State`] recognises that echo and puts the
//! files back, so copy → move cursor → paste works with Deskflow's
//! clipboard sharing left on.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::proto::Entry;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(not(any(windows, target_os = "macos")))]
mod other;
#[cfg(windows)]
mod win;

#[cfg(target_os = "macos")]
use mac as sys;
#[cfg(not(any(windows, target_os = "macos")))]
use other as sys;
#[cfg(windows)]
use win as sys;

/// Opens a sequential reader for entry `i` of a remote offer (the transfer
/// starts when it is first read and resumes by itself after network errors).
pub type Opener = Arc<dyn Fn(u32) -> io::Result<Box<dyn Read + Send>> + Send + Sync>;
/// Downloads top-level entry `i` (a file or a whole folder) into a local
/// cache and returns its path; returns at once when already downloaded.
pub type Materializer = Arc<dyn Fn(u32) -> io::Result<PathBuf> + Send + Sync>;
/// Runs a closure on the UI (main) thread – macOS pasteboard data providers
/// are called there.
pub type RunOnMain = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// Files the other machine offers.
#[derive(Clone)]
pub struct RemoteFiles {
    pub id: u64,
    pub entries: Vec<Entry>,
    pub open: Opener,
    pub materialize: Materializer,
}

impl RemoteFiles {
    pub fn top_level_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.is_top_level())
            .map(|e| e.path.clone())
            .collect()
    }
}

pub struct Callbacks {
    /// Files were copied on this machine.
    pub on_copy: Box<dyn Fn(Vec<PathBuf>) + Send + Sync>,
    /// The clipboard no longer holds the files copied on this machine.
    pub on_clear: Box<dyn Fn() + Send + Sync>,
}

/// What the clipboard changed to, as seen by the platform watcher (changes
/// this process made itself are filtered out before).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Files(Vec<PathBuf>),
    Other {
        /// Plain text on the clipboard, if any.
        text: Option<String>,
        /// Nothing but plain text (or nothing at all) is on the clipboard.
        only_text: bool,
        /// Whether Deskflow set it; `None` = cannot tell (macOS).
        from_deskflow: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Offer these local files to the other machine.
    Offer(Vec<PathBuf>),
    /// Tell the other machine the local files are gone from the clipboard.
    Withdraw,
    /// Deskflow overwrote the clipboard – put the local files back.
    RestoreLocal(Vec<PathBuf>),
    /// Deskflow overwrote the clipboard – put the remote promises back.
    RestoreRemote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Active {
    None,
    Local {
        paths: Vec<PathBuf>,
        names: Vec<String>,
    },
    Remote {
        id: u64,
        names: Vec<String>,
    },
}

/// Which files the clipboard should hold and how to react to changes.
#[derive(Debug)]
pub struct State {
    active: Active,
}

impl Default for State {
    fn default() -> Self {
        Self {
            active: Active::None,
        }
    }
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// True when `text` is what Deskflow carries over for copied files: nothing,
/// or just their names / paths, one per line.
pub fn is_echo(text: Option<&str>, names: &[String]) -> bool {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .all(|line| {
            let last = line
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(line);
            names.iter().any(|n| n.eq_ignore_ascii_case(last))
        })
}

impl State {
    pub fn on_change(&mut self, change: Change) -> Action {
        match change {
            Change::Files(paths) => {
                if matches!(&self.active, Active::Local { paths: p, .. } if *p == paths) {
                    return Action::None;
                }
                let names = paths.iter().map(|p| file_name(p)).collect();
                self.active = Active::Local {
                    paths: paths.clone(),
                    names,
                };
                Action::Offer(paths)
            }
            Change::Other {
                text,
                only_text,
                from_deskflow,
            } => {
                let echo = |names: &[String]| {
                    only_text && from_deskflow != Some(false) && is_echo(text.as_deref(), names)
                };
                match &self.active {
                    Active::None => Action::None,
                    Active::Local { paths, names } if echo(names) => {
                        Action::RestoreLocal(paths.clone())
                    }
                    Active::Remote { names, .. } if echo(names) => Action::RestoreRemote,
                    Active::Local { .. } => {
                        self.active = Active::None;
                        Action::Withdraw
                    }
                    Active::Remote { .. } => {
                        self.active = Active::None;
                        Action::None
                    }
                }
            }
        }
    }

    /// The remote promises were put on the clipboard.
    pub fn set_remote(&mut self, id: u64, names: Vec<String>) {
        self.active = Active::Remote { id, names };
    }

    /// The other machine withdrew offer `id`: stop restoring it.
    pub fn forget_remote(&mut self, id: u64) {
        if matches!(self.active, Active::Remote { id: a, .. } if a == id) {
            self.active = Active::None;
        }
    }

    pub fn remote_id(&self) -> Option<u64> {
        match self.active {
            Active::Remote { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// The platform clipboard bridge. Dropping it stops watching.
pub struct Clipboard {
    inner: sys::Clipboard,
}

impl Clipboard {
    /// Starts watching the clipboard. `run_on_main` is used on macOS only.
    pub fn start(callbacks: Callbacks, run_on_main: Option<RunOnMain>) -> io::Result<Self> {
        Ok(Self {
            inner: sys::Clipboard::start(callbacks, run_on_main)?,
        })
    }

    /// Puts the other machine's files on this clipboard (as promises).
    pub fn offer_remote(&self, files: RemoteFiles) {
        self.inner.offer_remote(files);
    }

    /// The other machine withdrew offer `id`.
    pub fn forget_remote(&self, id: u64) {
        self.inner.forget_remote(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn other(text: Option<&str>, from_deskflow: Option<bool>) -> Change {
        Change::Other {
            text: text.map(Into::into),
            only_text: true,
            from_deskflow,
        }
    }

    #[test]
    fn echo_detection() {
        let names = vec!["report.pdf".to_string(), "Photos".to_string()];
        assert!(is_echo(None, &names));
        assert!(is_echo(Some("  "), &names));
        assert!(is_echo(Some("report.pdf"), &names));
        assert!(is_echo(
            Some("C:\\Users\\x\\report.pdf\r\n/Users/y/Photos/"),
            &names
        ));
        assert!(!is_echo(Some("hello"), &names));
        assert!(!is_echo(Some("report.pdf\nhello"), &names));
    }

    #[test]
    fn local_copy_then_deskflow_echo_restores() {
        let mut s = State::default();
        let paths = vec![PathBuf::from("/x/report.pdf")];
        assert_eq!(
            s.on_change(Change::Files(paths.clone())),
            Action::Offer(paths.clone())
        );
        // our own restore shows up as the same files: nothing to do
        assert_eq!(s.on_change(Change::Files(paths.clone())), Action::None);
        assert_eq!(
            s.on_change(other(Some("report.pdf"), None)),
            Action::RestoreLocal(paths.clone())
        );
        assert_eq!(
            s.on_change(other(None, Some(true))),
            Action::RestoreLocal(paths)
        );
        // real text copied by the user: withdraw
        assert_eq!(
            s.on_change(other(Some("some text"), None)),
            Action::Withdraw
        );
        assert_eq!(s.on_change(other(None, None)), Action::None);
    }

    #[test]
    fn non_deskflow_or_rich_content_is_not_an_echo() {
        let mut s = State::default();
        s.set_remote(5, vec!["a.txt".into()]);
        assert_eq!(s.on_change(other(None, None)), Action::RestoreRemote);
        let image = Change::Other {
            text: None,
            only_text: false,
            from_deskflow: None,
        };
        assert_eq!(s.on_change(image), Action::None);
        assert_eq!(s.remote_id(), None);

        s.set_remote(6, vec!["a.txt".into()]);
        assert_eq!(s.on_change(other(Some("a.txt"), Some(false))), Action::None);
        assert_eq!(s.remote_id(), None);
    }

    #[test]
    fn withdraw_forgets_only_matching_offer() {
        let mut s = State::default();
        s.set_remote(1, vec![]);
        s.forget_remote(2);
        assert_eq!(s.remote_id(), Some(1));
        s.forget_remote(1);
        assert_eq!(s.remote_id(), None);
    }
}
