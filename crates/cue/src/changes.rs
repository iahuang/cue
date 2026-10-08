//! The changes view: the files git says changed since the last commit,
//! shown in the sidebar in place of the file tree.
//!
//! Each repository the workspace's folders are in is listed, under a row
//! for what it does as a whole: the branch it's on, which shows its
//! branches to switch to, a button that syncs it with the branch it tracks,
//! saying what it'd do (fetch, pull, push, or both), and buttons to its
//! commit view, where changes are staged and committed, and to its log.
//! Under its name are its changed files in their folders, each marked with
//! how it changed. A folder holding only another folder shows as one row,
//! `src/git`, as VS Code's compact folders do. Keys and the mouse work as
//! in the tree: a click previews a file, and a double click or Enter opens
//! it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use opentui::{Attributes, Buffer, Rgba};

use crate::git::{Change, Kind, Repo, Sync};
use crate::icons::{self, Icon};
use crate::keymap::Command;
use crate::theme;
use crate::tree::{truncate, Entry, Outline, TreeAction};
use crate::workspace::root_names;

/// The button on a repository's first row that shows its log.
const LOG_BUTTON: &str = "Log";
/// The button before it, which shows the commit view.
const COMMIT_BUTTON: &str = "Commit";

/// A button on a repository's first row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderButton {
    /// Syncs it with the branch it tracks.
    Sync,
    Commit,
    Log,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    name: String,
    /// 0 for repositories.
    depth: usize,
    what: What,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// A repository's first row: the branch it's on, as its name, and the
    /// buttons: the sync button, saying this, if it has one, then the
    /// buttons to its commit view and its log.
    Header(Option<String>),
    /// A repository's name, which collapses it.
    Repo,
    Folder,
    File(Kind),
    /// Between repositories; it can't be selected.
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
    repos: Vec<Repo>,
    /// What each repository is called.
    names: Vec<String>,
    /// Folders and repositories collapsed, by path.
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
        ChangesView {
            repos: Vec::new(),
            names: Vec::new(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
            active_preview: false,
            syncing: HashMap::new(),
        }
    }

    /// Says on its sync button that the repository at `root` is doing
    /// `sync`, or with `None`, that it's done.
    pub fn set_syncing(&mut self, root: &Path, sync: Option<Sync>) {
        match sync {
            Some(sync) => self.syncing.insert(root.to_path_buf(), sync),
            None => self.syncing.remove(root),
        };
        self.rebuild();
    }

    /// Lists the changes in `repos`, keeping the selection on the same
    /// entry while it's still listed.
    pub fn set_repos(&mut self, repos: &[Repo]) {
        self.repos = repos.to_vec();
        let roots: Vec<PathBuf> = repos.iter().map(|repo| repo.root.clone()).collect();
        self.names = root_names(&roots);
        self.rebuild();
    }

    /// What the repository at `root` is called, if it's listed.
    pub fn name_of(&self, root: &Path) -> Option<String> {
        let index = self.repos.iter().position(|repo| repo.root == root)?;
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

    /// The selected entry, if there are any. A repository counts as a
    /// workspace folder: it isn't renamed, moved, or deleted from here.
    pub fn selected(&self) -> Option<Entry> {
        let row = self.rows.get(self.selected)?;
        (row.what != What::Gap).then(|| Entry {
            path: row.path.clone(),
            is_dir: !matches!(row.what, What::File(_)),
            is_root: matches!(row.what, What::Header(_) | What::Repo),
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
    /// it. On a repository's first row, only its buttons do anything.
    pub fn click(&mut self, x: u32, y: u32, width: u32, double: bool) -> TreeAction {
        if !self.select_at(y) {
            return TreeAction::None;
        }
        let row = &self.rows[self.selected];
        if let What::Header(sync) = &row.what {
            let root = row.path.clone();
            return match header_button(x, width, sync.as_deref()) {
                Some((HeaderButton::Sync, _)) => TreeAction::Sync(root),
                Some((HeaderButton::Commit, _)) => TreeAction::Commit(root),
                Some((HeaderButton::Log, _)) => TreeAction::Log(root),
                None if branch_area(&row.name, width, sync.as_deref()).contains(&x) => {
                    TreeAction::Branches(root)
                }
                None => TreeAction::None,
            };
        }
        self.activate(double, !double)
    }

    /// The top folder of the repository the selected entry is in.
    pub fn selected_repo(&self) -> Option<PathBuf> {
        let path = &self.rows.get(self.selected)?.path;
        self.repos
            .iter()
            .filter(|repo| path.starts_with(&repo.root))
            .max_by_key(|repo| repo.root.components().count())
            .map(|repo| repo.root.clone())
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * crate::config::get().scroll_lines as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
    }

    /// The columns of what's under the pointer at column `x` of screen row
    /// `y`, that a click does something to, in a list `width` wide: a row,
    /// or a button on a repository's first row.
    pub fn hover(&self, x: u32, y: u32, width: u32) -> Option<Range<u32>> {
        let row = self.rows.get(self.scroll + y as usize)?;
        match &row.what {
            What::Gap => None,
            What::Header(sync) => match header_button(x, width, sync.as_deref()) {
                Some((_, columns)) => Some(button_area(columns)),
                None => Some(branch_area(&row.name, width, sync.as_deref()))
                    .filter(|area| area.contains(&x)),
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
            let note = match self.repos.is_empty() {
                true => Some("Not in a git repository."),
                false => self
                    .repos
                    .iter()
                    .all(|repo| repo.changes.is_empty())
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
            What::Header(_) => (colors.text, Attributes::NONE),
            What::Repo => (colors.text, Attributes::BOLD),
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
            What::Header(sync) => {
                buttons = header_text(sync.as_deref());
                let right = Some((buttons.as_str(), colors.muted));
                (None, Some(icons::branch()), right)
            }
            What::Repo => (Some(open), Some(icons::folder(open)), None),
            What::Folder => (Some(open), Some(icons::folder(open)), None),
            What::File(kind) => {
                letter = kind.letter().to_string();
                let right = Some((letter.as_str(), colors.hue(kind.hue())));
                (None, Some(icons::file(&row.name)), right)
            }
        };
        let look = Look {
            depth: row.depth,
            // A repository's first row isn't indented for an arrow.
            flush: matches!(row.what, What::Header(_)),
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

    /// Lists the rows again, from the repositories and what's collapsed.
    fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).cloned();
        let mut rows = Vec::new();
        for (repo, name) in self.repos.iter().zip(&self.names) {
            if !rows.is_empty() {
                rows.push(Row {
                    path: PathBuf::new(),
                    name: String::new(),
                    depth: 0,
                    what: What::Gap,
                });
            }
            rows.push(Row {
                path: repo.root.clone(),
                name: repo.head.name().to_string(),
                depth: 0,
                what: What::Header(sync_label(repo, self.syncing.get(&repo.root))),
            });
            rows.push(Row {
                path: repo.root.clone(),
                name: name.clone(),
                depth: 0,
                what: What::Repo,
            });
            if self.collapsed.contains(&repo.root) {
                continue;
            }
            let collapsed = |path: &Path| self.collapsed.contains(path);
            let files = file_rows(&repo.changes, &repo.root, 1, &collapsed);
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

    /// Enter/Shift+Space/click: opens the selected file, or collapses or expands
    /// a folder.
    fn activate(&mut self, focus: bool, preview: bool) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        match row.what {
            What::File(_) => {
                return TreeAction::Open {
                    path: row.path.clone(),
                    focus,
                    preview,
                }
            }
            What::Header(_) => return TreeAction::Log(row.path.clone()),
            What::Gap => return TreeAction::None,
            What::Repo | What::Folder => {}
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
        matches!(row.what, What::Repo | What::Folder).then(|| !self.collapsed.contains(&row.path))
    }

    fn toggle_row(&mut self, row: usize) {
        let path = self.rows[row].path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.rebuild();
    }

    /// Those between repositories.
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

/// What a repository's sync button says: what it does, with how many
/// commits it'd bring in or send, or what it's doing, if it's `syncing`.
/// `None` if it has no sync button.
fn sync_label(repo: &Repo, syncing: Option<&Sync>) -> Option<String> {
    if let Some(sync) = syncing {
        let doing = match sync {
            Sync::Fetch => "Fetching…",
            Sync::Pull => "Pulling…",
            Sync::Push => "Pushing…",
            Sync::Both => "Syncing…",
            Sync::Publish(_) => "Publishing…",
        };
        return Some(doing.to_string());
    }
    let (ahead, behind) = repo
        .upstream
        .as_ref()
        .map_or((0, 0), |upstream| (upstream.ahead, upstream.behind));
    Some(match repo.sync()? {
        Sync::Fetch => "Fetch".to_string(),
        Sync::Pull => format!("Pull ↓{behind}"),
        Sync::Push => format!("Push ↑{ahead}"),
        Sync::Both => format!("Sync ↓{behind} ↑{ahead}"),
        Sync::Publish(_) => "Publish".to_string(),
    })
}

/// A repository's first row's buttons, as they're drawn: the sync button,
/// saying `sync`, if it has one, then the others.
fn header_text(sync: Option<&str>) -> String {
    let others = format!("{COMMIT_BUTTON}  {LOG_BUTTON}");
    match sync {
        Some(sync) => format!("{sync}  {others}"),
        None => others,
    }
}

/// The buttons on a repository's first row, with their columns from the
/// list's left edge, in a list `width` wide, from the right: Log, Commit,
/// and the sync button, saying `sync`, if it has one.
fn header_buttons(width: u32, sync: Option<&str>) -> Vec<(HeaderButton, Range<u32>)> {
    let labels = [
        (HeaderButton::Log, LOG_BUTTON),
        (HeaderButton::Commit, COMMIT_BUTTON),
    ];
    let sync = sync.map(|label| (HeaderButton::Sync, label));
    let mut end = width.saturating_sub(1);
    let mut buttons = Vec::new();
    for (button, label) in labels.into_iter().chain(sync) {
        let start = end.saturating_sub(label.chars().count() as u32);
        buttons.push((button, start..end));
        end = start.saturating_sub(2);
    }
    buttons
}

/// The button on a repository's first row at column `x`, from the list's
/// left edge, in a list `width` wide, and its columns, if there's one
/// there. `sync` is what its sync button says, if it has one.
fn header_button(x: u32, width: u32, sync: Option<&str>) -> Option<(HeaderButton, Range<u32>)> {
    header_buttons(width, sync)
        .into_iter()
        .find(|(_, columns)| button_area(columns.clone()).contains(&x))
}

/// The branch on a repository's first row, which shows its branches: its
/// icon and name, as far as there's room before the buttons, in a list
/// `width` wide.
fn branch_area(branch: &str, width: u32, sync: Option<&str>) -> Range<u32> {
    let name_x = 1 + icons::width();
    let buttons = header_text(sync).chars().count() as u32;
    let room = width.saturating_sub(name_x + buttons + 2);
    let shown = (branch.chars().count() as u32).min(room);
    button_area(1..name_x + shown)
}

/// Where a click on a button that's at `columns` counts: there, and a
/// column either side.
pub(crate) fn button_area(columns: Range<u32>) -> Range<u32> {
    columns.start.saturating_sub(1)..columns.end + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Change, Head, Staged};
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

    fn view(repos: &[Repo]) -> ChangesView {
        let mut view = ChangesView::new();
        view.set_height(20);
        view.set_repos(repos);
        view
    }

    /// The rows, indented by depth, branches marked with `@`, folders with
    /// `/`, and files with how they changed.
    fn listing(view: &ChangesView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let name = &row.name;
                let indent = "  ".repeat(row.depth);
                match &row.what {
                    What::Header(_) => format!("@{name}"),
                    What::Repo => name.clone(),
                    What::Folder => format!("{indent}{name}/"),
                    What::File(kind) => format!("{indent}{name} {}", kind.letter()),
                    What::Gap => String::new(),
                }
            })
            .collect()
    }

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
                "@main",
                "cue",
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
        assert_eq!(listing(&view), ["@main", "cue", "  src/", "  z.rs M"]);
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
        view.set_repos(&[repo(
            "/w/cue",
            &[("new.rs", Kind::Untracked), ("src/a.rs", Kind::Modified)],
        )]);
        assert_eq!(
            view.selected().unwrap().path,
            PathBuf::from("/w/cue/src/a.rs")
        );
    }

    #[test]
    fn clicks_preview_and_double_clicks_open() {
        let mut view = view(&[repo("/w/cue", &[("a.rs", Kind::Modified)])]);
        let path = PathBuf::from("/w/cue/a.rs");
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
        // On the first row, the buttons at its right end show the log and
        // the commit view, and the branch shows the branches.
        let log = TreeAction::Log(PathBuf::from("/w/cue"));
        assert_eq!(view.click(17, 0, 20, false), log);
        let commit = TreeAction::Commit(PathBuf::from("/w/cue"));
        assert_eq!(view.click(10, 0, 20, false), commit);
        let branches = TreeAction::Branches(PathBuf::from("/w/cue"));
        assert_eq!(view.click(3, 0, 20, false), branches, "the branch");
        assert_eq!(view.click(6, 0, 20, false), TreeAction::None, "past it");
        assert_eq!(view.run(Command::TreeOpen), log, "as Enter on it does");
        assert_eq!(
            view.click(3, 1, 20, false),
            TreeAction::None,
            "a repository collapses"
        );
        assert_eq!(listing(&view), ["@main", "cue"]);
    }

    #[test]
    fn draws_the_branch_and_how_files_changed() {
        let _serial = crate::test_serial();
        let mut view = view(&[repo(
            "/w/cue",
            &[("a.rs", Kind::Modified), ("a-long-name.rs", Kind::Added)],
        )]);
        view.set_height(5);
        let screen =
            opentui::OwnedBuffer::new(20, 5, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, 20, true);
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(
            lines[..4],
            [
                " main   Commit  Log",
                " ▾ cue",
                "     a-long-name… A",
                "     a.rs         M"
            ]
        );
    }

    #[test]
    fn the_sync_button_says_what_it_does() {
        let _serial = crate::test_serial();
        let drawn = |view: &ChangesView| {
            let screen =
                opentui::OwnedBuffer::new(34, 1, false, opentui::WidthMethod::Unicode, "test")
                    .unwrap();
            screen.clear(Rgba::BLACK);
            view.draw(&screen, 0, 34, true);
            screen
                .to_text(true)
                .lines()
                .next()
                .unwrap()
                .trim_end()
                .to_string()
        };
        let mut cue = repo("/w/cue", &[]);
        cue.commit = Some("0123abc".into());
        let root = cue.root.clone();
        assert_eq!(
            drawn(&view(&[cue.clone()])),
            " main                 Commit  Log"
        );
        cue.publish_to = Some("origin".into());
        assert_eq!(
            drawn(&view(&[cue.clone()])),
            " main        Publish  Commit  Log"
        );
        cue.upstream = Some(crate::git::Upstream {
            name: "origin/main".into(),
            ahead: 0,
            behind: 0,
        });
        assert_eq!(
            drawn(&view(&[cue.clone()])),
            " main          Fetch  Commit  Log"
        );
        cue.upstream.as_mut().unwrap().ahead = 3;
        assert_eq!(
            drawn(&view(&[cue.clone()])),
            " main        Push ↑3  Commit  Log"
        );
        cue.upstream.as_mut().unwrap().behind = 12;
        let mut view = view(&[cue]);
        assert_eq!(drawn(&view), " main    Sync ↓12 ↑3  Commit  Log");
        // Commit and Log stay where they were.
        let at = |view: &mut ChangesView, x| view.click(x, 0, 34, false);
        assert_eq!(at(&mut view, 18), TreeAction::Sync(root.clone()));
        assert_eq!(at(&mut view, 9), TreeAction::Sync(root.clone()));
        assert_eq!(at(&mut view, 24), TreeAction::Commit(root.clone()));
        assert_eq!(at(&mut view, 31), TreeAction::Log(root.clone()));
        assert_eq!(at(&mut view, 3), TreeAction::Branches(root.clone()));
        view.set_syncing(&root, Some(Sync::Both));
        assert_eq!(drawn(&view), " main       Syncing…  Commit  Log");
        view.set_syncing(&root, None);
        assert_eq!(drawn(&view), " main    Sync ↓12 ↑3  Commit  Log");
    }

    #[test]
    fn repositories_are_apart_and_the_keys_go_over_the_gap() {
        let mut view = view(&[
            repo("/w/cue", &[("a.rs", Kind::Modified)]),
            repo("/w/other", &[("b.ts", Kind::Modified)]),
        ]);
        assert_eq!(
            listing(&view),
            ["@main", "cue", "  a.rs M", "", "@main", "other", "  b.ts M"]
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
        let drawn = |view: &ChangesView| {
            let screen =
                opentui::OwnedBuffer::new(30, 3, false, opentui::WidthMethod::Unicode, "test")
                    .unwrap();
            screen.clear(Rgba::BLACK);
            view.draw(&screen, 0, 30, true);
            screen.to_text(true)
        };
        assert!(drawn(&view(&[])).contains("Not in a git repository."));
        let clean = view(&[repo("/w/cue", &[])]);
        assert!(drawn(&clean).contains("No changes."));
    }
}
