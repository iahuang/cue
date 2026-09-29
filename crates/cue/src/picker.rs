//! The picker: a popup list that narrows as you type. It lists the
//! workspace's files (Ctrl+P), after what was shown recently, terminals and
//! untitled files included, or, when the query starts with `>`, every command with its
//! shortcut (Ctrl+K), as in Sublime and VS Code.
//!
//! Matching is fuzzy: the query's characters must appear in order, so `abcr`
//! finds `abracadabra.rs`. Matches at word starts, after `/`, and in runs
//! rank higher.
//!
//! The files come from the app's [`FileIndex`], which lists them in the
//! background.
//!
//! Ctrl+W closes the recent file, terminal, or untitled file selected, as
//! closing a panel closes what it shows.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use opentui::{Attributes, Buffer, Rgba};

use crate::file_index::FileIndex;
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::{Command, Context, Keymap};
use crate::line_edit::{Caret, Edit};
use crate::workspace::Workspace;

/// The terminal's own background, as behind the editor; the border sets
/// the popup apart.
pub const BG: Rgba = Rgba::terminal_default([0, 0, 0]);
pub const BORDER: Rgba = Rgba::rgb(88, 91, 112);
pub const FG: Rgba = Rgba::rgb(205, 214, 244);
pub const DIM: Rgba = Rgba::rgb(147, 153, 178);
pub const MATCH_FG: Rgba = Rgba::rgb(137, 180, 250);
pub const SELECTED_BG: Rgba = Rgba::rgb(69, 71, 110);

/// The widest the popup gets, in columns.
const MAX_WIDTH: u32 = 90;
/// The most results shown at once.
const MAX_ROWS: u32 = 14;
/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

/// What the picker lists. The query decides: `>` first means commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Files,
    Commands,
}

/// What was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    File(PathBuf),
    Command(Command),
    /// A terminal, by its id.
    Terminal(u32),
    /// An untitled file, by its number.
    Untitled(u32),
}

/// What the app should do after the picker handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    Continue,
    Close,
    Accept(Choice),
    /// Close this recent file, terminal, or untitled file.
    CloseItem(Choice),
}

/// A row the picker can list.
#[derive(Clone)]
pub struct Item {
    /// What the query matches and the row shows: a file's path in the
    /// workspace, or a command's title and id.
    pub text: String,
    /// The characters of `text` shown dimmed: a file's folder, a command's id.
    dim: Range<usize>,
    /// Shown on the right: a command's shortcut, or `recent`.
    detail: String,
    choice: Choice,
}

impl Item {
    pub fn file(path: PathBuf, workspace: &Workspace, detail: &str) -> Item {
        let text = workspace.display_path(&path);
        let folder = text
            .rsplit_once('/')
            .map_or(0, |(dir, _)| dir.chars().count() + 1);
        Item {
            text,
            dim: 0..folder,
            detail: detail.to_string(),
            choice: Choice::File(path),
        }
    }

    /// Terminal `id`, labeled `name` and, dimmed, `detail` (what's running
    /// in it).
    pub fn terminal(id: u32, name: &str, running: &str) -> Item {
        let text = if running.is_empty() {
            name.to_string()
        } else {
            format!("{name} {running}")
        };
        Item {
            dim: name.chars().count()..text.chars().count(),
            text,
            detail: "terminal".to_string(),
            choice: Choice::Terminal(id),
        }
    }

    /// Untitled file `number`, not yet saved.
    pub fn untitled(number: u32) -> Item {
        Item {
            text: crate::document::untitled_name(number),
            dim: 0..0,
            detail: "unsaved".to_string(),
            choice: Choice::Untitled(number),
        }
    }

    fn command(command: Command, keymap: &Keymap) -> Item {
        let title = command.title();
        let text = format!("{title} {}", command.id());
        let id_start = title.chars().count() + 1;
        Item {
            dim: id_start..text.chars().count(),
            text,
            detail: keymap
                .shortcut(command)
                .map_or(String::new(), |key| key.to_string()),
            choice: Choice::Command(command),
        }
    }

    fn path(&self) -> Option<&Path> {
        match &self.choice {
            Choice::File(path) => Some(path),
            Choice::Command(_) | Choice::Terminal(_) | Choice::Untitled(_) => None,
        }
    }
}

