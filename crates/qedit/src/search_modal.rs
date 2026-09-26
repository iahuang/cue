//! Workspace search (Ctrl+Shift+F): a large popup that searches every file
//! in the workspace as you type. As in Zed, matches are listed as excerpts:
//! each file's path, then its matching lines with a couple of lines around
//! them. Up and Down step through the matches, and Enter opens the file
//! with the match selected. The excerpts are only for reading.
//!
//! Closing the popup keeps the query and the selected match, so reopening
//! it searches again and picks up where it left off.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use opentui::{Attributes, Buffer, Rgba};

use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::{Command, Keymap};
use crate::picker::{self, Area, DIM, FG, MATCH_FG, SELECTED_BG};
use crate::search::{FileMatches, Line, Query, Search, Toggle, CONTEXT_LINES};
use crate::workspace::Workspace;

const LINE_NUMBER: Rgba = Rgba::rgb(108, 112, 134);
const MATCH_BG: Rgba = Rgba::rgb(49, 50, 68);
const ERROR: Rgba = Rgba::rgb(243, 139, 168);

/// The widest the popup gets, in columns.
const MAX_WIDTH: u32 = 160;
/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;
/// Columns a tab takes up to.
const TAB_WIDTH: usize = 4;
/// Left of a match scrolled into view sideways, this many columns show.
const LEAD_COLUMNS: usize = 12;

/// What the app should do after the popup handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchAction {
    Continue,
    Close,
    /// Open the file and select the bytes `range` of line `line` (0-based).
    Open {
        path: PathBuf,
        line: u32,
        range: Range<usize>,
    },
}

/// What's kept from one search to the next.
#[derive(Debug, Clone, Default)]
pub struct Memory {
    query: Query,
    /// The selected match: its file, line, and which match in the line.
    selected: Option<(PathBuf, u32, usize)>,
}

/// A row of the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    /// A file's path, above its excerpts.
    File(usize),
    /// A line of a file.
    Line { file: usize, line: usize },
    /// Lines skipped between two excerpts.
    Gap,
    /// Space between two files.
    Blank,
}

/// Where a match is.
#[derive(Debug, Clone, Copy)]
struct MatchAt {
    row: usize,
    file: usize,
    line: usize,
    /// Which of the line's matches.
    index: usize,
}

pub struct SearchModal {
    workspace: Workspace,
    /// Unsaved text of open files, searched instead of what's on disk.
    unsaved: HashMap<PathBuf, String>,
    query: Query,
    /// The query was filled in on opening: typing replaces it.
    replace_query: bool,
    /// The search in progress, if any.
    search: Option<Search>,
    /// The results are from the previous query, shown until the new search
    /// finds something.
    stale: bool,
    /// Why the query can't be searched: an invalid regex.
    error: Option<String>,
    /// Whether the last search stopped before searching everything.
    truncated: bool,
    /// Files with matches, sorted by path ignoring case.
    files: Vec<FileMatches>,
    rows: Vec<Row>,
    /// Every match, in order.
    matches: Vec<MatchAt>,
    selected: usize,
    /// The selection was moved, rather than left on the first match.
    moved: bool,
    /// A match to select when its file comes in: where the last search left
    /// off.
    restore: Option<(PathBuf, u32, usize)>,
    /// The first row on screen.
    scroll: usize,
    /// Shortcut hints for the bottom border.
    hints: String,
    screen_width: u32,
    screen_height: u32,
}

