//! VCP snapshots for reverse-engineering vendor codes (PbP/PiP):
//! take a snapshot, change something in the OSD, take another, diff.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::DisplayControl;

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub created_unix: u64,
    pub monitor: String,
    pub backend: String,
    /// Key: `"0xE0"`.
    pub codes: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Entry {
    fn same_as(&self, other: &Entry) -> bool {
        self.current == other.current
            && self.maximum == other.maximum
            && self.error.is_some() == other.error.is_some()
    }
}

pub fn code_key(code: u8) -> String {
    format!("0x{code:02X}")
}

/// Reads every code in `codes`. `progress` is called after each read.
pub fn snapshot(
    mon: &dyn DisplayControl,
    codes: &[u8],
    mut progress: impl FnMut(u8, &Entry),
) -> Snapshot {
    let mut out = BTreeMap::new();
    for &code in codes {
        let entry = match mon.get_vcp(code) {
            Ok(v) => Entry {
                current: Some(v.current),
                maximum: Some(v.maximum),
                error: None,
            },
            Err(e) => Entry {
                current: None,
                maximum: None,
                error: Some(e.to_string()),
            },
        };
        progress(code, &entry);
        out.insert(code_key(code), entry);
    }
    let info = mon.info();
    Snapshot {
        version: SNAPSHOT_VERSION,
        created_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        monitor: info.model().to_string(),
        backend: info.backend.clone(),
        codes: out,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub code: String,
    pub before: Option<Entry>,
    pub after: Option<Entry>,
}

pub fn diff(a: &Snapshot, b: &Snapshot) -> Vec<Change> {
    let mut keys: Vec<&String> = a.codes.keys().chain(b.codes.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|k| {
            let (x, y) = (a.codes.get(k), b.codes.get(k));
            let same = matches!((x, y), (Some(x), Some(y)) if x.same_as(y));
            (!same).then(|| Change {
                code: k.clone(),
                before: x.cloned(),
                after: y.cloned(),
            })
        })
        .collect()
}

/// Loads a snapshot. Handles UTF-16 (PowerShell 5 `>` redirect) and UTF-8 BOM.
pub fn load(path: &Path) -> Result<Snapshot> {
    let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
    let text = decode_text(&bytes);
    serde_json::from_str(&text).map_err(|e| Error::Snapshot {
        path: path.to_path_buf(),
        msg: e.to_string(),
    })
}

fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(entries: &[(u8, Option<u16>, bool)]) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            created_unix: 0,
            monitor: "X".into(),
            backend: "test".into(),
            codes: entries
                .iter()
                .map(|(c, v, err)| {
                    (
                        code_key(*c),
                        Entry {
                            current: *v,
                            maximum: v.map(|_| 3),
                            error: err.then(|| "nope".to_string()),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn diff_reports_changes_only() {
        let a = snap(&[(0xE0, Some(0), false), (0xE1, Some(1), false), (0xE2, None, true)]);
        let b = snap(&[(0xE0, Some(0), false), (0xE1, Some(2), false), (0xE2, None, true), (0xE3, Some(1), false)]);
        let d = diff(&a, &b);
        let codes: Vec<&str> = d.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(codes, vec!["0xE1", "0xE3"]);
    }

    #[test]
    fn decodes_utf16le_bom() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "{\"a\":1}".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_text(&bytes), "{\"a\":1}");
    }
}