pub struct Picker {
    query: String,
    caret: Caret,
    /// What was shown recently, files and terminals, most recent first.
    /// Listed before the rest.
    recent: Vec<Item>,
    /// The workspace's files, from the file index.
    files: Rc<Vec<Item>>,
    /// Positions in `files` of the recent files, ascending, so that each
    /// file is listed once.
    duplicates: Vec<usize>,
    /// Whether the first listing of the files is still in progress.
    listing: bool,
    /// Whether some files went unlisted.
    truncated: bool,
    /// The bottom border's hint for closing what was shown recently.
    close_hint: String,
    commands: Vec<Item>,
    pattern: Pattern,
    matcher: RefCell<Matcher>,
    /// Positions of the matching items (see [`Picker::item`]), best first.
    matches: Vec<usize>,
    selected: usize,
    /// The first match on screen.
    scroll: usize,
    screen_width: u32,
    screen_height: u32,
}

impl Picker {
    /// A picker for `mode` over a screen `width` x `height`. `recent` items
    /// (see [`Item::file`] and [`Item::terminal`]) are listed first, then
    /// the rest of the `index`'s files. The commands listed are those
    /// `available` where the picker opened, labeled by the keymap with
    /// their shortcuts.
    pub fn new(
        mode: Mode,
        keymap: &Keymap,
        available: impl Fn(Command) -> bool,
        recent: Vec<Item>,
        index: &FileIndex,
        width: u32,
        height: u32,
    ) -> Picker {
        let commands = Command::ALL
            .iter()
            // Moving through the picker, search options, and the find bar's
            // and file dialog's own keys aren't something to pick from it.
            .filter(|command| {
                !matches!(
                    command.context(),
                    Context::Picker
                        | Context::Search
                        | Context::SearchOptions
                        | Context::Find
                        | Context::Dialog
                )
            })
            .filter(|&&command| available(command))
            .map(|&command| Item::command(command, keymap))
            .collect();
        let mut picker = Picker {
            query: String::new(),
            caret: Caret::default(),
            recent,
            files: Rc::new(Vec::new()),
            duplicates: Vec::new(),
            listing: false,
            truncated: false,
            close_hint: keymap
                .shortcut(Command::PickerCloseItem)
                .map_or(String::new(), |key| format!(" {key:#} close ")),
            commands,
            pattern: Pattern::default(),
            matcher: RefCell::new(Matcher::new(Config::DEFAULT.match_paths())),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            screen_width: width,
            screen_height: height,
        };
        picker.take_files(index);
        picker.set_mode(mode);
        picker
    }

    pub fn mode(&self) -> Mode {
        if self.query.starts_with('>') {
            Mode::Commands
        } else {
            Mode::Files
        }
    }

    /// Switches to listing `mode`, keeping what was typed.
    pub fn set_mode(&mut self, mode: Mode) {
        let needle = self.needle().to_string();
        self.query = match mode {
            Mode::Files => needle,
            Mode::Commands => format!(">{needle}"),
        };
        self.caret.move_to_end();
        self.query_changed();
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        self.scroll_into_view();
    }

    /// Lists the index's current files, keeping the selection on the same
    /// item if it's still there.
    pub fn set_files(&mut self, index: &FileIndex) {
        // Looked up before the old files, which the matches point into, go.
        let selected = self.selected_item().map(|item| item.text.clone());
        self.take_files(index);
        if self.mode() == Mode::Files {
            self.refilter(selected);
        }
    }

    /// Lists `recent` first from now on, as after one was closed, keeping
    /// the selection where it was.
    pub fn set_recent(&mut self, recent: Vec<Item>) {
        self.recent = recent;
        self.find_duplicates();
        let (selected, scroll) = (self.selected, self.scroll);
        self.refilter(None);
        self.scroll = scroll;
        self.select(selected);
    }

    fn take_files(&mut self, index: &FileIndex) {
        self.files = index.files();
        self.listing = index.listing();
        self.truncated = index.truncated();
        self.find_duplicates();
    }

