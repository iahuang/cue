//! Keeps unsaved changes through crashes, hangups, and kills.
//!
//! While a file has unsaved changes, its text is copied to a recovery file,
//! at most once a second as it changes, and once more when cue exits other
//! than by quitting: on a panic, an error, or SIGHUP or SIGTERM, as when an
//! ssh connection drops. Saving the file, reverting it, or closing it
//! without saving removes its copy, and so does quitting.
//!
//! Each running cue holds a lock on a file named for its process, so the
//! copies of one that's gone are told from those of one still running. When
//! cue starts, it offers back the copies a cue that's gone left of files in
//! its workspace, and untitled files from a workspace with a folder in
//! common. Others stay until cue opens where they belong.
//!
//! The copies are in `$XDG_STATE_HOME/cue/recovery`, or
//! `~/.local/state/cue/recovery`.

use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use crate::document::{self, Document};

/// How often a file's copy is written at most, while it changes.
const INTERVAL: Duration = Duration::from_secs(1);
const HEADER: &str = "cue recovery 1";

/// A document's copy.
struct Tracked {
    doc: Weak<Document>,
    /// Names its copy, with the process's id.
    id: u32,
    /// The buffer's content epoch when the copy was written.
    epoch: Option<u64>,
    written: Option<Instant>,
}

pub struct Recovery {
    /// Where copies go; `None` keeps none.
    dir: Option<PathBuf>,
    /// This process's lock, held while it runs, once it wrote a copy.
    lock: Option<fs::File>,
    tracked: Vec<Tracked>,
    next_id: u32,
}

/// A copy a cue that's gone left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    /// The copy.
    pub file: PathBuf,
    /// The file it's of, or `None` for an untitled one.
    pub path: Option<PathBuf>,
    pub text: String,
}

impl Recovery {
    /// Keeps copies in `dir`, or none without one.
    pub fn new(dir: Option<PathBuf>) -> Recovery {
        Recovery {
            dir,
            lock: None,
            tracked: Vec::new(),
            next_id: 1,
        }
    }

    /// Where copies go by default.
    pub fn default_dir() -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
        Some(state.join("cue/recovery"))
    }

    /// Copies the text of `documents` with unsaved changes that changed
    /// since, unless copied less than a second ago, or with `now`, however
    /// recently. Removes the copies of documents saved or closed since.
    /// `roots` are the workspace's folders, for untitled files to be
    /// offered back in. Failures are ignored: the next call tries again.
    pub fn sync(&mut self, documents: &[Rc<Document>], roots: &[PathBuf], now: bool) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let pid = std::process::id();
        self.tracked.retain(|tracked| {
            let open = tracked
                .doc
                .upgrade()
                .is_some_and(|doc| documents.iter().any(|open| Rc::ptr_eq(open, &doc)));
            if !open && tracked.epoch.is_some() {
                let _ = fs::remove_file(copy_path(&dir, pid, tracked.id));
            }
            open
        });
        for doc in documents {
            let index = match self
                .tracked
                .iter()
                .position(|tracked| tracked.doc.as_ptr() == Rc::as_ptr(doc))
            {
                Some(index) => index,
                None => {
                    self.tracked.push(Tracked {
                        doc: Rc::downgrade(doc),
                        id: self.next_id,
                        epoch: None,
                        written: None,
                    });
                    self.next_id += 1;
                    self.tracked.len() - 1
                }
            };
            let tracked = &mut self.tracked[index];
            let copy = copy_path(&dir, pid, tracked.id);
            let Some(text) = doc.unsaved_text() else {
                if tracked.epoch.take().is_some() {
                    let _ = fs::remove_file(&copy);
                }
                continue;
            };
            let epoch = doc.buffer.content_epoch();
            let recent = tracked.written.is_some_and(|at| at.elapsed() < INTERVAL);
            if tracked.epoch == Some(epoch) || (recent && !now) {
                continue;
            }
            if self.lock.is_none() {
                self.lock = lock(&dir, pid).ok();
                if self.lock.is_none() {
                    return;
                }
            }
            let path = doc.try_path();
            if write_copy(&copy, path.as_deref(), roots, &text).is_ok() {
                tracked.epoch = Some(epoch);
                tracked.written = Some(Instant::now());
            }
        }
    }

    /// Removes every copy this process wrote, and writes no more, as
    /// quitting does: whatever wasn't saved was let go.
    pub fn discard(&mut self) {
        let Some(dir) = self.dir.take() else {
            return;
        };
        let dir = &dir;
        let pid = std::process::id();
        for tracked in self.tracked.drain(..) {
            if tracked.epoch.is_some() {
                let _ = fs::remove_file(copy_path(dir, pid, tracked.id));
            }
        }
        if self.lock.take().is_some() {
            let _ = fs::remove_file(lock_path(dir, pid));
        }
    }

    /// The copies cues that are gone left, of files in `roots` or of
    /// untitled files from a workspace with a folder in common, oldest
    /// first. Copies of files that are now as the copy has them are removed.
    pub fn orphans(&self, roots: &[PathBuf]) -> Vec<Orphan> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut alive = std::collections::HashMap::new();
        for entry in entries.flatten() {
            let file = entry.path();
            let Some(pid) = copy_pid(&file) else {
                continue;
            };
            if pid == std::process::id() {
                continue;
            }
            let running = *alive.entry(pid).or_insert_with(|| is_running(dir, pid));
            if running {
                continue;
            }
            let Some((path, from, text)) = read_copy(&file) else {
                continue;
            };
            let here = match &path {
                Some(path) => roots.iter().any(|root| path.starts_with(root)),
                None => from.iter().any(|root| roots.contains(root)),
            };
            if !here {
                continue;
            }
            let unchanged = path
                .as_deref()
                .and_then(|path| document::load(path).ok())
                .is_some_and(|loaded| loaded.text == text);
            if unchanged {
                remove(&[Orphan { file, path, text }]);
                continue;
            }
            let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
            found.push((modified, Orphan { file, path, text }));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.file.cmp(&b.1.file)));
        found.into_iter().map(|(_, orphan)| orphan).collect()
    }
}

