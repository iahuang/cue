//! The file tree: the workspace roots and whichever folders are expanded,
//! shown as an indented list.
//!
//! Folders are read when they are expanded and re-read on refresh; nothing
//! watches the file system yet. Which folders are expanded is remembered by
//! path, so collapsing a folder and expanding it again restores its subfolders.
//! What `.gitignore` and `.ignore` files exclude is shown dimmed, and left
//! out of the file picker and workspace search.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use opentui::{Attributes, Buffer, Rgba};

use crate::file_index;
use crate::keymap::Command;
use crate::workspace::{deepest_root, root_name};

const FG: Rgba = Rgba::rgb(186, 194, 222);
const ROOT_FG: Rgba = Rgba::rgb(205, 214, 244);
const ACTIVE_FG: Rgba = Rgba::rgb(137, 180, 250);
const ARROW_FG: Rgba = Rgba::rgb(108, 112, 134);
/// Ignored entries.
const IGNORED_FG: Rgba = Rgba::rgb(108, 112, 134);
const SELECTED_BG: Rgba = Rgba::rgb(69, 71, 110);
/// The selection while the editor has focus.
const SELECTED_BG_UNFOCUSED: Rgba = Rgba::rgb(49, 50, 68);

/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

/// Entries never shown, here or in the file picker and workspace search.
const HIDDEN: &[&str] = &[".git", ".DS_Store"];

/// Whether an entry named `name` is never shown.
pub fn is_hidden(name: &std::ffi::OsStr) -> bool {
    HIDDEN.iter().any(|&hidden| name == hidden)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: PathBuf,
    name: String,
    /// 0 for workspace roots.
    depth: usize,
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
    /// A workspace root, which isn't renamed, moved, or deleted from here.
    pub is_root: bool,
}

pub struct FileTree {
    roots: Vec<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// Every visible entry, top to bottom.
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
}

impl FileTree {
    /// A tree of `roots`, each expanded.
    pub fn new(roots: &[PathBuf]) -> FileTree {
        let mut tree = FileTree {
            roots: Vec::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            height: 1,
            active: None,
            active_preview: false,
        };
        tree.set_roots(roots);
        tree
    }

    /// Shows `roots`; new ones start expanded.
    pub fn set_roots(&mut self, roots: &[PathBuf]) {
        for root in roots {
            if !self.roots.contains(root) {
                self.expanded.insert(root.clone());
            }
        }
        self.roots = roots.to_vec();
        self.refresh();
    }

