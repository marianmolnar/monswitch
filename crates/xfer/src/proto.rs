//! Wire format.
//!
//! Every connection carries exactly one request: the client writes
//! [`MAGIC`] and a [`Request`], the server answers with a [`Response`].
//! For [`Request::Get`] the answer is [`Response::Data`] followed by `len`
//! raw bytes of the file, starting at the requested offset – so a broken
//! transfer resumes with a new `Get` from where it stopped.
//!
//! Messages are a big-endian `u32` length followed by JSON.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Protocol tag + version, sent first on every connection.
pub const MAGIC: &[u8; 4] = b"MSX1";

/// Upper bound for one JSON message (an offer listing a very large folder
/// tree is the biggest one).
const MAX_MSG: u32 = 64 << 20;

/// One file or folder of an offer. Folders come before their contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative path with `/` separators; the first component is the name
    /// of the copied file or folder.
    pub path: String,
    pub size: u64,
    pub dir: bool,
    /// Last modification, seconds since the Unix epoch (0 = unknown).
    pub mtime: u64,
}

impl Entry {
    /// Last path component.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// True for the copied files/folders themselves (not their contents).
    pub fn is_top_level(&self) -> bool {
        !self.path.contains('/')
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Request {
    /// "I am here" – lets the server learn the client's address.
    Hello { name: String },
    /// Files were copied on the sending machine.
    Offer {
        id: u64,
        from: String,
        entries: Vec<Entry>,
    },
    /// The sending machine's clipboard no longer holds the offered files.
    Withdraw { id: u64 },
    /// Send file `entry` of offer `id`, starting at `offset`.
    Get { id: u64, entry: u32, offset: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Response {
    Ok,
    /// `len` raw bytes follow.
    Data {
        len: u64,
    },
    Error {
        msg: String,
    },
}

pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|&l| l <= MAX_MSG)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)
}

pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len);
    if len > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Components of an entry path received from the other machine, or `None`
/// when it could escape the destination folder (absolute, `..`, drive
/// letters, characters Windows or macOS reject).
pub fn safe_components(path: &str) -> Option<Vec<&str>> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    let ok = parts.iter().all(|p| {
        !p.is_empty()
            && *p != "."
            && *p != ".."
            && !p
                .chars()
                .any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    });
    ok.then_some(parts)
}

/// Checks a whole offer: safe paths, every parent folder listed before its
/// contents, no duplicates.
pub fn validate_entries(entries: &[Entry]) -> Result<(), String> {
    let mut dirs = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        let parts = safe_components(&e.path).ok_or_else(|| format!("unsafe path {:?}", e.path))?;
        if !seen.insert(e.path.to_lowercase()) {
            return Err(format!("duplicate path {:?}", e.path));
        }
        if parts.len() > 1 {
            let parent = &e.path[..e.path.len() - parts[parts.len() - 1].len() - 1];
            if !dirs.contains(parent) {
                return Err(format!("{:?} listed before its folder", e.path));
            }
        }
        if e.dir {
            dirs.insert(e.path.as_str());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, dir: bool) -> Entry {
        Entry {
            path: path.into(),
            size: 0,
            dir,
            mtime: 0,
        }
    }

    #[test]
    fn roundtrip() {
        let req = Request::Get {
            id: 7,
            entry: 3,
            offset: 1 << 40,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let back: Request = read_msg(&mut buf.as_slice()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn rejects_oversized_length() {
        let mut buf = (MAX_MSG + 1).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        assert!(read_msg::<Response>(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn unsafe_paths() {
        for p in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            "C:/x",
            "a\\b",
            "a//b",
            "./a",
            "a/\u{0}",
        ] {
            assert!(safe_components(p).is_none(), "{p:?}");
        }
        assert_eq!(
            safe_components("Photos/2026/a b.jpg"),
            Some(vec!["Photos", "2026", "a b.jpg"])
        );
    }

    #[test]
    fn entries_need_parent_first() {
        assert!(validate_entries(&[entry("d", true), entry("d/f", false)]).is_ok());
        assert!(validate_entries(&[entry("d/f", false), entry("d", true)]).is_err());
        assert!(validate_entries(&[entry("f", false), entry("f/g", false)]).is_err());
        assert!(validate_entries(&[entry("a", false), entry("A", false)]).is_err());
    }

    #[test]
    fn names() {
        assert_eq!(entry("d/e/f.txt", false).name(), "f.txt");
        assert!(entry("f.txt", false).is_top_level());
        assert!(!entry("d/f.txt", false).is_top_level());
    }
}
