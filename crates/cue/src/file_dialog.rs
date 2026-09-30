//! The file dialog: a popup for choosing a file to open (Ctrl+O), where to
//! save a file (Ctrl+Shift+S, or saving a new one), or a new file to create
//! (Ctrl+Alt+N).
//!
//! As in the dialogs of macOS and Windows, it lists a folder, folders first,
//! and you go through folders to the file. As in VS Code's simple file
//! dialog, one line of text drives it: a path, whose folder is the one
//! listed, and whose name, after the last `/`, narrows the list as you type
//! it, fuzzily, as the file picker does.
//!
//! Typing `/` after a folder's name goes into it, and Backspace right after
//! a `/`, or Alt+Up, goes back up. Tab completes the selected name. A click
//! goes into a folder or picks a file, and a click on a folder in the path
//! goes back to it.
//!
//! Saving and creating take a path that doesn't exist yet, folders
//! included, which the app creates. Saving over another file asks first.
//!
//! From the file tree, it also names a new folder, where to rename or move
//! a file or folder, and where to duplicate one. None of those replace
//! anything already there.
//!
//! Adding a folder to the workspace, it lists only folders, and Enter takes
//! the folder listed once nothing's typed after the last `/`.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use opentui::{Attributes, Buffer, Rgba};

use crate::document;
use crate::icons;
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::{Command, Keymap};
use crate::line_edit::{Caret, Edit};
use crate::picker::{self, Area, DIM, FG, MATCH_FG, SELECTED_BG};
use crate::tree;

const ERROR: Rgba = Rgba::rgb(243, 139, 168);

/// The widest the popup gets, in columns.
const MAX_WIDTH: u32 = 90;
/// The most entries shown at once.
const MAX_ROWS: u32 = 16;
/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

/// What the dialog chooses a path for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A file that exists, to open.
    Open,
    /// Where to save the file in the active panel.
    SaveAs,
    /// A file that doesn't exist yet, to create.
    Create,
    /// A folder that doesn't exist yet, to create.
    CreateFolder,
    /// Where to rename or move the dialog's current file or folder.
    Move,
    /// Where to copy the dialog's current file or folder.
    Duplicate,
    /// A folder that exists, to add to the workspace.
    AddFolder,
}

impl Purpose {
    fn title(self) -> &'static str {
        match self {
            Purpose::Open => "Open File",
            Purpose::SaveAs => "Save As",
            Purpose::Create => "New File",
            Purpose::CreateFolder => "New Folder",
            Purpose::Move => "Rename or Move",
            Purpose::Duplicate => "Duplicate",
            Purpose::AddFolder => "Add Folder to Workspace",
        }
    }

    /// Whether the path chosen may not exist yet.
    fn names_a_new_file(self) -> bool {
        !matches!(self, Purpose::Open | Purpose::AddFolder)
    }
}

/// What the app should do after the dialog handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum DialogAction {
    Continue,
    Close,
    /// This path was chosen: absolute, with no `.` or `..` in it.
    Accept(PathBuf),
}

/// Something in the folder listed.
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    is_dir: bool,
    /// A file's size, in bytes.
    size: Option<u64>,
}

/// A row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// The folder above.
    Parent,
    /// An entry, by position.
    Entry(usize),
}

/// The folder listed.
struct Listing {
    dir: PathBuf,
    /// Why it couldn't be read, if it couldn't.
    error: Option<io::ErrorKind>,
    entries: Vec<Entry>,
}

pub struct FileDialog {
    purpose: Purpose,
    /// The path, as typed.
    text: String,
    caret: Caret,
    /// What relative paths are relative to: the folder it opened in.
    base: PathBuf,
    /// The file being saved, which saving over again doesn't ask about, or
    /// the file or folder being moved or duplicated.
    current: Option<PathBuf>,
    /// The name in the path is the one suggested, and selected: typing
    /// replaces it, and the whole folder is listed rather than narrowed to
    /// it.
    name_selected: bool,
    listing: Listing,
    rows: Vec<Row>,
    /// The row Enter takes, if any. Opening, it's the best match; saving
    /// or creating, Enter takes the path typed unless a row is picked.
    selected: Option<usize>,
    /// The first row on screen.
    scroll: usize,
    /// Enter would save over this file; Enter again does.
    confirm: Option<PathBuf>,
    /// Shown in the bottom border until the next input: text, and whether
    /// it's an error.
    message: Option<(String, bool)>,
    atom: Option<Atom>,
    matcher: RefCell<Matcher>,
    /// Shortcut hints for the bottom border.
    hints: String,
    screen_width: u32,
    screen_height: u32,
}