    /// Re-reads every expanded folder, keeping the selection on the same
    /// path when it still exists.
    pub fn refresh(&mut self) {
        let selected = self.rows.get(self.selected).map(|row| row.path.clone());
        let mut rows = Vec::new();
        for root in &self.roots {
            rows.push(Row {
                path: root.clone(),
                name: root_name(root),
                depth: 0,
                is_dir: true,
                ignored: false,
            });
            if self.expanded.contains(root) {
                self.push_children(&mut rows, root, 1, false);
            }
        }
        self.rows = rows;
        if let Some(index) = selected.and_then(|path| self.index_of(&path)) {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.scroll_into_view();
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

    /// The selected folder, or the folder of the selected file.
    pub fn selected_folder(&self) -> Option<PathBuf> {
        let row = self.rows.get(self.selected)?;
        match row.is_dir {
            true => Some(row.path.clone()),
            false => row.path.parent().map(Path::to_path_buf),
        }
    }

    /// The selected entry, if there are any.
    pub fn selected(&self) -> Option<Entry> {
        self.rows.get(self.selected).map(|row| Entry {
            path: row.path.clone(),
            is_dir: row.is_dir,
            is_root: row.depth == 0,
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
        let y = self.selected.checked_sub(self.scroll)?;
        (y < self.height).then_some((3 + 2 * row.depth as u32, y as u32))
    }

    /// Selects the entry on screen row `y`, as a right click does, without
    /// opening it. Returns false if there's none there.
    pub fn select_at(&mut self, y: u32) -> bool {
        let index = self.scroll + y as usize;
        if index >= self.rows.len() {
            return false;
        }
        self.select(index);
        true
    }

    /// Follows `from`, and what's in it, to `to`: folders expanded there
    /// stay expanded. Refreshes and selects `to`.
    pub fn moved(&mut self, from: &Path, to: &Path) {
        let expanded = std::mem::take(&mut self.expanded);
        self.expanded = expanded
            .into_iter()
            .map(|path| match path.strip_prefix(from) {
                Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
                Ok(rest) => to.join(rest),
                Err(_) => path,
            })
            .collect();
        self.refresh();
        self.reveal(to);
    }

    #[cfg(test)]
    pub fn active_is_preview(&self) -> bool {
        self.active_preview
    }

    /// Expands the folders down to `path` and selects it, if it is in the
    /// workspace. Re-reads the tree only if it had to expand a folder or
    /// doesn't list `path` yet.
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = deepest_root(&self.roots, path) else {
            return;
        };
        let folders: Vec<PathBuf> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(root))
            .map(Path::to_path_buf)
            .collect();
        let collapsed = folders.iter().any(|dir| !self.expanded.contains(dir));
        self.expanded.extend(folders);
        if collapsed || self.index_of(path).is_none() {
            self.refresh();
        }
        if let Some(index) = self.index_of(path) {
            self.select(index);
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
        let index = self.scroll + y as usize;
        if index >= self.rows.len() {
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
        let visible = self.rows.iter().enumerate().skip(self.scroll);
        for ((index, row), y) in visible.zip(0..self.height as u32) {
            if index == self.selected {
                let bg = if focused {
                    SELECTED_BG
                } else {
                    SELECTED_BG_UNFOCUSED
                };
                frame.fill_rect(x, y, width, 1, bg);
            }
            let indent = x + 1 + 2 * row.depth as u32;
            let name_x = indent + 2;
            if row.is_dir {
                let arrow = if self.expanded.contains(&row.path) {
                    "▾"
                } else {
                    "▸"
                };
                frame.draw_text(arrow, indent, y, ARROW_FG, None, Attributes::NONE);
            }
            let (fg, attributes) = if row.depth == 0 {
                (ROOT_FG, Attributes::BOLD)
            } else if self.active.as_ref() == Some(&row.path) && self.active_preview {
                (ACTIVE_FG, Attributes::BOLD | Attributes::ITALIC)
            } else if self.active.as_ref() == Some(&row.path) {
                (ACTIVE_FG, Attributes::BOLD)
            } else if row.ignored {
                (IGNORED_FG, Attributes::NONE)
            } else {
                (FG, Attributes::NONE)
            };
            let room = (x + width).saturating_sub(name_x + 1) as usize;
            frame.draw_text(&truncate(&row.name, room), name_x, y, fg, None, attributes);
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
        if self.expanded.contains(&row.path) {
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
        if !self.expanded.contains(&row.path) {
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
        if row.is_dir && self.expanded.contains(&row.path) {
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
        let (path, depth, ignored) = (row.path.clone(), row.depth, row.ignored);
        self.expanded.insert(path.clone());
        let mut children = Vec::new();
        self.push_children(&mut children, &path, depth + 1, ignored);
        self.rows.splice(index + 1..index + 1, children);
    }

    fn collapse(&mut self, index: usize) {
        let depth = self.rows[index].depth;
        self.expanded.remove(&self.rows[index].path);
        let end = self.rows[index + 1..]
            .iter()
            .position(|row| row.depth <= depth)
            .map_or(self.rows.len(), |n| index + 1 + n);
        self.rows.drain(index + 1..end);
        if self.selected >= end {
            self.selected -= end - (index + 1);
        } else if self.selected > index {
            self.selected = index;
        }
        self.scroll_into_view();
    }

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.rows.iter().position(|row| row.path == path)
    }

    /// Appends the entries of `dir`, and of its expanded subfolders. Those
    /// of an `ignored` folder are all ignored.
    fn push_children(&self, rows: &mut Vec<Row>, dir: &Path, depth: usize, ignored: bool) {
        for row in read_dir(dir, depth, ignored) {
            let expand = row.is_dir && self.expanded.contains(&row.path);
            let (path, ignored) = (row.path.clone(), row.ignored);
            rows.push(row);
            if expand {
                self.push_children(rows, &path, depth + 1, ignored);
            }
        }
    }
}

/// The entries of `dir`, folders first, each group by name ignoring case.
/// An unreadable folder shows as empty. The entries of an `ignored` folder
/// are all ignored.
fn read_dir(dir: &Path, depth: usize, ignored: bool) -> Vec<Row> {
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
fn truncate(s: &str, max: usize) -> String {
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
            .join(format!("qedit-tree-{}", std::process::id()))
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
}
