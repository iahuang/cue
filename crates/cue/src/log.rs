//! The log: the commits that led to what a repository has checked out,
//! newest first, shown in the sidebar in place of the changes.
//!
//! A row across the top names the repository and goes back to the
//! changes. Each commit shows its subject and how long ago it was made,
//! and opens, as a folder does, to its hash and author and the files it
//! changed, in their folders as the changes view has them. A file opens
//! to how the commit changed it. Commits are read a page at a time, the
//! next once the list gets near the end of those read.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use opentui::{Attributes, Buffer};

use crate::changes::{button_area, draw_look, file_rows, Look};
use crate::git::{self, Change, Commit, Kind, Repo};
use crate::icons;
use crate::keymap::Command;
use crate::theme;
use crate::tree::{truncate, Entry};

/// Commits read at a time.
#[cfg(not(test))]
const PAGE: usize = 200;
#[cfg(test)]
const PAGE: usize = 2;
/// The next page is read once the selection, or the bottom of the view,
/// is this close to the end of the rows.
const NEAR_END: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// The commit at this index.
    Commit(usize),
    /// Its hash and author.
    Info(usize),
    Folder(usize),
    File(usize, Change),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    what: What,
    name: String,
    depth: usize,
    /// A folder's or file's path.
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogAction {
    None,
    /// Go back to the changes.
    Back,
    /// Show the repository's branches, to switch to one.
    Branches,
    /// Show how `commit` changed a file, and move focus there if `focus`.
    Open {
        commit: Commit,
        change: Change,
        focus: bool,
    },
    /// Show the file at this path as it is now.
    OpenFile(PathBuf),
    /// Put `text` on the clipboard, and say so as `said`.
    Copy {
        text: String,
        said: String,
    },
    /// Check this commit out, on no branch.
    CheckOut(Commit),
    /// Ask for a name for a new branch at this commit.
    NewBranch(Commit),
    /// Ask whether to make a commit that undoes this one.
    Revert(Commit),
}

pub struct LogView {
    root: PathBuf,
    /// What the repository is called.
    name: String,
    branch: String,
    /// The commit checked out when the log was read: once it's another,
    /// the log is read again.
    head: Option<String>,
    commits: Vec<Commit>,
    /// Every commit has been read.
    complete: bool,
    /// The files each commit opened changed, by hash, read when it's first
    /// opened.
    files: HashMap<String, Vec<Change>>,
    /// The commits opened, by hash.
    open: HashSet<String>,
    /// The folders collapsed, by commit hash and path.
    collapsed: HashSet<(String, PathBuf)>,
    rows: Vec<Row>,
    selected: usize,
    /// The first row on screen.
    scroll: usize,
    /// Rows on screen, below the top row.
    height: usize,
    /// The commit and file shown in the editor, highlighted.
    active: Option<(String, PathBuf)>,
}

