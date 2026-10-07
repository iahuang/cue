//! The commit view: where a repository's changes are staged and committed,
//! shown in the sidebar in place of the changes, as the log is.
//!
//! A row across the top names the repository and its branch, goes back to
//! the changes, and has a button to the log. Under it is the box the
//! commit's message is written in, which grows with the message, then a row
//! with a switch to amend the last commit instead, and the button that
//! commits. Below are the repository's changed files, in their folders as
//! the changes view has them, each marked at the right with how much of it
//! is staged: all (●), some (◐), or none (○). A folder's mark, and the
//! mark of the row above them all, is of the files in it. Clicking a mark,
//! or `s`, stages them all, or unstages them if they all were.
//!
//! Next to the button that commits is one that stashes, with the message
//! written: what's staged, if anything is, and otherwise everything. Under
//! the changes are the stashes, which open, as the log's commits do, to
//! buttons to apply, pop, or drop them, and the files they changed.
//!
//! git commits in the background, since hooks may take a while.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{SystemTime, UNIX_EPOCH};

use opentui::{Attributes, Buffer};

use crate::changes::{button_area, draw_look, file_rows, Look};
use crate::git::{self, Change, Commit, Kind, Repo, Staged, Stash};
use crate::icons;
use crate::keymap::Command;
use crate::line_edit::{Caret, Edit};
use crate::log::ago;
use crate::theme;
use crate::tree::{truncate, Entry};

/// The most rows the message box grows to; past them, it scrolls.
const MAX_MESSAGE_ROWS: usize = 6;
/// The button on the top row that shows the log.
const LOG_BUTTON: &str = "Log";
/// The switch to amend the last commit, after its mark.
const AMEND: &str = "Amend";
/// Shown in the message box while it's empty.
const PLACEHOLDER: &str = "Message";
/// What went wrong if git went away without saying.
const STOPPED: &str = "git stopped unexpectedly.";
/// The buttons on the row under an open stash.
const STASH_BUTTONS: [(&str, StashButton); 3] = [
    ("Apply", StashButton::Apply),
    ("Pop", StashButton::Pop),
    ("Drop", StashButton::Drop),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StashButton {
    Apply,
    Pop,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitAction {
    None,
    /// Go back to the changes.
    Back,
    /// Show the log.
    Log,
    /// Show the repository's branches, to switch to one.
    Branches,
    /// Show this file in the editor, and move focus there if `focus`.
    Open {
        path: PathBuf,
        focus: bool,
        preview: bool,
    },
    /// Stage the files at `paths`, or with `stage` false, unstage them.
    Stage {
        paths: Vec<PathBuf>,
        stage: bool,
    },
    /// Commit what's staged, with the message written.
    Commit,
    /// Stash the changes as `message`: only what's staged, if `staged`.
    Stash {
        message: String,
        staged: bool,
    },
    /// Put the changes stashed as `hash` back, and if `pop`, drop the
    /// stash.
    ApplyStash {
        hash: String,
        pop: bool,
    },
    /// Drop the stash `hash`, stashed as `subject`, once that's confirmed.
    DropStash {
        hash: String,
        subject: String,
    },
    /// Show how a stash, as `commit`, made `change`, and move focus there
    /// if `focus`.
    OpenStashed {
        commit: Commit,
        change: Change,
        focus: bool,
    },
}

/// Why there's nothing to commit yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CantCommit {
    NoMessage,
    NothingStaged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// The row above all the changed files, which collapses them.
    All,
    Folder,
    File(Kind),
    /// Under that row, when nothing changed; it can't be selected.
    Note,
    /// The row above the stashes, which collapses them.
    Stashes,
    /// The stash at this index, which opens.
    Stash(usize),
    /// The buttons under an open stash.
    StashActions(usize),
    StashFolder(usize),
    /// A file the stash changed, and the commit that has it as stashed.
    StashFile(usize, Commit, Change),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    what: What,
    /// A folder's or file's path; the repository's top folder for the
    /// row above all the changed files.
    path: PathBuf,
    name: String,
    depth: usize,
    /// How much of the changed files it's of are staged.
    staged: Staged,
    /// The hash of the stash it's of, if it is.
    stash: Option<String>,
}

impl Row {
    /// Whether it's the same entry as `other`, laid out again.
    fn same(&self, other: &Row) -> bool {
        (&self.path, &self.stash) == (&other.path, &other.stash)
            && std::mem::discriminant(&self.what) == std::mem::discriminant(&other.what)
    }
}

/// The commit's message as it's written, and where in it the cursor is.
#[derive(Default)]
struct Message {
    text: String,
    caret: Caret,
    /// The first of its rows shown in the box.
    scroll: usize,
}

/// A row of the message as it wraps: the text on it, and whether it wraps
/// onto the next row, rather than ending at a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    range: Range<usize>,
    wraps: bool,
}

pub struct CommitView {
    root: PathBuf,
    /// What the repository is called.
    name: String,
    branch: String,
    /// The repository has no commits yet, so there's none to amend.
    initial: bool,
    /// A merge can be committed without any staged file differences.
    merging: bool,
    /// Sorted by path, as git lists them.
    changes: Vec<Change>,
    /// Folders collapsed, by path; the repository's, for all of them.
    collapsed: HashSet<PathBuf>,
    rows: Vec<Row>,
    selected: usize,
    /// The first row of the list on screen.
    scroll: usize,
    width: u32,
    /// Rows on screen, from the top row down.
    height: u32,
    message: Message,
    /// The keyboard is in the message box, rather than the list.
    editing: bool,
    amend: bool,
    /// The last commit's message, as turning on Amend filled it in, which
    /// turning it off takes away again, if it's still as it was.
    filled: Option<String>,
    /// The commit git is making, if it is: then its hash, or what went
    /// wrong.
    committing: Option<Receiver<Result<String, String>>>,
    /// The file shown in the editor, highlighted.
    active: Option<PathBuf>,
    /// The active file is a preview, shown in italics.
    active_preview: bool,
    /// The changes stashed, newest first.
    stashes: Vec<Stash>,
    /// The stashes collapsed under the row above them.
    stashes_collapsed: bool,
    /// The files each stash opened changed, by hash, read when it's first
    /// opened.
    stash_files: HashMap<String, Vec<(Commit, Change)>>,
    /// The stashes opened, by hash.
    open_stashes: HashSet<String>,
    /// The folders collapsed in stashes, by hash and path.
    stash_collapsed: HashSet<(String, PathBuf)>,
    /// The stashed file shown in the editor, by its commit's hash and path.
    active_stashed: Option<(String, PathBuf)>,
}