impl FileDialog {
    /// A dialog for `purpose` over a screen `width` x `height`, listing
    /// `dir`, with `name` suggested. Saving over `current` doesn't ask.
    pub fn new(
        purpose: Purpose,
        dir: &Path,
        name: &str,
        current: Option<PathBuf>,
        keymap: &Keymap,
        width: u32,
        height: u32,
    ) -> FileDialog {
        let add = (purpose == Purpose::AddFolder).then_some((Command::PickerAccept, "add"));
        let hints = add
            .iter()
            .chain(&[
                (Command::DialogComplete, "complete"),
                (Command::DialogParent, "parent folder"),
            ])
        .filter_map(|&(command, name)| Some(format!("{} {name}", keymap.shortcut(command)?)))
        .collect::<Vec<_>>()
        .join(" · ");
        let dir = normalize(dir);
        let mut dialog = FileDialog {
            purpose,
            text: format!("{}{name}", display_dir(&dir)),
            caret: Caret::default(),
            base: dir.clone(),
            current,
            name_selected: !name.is_empty(),
            listing: Listing::read(&dir, purpose),
            rows: Vec::new(),
            selected: None,
            scroll: 0,
            confirm: None,
            message: None,
            atom: None,
            matcher: RefCell::new(Matcher::new(Config::DEFAULT.match_paths())),
            hints: format!(" {hints} "),
            screen_width: width,
            screen_height: height,
        };
        dialog.refilter();
        dialog
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// The file being saved, moved, or duplicated.
    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        self.scroll_into_view();
    }

    /// Shows `text` as an error, as when the path chosen couldn't be used.
    pub fn show_error(&mut self, text: String) {
        self.message = Some((text, true));
    }

    /// Runs a picker or dialog command.
    pub fn run(&mut self, command: Command) -> DialogAction {
        self.message = None;
        let page = self.rows().saturating_sub(1).max(1) as isize;
        match command {
            Command::PickerUp => self.step(-1),
            Command::PickerDown => self.step(1),
            Command::PickerPageUp => self.step(-page),
            Command::PickerPageDown => self.step(page),
            Command::PickerAccept => return self.accept(),
            Command::PickerClose => return DialogAction::Close,
            Command::DialogParent => self.go_up(self.purpose.names_a_new_file()),
            Command::DialogComplete => self.complete(),
            _ => {}
        }
        DialogAction::Continue
    }

    /// Edits the path: typing, pasting, deleting, or moving the cursor.
    pub fn edit(&mut self, edit: Edit) {
        self.message = None;
        // A whole path pasted replaces the one there.
        if let Edit::Insert(text) = edit {
            if text.len() > 1 && (text.starts_with('/') || text.starts_with("~/")) {
                self.name_selected = false;
                self.text = text.to_string();
                self.caret.move_to_end();
                self.text_changed();
                return;
            }
        }
        if std::mem::take(&mut self.name_selected) {
            if let Edit::Insert(_)
            | Edit::DeleteBackward
            | Edit::DeleteForward
            | Edit::DeleteWordBackward
            | Edit::DeleteWordForward = edit
            {
                // Typing or deleting replaces the suggested name.
                let folder = self.folder_text().len();
                self.text.truncate(folder);
                self.caret.move_to_end();
                if let Edit::Insert(_) = edit {
                    self.caret.edit(&mut self.text, edit);
                }
                self.text_changed();
                return;
            }
            // Moving the cursor leaves the name to edit, narrowing the list.
            self.refilter();
        }
        let at_end = self.caret.at(&self.text) == self.text.len();
        if edit == Edit::DeleteBackward && at_end && self.text.ends_with('/') {
            self.go_up(false);
            return;
        }
        if self.caret.edit(&mut self.text, edit) {
            self.text_changed();
        }
    }

    /// A click on an entry goes into a folder or picks a file, and one on a
    /// folder in the path goes back to it. One outside the popup closes it.
    pub fn handle_mouse(&mut self, mouse: Mouse) -> DialogAction {
        let area = self.area();
        let inside = area.contains(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::Press(_) if !inside => DialogAction::Close,
            MouseKind::Press(MouseButton::Left) => {
                self.message = None;
                if mouse.y == area.y + 1 {
                    self.click_path(mouse.x.saturating_sub(area.x + 2) as usize);
                    return DialogAction::Continue;
                }
                let list = area.y + 3;
                if mouse.y < list || mouse.y + 1 >= area.y + area.height {
                    return DialogAction::Continue;
                }
                let index = self.scroll + (mouse.y - list) as usize;
                match self.rows.get(index) {
                    Some(&row) => self.click_row(index, row),
                    None => DialogAction::Continue,
                }
            }
            MouseKind::ScrollUp if inside => {
                self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
                DialogAction::Continue
            }
            MouseKind::ScrollDown if inside => {
                let max = self.rows.len().saturating_sub(self.rows() as usize);
                self.scroll = (self.scroll + WHEEL_ROWS).min(max);
                DialogAction::Continue
            }
            _ => DialogAction::Continue,
        }
    }

