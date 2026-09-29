//! Watching folders for files that other programs create, delete, or
//! change.
//!
//! The app watches the folders open in the file tree and those of open
//! files, each without the folders inside it: watching a whole workspace
//! can run out of inotify watches on Linux, and hears every file a build
//! writes. A save is several changes in a row, so changes are handed over
//! in batches, once they settle.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// How long changes have to stop for before they're handed over.
const SETTLE: Duration = Duration::from_millis(50);
/// The longest changes wait while they keep coming.
const MAX_WAIT: Duration = Duration::from_millis(300);

/// What changed in the watched folders.
#[derive(Debug, Default)]
pub struct Changes {
    /// The files and folders that came, went, or changed. Which of those
    /// isn't told: on macOS, a file created lately is told as created again
    /// each time it changes.
    pub paths: HashSet<PathBuf>,
    /// Changes may have been missed: anything may have changed.
    pub rescan: bool,
}

pub struct Watcher {
    /// `None` if watching isn't possible here.
    watcher: Option<RecommendedWatcher>,
    events: Receiver<notify::Result<Event>>,
    watched: BTreeSet<PathBuf>,
    /// Changes not handed over yet, and when the first and last came.
    pending: Option<(Changes, Instant, Instant)>,
}

impl Watcher {
    pub fn new() -> Watcher {
        let (sender, events) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        });
        Watcher {
            watcher: watcher.ok(),
            events,
            watched: BTreeSet::new(),
            pending: None,
        }
    }

    /// Watches `folders`, which exist, and stops watching others.
    pub fn watch(&mut self, folders: BTreeSet<PathBuf>) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        if folders == self.watched {
            return;
        }
        // All at once: on macOS, each change restarts the event stream.
        let mut paths = watcher.paths_mut();
        for gone in self.watched.difference(&folders) {
            let _ = paths.remove(gone);
        }
        let mut watched = folders.clone();
        for new in folders.difference(&self.watched) {
            if paths.add(new, RecursiveMode::NonRecursive).is_err() {
                watched.remove(new);
            }
        }
        if paths.commit().is_err() {
            // Tried again next time.
            watched.clear();
        }
        self.watched = watched;
    }

    /// The changes since the last batch, once they've settled.
    pub fn poll(&mut self) -> Option<Changes> {
        let now = Instant::now();
        while let Ok(event) = self.events.try_recv() {
            let event = match event {
                // Reading a file or listing a folder, as the app does.
                Ok(event) if matches!(event.kind, EventKind::Access(_)) => continue,
                Ok(event) => Some(event),
                Err(_) => None,
            };
            let (changes, _, last) = self
                .pending
                .get_or_insert_with(|| (Changes::default(), now, now));
            *last = now;
            let Some(event) = event else {
                changes.rescan = true;
                continue;
            };
            changes.rescan |= event.need_rescan();
            for path in event.paths {
                // A watched folder that's gone isn't watched any more, even
                // if it comes back.
                if matches!(event.kind, EventKind::Remove(_)) {
                    self.watched.remove(&path);
                }
                changes.paths.insert(path);
            }
        }
        let (_, first, last) = self.pending.as_ref()?;
        if now - *last >= SETTLE || now - *first >= MAX_WAIT {
            return self.pending.take().map(|(changes, ..)| changes);
        }
        None
    }

    /// Waits up to `timeout` for a batch of changes, for tests.
    #[cfg(test)]
    pub fn wait(&mut self, timeout: Duration) -> Option<Changes> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(changes) = self.poll() {
                return Some(changes);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-watch-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("inner")).unwrap();
        dir.canonicalize().unwrap()
    }

    /// Waits for a batch with `path` in it, skipping others (such as the
    /// folder being created, which macOS may still report).
    fn saw(watcher: &mut Watcher, path: &PathBuf) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if let Some(changes) = watcher.wait(Duration::from_millis(500)) {
                if changes.paths.contains(path) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn hears_files_in_watched_folders_but_not_in_folders_inside() {
        let dir = fixture("files");
        let mut watcher = Watcher::new();
        watcher.watch(BTreeSet::from([dir.clone()]));
        // Let the watch start before changing anything.
        std::thread::sleep(Duration::from_millis(100));
        watcher.poll();

        let inner = dir.join("inner").join("x.txt");
        fs::write(&inner, "x").unwrap();
        let file = dir.join("a.txt");
        fs::write(&file, "a").unwrap();
        let start = Instant::now();
        let mut paths = HashSet::new();
        while start.elapsed() < Duration::from_secs(5) && !paths.contains(&file) {
            if let Some(changes) = watcher.wait(Duration::from_millis(500)) {
                paths.extend(changes.paths);
            }
        }
        assert!(paths.contains(&file), "{paths:?}");
        assert!(!paths.contains(&inner), "{paths:?}");

        fs::write(&file, "changed").unwrap();
        assert!(saw(&mut watcher, &file));
        fs::remove_file(&file).unwrap();
        assert!(saw(&mut watcher, &file));

        // Unwatched, it hears nothing.
        watcher.watch(BTreeSet::new());
        watcher.wait(Duration::from_millis(200));
        fs::write(&file, "again").unwrap();
        assert!(watcher.wait(Duration::from_millis(300)).is_none());
    }
}
