//! The changes view: the files changed since the last commit, or since a
//! checkpoint, shown in the sidebar in place of the file tree.
//!
//! The top row says which: the last commit, or a checkpoint, and a click
//! on it lists the workspace's checkpoints to pick from (see
//! [`crate::checkpoint`]). Its buttons take a checkpoint (`+`) and, while
//! the files are compared with one, go back to the last commit (`✕`).
//!
//! Under it, each repository the workspace's folders are in is listed by
//! its folder's name, with the branch it's on and how far that is from
//! the branch it tracks on the right; with a checkpoint, so are workspace
//! folders in none. A click on a repository's row pops up, beside it, what
//! can be done to it: commit, see its log, switch branches, fetch, pull, or
//! push. Its arrow collapses it, as the arrow keys do. Under it are its
//! changed files in their folders, each marked with how it changed. A
//! folder holding only another folder shows as one row, `src/git`, as VS
//! Code's compact folders do. Keys and the mouse work as in the tree: a
//! click previews a file, and a double click or Enter opens it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use opentui::{Attributes, Buffer, Rgba};

use crate::checkpoint::{self, Mark};
use crate::git::{Change, Group, Kind, Sync};
use crate::icons::{self, Icon};
use crate::keymap::Command;
use crate::theme;
use crate::tree::{truncate, Entry, Outline, TreeAction};
use crate::workspace::root_names;

/// The top row's button that takes a checkpoint.
const TAKE_BUTTON: &str = "+";
/// The top row's button that compares with the last commit again, while
/// the files are compared with a checkpoint.
const CLEAR_BUTTON: &str = "✕";
/// The columns a group's arrow takes, which collapses it when clicked.
const ARROW: Range<u32> = 0..3;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    name: String,
    /// 0 for groups.
    depth: usize,
    what: What,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// The top row: what the files changed since, as its name says: a
    /// checkpoint, if `true`, or the last commit.
    Since(bool),
    /// A repository, or a workspace folder in none, by its folder's name,
    /// with this on the right: its branch and how far that is from the
    /// branch it tracks, or what it's doing to catch up.
    Group(String),
    Folder,
    File(Kind),
    /// Between groups; it can't be selected.
    Gap,
}