    /// Draws the popup over the top middle of the screen and returns where
    /// the terminal cursor goes: in the path. Draws nothing on a screen too
    /// small for it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let area = self.area();
        if area.width < 8 || area.height < 4 {
            return None;
        }
        Some(
            frame.with_clip(area.x, area.y, area.width, area.height, || {
                self.draw_popup(frame, area)
            }),
        )
    }

    // --- the path ---------------------------------------------------------------

    /// The path up to and including its last `/`: the folder.
    fn folder_text(&self) -> &str {
        match self.text.rfind('/') {
            Some(i) => &self.text[..=i],
            None => "",
        }
    }

    /// The path after its last `/`: the name.
    fn name(&self) -> &str {
        &self.text[self.folder_text().len()..]
    }

    /// The name narrowing the list: none while the suggested one is
    /// selected.
    fn filter(&self) -> &str {
        if self.name_selected {
            ""
        } else {
            self.name()
        }
    }

    /// `typed` as an absolute path: `~` is the home folder, and relative
    /// paths are from the folder the dialog opened in.
    fn resolve(&self, typed: &str) -> PathBuf {
        let path = match typed.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => match home() {
                Some(home) => home.join(rest.trim_start_matches('/')),
                None => self.base.join(typed),
            },
            _ => self.base.join(typed),
        };
        normalize(&path)
    }

    fn text_changed(&mut self) {
        self.confirm = None;
        let dir = self.resolve(self.folder_text());
        if dir != self.listing.dir {
            self.listing = Listing::read(&dir, self.purpose);
        }
        self.refilter();
    }

    /// Lists `dir`. The name typed is kept if `keep_name`; otherwise it was
    /// only narrowing the list.
    fn navigate(&mut self, dir: &Path, keep_name: bool) {
        let name = if keep_name {
            self.name().to_string()
        } else {
            self.name_selected = false;
            String::new()
        };
        self.text = format!("{}{name}", display_dir(dir));
        self.caret.move_to_end();
        self.text_changed();
    }

    /// Lists the folder above, if there is one.
    fn go_up(&mut self, keep_name: bool) {
        if let Some(parent) = self.listing.dir.parent().map(Path::to_path_buf) {
            self.navigate(&parent, keep_name);
        }
    }

    /// Puts the selected entry's name in the path, or the best match's. A
    /// folder's gets a `/`, going into it.
    fn complete(&mut self) {
        let row = self.selected.or((!self.rows.is_empty()).then_some(0));
        match row.and_then(|i| self.rows.get(i)) {
            Some(Row::Parent) => self.go_up(false),
            Some(&Row::Entry(i)) => {
                let entry = &self.listing.entries[i];
                let name = entry.name.clone();
                if entry.is_dir {
                    self.navigate(&self.listing.dir.join(name), false);
                } else {
                    self.set_name(&name, false);
                }
            }
            None => {}
        }
    }

    /// Puts `name` in the path after the folder, and selects its row. A
    /// `select`ed name is replaced by typing, and doesn't narrow the list.
    fn set_name(&mut self, name: &str, select: bool) {
        self.name_selected = select;
        self.text = format!("{}{name}", self.folder_text());
        self.caret.move_to_end();
        self.text_changed();
        self.selected = self
            .rows
            .iter()
            .position(|&row| matches!(row, Row::Entry(i) if self.listing.entries[i].name == name));
        self.scroll_into_view();
    }

    /// A click on column `column` of the path, from its left: on a folder,
    /// goes back to it.
    fn click_path(&mut self, column: usize) {
        let room = self.area().width.saturating_sub(4) as usize;
        let (_, cursor_column) = self.caret.view(&self.text, room);
        let cursor = self.text[..self.caret.at(&self.text)].chars().count();
        let clicked = cursor - cursor_column + column;
        let folder = self.folder_text();
        // The `/` ending the folder clicked.
        let Some((end, _)) = folder.char_indices().skip(clicked).find(|&(_, c)| c == '/') else {
            return;
        };
        let dir = self.resolve(&folder[..=end]);
        self.navigate(&dir, self.purpose.names_a_new_file());
    }

    fn click_row(&mut self, index: usize, row: Row) -> DialogAction {
        let keep_name = self.purpose.names_a_new_file();
        let Row::Entry(i) = row else {
            self.go_up(keep_name);
            return DialogAction::Continue;
        };
        let entry = &self.listing.entries[i];
        let path = self.listing.dir.join(&entry.name);
        if entry.is_dir {
            self.navigate(&path, keep_name);
            return DialogAction::Continue;
        }
        // Saving or creating, the first click names the file, and a second
        // takes it.
        if self.purpose == Purpose::Open || self.selected == Some(index) {
            return self.choose(path);
        }
        let name = entry.name.clone();
        self.set_name(&name, true);
        DialogAction::Continue
    }

    /// Enter: takes the selected row, or the path typed.
    fn accept(&mut self) -> DialogAction {
        match self.selected.and_then(|i| self.rows.get(i)) {
            Some(Row::Parent) => {
                self.go_up(false);
                return DialogAction::Continue;
            }
            Some(&Row::Entry(i)) => {
                let entry = &self.listing.entries[i];
                let path = self.listing.dir.join(&entry.name);
                if entry.is_dir {
                    self.navigate(&path, false);
                    return DialogAction::Continue;
                }
                return self.choose(path);
            }
            None => {}
        }
        let path = self.resolve(&self.text);
        if self.purpose == Purpose::AddFolder {
            return match path.is_dir() {
                // The folder listed.
                true if self.name().is_empty() => DialogAction::Accept(path),
                true => {
                    self.navigate(&path, false);
                    DialogAction::Continue
                }
                false => {
                    self.show_error("There's no such folder.".to_string());
                    DialogAction::Continue
                }
            };
        }
        if self.purpose == Purpose::Move && self.current.as_ref() == Some(&path) {
            // Left where it is.
            return DialogAction::Close;
        }
        if path.is_dir() {
            // `..` typed, or a folder the list didn't narrow to.
            self.navigate(&path, false);
            return DialogAction::Continue;
        }
        if self.name().is_empty() {
            return DialogAction::Continue;
        }
        if self.purpose == Purpose::Open && !path.is_file() {
            self.show_error(format!("There's no {}.", self.name()));
            return DialogAction::Continue;
        }
        self.choose(path)
    }

    /// Takes `path`, after asking whether to save over another file.
    fn choose(&mut self, path: PathBuf) -> DialogAction {
        let exists = fs::symlink_metadata(&path).is_ok();
        let name = file_name(&path);
        match self.purpose {
            Purpose::Open | Purpose::AddFolder => DialogAction::Accept(path),
            Purpose::Create if exists => {
                self.show_error(format!("{name} already exists."));
                DialogAction::Continue
            }
            Purpose::Create => DialogAction::Accept(path),
            // A new name for the same file, as when only its case changes.
            Purpose::Move
                if self
                    .current
                    .as_deref()
                    .is_some_and(|current| document::same_file(current, &path)) =>
            {
                DialogAction::Accept(path)
            }
            Purpose::CreateFolder | Purpose::Move | Purpose::Duplicate if exists => {
                self.show_error(format!("{name} already exists."));
                DialogAction::Continue
            }
            Purpose::CreateFolder | Purpose::Move | Purpose::Duplicate => {
                DialogAction::Accept(path)
            }
            Purpose::SaveAs => {
                let current = self
                    .current
                    .as_deref()
                    .is_some_and(|current| document::same_file(current, &path));
                if !exists || current || self.confirm.as_ref() == Some(&path) {
                    return DialogAction::Accept(path);
                }
                self.message = Some((
                    format!("{name} already exists. Enter again to replace it."),
                    true,
                ));
                self.confirm = Some(path);
                DialogAction::Continue
            }
        }
    }

    // --- the list ---------------------------------------------------------------

    /// Narrows the list to the entries matching the name typed, best first.
    /// Opening, the best is selected.
    fn refilter(&mut self) {
        let filter = self.filter().to_string();
        self.atom = (!filter.is_empty()).then(|| {
            Atom::new(
                &filter,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            )
        });
        let mut rows = Vec::new();
        let listed = self.listing.error.is_none();
        if filter.is_empty() && listed && self.listing.dir.parent().is_some() {
            rows.push(Row::Parent);
        }
        match &self.atom {
            None => rows.extend((0..self.listing.entries.len()).map(Row::Entry)),
            Some(atom) => {
                let mut matcher = self.matcher.borrow_mut();
                let mut buf = Vec::new();
                let mut scored: Vec<(usize, u16)> = self
                    .listing
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, entry)| {
                        let score =
                            atom.score(Utf32Str::new(&entry.name, &mut buf), &mut matcher)?;
                        Some((i, score))
                    })
                    .collect();
                scored.sort_by_key(|&(i, score)| (Reverse(score), i));
                rows.extend(scored.into_iter().map(|(i, _)| Row::Entry(i)));
            }
        }
        self.rows = rows;
        self.selected = match self.purpose {
            Purpose::Open => self.rows.iter().position(|&row| row != Row::Parent),
            // Enter takes the folder listed, until a name is typed.
            Purpose::AddFolder if !filter.is_empty() => {
                self.rows.iter().position(|&row| row != Row::Parent)
            }
            _ => None,
        };
        self.scroll = 0;
        self.scroll_into_view();
    }

    /// Moves the selection `step` rows, or onto the first row if there's
    /// none.
    fn step(&mut self, step: isize) {
        self.confirm = None;
        if self.rows.is_empty() {
            return;
        }
        let index = match self.selected {
            Some(selected) => selected.saturating_add_signed(step),
            None => 0,
        };
        self.selected = Some(index.min(self.rows.len() - 1));
        self.scroll_into_view();
    }

    fn scroll_into_view(&mut self) {
        let rows = self.rows().max(1) as usize;
        if let Some(selected) = self.selected {
            if selected < self.scroll {
                self.scroll = selected;
            } else if selected >= self.scroll + rows {
                self.scroll = selected + 1 - rows;
            }
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(rows));
    }

    // --- layout and drawing ---------------------------------------------------

    /// List rows on screen: a steady number, so the popup doesn't jump as
    /// the list narrows.
    fn rows(&self) -> u32 {
        // The row above, the borders, the path, and the rule under it.
        let room = self.screen_height.saturating_sub(5);
        MAX_ROWS.min(room).max(1)
    }

    fn area(&self) -> Area {
        let width = self
            .screen_width
            .saturating_sub(4)
            .min(MAX_WIDTH)
            .max(self.screen_width.min(24));
        Area {
            x: (self.screen_width - width) / 2,
            y: self.screen_height.min(1),
            width,
            height: (self.rows() + 4).min(self.screen_height),
        }
    }

    fn draw_popup(&self, frame: &Buffer, area: Area) -> (u32, u32) {
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        picker::draw_frame(frame, area, self.purpose.title());
        let bottom = y + height - 1;
        let status = self.status();
        picker::draw_status(frame, area, &status);
        let room_below = width.saturating_sub(status.chars().count() as u32 + 6) as usize;
        let (note, fg) = match &self.message {
            Some((text, error)) => (format!(" {text} "), if *error { ERROR } else { FG }),
            None => (self.hints.clone(), DIM),
        };
        let note: String = note.chars().take(room_below).collect();
        frame.draw_text(&note, x + 2, bottom, fg, None, Attributes::NONE);

        // The path: its folder dimmed, the cursor kept in view.
        let text_x = x + 2;
        let room = width.saturating_sub(4) as usize;
        let (shown, column) = self.caret.view(&self.text, room);
        let cursor = self.text[..self.caret.at(&self.text)].chars().count();
        let scroll = cursor - column;
        let folder = self.folder_text().chars().count().saturating_sub(scroll);
        let (dir_part, name_part): (String, String) = (
            shown.chars().take(folder).collect(),
            shown.chars().skip(folder).collect(),
        );
        frame.draw_text(&dir_part, text_x, y + 1, DIM, None, Attributes::NONE);
        let name_x = text_x + dir_part.chars().count() as u32;
        let name_bg = self.name_selected.then_some(SELECTED_BG);
        frame.draw_text(&name_part, name_x, y + 1, FG, name_bg, Attributes::NONE);

        let list = y + 3;
        if let Some(empty) = self.empty_text() {
            let (text, fg) = empty;
            let text: String = text.chars().take(room).collect();
            frame.draw_text(&text, text_x, list, fg, None, Attributes::NONE);
        }
        let visible = self.rows.iter().enumerate().skip(self.scroll);
        for ((index, &row), screen_y) in visible.zip(list..bottom) {
            if self.selected == Some(index) {
                frame.fill_rect(x + 1, screen_y, width.saturating_sub(2), 1, SELECTED_BG);
            }
            self.draw_row(frame, row, text_x, screen_y, room);
        }
        (text_x + column as u32, y + 1)
    }

    /// What to show when nothing's listed, and its color.
    fn empty_text(&self) -> Option<(String, Rgba)> {
        if let Some(error) = self.listing.error {
            let text = match (error, self.purpose) {
                (io::ErrorKind::NotFound, Purpose::Open | Purpose::AddFolder) => {
                    "There's no such folder.".to_string()
                }
                (io::ErrorKind::NotFound, _) => "A new folder, created with the file.".to_string(),
                (io::ErrorKind::NotADirectory, _) => "That's a file, not a folder.".to_string(),
                (error, _) => format!("Can't read this folder: {}.", io::Error::from(error)),
            };
            let fg = if self.purpose.names_a_new_file() && error == io::ErrorKind::NotFound {
                DIM
            } else {
                ERROR
            };
            return Some((text, fg));
        }
        if !self.rows.is_empty() {
            return None;
        }
        let text = match self.purpose {
            Purpose::AddFolder if self.filter().is_empty() => {
                "No folders in it. Enter adds it.".to_string()
            }
            _ if self.filter().is_empty() => "An empty folder.".to_string(),
            Purpose::Open => "No matching files.".to_string(),
            Purpose::AddFolder => "No matching folders.".to_string(),
            Purpose::SaveAs => format!("Enter saves as {}.", self.name()),
            Purpose::Create | Purpose::CreateFolder => format!("Enter creates {}.", self.name()),
            Purpose::Move => format!("Enter moves it to {}.", self.name()),
            Purpose::Duplicate => format!("Enter copies it to {}.", self.name()),
        };
        Some((text, DIM))
    }

    /// The count for the bottom border.
    fn status(&self) -> String {
        let total = self.listing.entries.len();
        if self.listing.error.is_some() {
            String::new()
        } else if self.filter().is_empty() {
            let noun = if total == 1 { "item" } else { "items" };
            format!(" {total} {noun} ")
        } else {
            let found = self.rows.iter().filter(|&&row| row != Row::Parent).count();
            format!(" {found} of {total} ")
        }
    }

    /// Draws `row` in the `room` columns from `x`: its name, the letters
    /// matched highlighted, a folder's ending in `/`, and a file's size on
    /// the right.
    fn draw_row(&self, frame: &Buffer, row: Row, x: u32, y: u32, room: usize) {
        let entry = match row {
            Row::Entry(i) => Some(&self.listing.entries[i]),
            Row::Parent => None,
        };
        let (x, room) = if icons::enabled() && room > 2 * icons::WIDTH as usize {
            let icon = match entry {
                Some(entry) if !entry.is_dir => icons::file(&entry.name),
                _ => icons::folder(false),
            };
            let dim = entry.is_none().then_some(DIM);
            (icon.draw(frame, x, y, dim), room - icons::WIDTH as usize)
        } else {
            (x, room)
        };
        let Some(entry) = entry else {
            frame.draw_text("../", x, y, DIM, None, Attributes::NONE);
            return;
        };
        let size = entry.size.map(human_size).unwrap_or_default();
        let size_width = size.chars().count();
        let room = if size_width > 0 && size_width + 2 < room {
            let size_x = x + (room - size_width) as u32;
            frame.draw_text(&size, size_x, y, DIM, None, Attributes::NONE);
            room - size_width - 2
        } else {
            room
        };

        let mut highlights = Vec::new();
        if let Some(atom) = &self.atom {
            let mut buf = Vec::new();
            atom.indices(
                Utf32Str::new(&entry.name, &mut buf),
                &mut self.matcher.borrow_mut(),
                &mut highlights,
            );
        }
        let slash = usize::from(entry.is_dir);
        let chars: Vec<char> = entry.name.chars().collect();
        let (shown, cut) = if chars.len() + slash <= room {
            (chars.len(), false)
        } else {
            (room.saturating_sub(1 + slash), true)
        };
        let mut x = x;
        for (i, &c) in chars[..shown].iter().enumerate() {
            let (fg, attributes) = if highlights.contains(&(i as u32)) {
                (MATCH_FG, Attributes::BOLD)
            } else {
                (FG, Attributes::NONE)
            };
            frame.draw_text(c.encode_utf8(&mut [0; 4]), x, y, fg, None, attributes);
            x += 1;
        }
        if cut {
            frame.draw_text("…", x, y, DIM, None, Attributes::NONE);
            x += 1;
        }
        if entry.is_dir {
            frame.draw_text("/", x, y, DIM, None, Attributes::NONE);
        }
    }
}