impl LogView {
    /// The log of `repo`, which is called `name`.
    pub fn open(repo: &Repo, name: String) -> LogView {
        let mut view = LogView {
            root: repo.root.clone(),
            name,
            branch: repo.head.name().to_string(),
            head: repo.commit.clone(),
            commits: Vec::new(),
            complete: false,
            files: HashMap::new(),
            open: HashSet::new(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
        };
        view.read_more();
        view
    }

    /// The repository's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Catches up with what git says of the repository now: once another
    /// commit is checked out, the log is read again, keeping the commits
    /// that are still in it open.
    pub fn set_repo(&mut self, repo: &Repo) {
        self.branch = repo.head.name().to_string();
        if repo.commit == self.head {
            return;
        }
        let selected = self.rows.get(self.selected).cloned();
        let selected_hash = selected.as_ref().and_then(|row| self.hash_of(row));
        self.head = repo.commit.clone();
        self.commits.clear();
        self.complete = false;
        self.read_more();
        let index = selected_hash.and_then(|hash| {
            let row = selected?;
            self.rows.iter().position(|candidate| {
                self.hash_of(candidate).as_deref() == Some(hash.as_str())
                    && std::mem::discriminant(&candidate.what) == std::mem::discriminant(&row.what)
                    && candidate.path == row.path
            })
        });
        self.select(index.unwrap_or(0));
    }

    pub fn set_height(&mut self, height: u32) {
        self.height = (height as usize).saturating_sub(1).max(1);
        self.scroll_into_view();
    }

    /// Highlights the file `commit` changed at `path` as the one in the
    /// editor.
    pub fn set_active(&mut self, active: Option<(&str, &Path)>) {
        self.active = active.map(|(hash, path)| (hash.to_string(), path.to_path_buf()));
    }

    pub fn run(&mut self, command: Command) -> LogAction {
        let page = self.height.saturating_sub(1).max(1);
        let mut action = LogAction::None;
        match command {
            Command::TreeUp => self.select(self.selected.saturating_sub(1)),
            Command::TreeDown => self.select(self.selected + 1),
            Command::TreePageUp => self.select(self.selected.saturating_sub(page)),
            Command::TreePageDown => self.select(self.selected + page),
            Command::TreeFirst => self.select(0),
            Command::TreeLast => self.select(usize::MAX),
            Command::TreeExpand => self.expand_or_enter(),
            Command::TreeCollapse => self.collapse_or_leave(),
            Command::TreeOpen => action = self.activate(true),
            Command::TreePreview => action = self.activate(false),
            Command::TreeOpenFile => {
                if let Some(Entry {
                    path,
                    is_dir: false,
                    ..
                }) = self.selected()
                {
                    action = LogAction::OpenFile(path);
                }
            }
            Command::TreeCopyHash
            | Command::TreeCopyMessage
            | Command::TreeCheckOut
            | Command::TreeNewBranch
            | Command::TreeRevert => {
                if let Some(commit) = self.selected_commit().cloned() {
                    action = self.commit_command(command, commit);
                }
            }
            _ => {}
        }
        self.read_more_near_end();
        action
    }

    /// A left click at column `x` of screen row `y`, in a list `width`
    /// wide: on the top row, the branch shows the branches, and elsewhere,
    /// goes back to the changes; another selects the entry there, then opens or closes a
    /// commit or folder, or shows a file: a `double` click moves focus to
    /// it.
    pub fn click(&mut self, x: u32, y: u32, width: u32, double: bool) -> LogAction {
        if y == 0 {
            return match self.branch_area(width).contains(&x) {
                true => LogAction::Branches,
                false => LogAction::Back,
            };
        }
        let index = self.scroll + y as usize - 1;
        if index >= self.rows.len() {
            return LogAction::None;
        }
        self.select(index);
        let action = self.activate(double);
        self.read_more_near_end();
        action
    }

    /// The file or folder selected, if it's one a commit changed, rather
    /// than a commit.
    pub fn selected(&self) -> Option<Entry> {
        let row = self.rows.get(self.selected)?;
        let is_dir = match row.what {
            What::Folder(_) => true,
            What::File(..) => false,
            What::Commit(_) | What::Info(_) => return None,
        };
        Some(Entry {
            path: row.path.clone(),
            is_dir,
            is_root: false,
        })
    }

    /// The commit selected, or the one the file or folder selected is in.
    pub fn selected_commit(&self) -> Option<&Commit> {
        let (What::Commit(i) | What::Info(i) | What::Folder(i) | What::File(i, _)) =
            self.rows.get(self.selected)?.what;
        self.commits.get(i)
    }

    /// Where the selection's name starts on screen, if it's in view, for a
    /// menu to open next to.
    pub fn selected_position(&self) -> Option<(u32, u32)> {
        let row = self.rows.get(self.selected)?;
        let y = self.selected.checked_sub(self.scroll)?;
        let icon = match row.what {
            What::Folder(_) | What::File(..) => icons::width(),
            What::Commit(_) | What::Info(_) => 0,
        };
        (y < self.height).then_some((3 + 2 * row.depth as u32 + icon, y as u32 + 1))
    }

    /// Selects the row on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        let Some(index) = (y as usize).checked_sub(1).map(|row| self.scroll + row) else {
            return false;
        };
        if index >= self.rows.len() {
            return false;
        }
        self.select(index);
        true
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
        self.read_more_near_end();
    }

    /// The columns of what's under the pointer on screen row `y`, that a
    /// click does something to, in a list `width` wide: the top row, or a
    /// commit, folder, or file. A commit's hash and author are only shown.
    pub fn hover(&self, x: u32, y: u32, width: u32) -> Option<Range<u32>> {
        if y == 0 {
            let branch = self.branch_area(width);
            return match branch.contains(&x) {
                true => Some(branch),
                false => Some(0..branch.start),
            };
        }
        match self.rows.get(self.scroll + y as usize - 1)?.what {
            What::Info(_) => None,
            _ => Some(0..width),
        }
    }

    /// Draws the log in the columns from `x` to `x + width`.
    pub fn draw(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        frame.with_clip(x, 0, width, self.height as u32 + 1, || {
            self.draw_top(frame, x, width);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64);
            let visible = self.scroll..self.rows.len();
            for (index, y) in visible.zip(1..=self.height as u32) {
                self.draw_row(frame, index, (x, y, width), focused, now);
            }
            if self.rows.is_empty() {
                let colors = theme::colors();
                let note = truncate("No commits yet.", width.saturating_sub(2) as usize);
                frame.draw_text(&note, x + 1, 1, colors.faint, None, Attributes::NONE);
            }
        });
    }

    /// The top row: back to the changes, the repository, and its branch.
    fn draw_top(&self, frame: &Buffer, x: u32, width: u32) {
        let colors = theme::colors();
        frame.draw_text("‹", x + 1, 0, colors.muted, None, Attributes::NONE);
        let (branch, branch_x) = self.top_branch(width);
        let branch_x = x + branch_x;
        let name_x = x + 3;
        let room = branch_x.saturating_sub(name_x + 1) as usize;
        let name = truncate(&self.name, room);
        frame.draw_text(&name, name_x, 0, colors.text, None, Attributes::BOLD);
        if branch_x > name_x + name.chars().count() as u32 {
            let mut at = branch_x;
            if icons::enabled() {
                at = icons::branch().draw(frame, at, 0, None);
            }
            frame.draw_text(&branch, at, 0, colors.muted, None, Attributes::NONE);
        }
    }

    /// The branch as the top row shows it, at its right end, in a list
    /// `width` wide, and the column it starts at, its icon's if it has one.
    fn top_branch(&self, width: u32) -> (String, u32) {
        let branch_room = (width / 2).saturating_sub(2) as usize;
        let branch = truncate(&self.branch, branch_room);
        let branch_width = branch.chars().count() as u32 + icons::width();
        let branch_x = width.saturating_sub(branch_width + 1);
        (branch, branch_x)
    }

    /// The top row's button that shows the branches: the branch.
    fn branch_area(&self, width: u32) -> Range<u32> {
        let (branch, start) = self.top_branch(width);
        button_area(start..start + icons::width() + branch.chars().count() as u32)
    }

    fn draw_row(
        &self,
        frame: &Buffer,
        index: usize,
        (x, y, width): (u32, u32, u32),
        focused: bool,
        now: i64,
    ) {
        let colors = theme::colors();
        let row = &self.rows[index];
        let selected = (index == self.selected).then_some(focused);
        let right;
        let look = match &row.what {
            What::Commit(i) => {
                let commit = &self.commits[*i];
                right = ago(now - commit.time);
                Look {
                    depth: 0,
                    flush: false,
                    open: Some(self.open.contains(&commit.hash)),
                    icon: None,
                    name: &row.name,
                    fg: colors.text,
                    attributes: Attributes::NONE,
                    right: Some((&right, colors.muted)),
                    mark: None,
                }
            }
            What::Info(_) => Look {
                depth: 1,
                flush: true,
                open: None,
                icon: None,
                name: &row.name,
                fg: colors.muted,
                attributes: Attributes::NONE,
                right: None,
                mark: None,
            },
            What::Folder(i) => {
                let key = (self.commits[*i].hash.clone(), row.path.clone());
                let open = !self.collapsed.contains(&key);
                Look {
                    depth: row.depth,
                    flush: false,
                    open: Some(open),
                    icon: Some(icons::folder(open)),
                    name: &row.name,
                    fg: colors.text,
                    attributes: Attributes::NONE,
                    right: None,
                    mark: None,
                }
            }
            What::File(i, change) => {
                let hash = &self.commits[*i].hash;
                let active = self
                    .active
                    .as_ref()
                    .is_some_and(|(active, path)| active == hash && *path == change.path);
                let (fg, mut attributes) = match active {
                    true => (colors.accent, Attributes::BOLD),
                    false => (colors.hue(change.kind.hue()), Attributes::NONE),
                };
                if change.kind == Kind::Deleted {
                    attributes |= Attributes::STRIKETHROUGH;
                }
                right = change.kind.letter().to_string();
                Look {
                    depth: row.depth,
                    flush: false,
                    open: None,
                    icon: Some(icons::file(&row.name)),
                    name: &row.name,
                    fg,
                    attributes,
                    right: Some((&right, colors.hue(change.kind.hue()))),
                    mark: None,
                }
            }
        };
        draw_look(frame, &look, (x, y, width), selected);
    }

    // --- rows ------------------------------------------------------------------

    /// Reads the next page of commits, if there's more.
    fn read_more(&mut self) {
        if self.complete {
            return;
        }
        let page = git::log(&self.root, self.commits.len(), PAGE);
        self.complete = page.len() < PAGE;
        self.commits.extend(page);
        self.rebuild();
    }

    /// Reads the next page once the selection or the view is near the end
    /// of the rows.
    fn read_more_near_end(&mut self) {
        let reached = self.selected.max(self.scroll + self.height);
        if !self.complete && reached + NEAR_END >= self.rows.len() {
            self.read_more();
        }
    }

    /// The hash of the commit `row` is of.
    fn hash_of(&self, row: &Row) -> Option<String> {
        let (What::Commit(i) | What::Info(i) | What::Folder(i) | What::File(i, _)) = row.what;
        self.commits.get(i).map(|commit| commit.hash.clone())
    }

    /// Lists the rows again, from the commits and what's open.
    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        for (i, commit) in self.commits.iter().enumerate() {
            rows.push(Row {
                what: What::Commit(i),
                name: commit.subject.clone(),
                depth: 0,
                path: PathBuf::new(),
            });
            if !self.open.contains(&commit.hash) {
                continue;
            }
            rows.push(Row {
                what: What::Info(i),
                name: format!("{} · {}", commit.short, commit.author),
                depth: 1,
                path: PathBuf::new(),
            });
            let changes = self.files.get(&commit.hash).map_or(&[][..], Vec::as_slice);
            let collapsed = |path: &Path| {
                self.collapsed
                    .contains(&(commit.hash.clone(), path.to_path_buf()))
            };
            for file in file_rows(changes, &self.root, 1, &collapsed) {
                let what = match file.kind {
                    None => What::Folder(i),
                    Some(_) => {
                        let Some(change) = changes.iter().find(|c| c.path == file.path) else {
                            continue;
                        };
                        What::File(i, change.clone())
                    }
                };
                rows.push(Row {
                    what,
                    name: file.name,
                    depth: file.depth,
                    path: file.path,
                });
            }
        }
        self.rows = rows;
        self.select(self.selected);
    }

    // --- navigation -------------------------------------------------------------

    /// Selects row `index` (clamped) and scrolls it into view.
    fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        self.scroll_into_view();
    }

    fn scroll_into_view(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + self.height {
            self.scroll = self.selected + 1 - self.height;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(self.height));
    }

    /// Enter/Space/click: shows the selected file, or opens or closes a
    /// commit or folder.
    fn activate(&mut self, focus: bool) -> LogAction {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return LogAction::None;
        };
        match row.what {
            What::File(i, change) => LogAction::Open {
                commit: self.commits[i].clone(),
                change,
                focus,
            },
            What::Commit(i) => {
                let hash = self.commits[i].hash.clone();
                if !self.open.remove(&hash) {
                    self.open_commit(i);
                }
                self.rebuild();
                LogAction::None
            }
            What::Folder(i) => {
                let key = (self.commits[i].hash.clone(), row.path);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.rebuild();
                LogAction::None
            }
            What::Info(_) => LogAction::None,
        }
    }

    /// What `command`, from the log's menu, does to `commit`.
    fn commit_command(&self, command: Command, commit: Commit) -> LogAction {
        match command {
            Command::TreeCopyHash => LogAction::Copy {
                said: format!("Copied {}.", commit.hash),
                text: commit.hash,
            },
            Command::TreeCopyMessage => {
                let text = git::message(&self.root, &commit.hash).unwrap_or(commit.subject);
                LogAction::Copy {
                    said: format!("Copied the message of {}.", commit.short),
                    text,
                }
            }
            Command::TreeCheckOut => LogAction::CheckOut(commit),
            Command::TreeNewBranch => LogAction::NewBranch(commit),
            Command::TreeRevert => LogAction::Revert(commit),
            _ => LogAction::None,
        }
    }

    /// Opens commit `i`, reading the files it changed the first time.
    fn open_commit(&mut self, i: usize) {
        let commit = &self.commits[i];
        if !self.files.contains_key(&commit.hash) {
            let changes = git::commit_changes(&self.root, commit);
            self.files.insert(commit.hash.clone(), changes);
        }
        self.open.insert(commit.hash.clone());
    }

    /// Right: opens a closed commit or folder, or steps into an open one.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let closed = match &row.what {
            What::Commit(i) => !self.open.contains(&self.commits[*i].hash),
            What::Folder(i) => self
                .collapsed
                .contains(&(self.commits[*i].hash.clone(), row.path.clone())),
            _ => return,
        };
        if closed {
            self.activate(false);
        } else if self
            .rows
            .get(self.selected + 1)
            .is_some_and(|next| next.depth > row.depth)
        {
            self.select(self.selected + 1);
        }
    }

    /// Left: closes an open commit or folder, or steps out to the commit
    /// or folder it's in.
    fn collapse_or_leave(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let open = match &row.what {
            What::Commit(i) => self.open.contains(&self.commits[*i].hash),
            What::Folder(i) => !self
                .collapsed
                .contains(&(self.commits[*i].hash.clone(), row.path.clone())),
            _ => false,
        };
        if open {
            self.activate(false);
            return;
        }
        let depth = row.depth;
        if let Some(parent) = self.rows[..self.selected]
            .iter()
            .rposition(|row| row.depth < depth)
        {
            self.select(parent);
        }
    }
}