impl SearchModal {
    /// A search popup over a screen `width` x `height`, starting from where
    /// the last one left off, with `text` as the query if given. `unsaved`
    /// has the text of open files with unsaved changes.
    pub fn new(
        workspace: &Workspace,
        keymap: &Keymap,
        memory: Memory,
        text: Option<String>,
        unsaved: HashMap<PathBuf, String>,
        width: u32,
        height: u32,
    ) -> SearchModal {
        let hints = Toggle::ALL
            .iter()
            .filter_map(|toggle| {
                let key = keymap.shortcut(toggle.command())?;
                Some(format!("{key} {}", toggle.name()))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let mut query = memory.query;
        let restore = match text {
            Some(text) if text != query.text => {
                query.text = text;
                None
            }
            _ => memory.selected,
        };
        let mut modal = SearchModal {
            workspace: workspace.clone(),
            unsaved,
            replace_query: !query.text.is_empty(),
            query,
            search: None,
            stale: false,
            error: None,
            truncated: false,
            files: Vec::new(),
            rows: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            moved: false,
            restore: None,
            scroll: 0,
            hints: format!(" {hints} "),
            screen_width: width,
            screen_height: height,
        };
        modal.start();
        modal.restore = restore;
        modal
    }

    /// What to keep for the next search.
    pub fn memory(&self) -> Memory {
        let selected = self.matches.get(self.selected).map(|m| {
            let file = &self.files[m.file];
            (file.path.clone(), file.lines[m.line].number, m.index)
        });
        Memory {
            query: self.query.clone(),
            selected: selected.or_else(|| self.restore.clone()),
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        self.scroll_into_view();
    }

    /// Runs a picker or search command.
    pub fn run(&mut self, command: Command) -> SearchAction {
        let page = self.body_rows().saturating_sub(1).max(1);
        match command {
            Command::PickerUp => self.select(self.selected.saturating_sub(1)),
            Command::PickerDown => self.select(self.selected + 1),
            Command::PickerPageUp => self.select_row_step(-(page as isize)),
            Command::PickerPageDown => self.select_row_step(page as isize),
            Command::PickerAccept => return self.open_selected(),
            Command::PickerClose => return SearchAction::Close,
            command => {
                if let Some(toggle) = Toggle::for_command(command) {
                    self.toggle(toggle);
                }
            }
        }
        SearchAction::Continue
    }

    /// Adds typed or pasted text to the query.
    pub fn insert(&mut self, text: &str) {
        self.take_query().push_str(text);
        self.query_changed();
    }

    pub fn delete_backward(&mut self) {
        self.take_query().pop();
        self.query_changed();
    }

    pub fn delete_word_backward(&mut self) {
        picker::delete_word_backward(self.take_query());
        self.query_changed();
    }

    /// A click on a line opens the file there, on a toggle flips it; one
    /// outside the popup closes it. The wheel scrolls.
    pub fn handle_mouse(&mut self, mouse: Mouse) -> SearchAction {
        let area = self.area();
        let inside = area.contains(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::Press(_) if !inside => SearchAction::Close,
            MouseKind::Press(MouseButton::Left) if mouse.y == area.y + 1 => {
                let toggle = self
                    .toggles(area)
                    .into_iter()
                    .find(|(_, columns)| columns.contains(&mouse.x));
                if let Some((toggle, _)) = toggle {
                    self.toggle(toggle);
                }
                SearchAction::Continue
            }
            MouseKind::Press(MouseButton::Left) => {
                let body = area.y + 3;
                if mouse.y < body || mouse.y + 1 >= area.y + area.height {
                    return SearchAction::Continue;
                }
                let row = self.scroll + (mouse.y - body) as usize;
                self.click_row(row)
            }
            MouseKind::ScrollUp if inside => {
                self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
                SearchAction::Continue
            }
            MouseKind::ScrollDown if inside => {
                self.scroll = (self.scroll + WHEEL_ROWS).min(self.max_scroll());
                SearchAction::Continue
            }
            _ => SearchAction::Continue,
        }
    }

    /// Takes in what the search found since the last call. Returns whether
    /// the results changed.
    pub fn poll(&mut self) -> bool {
        let Some(search) = &mut self.search else {
            return false;
        };
        let found = search.poll();
        let done = search.done();
        if found.is_empty() && !done {
            return false;
        }
        self.truncated = search.truncated();
        if done {
            self.search = None;
        }
        if std::mem::take(&mut self.stale) {
            self.files.clear();
            self.selected = 0;
            self.moved = false;
            self.scroll = 0;
        }
        let mut reveal = false;
        for file in found {
            reveal |= self.add_file(file);
        }
        if done {
            // Its file didn't match this time.
            self.restore = None;
        }
        self.layout(reveal);
        true
    }

    /// Draws the popup over the middle of the screen and returns where the
    /// terminal cursor goes: the end of the query. Draws nothing on a
    /// screen too small for it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let area = self.area();
        if area.width < 16 || area.height < 5 {
            return None;
        }
        Some(
            frame.with_clip(area.x, area.y, area.width, area.height, || {
                self.draw_popup(frame, area)
            }),
        )
    }

    // --- searching -----------------------------------------------------------

    /// The query's text, emptied first if it was filled in on opening.
    fn take_query(&mut self) -> &mut String {
        if std::mem::take(&mut self.replace_query) {
            self.query.text.clear();
        }
        &mut self.query.text
    }

    fn toggle(&mut self, toggle: Toggle) {
        toggle.flip(&mut self.query);
        self.replace_query = false;
        self.query_changed();
    }

    fn query_changed(&mut self) {
        self.restore = None;
        self.start();
    }

    /// Searches for the query, keeping the current results on screen until
    /// the search finds something.
    fn start(&mut self) {
        self.search = None;
        self.error = None;
        self.truncated = false;
        if self.query.text.is_empty() {
            self.files.clear();
            self.stale = false;
            self.layout(false);
            return;
        }
        match Search::start(&self.workspace, &self.query, self.unsaved.clone()) {
            Ok(search) => {
                self.search = Some(search);
                self.stale = true;
            }
            Err(error) => {
                self.error = Some(error);
                self.files.clear();
                self.stale = false;
                self.layout(false);
            }
        }
    }

    /// Adds a file's matches in order, keeping a moved selection on the same
    /// match. Returns whether it selected the match to restore.
    fn add_file(&mut self, file: FileMatches) -> bool {
        let key = sort_key(&file.display);
        let at = self
            .files
            .partition_point(|other| sort_key(&other.display) < key);
        let before: usize = self.files[..at].iter().map(|f| f.match_count).sum();
        if self.moved && before <= self.selected && !self.matches.is_empty() {
            self.selected += file.match_count;
        }
        let restored = match &self.restore {
            Some((path, line, index)) if *path == file.path => {
                // The first match at or after where the last search left off.
                let skipped = file
                    .lines
                    .iter()
                    .flat_map(|l| (0..l.matches.len()).map(move |i| (l.number, i)))
                    .take_while(|&at| at < (*line, *index))
                    .count();
                self.selected = before + skipped.min(file.match_count - 1);
                self.moved = true;
                true
            }
            _ => false,
        };
        if restored {
            self.restore = None;
        }
        self.files.insert(at, file);
        restored
    }

    // --- selection -------------------------------------------------------------

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.matches.len().saturating_sub(1));
        self.moved = true;
        self.scroll_into_view();
    }