impl Listing {
    /// The entries of `dir`, folders first, each group by name ignoring
    /// case, as in the file tree. Only folders, for adding one.
    fn read(dir: &Path, purpose: Purpose) -> Listing {
        let read = match fs::read_dir(dir) {
            Ok(read) => read,
            Err(err) => {
                // Reading a file as a folder fails with an unhelpful error.
                let error = match dir.is_file() {
                    true => io::ErrorKind::NotADirectory,
                    false => err.kind(),
                };
                return Listing {
                    dir: dir.to_path_buf(),
                    error: Some(error),
                    entries: Vec::new(),
                };
            }
        };
        let mut entries: Vec<Entry> = read
            .filter_map(Result::ok)
            .filter(|entry| !tree::is_hidden(&entry.file_name()))
            .map(|entry| {
                // Following symlinks, to folders or files.
                let meta = fs::metadata(entry.path()).ok();
                let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
                Entry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir,
                    size: meta.filter(|_| !is_dir).map(|meta| meta.len()),
                }
            })
            .filter(|entry| entry.is_dir || purpose != Purpose::AddFolder)
            .collect();
        entries.sort_by(|a, b| {
            (!a.is_dir, a.name.to_lowercase(), &a.name).cmp(&(
                !b.is_dir,
                b.name.to_lowercase(),
                &b.name,
            ))
        });
        Listing {
            dir: dir.to_path_buf(),
            error: None,
            entries,
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// How a folder is shown in the path: with `~` for the home folder, ending
/// in `/`.
fn display_dir(dir: &Path) -> String {
    let mut shown = match home().and_then(|home| dir.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    if !shown.ends_with('/') {
        shown.push('/');
    }
    shown
}

/// `path` without `.` and `..`, which go nowhere and up. Symlinks are kept,
/// so `..` may not go where the file system would take it; the folder
/// listed is the one the path names.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            component => normal.push(component),
        }
    }
    normal
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A file size, as Finder shows it: `512 B`, `1.2 KB`, `34 KB`, `1.5 MB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit + 1 < UNITS.len() {
        size /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if size < 10.0 => format!("{size:.1} {}", UNITS[unit]),
        _ => format!("{size:.0} {}", UNITS[unit]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    /// A fresh folder with `files` (paths ending in `/` are folders).
    fn fixture(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-dialog-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            let path = dir.join(file);
            if file.ends_with('/') {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "12345").unwrap();
            }
        }
        dir.canonicalize().unwrap()
    }

    fn dialog(purpose: Purpose, dir: &Path, name: &str) -> FileDialog {
        FileDialog::new(purpose, dir, name, None, &Keymap::default(), 80, 24)
    }

    /// The rows listed, folders ending in `/`.
    fn listed(dialog: &FileDialog) -> Vec<String> {
        dialog
            .rows
            .iter()
            .map(|&row| match row {
                Row::Parent => "../".to_string(),
                Row::Entry(i) => {
                    let entry = &dialog.listing.entries[i];
                    let slash = if entry.is_dir { "/" } else { "" };
                    format!("{}{slash}", entry.name)
                }
            })
            .collect()
    }

    fn selected(dialog: &FileDialog) -> Option<String> {
        dialog.selected.map(|i| listed(dialog)[i].clone())
    }

    fn type_text(dialog: &mut FileDialog, text: &str) {
        for c in text.chars() {
            dialog.edit(Edit::Insert(c.encode_utf8(&mut [0; 4])));
        }
    }

    #[test]
    fn lists_folders_first_and_narrows_to_the_name_typed() {
        let root = fixture(
            "list",
            &["b.rs", "A.txt", "src/main.rs", ".git/HEAD", ".env"],
        );
        let mut dialog = dialog(Purpose::Open, &root, "");
        assert_eq!(listed(&dialog), ["../", "src/", ".env", "A.txt", "b.rs"]);
        assert_eq!(selected(&dialog).as_deref(), Some("src/"), "not `..`");
        assert_eq!(dialog.text, display_dir(&root));

        type_text(&mut dialog, "br");
        assert_eq!(listed(&dialog), ["b.rs"]);
        assert_eq!(dialog.status(), " 1 of 4 ");
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(root.join("b.rs"))
        );
    }

    #[test]
    fn slashes_go_into_folders_and_backspace_goes_back_up() {
        let root = fixture("folders", &["src/main.rs", "src/lib.rs", "top.rs"]);
        let mut dialog = dialog(Purpose::Open, &root, "");
        type_text(&mut dialog, "src/");
        assert_eq!(listed(&dialog), ["../", "lib.rs", "main.rs"]);
        dialog.edit(Edit::DeleteBackward);
        assert_eq!(dialog.text, display_dir(&root), "the whole folder goes");
        assert_eq!(dialog.listing.dir, root);

        // Enter or Tab on a folder goes into it; `..` goes up.
        type_text(&mut dialog, "sr");
        dialog.run(Command::DialogComplete);
        assert_eq!(dialog.listing.dir, root.join("src"));
        assert_eq!(dialog.text, display_dir(&root.join("src")));
        type_text(&mut dialog, "..");
        assert_eq!(dialog.run(Command::PickerAccept), DialogAction::Continue);
        assert_eq!(dialog.listing.dir, root);
        dialog.run(Command::PickerUp);
        assert_eq!(selected(&dialog).as_deref(), Some("../"));
        dialog.run(Command::PickerAccept);
        assert_eq!(dialog.listing.dir, root.parent().unwrap());
        dialog.run(Command::DialogParent);
        assert_eq!(dialog.listing.dir, root.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn opening_takes_only_files_that_exist() {
        let root = fixture("open", &["a.rs"]);
        let mut dialog = dialog(Purpose::Open, &root, "");
        type_text(&mut dialog, "zzz");
        assert_eq!(dialog.run(Command::PickerAccept), DialogAction::Continue);
        assert_eq!(dialog.message, Some(("There's no zzz.".to_string(), true)));
        // A full path, pasted, replaces the one there.
        let pasted = root.join("a.rs").display().to_string();
        dialog.edit(Edit::Insert(&pasted));
        assert_eq!(dialog.text, pasted);
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(root.join("a.rs"))
        );
    }

    #[test]
    fn saving_suggests_the_name_and_asks_before_replacing() {
        let root = fixture("save", &["other.txt", "dir/"]);
        let mine = root.join("mine.txt");
        let mut dialog = FileDialog::new(
            Purpose::SaveAs,
            &root,
            "mine.txt",
            Some(mine.clone()),
            &Keymap::default(),
            80,
            24,
        );
        // The whole folder is listed, and nothing selected.
        assert_eq!(listed(&dialog), ["../", "dir/", "other.txt"]);
        assert_eq!(selected(&dialog), None);
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(mine)
        );

        // Typing replaces the suggested name.
        type_text(&mut dialog, "other.txt");
        assert_eq!(dialog.name(), "other.txt");
        assert_eq!(dialog.run(Command::PickerAccept), DialogAction::Continue);
        assert!(dialog
            .message
            .as_ref()
            .unwrap()
            .0
            .contains("already exists"));
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(root.join("other.txt"))
        );

        // New folders are fine.
        dialog.edit(Edit::DeleteWordBackward);
        dialog.edit(Edit::DeleteWordBackward);
        dialog.edit(Edit::DeleteWordBackward);
        type_text(&mut dialog, "new/deeper/x.md");
        assert_eq!(dialog.listing.error, Some(io::ErrorKind::NotFound));
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(root.join("new/deeper/x.md"))
        );
    }

    #[test]
    fn creating_refuses_files_that_exist() {
        let root = fixture("create", &["taken.rs"]);
        let mut dialog = dialog(Purpose::Create, &root, "");
        type_text(&mut dialog, "taken.rs");
        assert_eq!(dialog.run(Command::PickerAccept), DialogAction::Continue);
        assert_eq!(
            dialog.message,
            Some(("taken.rs already exists.".to_string(), true))
        );
        type_text(&mut dialog, "x");
        assert_eq!(
            dialog.run(Command::PickerAccept),
            DialogAction::Accept(root.join("taken.rsx"))
        );
    }

    #[test]
    fn clicks_go_into_folders_pick_files_and_go_back_up_the_path() {
        let root = fixture("mouse", &["dir/inner.txt", "file.txt"]);
        let at = |x, y| Mouse {
            kind: MouseKind::Press(MouseButton::Left),
            x,
            y,
            mods: Mods::NONE,
        };
        let mut dialog = dialog(Purpose::Create, &root, "new.txt");
        let area = dialog.area();
        let list = area.y + 3;
        // Rows: `..`, dir/, file.txt.
        dialog.handle_mouse(at(area.x + 4, list + 1));
        assert_eq!(dialog.listing.dir, root.join("dir"));
        assert_eq!(dialog.name(), "new.txt", "the name is kept");

        // A click on the `/` after the fixture's folder in the path, which
        // may be scrolled to show its end.
        let room = area.width as usize - 4;
        let (_, column) = dialog.caret.view(&dialog.text, room);
        let scroll = dialog.text.chars().count() - column;
        let root_end = dialog.text.len() - "dir/new.txt".len() - 1;
        dialog.handle_mouse(at(area.x + 2 + (root_end - scroll) as u32, area.y + 1));
        assert_eq!(dialog.listing.dir, root);

        // Creating, a click on a file names it, and another takes it.
        let mut dialog = self::dialog(Purpose::SaveAs, &root, "");
        dialog.handle_mouse(at(area.x + 4, list + 2));
        assert_eq!(dialog.name(), "file.txt");
        assert_eq!(selected(&dialog).as_deref(), Some("file.txt"));
        assert_eq!(
            dialog.handle_mouse(at(area.x + 4, list + 2)),
            DialogAction::Continue
        );
        assert_eq!(
            dialog.handle_mouse(at(area.x + 4, list + 2)),
            DialogAction::Accept(root.join("file.txt")),
            "after asking to replace it"
        );
        assert_eq!(
            dialog.handle_mouse(at(0, area.y + area.height)),
            DialogAction::Close
        );
    }

    #[test]
    fn paths_resolve_home_and_relative_folders() {
        let root = fixture("resolve", &["a/"]);
        let dialog = dialog(Purpose::Open, &root, "");
        assert_eq!(dialog.resolve("a/../a/./"), root.join("a"));
        assert_eq!(dialog.resolve("/tmp/x/.."), PathBuf::from("/tmp"));
        if let Some(home) = home() {
            assert_eq!(dialog.resolve("~/x"), home.join("x"));
            assert_eq!(dialog.resolve("~"), home);
            assert_eq!(display_dir(&home.join("x")), "~/x/");
        }
        assert_eq!(display_dir(Path::new("/")), "/");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1234), "1.2 KB");
        assert_eq!(human_size(34_000), "34 KB");
    }

    #[test]
    fn draws_the_path_the_list_and_sizes() {
        let _serial = crate::test_serial();
        let root = fixture("draw", &["dir/", "file.txt"]);
        let dialog = dialog(Purpose::Open, &root, "");
        let screen =
            opentui::OwnedBuffer::new(80, 24, false, opentui::WidthMethod::Unicode, "test")
                .unwrap();
        screen.clear(Rgba::BLACK);
        let cursor = dialog.draw(&screen).unwrap();
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("Open File"), "{text}");
        assert!(lines[4].contains("│ ../"), "{text}");
        assert!(lines[5].contains("│ dir/"), "{text}");
        assert!(
            lines[6].contains("file.txt") && lines[6].contains("5 B │"),
            "{text}"
        );
        assert!(lines[20].contains("2 items"), "{text}");
        assert_eq!(cursor.1, 2);
    }
}