/// How long `seconds` is, briefly, as the log says how long ago a commit
/// was made.
pub(crate) fn ago(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    match seconds.max(0) {
        s if s < MINUTE => "now".to_string(),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 7 * DAY => format!("{}d", s / DAY),
        s if s < 30 * DAY => format!("{}w", s / (7 * DAY)),
        s if s < 365 * DAY => format!("{}mo", s / (30 * DAY)),
        s => format!("{}y", s / (365 * DAY)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Head;
    use opentui::Rgba;

    fn repo(root: &Path) -> Repo {
        Repo {
            root: root.to_path_buf(),
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: git::log(root, 0, 1)
                .first()
                .map(|commit| commit.hash.clone()),
            changes: Vec::new(),
            added: 0,
            removed: 0,
            stashes: Vec::new(),
            upstream: None,
            publish_to: None,
            merging: false,
        }
    }

    /// The rows, indented by depth, commits marked with `>` or `v`.
    fn listing(view: &LogView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let indent = "  ".repeat(row.depth);
                match &row.what {
                    What::Commit(i) => {
                        let open = view.open.contains(&view.commits[*i].hash);
                        format!("{} {}", if open { 'v' } else { '>' }, row.name)
                    }
                    What::Info(_) => format!("{indent}(info)"),
                    What::Folder(_) => format!("{indent}{}/", row.name),
                    What::File(_, change) => {
                        format!("{indent}{} {}", row.name, change.kind.letter())
                    }
                }
            })
            .collect()
    }

    #[test]
    fn commits_open_to_the_files_they_changed() {
        let root = crate::git::tests::history_repo("log-view");
        let mut view = LogView::open(&repo(&root), "log-view".into());
        view.set_height(20);
        assert_eq!(listing(&view), ["> third", "> second"], "a page at first");
        view.run(Command::TreeDown);
        assert_eq!(
            listing(&view),
            ["> third", "> second", "> first"],
            "then more"
        );
        assert!(view.complete);

        assert_eq!(view.click(2, 2, 20, false), LogAction::None);
        assert_eq!(
            listing(&view),
            [
                "> third",
                "v second",
                "  (info)",
                "  a.txt M",
                "  c.txt R",
                "  d.txt A",
                "> first"
            ]
        );
        assert_eq!(view.hover(2, 0, 20), Some(0..14), "back");
        assert_eq!(view.hover(16, 0, 20), Some(14..20), "the branch");
        assert_eq!(view.click(16, 0, 20, false), LogAction::Branches);
        assert_eq!(view.hover(2, 2, 20), Some(0..20), "a commit");
        assert_eq!(view.hover(2, 3, 20), None, "its hash and author");
        assert_eq!(view.hover(2, 8, 20), None, "below the commits");
        let LogAction::Open {
            commit,
            change,
            focus,
        } = view.click(2, 5, 20, false)
        else {
            panic!("a file opens");
        };
        assert_eq!(
            (commit.subject.as_str(), change.kind, focus),
            ("second", Kind::Renamed, false)
        );
        assert_eq!(change.from, Some(root.join("b.txt")));
        assert!(matches!(
            view.click(2, 5, 20, true),
            LogAction::Open { focus: true, .. }
        ));

        // Left steps out to the commit, then closes it.
        view.run(Command::TreeCollapse);
        assert_eq!(view.selected, 1);
        view.run(Command::TreeCollapse);
        assert_eq!(listing(&view), ["> third", "> second", "> first"]);
        assert_eq!(
            view.click(2, 0, 20, false),
            LogAction::Back,
            "the top row goes back"
        );
    }

    #[test]
    fn a_new_commit_reads_the_log_again() {
        let root = crate::git::tests::repo("log-view-new");
        let mut view = LogView::open(&repo(&root), "log-view-new".into());
        view.set_height(20);
        view.click(2, 1, 20, false);
        assert_eq!(
            listing(&view),
            ["v first", "  (info)", "  a.txt A", "  b.txt A"]
        );
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        crate::git::tests::commit_all(&root, "second");
        view.set_repo(&repo(&root));
        assert_eq!(
            listing(&view),
            ["> second", "v first", "  (info)", "  a.txt A", "  b.txt A"]
        );
        assert_eq!(view.selected, 1, "the commit selected stays so");
    }

    #[test]
    fn draws_the_repository_and_how_long_ago_commits_were() {
        let _serial = crate::test_serial();
        let root = crate::git::tests::repo("log-view-draw");
        let mut view = LogView::open(&repo(&root), "log-view-draw".into());
        view.set_height(3);
        let screen =
            opentui::OwnedBuffer::new(24, 3, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, 24, true);
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(
            lines[..2],
            [" ‹ log-view-draw   main", " ▸ first            now"]
        );
    }

    #[test]
    fn says_how_long_ago_briefly() {
        let day = 24 * 3600;
        let cases = [
            (5, "now"),
            (125, "2m"),
            (7200, "2h"),
            (3 * day, "3d"),
            (15 * day, "2w"),
        ];
        for (seconds, said) in cases {
            assert_eq!(ago(seconds), said);
        }
        assert_eq!(ago(90 * day), "3mo");
        assert_eq!(ago(800 * day), "2y");
        assert_eq!(ago(-5), "now", "a clock behind");
    }
}
