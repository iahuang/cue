//! Every file in the workspace, for the file picker, skipping `.git` and
//! whatever `.gitignore` and `.ignore` files exclude.
//!
//! Background threads do the listing, so a large folder never blocks the
//! screen. The first listing starts with the app and shows up as it goes.
//! After that the list is kept for the whole session: a refresh lists the
//! files again in the background and swaps the new list in only when it's
//! complete, so the picker always has a whole list to show. Nothing watches
//! the file system yet, so the app refreshes when the picker opens and after
//! saving.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use ignore::{DirEntry, WalkBuilder, WalkState};

use crate::picker::Item;
use crate::tree;
use crate::workspace::Workspace;

/// Files beyond this many aren't listed, to bound memory in huge folders.
const MAX_FILES: usize = 200_000;
/// How often the first listing reports the files found so far.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// What a listing sends back.
enum Message {
    /// More files, during the first listing.
    Found(Vec<Item>),
    /// The whole list, sorted by path ignoring case.
    Done { files: Vec<Item>, truncated: bool },
}

pub struct FileIndex {
    workspace: Workspace,
    /// Shared with an open picker, which keeps its copy until given a new one.
    files: Rc<Vec<Item>>,
    /// Whether `files` is a whole listing, not the first one in progress.
    complete: bool,
    /// Whether some files went unlisted, past `MAX_FILES`.
    truncated: bool,
    /// The listing in progress, if any.
    listing: Option<Receiver<Message>>,
    /// A refresh was asked for during a listing: list again after it.
    again: bool,
}

impl FileIndex {
    /// Starts listing the files in `workspace`.
    pub fn new(workspace: &Workspace) -> FileIndex {
        let mut index = FileIndex {
            workspace: workspace.clone(),
            files: Rc::new(Vec::new()),
            complete: false,
            truncated: false,
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
            self.listing = Some(list(&self.workspace, !self.complete));
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
                Ok(Message::Found(found)) => {
                    Rc::make_mut(&mut self.files).extend(found);
                    changed = true;
                }
                Ok(Message::Done { files, truncated }) => {
                    self.files = Rc::new(files);
                    self.truncated = truncated;
                    self.complete = true;
                    changed = true;
                    break;
                }
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

    /// Waits for the listing in progress to finish, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) {
        while self.listing.is_some() {
            self.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Starts listing the files in `workspace` on other threads, reporting the
/// files found so far along the way if `progress`. Folders inside another
/// root are listed once, as part of it. The listing stops early if the
/// receiver is dropped.
fn list(workspace: &Workspace, progress: bool) -> Receiver<Message> {
    let (sender, receiver) = mpsc::channel();
    let workspace = workspace.clone();
    std::thread::spawn(move || collect(&workspace, progress, &sender));
    receiver
}

/// Collects the paths the walk finds into items, and sends them.
fn collect(workspace: &Workspace, progress: bool, sender: &Sender<Message>) {
    let (found, paths) = mpsc::channel();
    let walker = walker(workspace);
    std::thread::spawn(move || walk(walker, found));

    let mut files = Vec::new();
    let mut sent = 0;
    let mut last_sent = Instant::now();
    let mut truncated = false;
    loop {
        match paths.recv_timeout(PROGRESS_INTERVAL) {
            Ok(path) => {
                if files.len() >= MAX_FILES {
                    // Dropping the receiver stops the walk.
                    truncated = true;
                    break;
                }
                files.push(Item::file(path, workspace, ""));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if progress && files.len() > sent && last_sent.elapsed() >= PROGRESS_INTERVAL {
            if sender.send(Message::Found(files[sent..].to_vec())).is_err() {
                return;
            }
            sent = files.len();
            last_sent = Instant::now();
        }
    }
    // Ignoring case, as in the tree.
    files.sort_by_cached_key(|item| (item.text.to_lowercase(), item.text.clone()));
    let _ = sender.send(Message::Done { files, truncated });
}

/// A walk over the workspace's files, or `None` if it has no roots. It
/// skips `.git`, `.DS_Store`, and what `.gitignore` and `.ignore` files exclude, and lists
/// folders inside another root once, as part of it. The file picker and
/// workspace search use the same rules.
pub fn walker(workspace: &Workspace) -> Option<WalkBuilder> {
    let roots = workspace.roots();
    let mut outermost = roots.iter().filter(|root| {
        !roots
            .iter()
            .any(|other| other != *root && root.starts_with(other))
    });
    let mut builder = WalkBuilder::new(outermost.next()?);
    for root in outermost {
        builder.add(root);
    }
    builder
        // Dotfiles such as .env are shown, as in the tree; .git and
        // .DS_Store never are.
        .hidden(false)
        .filter_entry(|entry| !tree::is_hidden(entry.file_name()))
        // Honor .gitignore in folders that aren't git repositories too.
        .require_git(false);
    Some(builder)
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
            .join(format!("qedit-index-{}", std::process::id()))
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
        let mut index = FileIndex::new(&Workspace::new([root.to_path_buf()]).unwrap());
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
        let workspace = Workspace::new([root.clone(), root.join("inner")]).unwrap();
        let mut index = FileIndex::new(&workspace);
        index.wait();
        assert_eq!(index.files().len(), 2);
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
}