    fn find_duplicates(&mut self) {
        let recent: HashSet<&Path> = self.recent.iter().filter_map(Item::path).collect();
        self.duplicates = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, item)| item.path().is_some_and(|path| recent.contains(path)))
            .map(|(i, _)| i)
            .collect();
    }

    /// Runs a picker command.
    pub fn run(&mut self, command: Command) -> PickerAction {
        let page = self.rows().saturating_sub(1).max(1) as usize;
        match command {
            Command::PickerUp => self.select(self.selected.saturating_sub(1)),
            Command::PickerDown => self.select(self.selected + 1),
            Command::PickerPageUp => self.select(self.selected.saturating_sub(page)),
            Command::PickerPageDown => self.select(self.selected + page),
            Command::PickerAccept => return self.accept(),
            Command::PickerClose => return PickerAction::Close,
            Command::PickerCloseItem => {
                // Only what was shown recently is open to close.
                let recent = self.mode() == Mode::Files
                    && self
                        .matches
                        .get(self.selected)
                        .is_some_and(|&i| i < self.recent.len());
                if let Some(item) = self.selected_item().filter(|_| recent) {
                    return PickerAction::CloseItem(item.choice.clone());
                }
            }
            _ => {}
        }
        PickerAction::Continue
    }

    /// Edits the query: typing, pasting, deleting, or moving the cursor.
    pub fn edit(&mut self, edit: Edit) {
        if self.caret.edit(&mut self.query, edit) {
            self.query_changed();
        }
    }

    /// A click on a result picks it; one outside the popup closes it.
    pub fn handle_mouse(&mut self, mouse: Mouse) -> PickerAction {
        let area = self.area();
        let inside = area.contains(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::Press(_) if !inside => PickerAction::Close,
            MouseKind::Press(MouseButton::Left) => {
                let list = area.y + 3;
                if mouse.y < list || mouse.y + 1 >= area.y + area.height {
                    return PickerAction::Continue;
                }
                let index = self.scroll + (mouse.y - list) as usize;
                if index >= self.matches.len() {
                    return PickerAction::Continue;
                }
                self.selected = index;
                self.accept()
            }
            MouseKind::ScrollUp if inside => {
                self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
                PickerAction::Continue
            }
            MouseKind::ScrollDown if inside => {
                let max = self.matches.len().saturating_sub(self.rows() as usize);
                self.scroll = (self.scroll + WHEEL_ROWS).min(max);
                PickerAction::Continue
            }
            _ => PickerAction::Continue,
        }
    }

    /// Draws the popup over the top middle of the screen and returns where
    /// the terminal cursor goes: the end of the query. Draws nothing on a
    /// screen too small for it.
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

    // --- matching -----------------------------------------------------------

    /// The query without the `>` that asks for commands.
    fn needle(&self) -> &str {
        self.query.strip_prefix('>').unwrap_or(&self.query)
    }

    fn query_changed(&mut self) {
        // A new query starts at the best match.
        self.selected = 0;
        self.scroll = 0;
        self.refilter(None);
    }

    /// The items listed in the current mode: recent files, then the rest,
    /// which include the recent ones again (see `duplicates`).
    fn lists(&self) -> (&[Item], &[Item]) {
        match self.mode() {
            Mode::Files => (&self.recent, &self.files),
            Mode::Commands => (&self.commands, &[]),
        }
    }

    /// The item at `position` in [`Picker::lists`], taken as one list.
    fn item(&self, position: usize) -> &Item {
        let (first, rest) = self.lists();
        match first.get(position) {
            Some(item) => item,
            None => &rest[position - first.len()],
        }
    }

    fn selected_item(&self) -> Option<&Item> {
        self.matches.get(self.selected).map(|&i| self.item(i))
    }

    /// Matches the query against every item, selecting the item with the
    /// text `selected` if it matches. Better matches come first; among equal
    /// ones, recent files, then shorter paths.
    fn refilter(&mut self, selected: Option<String>) {
        self.pattern = Pattern::parse(self.needle(), CaseMatching::Smart, Normalization::Smart);
        let mut matcher = self.matcher.borrow_mut();
        let (first, rest) = self.lists();
        let mut skip = match self.mode() {
            Mode::Files => &self.duplicates[..],
            Mode::Commands => &[],
        }
        .iter()
        .map(|&i| first.len() + i)
        .peekable();
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = first
            .iter()
            .chain(rest.iter())
            .enumerate()
            .filter(|&(i, _)| skip.next_if_eq(&i).is_none())
            .filter_map(|(i, item)| {
                let score = self
                    .pattern
                    .score(Utf32Str::new(&item.text, &mut buf), &mut matcher)?;
                Some((i, score))
            })
            .collect();
        if !self.pattern.atoms.is_empty() {
            let recent = first.len();
            scored.sort_by_cached_key(|&(i, score)| {
                let len = if i < recent {
                    0
                } else {
                    self.item(i).text.len()
                };
                (Reverse(score), i >= recent, len, i)
            });
        }
        drop(matcher);
        self.matches = scored.into_iter().map(|(i, _)| i).collect();
        if let Some(text) = selected {
            if let Some(index) = self.matches.iter().position(|&i| self.item(i).text == text) {
                self.selected = index;
            }
        }
        self.select(self.selected);
    }

    fn accept(&self) -> PickerAction {
        match self.selected_item() {
            Some(item) => PickerAction::Accept(item.choice.clone()),
            None => PickerAction::Continue,
        }
    }

    // --- layout and drawing -------------------------------------------------

    /// Selects match `index` (clamped) and scrolls it into view.
    fn select(&mut self, index: usize) {
        self.selected = index.min(self.matches.len().saturating_sub(1));
        self.scroll_into_view();
    }

    fn scroll_into_view(&mut self) {
        let rows = self.rows().max(1) as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(rows));
    }

    /// Result rows on screen: as many as there are matches (one for "No
    /// matches"), up to what fits below the query.
    fn rows(&self) -> u32 {
        // The row above, the borders, the query, and the rule under it.
        let room = self.screen_height.saturating_sub(5);
        (self.matches.len() as u32).clamp(1, MAX_ROWS).min(room)
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
        let (title, noun) = match self.mode() {
            Mode::Files => ("Go to File", "files"),
            Mode::Commands => ("Commands", "commands"),
        };
        draw_frame(frame, area, title);
        let status = self.status(noun);
        draw_status(frame, area, &status);
        let room = width.saturating_sub(status.chars().count() as u32 + 6) as usize;
        let closable = self.mode() == Mode::Files && !self.recent.is_empty();
        if closable && self.close_hint.chars().count() <= room {
            let bottom = y + height - 1;
            frame.draw_text(&self.close_hint, x + 2, bottom, DIM, None, Attributes::NONE);
        }

        // The query, the cursor kept in view.
        let text_x = x + 2;
        let room = width.saturating_sub(4) as usize;
        let (shown, column) = self.caret.view(&self.query, room);
        let shown_width = shown.chars().count();
        frame.draw_text(&shown, text_x, y + 1, FG, None, Attributes::NONE);
        let cursor = (text_x + column as u32, y + 1);
        if self.needle().is_empty() {
            let hint = match self.mode() {
                Mode::Files => "Search files by name, or type > for commands",
                Mode::Commands => "Search commands",
            };
            let hint: String = hint
                .chars()
                .take(room.saturating_sub(shown_width + 1))
                .collect();
            let hint_x = text_x + shown_width as u32 + 1;
            frame.draw_text(&hint, hint_x, y + 1, DIM, None, Attributes::NONE);
        }

        let list = y + 3;
        if self.matches.is_empty() {
            let empty = if self.listing() {
                "Listing files…".to_string()
            } else {
                format!("No matching {noun}")
            };
            frame.draw_text(&empty, text_x, list, DIM, None, Attributes::NONE);
        }
        let visible = self.matches.iter().enumerate().skip(self.scroll);
        for ((index, &item), row) in visible.zip(list..y + height - 1) {
            if index == self.selected {
                frame.fill_rect(x + 1, row, width.saturating_sub(2), 1, SELECTED_BG);
            }
            self.draw_item(frame, self.item(item), text_x, row, room);
        }
        cursor
    }

    /// Whether files are still being listed, in file mode.
    fn listing(&self) -> bool {
        self.mode() == Mode::Files && self.listing
    }

    /// The count of matches, or of files so far while listing them.
    fn status(&self, noun: &str) -> String {
        let (first, rest) = self.lists();
        let mut total = first.len() + rest.len();
        if self.mode() == Mode::Files {
            total -= self.duplicates.len();
        }
        let listing = if self.listing() {
            " listing…"
        } else if self.truncated && self.mode() == Mode::Files {
            " (too many to list all)"
        } else {
            ""
        };
        if self.needle().is_empty() {
            format!(" {total} {noun}{listing} ")
        } else {
            format!(" {} of {total}{listing} ", self.matches.len())
        }
    }

    /// Draws `item` in the `room` columns from `x`: its text with the matched
    /// characters highlighted, and its detail on the right. A long file path
    /// is cut on the left, to keep its name.
    fn draw_item(&self, frame: &Buffer, item: &Item, x: u32, y: u32, room: usize) {
        let detail = item.detail.chars().count();
        if detail > 0 && detail + 2 < room {
            let detail_x = x + (room - detail) as u32;
            frame.draw_text(&item.detail, detail_x, y, DIM, None, Attributes::NONE);
        }
        let room = if detail > 0 && detail + 2 < room {
            room - detail - 2
        } else {
            room
        };

        let mut highlights = Vec::new();
        let mut buf = Vec::new();
        self.pattern.indices(
            Utf32Str::new(&item.text, &mut buf),
            &mut self.matcher.borrow_mut(),
            &mut highlights,
        );
        let highlights: HashSet<usize> = highlights.into_iter().map(|i| i as usize).collect();

        let chars: Vec<char> = item.text.chars().collect();
        let (shown, ellipsis): (Range<usize>, &str) = if chars.len() <= room {
            (0..chars.len(), "")
        } else if matches!(item.choice, Choice::File(_)) {
            (chars.len() + 1 - room..chars.len(), "…")
        } else {
            (0..room.saturating_sub(1), "…")
        };
        let mut x = x;
        let style = |i: usize| {
            if highlights.contains(&i) {
                (MATCH_FG, Attributes::BOLD)
            } else if item.dim.contains(&i) {
                (DIM, Attributes::NONE)
            } else {
                (FG, Attributes::NONE)
            }
        };
        let mut draw = |text: &str, (fg, attributes): (Rgba, Attributes)| {
            frame.draw_text(text, x, y, fg, None, attributes);
            x += text.chars().count() as u32;
        };
        let cut_left = shown.start > 0;
        if cut_left {
            draw(ellipsis, (DIM, Attributes::NONE));
        }
        // Runs of characters with the same style.
        let mut run = String::new();
        let mut run_style = None;
        for i in shown.clone() {
            let style = style(i);
            if let Some(previous) = run_style.filter(|&s| s != style) {
                draw(&run, previous);
                run.clear();
            }
            run.push(chars[i]);
            run_style = Some(style);
        }
        if let Some(style) = run_style {
            draw(&run, style);
        }
        if !cut_left && shown.end < chars.len() {
            draw(ellipsis, (DIM, Attributes::NONE));
        }
    }
}

