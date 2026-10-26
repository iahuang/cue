//! The changes view: the files changed since the last commit, or since a
//! checkpoint, shown in the sidebar in place of the file tree.
//!
//! The top row says which: the last commit, or a checkpoint, and a click
//! on it lists the workspace's checkpoints to pick from (see
//! [`crate::checkpoint`]). Its buttons take a checkpoint (`+`) and, while
//! the files are compared with one, go back to the last commit (`✕`).
//!
//! Under it, each repository the workspace's folders are in has a section,
//! headed by its folder's name, with the branch it's on on the right,
//! which a click switches; with a checkpoint, so do workspace folders in
//! none. The sections share the height evenly, but for those collapsed,
//! which are only their headers, and each scrolls on its own. A click elsewhere on the
//! header collapses the section, as the arrow keys do. Under it are its
//! changed files in their folders, each marked with how it changed and, at
//! the end, how much of it is staged: all (●), some (◐), or none (○). A
//! folder's mark is of the files in it. Clicking a mark, or `s`, stages
//! them all, or unstages them if they all were; `s` on the header does it
//! to every file. A folder holding only another folder shows as one row,
//! `src/git`, as VS Code's compact folders do. Keys and the mouse work as
//! in the tree: a click previews a file, and a double click or Enter opens
//! it.
//!
//! At the bottom of a repository's section is a row of buttons: Commit,
//! which opens the commit dialog; one that catches up with the branch it
//! tracks, as that takes (fetching, pulling, pushing, or both, as GitHub
//! Desktop's does), with how many commits it's behind and ahead; View log;
//! and `⋯`, which pops up the rest of what can be done to it. A rule is
//! between sections.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use opentui::{Attributes, Buffer, Rgba};

use crate::checkpoint::{self, Mark};
use crate::git::{Change, Group, Kind, Repo, Staged, Sync};
use crate::icons::{self, Icon};
use crate::keymap::Command;
use crate::theme;
use crate::tree::{scroll_to_show, truncate, Entry, Outline, TreeAction};
use crate::workspace::root_names;

/// The top row's button that takes a checkpoint.
const TAKE_BUTTON: &str = "+";
/// The top row's button that compares with the last commit again, while
/// the files are compared with a checkpoint.
const CLEAR_BUTTON: &str = "✕";
/// The button that pops up everything that can be done to a repository.
const MORE_BUTTON: &str = "⋯";
/// The log's button, and what it says when there's no room for that.
const LOG_BUTTON: &str = "View log";
const SHORT_LOG_BUTTON: &str = "Log";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    name: String,
    /// 0 for groups.
    depth: usize,
    what: What,
    /// The row has a mark at its end for how much is staged: it's a file or
    /// folder in a repository.
    marked: bool,
    /// How much of the changes it's of are staged; `None` if git lists none
    /// of them, as for a file changed since a checkpoint but not since the
    /// last commit.
    staged: Option<Staged>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// The top row: what the files changed since, as its name says: a
    /// checkpoint, if `true`, or the last commit.
    Since(bool),
    /// A repository, or a workspace folder in none, by its folder's name,
    /// with this on the right: its branch.
    Group(String),
    Folder,
    File(Kind),
    /// Under a group, when nothing in it changed; it can't be selected.
    Note,
}

/// What's on a row of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A row of the list, by its index.
    Row(usize),
    /// The buttons of the group at this index.
    Actions(usize),
    /// Between sections.
    Rule,
    /// Room in a section past its files, or below them all.
    Blank,
}

/// A group's section of the screen.
struct Section {
    group: usize,
    /// Its header's row, and the rows under it: its files, or its note.
    header: usize,
    body: Range<usize>,
    /// How many of those there's room for, and the first of them shown.
    shown: usize,
    scroll: usize,
    /// It ends with its buttons, then a rule, unless it's the last.
    actions: bool,
    rule: bool,
}

impl Section {
    /// How many rows of the screen it takes.
    fn len(&self) -> usize {
        1 + self.shown + self.actions as usize + self.rule as usize
    }
}

impl Row {
    /// Whether it's the same entry as `other`, laid out again: a group's
    /// header and note are of the same path.
    fn same(&self, other: &Row) -> bool {
        self.path == other.path
            && std::mem::discriminant(&self.what) == std::mem::discriminant(&other.what)
    }
}

/// A button under a repository's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Commit,
    Sync,
    Log,
    More,
}

/// The folders and files under a folder, each by name ignoring case, then
/// by name.
#[derive(Default)]
pub(crate) struct Node {
    folders: BTreeMap<(String, String), Node>,
    files: BTreeMap<(String, String), (PathBuf, Kind)>,
}

impl Node {
    /// Adds the file at `path`, `rest` below this folder.
    fn insert(&mut self, rest: &Path, path: &Path, kind: Kind) {
        let mut names = rest.iter().map(|name| name.to_string_lossy().into_owned());
        let Some(mut name) = names.next() else {
            return;
        };
        let mut node = self;
        for next in names {
            node = node.folders.entry(sort_key(name)).or_default();
            name = next;
        }
        node.files
            .insert(sort_key(name), (path.to_path_buf(), kind));
    }
}

fn sort_key(name: String) -> (String, String) {
    (name.to_lowercase(), name)
}

/// A folder or a changed file in a list of them, under a repository or a
/// commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    /// How a file changed; `None` for a folder.
    pub kind: Option<Kind>,
}

/// The rows of `changes`, in their folders under `root`, at `depth` and
/// below: each folder's folders, with what's in them unless `collapsed`
/// says they are, then its files.
pub(crate) fn file_rows(
    changes: &[Change],
    root: &Path,
    depth: usize,
    collapsed: &dyn Fn(&Path) -> bool,
) -> Vec<FileRow> {
    let mut node = Node::default();
    for change in changes {
        if let Ok(rest) = change.path.strip_prefix(root) {
            node.insert(rest, &change.path, change.kind);
        }
    }
    let mut rows = Vec::new();
    push_node(&mut rows, &node, root, depth, collapsed);
    rows
}

/// Appends the rows of `node`, the folder `dir`, at `depth`.
fn push_node(
    rows: &mut Vec<FileRow>,
    node: &Node,
    dir: &Path,
    depth: usize,
    collapsed: &dyn Fn(&Path) -> bool,
) {
    for ((_, name), mut folder) in &node.folders {
        let mut name = name.clone();
        let mut path = dir.join(&name);
        // A folder with only a folder in it shows as one row with it.
        while folder.files.is_empty() && folder.folders.len() == 1 {
            let Some(((_, inner), contents)) = folder.folders.iter().next() else {
                break;
            };
            name = format!("{name}/{inner}");
            path = path.join(inner);
            folder = contents;
        }
        rows.push(FileRow {
            path: path.clone(),
            name,
            depth,
            kind: None,
        });
        if !collapsed(&path) {
            push_node(rows, folder, &path, depth + 1, collapsed);
        }
    }
    for ((_, name), (path, kind)) in &node.files {
        rows.push(FileRow {
            path: path.clone(),
            name: name.clone(),
            depth,
            kind: Some(*kind),
        });
    }
}

/// The changes in the folder `dir`, of `changes`, sorted by path, which
/// lists a folder's files together.
pub(crate) fn changes_under<'a>(
    changes: &'a [Change],
    dir: &'a Path,
) -> impl Iterator<Item = &'a Change> {
    let start = changes.partition_point(|change| change.path.as_path() < dir);
    changes[start..]
        .iter()
        .take_while(move |change| change.path.starts_with(dir))
}