impl CommitView {
    /// The commit view of `repo`, which is called `name`.
    pub fn open(repo: &Repo, name: String) -> CommitView {
        let mut view = CommitView {
            root: repo.root.clone(),
            name,
            branch: String::new(),
            initial: false,
            merging: false,
            changes: Vec::new(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            width: 1,
            height: 1,
            message: Message::default(),
            editing: false,
            amend: false,
            filled: None,
            committing: None,
            active: None,
            active_preview: false,
            stashes: Vec::new(),
            stashes_collapsed: false,
            stash_files: HashMap::new(),
            open_stashes: HashSet::new(),
            stash_collapsed: HashSet::new(),
            active_stashed: None,
        };
        view.set_repo(repo);
        view
    }

    /// The repository's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Catches up with what git says of the repository now, keeping the
    /// selection on the same entry while it's still listed.
    pub fn set_repo(&mut self, repo: &Repo) {
        if self.branch != repo.head.name() {
            self.clear_amend();
        }
        self.branch = repo.head.name().to_string();
        self.initial = repo.commit.is_none();
        self.merging = repo.merging;
        if self.initial {
            self.amend = false;
        }
        self.changes = repo.changes.clone();
        if self.stashes != repo.stashes {
            self.set_stashes(repo.stashes.clone());
        }
        self.rebuild();
    }

    /// Reads the stashes again, as after stashing, applying, or dropping
    /// one, keeping those still there open.
    pub fn read_stashes(&mut self) {
        self.set_stashes(git::stashes(&self.root));
    }

    fn set_stashes(&mut self, stashes: Vec<Stash>) {
        self.stashes = stashes;
        let hashes: HashSet<&String> = self.stashes.iter().map(|s| &s.commit.hash).collect();
        self.open_stashes.retain(|hash| hashes.contains(hash));
        self.stash_files.retain(|hash, _| hashes.contains(hash));
        self.rebuild();
    }

    /// The changes were stashed: the message box is emptied.
    pub fn stashed(&mut self) {
        self.message = Message::default();
        self.amend = false;
        self.filled = None;
        self.read_stashes();
    }

    /// Marks the files at `paths` staged, or not, as they are once git's
    /// done, before it says so.
    pub fn set_staged(&mut self, paths: &[PathBuf], stage: bool) {
        let paths: HashSet<&PathBuf> = paths.iter().collect();
        for change in &mut self.changes {
            if paths.contains(&change.path) {
                change.staged = if stage { Staged::Yes } else { Staged::No };
            }
        }
        self.rebuild();
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.show_cursor();
        self.scroll_into_view();
    }

    /// Highlights `path` as the file in the editor, in italics if it is a
    /// preview.
    pub fn set_active(&mut self, path: Option<&Path>, preview: bool) {
        self.active = path.map(Path::to_path_buf);
        self.active_preview = preview;
    }

    /// Highlights the file `commit` changed at `path`, if it's a stash's,
    /// as the one in the editor.
    pub fn set_active_stashed(&mut self, active: Option<(&str, &Path)>) {
        self.active_stashed = active.map(|(hash, path)| (hash.to_string(), path.to_path_buf()));
    }

    /// Whether the keyboard is in the message box.
    pub fn editing(&self) -> bool {
        self.editing
    }

    /// Puts the keyboard in the message box, or back in the list.
    pub fn set_editing(&mut self, editing: bool) {
        self.editing = editing;
    }

    /// The selected changed file or folder, if any.
    pub fn selected(&self) -> Option<Entry> {
        let row = self.rows.get(self.selected)?;
        if !matches!(row.what, What::All | What::Folder | What::File(_)) {
            return None;
        }
        Some(Entry {
            path: row.path.clone(),
            is_dir: !matches!(row.what, What::File(_)),
            is_root: row.what == What::All,
        })
    }

    /// How many files are staged, wholly or partly.
    pub fn staged_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|change| change.staged != Staged::No)
            .count()
    }

    pub fn committing(&self) -> bool {
        self.committing.is_some()
    }

    /// Whether a stash, or something in one, is selected.
    pub fn stash_selected(&self) -> bool {
        self.selected_stash().is_some()
    }

    /// Selects the entry on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        let Some(row) = y.checked_sub(self.list_top() as u32) else {
            return false;
        };
        let index = self.scroll + row as usize;
        if self
            .rows
            .get(index)
            .is_none_or(|row| row.what == What::Note)
        {
            return false;
        }
        self.editing = false;
        self.select(index);
        true
    }

    /// Where the selected entry's name is on screen: the column, from the
    /// view's left edge, and the row. `None` if it's scrolled out of view.
    pub fn selected_position(&self) -> Option<(u32, u32)> {
        let row = self.rows.get(self.selected)?;
        let y = self.selected.checked_sub(self.scroll)?;
        let x = 3 + 2 * row.depth as u32;
        (y < self.list_height()).then_some((x, (self.list_top() + y) as u32))
    }

    pub fn run(&mut self, command: Command) -> CommitAction {
        let page = self.list_height().saturating_sub(1).max(1);
        match command {
            Command::TreeUp => self.step(-1),
            Command::TreeDown => self.step(1),
            Command::TreePageUp => self.select(self.selected.saturating_sub(page)),
            Command::TreePageDown => self.select(self.selected + page),
            Command::TreeFirst => self.select(0),
            Command::TreeLast => self.select(usize::MAX),
            Command::TreeExpand => self.expand_or_enter(),
            Command::TreeCollapse => self.collapse_or_leave(),
            Command::TreeOpen => return self.activate(true, false),
            Command::TreePreview => return self.activate(false, true),
            Command::TreeToggleStaged => return self.toggle_staged(self.selected),
            Command::TreeStash => return self.stash(),
            Command::TreeApplyStash => return self.stash_button(StashButton::Apply),
            Command::TreePopStash => return self.stash_button(StashButton::Pop),
            Command::TreeDropStash => return self.stash_button(StashButton::Drop),
            _ => {}
        }
        CommitAction::None
    }

    /// Stashes the changes, with the message written: what's staged, if
    /// anything is, or else all of them.
    fn stash(&self) -> CommitAction {
        if self.changes.is_empty() || self.committing() {
            return CommitAction::None;
        }
        CommitAction::Stash {
            message: self.message.text.clone(),
            staged: self.staged_count() > 0,
        }
    }

    /// The stash the selected row is of, if it is.
    fn selected_stash(&self) -> Option<&Stash> {
        let hash = self.rows.get(self.selected)?.stash.as_ref()?;
        self.stashes.iter().find(|stash| &stash.commit.hash == hash)
    }

    /// What `button` does to the selected stash.
    fn stash_button(&self, button: StashButton) -> CommitAction {
        if self.committing() && matches!(button, StashButton::Apply | StashButton::Pop) {
            return CommitAction::None;
        }
        let Some(stash) = self.selected_stash() else {
            return CommitAction::None;
        };
        let hash = stash.commit.hash.clone();
        match button {
            StashButton::Apply => CommitAction::ApplyStash { hash, pop: false },
            StashButton::Pop => CommitAction::ApplyStash { hash, pop: true },
            StashButton::Drop => CommitAction::DropStash {
                hash,
                subject: stash.commit.subject.clone(),
            },
        }
    }

    /// A left click at column `x` of screen row `y`, from the view's top
    /// left: the top row goes back to the changes, or its button shows the
    /// log; a click in the message box puts the cursor there; the row under
    /// it amends or commits; and in the list, a mark stages or unstages,
    /// and elsewhere, a click selects the entry there, then collapses or
    /// expands a folder or opens a file: a single click previews it, and a
    /// `double` click keeps it open and moves focus to it.
    pub fn click(&mut self, x: u32, y: u32, double: bool) -> CommitAction {
        let rows = self.message_rows() as u32;
        let actions = rows + 3;
        if y == 0 {
            return if self.back_area().contains(&x) {
                CommitAction::Back
            } else if self.log_area().contains(&x) {
                CommitAction::Log
            } else if self.branch_area().contains(&x) {
                CommitAction::Branches
            } else {
                CommitAction::None
            };
        }
        if y < actions {
            self.editing = true;
            let row = (y.saturating_sub(2) as usize).min(rows as usize - 1) + self.message.scroll;
            let room = self.room();
            let lines = wrap(&self.message.text, room);
            let row = row.min(lines.len() - 1);
            let at = offset_at(
                &self.message.text,
                &lines[row],
                x.saturating_sub(3) as usize,
            );
            self.message.caret.move_to(&self.message.text, at, false);
            return CommitAction::None;
        }
        if y == actions {
            if self.amend_area().contains(&x) {
                self.toggle_amend();
            } else if self.stash_area().contains(&x) {
                return self.stash();
            } else if self.commit_area().contains(&x) {
                return CommitAction::Commit;
            }
            return CommitAction::None;
        }
        if !self.select_at(y) {
            return CommitAction::None;
        }
        let row = &self.rows[self.selected];
        match row.what {
            What::StashActions(_) => {
                let buttons = self.stash_buttons(row.depth);
                return match buttons.into_iter().find(|(area, _)| area.contains(&x)) {
                    Some((_, button)) => self.stash_button(button),
                    None => CommitAction::None,
                };
            }
            What::All | What::Folder | What::File(_) if self.mark_area().contains(&x) => {
                return self.toggle_staged(self.selected);
            }
            _ => {}
        }
        self.activate(double, !double)
    }

    /// Scrolls the list by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let max = self.rows.len().saturating_sub(self.list_height());
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
    }

    /// The columns of what's under the pointer at column `x` of screen row
    /// `y`, that a click does something to: a button, a row of the list,
    /// or the mark at its end. The message box is for typing in.
    pub fn hover(&self, x: u32, y: u32) -> Option<Range<u32>> {
        let actions = self.message_rows() as u32 + 3;
        let areas = match y {
            0 => vec![self.back_area(), self.branch_area(), self.log_area()],
            y if y == actions => vec![self.amend_area(), self.stash_area(), self.commit_area()],
            y if y > actions => {
                let row = self.rows.get(self.scroll + (y - actions - 1) as usize)?;
                match row.what {
                    What::Note => return None,
                    What::All | What::Folder | What::File(_) => {
                        vec![0..self.mark_area().start, self.mark_area()]
                    }
                    What::StashActions(_) => {
                        let buttons = self.stash_buttons(row.depth).into_iter();
                        buttons.map(|(area, _)| area).collect()
                    }
                    _ => return Some(0..self.width),
                }
            }
            _ => return None,
        };
        areas.into_iter().find(|area| area.contains(&x))
    }

    // --- what's where ---------------------------------------------------------

    /// The top row's button back to the changes: the arrow and the
    /// repository's name.
    fn back_area(&self) -> Range<u32> {
        let name = truncate(&self.name, self.name_room());
        0..4 + name.chars().count() as u32
    }

    /// The branch on the top row, after the repository's name, which shows
    /// the branches, and the column it starts at, its icon's if it has one;
    /// `None` if there's no room for it.
    fn top_branch(&self) -> Option<(String, u32)> {
        let name = truncate(&self.name, self.name_room());
        let start = 3 + name.chars().count() as u32 + 2;
        let log = self.log_area().start + 1;
        let room = log.saturating_sub(start + icons::width() + 1) as usize;
        (room > 0).then(|| (truncate(&self.branch, room), start))
    }

    /// The top row's button that shows the branches: the branch.
    fn branch_area(&self) -> Range<u32> {
        match self.top_branch() {
            Some((branch, start)) => {
                button_area(start..start + icons::width() + branch.chars().count() as u32)
            }
            None => 0..0,
        }
    }

    /// The top row's button to the log.
    fn log_area(&self) -> Range<u32> {
        let start = self.width.saturating_sub(LOG_BUTTON.len() as u32 + 1);
        button_area(start..start + LOG_BUTTON.len() as u32)
    }

    /// The columns the repository's name has on the top row.
    fn name_room(&self) -> usize {
        let log = self.width.saturating_sub(LOG_BUTTON.len() as u32 + 1);
        log.saturating_sub(4) as usize
    }

    /// The switch to amend, under the message box: its mark and label.
    fn amend_area(&self) -> Range<u32> {
        0..4 + AMEND.len() as u32
    }

    /// The button that commits, at the end of the row the switch is on.
    fn commit_area(&self) -> Range<u32> {
        let width = self.button_label().chars().count() as u32;
        let start = self.width.saturating_sub(width + 1);
        button_area(start..start + width)
    }

    /// The button that stashes, before the one that commits, if there's
    /// room for it.
    fn stash_area(&self) -> Range<u32> {
        let commit = self.commit_area();
        let width = self.stash_label().chars().count() as u32;
        let start = (commit.start + 1).saturating_sub(width + 2);
        match start > self.amend_area().end {
            true => button_area(start..start + width),
            false => 0..0,
        }
    }

    /// Where the buttons under a stash at `depth` are, and which each is.
    fn stash_buttons(&self, depth: usize) -> Vec<(Range<u32>, StashButton)> {
        let mut x = 1 + 2 * depth as u32;
        STASH_BUTTONS
            .iter()
            .map(|&(label, button)| {
                let columns = x..x + label.len() as u32;
                x = columns.end + 2;
                (button_area(columns), button)
            })
            .filter(|(area, _)| area.end <= self.width)
            .collect()
    }

    /// A row's mark, at the end of the list's rows.
    fn mark_area(&self) -> Range<u32> {
        self.width.saturating_sub(3)..self.width
    }

    // --- the message ----------------------------------------------------------

    /// What's written in the message box.
    #[cfg(test)]
    pub fn message(&self) -> &str {
        &self.message.text
    }

    /// Applies `edit` to the message, with Shift held if `select`.
    pub fn edit(&mut self, edit: Edit, select: bool) {
        if self.committing() {
            return;
        }
        let Message { text, caret, .. } = &mut self.message;
        let edit = match edit {
            // Terminals send line breaks in pastes as CR.
            Edit::Insert(inserted) if inserted.contains('\r') => {
                let inserted = inserted.replace("\r\n", "\n").replace('\r', "\n");
                caret.edit(text, Edit::Insert(&inserted));
                self.show_cursor();
                return;
            }
            edit => edit,
        };
        match select {
            true => caret.select(text, edit),
            false => caret.edit(text, edit),
        };
        self.show_cursor();
    }

    /// Moves the cursor `by` rows of the message as it wraps: from the
    /// first row up, to the start; from the last down, to the end.
    pub fn move_rows(&mut self, by: isize, select: bool) {
        let room = self.room();
        let text = &self.message.text;
        let lines = wrap(text, room);
        let at = self.message.caret.at(text);
        let row = row_of(&lines, at);
        let column = text[lines[row].range.start..at].chars().count();
        let at = match row.checked_add_signed(by) {
            Some(row) if row < lines.len() => offset_at(text, &lines[row], column),
            _ if by < 0 => 0,
            _ => text.len(),
        };
        self.message.caret.move_to(text, at, select);
        self.show_cursor();
    }

    /// Moves the cursor to the start of its row of the message, or with
    /// `end`, to the end.
    pub fn move_to_row_edge(&mut self, end: bool, select: bool) {
        let room = self.room();
        let text = &self.message.text;
        let lines = wrap(text, room);
        let line = &lines[row_of(&lines, self.message.caret.at(text))];
        let at = match end {
            true => offset_at(text, line, usize::MAX),
            false => line.range.start,
        };
        self.message.caret.move_to(text, at, select);
        self.show_cursor();
    }

    pub fn select_all(&mut self) {
        self.message.caret.select_all(&self.message.text);
    }

    /// Takes away the selection. Returns false if there was none.
    pub fn clear_selection(&mut self) -> bool {
        let text = &self.message.text;
        if self.message.caret.selection(text).is_none() {
            return false;
        }
        let at = self.message.caret.at(text);
        self.message.caret.move_to(text, at, false);
        true
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.message.caret.selected_text(&self.message.text)
    }

    /// Turns amending the last commit on or off. On, an empty message box
    /// gets the last commit's message; off, it's taken away again if it
    /// wasn't changed.
    pub fn toggle_amend(&mut self) {
        if self.initial || self.committing() {
            return;
        }
        self.amend = !self.amend;
        if self.amend && self.message.text.trim().is_empty() {
            if let Some(last) = git::last_message(&self.root) {
                self.message = Message::default();
                self.message.text = last.clone();
                self.filled = Some(last);
            }
        } else if !self.amend && self.filled.take().as_ref() == Some(&self.message.text) {
            self.message = Message::default();
        }
        self.show_cursor();
    }

    /// Leaves Amend after switching branches, before the next repository
    /// refresh. Keeps a message the user edited, as turning Amend off does.
    pub fn clear_amend(&mut self) {
        if self.amend {
            self.toggle_amend();
        }
    }

    /// Starts committing what's staged, with the message written, in the
    /// background, unless there's nothing to commit yet, or a commit is
    /// being made already.
    pub fn commit(&mut self) -> Result<(), CantCommit> {
        if self.committing() {
            return Ok(());
        }
        if self.message.text.trim().is_empty() {
            return Err(CantCommit::NoMessage);
        }
        if !self.amend && !self.merging && self.staged_count() == 0 {
            return Err(CantCommit::NothingStaged);
        }
        let (sender, receiver) = mpsc::channel();
        let root = self.root.clone();
        let message = self.message.text.clone();
        let amend = self.amend;
        std::thread::spawn(move || {
            let _ = sender.send(git::commit(&root, &message, amend));
        });
        self.committing = Some(receiver);
        Ok(())
    }

    /// Takes how the commit went, once git's done: the new commit's
    /// abbreviated hash, or what went wrong. Once it's made, the message
    /// box is emptied, and Amend turned off.
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        let result = match self.committing.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(STOPPED.to_string()),
        };
        Some(self.finish(result))
    }

    /// Waits for the commit being made, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) -> Option<Result<String, String>> {
        let result = self.committing.as_ref()?.recv();
        Some(self.finish(result.unwrap_or_else(|_| Err(STOPPED.to_string()))))
    }

    fn finish(&mut self, result: Result<String, String>) -> Result<String, String> {
        self.committing = None;
        if result.is_ok() {
            self.message = Message::default();
            self.amend = false;
            self.filled = None;
        }
        result
    }

    // --- drawing --------------------------------------------------------------

    /// Draws the view in the columns from `x` to `x + width`. Returns where
    /// the cursor goes, if the keyboard's in the message box.
    pub fn draw(&self, frame: &Buffer, x: u32, focused: bool) -> Option<(u32, u32)> {
        let width = self.width;
        let mut cursor = None;
        frame.with_clip(x, 0, width, self.height, || {
            self.draw_top(frame, x);
            cursor = self.draw_message(frame, x, focused && self.editing);
            self.draw_actions(frame, x);
            let top = self.list_top() as u32;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64);
            let visible = self.scroll..self.rows.len();
            for (index, y) in visible.zip(top..self.height) {
                let selected = (index == self.selected).then_some(focused && !self.editing);
                self.draw_row(frame, index, (x, y, width), selected, now);
            }
        });
        cursor
    }

    /// The top row: back to the changes, the repository and its branch,
    /// and the button to the log.
    fn draw_top(&self, frame: &Buffer, x: u32) {
        let colors = theme::colors();
        frame.draw_text("‹", x + 1, 0, colors.muted, None, Attributes::NONE);
        let end = x + self.width;
        let log_x = end.saturating_sub(LOG_BUTTON.len() as u32 + 1);
        let name_x = x + 3;
        let name = truncate(&self.name, self.name_room());
        frame.draw_text(&name, name_x, 0, colors.text, None, Attributes::BOLD);
        if let Some((branch, start)) = self.top_branch() {
            let mut at = x + start;
            if icons::enabled() {
                at = icons::branch().draw(frame, at, 0, None);
            }
            frame.draw_text(&branch, at, 0, colors.muted, None, Attributes::NONE);
        }
        if log_x > name_x + name.chars().count() as u32 {
            frame.draw_text(LOG_BUTTON, log_x, 0, colors.muted, None, Attributes::NONE);
        }
    }

    /// The message box, bordered, with the cursor in it if `editing`.
    fn draw_message(&self, frame: &Buffer, x: u32, editing: bool) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let rows = self.message_rows() as u32;
        let border = match editing {
            true => colors.accent,
            false => colors.border,
        };
        let inner = self.width.saturating_sub(4) as usize;
        let (left, right) = (x + 1, x + self.width.saturating_sub(2));
        let edge = "─".repeat(inner);
        let top = format!("╭{edge}╮");
        let bottom = format!("╰{edge}╯");
        frame.draw_text(&top, left, 1, border, None, Attributes::NONE);
        frame.draw_text(&bottom, left, rows + 2, border, None, Attributes::NONE);
        for y in 2..rows + 2 {
            frame.draw_text("│", left, y, border, None, Attributes::NONE);
            frame.draw_text("│", right, y, border, None, Attributes::NONE);
        }
        let text = &self.message.text;
        let text_x = x + 3;
        if text.is_empty() {
            let placeholder = truncate(PLACEHOLDER, self.room());
            frame.draw_text(
                &placeholder,
                text_x,
                2,
                colors.faint,
                None,
                Attributes::NONE,
            );
        }
        let lines = wrap(text, self.room());
        let selection = self.message.caret.selection(text);
        let shown = lines.iter().enumerate().skip(self.message.scroll);
        for ((_, line), y) in shown.zip(2..rows + 2) {
            if let Some(selection) = &selection {
                let start = selection.start.max(line.range.start);
                let end = selection.end.min(line.range.end);
                if start < end {
                    let column = text[line.range.start..start].chars().count() as u32;
                    let count = text[start..end].chars().count().max(1) as u32;
                    frame.fill_rect(text_x + column, y, count, 1, colors.selection);
                }
            }
            let shown = text[line.range.clone()].trim_end_matches('\n');
            frame.draw_text(shown, text_x, y, colors.text, None, Attributes::NONE);
        }
        if !editing {
            return None;
        }
        let at = self.message.caret.at(text);
        let row = row_of(&lines, at);
        let column = text[lines[row].range.start..at].chars().count() as u32;
        let y = (row - self.message.scroll.min(row)) as u32 + 2;
        (y < rows + 2).then_some((text_x + column, y))
    }

    /// The row under the message box: the switch to amend, and the button
    /// that commits.
    fn draw_actions(&self, frame: &Buffer, x: u32) {
        let colors = theme::colors();
        let y = self.message_rows() as u32 + 3;
        let (mark, fg) = match (self.amend, self.initial) {
            (true, _) => ("●", colors.accent),
            (false, false) => ("○", colors.muted),
            (false, true) => ("○", colors.faint),
        };
        frame.draw_text(mark, x + 1, y, fg, None, Attributes::NONE);
        let label_fg = if self.initial {
            colors.faint
        } else {
            colors.text
        };
        frame.draw_text(AMEND, x + 3, y, label_fg, None, Attributes::NONE);
        let label = self.button_label();
        let button_x = (x + self.width).saturating_sub(label.chars().count() as u32 + 1);
        if button_x > x + 3 + AMEND.len() as u32 {
            let ready = !self.committing()
                && !self.message.text.trim().is_empty()
                && (self.amend || self.merging || self.staged_count() > 0);
            let (fg, attributes) = match ready {
                true => (colors.accent, Attributes::BOLD),
                false => (colors.faint, Attributes::NONE),
            };
            frame.draw_text(&label, button_x, y, fg, None, attributes);
        }
        let stash = self.stash_area();
        if !stash.is_empty() {
            let fg = match self.changes.is_empty() || self.committing() {
                true => colors.faint,
                false => colors.text,
            };
            let label = self.stash_label();
            frame.draw_text(&label, x + stash.start + 1, y, fg, None, Attributes::NONE);
        }
    }

    /// What the button that stashes says: how many files it stashes, if
    /// some are staged, or that it stashes all of them.
    fn stash_label(&self) -> String {
        match self.staged_count() {
            0 => "Stash all".to_string(),
            count => format!("Stash {count}"),
        }
    }

    /// What the button that commits says.
    fn button_label(&self) -> String {
        if self.committing() {
            return "Committing…".to_string();
        }
        if self.amend {
            return "Amend".to_string();
        }
        match self.staged_count() {
            0 => "Commit".to_string(),
            count => format!("Commit {count}"),
        }
    }

    fn draw_row(
        &self,
        frame: &Buffer,
        index: usize,
        at: (u32, u32, u32),
        selected: Option<bool>,
        now: i64,
    ) {
        let colors = theme::colors();
        let row = &self.rows[index];
        let active = match &row.what {
            What::StashFile(_, commit, change) => self
                .active_stashed
                .as_ref()
                .is_some_and(|(hash, path)| *hash == commit.hash && *path == change.path),
            What::File(_) => self.active.as_ref() == Some(&row.path),
            _ => false,
        };
        let kind = match &row.what {
            What::File(kind) => Some(*kind),
            What::StashFile(_, _, change) => Some(change.kind),
            _ => None,
        };
        let (fg, mut attributes) = match &row.what {
            What::All | What::Stashes => (colors.text, Attributes::BOLD),
            What::Note | What::StashActions(_) => (colors.faint, Attributes::NONE),
            _ if active && self.active_preview && row.stash.is_none() => {
                (colors.accent, Attributes::BOLD | Attributes::ITALIC)
            }
            _ if active => (colors.accent, Attributes::BOLD),
            _ => match kind {
                Some(kind) => (colors.hue(kind.hue()), Attributes::NONE),
                None => (colors.text, Attributes::NONE),
            },
        };
        if kind == Some(Kind::Deleted) {
            attributes |= Attributes::STRIKETHROUGH;
        }
        if let What::StashActions(_) = row.what {
            if let Some(focused) = selected {
                let bg = match focused {
                    true => colors.selected,
                    false => colors.selected_unfocused,
                };
                frame.fill_rect(at.0, at.1, at.2, 1, bg);
            }
            for (&(label, _), (area, _)) in STASH_BUTTONS.iter().zip(self.stash_buttons(row.depth))
            {
                frame.draw_text(
                    label,
                    at.0 + area.start + 1,
                    at.1,
                    colors.muted,
                    None,
                    Attributes::NONE,
                );
            }
            return;
        }
        let letter;
        let ago_text;
        let count;
        let right = match &row.what {
            _ if kind.is_some() => {
                let kind = kind.unwrap_or(Kind::Modified);
                letter = kind.letter().to_string();
                Some((letter.as_str(), colors.hue(kind.hue())))
            }
            What::Stash(i) => {
                ago_text = ago(now - self.stashes[*i].commit.time);
                Some((ago_text.as_str(), colors.muted))
            }
            What::Stashes => {
                count = self.stashes.len().to_string();
                Some((count.as_str(), colors.muted))
            }
            _ => None,
        };
        let open = self.is_open(row);
        let icon = match &row.what {
            What::Folder | What::StashFolder(_) => Some(icons::folder(open == Some(true))),
            What::File(_) | What::StashFile(..) => Some(icons::file(&row.name)),
            _ => None,
        };
        let mark = match (&row.what, row.staged) {
            (What::All | What::Folder | What::File(_), Staged::Yes) => Some(("●", colors.accent)),
            (What::All | What::Folder | What::File(_), Staged::Partly) => {
                Some(("◐", colors.accent))
            }
            (What::All | What::Folder | What::File(_), Staged::No) => Some(("○", colors.muted)),
            // So how a stash changed a file lines up with how files changed.
            (What::StashFile(..), _) => Some((" ", colors.muted)),
            _ => None,
        };
        let look = Look {
            depth: row.depth,
            flush: false,
            open,
            icon,
            name: &row.name,
            fg,
            attributes,
            right,
            mark,
        };
        draw_look(frame, &look, at, selected);
    }

    /// Whether `row` is open, if it opens: the rows above the changes and
    /// the stashes, folders, and stashes.
    fn is_open(&self, row: &Row) -> Option<bool> {
        let hash = row.stash.clone().unwrap_or_default();
        match &row.what {
            What::All | What::Folder => Some(!self.collapsed.contains(&row.path)),
            What::Stashes => Some(!self.stashes_collapsed),
            What::Stash(_) => Some(self.open_stashes.contains(&hash)),
            What::StashFolder(_) => Some(!self.stash_collapsed.contains(&(hash, row.path.clone()))),
            _ => None,
        }
    }

    /// Opens `row`, or closes it if it's open, reading a stash's files the
    /// first time it opens.
    fn toggle(&mut self, row: &Row) {
        let hash = row.stash.clone().unwrap_or_default();
        match &row.what {
            What::All | What::Folder => {
                if !self.collapsed.remove(&row.path) {
                    self.collapsed.insert(row.path.clone());
                }
            }
            What::Stashes => self.stashes_collapsed = !self.stashes_collapsed,
            What::Stash(i) => {
                if !self.open_stashes.remove(&hash) {
                    if !self.stash_files.contains_key(&hash) {
                        let files = git::stash_changes(&self.root, &self.stashes[*i]);
                        self.stash_files.insert(hash.clone(), files);
                    }
                    self.open_stashes.insert(hash);
                }
            }
            What::StashFolder(_) => {
                let key = (hash, row.path.clone());
                if !self.stash_collapsed.remove(&key) {
                    self.stash_collapsed.insert(key);
                }
            }
            _ => return,
        }
        self.rebuild();
    }

    // --- layout ---------------------------------------------------------------

    /// The columns a row of the message has, inside the box's borders and
    /// the space along each.
    fn room(&self) -> usize {
        (self.width.saturating_sub(6) as usize).max(1)
    }

    /// The rows the message box shows: as many as the message takes, up to
    /// [`MAX_MESSAGE_ROWS`].
    fn message_rows(&self) -> usize {
        wrap(&self.message.text, self.room())
            .len()
            .clamp(1, MAX_MESSAGE_ROWS)
    }

    /// The screen row the list starts on: below the top row, the message
    /// box and its borders, and the row under it.
    fn list_top(&self) -> usize {
        self.message_rows() + 4
    }

    fn list_height(&self) -> usize {
        (self.height as usize)
            .saturating_sub(self.list_top())
            .max(1)
    }

    /// Scrolls the message box to the cursor's row.
    fn show_cursor(&mut self) {
        let text = &self.message.text;
        let lines = wrap(text, self.room());
        let row = row_of(&lines, self.message.caret.at(text));
        let rows = lines.len().clamp(1, MAX_MESSAGE_ROWS);
        let scroll = &mut self.message.scroll;
        if row < *scroll {
            *scroll = row;
        } else if row >= *scroll + rows {
            *scroll = row + 1 - rows;
        }
        *scroll = (*scroll).min(lines.len().saturating_sub(rows));
        // The box may have grown or shrunk.
        self.scroll_into_view();
    }

    // --- rows -----------------------------------------------------------------

    /// Lists the rows again, from the changes, the stashes, and what's
    /// open, keeping the selection on the same entry while it's still
    /// listed.
    fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).cloned();
        let row = |what, path: PathBuf, name: &str, depth| Row {
            what,
            path,
            name: name.to_string(),
            depth,
            staged: Staged::No,
            stash: None,
        };
        let mut rows = vec![Row {
            staged: staged_under(&self.changes, &self.root),
            ..row(What::All, self.root.clone(), "Changes", 0)
        }];
        if self.changes.is_empty() {
            rows.push(row(What::Note, PathBuf::new(), "No changes.", 0));
        } else if !self.collapsed.contains(&self.root) {
            let collapsed = |path: &Path| self.collapsed.contains(path);
            for file in file_rows(&self.changes, &self.root, 1, &collapsed) {
                let (what, staged) = match file.kind {
                    Some(kind) => (What::File(kind), self.staged_file(&file.path)),
                    None => (What::Folder, staged_under(&self.changes, &file.path)),
                };
                rows.push(Row {
                    staged,
                    ..row(what, file.path, &file.name, file.depth)
                });
            }
        }
        if !self.stashes.is_empty() {
            rows.push(row(What::Stashes, PathBuf::new(), "Stashes", 0));
        }
        for (i, stash) in self.stashes.iter().enumerate() {
            if self.stashes_collapsed {
                break;
            }
            let hash = &stash.commit.hash;
            let of_stash = |row: Row| Row {
                stash: Some(hash.clone()),
                ..row
            };
            rows.push(of_stash(row(
                What::Stash(i),
                PathBuf::new(),
                &stash.commit.subject,
                1,
            )));
            if !self.open_stashes.contains(hash) {
                continue;
            }
            rows.push(of_stash(row(What::StashActions(i), PathBuf::new(), "", 2)));
            let files = self.stash_files.get(hash).map_or(&[][..], Vec::as_slice);
            let changes: Vec<Change> = files.iter().map(|(_, change)| change.clone()).collect();
            let collapsed = |path: &Path| {
                self.stash_collapsed
                    .contains(&(hash.clone(), path.to_path_buf()))
            };
            for file in file_rows(&changes, &self.root, 2, &collapsed) {
                let what = match file.kind {
                    None => What::StashFolder(i),
                    Some(_) => {
                        let Some((commit, change)) =
                            files.iter().find(|(_, change)| change.path == file.path)
                        else {
                            continue;
                        };
                        What::StashFile(i, commit.clone(), change.clone())
                    }
                };
                rows.push(of_stash(row(what, file.path, &file.name, file.depth)));
            }
        }
        self.rows = rows;
        if let Some(index) =
            selected.and_then(|selected| self.rows.iter().position(|row| row.same(&selected)))
        {
            self.selected = index;
        }
        self.select(self.selected);
    }

    fn change(&self, path: &Path) -> Option<&Change> {
        let index = self
            .changes
            .binary_search_by(|change| change.path.as_path().cmp(path))
            .ok()?;
        Some(&self.changes[index])
    }

    fn staged_file(&self, path: &Path) -> Staged {
        self.change(path).map_or(Staged::No, |change| change.staged)
    }

    /// `s` or a click on row `index`'s mark: stages the files it's of, or
    /// if they all are, unstages them.
    fn toggle_staged(&self, index: usize) -> CommitAction {
        let Some(row) = self.rows.get(index) else {
            return CommitAction::None;
        };
        let changes: Vec<&Change> = match row.what {
            What::File(_) => self.change(&row.path).into_iter().collect(),
            What::All | What::Folder => changes_under(&self.changes, &row.path).collect(),
            _ => Vec::new(),
        };
        if changes.is_empty() || self.committing() {
            return CommitAction::None;
        }
        let stage = row.staged != Staged::Yes;
        let paths = match stage {
            true => changes
                .iter()
                .filter(|change| change.staged != Staged::Yes)
                .map(|change| change.path.clone())
                .collect(),
            // A rename is unstaged from both of its paths.
            false => changes
                .iter()
                .flat_map(|change| [Some(&change.path), change.from.as_ref()])
                .flatten()
                .cloned()
                .collect(),
        };
        CommitAction::Stage { paths, stage }
    }

    // --- navigation -----------------------------------------------------------

    /// Selects row `index` (clamped), or if that's the note that nothing
    /// changed, the row next to it, and scrolls it into view.
    fn select(&mut self, index: usize) {
        let mut index = index.min(self.rows.len().saturating_sub(1));
        if self
            .rows
            .get(index)
            .is_some_and(|row| row.what == What::Note)
        {
            index = match index + 1 < self.rows.len() {
                true => index + 1,
                false => index - 1,
            };
        }
        self.selected = index;
        self.scroll_into_view();
    }

    /// Moves the selection `by` rows, over the note that nothing changed.
    fn step(&mut self, by: isize) {
        let mut index = self.selected.saturating_add_signed(by);
        if self
            .rows
            .get(index)
            .is_some_and(|row| row.what == What::Note)
        {
            index = index.saturating_add_signed(by.signum());
        }
        if index < self.rows.len() {
            self.select(index);
        }
    }

    fn scroll_into_view(&mut self) {
        let height = self.list_height();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(height));
    }

    /// Enter/Shift+Space/click: opens the selected file, or a stash's, or
    /// opens or closes what opens.
    fn activate(&mut self, focus: bool, preview: bool) -> CommitAction {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return CommitAction::None;
        };
        match row.what {
            What::File(_) => CommitAction::Open {
                path: row.path,
                focus,
                preview,
            },
            What::StashFile(_, commit, change) => CommitAction::OpenStashed {
                commit,
                change,
                focus,
            },
            _ => {
                self.toggle(&row);
                CommitAction::None
            }
        }
    }

    /// Right: opens what's selected, if it's closed, or steps into it.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        match self.is_open(&row) {
            Some(false) => self.toggle(&row),
            Some(true)
                if self
                    .rows
                    .get(self.selected + 1)
                    .is_some_and(|next| next.depth > row.depth) =>
            {
                self.select(self.selected + 1)
            }
            _ => {}
        }
    }

    /// Left: closes what's selected, if it's open, or steps out to what
    /// it's in.
    fn collapse_or_leave(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if self.is_open(&row) == Some(true) {
            self.toggle(&row);
            return;
        }
        if let Some(parent) = self.rows[..self.selected]
            .iter()
            .rposition(|other| other.depth < row.depth)
        {
            self.select(parent);
        }
    }
}

