//! The changes view: the files git says changed since the last commit,
//! shown in the sidebar in place of the file tree.
//!
//! Each repository the workspace's folders are in is listed, with the
//! branch it's on, and under it its changed files in their folders, each
//! marked with how it changed. A folder holding only another folder shows
//! as one row, `src/git`, as VS Code's compact folders do. Keys and the
//! mouse work as in the tree: a click previews a file, and a double click
//! or Enter opens it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use opentui::{Attributes, Buffer};

use crate::git::{Kind, Repo};
use crate::icons;
use crate::keymap::Command;
use crate::theme;
use crate::tree::{truncate, Entry, TreeAction};
use crate::workspace::root_names;

/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

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
    /// A repository, and the branch it's on.
    Repo(String),
    Folder,
    File(Kind),
}

/// The folders and files under a folder, each by name ignoring case, then
/// by name.
#[derive(Default)]
struct Node {
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
        }
    }

    /// Lists the changes in `repos`, keeping the selection on the same
    /// entry while it's still listed.
    pub fn set_repos(&mut self, repos: &[Repo]) {
        self.repos = repos.to_vec();
        let roots: Vec<PathBuf> = repos.iter().map(|repo| repo.root.clone()).collect();
        self.names = root_names(&roots);
        self.rebuild();
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
        self.rows.get(self.selected).map(|row| Entry {
            path: row.path.clone(),
            is_dir: !matches!(row.what, What::File(_)),
            is_root: matches!(row.what, What::Repo(_)),
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
        if index >= self.rows.len() {
            return false;
        }
        self.select(index);
        true
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
            _ => {}
        }
        TreeAction::None
    }

    /// A left click on screen row `y`: selects the entry there, then
    /// collapses or expands a folder, or opens a file: a single click
    /// previews it and keeps focus here; a `double` click keeps it open and
    /// moves focus to it.
    pub fn click(&mut self, y: u32, double: bool) -> TreeAction {
        if !self.select_at(y) {
            return TreeAction::None;
        }
        self.activate(double, !double)
    }

    /// Scrolls by `rows` without moving the selection.
    pub fn scroll(&mut self, rows: isize) {
        let rows = rows * WHEEL_ROWS as isize;
        let max = self.rows.len().saturating_sub(self.height);
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
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
        if index == self.selected {
            let bg = match focused {
                true => colors.selected,
                false => colors.selected_unfocused,
            };
            frame.fill_rect(x, y, width, 1, bg);
        }
        let indent = x + 1 + 2 * row.depth as u32;
        let open = !self.collapsed.contains(&row.path);
        if !matches!(row.what, What::File(_)) {
            let arrow = if open { "▾" } else { "▸" };
            frame.draw_text(arrow, indent, y, colors.faint, None, Attributes::NONE);
        }
        let mut name_x = indent + 2;
        if icons::enabled() {
            let icon = match row.what {
                What::File(_) => icons::file(&row.name),
                _ => icons::folder(open),
            };
            name_x = icon.draw(frame, name_x, y, None);
        }
        // What goes at the right end: the branch, or how a file changed.
        let (right, right_fg) = match &row.what {
            What::Repo(branch) => {
                let most = (width / 2).saturating_sub(2) as usize;
                (truncate(branch, most), colors.muted)
            }
            What::File(kind) => (kind.letter().to_string(), colors.hue(kind.hue())),
            What::Folder => (String::new(), colors.muted),
        };
        let right_width = right.chars().count() as u32;
        let end = x + width;
        let right_x = end.saturating_sub(right_width + 1);
        let active = self.active.as_ref() == Some(&row.path);
        let (fg, mut attributes) = match &row.what {
            What::Repo(_) => (colors.text, Attributes::BOLD),
            _ if active && self.active_preview => {
                (colors.accent, Attributes::BOLD | Attributes::ITALIC)
            }
            _ if active => (colors.accent, Attributes::BOLD),
            What::Folder => (colors.text, Attributes::NONE),
            What::File(kind) => (colors.hue(kind.hue()), Attributes::NONE),
        };
        if row.what == What::File(Kind::Deleted) {
            attributes |= Attributes::STRIKETHROUGH;
        }
        let gap = if right_width > 0 { right_width + 2 } else { 1 };
        let room = end.saturating_sub(name_x + gap) as usize;
        frame.draw_text(&truncate(&row.name, room), name_x, y, fg, None, attributes);
        if right_width > 0 && right_x > name_x {
            frame.draw_text(&right, right_x, y, right_fg, None, Attributes::NONE);
        }
    }

    // --- rows ------------------------------------------------------------------

    /// Lists the rows again, from the repositories and what's collapsed.
    fn rebuild(&mut self) {
        let selected = self.rows.get(self.selected).map(|row| row.path.clone());
        let mut rows = Vec::new();
        for (repo, name) in self.repos.iter().zip(&self.names) {
            rows.push(Row {
                path: repo.root.clone(),
                name: name.clone(),
                depth: 0,
                what: What::Repo(repo.head.name().to_string()),
            });
            if self.collapsed.contains(&repo.root) {
                continue;
            }
            let mut node = Node::default();
            for change in &repo.changes {
                if let Ok(rest) = change.path.strip_prefix(&repo.root) {
                    node.insert(rest, &change.path, change.kind);
                }
            }
            self.push_node(&mut rows, &node, &repo.root, 1);
        }
        self.rows = rows;
        if let Some(index) = selected.and_then(|path| self.rows.iter().position(|r| r.path == path))
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.scroll_into_view();
    }

    /// Appends the rows of `node`, the folder `dir`, at `depth`: its
    /// folders, each with what's in it unless it's collapsed, then its
    /// files.
    fn push_node(&self, rows: &mut Vec<Row>, node: &Node, dir: &Path, depth: usize) {
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
            let collapsed = self.collapsed.contains(&path);
            rows.push(Row {
                path: path.clone(),
                name,
                depth,
                what: What::Folder,
            });
            if !collapsed {
                self.push_node(rows, folder, &path, depth + 1);
            }
        }
        for ((_, name), (path, kind)) in &node.files {
            rows.push(Row {
                path: path.clone(),
                name: name.clone(),
                depth,
                what: What::File(*kind),
            });
        }
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

    /// Enter/Space/click: opens the selected file, or collapses or expands
    /// a folder.
    fn activate(&mut self, focus: bool, preview: bool) -> TreeAction {
        let Some(row) = self.rows.get(self.selected) else {
            return TreeAction::None;
        };
        if let What::File(_) = row.what {
            return TreeAction::Open {
                path: row.path.clone(),
                focus,
                preview,
            };
        }
        let path = row.path.clone();
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        self.rebuild();
        TreeAction::None
    }

    /// Right: expands a collapsed folder, or steps into an expanded one.
    fn expand_or_enter(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if let What::File(_) = row.what {
            return;
        }
        if self.collapsed.remove(&row.path) {
            self.rebuild();
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
        if !matches!(row.what, What::File(_)) && !self.collapsed.contains(&row.path) {
            self.collapsed.insert(row.path.clone());
            self.rebuild();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Change, Head};
    use opentui::Rgba;

    fn repo(root: &str, changes: &[(&str, Kind)]) -> Repo {
        let root = PathBuf::from(root);
        Repo {
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: None,
            changes: changes
                .iter()
                .map(|&(path, kind)| Change {
                    path: root.join(path),
                    kind,
                    from: None,
                })
                .collect(),
            added: 0,
            removed: 0,
            root,
        }
    }

    fn view(repos: &[Repo]) -> ChangesView {
        let mut view = ChangesView::new();
        view.set_height(20);
        view.set_repos(repos);
        view
    }

    /// The rows, indented by depth, folders marked with `/` and files with
    /// how they changed.
    fn listing(view: &ChangesView) -> Vec<String> {
        view.rows
            .iter()
            .map(|row| {
                let mark = match &row.what {
                    What::Repo(branch) => format!(" ({branch})"),
                    What::Folder => "/".to_string(),
                    What::File(kind) => format!(" {}", kind.letter()),
                };
                format!("{}{}{mark}", "  ".repeat(row.depth), row.name)
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
                "cue (main)",
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
        view.select(1);
        view.run(Command::TreeCollapse);
        assert_eq!(listing(&view), ["cue (main)", "  src/", "  z.rs M"]);
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
        assert_eq!(view.click(1, false), open(false, true));
        assert_eq!(view.click(1, true), open(true, false));
        assert_eq!(view.click(5, false), TreeAction::None, "below the rows");
        assert_eq!(
            view.click(0, false),
            TreeAction::None,
            "a repository collapses"
        );
        assert_eq!(listing(&view), ["cue (main)"]);
    }

    #[test]
    fn draws_the_branch_and_how_files_changed() {
        let _serial = crate::test_serial();
        let mut view = view(&[repo(
            "/w/cue",
            &[("a.rs", Kind::Modified), ("a-long-name.rs", Kind::Added)],
        )]);
        view.set_height(4);
        let screen =
            opentui::OwnedBuffer::new(20, 4, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        view.draw(&screen, 0, 20, true);
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(
            lines[..3],
            [
                " ▾ cue         main",
                "     a-long-name… A",
                "     a.rs         M"
            ]
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
