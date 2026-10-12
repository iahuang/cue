//! Every file in the workspace, for the file picker, skipping `.git` and
//! whatever `.gitignore` and `.ignore` files exclude.
//!
//! The host does the listing (see [`crate::host`]), so a large folder
//! never blocks the screen. The first listing starts with the app and shows
//! up as it goes. After that the list is kept for the whole session: a
//! refresh lists the files again in the background and swaps the new list
//! in only when it's complete, so the picker always has a whole list to
//! show. Nothing watches the file system yet, so the app refreshes when the
//! picker opens and after saving.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ignore::{DirEntry, WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};

use crate::host::{Call, Link, Reply, Request, Responder};
use crate::picker::Item;
use crate::tree;
use crate::workspace::Workspace;

/// Files beyond this many aren't listed, to bound memory in huge folders.
const MAX_FILES: usize = 200_000;
/// How often the first listing reports the files found so far.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

pub struct FileIndex {
    host: Link,
    workspace: Workspace,
    /// Shared with an open picker, which keeps its copy until given a new one.
    files: Rc<Vec<Item>>,
    /// Whether `files` is a whole listing, not the first one in progress.
    complete: bool,
    /// Whether some files went unlisted, past `MAX_FILES`.
    truncated: bool,
    /// Which of the host's lists `files` is, once it's a whole one.
    version: Option<u64>,
    /// The listing in progress, if any.
    listing: Option<Call>,
    /// A refresh was asked for during a listing: list again after it.
    again: bool,
}

impl FileIndex {
    /// Starts listing the files in `workspace`, on `host`.
    pub fn new(host: &Link, workspace: &Workspace) -> FileIndex {
        let mut index = FileIndex {
            host: host.clone(),
            workspace: workspace.clone(),
            files: Rc::new(Vec::new()),
            complete: false,
            truncated: false,
            version: None,
            listing: None,
            again: false,
        };
        index.refresh();
        index
    }

    /// The files, sorted by path ignoring case once the first listing is
    /// complete.
    pub fn files(&self) -> Rc<Vec<Item>> {
        Rc::clone(&self.files)
    }

