//! The file tree: the workspace roots and whichever folders are expanded,
//! shown as an indented list.
//!
//! Folders are read when they are expanded and re-read on refresh; nothing
//! watches the file system yet. Which folders are expanded is remembered by
//! path, so collapsing a folder and expanding it again restores its subfolders.
//! A root inside another shows in both, expanded apart in each.
//! What `.gitignore` and `.ignore` files exclude is shown dimmed, and left
//! out of the file picker and workspace search.
//!
//! Scrolled down, the folders the top rows are in stick to the top, one
//! row each, outermost first, so it's clear where in the tree you are.
//!
//! Files git says changed are colored by how, with its letter at the right
//! end, and a collapsed folder with changes in it is marked with a dot.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use opentui::{Attributes, Buffer, Rgba};

use crate::file_index;
use crate::git::{Kind, Repo};
use crate::icons;
use crate::keymap::Command;
use crate::theme;
use crate::workspace::{deepest_root, root_names};

/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

/// Entries never shown, here or in the file picker and workspace search,
/// with those the `files.exclude` setting names.
const HIDDEN: &[&str] = &[".git", ".DS_Store"];

/// Whether an entry named `name` is never shown.
pub fn is_hidden(name: &std::ffi::OsStr) -> bool {
    HIDDEN.iter().any(|&hidden| name == hidden)
        || name
            .to_str()
            .is_some_and(|name| crate::config::get().excludes(name))
}

/// Whether a file named `name` says which entries are ignored.
fn is_ignore_file(name: &std::ffi::OsStr) -> bool {
    name == ".gitignore" || name == ".ignore"
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    name: String,
    /// 0 for workspace roots.
    depth: usize,
    /// The root it's listed under, by position.
    root: usize,
    is_dir: bool,
    /// Excluded by an ignore file, or inside a folder that is.
    ignored: bool,
}

/// What the app should do after the tree handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum TreeAction {
    None,
    /// Show this file in the editor, and move focus there if `focus`. A
    /// `preview` replaces the previous preview unless it was edited.
    Open {
        path: PathBuf,
        focus: bool,
        preview: bool,
    },
}

/// An entry in the tree, as the file commands see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    /// A workspace root, which isn't renamed, moved, or deleted from here,
    /// wherever it's listed.
    pub is_root: bool,
}

pub struct FileTree {
    roots: Vec<PathBuf>,
    /// What each root is called.
    names: Vec<String>,
    /// The folders expanded under each root.
    expanded: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Every visible entry, top to bottom.
    rows: Vec<Row>,
    /// Counts changes to `rows`, to tell when the open folders may have.
    listing: u64,
    selected: usize,
    /// The first row on screen.
    scroll: usize,
    /// Rows on screen.
    height: usize,
    /// The file shown in the editor, highlighted.
    active: Option<PathBuf>,
    /// The active file is a preview, shown in italics.
    active_preview: bool,
    /// How each file git says changed did.
    changes: HashMap<PathBuf, Kind>,
    /// The folders with changes in them.
    changed_folders: HashSet<PathBuf>,
}

impl FileTree {
    /// A tree of `roots`, each expanded.
    pub fn new(roots: &[PathBuf]) -> FileTree {
        let mut tree = FileTree {
            roots: Vec::new(),
            names: Vec::new(),
            expanded: HashMap::new(),
            rows: Vec::new(),
            listing: 0,
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
            active_preview: false,
            changes: HashMap::new(),
            changed_folders: HashSet::new(),
        };
        tree.set_roots(roots);
        tree
    }

    /// Shows `roots`; new ones start expanded.
    pub fn set_roots(&mut self, roots: &[PathBuf]) {
        self.expanded.retain(|root, _| roots.contains(root));
        for root in roots {
            self.expanded
                .entry(root.clone())
                .or_insert_with(|| HashSet::from([root.clone()]));
        }
        self.roots = roots.to_vec();
        self.names = root_names(roots);
        self.refresh();
    }