    /// Selects the first match about `step` rows below the selected one, or
    /// the last one about that far above it.
    fn select_row_step(&mut self, step: isize) {
        let Some(current) = self.matches.get(self.selected) else {
            return;
        };
        let target = current.row as isize + step;
        let index = if step > 0 {
            self.matches
                .iter()
                .position(|m| m.row as isize >= target)
                .unwrap_or(self.matches.len() - 1)
        } else {
            self.matches
                .iter()
                .rposition(|m| m.row as isize <= target)
                .unwrap_or(0)
        };
        self.select(index);
    }

    fn open_selected(&self) -> SearchAction {
        let Some(m) = self.matches.get(self.selected) else {
            return SearchAction::Continue;
        };
        let file = &self.files[m.file];
        let line = &file.lines[m.line];
        SearchAction::Open {
            path: file.path.clone(),
            line: line.number,
            range: line.matches[m.index].clone(),
        }
    }

    /// A click on a file's path opens its first match; on a line, the line's
    /// first match, or the line itself if it has none.
    fn click_row(&mut self, row: usize) -> SearchAction {
        let (file, line) = match self.rows.get(row) {
            Some(&Row::File(file)) => (file, None),
            Some(&Row::Line { file, line }) => (file, Some(line)),
            _ => return SearchAction::Continue,
        };
        let first = self
            .matches
            .iter()
            .position(|m| m.file == file && line.is_none_or(|line| m.line == line));
        match first {
            Some(index) => {
                self.select(index);
                self.open_selected()
            }
            None => {
                let file = &self.files[file];
                let number = line.map_or(0, |line| file.lines[line].number);
                SearchAction::Open {
                    path: file.path.clone(),
                    line: number,
                    range: 0..0,
                }
            }
        }
    }