    /// Whether the first listing is still in progress.
    pub fn listing(&self) -> bool {
        !self.complete
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Lists the files again in the background, keeping the current list
    /// until the new one is complete.
    pub fn refresh(&mut self) {
        if self.listing.is_some() {
            // This listing may already have passed the change.
            self.again = true;
        } else {
            self.listing = Some(self.host.call(Request::ListFiles {
                workspace: self.workspace.clone(),
                progress: !self.complete,
                since: self.version,
            }));
        }
    }

    /// Takes in what the listing found since the last call. Returns whether
    /// the files changed.
    pub fn poll(&mut self) -> bool {
        let Some(listing) = self.listing.take() else {
            return false;
        };
        let mut changed = false;
        loop {
            match listing.try_recv() {
                Ok(Reply::FilesFound(found)) => {
                    let workspace = &self.workspace;
                    let found = found
                        .unpack()
                        .into_iter()
                        .map(|path| Item::file(path, workspace, ""));
                    Rc::make_mut(&mut self.files).extend(found);
                    changed = true;
                }
                Ok(Reply::Files {
                    files,
                    truncated,
                    version,
                }) => {
                    let workspace = &self.workspace;
                    let files = files
                        .unpack()
                        .into_iter()
                        .map(|path| Item::file(path, workspace, ""));
                    self.files = Rc::new(files.collect());
                    self.truncated = truncated;
                    self.version = Some(version);
                    self.complete = true;
                    changed = true;
                    break;
                }
                Ok(Reply::FilesChanged {
                    added,
                    removed,
                    truncated,
                    version,
                }) => {
                    changed = self.change(added.unpack(), removed.unpack());
                    self.truncated = truncated;
                    self.version = Some(version);
                    break;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    self.listing = Some(listing);
                    return changed;
                }
                // The listing failed; keep what it found.
                Err(TryRecvError::Disconnected) => {
                    self.complete = true;
                    break;
                }
            }
        }
        if std::mem::take(&mut self.again) {
            self.refresh();
        }
        changed
    }

    /// Adds `added` to the list, and takes `removed` from it, keeping it
    /// sorted. Returns whether it changed.
    fn change(&mut self, added: Vec<PathBuf>, removed: Vec<PathBuf>) -> bool {
        if added.is_empty() && removed.is_empty() {
            return false;
        }
        let files = Rc::make_mut(&mut self.files);
        if !removed.is_empty() {
            let removed: HashSet<PathBuf> = removed.into_iter().collect();
            files.retain(|item| !item.path().is_some_and(|path| removed.contains(path)));
        }
        if !added.is_empty() {
            let workspace = &self.workspace;
            files.extend(
                added
                    .into_iter()
                    .map(|path| Item::file(path, workspace, "")),
            );
            files.sort_by_cached_key(|item| sort_key(&item.text));
        }
        true
    }

    /// Waits for the listing in progress to finish, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) {
        while self.listing.is_some() {
            self.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Paths, as sent: each as the bytes it doesn't share with the one
/// before, after how many it does. Sorted, they share most: their roots,
/// and their folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths(Vec<(u32, Vec<u8>)>);

impl Paths {
    pub fn pack<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Paths {
        let mut packed = Vec::new();
        let mut before: &[u8] = &[];
        for path in paths {
            let bytes = path.as_os_str().as_bytes();
            let shared = bytes.iter().zip(before).take_while(|(a, b)| a == b).count();
            packed.push((shared as u32, bytes[shared..].to_vec()));
            before = bytes;
        }
        Paths(packed)
    }

    pub fn unpack(self) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(self.0.len());
        let mut before: Vec<u8> = Vec::new();
        for (shared, rest) in self.0 {
            before.truncate(shared as usize);
            before.extend_from_slice(&rest);
            paths.push(PathBuf::from(OsStr::from_bytes(&before)));
        }
        paths
    }
}

/// The last whole list the host sent on a connection, for the next to be
/// sent as what changed since.
#[derive(Default)]
pub struct Listed {
    version: u64,
    files: Vec<PathBuf>,
}

/// Lists the files in `workspace`, for the host, replying with the files
/// found so far along the way if `progress`, and then all of them; or, if
/// the app has the list `listed` was as of `since`, what changed since. A
/// root inside another is listed once, as itself. The listing stops early
/// if the call is cancelled.
pub fn list(
    workspace: &Workspace,
    progress: bool,
    since: Option<u64>,
    listed: &Mutex<Listed>,
    responder: &Responder,
) {
    let (found, paths) = mpsc::channel();
    let walker = walker(workspace);
    std::thread::spawn(move || walk(walker, found));

    let mut files = Vec::new();
    let mut sent = 0;
    let mut last_sent = Instant::now();
    let mut truncated = false;
    loop {
        if responder.is_cancelled() {
            // Dropping the receiver stops the walk.
            return;
        }
        match paths.recv_timeout(PROGRESS_INTERVAL) {
            Ok(path) => {
                if files.len() >= MAX_FILES {
                    truncated = true;
                    break;
                }
                files.push(path);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if progress && files.len() > sent && last_sent.elapsed() >= PROGRESS_INTERVAL {
            if !responder.reply(Reply::FilesFound(Paths::pack(&files[sent..]))) {
                return;
            }
            sent = files.len();
            last_sent = Instant::now();
        }
    }
    files.sort_by_cached_key(|path| sort_key(&workspace.display_path(path)));
    let mut listed = listed.lock().unwrap_or_else(|e| e.into_inner());
    let version = listed.version + 1;
    let reply = match since == Some(listed.version) {
        true => {
            let (added, removed) = changes(&listed.files, &files);
            Reply::FilesChanged {
                added: Paths::pack(added),
                removed: Paths::pack(removed),
                truncated,
                version,
            }
        }
        false => Reply::Files {
            files: Paths::pack(&files),
            truncated,
            version,
        },
    };
    *listed = Listed { version, files };
    responder.reply(reply);
}

/// What a file's sorted by, as shown: ignoring case, as in the tree.
fn sort_key(text: &str) -> (String, String) {
    (text.to_lowercase(), text.to_string())
}

/// The paths in `after` that aren't in `before`, and those in `before`
/// that aren't in `after`.
fn changes<'a>(
    before: &'a [PathBuf],
    after: &'a [PathBuf],
) -> (Vec<&'a PathBuf>, Vec<&'a PathBuf>) {
    let old: HashSet<&Path> = before.iter().map(PathBuf::as_path).collect();
    let new: HashSet<&Path> = after.iter().map(PathBuf::as_path).collect();
    let added = after.iter().filter(|path| !old.contains(path.as_path()));
    let removed = before.iter().filter(|path| !new.contains(path.as_path()));
    (added.collect(), removed.collect())
}

/// A walk over the workspace's files, or `None` if it has no roots. It
/// skips `.git`, `.DS_Store`, and what `.gitignore` and `.ignore` files
/// exclude. A root inside another is walked as itself and skipped in the
/// other, so each file is found once, and the other's ignore files don't
/// hide the root added. The file picker and workspace search use the same
/// rules.
pub fn walker(workspace: &Workspace) -> Option<WalkBuilder> {
    let (first, rest) = workspace.roots().split_first()?;
    let mut builder = WalkBuilder::new(first);
    for root in rest {
        builder.add(root);
    }
    skip_ignored(&mut builder);
    if !rest.is_empty() {
        let roots = workspace.roots().to_vec();
        builder.filter_entry(move |entry| {
            shown(entry) && (entry.depth() == 0 || !roots.iter().any(|root| root == entry.path()))
        });
    }
    Some(builder)
}

/// Makes `builder` skip `.git`, `.DS_Store`, and what `.gitignore` and
/// `.ignore` files exclude. The tree dims what these rules skip.
pub fn skip_ignored(builder: &mut WalkBuilder) -> &mut WalkBuilder {
    builder
        // Dotfiles such as .env are shown, as in the tree; .git and
        // .DS_Store never are.
        .hidden(false)
        .filter_entry(shown)
        // Honor .gitignore in folders that aren't git repositories too.
        .require_git(false)
}

/// Whether a walked entry is ever shown.
fn shown(entry: &DirEntry) -> bool {
    !tree::is_hidden(entry.file_name())
}

/// Whether a walked entry is a file, or a symlink to one.
pub fn is_file(entry: &DirEntry) -> bool {
    match entry.file_type() {
        Some(kind) if kind.is_symlink() => entry.path().is_file(),
        Some(kind) => kind.is_file(),
        None => false,
    }
}

/// Walks in parallel, sending each file's path.
fn walk(walker: Option<WalkBuilder>, found: Sender<PathBuf>) {
    let Some(walker) = walker else {
        return;
    };
    walker.build_parallel().run(|| {
        let found = found.clone();
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if is_file(&entry) && found.send(entry.into_path()).is_err() {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// A fresh workspace folder with `files`.
    fn fixture(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-index-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn index(root: &Path) -> FileIndex {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        let mut index = FileIndex::new(&Link::local(), &workspace);
        index.wait();
        index
    }

    fn texts(index: &FileIndex) -> Vec<String> {
        index.files().iter().map(|item| item.text.clone()).collect()
    }

    #[test]
    fn lists_files_skipping_git_and_ignored_ones() {
        let root = fixture(
            "ignore",
            &[
                ".gitignore",
                ".git/HEAD",
                ".env",
                "b.rs",
                "a/c.rs",
                "target/debug/out",
                "logs/x.log",
            ],
        );
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        let index = index(&root);
        assert_eq!(texts(&index), [".env", ".gitignore", "a/c.rs", "b.rs"]);
        assert!(!index.listing());
    }

    #[test]
    fn files_are_sorted_ignoring_case() {
        let root = fixture("case", &["b.md", "C.md", "a.md"]);
        assert_eq!(texts(&index(&root)), ["a.md", "b.md", "C.md"]);
    }

    #[test]
    fn nested_roots_list_each_file_once() {
        let root = fixture("nested", &["inner/a.rs", "b.rs"]);
        for roots in [
            [root.clone(), root.join("inner")],
            [root.join("inner"), root.clone()],
        ] {
            let mut index = FileIndex::new(&Link::local(), &Workspace::new(roots).unwrap());
            index.wait();
            assert_eq!(texts(&index), ["inner/a.rs", "nested/b.rs"]);
        }
    }

    #[test]
    fn a_root_inside_an_ignored_folder_is_listed() {
        let root = fixture(
            "nested-ignored",
            &[
                ".gitignore",
                "a.rs",
                "worktrees/feature/b.rs",
                "worktrees/other/c.rs",
            ],
        );
        fs::write(root.join(".gitignore"), "worktrees/\n").unwrap();
        let workspace = Workspace::new([root.clone(), root.join("worktrees/feature")]).unwrap();
        let mut index = FileIndex::new(&Link::local(), &workspace);
        index.wait();
        assert_eq!(
            texts(&index),
            [
                "feature/b.rs",
                "nested-ignored/.gitignore",
                "nested-ignored/a.rs"
            ]
        );
    }

    #[test]
    fn a_refresh_keeps_the_old_list_until_the_new_one_is_complete() {
        let root = fixture("refresh", &["a.rs"]);
        let mut index = index(&root);
        let before = index.files();
        fs::write(root.join("b.rs"), "").unwrap();
        index.refresh();
        // Asked again mid-listing, it lists once more afterwards.
        index.refresh();
        assert!(index.again);
        assert!(!index.listing(), "there's a whole list meanwhile");
        assert_eq!(texts(&index), ["a.rs"]);

        index.wait();
        assert!(!index.again);
        assert_eq!(texts(&index), ["a.rs", "b.rs"]);
        assert_eq!(before.len(), 1, "a picker's copy is left alone");
    }

    #[test]
    fn paths_pack_by_what_they_share() {
        let paths: Vec<PathBuf> = ["/w/src/a.rs", "/w/src/b.rs", "/w/z"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let packed = Paths::pack(&paths);
        assert_eq!(packed.0[1], (7, b"b.rs".to_vec()));
        assert_eq!(packed.0[2], (3, b"z".to_vec()));
        assert_eq!(packed.unpack(), paths);
    }

    #[test]
    fn a_refresh_sends_only_what_changed() {
        let root = fixture("changes", &["a.rs", "b.rs", "c/d.rs"]);
        let link = Link::local();
        let workspace = Workspace::new([root.clone()]).unwrap();
        let list = |since| {
            link.call(Request::ListFiles {
                workspace: workspace.clone(),
                progress: false,
                since,
            })
            .wait()
        };
        let Some(Reply::Files { version, .. }) = list(None) else {
            panic!("a whole list comes first");
        };
        fs::remove_file(root.join("b.rs")).unwrap();
        fs::write(root.join("c/e.rs"), "").unwrap();
        match list(Some(version)) {
            Some(Reply::FilesChanged { added, removed, .. }) => {
                assert_eq!(added.unpack(), [root.join("c/e.rs")]);
                assert_eq!(removed.unpack(), [root.join("b.rs")]);
            }
            other => panic!("{other:?}"),
        }
        // A list the host didn't keep: a whole one.
        assert!(matches!(list(Some(version)), Some(Reply::Files { .. })));

        let mut index = FileIndex::new(&link, &workspace);
        index.wait();
        fs::write(root.join("b.rs"), "").unwrap();
        fs::remove_file(root.join("a.rs")).unwrap();
        index.refresh();
        index.wait();
        assert_eq!(texts(&index), ["b.rs", "c/d.rs", "c/e.rs"]);
    }
}