/// The change to the file at `path`, of `changes`, sorted by path.
fn change_at<'a>(changes: &'a [Change], path: &Path) -> Option<&'a Change> {
    let index = changes
        .binary_search_by(|change| change.path.as_path().cmp(path))
        .ok()?;
    Some(&changes[index])
}

/// How much of all of `states` is staged: all if all of each is, none if
/// none of any is, and otherwise some. `None` if there are none.
fn combined(mut states: impl Iterator<Item = Staged>) -> Option<Staged> {
    let first = states.next()?;
    match states.all(|staged| staged == first) {
        true => Some(first),
        false => Some(Staged::Partly),
    }
}

/// How a row of a list in the sidebar is drawn.
pub(crate) struct Look<'a> {
    pub depth: usize,
    /// It starts where the arrow would, having none.
    pub flush: bool,
    /// Whether it's open, for a row that collapses; `None` for one that
    /// doesn't.
    pub open: Option<bool>,
    pub icon: Option<Icon>,
    pub name: &'a str,
    pub fg: Rgba,
    pub attributes: Attributes,
    /// What goes at the right end, if anything, and its color.
    pub right: Option<(&'a str, Rgba)>,
    /// An icon before that.
    pub right_icon: Option<Icon>,
    /// A one-column mark past that, at the very end, and its color: how
    /// much of a change is staged.
    pub mark: Option<(&'a str, Rgba)>,
}

/// Draws a row as `look` has it, at (`x`, `y`), `width` wide, on the
/// selection's color if `selected` (and focused).
pub(crate) fn draw_look(
    frame: &Buffer,
    look: &Look,
    (x, y, width): (u32, u32, u32),
    selected: Option<bool>,
) {
    let colors = theme::colors();
    if let Some(focused) = selected {
        let bg = match focused {
            true => colors.selected,
            false => colors.selected_unfocused,
        };
        frame.fill_rect(x, y, width, 1, bg);
    }
    let indent = x + 1 + 2 * look.depth as u32;
    if let Some(open) = look.open {
        let arrow = if open { "▾" } else { "▸" };
        frame.draw_text(arrow, indent, y, colors.faint, None, Attributes::NONE);
    }
    let mut name_x = if look.flush { indent } else { indent + 2 };
    if let Some(icon) = look.icon.filter(|_| icons::enabled()) {
        name_x = icon.draw(frame, name_x, y, None);
    }
    let mut end = x + width;
    if let Some((mark, fg)) = look.mark {
        end = end.saturating_sub(2);
        if end > name_x {
            frame.draw_text(mark, end, y, fg, None, Attributes::NONE);
        }
    }
    let (right, right_fg) = look.right.unwrap_or(("", colors.muted));
    let right_icon = look.right_icon.filter(|_| icons::enabled());
    let icon_width = right_icon.map_or(0, |_| icons::WIDTH);
    let right_width = right.chars().count() as u32;
    let gap = match right_width {
        0 => 1,
        _ => right_width + icon_width + 2,
    };
    let room = end.saturating_sub(name_x + gap) as usize;
    let name = truncate(look.name, room);
    frame.draw_text(&name, name_x, y, look.fg, None, look.attributes);
    let right_x = end.saturating_sub(right_width + 1);
    if right_width > 0 && right_x > name_x + icon_width {
        if let Some(icon) = right_icon {
            icon.draw(frame, right_x - icon_width, y, None);
        }
        frame.draw_text(right, right_x, y, right_fg, None, Attributes::NONE);
    }
}

pub struct ChangesView {
    groups: Vec<Group>,
    /// What each group is called.
    names: Vec<String>,
    /// The checkpoint the files are compared with, if any.
    mark: Option<Mark>,
    /// A checkpoint's being taken.
    taking: bool,
    /// Folders and groups collapsed, by path.
    collapsed: HashSet<PathBuf>,
    rows: Vec<Row>,
    /// Each group's header's row.
    headers: Vec<usize>,
    selected: usize,
    /// The first of its files shown, of each group scrolled, by path.
    scrolls: HashMap<PathBuf, usize>,
    /// What [`Outline`] scrolls; the sections scroll on their own instead
    /// (see [`ChangesView::sections`]).
    scroll: usize,
    /// Rows on screen.
    height: usize,
    /// The file shown in the editor, highlighted.
    active: Option<PathBuf>,
    /// The active file is a preview, shown in italics.
    active_preview: bool,
    /// What the repositories syncing are doing, by their top folders.
    syncing: HashMap<PathBuf, Sync>,
}

impl ChangesView {
    pub fn new() -> ChangesView {
        let mut view = ChangesView {
            groups: Vec::new(),
            names: Vec::new(),
            mark: None,
            taking: false,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            headers: Vec::new(),
            selected: 0,
            scrolls: HashMap::new(),
            scroll: 0,
            height: 1,
            active: None,
            active_preview: false,
            syncing: HashMap::new(),
        };
        view.rebuild();
        view
    }

    /// Says on its sync button that the repository at `root` is doing
    /// `sync`, or with `None`, that it's done.
    pub fn set_syncing(&mut self, root: &Path, sync: Option<Sync>) {
        match sync {
            Some(sync) => self.syncing.insert(root.to_path_buf(), sync),
            None => self.syncing.remove(root),
        };
    }

    /// Lists the changes in `groups`, keeping the selection on the same
    /// entry while it's still listed.
    pub fn set_groups(&mut self, groups: &[Group]) {
        self.groups = groups.to_vec();
        let roots: Vec<PathBuf> = groups.iter().map(|group| group.root.clone()).collect();
        self.names = root_names(&roots);
        self.rebuild();
    }

    /// Says the files are compared with `mark`, or the last commit, and
    /// whether a checkpoint's being `taking`.
    pub fn set_since(&mut self, mark: Option<&Mark>, taking: bool) {
        self.mark = mark.cloned();
        self.taking = taking;
        self.rebuild();
    }

    /// Marks the files at `paths`, in the repository at `root`, staged, or
    /// not, as they are once git's done, before it says so.
    pub fn set_staged(&mut self, root: &Path, paths: &[PathBuf], stage: bool) {
        let paths: HashSet<&PathBuf> = paths.iter().collect();
        let repo = self
            .groups
            .iter_mut()
            .filter(|group| group.root == root)
            .find_map(|group| group.repo.as_mut());
        let Some(repo) = repo else {
            return;
        };
        for change in &mut repo.changes {
            if paths.contains(&change.path) {
                change.staged = if stage { Staged::Yes } else { Staged::No };
            }
        }
        self.rebuild();
    }

    /// What the group at `root` is called, if it's listed.
    pub fn name_of(&self, root: &Path) -> Option<String> {
        let index = self.groups.iter().position(|group| group.root == root)?;
        self.names.get(index).cloned()
    }

    pub fn set_height(&mut self, height: u32) {
        self.height = (height as usize).max(1);
        self.scroll_into_view();
    }

    /// Highlights `path` as the file in the editor, in italics if it is a
    /// preview. A file that wasn't already the active one is selected, if
    /// it's listed.
    pub fn set_active(&mut self, path: Option<&Path>, preview: bool) {
        if let Some(path) = path.filter(|&path| self.active.as_deref() != Some(path)) {
            let file = |row: &Row| matches!(row.what, What::File(_)) && row.path == path;
            if let Some(index) = self.rows.iter().position(file) {
                self.select(index);
            }
        }
        self.active = path.map(Path::to_path_buf);
        self.active_preview = preview;
    }

    /// The selected entry, if it's a file, folder, or group. A group
    /// counts as a workspace folder: it isn't renamed, moved, or deleted
    /// from here.
    pub fn selected(&self) -> Option<Entry> {
        let row = self.rows.get(self.selected)?;
        matches!(row.what, What::Group(_) | What::Folder | What::File(_)).then(|| Entry {
            path: row.path.clone(),
            is_dir: !matches!(row.what, What::File(_)),
            is_root: matches!(row.what, What::Group(_)),
        })
    }

    /// Where the selected entry's name is on screen: the column, from the
    /// list's left edge, and the row. `None` if it's scrolled out of view.
    pub fn selected_position(&self) -> Option<(u32, u32)> {
        let row = self.rows.get(self.selected)?;
        let y = self
            .screen()
            .iter()
            .position(|&slot| slot == Slot::Row(self.selected))?;
        Some((3 + 2 * row.depth as u32 + icons::width(), y as u32))
    }

    /// Selects the entry on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        match self.slot(y) {
            Slot::Row(index) if !self.row_skipped(index) => {
                self.select(index);
                true
            }
            _ => false,
        }
    }

    pub fn run(&mut self, command: Command) -> TreeAction {
        match command {
            Command::TreeOpen => return self.activate(true, false),
            Command::TreePreview => return self.activate(false, true),
            Command::TreeToggleStaged => return self.toggle_staged(self.selected),
            _ => self.navigate(command),
        }
        TreeAction::None
    }

    /// A left click at column `x` of screen row `y`, from the list's
    /// left edge, `width` wide: on a repository's buttons, does what they
    /// do; elsewhere, selects the entry there, then on a mark, stages or
    /// unstages what it's of; on a repository's branch, shows its
    /// branches; and otherwise collapses or expands a group or folder, or
    /// opens a file: a single click previews it and keeps focus here; a
    /// `double` click keeps it open and moves focus to it. On the top row,
    /// it does what that does.
    pub fn click(&mut self, x: u32, y: u32, width: u32, double: bool) -> TreeAction {
        if let Slot::Actions(group) = self.slot(y) {
            return self.button_click(group, x, y, width);
        }
        if !self.select_at(y) {
            return TreeAction::None;
        }
        let row = &self.rows[self.selected];
        match row.what {
            What::Since(comparing) => self.since_click(x, width, comparing),
            What::Group(_) if self.branch_area(self.selected, width).contains(&x) => {
                TreeAction::Branches(row.path.clone())
            }
            What::Folder | What::File(_) if row.marked && mark_area(width).contains(&x) => {
                self.toggle_staged(self.selected)
            }
            _ => self.activate(double, !double),
        }
    }

    /// What a click at column `x` of screen row `y` does on the buttons of
    /// the repository `group`, in a list `width` wide.
    fn button_click(&self, group: usize, x: u32, y: u32, width: u32) -> TreeAction {
        let group = &self.groups[group];
        let buttons = self.buttons(group, width);
        let Some((button, _, columns)) = buttons
            .into_iter()
            .find(|(_, _, columns)| button_area(columns.clone()).contains(&x))
        else {
            return TreeAction::None;
        };
        let root = group.root.clone();
        match button {
            Button::Commit => TreeAction::Commit(root),
            Button::Sync if self.syncing.contains_key(&root) => TreeAction::None,
            Button::Sync => TreeAction::Sync(root),
            Button::Log => TreeAction::Log(root),
            Button::More => TreeAction::RepoMenu {
                root,
                at: Some((columns.start, y)),
            },
        }
    }

    /// What a click at column `x` of the top row does, in a list `width`
    /// wide: takes a checkpoint, compares with the last commit again, if
    /// `comparing` with a checkpoint, or lists the checkpoints.
    fn since_click(&self, x: u32, width: u32, comparing: bool) -> TreeAction {
        match since_button(x, width, comparing, self.taking) {
            Some((TAKE_BUTTON, _)) => TreeAction::TakeCheckpoint,
            Some(_) => TreeAction::CompareWithCommit,
            None => TreeAction::Checkpoints,
        }
    }

    /// Stages all the changes in the repository at `root`, or if they all
    /// are, unstages them, as `s` on its header does.
    pub fn stage_all(&self, root: &Path) -> TreeAction {
        let header = |row: &Row| matches!(row.what, What::Group(_)) && row.path == root;
        match self.rows.iter().position(header) {
            Some(index) => self.toggle_staged(index),
            None => TreeAction::None,
        }
    }

    /// The top folder of the repository the selected entry is in.
    pub fn selected_repo(&self) -> Option<PathBuf> {
        let path = &self.rows.get(self.selected)?.path;
        self.group_of(path)
            .filter(|group| group.repo.is_some())
            .map(|group| group.root.clone())
    }

    /// The group `path` is listed in.
    fn group_of(&self, path: &Path) -> Option<&Group> {
        self.groups
            .iter()
            .filter(|group| path.starts_with(&group.root))
            .max_by_key(|group| group.root.components().count())
    }

    /// Scrolls the section on screen row `y` by `rows`, without moving the
    /// selection.
    pub fn scroll(&mut self, y: u32, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let mut top = 1;
        for section in self.sections() {
            top += section.len();
            if (y as usize) < top {
                let max = section.body.len().saturating_sub(section.shown);
                let scroll = section.scroll.saturating_add_signed(rows).min(max);
                let root = self.groups[section.group].root.clone();
                self.scrolls.insert(root, scroll);
                return;
            }
        }
    }

    /// The columns of what's under the pointer at column `x` of screen row
    /// `y`, that a click does something to, in a list `width` wide: a row,
    /// a mark, a repository's branch, or a button.
    pub fn hover(&self, x: u32, y: u32, width: u32) -> Option<Range<u32>> {
        let index = match self.slot(y) {
            Slot::Row(index) => index,
            Slot::Actions(group) => {
                let buttons = self.buttons(&self.groups[group], width);
                return buttons
                    .into_iter()
                    .map(|(_, _, columns)| button_area(columns))
                    .find(|area| area.contains(&x));
            }
            Slot::Rule | Slot::Blank => return None,
        };
        let row = &self.rows[index];
        let areas = match &row.what {
            What::Note => return None,
            What::Since(comparing) => match since_button(x, width, *comparing, self.taking) {
                Some((_, columns)) => return Some(button_area(columns)),
                None => {
                    let buttons = since_buttons(width, *comparing, self.taking);
                    let end = buttons
                        .last()
                        .map_or(width, |(_, columns)| columns.start - 1);
                    return Some(0..end);
                }
            },
            What::Group(_) => {
                let branch = self.branch_area(index, width);
                match branch.is_empty() {
                    true => return Some(0..width),
                    false => vec![0..branch.start, branch],
                }
            }
            What::Folder | What::File(_) if row.marked => {
                let mark = mark_area(width);
                vec![0..mark.start, mark]
            }
            What::Folder | What::File(_) => return Some(0..width),
        };
        areas.into_iter().find(|area| area.contains(&x))
    }

    /// Draws the list in the columns from `x` to `x + width`.
    pub fn draw(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        frame.with_clip(x, 0, width, self.height as u32, || {
            let colors = theme::colors();
            for (slot, y) in self.screen().into_iter().zip(0..) {
                match slot {
                    Slot::Row(index) => self.draw_row(frame, index, (x, y, width), focused),
                    Slot::Actions(group) => {
                        self.draw_buttons(frame, &self.groups[group], (x, y, width))
                    }
                    Slot::Rule => {
                        let rule = "─".repeat(width as usize);
                        frame.draw_text(&rule, x, y, colors.divider, None, Attributes::NONE);
                    }
                    Slot::Blank => {}
                }
            }
            let y = self.rows.len() as u32;
            if self.groups.is_empty() && y < self.height as u32 {
                let note = "Not in a git repository. Take a checkpoint (+) to track changes.";
                let room = width.saturating_sub(2) as usize;
                frame.draw_text(
                    &truncate(note, room),
                    x + 1,
                    y,
                    colors.faint,
                    None,
                    Attributes::NONE,
                );
            }
        });
    }

    fn draw_row(
        &self,
        frame: &Buffer,
        index: usize,
        (x, y, width): (u32, u32, u32),
        focused: bool,
    ) {
        let colors = theme::colors();
        let row = &self.rows[index];
        let active = self.active.as_ref() == Some(&row.path) && matches!(row.what, What::File(_));
        let (fg, mut attributes) = match &row.what {
            What::Since(true) => (colors.text, Attributes::NONE),
            What::Since(false) => (colors.muted, Attributes::NONE),
            What::Group(_) => (colors.text, Attributes::BOLD),
            What::Note => (colors.faint, Attributes::NONE),
            _ if active && self.active_preview => {
                (colors.accent, Attributes::BOLD | Attributes::ITALIC)
            }
            _ if active => (colors.accent, Attributes::BOLD),
            What::File(kind) => (colors.hue(kind.hue()), Attributes::NONE),
            _ => (colors.text, Attributes::NONE),
        };
        if row.what == What::File(Kind::Deleted) {
            attributes |= Attributes::STRIKETHROUGH;
        }
        let open = !self.collapsed.contains(&row.path);
        let letter;
        let buttons;
        let mut right_icon = None;
        let (open, icon, right) = match &row.what {
            What::Since(comparing) => {
                buttons = since_text(*comparing, self.taking);
                let right = Some((buttons.as_str(), colors.muted));
                (None, Some(icons::checkpoint()), right)
            }
            What::Group(about) => {
                if !about.is_empty() {
                    right_icon = Some(icons::branch());
                }
                let right = Some((about.as_str(), colors.muted));
                (Some(open), Some(icons::folder(open)), right)
            }
            What::Folder => (Some(open), Some(icons::folder(open)), None),
            What::File(kind) => {
                letter = kind.letter().to_string();
                let right = Some((letter.as_str(), colors.hue(kind.hue())));
                (None, Some(icons::file(&row.name)), right)
            }
            What::Note => (None, None, None),
        };
        let mark = match (row.marked, row.staged) {
            (false, _) => None,
            (true, Some(Staged::Yes)) => Some(("●", colors.accent)),
            (true, Some(Staged::Partly)) => Some(("◐", colors.accent)),
            (true, Some(Staged::No)) => Some(("○", colors.muted)),
            // So how it changed lines up with how the others did.
            (true, None) => Some((" ", colors.muted)),
        };
        let look = Look {
            // The note is where the files would be.
            depth: if row.what == What::Note { 1 } else { row.depth },
            // The top row isn't indented for an arrow, nor is the note.
            flush: matches!(row.what, What::Since(_) | What::Note),
            open,
            icon,
            name: &row.name,
            fg,
            attributes,
            right,
            right_icon,
            mark,
        };
        let selected = (index == self.selected).then_some(focused);
        draw_look(frame, &look, (x, y, width), selected);
    }

    /// Draws the buttons of the repository `group`.
    fn draw_buttons(&self, frame: &Buffer, group: &Group, (x, y, width): (u32, u32, u32)) {
        let colors = theme::colors();
        let repo = group.repo.as_ref();
        let staged = repo.is_some_and(|repo| {
            repo.merging
                || repo
                    .changes
                    .iter()
                    .any(|change| change.staged != Staged::No)
        });
        let syncing = self.syncing.contains_key(&group.root);
        // What there's something to do for stands out.
        let to_sync = repo
            .and_then(Repo::sync)
            .is_some_and(|sync| sync != Sync::Fetch);
        for (button, label, columns) in self.buttons(group, width) {
            let fg = match button {
                Button::Commit if staged => colors.accent,
                Button::Sync if syncing => colors.faint,
                Button::Sync if to_sync => colors.accent,
                _ => colors.muted,
            };
            frame.draw_text(&label, x + columns.start, y, fg, None, Attributes::NONE);
        }
    }

    /// What the buttons shown under the files of the repository at `root`
    /// do, in a list `width` wide, but for `⋯`: none while it's collapsed.
    pub fn button_commands(&self, root: &Path, width: u32) -> Vec<Command> {
        let Some(group) = self.groups.iter().find(|group| group.root == root) else {
            return Vec::new();
        };
        if self.collapsed.contains(root) {
            return Vec::new();
        }
        let sync = group.repo.as_ref().and_then(Repo::sync);
        (self.buttons(group, width).into_iter())
            .filter_map(|(button, _, _)| match button {
                Button::Commit => Some(Command::TreeCommit),
                Button::Log => Some(Command::GitLog),
                Button::Sync => sync.as_ref().map(sync_command),
                Button::More => None,
            })
            .collect()
    }

    /// The buttons under the files of the repository `group`, with their
    /// labels and columns, in a list `width` wide: as many as fit from the
    /// left, and `⋯` at the right end, which has the rest.
    fn buttons(&self, group: &Group, width: u32) -> Vec<(Button, String, Range<u32>)> {
        let more = width.saturating_sub(2);
        let mut labels = vec![(Button::Commit, "Commit".to_string())];
        if let Some(repo) = &group.repo {
            let sync = sync_label(repo, self.syncing.get(&group.root));
            labels.extend(sync.map(|label| (Button::Sync, label)));
        }
        labels.push((Button::Log, LOG_BUTTON.to_string()));
        let mut buttons = place(&labels, more);
        if buttons.len() < labels.len() {
            if let Some(log) = labels.last_mut() {
                log.1 = SHORT_LOG_BUTTON.to_string();
            }
            buttons = place(&labels, more);
        }
        buttons.push((Button::More, MORE_BUTTON.to_string(), more..more + 1));
        buttons
    }

    /// The groups' sections, from the top, under the top row, and over a
    /// row left empty. Those open
    /// share what room there is evenly, a row more each for the first to
    /// spare; those collapsed are only their headers.
    fn sections(&self) -> Vec<Section> {
        let count = self.groups.len();
        let open = |group: &Group| !self.collapsed.contains(&group.root);
        let fixed: usize = (self.groups.iter().enumerate())
            .map(|(index, group)| {
                1 + (open(group) && group.repo.is_some()) as usize + (index + 1 < count) as usize
            })
            .sum();
        // The top row's above them, and a row's left below them, apart from
        // the status bar.
        let room = self.height.saturating_sub(2 + fixed);
        let opened = self
            .groups
            .iter()
            .filter(|group| open(group))
            .count()
            .max(1);
        let mut spare = room % opened;
        let mut sections = Vec::new();
        for (index, group) in self.groups.iter().enumerate() {
            let header = self.headers[index];
            let end = self
                .headers
                .get(index + 1)
                .copied()
                .unwrap_or(self.rows.len());
            let body = header + 1..end;
            let shown = match open(group) {
                true => room / opened + (spare > 0) as usize,
                false => 0,
            };
            if open(group) {
                spare = spare.saturating_sub(1);
            }
            let max = body.len().saturating_sub(shown);
            let scroll = self
                .scrolls
                .get(&group.root)
                .map_or(0, |&scroll| scroll.min(max));
            sections.push(Section {
                group: index,
                header,
                body,
                shown,
                scroll,
                actions: open(group) && group.repo.is_some(),
                rule: index + 1 < count,
            });
        }
        sections
    }

    /// What's on each row of the screen.
    fn screen(&self) -> Vec<Slot> {
        let mut screen = vec![Slot::Row(0)];
        for section in self.sections() {
            screen.push(Slot::Row(section.header));
            let start = section.body.start + section.scroll;
            for index in start..start + section.shown {
                screen.push(match index < section.body.end {
                    true => Slot::Row(index),
                    false => Slot::Blank,
                });
            }
            if section.actions {
                screen.push(Slot::Actions(section.group));
            }
            if section.rule {
                screen.push(Slot::Rule);
            }
        }
        screen.truncate(self.height);
        screen
    }

    /// What's on screen row `y`.
    fn slot(&self, y: u32) -> Slot {
        let screen = self.screen();
        screen.get(y as usize).copied().unwrap_or(Slot::Blank)
    }

    /// The section row `index` is under the header of, if any.
    fn section_of(&self, index: usize) -> Option<Section> {
        self.sections()
            .into_iter()
            .find(|section| section.body.contains(&index))
    }

    /// The columns of the branch on the header at `index`, which shows
    /// the branches, its icon's included; empty if it's not a
    /// repository's, or there's no room for it.
    fn branch_area(&self, index: usize, width: u32) -> Range<u32> {
        let Some(row) = self.rows.get(index) else {
            return 0..0;
        };
        let What::Group(about) = &row.what else {
            return 0..0;
        };
        let start = width.saturating_sub(about.chars().count() as u32 + 1 + icons::width());
        // As [`draw_look`] has it: past the arrow, icon, and a column of
        // the name.
        let name = 3 + icons::width() + 1;
        match about.is_empty() || start <= name {
            true => 0..0,
            false => button_area(start..width - 1),
        }
    }

    // --- rows ------------------------------------------------------------------

    /// Lists the rows again, from the groups and what's collapsed.
    fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).cloned();
        let name = match (&self.mark, self.taking) {
            (_, true) => "Taking a checkpoint…".to_string(),
            (Some(mark), _) => format!("Since {}", checkpoint::label(mark)),
            (None, _) => "Since the last commit".to_string(),
        };
        let row = |what, path: &Path, name: String, depth| Row {
            path: path.to_path_buf(),
            name,
            depth,
            what,
            marked: false,
            staged: None,
        };
        let mut rows = vec![row(
            What::Since(self.mark.is_some()),
            Path::new(""),
            name,
            0,
        )];
        self.headers.clear();
        for (group, name) in self.groups.iter().zip(&self.names) {
            let about = (group.repo.as_ref())
                .map(|repo| repo.head.name().to_string())
                .unwrap_or_default();
            self.headers.push(rows.len());
            rows.push(row(What::Group(about), &group.root, name.clone(), 0));
            if !self.collapsed.contains(&group.root) {
                if group.changes.is_empty() {
                    let note = "No changes.".to_string();
                    rows.push(row(What::Note, &group.root, note, 0));
                }
                let repo = group.repo.as_ref();
                let collapsed = |path: &Path| self.collapsed.contains(path);
                for file in file_rows(&group.changes, &group.root, 1, &collapsed) {
                    let staged = repo.and_then(|repo| match file.kind {
                        Some(_) => change_at(&repo.changes, &file.path).map(|change| change.staged),
                        None => staged_under(group, repo, &file.path),
                    });
                    let what = match file.kind {
                        Some(kind) => What::File(kind),
                        None => What::Folder,
                    };
                    rows.push(Row {
                        marked: repo.is_some(),
                        staged,
                        ..row(what, &file.path, file.name, file.depth)
                    });
                }
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

    // --- navigation -------------------------------------------------------------

    /// Enter/Shift+Space/click: opens the selected file, collapses or
    /// expands a group or folder, or lists the checkpoints.
    fn activate(&mut self, focus: bool, preview: bool) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        match &row.what {
            What::File(_) => TreeAction::Open {
                path: row.path.clone(),
                focus,
                preview,
            },
            What::Since(_) => TreeAction::Checkpoints,
            What::Group(_) | What::Folder => {
                self.toggle_row(self.selected);
                TreeAction::None
            }
            What::Note => TreeAction::None,
        }
    }

    /// `s` or a click on row `index`'s mark: stages the files it's of, or
    /// if they all are, unstages them. A repository's header is of all its
    /// changes, as git lists them.
    fn toggle_staged(&self, index: usize) -> TreeAction {
        let Some(row) = self.rows.get(index) else {
            return TreeAction::None;
        };
        let Some(group) = self.group_of(&row.path) else {
            return TreeAction::None;
        };
        let Some(repo) = &group.repo else {
            return TreeAction::None;
        };
        let changes: Vec<&Change> = match row.what {
            What::Group(_) => repo.changes.iter().collect(),
            What::Folder => changes_under(&group.changes, &row.path)
                .filter_map(|listed| change_at(&repo.changes, &listed.path))
                .collect(),
            What::File(_) => change_at(&repo.changes, &row.path).into_iter().collect(),
            _ => Vec::new(),
        };
        let Some(staged) = combined(changes.iter().map(|change| change.staged)) else {
            return TreeAction::None;
        };
        let stage = staged != Staged::Yes;
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
        TreeAction::Stage {
            root: group.root.clone(),
            paths,
            stage,
        }
    }
}

