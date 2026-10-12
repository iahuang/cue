//! A file as a commit changed it, opened from the log: what it was in the
//! commit's first parent against what it was in the commit, shown as diff
//! mode shows a file's changes, but read-only, since neither is the file
//! as it is now.

use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use opentui::Buffer;

use crate::diff::DiffView;
use crate::git::{Base, Change, Commit, GitAnswer, GitCall};
use crate::host::{Call, Link, Reply, Request};
use crate::input::{Key, Mouse};
use crate::keymap::Command;
use crate::layout::Rect;
use crate::scroller::{Ran, Scrolled};
use crate::status::Status;

/// What a key or command did, for the app to finish.
pub enum Outcome {
    Continue,
    /// Put this text on the clipboard.
    Copy(String),
    /// Say this in the status bar.
    Message(&'static str),
}

pub struct CommitDiff {
    /// The repository's top folder.
    root: PathBuf,
    commit: Commit,
    change: Change,
    /// Empty until git has said what the file was on both sides.
    view: DiffView,
    /// Asking git that.
    reading: Option<Call>,
    size: (u32, u32),
}

impl CommitDiff {
    /// How `commit`, in the repository at `root`, made `change`, once git on
    /// `host` has read both sides.
    pub fn open(host: &Link, root: &Path, commit: Commit, change: Change) -> CommitDiff {
        let empty = || Base::Text(String::new());
        let diff = CommitDiff {
            root: root.to_path_buf(),
            view: DiffView::fixed(empty(), empty(), change.path.clone()),
            reading: Some(host.call(Request::Git(GitCall::Sides {
                root: root.to_path_buf(),
                commit: commit.clone(),
                change: change.clone(),
            }))),
            size: (1, 1),
            commit,
            change,
        };
        #[cfg(test)]
        let diff = {
            let mut diff = diff;
            while crate::host::at_once() && diff.reading.is_some() {
                diff.poll();
            }
            diff
        };
        diff
    }

    /// Takes in both sides, once git has read them. Returns whether it did.
    pub fn poll(&mut self) -> bool {
        let Some(reading) = &self.reading else {
            return false;
        };
        let (old, new) = match reading.try_recv() {
            Ok(Reply::Git(GitAnswer::Sides(old, new))) => (old, new),
            Ok(_) | Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => (Base::Binary, Base::Binary),
        };
        self.reading = None;
        self.view = DiffView::fixed(old, new, self.change.path.clone());
        self.view.set_size(self.size.0, self.size.1);
        true
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn commit(&self) -> &Commit {
        &self.commit
    }

    pub fn change(&self) -> &Change {
        &self.change
    }

    /// Whether it's of the same file in the same commit as `other`.
    pub fn is(&self, root: &Path, hash: &str, path: &Path) -> bool {
        self.root == root && self.commit.hash == hash && self.change.path == path
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.size = (width, height);
        self.view.set_size(width, height);
    }

    /// Lays the text out again, as when the theme changed.
    pub fn invalidate(&mut self) {
        self.view.invalidate();
    }

    pub fn draw(&self, frame: &Buffer, area: Rect) {
        self.view.draw(frame, area.x, area.y);
    }

    /// A mouse event at (`mouse.x`, `mouse.y`) from the top left of the
    /// text, `above` it when dragged there.
    pub fn handle_mouse(&mut self, mouse: Mouse, above: bool, now: Instant) {
        // Double clicks don't go to editing: it's not the file as it is.
        self.view.handle_mouse(mouse, above, now);
    }

    /// See [`Scrolled::drag_scroll`].
    pub fn drag_scroll(&mut self, now: Instant) -> bool {
        self.view.drag_scroll(now)
    }

    /// See [`Scrolled::drag_scrolls_in`].
    pub fn drag_scrolls_in(&self, now: Instant) -> Option<Duration> {
        self.view.drag_scrolls_in(now)
    }

    pub fn status(&self) -> Status {
        self.view.status()
    }

    /// Runs an editor command: keys that move the cursor scroll, and
    /// those that edit don't.
    pub fn run(&mut self, command: Command) -> Outcome {
        match self.view.run(command) {
            Ran::Done => Outcome::Continue,
            Ran::Copy(Some(text)) => Outcome::Copy(text),
            Ran::Copy(None) => Outcome::Message("Nothing selected."),
            Ran::ReadOnly => Outcome::Message(READ_ONLY),
        }
    }

    /// Types a key bound to no command: Space pages down, and Shift+Space
    /// up.
    pub fn type_key(&mut self, key: Key) -> Outcome {
        match self.view.type_key(key) {
            true => Outcome::Message(READ_ONLY),
            false => Outcome::Continue,
        }
    }
}

const READ_ONLY: &str = "Commit diffs are read-only.";