/// Where a popup is on screen.
#[derive(Debug, Clone, Copy)]
pub struct Area {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    pub fn contains(&self, x: u32, y: u32) -> bool {
        (self.x..self.x + self.width).contains(&x) && (self.y..self.y + self.height).contains(&y)
    }
}

/// Clears `area` and draws a popup's box around it, with `title` in the top
/// border and a rule under the first row, which holds the query.
pub fn draw_frame(frame: &Buffer, area: Area, title: &str) {
    let Area {
        x,
        y,
        width,
        height,
    } = area;
    let right = x + width - 1;
    frame.fill_rect(x, y, width, height, BG);
    let rule = "─".repeat(width.saturating_sub(2) as usize);
    let border = |left: &str, right_end: &str, y: u32| {
        frame.draw_text(
            &format!("{left}{rule}{right_end}"),
            x,
            y,
            BORDER,
            None,
            Attributes::NONE,
        );
    };
    border("╭", "╮", y);
    border("├", "┤", y + 2);
    border("╰", "╯", y + height - 1);
    for row in y + 1..y + height - 1 {
        if row != y + 2 {
            frame.draw_text("│", x, row, BORDER, None, Attributes::NONE);
            frame.draw_text("│", right, row, BORDER, None, Attributes::NONE);
        }
    }
    frame.draw_text(&format!(" {title} "), x + 2, y, FG, None, Attributes::BOLD);
}