impl Outline for ChangesView {
    fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn row_depth(&self, row: usize) -> usize {
        self.rows[row].depth
    }

    fn row_open(&self, row: usize) -> Option<bool> {
        let row = &self.rows[row];
        matches!(row.what, What::Group(_) | What::Folder)
            .then(|| !self.collapsed.contains(&row.path))
    }

    fn toggle_row(&mut self, row: usize) {
        let path = self.rows[row].path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.rebuild();
    }

    /// The notes under groups with nothing in them.
    fn row_skipped(&self, row: usize) -> bool {
        self.rows.get(row).is_some_and(|row| row.what == What::Note)
    }

    /// As many as the selection's section has room for.
    fn rows_shown(&self) -> usize {
        self.section_of(self.selected)
            .map_or(self.height, |section| section.shown.max(1))
    }

    /// Scrolls the selection's section to show it.
    fn scroll_into_view(&mut self) {
        let Some(section) = self.section_of(self.selected) else {
            return;
        };
        let (selected, count) = (self.selected - section.body.start, section.body.len());
        let scroll = scroll_to_show(selected, section.scroll, section.shown.max(1), count);
        let root = self.groups[section.group].root.clone();
        self.scrolls.insert(root, scroll);
    }

    fn place(&mut self) -> (&mut usize, &mut usize) {
        (&mut self.selected, &mut self.scroll)
    }
}

