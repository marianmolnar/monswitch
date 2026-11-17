//! Files this machine offers to the other one.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proto::Entry;

/// A set of copied files and folders, expanded into a flat list of entries
/// (folders first, then their contents, recursively).
#[derive(Debug, Clone)]
pub struct LocalOffer {
    pub id: u64,
    /// What was copied (top-level paths), in clipboard order.
    pub roots: Vec<PathBuf>,
    pub entries: Vec<Entry>,
    /// Source path of each entry.
    sources: Vec<PathBuf>,
}

/// A fresh offer id. Unique enough: time + process id.
pub fn new_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 48)
}

fn mtime_of(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// `name`, or `name (2)`, `name (3)`, … if already taken (case-insensitive).
fn unique_name(name: &str, taken: &mut HashSet<String>) -> String {
    let mut candidate = name.to_string();
    let mut n = 2;
    while !taken.insert(candidate.to_lowercase()) {
        candidate = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
            _ => format!("{name} ({n})"),
        };
        n += 1;
    }
    candidate
}

impl LocalOffer {
    /// Expands `paths` (files and folders). Symlinked folders are not
    /// followed (no loops); symlinked files are sent as files. Unreadable
    /// items inside folders are skipped with a warning.
    pub fn from_paths(paths: &[PathBuf]) -> io::Result<Self> {
        let mut offer = LocalOffer {
            id: new_id(),
            roots: paths.to_vec(),
            entries: Vec::new(),
            sources: Vec::new(),
        };
        let mut taken = HashSet::new();
        for p in paths {
            let name = p.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("no file name: {}", p.display()),
                )
            })?;
            let name = unique_name(name, &mut taken);
            let meta = fs::metadata(p)?;
            offer.add(p, name, &meta)?;
        }
        if offer.entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nothing to offer",
            ));
        }
        Ok(offer)
    }

    fn add(&mut self, src: &Path, rel: String, meta: &fs::Metadata) -> io::Result<()> {
        if meta.is_dir() {
            self.entries.push(Entry {
                path: rel.clone(),
                size: 0,
                dir: true,
                mtime: mtime_of(meta),
            });
            self.sources.push(src.to_path_buf());
            let mut children: Vec<_> = fs::read_dir(src)?.filter_map(Result::ok).collect();
            children.sort_by_key(|c| c.file_name());
            for child in children {
                let path = child.path();
                let Some(name) = child.file_name().to_str().map(str::to_string) else {
                    log::warn!("xfer: skipping non-UTF-8 name {}", path.display());
                    continue;
                };
                let link = child.file_type().is_ok_and(|t| t.is_symlink());
                match fs::metadata(&path) {
                    Ok(m) if link && m.is_dir() => {
                        log::warn!("xfer: skipping symlinked folder {}", path.display())
                    }
                    Ok(m) => {
                        if let Err(e) = self.add(&path, format!("{rel}/{name}"), &m) {
                            log::warn!("xfer: skipping {}: {e}", path.display());
                        }
                    }
                    Err(e) => log::warn!("xfer: skipping {}: {e}", path.display()),
                }
            }
        } else {
            self.entries.push(Entry {
                path: rel,
                size: meta.len(),
                dir: false,
                mtime: mtime_of(meta),
            });
            self.sources.push(src.to_path_buf());
        }
        Ok(())
    }

    pub fn total_size(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Opens file `entry` at `offset`; returns the file and the number of
    /// bytes left. Fails when the file changed since it was copied – the
    /// other machine already announced its size.
    pub fn open(&self, entry: u32, offset: u64) -> io::Result<(File, u64)> {
        let i = entry as usize;
        let (e, src) = self
            .entries
            .get(i)
            .zip(self.sources.get(i))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such entry"))?;
        if e.dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry is a folder",
            ));
        }
        let mut f = File::open(src)?;
        let meta = f.metadata()?;
        if meta.len() != e.size || mtime_of(&meta) != e.mtime {
            return Err(io::Error::other(format!(
                "{} changed since it was copied",
                e.path
            )));
        }
        if offset > e.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset past end of file",
            ));
        }
        f.seek(SeekFrom::Start(offset))?;
        Ok((f, e.size - offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::validate_entries;
    use std::io::Read;

    #[test]
    fn expands_folders_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("photos");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("b.txt"), b"bb").unwrap();
        fs::write(d.join("sub").join("a.txt"), b"a").unwrap();
        let f = tmp.path().join("note.txt");
        fs::write(&f, b"hello").unwrap();

        let offer = LocalOffer::from_paths(&[d, f]).unwrap();
        let paths: Vec<_> = offer.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "photos",
                "photos/b.txt",
                "photos/sub",
                "photos/sub/a.txt",
                "note.txt"
            ]
        );
        assert_eq!(offer.total_size(), 8);
        validate_entries(&offer.entries).unwrap();

        let (mut file, left) = offer.open(4, 1).unwrap();
        assert_eq!(left, 4);
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        assert_eq!(s, "ello");
        assert!(offer.open(0, 0).is_err(), "folder");
        assert!(offer.open(99, 0).is_err());
    }

    #[test]
    fn duplicate_top_level_names_get_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["x", "y"] {
            fs::create_dir(tmp.path().join(d)).unwrap();
            fs::write(tmp.path().join(d).join("r.pdf"), b"1").unwrap();
        }
        let offer =
            LocalOffer::from_paths(&[tmp.path().join("x/r.pdf"), tmp.path().join("y/r.pdf")])
                .unwrap();
        assert_eq!(offer.entries[1].path, "r (2).pdf");
    }

    #[test]
    fn changed_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.bin");
        fs::write(&f, b"1234").unwrap();
        let offer = LocalOffer::from_paths(std::slice::from_ref(&f)).unwrap();
        fs::write(&f, b"123456").unwrap();
        assert!(offer.open(0, 0).is_err());
    }
}