impl Row {
    /// Whether it's the same entry as `other`, laid out again: a
    /// repository's header and name are of the same path.
    fn same(&self, other: &Row) -> bool {
        self.path == other.path
            && std::mem::discriminant(&self.what) == std::mem::discriminant(&other.what)
    }
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

/// How to draw a row of a list in the sidebar.
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
    let right_width = right.chars().count() as u32;
    let gap = if right_width > 0 { right_width + 2 } else { 1 };
    let room = end.saturating_sub(name_x + gap) as usize;
    let name = truncate(look.name, room);
    frame.draw_text(&name, name_x, y, look.fg, None, look.attributes);
    let right_x = end.saturating_sub(right_width + 1);
    if right_width > 0 && right_x > name_x {
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
    selected: usize,
    /// The first row on screen.
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
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
            active_preview: false,
            syncing: HashMap::new(),
        };
        view.rebuild();
        view
    }

    /// Says on its row that the repository at `root` is doing `sync`, or
    /// with `None`, that it's done.
    pub fn set_syncing(&mut self, root: &Path, sync: Option<Sync>) {
        match sync {
            Some(sync) => self.syncing.insert(root.to_path_buf(), sync),
            None => self.syncing.remove(root),
        };
        self.rebuild();
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
            if let Some(index) = self.rows.iter().position(|row| row.path == path) {
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
        let y = self.selected.checked_sub(self.scroll)?;
        (y < self.height).then_some((3 + 2 * row.depth as u32 + icons::width(), y as u32))
    }

    /// Selects the entry on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        let index = self.scroll + y as usize;
        if self.rows.get(index).is_none_or(|row| row.what == What::Gap) {
            return false;
        }
        self.select(index);
        true
    }

    pub fn run(&mut self, command: Command) -> TreeAction {
        match command {
            Command::TreeOpen => return self.activate(true, false),
            Command::TreePreview => return self.activate(false, true),
            _ => self.navigate(command),
        }
        TreeAction::None
    }

    /// A left click at column `x` of screen row `y`, from the list's
    /// left edge, `width` wide: selects the entry there, then collapses or
    /// expands a folder, or opens a file: a single click previews it and
    /// keeps focus here; a `double` click keeps it open and moves focus to
    /// it. On the top row, and a repository's, it does what they do.
    pub fn click(&mut self, x: u32, y: u32, width: u32, double: bool) -> TreeAction {
        if !self.select_at(y) {
            return TreeAction::None;
        }
        let row = &self.rows[self.selected];
        match row.what {
            What::Since(comparing) => self.since_click(x, width, comparing),
            What::Group(_) if ARROW.contains(&x) => {
                self.toggle_row(self.selected);
                TreeAction::None
            }
            _ => self.activate(double, !double),
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

    /// The top folder of the repository the selected entry is in.
    pub fn selected_repo(&self) -> Option<PathBuf> {
        let path = &self.rows.get(self.selected)?.path;
        self.groups
            .iter()
            .filter(|group| group.repo.is_some() && path.starts_with(&group.root))
            .max_by_key(|group| group.root.components().count())
            .map(|group| group.root.clone())
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
    }

    /// The columns of what's under the pointer at column `x` of screen row
    /// `y`, that a click does something to, in a list `width` wide: a row,
    /// or a button on the top row.
    pub fn hover(&self, x: u32, y: u32, width: u32) -> Option<Range<u32>> {
        let row = self.rows.get(self.scroll + y as usize)?;
        match &row.what {
            What::Gap => None,
            What::Since(comparing) => match since_button(x, width, *comparing, self.taking) {
                Some((_, columns)) => Some(button_area(columns)),
                None => {
                    let buttons = since_buttons(width, *comparing, self.taking);
                    let end = buttons
                        .last()
                        .map_or(width, |(_, columns)| columns.start - 1);
                    Some(0..end)
                }
            },
            _ => Some(0..width),
        }
    }

    /// Draws the list in the columns from `x` to `x + width`.
    pub fn draw(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        frame.with_clip(x, 0, width, self.height as u32, || {
            let colors = theme::colors();
            let visible = self.scroll..self.rows.len();
            for (index, y) in visible.zip(0..self.height as u32) {
                self.draw_row(frame, index, (x, y, width), focused);
            }
            let note = match self.groups.is_empty() {
                true => Some("Not in a git repository. Take a checkpoint (+) to track changes."),
                false => self
                    .groups
                    .iter()
                    .all(|group| group.changes.is_empty())
                    .then_some("No changes."),
            };
            let y = self.rows.len().saturating_sub(self.scroll) as u32;
            if let Some(note) = note.filter(|_| y < self.height as u32) {
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
        let active = self.active.as_ref() == Some(&row.path);
        let (fg, mut attributes) = match &row.what {
            What::Since(true) => (colors.text, Attributes::NONE),
            What::Since(false) => (colors.muted, Attributes::NONE),
            What::Group(_) => (colors.text, Attributes::BOLD),
            _ if active && self.active_preview => {
                (colors.accent, Attributes::BOLD | Attributes::ITALIC)
            }
            _ if active => (colors.accent, Attributes::BOLD),
            What::File(kind) => (colors.hue(kind.hue()), Attributes::NONE),
            What::Folder | What::Gap => (colors.text, Attributes::NONE),
        };
        if row.what == What::File(Kind::Deleted) {
            attributes |= Attributes::STRIKETHROUGH;
        }
        let open = !self.collapsed.contains(&row.path);
        let letter;
        let buttons;
        let (open, icon, right) = match &row.what {
            What::Gap => return,
            What::Since(comparing) => {
                buttons = since_text(*comparing, self.taking);
                let right = Some((buttons.as_str(), colors.muted));
                (None, Some(icons::checkpoint()), right)
            }
            What::Group(about) => {
                let right = Some((about.as_str(), colors.muted));
                (Some(open), Some(icons::folder(open)), right)
            }
            What::Folder => (Some(open), Some(icons::folder(open)), None),
            What::File(kind) => {
                letter = kind.letter().to_string();
                let right = Some((letter.as_str(), colors.hue(kind.hue())));
                (None, Some(icons::file(&row.name)), right)
            }
        };
        let look = Look {
            depth: row.depth,
            // The top row isn't indented for an arrow.
            flush: matches!(row.what, What::Since(_)),
            open,
            icon,
            name: &row.name,
            fg,
            attributes,
            right,
            mark: None,
        };
        let selected = (index == self.selected).then_some(focused);
        draw_look(frame, &look, (x, y, width), selected);
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
        let mut rows = vec![Row {
            path: PathBuf::new(),
            name,
            depth: 0,
            what: What::Since(self.mark.is_some()),
        }];
        for (index, (group, name)) in self.groups.iter().zip(&self.names).enumerate() {
            if index > 0 {
                rows.push(Row {
                    path: PathBuf::new(),
                    name: String::new(),
                    depth: 0,
                    what: What::Gap,
                });
            }
            rows.push(Row {
                path: group.root.clone(),
                name: name.clone(),
                depth: 0,
                what: What::Group(group_text(group, self.syncing.get(&group.root))),
            });
            if self.collapsed.contains(&group.root) {
                continue;
            }
            let collapsed = |path: &Path| self.collapsed.contains(path);
            let files = file_rows(&group.changes, &group.root, 1, &collapsed);
            rows.extend(files.into_iter().map(|file| Row {
                path: file.path,
                name: file.name,
                depth: file.depth,
                what: match file.kind {
                    Some(kind) => What::File(kind),
                    None => What::Folder,
                },
            }));
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
    /// expands a folder, pops up what can be done to a repository, or
    /// lists the checkpoints.
    fn activate(&mut self, focus: bool, preview: bool) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        match &row.what {
            What::File(_) => {
                return TreeAction::Open {
                    path: row.path.clone(),
                    focus,
                    preview,
                }
            }
            What::Since(_) => return TreeAction::Checkpoints,
            What::Group(_) if self.selected_repo().as_ref() == Some(&row.path) => {
                return TreeAction::RepoMenu(row.path.clone())
            }
            What::Gap => return TreeAction::None,
            What::Group(_) | What::Folder => {}
        }
        self.toggle_row(self.selected);
        TreeAction::None
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

    /// Those between groups.
    fn row_skipped(&self, row: usize) -> bool {
        self.rows.get(row).is_some_and(|row| row.what == What::Gap)
    }

    fn rows_shown(&self) -> usize {
        self.height
    }

    fn place(&mut self) -> (&mut usize, &mut usize) {
        (&mut self.selected, &mut self.scroll)
    }
}

/// What a group's row says on the right: a repository's branch, and how
/// many commits it's ahead of and behind the branch it tracks, or what
/// it's doing, if it's `syncing`. Nothing for a folder in no repository.
fn group_text(group: &Group, syncing: Option<&Sync>) -> String {
    let Some(repo) = &group.repo else {
        return String::new();
    };
    let branch = repo.head.name();
    if let Some(sync) = syncing {
        let doing = match sync {
            Sync::Fetch => "Fetching…",
            Sync::Pull => "Pulling…",
            Sync::Push => "Pushing…",
            Sync::Both => "Syncing…",
            Sync::Publish(_) => "Publishing…",
        };
        return format!("{branch} {doing}");
    }
    let mut text = branch.to_string();
    if let Some(upstream) = &repo.upstream {
        if upstream.behind > 0 {
            text.push_str(&format!(" ↓{}", upstream.behind));
        }
        if upstream.ahead > 0 {
            text.push_str(&format!(" ↑{}", upstream.ahead));
        }
    }
    text
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
    use crate::git::{Change, Head, Repo, Staged, Upstream};
    use opentui::Rgba;

    fn repo(root: &str, changes: &[(&str, Kind)]) -> Repo {
        let root = PathBuf::from(root);
        Repo {
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: None,
            merging: false,
            changes: changes
                .iter()
                .map(|&(path, kind)| Change {
                    path: root.join(path),
                    kind,
                    from: None,
                    staged: Staged::No,
                })
                .collect(),
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
    /// group's right end after `@`, folders with `/`, and files with how
    /// they changed.
    fn listing(view: &ChangesView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let name = &row.name;
                let indent = "  ".repeat(row.depth);
                match &row.what {
                    What::Since(_) => format!("◆ {name}"),
                    What::Group(about) if about.is_empty() => name.clone(),
                    What::Group(about) => format!("{name} @{about}"),
                    What::Folder => format!("{indent}{name}/"),
                    What::File(kind) => format!("{indent}{name} {}", kind.letter()),
                    What::Gap => String::new(),
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
                "  src/",
                "    deep/er/",
                "      b.rs D",
                "    a.rs A",
                "  Cargo.toml U",
                "  README.md M",
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
        assert_eq!(view.selected().unwrap().path, PathBuf::from("/w/cue/z.rs"));
        view.select(2);
        view.run(Command::TreeCollapse);
        assert_eq!(listing(&view), [SINCE, "cue @main", "  src/", "  z.rs M"]);
        assert_eq!(view.selected().unwrap().path, PathBuf::from("/w/cue/src"));
        view.run(Command::TreeCollapse);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue"),
            "left steps out"
        );
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
        assert_eq!(view.click(3, 2, 20, false), open(false, true));
        assert_eq!(view.click(3, 2, 20, true), open(true, false));
        assert_eq!(
            view.click(3, 5, 20, false),
            TreeAction::None,
            "below the rows"
        );
        // A repository's row pops up what can be done to it, from a click
        // or Enter; its arrow collapses it.
        let menu = TreeAction::RepoMenu(root.clone());
        assert_eq!(view.click(8, 1, 20, false), menu);
        assert_eq!(view.run(Command::TreeOpen), menu);
        assert_eq!(view.click(1, 1, 20, false), TreeAction::None);
        assert_eq!(listing(&view), [SINCE, "cue @main"]);
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
        assert_eq!(
            drawn(&view, 24, 4),
            [
                " Since the last comm… +",
                " ▾ cue      main ↓12 ↑3",
                "     a-long-name.rs   A",
                "     a.rs             M"
            ]
        );
        view.set_syncing(Path::new("/w/cue"), Some(Sync::Push));
        assert_eq!(drawn(&view, 24, 2)[1], " ▾ cue    main Pushing…");
        view.set_syncing(Path::new("/w/cue"), None);
        // A folder in no repository says nothing more.
        view.set_groups(&[Group {
            root: PathBuf::from("/w/notes"),
            repo: None,
            changes: Vec::new(),
        }]);
        assert_eq!(drawn(&view, 24, 2)[1], " ▾ notes");
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
    fn groups_are_apart_and_the_keys_go_over_the_gap() {
        let mut view = view(&[
            repo("/w/cue", &[("a.rs", Kind::Modified)]),
            repo("/w/other", &[("b.ts", Kind::Modified)]),
        ]);
        assert_eq!(
            listing(&view),
            [
                SINCE,
                "cue @main",
                "  a.rs M",
                "",
                "other @main",
                "  b.ts M"
            ]
        );
        view.select(2);
        view.run(Command::TreeDown);
        assert_eq!(view.selected, 4, "the gap is stepped over");
        view.run(Command::TreeUp);
        assert_eq!(view.selected, 2);
        assert!(!view.select_at(3), "nor can it be clicked");
        assert_eq!(
            view.name_of(Path::new("/w/other")).as_deref(),
            Some("other")
        );
    }

    #[test]
    fn says_when_there_is_nothing_to_list() {
        let _serial = crate::test_serial();
        assert!(drawn(&view(&[]), 30, 2)[1].starts_with(" Not in a git repository."));
        assert_eq!(
            drawn(&view(&[repo("/w/cue", &[])]), 30, 3)[2],
            " No changes."
        );
    }
}