    // --- layout ----------------------------------------------------------------

    /// Lays out the rows for the files. A selection that was moved keeps its
    /// place on screen; with `reveal`, it's scrolled into view.
    fn layout(&mut self, reveal: bool) {
        let offset = self
            .matches
            .get(self.selected)
            .map(|m| m.row as isize - self.scroll as isize);
        self.rows.clear();
        self.matches.clear();
        for (index, file) in self.files.iter().enumerate() {
            if index > 0 {
                self.rows.push(Row::Blank);
            }
            self.rows.push(Row::File(index));
            let mut previous: Option<u32> = None;
            for (line_index, line) in file.lines.iter().enumerate() {
                if previous.is_some_and(|previous| line.number != previous + 1) {
                    self.rows.push(Row::Gap);
                }
                previous = Some(line.number);
                for match_index in 0..line.matches.len() {
                    self.matches.push(MatchAt {
                        row: self.rows.len(),
                        file: index,
                        line: line_index,
                        index: match_index,
                    });
                }
                self.rows.push(Row::Line {
                    file: index,
                    line: line_index,
                });
            }
        }
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        if reveal {
            self.scroll_into_view();
        } else if let (Some(offset), Some(m), true) =
            (offset, self.matches.get(self.selected), self.moved)
        {
            self.scroll = (m.row as isize - offset).max(0) as usize;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Scrolls the selected match into view with the lines around it, and
    /// its file's path if it's the file's first match.
    fn scroll_into_view(&mut self) {
        let rows = self.body_rows().max(1);
        if let Some(m) = self.matches.get(self.selected) {
            let first_in_file =
                self.selected == 0 || self.matches[self.selected - 1].file != m.file;
            let top = if first_in_file {
                self.rows[..m.row]
                    .iter()
                    .rposition(|row| *row == Row::File(m.file))
                    .unwrap_or(0)
            } else {
                m.row.saturating_sub(CONTEXT_LINES)
            };
            let bottom = (m.row + CONTEXT_LINES + 1).min(self.rows.len());
            if top < self.scroll {
                self.scroll = top;
            } else if bottom > self.scroll + rows {
                self.scroll = bottom - rows;
            }
            // Too little room for the context: the match itself, at least.
            self.scroll = self.scroll.clamp((m.row + 1).saturating_sub(rows), m.row);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(self.body_rows())
    }

    /// Rows for results: all but the borders, the query, and the rule.
    fn body_rows(&self) -> usize {
        self.area().height.saturating_sub(4) as usize
    }

    fn area(&self) -> Area {
        let width = self
            .screen_width
            .saturating_sub(4)
            .min(MAX_WIDTH)
            .max(self.screen_width.min(30));
        let y = self.screen_height.min(1);
        Area {
            x: (self.screen_width - width) / 2,
            y,
            width,
            height: self
                .screen_height
                .saturating_sub(y + 1)
                .max(self.screen_height.min(5)),
        }
    }

    /// The toggles on the query row, right-aligned, with their columns.
    fn toggles(&self, area: Area) -> Vec<(Toggle, Range<u32>)> {
        let mut x = (area.x + area.width).saturating_sub(2 + 4 * Toggle::ALL.len() as u32);
        Toggle::ALL
            .iter()
            .map(|&toggle| {
                let columns = x..x + 4;
                x += 4;
                (toggle, columns)
            })
            .collect()
    }

    // --- drawing ---------------------------------------------------------------

    fn draw_popup(&self, frame: &Buffer, area: Area) -> (u32, u32) {
        let Area { x, y, width, .. } = area;
        picker::draw_frame(frame, area, "Search");
        let bottom = area.y + area.height - 1;
        let status = self.status();
        picker::draw_status(frame, area, &status);
        let hints_room = width.saturating_sub(status.chars().count() as u32 + 6) as usize;
        if self.hints.chars().count() <= hints_room {
            frame.draw_text(&self.hints, x + 2, bottom, DIM, None, Attributes::NONE);
        }

        // The query, its end kept in view, and the toggles right of it.
        let toggles = self.toggles(area);
        let text_x = x + 2;
        let room = toggles[0].1.start.saturating_sub(text_x + 1) as usize;
        let chars: Vec<char> = self.query.text.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        let query_bg = self.replace_query.then_some(SELECTED_BG);
        frame.draw_text(&shown, text_x, y + 1, FG, query_bg, Attributes::NONE);
        let cursor = (text_x + shown.chars().count() as u32, y + 1);
        if self.query.text.is_empty() {
            let hint: String = "Search in files"
                .chars()
                .take(room.saturating_sub(1))
                .collect();
            frame.draw_text(&hint, cursor.0 + 1, y + 1, DIM, None, Attributes::NONE);
        }
        for (toggle, columns) in toggles {
            let on = toggle.is_on(&self.query);
            let (fg, bg, attributes) = if on {
                (FG, Some(SELECTED_BG), Attributes::BOLD)
            } else {
                (DIM, None, Attributes::NONE)
            };
            let label = format!(" {} ", toggle.label());
            frame.draw_text(&label, columns.start, y + 1, fg, bg, attributes);
        }

        let body = y + 3;
        let room = width.saturating_sub(4);
        if let Some(message) = self.message() {
            let (text, fg) = message;
            let text: String = text.chars().take(room as usize).collect();
            frame.draw_text(&text, text_x, body, fg, None, Attributes::NONE);
            return cursor;
        }
        let selected = self.matches.get(self.selected);
        let rows = self.rows.iter().skip(self.scroll);
        for (row, screen_y) in rows.zip(body..bottom) {
            match *row {
                Row::File(file) => self.draw_file(frame, &self.files[file], text_x, screen_y, room),
                Row::Line { file, line } => {
                    let selected = selected
                        .filter(|m| m.file == file && m.line == line)
                        .map(|m| m.index);
                    let digits = digits(&self.files[file]);
                    let line = &self.files[file].lines[line];
                    self.draw_line(frame, line, digits, selected, text_x, screen_y, room);
                }
                Row::Gap => {
                    frame.draw_text(
                        "⋯",
                        text_x + 1,
                        screen_y,
                        LINE_NUMBER,
                        None,
                        Attributes::NONE,
                    );
                }
                Row::Blank => {}
            }
        }
        cursor
    }

    /// What to show instead of results, if anything.
    fn message(&self) -> Option<(String, Rgba)> {
        if let Some(error) = &self.error {
            return Some((format!("Invalid regex: {error}"), ERROR));
        }
        if !self.files.is_empty() && !self.query.text.is_empty() {
            return None;
        }
        let text = if self.query.text.is_empty() {
            "Type to search the workspace's files."
        } else if self.search.is_some() {
            "Searching…"
        } else {
            "No matches."
        };
        Some((text.to_string(), DIM))
    }

    /// The counts for the bottom border.
    fn status(&self) -> String {
        if self.query.text.is_empty() || self.error.is_some() {
            return String::new();
        }
        let total: usize = self.files.iter().map(|f| f.match_count).sum();
        if total == 0 {
            return String::new();
        }
        let plus = if self.truncated { "+" } else { "" };
        let files = match self.files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        let searching = if self.search.is_some() {
            " searching…"
        } else {
            ""
        };
        format!(
            " {} of {total}{plus} in {files}{searching} ",
            self.selected + 1
        )
    }

    /// A file's path, its folder dimmed, and its match count on the right.
    fn draw_file(&self, frame: &Buffer, file: &FileMatches, x: u32, y: u32, room: u32) {
        let count = file.match_count.to_string();
        let count_width = count.chars().count() as u32;
        let count_x = x + room.saturating_sub(count_width);
        frame.draw_text(&count, count_x, y, DIM, None, Attributes::NONE);
        let room = room.saturating_sub(count_width + 2) as usize;
        let chars: Vec<char> = file.display.chars().collect();
        // Cut on the left, to keep the name.
        let (shown, cut) = if chars.len() <= room {
            (&chars[..], false)
        } else {
            (&chars[chars.len() + 1 - room.max(1)..], true)
        };
        let mut x = x;
        if cut {
            frame.draw_text("…", x, y, DIM, None, Attributes::NONE);
            x += 1;
        }
        let name_start = shown
            .iter()
            .rposition(|&c| c == '/')
            .map_or(0, |slash| slash + 1);
        let folder: String = shown[..name_start].iter().collect();
        let name: String = shown[name_start..].iter().collect();
        frame.draw_text(&folder, x, y, DIM, None, Attributes::NONE);
        let name_x = x + folder.chars().count() as u32;
        frame.draw_text(&name, name_x, y, FG, None, Attributes::BOLD);
    }

    /// A line of a file: its number, then its text with matches highlighted,
    /// the `selected` one most. A match past the right edge is scrolled into
    /// view.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &self,
        frame: &Buffer,
        line: &Line,
        digits: usize,
        selected: Option<usize>,
        x: u32,
        y: u32,
        room: u32,
    ) {
        let (number_fg, number_attributes) = if selected.is_some() {
            (FG, Attributes::BOLD)
        } else {
            (LINE_NUMBER, Attributes::NONE)
        };
        let number = format!("{:>digits$}", line.number + 1);
        frame.draw_text(&number, x, y, number_fg, None, number_attributes);
        let text_x = x + digits as u32 + 2;
        let room = room.saturating_sub(digits as u32 + 2) as usize;
        if room < 2 {
            return;
        }

        let matches: Vec<Range<usize>> = line.matches_in_text().collect();
        let style = |byte: usize| -> Style {
            let hit = matches.iter().position(|m| m.contains(&byte));
            match hit {
                Some(index) if Some(index) == selected => {
                    (BG_TEXT, Some(MATCH_FG), Attributes::BOLD)
                }
                Some(_) => (MATCH_FG, Some(MATCH_BG), Attributes::BOLD),
                None => (FG, None, Attributes::NONE),
            }
        };
        let mut cells: Vec<(char, Style)> = Vec::new();
        // The columns of the match to keep in view: the selected one, or the
        // first.
        let focus = selected.or((!matches.is_empty()).then_some(0));
        let mut focus_columns = 0..0;
        for (byte, c) in line.text.char_indices() {
            let column = cells.len();
            let style = style(byte);
            match c {
                '\t' => {
                    let spaces = TAB_WIDTH - column % TAB_WIDTH;
                    cells.extend(std::iter::repeat_n((' ', style), spaces));
                }
                c if c.is_control() => cells.push((' ', style)),
                c => cells.push((c, style)),
            }
            if let Some(focus) = focus.map(|i| &matches[i]) {
                if byte == focus.start {
                    focus_columns.start = column;
                }
                if focus.contains(&byte) {
                    focus_columns.end = cells.len();
                }
            }
        }
        let shift = if focus_columns.end > room {
            focus_columns.start.saturating_sub(LEAD_COLUMNS)
        } else {
            0
        };
        let mut shown: Vec<(char, Style)> = cells.iter().skip(shift).take(room).copied().collect();
        if shown.is_empty() {
            return;
        }
        let ellipsis = ('…', (DIM, None, Attributes::NONE));
        if shift > 0 || line.start > 0 {
            shown[0] = ellipsis;
        }
        if cells.len() > shift + room || line.cut {
            let last = shown.len() - 1;
            shown[last] = ellipsis;
        }
        let mut x = text_x;
        for run in shown.chunk_by(|a, b| a.1 == b.1) {
            let text: String = run.iter().map(|(c, _)| c).collect();
            let (fg, bg, attributes) = run[0].1;
            frame.draw_text(&text, x, y, fg, bg, attributes);
            x += run.len() as u32;
        }
    }
}

type Style = (Rgba, Option<Rgba>, Attributes);

/// Text on a highlighted background.
const BG_TEXT: Rgba = Rgba::rgb(30, 30, 46);

/// Files sort by path ignoring case, as in the tree and the file picker.
fn sort_key(display: &str) -> (String, &str) {
    (display.to_lowercase(), display)
}

/// Columns for the line numbers of `file`.
fn digits(file: &FileMatches) -> usize {
    let last = file.lines.last().map_or(1, |line| line.number + 1);
    last.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use std::fs;
    use std::path::Path;

    /// A fresh workspace folder with `files` (name, contents).
    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("qedit-search-modal-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, contents) in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn modal(root: &Path, memory: Memory, text: Option<&str>, height: u32) -> SearchModal {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        SearchModal::new(
            &workspace,
            &Keymap::default(),
            memory,
            text.map(str::to_string),
            HashMap::new(),
            80,
            height,
        )
    }

    /// Waits for the search to finish.
    fn wait(modal: &mut SearchModal) {
        while modal.search.is_some() {
            modal.poll();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn screen(modal: &SearchModal) -> Vec<String> {
        let _serial = crate::test_serial();
        let frame = opentui::OwnedBuffer::new(
            modal.screen_width,
            modal.screen_height,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        frame.clear(Rgba::BLACK);
        modal.draw(&frame);
        frame.to_text(true).lines().map(str::to_string).collect()
    }

    fn opened(action: SearchAction) -> (String, u32, Range<usize>) {
        match action {
            SearchAction::Open { path, line, range } => (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                line,
                range,
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lists_excerpts_by_file_and_steps_through_matches() {
        let a = "one\ntwo\nfind me\nthree\nfour\nfive\nsix\nseven\nfind find\n";
        let root = fixture("excerpts", &[("b.txt", "find\n"), ("A/a.txt", a)]);
        let mut modal = modal(&root, Memory::default(), None, 30);
        modal.insert("find");
        wait(&mut modal);
        let text = screen(&modal).join("\n");
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let body: Vec<String> = lines[4..17]
            .iter()
            .map(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words[1..words.len() - 1].join(" ")
            })
            .collect();
        assert_eq!(
            body,
            [
                "A/a.txt 3",
                "1 one",
                "2 two",
                "3 find me",
                "4 three",
                "5 four",
                "⋯",
                "7 six",
                "8 seven",
                "9 find find",
                "",
                "b.txt 1",
                "1 find",
            ],
            "{text}"
        );
        assert!(lines[2].contains("│ find"), "{text}");
        assert!(text.contains("1 of 4 in 2 files"), "{text}");

        assert_eq!(
            opened(modal.run(Command::PickerAccept)),
            ("a.txt".into(), 2, 0..4)
        );
        modal.run(Command::PickerDown);
        modal.run(Command::PickerDown);
        assert_eq!(
            opened(modal.run(Command::PickerAccept)),
            ("a.txt".into(), 8, 5..9)
        );
        modal.run(Command::PickerDown);
        modal.run(Command::PickerDown);
        assert_eq!(
            opened(modal.run(Command::PickerAccept)),
            ("b.txt".into(), 0, 0..4)
        );
        modal.run(Command::PickerPageUp);
        assert_eq!(modal.selected, 0);
    }

    #[test]
    fn toggles_and_invalid_regexes() {
        let root = fixture("toggles", &[("a.txt", "Foo foo\n")]);
        let mut modal = modal(&root, Memory::default(), None, 12);
        modal.insert("foo");
        wait(&mut modal);
        assert_eq!(modal.matches.len(), 2);
        modal.run(Command::SearchToggleCase);
        wait(&mut modal);
        assert_eq!(modal.matches.len(), 1);

        // Clicking a toggle flips it.
        let area = modal.area();
        let (_, columns) = modal.toggles(area)[2].clone();
        modal.handle_mouse(Mouse {
            kind: MouseKind::Press(MouseButton::Left),
            x: columns.start + 1,
            y: area.y + 1,
            mods: Mods::NONE,
        });
        assert!(modal.query.regex);
        modal.insert("(");
        let text = screen(&modal).join("\n");
        assert!(text.contains("Invalid regex:"), "{text}");
        assert!(modal.matches.is_empty());
    }

    #[test]
    fn reopening_searches_again_and_keeps_the_selected_match() {
        let root = fixture("reopen", &[("a.txt", "x\nx\nx\n"), ("b.txt", "x\n")]);
        let mut first = modal(&root, Memory::default(), None, 20);
        first.insert("x");
        wait(&mut first);
        first.run(Command::PickerDown);
        first.run(Command::PickerDown);
        fs::write(root.join("a.txt"), "x\ny\nx\nx\n").unwrap();

        let mut second = modal(&root, first.memory(), None, 20);
        assert!(second.replace_query);
        wait(&mut second);
        assert_eq!(
            opened(second.run(Command::PickerAccept)),
            ("a.txt".into(), 2, 0..1)
        );

        // Typing replaces the query filled in on opening.
        second.insert("y");
        assert_eq!(second.query.text, "y");
        wait(&mut second);
        assert_eq!(second.matches.len(), 1);

        // Given text replaces the remembered query, and starts at the top.
        let mut third = modal(&root, first.memory(), Some("x"), 20);
        wait(&mut third);
        assert_eq!(
            opened(third.run(Command::PickerAccept)),
            ("a.txt".into(), 2, 0..1),
            "same query: picks up where it left off"
        );
        let mut fourth = modal(&root, first.memory(), Some("y"), 20);
        wait(&mut fourth);
        assert_eq!(fourth.selected, 0);
    }

    #[test]
    fn clicks_open_lines_and_scrolling_follows_the_selection() {
        let text: String = (0..60).map(|i| format!("line {i} hit\n")).collect();
        let root = fixture("clicks", &[("a.txt", &text)]);
        let mut modal = modal(&root, Memory::default(), None, 12);
        modal.insert("hit");
        wait(&mut modal);
        let rows = modal.body_rows();
        for _ in 0..20 {
            modal.run(Command::PickerDown);
        }
        let m = modal.matches[modal.selected];
        assert!(m.row >= modal.scroll && m.row < modal.scroll + rows);

        let area = modal.area();
        let click = |modal: &mut SearchModal, y| {
            modal.handle_mouse(Mouse {
                kind: MouseKind::Press(MouseButton::Left),
                x: area.x + 10,
                y,
                mods: Mods::NONE,
            })
        };
        let first_line = modal.scroll - 1;
        let (_, line, _) = opened(click(&mut modal, area.y + 3));
        assert_eq!(line as usize, first_line, "the file's path is row 0");
        assert_eq!(
            click(&mut modal, area.y + area.height + 1),
            SearchAction::Close
        );
    }

    #[test]
    fn long_lines_scroll_to_the_match() {
        // Scrolled sideways to the match.
        let wide = format!("{}needle{}", "x".repeat(100), "y".repeat(100));
        // Kept only from a little before the match.
        let long = format!("{}needle{}", "é".repeat(300), "y".repeat(300));
        let root = fixture("long", &[("a.txt", &wide), ("b.txt", &long)]);
        let mut modal = modal(&root, Memory::default(), None, 12);
        modal.insert("needle");
        wait(&mut modal);
        let text = screen(&modal).join("\n");
        let lines: Vec<&str> = text.lines().collect();
        let a = format!("│ 1  …{}needleyyy", "x".repeat(LEAD_COLUMNS - 1));
        assert!(lines[5].contains(&a), "{text}");
        assert!(lines[5].contains("yyy… │"), "{text}");
        assert!(lines[8].contains("│ 1  …éé"), "{text}");
        assert!(lines[8].contains("éneedleyyy"), "{text}");
    }
}
