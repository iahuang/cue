//! A file as a commit changed it, opened from the log: what it was in the
//! commit's first parent against what it was in the commit, shown as diff
//! mode shows a file's changes, but read-only, since neither is the file
//! as it is now.

use std::path::{Path, PathBuf};
use std::time::Instant;

use opentui::Buffer;

use crate::diff::DiffView;
use crate::git::{self, Base, Change, Commit, Kind};
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
    view: DiffView,
}

impl CommitDiff {
    /// How `commit`, in the repository at `root`, made `change`, reading
    /// both sides from git.
    pub fn open(root: &Path, commit: Commit, change: Change) -> CommitDiff {
        let relative = |path: &Path| path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let old_path = relative(change.from.as_deref().unwrap_or(&change.path));
        let old = match (&commit.parent, change.kind) {
            (Some(parent), kind) if kind != Kind::Added => git::file_at(root, parent, &old_path),
            _ => Base::Missing,
        };
        let new = match change.kind {
            Kind::Deleted => Base::Missing,
            _ => git::file_at(root, &commit.hash, &relative(&change.path)),
        };
        CommitDiff {
            root: root.to_path_buf(),
            view: DiffView::fixed(old, new, change.path.clone()),
            commit,
            change,
        }
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
    /// text.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) {
        // Double clicks don't go to editing: it's not the file as it is.
        self.view.handle_mouse(mouse, now);
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