/// The changes in the folder `dir`, of `changes`, sorted by path, which
/// lists a folder's files together.
fn changes_under<'a>(changes: &'a [Change], dir: &'a Path) -> impl Iterator<Item = &'a Change> {
    let start = changes.partition_point(|change| change.path.as_path() < dir);
    changes[start..]
        .iter()
        .take_while(move |change| change.path.starts_with(dir))
}

/// How much of the changes in the folder `dir` are staged: all if all of
/// each is, none if none of any is, and otherwise some.
fn staged_under(changes: &[Change], dir: &Path) -> Staged {
    let mut under = changes_under(changes, dir).map(|change| change.staged);
    let Some(first) = under.next() else {
        return Staged::No;
    };
    match under.all(|staged| staged == first) {
        true => first,
        false => Staged::Partly,
    }
}

/// `text` in rows of `room` columns: broken at line breaks, and where a
/// line's too long, after the last space that fits, or if there's none,
/// after the last character that does.
fn wrap(text: &str, room: usize) -> Vec<Line> {
    let room = room.max(1);
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        let end = start + line.len();
        let mut at = start;
        loop {
            let rest = &text[at..end];
            let Some((limit, next)) = rest.char_indices().nth(room) else {
                lines.push(Line {
                    range: at..end,
                    wraps: false,
                });
                break;
            };
            // A space just past the end can stay on the row, unseen.
            let window = &rest[..limit + next.len_utf8()];
            let cut = match window.rfind(' ') {
                Some(space) if space > 0 => space + 1,
                _ => limit,
            };
            lines.push(Line {
                range: at..at + cut,
                wraps: true,
            });
            at += cut;
        }
        start = end + 1;
    }
    lines
}