/// Removes the copies `orphans` came from, and the locks of the cues that
/// left them once none of theirs are left.
pub fn remove(orphans: &[Orphan]) {
    for orphan in orphans {
        let _ = fs::remove_file(&orphan.file);
        let (Some(dir), Some(pid)) = (orphan.file.parent(), copy_pid(&orphan.file)) else {
            continue;
        };
        let others = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| copy_pid(&entry.path()) == Some(pid));
        if !others {
            let _ = fs::remove_file(lock_path(dir, pid));
        }
    }
}

fn copy_path(dir: &Path, pid: u32, id: u32) -> PathBuf {
    dir.join(format!("{pid}-{id}.txt"))
}

fn lock_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.lock"))
}

/// The process a copy is from, if `file` is one.
fn copy_pid(file: &Path) -> Option<u32> {
    let name = file.file_name()?.to_str()?.strip_suffix(".txt")?;
    let (pid, id) = name.split_once('-')?;
    id.parse::<u32>().ok()?;
    pid.parse().ok()
}

/// Takes this process's lock, which it holds until it exits.
fn lock(dir: &Path, pid: u32) -> io::Result<fs::File> {
    fs::create_dir_all(dir)?;
    let file = fs::File::create(lock_path(dir, pid))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Whether the cue with process id `pid` still runs: whether it holds its
/// lock. Locks are let go when a process exits however it does, so an id
/// used again by another program doesn't count.
fn is_running(dir: &Path, pid: u32) -> bool {
    let Ok(file) = fs::File::open(lock_path(dir, pid)) else {
        return false;
    };
    // Let go when the file closes.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

/// Writes a copy of `text`, of the file at `path` or an untitled one, in
/// a workspace with `roots`: whole or not at all.
fn write_copy(copy: &Path, path: Option<&Path>, roots: &[PathBuf], text: &str) -> io::Result<()> {
    let line = |path: &Path| {
        let path = path.to_str().filter(|path| !path.contains('\n'));
        path.map(str::to_string)
            .ok_or_else(|| io::Error::other("unusual path"))
    };
    let mut header = format!("{HEADER}\n");
    match path {
        Some(path) => header += &format!("path {}\n", line(path)?),
        None => header += "untitled\n",
    }
    for root in roots {
        if let Ok(root) = line(root) {
            header += &format!("root {root}\n");
        }
    }
    let tmp = copy.with_extension("tmp");
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(header.as_bytes())?;
        file.write_all(b"\n")?;
        file.write_all(text.as_bytes())?;
        fs::rename(&tmp, copy)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A copy's file (`None` if untitled), the workspace's folders then, and
/// the text.
fn read_copy(copy: &Path) -> Option<(Option<PathBuf>, Vec<PathBuf>, String)> {
    let contents = fs::read_to_string(copy).ok()?;
    let (header, text) = contents.split_once("\n\n")?;
    let mut lines = header.lines();
    if lines.next()? != HEADER {
        return None;
    }
    let mut path = None;
    let mut roots = Vec::new();
    for line in lines {
        match line.split_once(' ') {
            Some(("path", value)) => path = Some(PathBuf::from(value)),
            Some(("root", value)) => roots.push(PathBuf::from(value)),
            _ => {}
        }
    }
    Some((path, roots, text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_are_read_back_whole() {
        let dir = std::env::temp_dir().join(format!("cue-recovery-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let copy = copy_path(&dir, 7, 3);
        assert_eq!(copy_pid(&copy), Some(7));
        assert_eq!(copy_pid(&dir.join("7.lock")), None);
        let text = "first\n\nsecond\n";
        write_copy(
            &copy,
            Some(Path::new("/w/a.rs")),
            &[PathBuf::from("/w")],
            text,
        )
        .unwrap();
        assert_eq!(
            read_copy(&copy),
            Some((
                Some(PathBuf::from("/w/a.rs")),
                vec![PathBuf::from("/w")],
                text.to_string()
            ))
        );
        write_copy(&copy, None, &[], "").unwrap();
        assert_eq!(read_copy(&copy), Some((None, Vec::new(), String::new())));

        // Held by a process, a lock says it runs; let go, it doesn't.
        assert!(!is_running(&dir, 7));
        let held = lock(&dir, 7).unwrap();
        assert!(is_running(&dir, 7));
        drop(held);
        assert!(crate::let_go(|| is_running(&dir, 7)));
    }
}