    /// Re-reads every expanded folder, keeping the selection on the same
    /// entry when it still exists.
    pub fn refresh(&mut self) {
        let selected = self
            .rows
            .get(self.selected)
            .map(|row| (self.roots.get(row.root).cloned(), row.path.clone()));
        let mut rows = Vec::new();
        for (index, root) in self.roots.iter().enumerate() {
            rows.push(Row {
                path: root.clone(),
                name: self.names[index].clone(),
                depth: 0,
                root: index,
                is_dir: true,
                ignored: false,
            });
            if self.expanded[root].contains(root) {
                self.push_children(&mut rows, index, root, 1, false);
            }
        }
        self.rows = rows;
        self.listing += 1;
        let selected = selected.and_then(|(root, path)| {
            let root = self.roots.iter().position(|other| Some(other) == root.as_ref())?;
            self.index_of(root, &path)
        });
        if let Some(index) = selected {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.scroll_into_view();
    }

    /// A number that changes whenever what's listed does.
    pub fn listing(&self) -> u64 {
        self.listing
    }

    /// Whether changes to `paths` change what's listed: an entry of an open
    /// folder came, went, or became a folder or stopped being one, or an
    /// ignore file there changed. Told by looking, not by what the changes
    /// were said to be, which isn't to be trusted (see [`Changes`](crate::watch::Changes)).
    pub fn is_changed_by<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) -> bool {
        let open: HashSet<&Path> = self.open_folders().map(PathBuf::as_path).collect();
        let listed: HashMap<&Path, bool> = self
            .rows
            .iter()
            .map(|row| (row.path.as_path(), row.is_dir))
            .collect();
        paths.into_iter().any(|path| {
            let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
                return false;
            };
            if !open.contains(folder) || is_hidden(name) {
                return false;
            }
            // Whether it's a folder, as `read_dir` tells, if it's there.
            let now = fs::symlink_metadata(path).ok().map(|_| path.is_dir());
            is_ignore_file(name) || now != listed.get(path.as_path()).copied()
        })
    }

    /// The folders listed with their contents showing.
    pub fn open_folders(&self) -> impl Iterator<Item = &PathBuf> {
        self.rows
            .iter()
            .filter(|row| row.is_dir && self.is_open(row))
            .map(|row| &row.path)
    }

    pub fn set_height(&mut self, height: u32) {
        self.height = (height as usize).max(1);
        self.scroll_into_view();
    }

    /// Highlights `path` as the file in the editor, in italics if it is a
    /// preview. A file that wasn't already the active one is revealed.
    pub fn set_active(&mut self, path: Option<&Path>, preview: bool) {
        if let Some(path) = path.filter(|&path| self.active.as_deref() != Some(path)) {
            self.reveal(path);
        }
        self.active = path.map(Path::to_path_buf);
        self.active_preview = preview;
    }

    /// Marks the files changed in `repos`, and the folders they're in.
    pub fn set_changes(&mut self, repos: &[Repo]) {
        self.changes.clear();
        self.changed_folders.clear();
        for repo in repos {
            for change in &repo.changes {
                self.changes.insert(change.path.clone(), change.kind);
                let folders = change.path.ancestors().skip(1);
                for folder in folders.take_while(|folder| folder.starts_with(&repo.root)) {
                    if !self.changed_folders.insert(folder.to_path_buf()) {
                        break;
                    }
                }
            }
        }
    }

    /// The selected entry, if there are any.
    pub fn selected(&self) -> Option<Entry> {
        self.rows.get(self.selected).map(|row| Entry {
            path: row.path.clone(),
            is_dir: row.is_dir,
            is_root: row.depth == 0 || self.roots.contains(&row.path),
        })
    }

    /// The first workspace root.
    pub fn root(&self) -> Option<Entry> {
        self.roots.first().map(|root| Entry {
            path: root.clone(),
            is_dir: true,
            is_root: true,
        })
    }

    /// Where the selected entry's name is on screen: the column, from the
    /// tree's left edge, and the row. `None` if it's scrolled out of view.
    pub fn selected_position(&self) -> Option<(u32, u32)> {
        let row = self.rows.get(self.selected)?;
        let sticky = self.sticky();
        let y = match sticky.iter().position(|&index| index == self.selected) {
            Some(y) => y,
            None => self
                .selected
                .checked_sub(self.scroll)
                .filter(|&y| y >= sticky.len())?,
        };
        (y < self.height).then_some((3 + 2 * row.depth as u32 + icons::width(), y as u32))
    }

    /// Selects the entry on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        let Some(index) = self.row_at(y) else {
            return false;
        };
        self.select(index);
        true
    }

    /// The index of the row on screen row `y`, if any: a folder stuck to
    /// the top, or the row scrolled there.
    fn row_at(&self, y: u32) -> Option<usize> {
        let y = y as usize;
        let index = match self.sticky().get(y) {
            Some(&index) => index,
            None => self.scroll + y,
        };
        (index < self.rows.len()).then_some(index)
    }

    /// The folders stuck to the top, as row indices, outermost first: the
    /// folders the first rows below them are in, where they're scrolled
    /// out of sight above. They take at most half the rows.
    fn sticky(&self) -> Vec<usize> {
        let mut sticky = Vec::new();
        while sticky.len() < self.height / 2 {
            let depth = sticky.len();
            // The row just below, if this one sticks.
            let below = self.scroll + depth + 1;
            if self.rows.get(below).is_none_or(|row| row.depth <= depth) {
                break;
            }
            // The folder it's in at this depth: the last row above it no deeper.
            let Some(folder) = self.rows[..below]
                .iter()
                .rposition(|row| row.depth <= depth)
            else {
                break;
            };
            // Where it would be on screen anyway.
            if folder >= self.scroll + depth {
                break;
            }
            sticky.push(folder);
        }
        sticky
    }

    /// Follows `from`, and what's in it, to `to`: folders expanded there
    /// stay expanded. Refreshes and selects `to`.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        for expanded in self.expanded.values_mut() {
            *expanded = std::mem::take(expanded)
                .into_iter()
                .map(|path| match path.strip_prefix(from) {
                    Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
                    Ok(rest) => to.join(rest),
                    Err(_) => path,
                })
                .collect();
        }
        self.refresh();
        self.reveal(to);
    }

    #[cfg(test)]
    pub fn active_is_preview(&self) -> bool {
        self.active_preview
    }

    /// Expands the folders down to `path` under the deepest root it's in,
    /// and selects it there, if it is in the workspace. Re-reads the tree
    /// only if it had to expand a folder or doesn't list `path` yet.
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = deepest_root(&self.roots, path).map(Path::to_path_buf) else {
            return;
        };
        let Some(index) = self.roots.iter().position(|other| *other == root) else {
            return;
        };
        let expanded = self.expanded.entry(root.clone()).or_default();
        let folders: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(&root))
            .map(Path::to_path_buf)
            .collect();
        let collapsed = folders.iter().any(|dir| !expanded.contains(dir));
        expanded.extend(folders);
        if collapsed || self.index_of(index, path).is_none() {
            self.refresh();
        }
        if let Some(row) = self.index_of(index, path) {
            self.select(row);
        }
    }

    pub fn run(&mut self, command: Command) -> TreeAction {
        let page = self.height.saturating_sub(1).max(1);
        match command {
            Command::TreeUp => self.select(self.selected.saturating_sub(1)),
            Command::TreeDown => self.select(self.selected + 1),
            Command::TreePageUp => self.select(self.selected.saturating_sub(page)),
            Command::TreePageDown => self.select(self.selected + page),
            Command::TreeFirst => self.select(0),
            Command::TreeLast => self.select(usize::MAX),
            Command::TreeExpand => self.expand_or_enter(),
            Command::TreeCollapse => self.collapse_or_leave(),
            Command::TreeOpen => return self.activate(true, false),
            Command::TreePreview => return self.activate(false, true),
            Command::TreeRefresh => self.refresh(),
            _ => {}
        }
        TreeAction::None
    }

    /// A left click on screen row `y`: selects the entry there, then
    /// expands/collapses a folder or opens a file. A single click previews
    /// the file and keeps focus in the tree; a `double` click keeps the file
    /// open and moves focus to it.
    pub fn click(&mut self, y: u32, double: bool) -> TreeAction {
        let Some(index) = self.row_at(y) else {
            return TreeAction::None;
        };
        if self.sticky().contains(&index) {
            // A folder stuck to the top: scroll back to it, below the
            // folders it's in.
            self.scroll = index.saturating_sub(self.rows[index].depth);
            self.select(index);
            return TreeAction::None;
        }
        self.select(index);
        self.activate(double, !double)
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * WHEEL_ROWS as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
    }

    /// Draws the tree in the columns from `x` to `x + width`. Deep or long
    /// rows are cut off at the right edge.
    pub fn draw(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        frame.with_clip(x, 0, width, self.height as u32, || {
            self.draw_rows(frame, x, width, focused)
        });
    }

    fn draw_rows(&self, frame: &Buffer, x: u32, width: u32, focused: bool) {
        let colors = theme::colors();
        let sticky = self.sticky();
        let visible = (self.scroll..self.rows.len()).skip(sticky.len());
        for (index, y) in visible.zip(sticky.len() as u32..self.height as u32) {
            self.draw_row(frame, index, (x, y, width), None, focused);
        }
        for (y, &index) in sticky.iter().enumerate() {
            self.draw_row(
                frame,
                index,
                (x, y as u32, width),
                Some(colors.surface_inactive),
                focused,
            );
        }
    }

    /// Draws row `index` at (`x`, `y`), `width` wide, over `bg`.
    fn draw_row(
        &self,
        frame: &Buffer,
        index: usize,
        (x, y, width): (u32, u32, u32),
        bg: Option<Rgba>,
        focused: bool,
    ) {
        let colors = theme::colors();
        let row = &self.rows[index];
        let bg = match index == self.selected {
            true if focused => Some(colors.selected),
            true => Some(colors.selected_unfocused),
            false => bg,
        };
        if let Some(bg) = bg {
            frame.fill_rect(x, y, width, 1, bg);
        }
        let indent = x + 1 + 2 * row.depth as u32;
        let open = self.is_open(row);
        if row.is_dir {
            let arrow = if open { "▾" } else { "▸" };
            frame.draw_text(arrow, indent, y, colors.faint, None, Attributes::NONE);
        }
        let mut name_x = indent + 2;
        if icons::enabled() {
            let icon = match row.is_dir {
                true => icons::folder(open),
                false => icons::file(&row.name),
            };
            let dim = row.ignored.then_some(colors.faint);
            name_x = icon.draw(frame, name_x, y, dim);
        }
        let change = match row.is_dir {
            true => None,
            false => self.changes.get(&row.path).copied(),
        };
        let (fg, attributes) = if row.depth == 0 {
            (colors.text, Attributes::BOLD)
        } else if self.active.as_ref() == Some(&row.path) && self.active_preview {
            (colors.accent, Attributes::BOLD | Attributes::ITALIC)
        } else if self.active.as_ref() == Some(&row.path) {
            (colors.accent, Attributes::BOLD)
        } else if let Some(kind) = change {
            (colors.hue(kind.hue()), Attributes::NONE)
        } else if row.ignored {
            (colors.faint, Attributes::NONE)
        } else {
            (colors.text, Attributes::NONE)
        };
        // At the right end: how the file changed, or that a collapsed
        // folder has changes in it.
        let mark = match change {
            Some(kind) => Some((kind.letter(), colors.hue(kind.hue()))),
            None if row.is_dir && !open && self.changed_folders.contains(&row.path) => {
                Some(('•', colors.muted))
            }
            None => None,
        };
        let end = x + width;
        let gap = if mark.is_some() { 3 } else { 1 };
        let room = end.saturating_sub(name_x + gap) as usize;
        frame.draw_text(&truncate(&row.name, room), name_x, y, fg, None, attributes);
        if let Some((mark, fg)) = mark.filter(|_| end >= name_x + 3) {
            let mark = mark.encode_utf8(&mut [0; 4]).to_owned();
            frame.draw_text(&mark, end - 2, y, fg, None, Attributes::NONE);
        }
    }

    // --- navigation -----------------------------------------------------------

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
        // Nor under the folders stuck to the top.
        while self.scroll > 0 && self.selected < self.scroll + self.sticky().len() {
            self.scroll -= 1;
        }
    }

    /// Enter/Space/click: opens the selected file, or expands/collapses a
    /// folder.
    fn activate(&mut self, focus: bool, preview: bool) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        if !row.is_dir {
            return TreeAction::Open {
                path: row.path.clone(),
                focus,
                preview,
            };
        }
        if self.is_open(row) {
            self.collapse(self.selected);
        } else {
            self.expand(self.selected);
        }
        TreeAction::None
    }

    /// Right: expands a collapsed folder, or steps into an expanded one.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if !self.is_open(row) {
            self.expand(self.selected);
        } else if self
            .rows
            .get(self.selected + 1)
            .is_some_and(|next| next.depth > row.depth)
        {
            self.select(self.selected + 1);
        }
    }

    /// Left: collapses an expanded folder, or steps out to the parent.
    fn collapse_or_leave(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if row.is_dir && self.is_open(row) {
            self.collapse(self.selected);
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

    fn expand(&mut self, index: usize) {
        let row = &self.rows[index];
        let (path, depth, root, ignored) = (row.path.clone(), row.depth, row.root, row.ignored);
        self.expanded_under(root).insert(path.clone());
        let mut children = Vec::new();
        self.push_children(&mut children, root, &path, depth + 1, ignored);
        self.rows.splice(index + 1..index + 1, children);
        self.listing += 1;
    }

    fn collapse(&mut self, index: usize) {
        let (depth, root) = (self.rows[index].depth, self.rows[index].root);
        let path = self.rows[index].path.clone();
        self.expanded_under(root).remove(&path);
        let end = self.rows[index + 1..]
            .iter()
            .position(|row| row.depth <= depth)
            .map_or(self.rows.len(), |n| index + 1 + n);
        self.rows.drain(index + 1..end);
        self.listing += 1;
        if self.selected >= end {
            self.selected -= end - (index + 1);
        } else if self.selected > index {
            self.selected = index;
        }
        self.scroll_into_view();
    }

    /// Where `path` is listed under the root at position `root`.
    fn index_of(&self, root: usize, path: &Path) -> Option<usize> {
        self.rows.iter().position(|row| row.root == root && row.path == path)
    }

    /// Whether `row`'s folder is expanded where it's listed.
    fn is_open(&self, row: &Row) -> bool {
        let root = &self.roots[row.root];
        self.expanded.get(root).is_some_and(|open| open.contains(&row.path))
    }

    /// The folders expanded under the root at position `root`.
    fn expanded_under(&mut self, root: usize) -> &mut HashSet<PathBuf> {
        self.expanded.entry(self.roots[root].clone()).or_default()
    }

    /// Appends the entries of `dir`, under the root at position `root`, and
    /// of its expanded subfolders. Those of an `ignored` folder are all
    /// ignored.
    fn push_children(
        &self,
        rows: &mut Vec<Row>,
        root: usize,
        dir: &Path,
        depth: usize,
        ignored: bool,
    ) {
        let expanded = &self.expanded[&self.roots[root]];
        for row in read_dir(dir, depth, root, ignored) {
            let expand = row.is_dir && expanded.contains(&row.path);
            let (path, ignored) = (row.path.clone(), row.ignored);
            rows.push(row);
            if expand {
                self.push_children(rows, root, &path, depth + 1, ignored);
            }
        }
    }
}

/// The entries of `dir`, listed under the root at position `root`, folders
/// first, each group by name ignoring case. An unreadable folder shows as
/// empty. The entries of an `ignored` folder are all ignored.
fn read_dir(dir: &Path, depth: usize, root: usize, ignored: bool) -> Vec<Row> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let kept = if ignored {
        HashSet::new()
    } else {
        not_ignored(dir)
    };
    let mut rows: Vec<Row> = entries
        .filter_map(Result::ok)
        .filter(|entry| !is_hidden(&entry.file_name()))
        .map(|entry| {
            let path = entry.path();
            // Follow symlinks to folders. They're read only when expanded,
            // so a link cycle can't recurse on its own.
            let is_dir = match entry.file_type() {
                Ok(kind) if kind.is_symlink() => path.is_dir(),
                Ok(kind) => kind.is_dir(),
                Err(_) => false,
            };
            Row {
                name: entry.file_name().to_string_lossy().into_owned(),
                ignored: !kept.contains(&path),
                path,
                depth,
                root,
                is_dir,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        (!a.is_dir, a.name.to_lowercase(), &a.name).cmp(&(
            !b.is_dir,
            b.name.to_lowercase(),
            &b.name,
        ))
    });
    rows
}

/// The entries of `dir` that the file picker and workspace search would
/// list: those no ignore file excludes, in `dir` or the folders above it.
fn not_ignored(dir: &Path) -> HashSet<PathBuf> {
    let mut builder = WalkBuilder::new(dir);
    file_index::skip_ignored(&mut builder).max_depth(Some(1));
    builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .map(|entry| entry.into_path())
        .collect()
}

/// `s` cut to `max` characters, ending in an ellipsis if cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory with `files` (paths ending in `/` are folders).
    fn fixture(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-tree-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        for file in files {
            let path = dir.join(file);
            if file.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "").unwrap();
            }
        }
        dir.canonicalize().unwrap()
    }

    /// A tree with room for 20 rows.
    fn tree(roots: &[PathBuf]) -> FileTree {
        let mut tree = FileTree::new(roots);
        tree.set_height(20);
        tree
    }

    /// The visible rows, indented by depth, folders marked with `/`.
    fn listing(tree: &FileTree) -> Vec<String> {
        tree.rows
            .iter()
            .map(|row| {
                let slash = if row.is_dir { "/" } else { "" };
                format!("{}{}{slash}", "  ".repeat(row.depth), row.name)
            })
            .collect()
    }

    fn selected(tree: &FileTree) -> &str {
        &tree.rows[tree.selected].name
    }

    #[test]
    fn tells_which_changes_change_the_listing() {
        let root = fixture("changed-by", &["a.txt", "shut/b.txt"]);
        let tree = tree(std::slice::from_ref(&root));
        let changed = |path: &PathBuf| tree.is_changed_by([path]);
        let a = root.join("a.txt");
        assert!(!changed(&a), "there, and listed: changed within");
        fs::write(root.join("new.txt"), "").unwrap();
        assert!(changed(&root.join("new.txt")), "came");
        assert!(!changed(&root.join("gone.txt")), "came and went");
        fs::remove_file(&a).unwrap();
        assert!(changed(&a), "went");
        fs::create_dir(&a).unwrap();
        assert!(changed(&a), "became a folder");
        fs::write(root.join("shut/c.txt"), "").unwrap();
        assert!(!changed(&root.join("shut/c.txt")), "in a closed folder");
        fs::write(root.join(".DS_Store"), "").unwrap();
        assert!(!changed(&root.join(".DS_Store")), "never listed");
        assert!(changed(&root.join(".gitignore")), "may ignore others");
    }

    #[test]
    fn lists_folders_first_and_hides_git() {
        let root = fixture(
            "order",
            &[
                "b.txt",
                "A.txt",
                "src/main.rs",
                ".git/HEAD",
                "docs/",
                ".env",
            ],
        );
        let tree = tree(&[root]);
        assert_eq!(
            listing(&tree),
            ["order/", "  docs/", "  src/", "  .env", "  A.txt", "  b.txt"]
        );
    }

    #[test]
    fn marks_ignored_entries_and_their_contents() {
        let root = fixture(
            "ignored",
            &[
                ".gitignore",
                "target/debug/out",
                "src/main.rs",
                "src/gen.rs",
                "src/.gitignore",
                "app.log",
            ],
        );
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("src/.gitignore"), "gen.rs\n").unwrap();
        let mut tree = tree(std::slice::from_ref(&root));
        tree.reveal(&root.join("target/debug/out"));
        tree.reveal(&root.join("src/main.rs"));
        let ignored: Vec<String> = tree
            .rows
            .iter()
            .filter(|row| row.ignored)
            .map(|row| row.name.clone())
            .collect();
        assert_eq!(ignored, ["gen.rs", "target", "debug", "out", "app.log"]);
    }

    #[test]
    fn expanding_and_collapsing_with_the_keyboard() {
        let root = fixture("keys", &["src/tree/mod.rs", "src/main.rs", "z.txt"]);
        let mut tree = tree(&[root]);
        tree.run(Command::TreeDown);
        assert_eq!(selected(&tree), "src");
        tree.run(Command::TreeExpand);
        tree.run(Command::TreeExpand);
        assert_eq!(
            selected(&tree),
            "tree",
            "Right steps into an expanded folder"
        );
        tree.run(Command::TreeExpand);
        assert_eq!(
            listing(&tree),
            [
                "keys/",
                "  src/",
                "    tree/",
                "      mod.rs",
                "    main.rs",
                "  z.txt"
            ]
        );

        tree.run(Command::TreeDown);
        tree.run(Command::TreeCollapse);
        assert_eq!(selected(&tree), "tree", "Left steps out to the parent");
        tree.run(Command::TreeUp);
        tree.run(Command::TreeCollapse);
        assert_eq!(listing(&tree), ["keys/", "  src/", "  z.txt"]);

        tree.run(Command::TreeExpand);
        assert!(
            listing(&tree).contains(&"      mod.rs".to_string()),
            "subfolders stay expanded"
        );
    }

    #[test]
    fn opening_files_and_toggling_folders() {
        let root = fixture("open", &["dir/inner.txt", "file.txt"]);
        let mut tree = tree(std::slice::from_ref(&root));
        tree.run(Command::TreeLast);
        assert_eq!(
            tree.run(Command::TreeOpen),
            TreeAction::Open {
                path: root.join("file.txt"),
                focus: true,
                preview: false,
            }
        );
        assert_eq!(
            tree.run(Command::TreePreview),
            TreeAction::Open {
                path: root.join("file.txt"),
                focus: false,
                preview: true,
            }
        );
        // Clicking a folder toggles it.
        assert_eq!(tree.click(1, false), TreeAction::None);
        assert_eq!(
            listing(&tree),
            ["open/", "  dir/", "    inner.txt", "  file.txt"]
        );
        tree.click(1, false);
        assert_eq!(listing(&tree), ["open/", "  dir/", "  file.txt"]);
        assert_eq!(
            tree.click(10, false),
            TreeAction::None,
            "below the last row"
        );
    }

    #[test]
    fn collapsing_the_parent_of_the_selection_selects_the_parent() {
        let root = fixture("collapse", &["dir/a", "dir/b", "last"]);
        let mut tree = tree(&[root]);
        tree.click(1, false);
        tree.select(3);
        assert_eq!(selected(&tree), "b");
        tree.click(1, false);
        assert_eq!(selected(&tree), "dir");
        tree.select(2);
        assert_eq!(selected(&tree), "last");
    }

    #[test]
    fn reveal_expands_ancestors_and_refresh_sees_new_files() {
        let root = fixture("reveal", &["a/b/c.rs", "a/other.rs"]);
        let mut tree = tree(std::slice::from_ref(&root));
        tree.set_height(3);
        tree.reveal(&root.join("a/b/c.rs"));
        assert_eq!(selected(&tree), "c.rs");
        assert_eq!(tree.scroll, 1, "scrolled so the selection is visible");

        fs::write(root.join("a/b/new.rs"), "").unwrap();
        tree.refresh();
        assert_eq!(selected(&tree), "c.rs");
        assert!(listing(&tree).contains(&"      new.rs".to_string()));
    }

    #[test]
    fn a_new_active_file_is_revealed() {
        let root = fixture("active", &["a/b/c.rs", "a/d.rs", "e.rs"]);
        let mut tree = tree(std::slice::from_ref(&root));
        tree.set_active(Some(&root.join("a/b/c.rs")), true);
        assert_eq!(selected(&tree), "c.rs");

        // The same file again leaves the tree as the user left it.
        tree.run(Command::TreeFirst);
        tree.run(Command::TreeDown);
        tree.run(Command::TreeCollapse);
        tree.set_active(Some(&root.join("a/b/c.rs")), false);
        assert_eq!(listing(&tree), ["active/", "  a/", "  e.rs"]);

        tree.set_active(None, false);
        tree.set_active(Some(&root.join("a/b/c.rs")), false);
        assert_eq!(selected(&tree), "c.rs");
    }

    #[test]
    fn several_roots() {
        let one = fixture("one", &["x"]);
        let two = fixture("two", &["y"]);
        let tree = tree(&[one, two]);
        assert_eq!(listing(&tree), ["one/", "  x", "two/", "  y"]);
    }

    #[test]
    fn a_root_inside_another_is_expanded_apart_and_owns_its_files() {
        let outer = fixture("outer", &[".gitignore", "pkg/src/lib.rs", "top.rs"]);
        fs::write(outer.join(".gitignore"), "pkg/\n").unwrap();
        let inner = outer.join("pkg");
        let mut tree = tree(&[outer.clone(), inner.clone()]);
        assert_eq!(
            listing(&tree),
            ["outer/", "  pkg/", "  .gitignore", "  top.rs", "pkg/", "  src/"]
        );
        let ignored = |tree: &FileTree| -> Vec<(usize, String)> {
            let rows = tree.rows.iter().filter(|row| row.ignored);
            rows.map(|row| (row.root, row.name.clone())).collect()
        };
        assert_eq!(ignored(&tree), [(0, "pkg".to_string())], "only in outer");

        // Revealed under the root it belongs to, not in outer's listing.
        tree.reveal(&inner.join("src/lib.rs"));
        assert_eq!(tree.rows[tree.selected].root, 1);
        assert_eq!(selected(&tree), "lib.rs");
        assert_eq!(listing(&tree)[1], "  pkg/", "outer's pkg stays shut");
        let entry = tree.selected().unwrap();
        assert!(!entry.is_root);

        // Outer's copy of the root is a root too, for the file commands.
        tree.select(1);
        assert!(tree.selected().unwrap().is_root);
        tree.run(Command::TreeExpand);
        assert_eq!(listing(&tree)[..3], ["outer/", "  pkg/", "    src/"]);
        tree.refresh();
        assert_eq!(selected(&tree), "pkg");
        assert_eq!(tree.rows[tree.selected].root, 0, "the selection stays put");

        tree.set_roots(std::slice::from_ref(&outer));
        assert_eq!(listing(&tree)[..3], ["outer/", "  pkg/", "    src/"]);
        assert_eq!(listing(&tree).len(), 5);
    }

    #[test]
    fn roots_with_the_same_name_are_told_apart() {
        let root = fixture("same-name", &["a/src/x.rs", "b/src/y.rs"]);
        let tree = tree(&[root.join("a/src"), root.join("b/src")]);
        assert_eq!(listing(&tree), ["a/src/", "  x.rs", "b/src/", "  y.rs"]);
    }

    #[test]
    fn deep_rows_stay_inside_the_tree() {
        let _serial = crate::test_serial();
        let root = fixture("deep", &["a/b/c/d/e/f/g/h/file.txt"]);
        let mut tree = tree(std::slice::from_ref(&root));
        tree.reveal(&root.join("a/b/c/d/e/f/g/h/file.txt"));
        let screen =
            opentui::OwnedBuffer::new(16, 12, false, opentui::WidthMethod::Unicode, "test")
                .unwrap();
        screen.clear(Rgba::BLACK);
        tree.draw(&screen, 0, 12, true);
        let text = screen.to_text(true);
        for line in text.lines() {
            let outside: String = line.chars().skip(12).collect();
            assert_eq!(outside.trim(), "", "{text}");
        }
        assert_eq!(truncate("name", 0), "");
    }

    #[test]
    fn draws_indented_rows_and_truncates() {
        let _serial = crate::test_serial();
        let root = fixture("draw", &["folder/", "a-very-long-file-name.txt"]);
        let mut tree = tree(&[root]);
        tree.set_height(4);
        let screen =
            opentui::OwnedBuffer::new(16, 4, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        tree.draw(&screen, 0, 16, true);
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(lines[..3], [" ▾ draw", "   ▸ folder", "     a-very-lo…"]);
    }

    /// The tree's rows as drawn, `width` columns wide.
    fn drawn(tree: &FileTree, width: u32) -> Vec<String> {
        let height = tree.height as u32;
        let screen =
            opentui::OwnedBuffer::new(width, height, false, opentui::WidthMethod::Unicode, "test")
                .unwrap();
        screen.clear(Rgba::BLACK);
        tree.draw(&screen, 0, width, true);
        let text = screen.to_text(true);
        text.lines().map(|l| l.trim_end().to_string()).collect()
    }

    #[test]
    fn the_folders_scrolled_past_stick_to_the_top() {
        let _serial = crate::test_serial();
        let files: Vec<String> = (0..8)
            .map(|i| format!("a/b/{i}.rs"))
            .chain(["a/z.rs".to_string()])
            .chain((1..=4).map(|i| format!("m{i}.txt")))
            .collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        let root = fixture("sticky", &files);
        let mut tree = tree(std::slice::from_ref(&root));
        tree.reveal(&root.join("a/b/0.rs"));
        tree.set_height(6);
        tree.run(Command::TreeFirst);
        assert!(tree.sticky().is_empty(), "nothing scrolled past");

        // Scrolled 3 rows down: sticky, a, and b stick, over 0.rs to 2.rs.
        tree.scroll(1);
        assert_eq!(tree.scroll, 3);
        assert_eq!(
            drawn(&tree, 16),
            [
                " ▾ sticky",
                "   ▾ a",
                "     ▾ b",
                "         3.rs",
                "         4.rs",
                "         5.rs"
            ]
        );
        // Clicking a stuck folder scrolls back to it.
        assert_eq!(tree.click(2, false), TreeAction::None);
        assert_eq!(selected(&tree), "b");
        assert_eq!(tree.scroll, 0);
        assert_eq!(drawn(&tree, 16)[2], "     ▾ b", "not collapsed");

        // Past b's files, only the folders the top rows are in stick.
        tree.scroll(3);
        assert_eq!(
            drawn(&tree, 16),
            [
                " ▾ sticky",
                "   ▾ a",
                "       z.rs",
                "     m1.txt",
                "     m2.txt",
                "     m3.txt"
            ]
        );
        tree.scroll(1);
        assert_eq!(
            drawn(&tree, 16),
            [
                " ▾ sticky",
                "       z.rs",
                "     m1.txt",
                "     m2.txt",
                "     m3.txt",
                "     m4.txt"
            ]
        );

        // Stepping up from the first row below them scrolls, rather than
        // selecting a row they cover.
        tree.run(Command::TreeFirst);
        tree.scroll(1);
        tree.select_at(3);
        assert_eq!(selected(&tree), "3.rs");
        tree.run(Command::TreeUp);
        assert_eq!(selected(&tree), "2.rs");
        assert_eq!(tree.selected_position().map(|(_, y)| y), Some(3));
        assert_eq!(drawn(&tree, 16)[3], "         2.rs");
    }

    #[test]
    fn draws_icons_before_names_when_enabled() {
        let _serial = crate::test_serial();
        crate::icons::enable();
        let root = fixture("icons", &["folder/", "main.rs"]);
        let mut tree = tree(&[root]);
        tree.set_height(3);
        let screen =
            opentui::OwnedBuffer::new(16, 3, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        tree.draw(&screen, 0, 16, true);
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(
            lines,
            [
                " ▾ \u{e5fe} icons",
                "   ▸ \u{e5ff} folder",
                "     \u{e7a8} main.rs"
            ]
        );
        tree.select(2);
        assert_eq!(tree.selected_position(), Some((7, 2)));
    }
}
