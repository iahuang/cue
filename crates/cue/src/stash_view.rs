//! The stashes: the changes a repository has put away, newest first, shown
//! in the sidebar in place of the changes, as the log is.
//!
//! A row across the top names the repository and goes back to the changes,
//! and its branch shows the branches. Each stash shows what it was stashed
//! as and how long ago, and opens, as a folder does, to buttons that apply,
//! pop, or drop it, and the files it changed, in their folders as the
//! changes view has them. A file opens to how the stash changed it.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use opentui::{Attributes, Buffer};

use crate::changes::{button_area, draw_look, file_rows, Look};
use crate::git::{self, Change, Commit, Kind, Repo, Stash};
use crate::icons;
use crate::keymap::Command;
use crate::log::{ago, draw_top, top_branch_area};
use crate::theme;
use crate::tree::{truncate, Entry, Outline};

/// The buttons on the row under an open stash.
const BUTTONS: [(&str, StashButton); 3] = [
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
pub enum StashAction {
    None,
    /// Go back to the changes.
    Back,
    /// Show the repository's branches, to switch to one.
    Branches,
    /// Put the changes stashed as `hash` back, and if `pop`, drop the
    /// stash.
    Apply {
        hash: String,
        pop: bool,
    },
    /// Drop the stash `hash`, stashed as `subject`, once that's confirmed.
    Drop {
        hash: String,
        subject: String,
    },
    /// Show how a stash, as `commit`, made `change`, and move focus there
    /// if `focus`.
    Open {
        commit: Commit,
        change: Change,
        focus: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// The stash at this index, which opens.
    Stash(usize),
    /// The buttons under an open stash.
    Buttons(usize),
    Folder(usize),
    /// A file the stash changed, and the commit that has it as stashed.
    File(usize, Commit, Change),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    what: What,
    /// A folder's or file's path.
    path: PathBuf,
    name: String,
    depth: usize,
    /// The hash of the stash it's of.
    stash: String,
}

impl Row {
    /// Whether it's the same entry as `other`, laid out again.
    fn same(&self, other: &Row) -> bool {
        (&self.path, &self.stash) == (&other.path, &other.stash)
            && std::mem::discriminant(&self.what) == std::mem::discriminant(&other.what)
    }
}

pub struct StashView {
    root: PathBuf,
    /// What the repository is called.
    name: String,
    branch: String,
    /// Newest first.
    stashes: Vec<Stash>,
    /// The files each stash opened changed, by hash, read when it's first
    /// opened.
    files: HashMap<String, Vec<(Commit, Change)>>,
    /// The stashes opened, by hash.
    open: HashSet<String>,
    /// The folders collapsed, by hash and path.
    collapsed: HashSet<(String, PathBuf)>,
    rows: Vec<Row>,
    selected: usize,
    /// The first row on screen.
    scroll: usize,
    /// Rows on screen, below the top row.
    height: usize,
    /// The stashed file shown in the editor, by its commit's hash and path.
    active: Option<(String, PathBuf)>,
}

impl StashView {
    /// The stashes of `repo`, which is called `name`.
    pub fn open(repo: &Repo, name: String) -> StashView {
        let mut view = StashView {
            root: repo.root.clone(),
            name,
            branch: String::new(),
            stashes: Vec::new(),
            files: HashMap::new(),
            open: HashSet::new(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
        };
        view.set_repo(repo);
        view
    }

    /// The repository's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Catches up with what git says of the repository now, keeping the
    /// stashes still there open.
    pub fn set_repo(&mut self, repo: &Repo) {
        self.branch = repo.head.name().to_string();
        if self.stashes != repo.stashes {
            self.set_stashes(repo.stashes.clone());
        }
    }

    /// Reads the stashes again, as after stashing, applying, or dropping
    /// one.
    pub fn read_stashes(&mut self) {
        self.set_stashes(git::stashes(&self.root));
    }

    fn set_stashes(&mut self, stashes: Vec<Stash>) {
        self.stashes = stashes;
        let hashes: HashSet<&String> = self.stashes.iter().map(|s| &s.commit.hash).collect();
        self.open.retain(|hash| hashes.contains(hash));
        self.files.retain(|hash, _| hashes.contains(hash));
        self.rebuild();
    }

    pub fn set_height(&mut self, height: u32) {
        self.height = (height as usize).saturating_sub(1).max(1);
        self.scroll_into_view();
    }

    /// Highlights the file `commit` changed at `path`, if it's a stash's,
    /// as the one in the editor.
    pub fn set_active(&mut self, active: Option<(&str, &Path)>) {
        self.active = active.map(|(hash, path)| (hash.to_string(), path.to_path_buf()));
    }

    /// The file or folder selected, if it's one a stash changed.
    pub fn selected(&self) -> Option<Entry> {
        let row = self.rows.get(self.selected)?;
        let is_dir = match row.what {
            What::Folder(_) => true,
            What::File(..) => false,
            What::Stash(_) | What::Buttons(_) => return None,
        };
        Some(Entry {
            path: row.path.clone(),
            is_dir,
            is_root: false,
        })
    }

    /// Where the selection's name starts on screen, if it's in view, for a
    /// menu to open next to.
    pub fn selected_position(&self) -> Option<(u32, u32)> {
        let row = self.rows.get(self.selected)?;
        let y = self.selected.checked_sub(self.scroll)?;
        let icon = match row.what {
            What::Folder(_) | What::File(..) => icons::width(),
            What::Stash(_) | What::Buttons(_) => 0,
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

    pub fn run(&mut self, command: Command) -> StashAction {
        match command {
            Command::TreeOpen => return self.activate(true),
            Command::TreePreview => return self.activate(false),
            Command::TreeApplyStash => return self.press(StashButton::Apply),
            Command::TreePopStash => return self.press(StashButton::Pop),
            Command::TreeDropStash => return self.press(StashButton::Drop),
            _ => self.navigate(command),
        }
        StashAction::None
    }

    /// A left click at column `x` of screen row `y`, in a view `width`
    /// wide: on the top row, the branch shows the branches, and elsewhere,
    /// goes back to the changes; a stash's buttons do what they say; and
    /// another click selects the row there, then opens or closes a stash
    /// or folder, or shows a file: a `double` click moves focus to it.
    pub fn click(&mut self, x: u32, y: u32, width: u32, double: bool) -> StashAction {
        if y == 0 {
            return match top_branch_area(&self.branch, width).contains(&x) {
                true => StashAction::Branches,
                false => StashAction::Back,
            };
        }
        if !self.select_at(y) {
            return StashAction::None;
        }
        let row = &self.rows[self.selected];
        if let What::Buttons(_) = row.what {
            let buttons = buttons(row.depth, width);
            return match buttons.into_iter().find(|(area, _)| area.contains(&x)) {
                Some((_, button)) => self.press(button),
                None => StashAction::None,
            };
        }
        self.activate(double)
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
    }

    /// The columns of what's under the pointer at column `x` of screen row
    /// `y`, that a click does something to, in a view `width` wide.
    pub fn hover(&self, x: u32, y: u32, width: u32) -> Option<Range<u32>> {
        if y == 0 {
            let branch = top_branch_area(&self.branch, width);
            return match branch.contains(&x) {
                true => Some(branch),
                false => Some(0..branch.start),
            };
        }
        let row = self.rows.get(self.scroll + y as usize - 1)?;
        match row.what {
            What::Buttons(_) => buttons(row.depth, width)
                .into_iter()
                .map(|(area, _)| area)
                .find(|area| area.contains(&x)),
            _ => Some(0..width),
        }
    }

    /// Draws the stashes in the columns from `x` to `x + width`.
    pub fn draw(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        frame.with_clip(x, 0, width, self.height as u32 + 1, || {
            draw_top(frame, (x, width), &self.name, &self.branch);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs() as i64);
            let visible = self.scroll..self.rows.len();
            for (index, y) in visible.zip(1..=self.height as u32) {
                self.draw_row(frame, index, (x, y, width), focused, now);
            }
            if self.rows.is_empty() {
                let colors = theme::colors();
                let note = truncate("No stashes.", width.saturating_sub(2) as usize);
                frame.draw_text(&note, x + 1, 1, colors.faint, None, Attributes::NONE);
            }
        });
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
        if let What::Buttons(_) = row.what {
            if let Some(focused) = selected {
                let bg = match focused {
                    true => colors.selected,
                    false => colors.selected_unfocused,
                };
                frame.fill_rect(x, y, width, 1, bg);
            }
            for (&(label, _), (area, _)) in BUTTONS.iter().zip(buttons(row.depth, width)) {
                let (bx, fg) = (x + area.start + 1, colors.muted);
                frame.draw_text(label, bx, y, fg, None, Attributes::NONE);
            }
            return;
        }
        let ago_text;
        let letter;
        let look = match &row.what {
            What::Stash(i) => {
                ago_text = ago(now - self.stashes[*i].commit.time);
                Look {
                    depth: 0,
                    flush: false,
                    open: Some(self.open.contains(&row.stash)),
                    icon: None,
                    name: &row.name,
                    fg: colors.text,
                    attributes: Attributes::NONE,
                    right: Some((&ago_text, colors.muted)),
                    right_icon: None,
                    mark: None,
                }
            }
            What::Folder(_) => {
                let key = (row.stash.clone(), row.path.clone());
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
                    right_icon: None,
                    mark: None,
                }
            }
            What::File(_, commit, change) => {
                let active = self
                    .active
                    .as_ref()
                    .is_some_and(|(hash, path)| *hash == commit.hash && *path == change.path);
                let (fg, mut attributes) = match active {
                    true => (colors.accent, Attributes::BOLD),
                    false => (colors.hue(change.kind.hue()), Attributes::NONE),
                };
                if change.kind == Kind::Deleted {
                    attributes |= Attributes::STRIKETHROUGH;
                }
                letter = change.kind.letter().to_string();
                Look {
                    depth: row.depth,
                    flush: false,
                    open: None,
                    icon: Some(icons::file(&row.name)),
                    name: &row.name,
                    fg,
                    attributes,
                    right: Some((&letter, colors.hue(change.kind.hue()))),
                    right_icon: None,
                    mark: None,
                }
            }
            What::Buttons(_) => return,
        };
        draw_look(frame, &look, (x, y, width), selected);
    }

    // --- rows -----------------------------------------------------------------

    /// Lists the rows again, from the stashes and what's open, keeping the
    /// selection on the same entry while it's still listed.
    fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).cloned();
        let mut rows = Vec::new();
        for (i, stash) in self.stashes.iter().enumerate() {
            let hash = &stash.commit.hash;
            let row = |what, path: PathBuf, name: &str, depth| Row {
                what,
                path,
                name: name.to_string(),
                depth,
                stash: hash.clone(),
            };
            rows.push(row(
                What::Stash(i),
                PathBuf::new(),
                &stash.commit.subject,
                0,
            ));
            if !self.open.contains(hash) {
                continue;
            }
            rows.push(row(What::Buttons(i), PathBuf::new(), "", 1));
            let files = self.files.get(hash).map_or(&[][..], Vec::as_slice);
            let changes: Vec<Change> = files.iter().map(|(_, change)| change.clone()).collect();
            let collapsed =
                |path: &Path| self.collapsed.contains(&(hash.clone(), path.to_path_buf()));
            for file in file_rows(&changes, &self.root, 1, &collapsed) {
                let what = match file.kind {
                    None => What::Folder(i),
                    Some(_) => {
                        let Some((commit, change)) =
                            files.iter().find(|(_, change)| change.path == file.path)
                        else {
                            continue;
                        };
                        What::File(i, commit.clone(), change.clone())
                    }
                };
                rows.push(row(what, file.path, &file.name, file.depth));
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

    // --- navigation -----------------------------------------------------------

    /// The stash the selected row is of.
    fn selected_stash(&self) -> Option<&Stash> {
        let hash = &self.rows.get(self.selected)?.stash;
        self.stashes.iter().find(|stash| &stash.commit.hash == hash)
    }

    /// What `button` does to the selected stash.
    fn press(&self, button: StashButton) -> StashAction {
        let Some(stash) = self.selected_stash() else {
            return StashAction::None;
        };
        let hash = stash.commit.hash.clone();
        match button {
            StashButton::Apply => StashAction::Apply { hash, pop: false },
            StashButton::Pop => StashAction::Apply { hash, pop: true },
            StashButton::Drop => StashAction::Drop {
                hash,
                subject: stash.commit.subject.clone(),
            },
        }
    }

    /// Enter/Space/click: shows the selected file, or opens or closes a
    /// stash or folder.
    fn activate(&mut self, focus: bool) -> StashAction {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return StashAction::None;
        };
        match row.what {
            What::File(_, commit, change) => StashAction::Open {
                commit,
                change,
                focus,
            },
            What::Stash(_) | What::Folder(_) => {
                self.toggle_row(self.selected);
                StashAction::None
            }
            What::Buttons(_) => StashAction::None,
        }
    }
}

impl Outline for StashView {
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn row_depth(&self, row: usize) -> usize {
        self.rows[row].depth
    }

    fn row_open(&self, row: usize) -> Option<bool> {
        let row = &self.rows[row];
        match row.what {
            What::Stash(_) => Some(self.open.contains(&row.stash)),
            What::Folder(_) => Some(
                !self
                    .collapsed
                    .contains(&(row.stash.clone(), row.path.clone())),
            ),
            What::Buttons(_) | What::File(..) => None,
        }
    }

    /// Reads a stash's files the first time it opens.
    fn toggle_row(&mut self, row: usize) {
        let row = self.rows[row].clone();
        match &row.what {
            What::Stash(i) => {
                if !self.open.remove(&row.stash) {
                    if !self.files.contains_key(&row.stash) {
                        let files = git::stash_changes(&self.root, &self.stashes[*i]);
                        self.files.insert(row.stash.clone(), files);
                    }
                    self.open.insert(row.stash);
                }
            }
            What::Folder(_) => {
                let key = (row.stash, row.path);
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
            }
            What::Buttons(_) | What::File(..) => return,
        }
        self.rebuild();
    }

    fn rows_shown(&self) -> usize {
        self.height
    }

    fn place(&mut self) -> (&mut usize, &mut usize) {
        (&mut self.selected, &mut self.scroll)
    }
}

/// Where the buttons under a stash at `depth` are, in a view `width` wide,
/// and which each is.
fn buttons(depth: usize, width: u32) -> Vec<(Range<u32>, StashButton)> {
    let mut x = 1 + 2 * depth as u32;
    BUTTONS
        .iter()
        .map(|&(label, button)| {
            let columns = x..x + label.len() as u32;
            x = columns.end + 2;
            (button_area(columns), button)
        })
        .filter(|(area, _)| area.end <= width)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentui::Rgba;

    fn drawn(view: &StashView, width: u32) -> Vec<String> {
        let screen = opentui::OwnedBuffer::new(
            width,
            view.height as u32 + 1,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, width, true);
        screen
            .to_text(true)
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    #[test]
    fn stashes_open_to_their_buttons_and_files() {
        let _serial = crate::test_serial();
        let root = git::tests::repo("stash-view");
        git::tests::commit_as_cue(&root);
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        git::stash(&root, "wip", false).unwrap();
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        let mut view = StashView::open(&git.repos()[0], "cue".into());
        view.set_height(8);
        let shown = drawn(&view, 30);
        assert_eq!(shown[0], " ‹ cue                   main");
        assert!(shown[1].starts_with(" ▸ wip"), "{shown:?}");
        assert_eq!(view.click(5, 0, 30, false), StashAction::Back);
        assert_eq!(view.click(27, 0, 30, false), StashAction::Branches);

        assert_eq!(view.click(5, 1, 30, false), StashAction::None);
        let shown = drawn(&view, 30);
        assert_eq!(shown[2], "   Apply  Pop  Drop");
        assert!(
            shown[3].contains("a.txt") && shown[4].contains("new.txt"),
            "{shown:?}"
        );
        let hash = view.stashes[0].commit.hash.clone();
        assert_eq!(
            view.click(10, 2, 30, false),
            StashAction::Apply {
                hash: hash.clone(),
                pop: true
            }
        );
        assert_eq!(
            view.run(Command::TreeDropStash),
            StashAction::Drop {
                hash: hash.clone(),
                subject: "wip".into(),
            }
        );
        assert!(matches!(
            view.click(5, 4, 30, false),
            StashAction::Open { focus: false, .. }
        ));

        // A stash replaced without the count changing is read again.
        git::drop_stash(&root, &hash).unwrap();
        std::fs::write(root.join("a.txt"), "again\n").unwrap();
        git::stash(&root, "new stash", false).unwrap();
        git.restart();
        git.wait();
        view.set_repo(&git.repos()[0]);
        assert_eq!(view.stashes.len(), 1);
        assert_eq!(view.stashes[0].commit.subject, "new stash");
        assert_eq!(view.rows.len(), 1, "the one opened is gone");

        git::drop_stash(&root, &view.stashes[0].commit.hash.clone()).unwrap();
        view.read_stashes();
        assert_eq!(drawn(&view, 30)[1], " No stashes.");
    }
}