/// Draws `status` into the right of a popup's bottom border.
pub fn draw_status(frame: &Buffer, area: Area, status: &str) {
    let x = (area.x + area.width).saturating_sub(status.chars().count() as u32 + 2);
    let y = area.y + area.height - 1;
    frame.draw_text(status, x, y, DIM, None, Attributes::NONE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use std::fs;
    use std::path::Path;

    /// A fresh workspace folder with `files`.
    fn fixture(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-picker-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        dir.canonicalize().unwrap()
    }

    /// The workspace at `root` and its files, listed.
    fn index(root: &Path) -> (Workspace, FileIndex) {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        let mut index = FileIndex::new(&workspace);
        index.wait();
        (workspace, index)
    }

    fn picker(root: &Path, mode: Mode, recent: Vec<PathBuf>) -> Picker {
        let (workspace, index) = index(root);
        let recent = recent
            .into_iter()
            .map(|path| Item::file(path, &workspace, "recent"))
            .collect();
        Picker::new(mode, &Keymap::default(), |_| true, recent, &index, 80, 24)
    }

    /// The listed texts, best match first.
    fn listed(picker: &Picker) -> Vec<&str> {
        picker
            .matches
            .iter()
            .map(|&i| picker.item(i).text.as_str())
            .collect()
    }

    fn selected(picker: &Picker) -> &str {
        &picker.item(picker.matches[picker.selected]).text
    }

    #[test]
    fn fuzzy_matches_letters_in_order() {
        let root = fixture(
            "fuzzy",
            &["abracadabra.rs", "src/about.rs", "crab.rs", "src/main.rs"],
        );
        let mut picker = picker(&root, Mode::Files, Vec::new());
        picker.edit(Edit::Insert("abcr"));
        assert_eq!(listed(&picker), ["abracadabra.rs"]);
        picker.edit(Edit::DeleteBackward);
        picker.edit(Edit::DeleteBackward);
        let mut ab = listed(&picker);
        ab.sort();
        assert_eq!(ab, ["abracadabra.rs", "crab.rs", "src/about.rs"]);

        picker.edit(Edit::DeleteWordBackward);
        assert_eq!(picker.query, "");
        picker.edit(Edit::Insert("main"));
        assert_eq!(listed(&picker), ["src/main.rs"]);
    }

    #[test]
    fn edits_the_query_at_the_cursor() {
        let root = fixture("cursor", &["src/main.rs", "src/about.rs"]);
        let mut picker = picker(&root, Mode::Files, Vec::new());
        picker.edit(Edit::Insert("ain"));
        picker.edit(Edit::Start);
        picker.edit(Edit::Insert("m"));
        assert_eq!(picker.query, "main");
        assert_eq!(listed(&picker), ["src/main.rs"]);
        picker.edit(Edit::Right);
        picker.edit(Edit::DeleteForward);
        assert_eq!(picker.query, "man");

        // Deleting the `>` lists files.
        picker.set_mode(Mode::Commands);
        picker.edit(Edit::Start);
        picker.edit(Edit::DeleteForward);
        assert_eq!(picker.mode(), Mode::Files);
        assert_eq!(picker.query, "man");
    }

    #[test]
    fn prefers_shorter_paths_among_equal_matches() {
        let root = fixture("shorter", &["deep/nested/mod.rs", "src/mod.rs"]);
        let mut picker = picker(&root, Mode::Files, Vec::new());
        picker.edit(Edit::Insert("mod"));
        assert_eq!(listed(&picker), ["src/mod.rs", "deep/nested/mod.rs"]);
    }

    #[test]
    fn recent_files_come_first_and_are_listed_once() {
        let root = fixture("recent", &["a.rs", "b.rs", "c.rs"]);
        let recent = vec![root.join("c.rs"), root.join("b.rs")];
        let mut picker = picker(&root, Mode::Files, recent);
        assert_eq!(listed(&picker), ["c.rs", "b.rs", "a.rs"]);
        picker.edit(Edit::Insert("rs"));
        assert_eq!(
            listed(&picker),
            ["c.rs", "b.rs", "a.rs"],
            "ties keep recents first"
        );
        assert_eq!(
            picker.run(Command::PickerAccept),
            PickerAction::Accept(Choice::File(root.join("c.rs")))
        );
        assert!(picker.status("files").contains(" 3 of 3 "));
    }

    #[test]
    fn new_files_keep_the_selection() {
        let root = fixture("new-files", &["b.rs", "d.rs"]);
        let (_, mut index) = index(&root);
        let mut picker = Picker::new(
            Mode::Files,
            &Keymap::default(),
            |_| true,
            Vec::new(),
            &index,
            80,
            24,
        );
        picker.run(Command::PickerDown);
        assert_eq!(selected(&picker), "d.rs");
        fs::write(root.join("a.rs"), "").unwrap();
        index.refresh();
        index.wait();
        picker.set_files(&index);
        assert_eq!(listed(&picker), ["a.rs", "b.rs", "d.rs"]);
        assert_eq!(selected(&picker), "d.rs");
    }

    #[test]
    fn a_leading_angle_bracket_lists_commands() {
        let root = fixture("commands", &["a.rs"]);
        let mut picker = picker(&root, Mode::Files, Vec::new());
        picker.edit(Edit::Insert(">wrap"));
        assert_eq!(picker.mode(), Mode::Commands);
        assert_eq!(selected(&picker), "Toggle Word Wrap editor:toggle-wrap");
        assert_eq!(
            picker.run(Command::PickerAccept),
            PickerAction::Accept(Choice::Command(Command::ToggleWrap))
        );
        // Ids match too.
        picker.set_mode(Mode::Files);
        assert_eq!(picker.query, "wrap");
        picker.set_mode(Mode::Commands);
        picker.edit(Edit::DeleteWordBackward);
        picker.edit(Edit::Insert("select-all"));
        assert_eq!(selected(&picker), "Select All editor:select-all");

        // Picker navigation isn't listed.
        picker.edit(Edit::DeleteWordBackward);
        assert_eq!(picker.query, ">select-");
        picker.query = ">".to_string();
        picker.query_changed();
        assert!(listed(&picker).iter().all(|text| !text.contains("picker:")));
        assert_eq!(listed(&picker).len(), picker.commands.len());
    }

    #[test]
    fn keyboard_and_mouse_selection() {
        let files: Vec<String> = (0..30).map(|i| format!("f{i:02}.rs")).collect();
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        let root = fixture("select", &files);
        let mut picker = picker(&root, Mode::Files, Vec::new());
        assert_eq!(picker.rows(), MAX_ROWS);
        picker.run(Command::PickerDown);
        picker.run(Command::PickerPageDown);
        assert_eq!(selected(&picker), "f14.rs");
        assert_eq!(picker.scroll, 1);
        picker.run(Command::PickerUp);
        picker.run(Command::PickerPageUp);
        assert_eq!(selected(&picker), "f00.rs");

        let at = |kind, x, y| Mouse {
            kind,
            x,
            y,
            mods: Mods::NONE,
        };
        let area = picker.area();
        picker.handle_mouse(at(MouseKind::ScrollDown, 40, area.y + 5));
        assert_eq!(picker.scroll, WHEEL_ROWS);
        // The first result row is below the top border, query, and rule.
        assert_eq!(
            picker.handle_mouse(at(MouseKind::Press(MouseButton::Left), 40, area.y + 4)),
            PickerAction::Accept(Choice::File(root.join("f04.rs")))
        );
        assert_eq!(
            picker.handle_mouse(at(
                MouseKind::Press(MouseButton::Left),
                40,
                area.y + area.height
            )),
            PickerAction::Close
        );
    }

    #[test]
    fn draws_highlights_and_cuts_long_paths_on_the_left() {
        let _serial = crate::test_serial();
        let long = format!("{}/name.rs", "folder".repeat(20));
        let root = fixture("draw", &[&long, "short.rs"]);
        let (_, index) = index(&root);
        let mut picker = Picker::new(
            Mode::Files,
            &Keymap::default(),
            |_| true,
            Vec::new(),
            &index,
            60,
            12,
        );
        picker.edit(Edit::Insert("name"));
        let screen =
            opentui::OwnedBuffer::new(60, 12, false, opentui::WidthMethod::Unicode, "test")
                .unwrap();
        screen.clear(Rgba::BLACK);
        let cursor = picker.draw(&screen).unwrap();
        let text = screen.to_text(true);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("Go to File"), "{text}");
        assert!(lines[2].contains("│ name"), "{text}");
        assert!(lines[4].contains("│ …"), "{text}");
        assert!(lines[4].trim_end().ends_with("name.rs │"), "{text}");
        assert!(lines[5].contains("1 of 2"), "{text}");
        assert_eq!(cursor, (picker.area().x + 6, 2));
    }
}