/// How much of the changes listed under the folder `dir`, in `group`, are
/// staged in `repo`, of those git lists.
fn staged_under(group: &Group, repo: &Repo, dir: &Path) -> Option<Staged> {
    let staged = changes_under(&group.changes, dir)
        .filter_map(|listed| change_at(&repo.changes, &listed.path))
        .map(|change| change.staged);
    combined(staged)
}

/// A mark's columns, at the end of a row `width` wide, with a column
/// either side.
fn mark_area(width: u32) -> Range<u32> {
    width.saturating_sub(3)..width
}

/// What `sync` does, as a command.
pub fn sync_command(sync: &Sync) -> Command {
    match sync {
        Sync::Fetch => Command::GitFetch,
        Sync::Pull => Command::GitPull,
        Sync::Push | Sync::Publish(_) => Command::GitPush,
        Sync::Both => Command::GitSync,
    }
}

/// What the sync button of `repo` says: what it does, and how many
/// commits it's behind and ahead of the branch it tracks, or what it's
/// doing, if it's `syncing`. `None` if it has nothing to sync with.
fn sync_label(repo: &Repo, syncing: Option<&Sync>) -> Option<String> {
    if let Some(sync) = syncing {
        let label = match sync {
            Sync::Fetch => "Fetching…",
            Sync::Pull => "Pulling…",
            Sync::Push => "Pushing…",
            Sync::Both => "Syncing…",
            Sync::Publish(_) => "Publishing…",
        };
        return Some(label.to_string());
    }
    let mut label = match repo.sync()? {
        Sync::Fetch => "Fetch",
        Sync::Pull => "Pull",
        Sync::Push => "Push",
        Sync::Both => "Pull/Push",
        Sync::Publish(_) => "Publish",
    }
    .to_string();
    if let Some(upstream) = &repo.upstream {
        if upstream.behind > 0 {
            label.push_str(&format!(" ↓{}", upstream.behind));
        }
        if upstream.ahead > 0 {
            label.push_str(&format!(" ↑{}", upstream.ahead));
        }
    }
    Some(label)
}