/// The row of `lines` the cursor at byte `at` is on. At the end of a row
/// that wraps, it's at the start of the next.
fn row_of(lines: &[Line], at: usize) -> usize {
    lines
        .iter()
        .rposition(|line| line.range.start <= at)
        .unwrap_or(0)
}

/// Where in `text` column `column` of `line` is, or the end of the row,
/// if it's shorter: on a row that wraps, before its last character, which
/// is where the next row starts.
fn offset_at(text: &str, line: &Line, column: usize) -> usize {
    let mut end = line.range.end;
    if line.wraps {
        end = text[..end].char_indices().next_back().map_or(0, |(i, _)| i);
    }
    let row = &text[line.range.start..end];
    line.range.start + row.char_indices().nth(column).map_or(row.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Head;
    use opentui::Rgba;

    fn repo(changes: &[(&str, Kind, Staged)]) -> Repo {
        let root = PathBuf::from("/w/cue");
        Repo {
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: Some("0123456".into()),
            merging: false,
            changes: changes
                .iter()
                .map(|&(path, kind, staged)| Change {
                    path: root.join(path),
                    kind,
                    from: None,
                    staged,
                })
                .collect(),
            added: 0,
            removed: 0,
            stashes: Vec::new(),
            root,
        }
    }

    fn view(changes: &[(&str, Kind, Staged)]) -> CommitView {
        let mut view = CommitView::open(&repo(changes), "cue".into());
        view.set_size(24, 12);
        view
    }

    /// The rows of the list, indented by depth, folders with `/`, and each
    /// with how much is staged.
    fn listing(view: &CommitView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let mark = match row.staged {
                    Staged::Yes => '●',
                    Staged::Partly => '◐',
                    Staged::No => '○',
                };
                let slash = if row.what == What::Folder { "/" } else { "" };
                format!("{}{}{slash} {mark}", "  ".repeat(row.depth), row.name)
            })
            .collect()
    }

    fn drawn(view: &CommitView) -> Vec<String> {
        let screen = opentui::OwnedBuffer::new(
            view.width,
            view.height,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, true);
        screen
            .to_text(true)
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    /// `text` as [`wrap`] has it in `room` columns, a row a line, with `\`
    /// at the end of those that wrap.
    fn wrapped(text: &str, room: usize) -> Vec<String> {
        wrap(text, room)
            .into_iter()
            .map(|line| {
                let wraps = if line.wraps { "\\" } else { "" };
                format!("{}{wraps}", &text[line.range])
            })
            .collect()
    }

    #[test]
    fn refreshes_a_stash_replaced_without_changing_the_count() {
        let root = git::tests::repo("stash-replaced");
        git::tests::commit_as_cue(&root);
        std::fs::write(root.join("a.txt"), "old\n").unwrap();
        git::stash(&root, "old stash", false).unwrap();
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        let before = git.repos()[0].clone();
        let mut view = CommitView::open(&before, "cue".into());
        git::drop_stash(&root, &view.stashes[0].commit.hash).unwrap();
        std::fs::write(root.join("a.txt"), "new\n").unwrap();
        git::stash(&root, "new stash", false).unwrap();
        git.restart();
        git.wait();
        let after = &git.repos()[0];
        assert_ne!(&before, after, "stash replacement changes the snapshot");
        view.set_repo(after);
        assert_eq!(view.stashes.len(), 1);
        assert_eq!(view.stashes[0].commit.subject, "new stash");
    }

    #[test]
    fn blocks_stash_application_until_commit_finishes() {
        let root = git::tests::repo("stash-during-commit");
        git::tests::commit_as_cue(&root);
        std::fs::write(root.join("new.txt"), "stashed addition\n").unwrap();
        git::stage(&root, &[root.join("new.txt")]).unwrap();
        git::stash(&root, "new file", true).unwrap();
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        let mut view = CommitView::open(&git.repos()[0], "cue".into());
        view.set_size(40, 24);
        let stash = view
            .rows
            .iter()
            .position(|row| matches!(row.what, What::Stash(_)))
            .unwrap();
        view.select(stash);
        view.run(Command::TreeExpand);
        let actions = view
            .rows
            .iter()
            .position(|row| matches!(row.what, What::StashActions(_)))
            .unwrap();
        let y = (view.list_top() + actions - view.scroll) as u32;
        let buttons = view.stash_buttons(view.rows[actions].depth);
        let (sender, receiver) = mpsc::channel();
        view.committing = Some(receiver);
        for command in [Command::TreeApplyStash, Command::TreePopStash] {
            assert_eq!(view.run(command), CommitAction::None);
        }
        for (area, button) in &buttons {
            if matches!(button, StashButton::Apply | StashButton::Pop) {
                assert_eq!(view.click(area.start, y, false), CommitAction::None);
            }
        }
        sender.send(Ok("1234567".into())).unwrap();
        assert!(view.poll().unwrap().is_ok());
        for (command, pop) in [(Command::TreeApplyStash, false), (Command::TreePopStash, true)] {
            assert_eq!(
                view.run(command),
                CommitAction::ApplyStash {
                    hash: view.stashes[0].commit.hash.clone(),
                    pop,
                }
            );
        }
    }

    #[test]
    fn switching_branches_clears_amend_but_keeps_edited_messages() {
        let mut repo = repo(&[]);
        repo.root = git::tests::repo("amend-switch");
        for edited in [false, true] {
            repo.head = Head::Branch("main".into());
            let mut view = CommitView::open(&repo, "cue".into());
            view.toggle_amend();
            assert_eq!(view.message(), "first");
            if edited {
                view.edit(Edit::Insert("My draft"), false);
            }
            let message = view.message().to_string();
            view.set_repo(&repo);
            assert!(view.amend, "ordinary refreshes keep amend enabled");
            repo.head = Head::Branch("other".into());
            view.set_repo(&repo);
            assert!(!view.amend);
            assert_eq!(view.message(), if edited { &message } else { "" });
            assert_eq!(
                view.commit(),
                Err(if edited {
                    CantCommit::NothingStaged
                } else {
                    CantCommit::NoMessage
                })
            );
        }
    }

    #[test]
    fn wraps_after_spaces_or_else_anywhere() {
        assert_eq!(wrapped("", 5), [""]);
        assert_eq!(wrapped("fix the bug", 5), ["fix \\", "the \\", "bug"]);
        assert_eq!(wrapped("abcdefgh", 3), ["abc\\", "def\\", "gh"]);
        assert_eq!(
            wrapped("abc def", 3),
            ["abc \\", "def"],
            "a space past the end stays"
        );
        assert_eq!(wrapped("one\n\ntwo\n", 5), ["one", "", "two", ""]);
    }

    #[test]
    fn the_cursor_moves_by_rows_as_they_wrap() {
        let mut view = view(&[]);
        view.set_size(11, 12);
        view.edit(Edit::Insert("fix the bug\nin it"), false);
        let cursor = |view: &CommitView| {
            let text = view.message();
            let at = view.message.caret.at(text);
            format!("{}|{}", &text[..at], &text[at..])
        };
        assert_eq!(cursor(&view), "fix the bug\nin it|");
        view.move_rows(-1, false);
        assert_eq!(
            cursor(&view),
            "fix the bug|\nin it",
            "to the end of a shorter row"
        );
        view.move_rows(-1, false);
        assert_eq!(
            cursor(&view),
            "fix the| bug\nin it",
            "rows that wrap end before the space"
        );
        view.move_to_row_edge(false, false);
        assert_eq!(cursor(&view), "fix |the bug\nin it");
        view.move_to_row_edge(true, true);
        assert_eq!(view.selected_text(), Some("the"));
        view.move_rows(-1, false);
        assert_eq!(cursor(&view), "fix| the bug\nin it");
        view.move_rows(-1, false);
        assert_eq!(
            cursor(&view),
            "|fix the bug\nin it",
            "up from the top goes to the start"
        );
        view.move_rows(5, false);
        assert_eq!(cursor(&view), "fix the bug\nin it|");
        view.edit(Edit::Insert("\r\nmore"), false);
        assert_eq!(
            view.message(),
            "fix the bug\nin it\nmore",
            "pasted CRs are line breaks"
        );
    }

    #[test]
    fn marks_how_much_is_staged_in_each_folder() {
        let view = view(&[
            ("a.rs", Kind::Modified, Staged::Partly),
            ("src/b.rs", Kind::Added, Staged::Yes),
            ("src/c.rs", Kind::Modified, Staged::Yes),
            ("z/d.rs", Kind::Untracked, Staged::No),
        ]);
        assert_eq!(
            listing(&view),
            [
                "Changes ◐",
                "  src/ ●",
                "    b.rs ●",
                "    c.rs ●",
                "  z/ ○",
                "    d.rs ○",
                "  a.rs ◐",
            ]
        );
        assert_eq!(view.staged_count(), 3);
    }

    #[test]
    fn stages_what_isnt_and_unstages_what_all_is() {
        let mut changes = repo(&[
            ("src/b.rs", Kind::Renamed, Staged::Yes),
            ("src/c.rs", Kind::Modified, Staged::Yes),
            ("z/d.rs", Kind::Modified, Staged::Partly),
            ("z/e.rs", Kind::Untracked, Staged::No),
        ]);
        changes.changes[0].from = Some(PathBuf::from("/w/cue/old.rs"));
        let mut view = CommitView::open(&changes, "cue".into());
        view.set_size(24, 12);
        let paths = |names: &[&str]| -> Vec<PathBuf> {
            names
                .iter()
                .map(|name| Path::new("/w/cue").join(name))
                .collect()
        };
        view.select(1);
        assert_eq!(
            view.run(Command::TreeToggleStaged),
            CommitAction::Stage {
                paths: paths(&["src/b.rs", "old.rs", "src/c.rs"]),
                stage: false,
            },
            "a rename unstages from both its paths"
        );
        view.select(4);
        assert_eq!(
            view.run(Command::TreeToggleStaged),
            CommitAction::Stage {
                paths: paths(&["z/d.rs", "z/e.rs"]),
                stage: true,
            }
        );
        view.set_staged(&paths(&["z/d.rs", "z/e.rs"]), true);
        assert_eq!(listing(&view)[0], "Changes ●");
    }

    #[test]
    fn clicks_go_back_type_amend_commit_and_stage() {
        let mut view = view(&[("a.rs", Kind::Modified, Staged::No)]);
        assert_eq!(view.click(2, 0, false), CommitAction::Back);
        assert_eq!(view.click(21, 0, false), CommitAction::Log);
        assert!(!view.editing());
        assert_eq!(view.click(5, 2, false), CommitAction::None);
        assert!(view.editing(), "the message box has the keyboard");
        // Rows: top, border, message, border, actions, then the list.
        assert_eq!(view.click(20, 4, false), CommitAction::Commit);
        assert_eq!(view.click(10, 4, false), CommitAction::None);
        let file = Path::new("/w/cue/a.rs").to_path_buf();
        assert_eq!(
            view.click(5, 6, false),
            CommitAction::Open {
                path: file.clone(),
                focus: false,
                preview: true,
            }
        );
        assert!(!view.editing(), "the list has it");
        assert_eq!(
            view.click(22, 6, false),
            CommitAction::Stage {
                paths: vec![file],
                stage: true,
            }
        );
    }

    #[test]
    fn commits_a_merge_resolved_to_the_current_tree() {
        let root = git::tests::repo("commit-merge-ours");
        git::tests::commit_as_cue(&root);
        git::switch(&root, &git::SwitchTo::New("topic".into())).unwrap();
        std::fs::write(root.join("a.txt"), "topic\n").unwrap();
        git::tests::commit_all(&root, "topic");
        git::switch(&root, &git::SwitchTo::Branch("main".into())).unwrap();
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        git::tests::commit_all(&root, "ours");
        let merge = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["merge", "topic"])
            .output()
            .unwrap();
        assert!(!merge.status.success());
        assert!(root.join(".git/MERGE_HEAD").is_file());
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        git::stage(&root, &[root.join("a.txt")]).unwrap();
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        assert!(git.repos()[0].changes.is_empty());
        let mut view = CommitView::open(&git.repos()[0], "cue".into());
        view.edit(Edit::Insert("Resolve using ours"), false);
        assert_eq!(view.commit(), Ok(()));
        view.wait().unwrap().unwrap();
        assert!(!root.join(".git/MERGE_HEAD").exists());
        let parents = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-list", "--parents", "-1", "HEAD"])
            .output()
            .unwrap();
        assert!(parents.status.success());
        assert_eq!(
            String::from_utf8_lossy(&parents.stdout).split_whitespace().count(),
            3,
            "the commit has two parents"
        );
        git.restart();
        git.wait();
        view.set_repo(&git.repos()[0]);
        view.edit(Edit::Insert("No further changes"), false);
        assert_eq!(view.commit(), Err(CantCommit::NothingStaged));
    }

    #[test]
    fn says_what_can_be_committed() {
        let _serial = crate::test_serial();
        let mut view = view(&[
            ("a.rs", Kind::Modified, Staged::Yes),
            ("b.rs", Kind::Modified, Staged::No),
        ]);
        assert_eq!(
            drawn(&view)[..8],
            [
                " ‹ cue  main        Log",
                " ╭────────────────────╮",
                " │ Message            │",
                " ╰────────────────────╯",
                " ○ Amend       Commit 1",
                " ▾ Changes            ◐",
                "     a.rs           M ●",
                "     b.rs           M ○",
            ]
        );
        assert_eq!(view.commit(), Err(CantCommit::NoMessage));
        view.edit(Edit::Insert("Fix it\n\nAll of it, really"), false);
        assert_eq!(
            drawn(&view)[1..6],
            [
                " ╭────────────────────╮",
                " │ Fix it             │",
                " │                    │",
                " │ All of it, really  │",
                " ╰────────────────────╯",
            ],
            "the box grows"
        );
        view.set_staged(&[PathBuf::from("/w/cue/a.rs")], false);
        assert_eq!(view.commit(), Err(CantCommit::NothingStaged));
    }
}