/// `labels` from the left of a row, two columns apart, as many as fit
/// before column `end` with two to spare, and the columns each takes.
fn place(labels: &[(Button, String)], end: u32) -> Vec<(Button, String, Range<u32>)> {
    let mut x = 1;
    let mut placed = Vec::new();
    for (button, label) in labels {
        let after = x + label.chars().count() as u32;
        if after + 2 > end {
            break;
        }
        placed.push((*button, label.clone(), x..after));
        x = after + 2;
    }
    placed
}

/// The top row's buttons, as they're drawn: back to the last commit, if
/// `comparing` with a checkpoint, then the one that takes one; none while
/// one's `taking`.
fn since_text(comparing: bool, taking: bool) -> String {
    since_buttons(0, comparing, taking)
        .iter()
        .rev()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("  ")
}

/// The top row's buttons, with their columns from the list's left edge,
/// in a list `width` wide, from the right (see [`since_text`]).
fn since_buttons(width: u32, comparing: bool, taking: bool) -> Vec<(&'static str, Range<u32>)> {
    if taking {
        return Vec::new();
    }
    let labels = match comparing {
        true => &[TAKE_BUTTON, CLEAR_BUTTON][..],
        false => &[TAKE_BUTTON][..],
    };
    let mut end = width.saturating_sub(1);
    let mut buttons = Vec::new();
    for label in labels {
        let start = end.saturating_sub(label.chars().count() as u32);
        buttons.push((*label, start..end));
        end = start.saturating_sub(2);
    }
    buttons
}

/// The top row's button at column `x`, in a list `width` wide, and its
/// columns, if there's one there.
fn since_button(
    x: u32,
    width: u32,
    comparing: bool,
    taking: bool,
) -> Option<(&'static str, Range<u32>)> {
    since_buttons(width, comparing, taking)
        .into_iter()
        .find(|(_, columns)| button_area(columns.clone()).contains(&x))
}

/// Where a click on a button that's at `columns` counts: there, and a
/// column either side.
pub(crate) fn button_area(columns: Range<u32>) -> Range<u32> {
    columns.start.saturating_sub(1)..columns.end + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Change, Head, Repo, Upstream};
    use opentui::Rgba;

    fn repo(root: &str, changes: &[(&str, Kind)]) -> Repo {
        staged_repo(
            root,
            &changes
                .iter()
                .map(|&(path, kind)| (path, kind, Staged::No))
                .collect::<Vec<_>>(),
        )
    }

    fn staged_repo(root: &str, changes: &[(&str, Kind, Staged)]) -> Repo {
        let root = PathBuf::from(root);
        let mut changes: Vec<Change> = changes
            .iter()
            .map(|&(path, kind, staged)| Change {
                path: root.join(path),
                kind,
                from: None,
                staged,
            })
            .collect();
        // As git lists them.
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Repo {
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: Some("0123456".into()),
            merging: false,
            changes,
            added: 0,
            removed: 0,
            stashes: Vec::new(),
            upstream: None,
            publish_to: None,
            root,
        }
    }

    fn group(repo: &Repo) -> Group {
        Group {
            root: repo.root.clone(),
            repo: Some(repo.clone()),
            changes: repo.changes.clone(),
        }
    }

    fn view(repos: &[Repo]) -> ChangesView {
        let mut view = ChangesView::new();
        view.set_height(20);
        view.set_groups(&repos.iter().map(group).collect::<Vec<_>>());
        view
    }

    /// The rows, indented by depth: the top row marked with `◆`, a
    /// group's right end after `@`, folders with `/`, files with how they
    /// changed, and either with how much is staged.
    fn listing(view: &ChangesView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let name = &row.name;
                let indent = "  ".repeat(row.depth);
                let mark = match (row.marked, row.staged) {
                    (false, _) => "",
                    (true, None) => " _",
                    (true, Some(Staged::No)) => " ○",
                    (true, Some(Staged::Partly)) => " ◐",
                    (true, Some(Staged::Yes)) => " ●",
                };
                match &row.what {
                    What::Since(_) => format!("◆ {name}"),
                    What::Group(about) if about.is_empty() => name.clone(),
                    What::Group(about) => format!("{name} @{about}"),
                    What::Folder => format!("{indent}{name}/{mark}"),
                    What::File(kind) => format!("{indent}{name} {}{mark}", kind.letter()),
                    What::Note => format!("  ({name})"),
                }
            })
            .collect()
    }

    /// The first `rows` rows drawn `width` wide, trimmed.
    fn drawn(view: &ChangesView, width: u32, rows: usize) -> Vec<String> {
        let screen = opentui::OwnedBuffer::new(
            width,
            rows as u32,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, width, true);
        let text = screen.to_text(true);
        text.lines()
            .take(rows)
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    /// A repository's buttons, `labels`, as a row `width` wide shows them:
    /// from the left, two columns apart, and `⋯` past them, at the end.
    fn row(labels: &[&str], width: usize) -> String {
        let left = format!(" {}", labels.join("  "));
        format!("{left:<0$}⋯", width - 2)
    }

    const SINCE: &str = "◆ Since the last commit";

    #[test]
    fn lists_changes_in_their_folders_folders_first() {
        let view = view(&[repo(
            "/w/cue",
            &[
                ("README.md", Kind::Modified),
                ("src/a.rs", Kind::Added),
                ("src/deep/er/b.rs", Kind::Deleted),
                ("Cargo.toml", Kind::Untracked),
            ],
        )]);
        assert_eq!(
            listing(&view),
            [
                SINCE,
                "cue @main",
                "  src/ ○",
                "    deep/er/ ○",
                "      b.rs D ○",
                "    a.rs A ○",
                "  Cargo.toml U ○",
                "  README.md M ○",
            ]
        );
    }

    #[test]
    fn folders_collapse_and_keep_the_selection() {
        let mut view = view(&[repo(
            "/w/cue",
            &[("src/a.rs", Kind::Modified), ("z.rs", Kind::Modified)],
        )]);
        view.run(Command::TreeLast);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue/z.rs"),
            "the buttons and rule are stepped over"
        );
        view.select(2);
        view.run(Command::TreeCollapse);
        assert_eq!(
            listing(&view),
            [SINCE, "cue @main", "  src/ ○", "  z.rs M ○"]
        );
        assert_eq!(view.selected().unwrap().path, PathBuf::from("/w/cue/src"));
        view.run(Command::TreeCollapse);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue"),
            "left steps out"
        );
        view.run(Command::TreeCollapse);
        assert_eq!(
            listing(&view),
            [SINCE, "cue @main"],
            "the buttons go with the files"
        );
        view.run(Command::TreeExpand);
        view.run(Command::TreeExpand);
        view.run(Command::TreeExpand);
        view.run(Command::TreeExpand);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue/src/a.rs")
        );
        // New status from git keeps the selected file selected.
        let cue = repo(
            "/w/cue",
            &[("new.rs", Kind::Untracked), ("src/a.rs", Kind::Modified)],
        );
        view.set_groups(&[group(&cue)]);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue/src/a.rs")
        );
    }

    #[test]
    fn clicks_preview_and_double_clicks_open() {
        let mut view = view(&[repo("/w/cue", &[("a.rs", Kind::Modified)])]);
        let root = PathBuf::from("/w/cue");
        let path = root.join("a.rs");
        let open = |focus, preview| TreeAction::Open {
            path: path.clone(),
            focus,
            preview,
        };
        assert_eq!(view.click(3, 2, 30, false), open(false, true));
        assert_eq!(view.click(3, 2, 30, true), open(true, false));
        assert_eq!(
            view.click(3, 4, 30, false),
            TreeAction::None,
            "below the files"
        );
        // A repository's branch shows its branches; the rest of its row
        // collapses it, as Enter does.
        assert_eq!(
            view.click(26, 1, 30, false),
            TreeAction::Branches(root.clone())
        );
        assert_eq!(view.click(8, 1, 30, false), TreeAction::None);
        assert_eq!(listing(&view), [SINCE, "cue @main"]);
        assert_eq!(view.run(Command::TreeOpen), TreeAction::None);
        assert_eq!(listing(&view)[2], "  a.rs M ○");
    }

    #[test]
    fn the_buttons_under_a_repository_do_what_they_say() {
        let _serial = crate::test_serial();
        let mut cue = repo("/w/cue", &[("a.rs", Kind::Modified)]);
        cue.upstream = Some(Upstream {
            name: "origin/main".into(),
            ahead: 0,
            behind: 0,
        });
        let mut view = view(&[cue]);
        view.set_height(5);
        let root = PathBuf::from("/w/cue");
        let selected = view.selected;
        assert_eq!(
            drawn(&view, 30, 4)[3],
            row(&["Commit", "Fetch", "View log"], 30)
        );
        assert_eq!(
            view.click(3, 3, 30, false),
            TreeAction::Commit(root.clone())
        );
        assert_eq!(view.click(10, 3, 30, false), TreeAction::Sync(root.clone()));
        assert_eq!(view.click(18, 3, 30, false), TreeAction::Log(root.clone()));
        assert_eq!(view.click(25, 3, 30, false), TreeAction::None, "between");
        assert_eq!(
            view.click(28, 3, 30, false),
            TreeAction::RepoMenu {
                root: root.clone(),
                at: Some((28, 3)),
            }
        );
        assert_eq!(
            view.selected, selected,
            "the buttons don't take the selection"
        );
        assert_eq!(view.hover(10, 3, 30), Some(8..15));
        assert_eq!(view.hover(25, 3, 30), None);
    }

    #[test]
    fn the_sync_button_says_what_catching_up_takes() {
        let _serial = crate::test_serial();
        let mut cue = repo("/w/cue", &[("a.rs", Kind::Modified)]);
        let buttons = |cue: &Repo, view: &mut ChangesView, width| {
            view.set_groups(&[group(cue)]);
            drawn(view, width, 4)[3].clone()
        };
        let mut view = view(&[]);
        view.set_height(5);
        let upstream = |ahead, behind| {
            Some(Upstream {
                name: "origin/main".into(),
                ahead,
                behind,
            })
        };
        cue.upstream = upstream(0, 2);
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "Pull ↓2", "View log"], 30)
        );
        cue.upstream = upstream(3, 0);
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "Push ↑3", "View log"], 30)
        );
        cue.upstream = upstream(3, 2);
        assert_eq!(
            buttons(&cue, &mut view, 33),
            row(&["Commit", "Pull/Push ↓2 ↑3", "Log"], 33),
            "the log's button gets shorter to fit"
        );
        assert_eq!(drawn(&view, 30, 2)[1], " ▾ cue                   main");
        assert_eq!(
            buttons(&cue, &mut view, 40),
            row(&["Commit", "Pull/Push ↓2 ↑3", "View log"], 40)
        );
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "Pull/Push ↓2 ↑3"], 30)
        );
        assert_eq!(
            buttons(&cue, &mut view, 18),
            row(&["Commit"], 18),
            "and the rest are in the menu"
        );
        view.set_syncing(Path::new("/w/cue"), Some(Sync::Both));
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "Syncing…", "Log"], 30)
        );
        assert_eq!(view.click(10, 3, 30, false), TreeAction::None);
        view.set_syncing(Path::new("/w/cue"), None);
        cue.upstream = None;
        cue.publish_to = Some("origin".into());
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "Publish", "View log"], 30)
        );
        cue.publish_to = None;
        assert_eq!(
            buttons(&cue, &mut view, 30),
            row(&["Commit", "View log"], 30),
            "nowhere to push to"
        );
    }

    #[test]
    fn marks_and_stages_what_is_staged() {
        let _serial = crate::test_serial();
        let mut cue = staged_repo(
            "/w/cue",
            &[
                ("a.rs", Kind::Modified, Staged::Partly),
                ("src/b.rs", Kind::Renamed, Staged::Yes),
                ("src/c.rs", Kind::Added, Staged::Yes),
                ("z/d.rs", Kind::Untracked, Staged::No),
            ],
        );
        cue.changes[1].from = Some(PathBuf::from("/w/cue/old.rs"));
        let mut view = view(&[cue]);
        assert_eq!(
            listing(&view),
            [
                SINCE,
                "cue @main",
                "  src/ ●",
                "    b.rs R ●",
                "    c.rs A ●",
                "  z/ ○",
                "    d.rs U ○",
                "  a.rs M ◐",
            ]
        );
        let root = PathBuf::from("/w/cue");
        let paths =
            |names: &[&str]| -> Vec<PathBuf> { names.iter().map(|name| root.join(name)).collect() };
        assert_eq!(
            view.click(28, 2, 30, false),
            TreeAction::Stage {
                root: root.clone(),
                paths: paths(&["src/b.rs", "old.rs", "src/c.rs"]),
                stage: false,
            },
            "a rename unstages from both its paths"
        );
        assert_eq!(view.hover(28, 2, 30), Some(27..30));
        assert_eq!(view.hover(5, 2, 30), Some(0..27));
        view.select(7);
        assert_eq!(
            view.run(Command::TreeToggleStaged),
            TreeAction::Stage {
                root: root.clone(),
                paths: paths(&["a.rs"]),
                stage: true,
            }
        );
        view.select(1);
        assert_eq!(
            view.run(Command::TreeToggleStaged),
            TreeAction::Stage {
                root: root.clone(),
                paths: paths(&["a.rs", "z/d.rs"]),
                stage: true,
            },
            "the header's of all of them"
        );
        view.set_staged(&root, &paths(&["a.rs", "z/d.rs"]), true);
        assert_eq!(listing(&view)[5], "  z/ ●");
        assert_eq!(listing(&view)[7], "  a.rs M ●");
        assert_eq!(drawn(&view, 30, 8)[7], "     a.rs                 M ●");
    }

    #[test]
    fn only_what_git_lists_is_marked_against_a_checkpoint() {
        let cue = staged_repo("/w/cue", &[("a.rs", Kind::Modified, Staged::Yes)]);
        let mut since = group(&cue);
        since.changes = vec![
            cue.changes[0].clone(),
            Change {
                path: PathBuf::from("/w/cue/b.rs"),
                kind: Kind::Added,
                from: None,
                staged: Staged::No,
            },
        ];
        let notes = Group {
            root: PathBuf::from("/w/notes"),
            repo: None,
            changes: vec![Change {
                path: PathBuf::from("/w/notes/todo.md"),
                kind: Kind::Modified,
                from: None,
                staged: Staged::No,
            }],
        };
        let mut view = view(&[]);
        view.set_groups(&[since, notes]);
        assert_eq!(
            listing(&view),
            [
                SINCE,
                "cue @main",
                "  a.rs M ●",
                "  b.rs A _",
                "notes",
                "  todo.md M",
            ],
            "a folder in no repository has no buttons"
        );
        view.select(3);
        assert_eq!(view.run(Command::TreeToggleStaged), TreeAction::None);
    }

    #[test]
    fn draws_the_branch_and_how_files_changed() {
        let _serial = crate::test_serial();
        let mut cue = repo(
            "/w/cue",
            &[("a.rs", Kind::Modified), ("a-long-name.rs", Kind::Added)],
        );
        cue.upstream = Some(Upstream {
            name: "origin/main".into(),
            ahead: 3,
            behind: 12,
        });
        let mut view = view(&[cue]);
        view.set_height(6);
        assert_eq!(
            drawn(&view, 30, 5),
            [
                " Since the last commit      +",
                " ▾ cue                   main",
                "     a-long-name.rs       A ○",
                "     a.rs                 M ○",
                " Commit  Pull/Push ↓12 ↑3   ⋯",
            ]
        );
        // A folder in no repository says nothing more.
        view.set_groups(&[Group {
            root: PathBuf::from("/w/notes"),
            repo: None,
            changes: Vec::new(),
        }]);
        assert_eq!(drawn(&view, 24, 3)[1..], [" ▾ notes", "   No changes."]);
    }

    #[test]
    fn the_top_row_takes_and_picks_checkpoints() {
        let _serial = crate::test_serial();
        let mut view = view(&[repo("/w/cue", &[("a.rs", Kind::Modified)])]);
        assert_eq!(drawn(&view, 34, 1), [" Since the last commit          +"]);
        assert_eq!(view.click(32, 0, 34, false), TreeAction::TakeCheckpoint);
        assert_eq!(view.hover(32, 0, 34), Some(31..34));
        assert_eq!(view.click(5, 0, 34, false), TreeAction::Checkpoints);
        assert_eq!(view.run(Command::TreeOpen), TreeAction::Checkpoints);

        let mark = Mark {
            id: 1,
            name: Some("agent".into()),
        };
        view.set_since(Some(&mark), false);
        assert_eq!(drawn(&view, 34, 1), [" Since “agent”               ✕  +"]);
        assert_eq!(view.click(29, 0, 34, false), TreeAction::CompareWithCommit);
        assert_eq!(view.click(32, 0, 34, false), TreeAction::TakeCheckpoint);
        assert_eq!(view.hover(5, 0, 34), Some(0..28));

        view.set_since(Some(&mark), true);
        assert_eq!(drawn(&view, 34, 1), [" Taking a checkpoint…"]);
        assert_eq!(view.click(32, 0, 34, false), TreeAction::Checkpoints);
    }

    #[test]
    fn groups_are_apart_and_the_keys_go_over_what_is_between() {
        let mut view = view(&[
            repo("/w/cue", &[("a.rs", Kind::Modified)]),
            repo("/w/empty", &[]),
            repo("/w/other", &[("b.ts", Kind::Modified)]),
        ]);
        assert_eq!(
            listing(&view),
            [
                SINCE,
                "cue @main",
                "  a.rs M ○",
                "empty @main",
                "  (No changes.)",
                "other @main",
                "  b.ts M ○",
            ]
        );
        view.select(2);
        view.run(Command::TreeDown);
        assert_eq!(view.selected, 3);
        view.run(Command::TreeDown);
        assert_eq!(view.selected, 5, "the note is stepped over");
        view.run(Command::TreeUp);
        assert_eq!(view.selected, 3);
        view.run(Command::TreeExpand);
        assert_eq!(view.selected, 3, "there's nothing in it to go into");
        // The sections share the 18 rows between the top one and the
        // empty one at the bottom: 3 each for the header, buttons, and
        // rule, but the last one's, and 4, 3, and 3 for files.
        assert!(
            !view.select_at(3),
            "nor can the room past the files be clicked"
        );
        assert!(!view.select_at(6), "or the buttons");
        assert!(!view.select_at(7), "or a rule");
        assert!(view.select_at(8));
        assert!(!view.select_at(9), "or a note");
        assert_eq!(
            view.name_of(Path::new("/w/other")).as_deref(),
            Some("other")
        );
    }

    #[test]
    fn sections_share_the_height_and_scroll_on_their_own() {
        let _serial = crate::test_serial();
        let mut view = view(&[
            repo(
                "/w/cue",
                &[
                    ("a.rs", Kind::Modified),
                    ("b.rs", Kind::Modified),
                    ("c.rs", Kind::Modified),
                    ("d.rs", Kind::Modified),
                    ("e.rs", Kind::Modified),
                ],
            ),
            repo("/w/other", &[("z.rs", Kind::Modified)]),
        ]);
        view.set_height(13);
        let names = |view: &ChangesView| -> Vec<String> {
            drawn(view, 30, 13)
                .iter()
                .map(|line| {
                    let words = line
                        .split_whitespace()
                        .filter(|&word| !matches!(word, "▾" | "▸"));
                    let word = words.take(1).collect::<String>();
                    match word.as_str() {
                        "Since" | "Commit" | "──────────────────────────────" => {
                            word.chars().take(1).collect()
                        }
                        _ => word,
                    }
                })
                .collect()
        };
        let shown =
            |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };
        assert_eq!(
            names(&view),
            shown(&[
                "S", "cue", "a.rs", "b.rs", "c.rs", "C", "─", "other", "z.rs", "", "", "C", ""
            ])
        );
        view.scroll(3, 1);
        assert_eq!(names(&view)[2..5], shown(&["c.rs", "d.rs", "e.rs"]));
        view.scroll(9, 1);
        assert_eq!(names(&view)[8], "z.rs", "the other has nothing to scroll");
        view.select(2);
        assert_eq!(names(&view)[2..5], shown(&["a.rs", "b.rs", "c.rs"]));
        view.run(Command::TreeLast);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/other/z.rs")
        );
        view.select(7);
        view.run(Command::TreeCollapse);
        assert_eq!(
            names(&view),
            shown(&[
                "S", "cue", "a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "", "", "C", "─", "other", ""
            ]),
            "a collapsed section is only its header"
        );
    }

    #[test]
    fn says_when_there_is_nothing_to_list() {
        let _serial = crate::test_serial();
        assert!(drawn(&view(&[]), 30, 2)[1].starts_with(" Not in a git repository."));
        assert_eq!(
            drawn(&view(&[repo("/w/cue", &[])]), 30, 3)[2],
            "   No changes."
        );
    }
}
