//! The picker: a popup list that narrows as you type. It lists the
//! workspace's files (Ctrl+P), after what was shown recently, terminals and
//! untitled files included, or, when the query starts with `>`, every command with its
//! shortcut (Ctrl+K), as in Sublime and VS Code.
//!
//! As there, the query's first character can ask for something else: `@`
//! for the symbols the file on screen defines (Ctrl+R), `#` for those the
//! whole workspace does (Ctrl+Shift+R), `:` for a line to go to
//! (Ctrl+L), and `$` for just the terminals. A file's name can end in a line to go to, as compilers print
//! it: `main.rs:12` or `main.rs:12:5`.
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
use std::time::Instant;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use opentui::{Attributes, Buffer, Rgba};

use crate::file_index::FileIndex;
use crate::icons;
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::{Command, Context, Keymap};
use crate::line_edit::{Caret, Edit};
use crate::location::{self, Position};
use crate::symbols::{self, Symbol};
use crate::theme::{self, SyntaxColor, ThemeId};
use crate::workspace::Workspace;

/// The widest the popup gets, in columns.
const MAX_WIDTH: u32 = 90;
/// The most results shown at once.
const MAX_ROWS: u32 = 14;
/// Rows the mouse wheel scrolls.
const WHEEL_ROWS: usize = 3;

/// What the picker lists. Outside language selection, the query's first
/// character decides (see [`Mode::prefix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Files,
    Commands,
    Languages,
    Themes,
    /// A line in the file on screen.
    Line,
    /// The symbols the file on screen defines.
    Symbols,
    /// The symbols the workspace's files define.
    WorkspaceSymbols,
    /// The terminals, as listed among what was shown recently.
    Terminals,
}

impl Mode {
    /// The character that starts a query for this mode.
    fn prefix(self) -> Option<char> {
        match self {
            Mode::Files | Mode::Languages | Mode::Themes => None,
            Mode::Commands => Some('>'),
            Mode::Line => Some(':'),
            Mode::Symbols => Some('@'),
            Mode::WorkspaceSymbols => Some('#'),
            // A shell's prompt.
            Mode::Terminals => Some('$'),
        }
    }
}

/// What was picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    File(PathBuf),
    /// A file, at a line and maybe a column.
    FileAt(PathBuf, Position),
    /// A line, and maybe a column, in the file on screen.
    Line(Position),
    /// A symbol's name: in the file at the path, or on screen without one;
    /// on a line, 0-based, in its bytes there.
    Symbol(Option<PathBuf>, u32, Range<usize>),
    Command(Command),
    Language(Option<&'static str>),
    Theme(ThemeId),
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
    /// The color of the text that isn't dimmed: a symbol's name, as the
    /// editor colors it.
    color: Option<SyntaxColor>,
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
            color: None,
        }
    }

    /// Terminal `id`, labeled `name` and, dimmed, `about`, such as what's
    /// running in it.
    pub fn terminal(id: u32, name: &str, about: &str) -> Item {
        let text = if about.is_empty() {
            name.to_string()
        } else {
            format!("{name} · {about}")
        };
        Item {
            dim: name.chars().count()..text.chars().count(),
            text,
            detail: format!("#{id}"),
            choice: Choice::Terminal(id),
            color: None,
        }
    }

    /// Untitled file `number`, not yet saved.
    pub fn untitled(number: u32) -> Item {
        Item {
            text: crate::document::untitled_name(number),
            dim: 0..0,
            detail: "unsaved".to_string(),
            choice: Choice::Untitled(number),
            color: None,
        }
    }

    /// `symbol`, defined in the file on screen.
    pub fn symbol(symbol: Symbol) -> Item {
        let name = symbol.name.chars().count();
        let text = match symbol.container.is_empty() {
            true => symbol.name,
            false => format!("{} {}", symbol.name, symbol.container),
        };
        Item {
            dim: name..text.chars().count(),
            text,
            detail: symbol.kind.to_string(),
            color: symbols::color(symbol.kind),
            choice: Choice::Symbol(None, symbol.line, symbol.bytes),
        }
    }

    /// `symbol`, defined in the file at `path`, which is `shown` as named
    /// in the workspace.
    pub fn workspace_symbol(symbol: Symbol, path: PathBuf, shown: &str) -> Item {
        let name = symbol.name.chars().count();
        let text = format!("{} {shown}:{}", symbol.name, symbol.line + 1);
        Item {
            dim: name..text.chars().count(),
            text,
            detail: symbol.kind.to_string(),
            color: symbols::color(symbol.kind),
            choice: Choice::Symbol(Some(path), symbol.line, symbol.bytes),
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
            color: None,
        }
    }

    /// The file it is, if it's one.
    pub fn path(&self) -> Option<&Path> {
        match &self.choice {
            Choice::File(path) => Some(path),
            _ => None,
        }
    }
}

pub struct Picker {
    query: String,
    caret: Caret,
    /// What was shown recently, files and terminals, most recent first.
    /// Listed before the rest.
    recent: Vec<Item>,
    /// The terminals among `recent`, in terminal mode.
    terminals: Vec<Item>,
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
    languages: Vec<Item>,
    themes: Vec<Item>,
    /// The mode it's in when that isn't what the query starts with.
    fixed_mode: Option<Mode>,
    /// The symbols the file on screen defines, once given.
    outline: Option<Vec<Item>>,
    /// The workspace's symbols, from the app's symbol index.
    workspace_symbols: Rc<Vec<Item>>,
    /// Whether the workspace's symbols are still being indexed.
    indexing: bool,
    /// The workspace's symbols were asked for, and not yet given.
    wants_symbols: bool,
    /// What `:` and a line number go to, in line mode.
    line: Vec<Item>,
    /// The line a file's name in the query ends with, in file mode.
    position: Option<Position>,
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
            terminals: terminals(&recent),
            recent,
            files: Rc::new(Vec::new()),
            duplicates: Vec::new(),
            listing: false,
            truncated: false,
            close_hint: keymap
                .shortcut(Command::PickerCloseItem)
                .map_or(String::new(), |key| format!(" {key:#} close ")),
            commands,
            languages: std::iter::once(None)
                .chain(crate::language::all().map(|language| Some(language.name)))
                .map(|name| Item {
                    text: name.unwrap_or("Plain Text").to_string(),
                    dim: 0..0,
                    detail: String::new(),
                    choice: Choice::Language(name),
                    color: None,
                })
                .collect(),
            // Terminal, then the dark themes, then the light ones.
            themes: ThemeId::all()
                .filter(|id| id.light() != Some(true))
                .chain(ThemeId::all().filter(|id| id.light() == Some(true)))
                .map(|id| Item {
                    text: id.name().to_string(),
                    dim: 0..0,
                    detail: match id.light() {
                        None => "",
                        Some(true) => "light",
                        Some(false) => "dark",
                    }
                    .to_string(),
                    choice: Choice::Theme(id),
                    color: None,
                })
                .collect(),
            fixed_mode: None,
            outline: None,
            workspace_symbols: Rc::new(Vec::new()),
            indexing: false,
            wants_symbols: false,
            line: Vec::new(),
            position: None,
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

    /// Whether the command palette lists `command`.
    #[cfg(test)]
    pub fn lists_command(&self, command: Command) -> bool {
        self.commands
            .iter()
            .any(|item| item.choice == Choice::Command(command))
    }

    pub fn mode(&self) -> Mode {
        if let Some(mode) = self.fixed_mode {
            return mode;
        }
        let first = self.query.chars().next();
        [
            Mode::Commands,
            Mode::Line,
            Mode::Symbols,
            Mode::WorkspaceSymbols,
            Mode::Terminals,
        ]
        .into_iter()
        .find(|mode| first.is_some() && mode.prefix() == first)
        .unwrap_or(Mode::Files)
    }

    /// Switches to listing `mode`, keeping what was typed.
    pub fn set_mode(&mut self, mode: Mode) {
        let needle = self.needle().to_string();
        self.fixed_mode = matches!(mode, Mode::Languages | Mode::Themes).then_some(mode);
        self.query = match mode.prefix() {
            Some(prefix) => format!("{prefix}{needle}"),
            None => needle,
        };
        self.caret.move_to_end();
        self.query_changed();
    }

    /// Whether it lists the file's symbols, but wasn't given them yet
    /// (see [`Picker::set_outline`]).
    pub fn wants_outline(&self) -> bool {
        self.mode() == Mode::Symbols && self.outline.is_none()
    }

    /// Lists `items` as the symbols of the file on screen, in order.
    pub fn set_outline(&mut self, items: Vec<Item>) {
        self.outline = Some(items);
        if self.mode() == Mode::Symbols {
            self.refilter(None);
        }
    }

    /// Whether it lists the workspace's symbols, and hasn't asked for them
    /// since it opened: true once.
    pub fn take_symbols_request(&mut self) -> bool {
        if self.mode() != Mode::WorkspaceSymbols || self.wants_symbols {
            return false;
        }
        self.wants_symbols = true;
        true
    }

    /// Lists `items` as the workspace's symbols, keeping the selection on
    /// the same one if it's still there. `indexing` says the first index
    /// isn't complete.
    pub fn set_workspace_symbols(&mut self, items: Rc<Vec<Item>>, indexing: bool) {
        let selected = self.selected_item().map(|item| item.text.clone());
        self.workspace_symbols = items;
        self.indexing = indexing;
        if self.mode() == Mode::WorkspaceSymbols {
            self.refilter(selected);
        }
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
        self.terminals = terminals(&recent);
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
                let recent = match self.mode() {
                    Mode::Files => self
                        .matches
                        .get(self.selected)
                        .is_some_and(|&i| i < self.recent.len()),
                    Mode::Terminals => true,
                    _ => false,
                };
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

    /// Edits the query with Shift held: moving the cursor selects.
    pub fn edit_selecting(&mut self, edit: Edit) {
        if self.caret.select(&mut self.query, edit) {
            self.query_changed();
        }
    }

    pub fn select_all(&mut self) {
        self.caret.select_all(&self.query);
    }

    /// The part of the query selected.
    pub fn selected_text(&self) -> Option<&str> {
        self.caret.selected_text(&self.query)
    }

    /// A click on a result picks it; one outside the popup closes it. In
    /// the query, a click puts the cursor, and a drag or a double or triple
    /// click selects.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> PickerAction {
        let area = self.area();
        let inside = area.contains(mouse.x, mouse.y);
        let column = mouse.x.saturating_sub(area.x + 2) as usize;
        match mouse.kind {
            MouseKind::Drag(MouseButton::Left) => self.caret.drag(&self.query, column),
            MouseKind::Release(_) => {
                self.caret.release(&self.query);
            }
            _ => {}
        }
        if self
            .preview_area()
            .is_some_and(|area| area.contains(mouse.x, mouse.y))
        {
            return PickerAction::Continue;
        }
        match mouse.kind {
            MouseKind::Press(_) if !inside => PickerAction::Close,
            MouseKind::Press(MouseButton::Left) if mouse.y == area.y + 1 => {
                self.caret.press(&self.query, column, now);
                PickerAction::Continue
            }
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
            MouseKind::Move if inside => {
                let list = area.y + 3;
                if mouse.y >= list && mouse.y + 1 < area.y + area.height {
                    let index = self.scroll + (mouse.y - list) as usize;
                    if self
                        .matches
                        .get(index)
                        .is_some_and(|&i| matches!(self.item(i).choice, Choice::Terminal(_)))
                    {
                        self.selected = index;
                    }
                }
                PickerAction::Continue
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

    /// The query without the character that asks for the mode, or in file
    /// mode, the line a name ends with.
    fn needle(&self) -> &str {
        let mode = self.mode();
        match mode.prefix() {
            Some(prefix) => &self.query[prefix.len_utf8()..],
            None if mode == Mode::Files => match location::split_position(&self.query) {
                (name, Some(_)) if !name.is_empty() => name,
                _ => &self.query,
            },
            None => &self.query,
        }
    }

    fn query_changed(&mut self) {
        // A new query starts at the best match.
        self.selected = 0;
        self.scroll = 0;
        self.position = match self.mode() {
            Mode::Files => match location::split_position(&self.query) {
                (name, position) if !name.is_empty() => position,
                _ => None,
            },
            _ => None,
        };
        self.line = match self.mode() {
            Mode::Line => line_item(self.needle()).into_iter().collect(),
            _ => Vec::new(),
        };
        self.refilter(None);
    }

    /// The items listed in the current mode: recent files, then the rest,
    /// which include the recent ones again (see `duplicates`).
    fn lists(&self) -> (&[Item], &[Item]) {
        match self.mode() {
            Mode::Files => (&self.recent, &self.files),
            Mode::Commands => (&self.commands, &[]),
            Mode::Languages => (&self.languages, &[]),
            Mode::Themes => (&self.themes, &[]),
            Mode::Line => (&self.line, &[]),
            Mode::Symbols => (self.outline.as_deref().unwrap_or_default(), &[]),
            Mode::WorkspaceSymbols => (&self.workspace_symbols, &[]),
            Mode::Terminals => (&self.terminals, &[]),
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

    /// What's selected, as Enter would pick it.
    pub fn selected_choice(&self) -> Option<&Choice> {
        self.selected_item().map(|item| &item.choice)
    }

    /// Selects the item with the text `text`, if it's listed.
    pub fn select_text(&mut self, text: &str) {
        self.refilter(Some(text.to_string()));
    }

    /// Matches the query against every item, selecting the item with the
    /// text `selected` if it matches. Better matches come first; among equal
    /// ones, recent files, then shorter paths.
    fn refilter(&mut self, selected: Option<String>) {
        // The line's number is what's asked for; it isn't matched.
        let needle = match self.mode() {
            Mode::Line => "",
            _ => self.needle(),
        };
        self.pattern = Pattern::parse(needle, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = self.matcher.borrow_mut();
        let (first, rest) = self.lists();
        let mut skip = match self.mode() {
            Mode::Files => &self.duplicates[..],
            _ => &[],
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
        let Some(item) = self.selected_item() else {
            return PickerAction::Continue;
        };
        PickerAction::Accept(match (&item.choice, self.position) {
            (Choice::File(path), Some(position)) => Choice::FileAt(path.clone(), position),
            (choice, _) => choice.clone(),
        })
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
        let mut room = self.screen_height.saturating_sub(5);
        if self.has_preview_space() {
            room = room.saturating_sub(self.preview_height() + 1);
        }
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

    // Reserve vertical space only when the selected result has a preview.
    fn has_preview_space(&self) -> bool {
        matches!(self.mode(), Mode::Files | Mode::Terminals)
            && !self.terminals.is_empty()
            && self.screen_width >= 32
            && self.screen_height >= 16
            && matches!(self.selected_choice(), Some(Choice::Terminal(_)))
    }

    fn preview_height(&self) -> u32 {
        (self.screen_height / 2 + 3)
            .min(18)
            .min(self.screen_height.saturating_sub(8))
    }

    /// A separate, read-only viewport for the selected terminal.
    pub fn preview_area(&self) -> Option<Area> {
        if !self.has_preview_space() {
            return None;
        }
        let list = self.area();
        Some(Area {
            x: list.x,
            y: list.y + list.height + 1,
            width: list.width,
            height: self.preview_height(),
        })
    }

    fn draw_popup(&self, frame: &Buffer, area: Area) -> (u32, u32) {
        let colors = theme::colors();
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        let (title, noun) = match self.mode() {
            Mode::Files => ("Go to File", "files"),
            Mode::Commands => ("Commands", "commands"),
            Mode::Languages => ("Syntax Highlighting", "languages"),
            Mode::Themes => ("Select Theme", "themes"),
            Mode::Line => ("Go to Line", "lines"),
            Mode::Symbols => ("Go to Symbol in File", "symbols"),
            Mode::WorkspaceSymbols => ("Go to Symbol in Workspace", "symbols"),
            Mode::Terminals => ("Go to Terminal", "terminals"),
        };
        draw_frame(frame, area, title);
        let status = self.status(noun);
        draw_status(frame, area, &status);
        let room = width.saturating_sub(status.chars().count() as u32 + 6) as usize;
        let closable = match self.mode() {
            Mode::Files => !self.recent.is_empty(),
            Mode::Terminals => !self.terminals.is_empty(),
            _ => false,
        };
        if closable && self.close_hint.chars().count() <= room {
            let bottom = y + height - 1;
            frame.draw_text(
                &self.close_hint,
                x + 2,
                bottom,
                colors.muted,
                None,
                Attributes::NONE,
            );
        }

        // The query, the cursor kept in view.
        let text_x = x + 2;
        let room = width.saturating_sub(4) as usize;
        let (shown, column) = self.caret.view(&self.query, room);
        let shown_width = shown.chars().count();
        draw_selection(frame, &self.caret, &self.query, text_x, y + 1, room);
        frame.draw_text(&shown, text_x, y + 1, colors.text, None, Attributes::NONE);
        let cursor = (text_x + column as u32, y + 1);
        if self.needle().is_empty() {
            let hint = match self.mode() {
                Mode::Files => {
                    "Search files by name. Type > for commands, @ for symbols, or $ for terminals."
                }
                Mode::Commands => "Search commands",
                Mode::Languages => "Search languages",
                Mode::Themes => "Search themes",
                Mode::Line => "Enter a line number or line:column.",
                Mode::Symbols => "Search symbols in this file",
                Mode::WorkspaceSymbols => "Search symbols in the workspace",
                Mode::Terminals => "Search terminals by name or running program.",
            };
            let hint: String = hint
                .chars()
                .take(room.saturating_sub(shown_width + 1))
                .collect();
            let hint_x = text_x + shown_width as u32 + 1;
            frame.draw_text(&hint, hint_x, y + 1, colors.muted, None, Attributes::NONE);
        }

        let list = y + 3;
        if self.matches.is_empty() {
            let (first, rest) = self.lists();
            let empty = match self.mode() {
                _ if self.listing() => "Listing files…".to_string(),
                Mode::WorkspaceSymbols if self.indexing => "Indexing symbols…".to_string(),
                Mode::Line if !self.needle().is_empty() => "Not a line number".to_string(),
                Mode::Line => String::new(),
                Mode::Symbols if self.outline.is_none() => String::new(),
                Mode::Symbols if first.is_empty() => "No symbols in this file".to_string(),
                // The one on screen isn't listed.
                Mode::Terminals if first.is_empty() => "No other terminals".to_string(),
                _ if first.is_empty() && rest.is_empty() => format!("No {noun}"),
                _ => format!("No matching {noun}"),
            };
            frame.draw_text(&empty, text_x, list, colors.muted, None, Attributes::NONE);
        }
        let visible = self.matches.iter().enumerate().skip(self.scroll);
        for ((index, &item), row) in visible.zip(list..y + height - 1) {
            if index == self.selected {
                frame.fill_rect(x + 1, row, width.saturating_sub(2), 1, colors.selected);
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
        if self.mode() == Mode::Line {
            return String::new();
        }
        let (first, rest) = self.lists();
        let mut total = first.len() + rest.len();
        if self.mode() == Mode::Files {
            total -= self.duplicates.len();
        }
        let listing = if self.listing() {
            " listing…"
        } else if self.indexing && self.mode() == Mode::WorkspaceSymbols {
            " indexing…"
        } else if self.truncated && self.mode() == Mode::Files {
            " (list truncated)"
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
        let colors = theme::colors();
        let icon = match &item.choice {
            Choice::File(path) => Some(icons::file(
                &path.file_name().unwrap_or_default().to_string_lossy(),
            )),
            Choice::Untitled(_) => Some(icons::file("")),
            Choice::Terminal(_) => Some(icons::terminal()),
            _ => None,
        };
        let (x, room) = match icon.filter(|_| icons::enabled() && room > 2 * icons::WIDTH as usize)
        {
            Some(icon) => (icon.draw(frame, x, y, None), room - icons::WIDTH as usize),
            None => (x, room),
        };
        let detail = item.detail.chars().count();
        if detail > 0 && detail + 2 < room {
            let detail_x = x + (room - detail) as u32;
            frame.draw_text(
                &item.detail,
                detail_x,
                y,
                colors.muted,
                None,
                Attributes::NONE,
            );
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
        // A colored name's matches keep its color, as the match color
        // could be the name's own.
        let color = item
            .color
            .map(|color| (color.fg().unwrap_or(colors.text), color.attributes()));
        let style = |i: usize| {
            let dim = item.dim.contains(&i);
            match (highlights.contains(&i), color.filter(|_| !dim)) {
                (true, Some((fg, attributes))) => {
                    (fg, attributes | Attributes::BOLD | Attributes::UNDERLINE)
                }
                (true, None) => (colors.accent, Attributes::BOLD),
                (false, _) if dim => (colors.muted, Attributes::NONE),
                (false, Some(style)) => style,
                (false, None) => (colors.text, Attributes::NONE),
            }
        };
        let mut draw = |text: &str, (fg, attributes): (Rgba, Attributes)| {
            frame.draw_text(text, x, y, fg, None, attributes);
            x += text.chars().count() as u32;
        };
        let cut_left = shown.start > 0;
        if cut_left {
            draw(ellipsis, (colors.muted, Attributes::NONE));
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
            draw(ellipsis, (colors.muted, Attributes::NONE));
        }
    }
}

/// The terminals among `recent`, in order.
fn terminals(recent: &[Item]) -> Vec<Item> {
    recent
        .iter()
        .filter(|item| matches!(item.choice, Choice::Terminal(_)))
        .cloned()
        .collect()
}

/// What `needle`, a line number or `line:column` typed after `:`, goes to.
fn line_item(needle: &str) -> Option<Item> {
    let query = format!(":{}", needle.trim());
    let (rest, position) = location::split_position(&query);
    let position = position.filter(|_| rest.is_empty())?;
    let text = match position.column {
        Some(column) => format!("Go to line {}, column {}", position.line + 1, column + 1),
        None => format!("Go to line {}", position.line + 1),
    };
    Some(Item {
        text,
        dim: 0..0,
        detail: String::new(),
        choice: Choice::Line(position),
        color: None,
    })
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

/// Clears `area` and draws a popup's box around it, with `title`, if any, in
/// the top border and a rule under the first row, which holds the query.
pub fn draw_frame(frame: &Buffer, area: Area, title: &str) {
    let colors = theme::colors();
    let Area {
        x,
        y,
        width,
        height,
    } = area;
    let right = x + width - 1;
    frame.fill_rect(x, y, width, height, colors.bg);
    let rule = "─".repeat(width.saturating_sub(2) as usize);
    let border = |left: &str, right_end: &str, y: u32| {
        frame.draw_text(
            &format!("{left}{rule}{right_end}"),
            x,
            y,
            colors.border,
            None,
            Attributes::NONE,
        );
    };
    border("╭", "╮", y);
    border("├", "┤", y + 2);
    border("╰", "╯", y + height - 1);
    for row in y + 1..y + height - 1 {
        if row != y + 2 {
            frame.draw_text("│", x, row, colors.border, None, Attributes::NONE);
            frame.draw_text("│", right, row, colors.border, None, Attributes::NONE);
        }
    }
    if !title.is_empty() {
        frame.draw_text(
            &format!(" {title} "),
            x + 2,
            y,
            colors.text,
            None,
            Attributes::BOLD,
        );
    }
}

/// Draws `status` into the right of a popup's bottom border.
/// Shades the part of a field's `text` selected, under where it's drawn
/// from `x`, in `room` columns.
pub fn draw_selection(frame: &Buffer, caret: &Caret, text: &str, x: u32, y: u32, room: usize) {
    if let Some(columns) = caret.selected_columns(text) {
        let end = columns.end.min(room);
        if columns.start < end {
            let width = (end - columns.start) as u32;
            let x = x + columns.start as u32;
            frame.fill_rect(x, y, width, 1, theme::colors().selection);
        }
    }
}

pub fn draw_status(frame: &Buffer, area: Area, status: &str) {
    let colors = theme::colors();
    let x = (area.x + area.width).saturating_sub(status.chars().count() as u32 + 2);
    let y = area.y + area.height - 1;
    frame.draw_text(status, x, y, colors.muted, None, Attributes::NONE);
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
    fn terminal_previews_fit_and_leave_clicks_in_the_preview_alone() {
        let root = fixture("terminal-preview", &[]);
        let mut picker = picker(&root, Mode::Terminals, Vec::new());
        picker.set_recent((1..20).map(|id| Item::terminal(id, "cue", "zsh")).collect());
        for (width, height) in [(80, 24), (120, 24), (160, 40), (32, 16)] {
            picker.set_size(width, height);
            let list = picker.area();
            let preview = picker.preview_area().unwrap();
            assert!(preview.x + preview.width <= width);
            assert!(preview.y + preview.height <= height);
            assert!(!list.contains(preview.x, preview.y));
            let choice = picker.selected_choice().cloned();
            assert_eq!(
                picker.handle_mouse(
                    Mouse {
                        kind: MouseKind::Press(MouseButton::Left),
                        x: preview.x + 2,
                        y: preview.y + 3,
                        mods: crate::input::Mods::NONE,
                    },
                    Instant::now()
                ),
                PickerAction::Continue
            );
            assert_eq!(picker.selected_choice(), choice.as_ref());
            picker.run(Command::PickerDown);
            assert_ne!(picker.selected_choice(), choice.as_ref());
        }
        picker.set_size(30, 10);
        assert!(picker.preview_area().is_none());
    }

    #[test]
    fn a_leading_dollar_lists_only_terminals() {
        let root = fixture("terminals", &["build.rs", "a.rs"]);
        let (workspace, index) = index(&root);
        let recent = vec![
            Item::file(root.join("build.rs"), &workspace, "recent"),
            Item::terminal(1, "shell", "cargo build"),
            Item::untitled(1),
            Item::terminal(2, "server", ""),
        ];
        let mut picker = Picker::new(
            Mode::Files,
            &Keymap::default(),
            |_| true,
            recent,
            &index,
            80,
            24,
        );
        picker.edit(Edit::Insert("$"));
        assert_eq!(picker.mode(), Mode::Terminals);
        assert_eq!(listed(&picker), ["shell · cargo build", "server"]);
        // What's running in it matches too, but files don't.
        picker.edit(Edit::Insert("build"));
        assert_eq!(listed(&picker), ["shell · cargo build"]);
        assert_eq!(
            picker.run(Command::PickerAccept),
            PickerAction::Accept(Choice::Terminal(1))
        );
        assert_eq!(
            picker.run(Command::PickerCloseItem),
            PickerAction::CloseItem(Choice::Terminal(1))
        );
        picker.set_recent(vec![Item::terminal(2, "server", "")]);
        assert!(listed(&picker).is_empty());
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
        picker.handle_mouse(at(MouseKind::ScrollDown, 40, area.y + 5), Instant::now());
        assert_eq!(picker.scroll, WHEEL_ROWS);
        // The first result row is below the top border, query, and rule.
        assert_eq!(
            picker.handle_mouse(
                at(MouseKind::Press(MouseButton::Left), 40, area.y + 4),
                Instant::now()
            ),
            PickerAction::Accept(Choice::File(root.join("f04.rs")))
        );
        assert_eq!(
            picker.handle_mouse(
                at(
                    MouseKind::Press(MouseButton::Left),
                    40,
                    area.y + area.height
                ),
                Instant::now()
            ),
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

        // The query's selection is shaded.
        picker.edit(Edit::Left);
        picker.edit_selecting(Edit::Left);
        picker.draw(&screen);
        let x = picker.area().x + 2;
        let selection = theme::colors().selection;
        let shaded: Vec<bool> = (x..x + 5)
            .map(|x| screen.bg_at(x, 2) == Some(selection))
            .collect();
        assert_eq!(shaded, [false, false, true, false, false]);
    }

    #[test]
    fn symbols_are_colored_by_kind() {
        let colors = theme::colors();
        let _serial = crate::test_serial();
        let root = fixture("symbol-colors", &["a.rs"]);
        let (_, index) = index(&root);
        let mut picker = Picker::new(
            Mode::Symbols,
            &Keymap::default(),
            |_| true,
            Vec::new(),
            &index,
            60,
            12,
        );
        let symbol = |name: &str, kind, container: &str| {
            Item::symbol(Symbol {
                name: name.to_string(),
                kind,
                line: 0,
                bytes: 0..name.len(),
                container: container.to_string(),
            })
        };
        picker.set_outline(vec![
            symbol("Point", "struct", ""),
            symbol("norm", "method", "Point"),
            symbol("geometry", "module", ""),
        ]);
        let screen =
            opentui::OwnedBuffer::new(60, 12, false, opentui::WidthMethod::Unicode, "test")
                .unwrap();
        let draw = |picker: &Picker| {
            screen.clear(Rgba::BLACK);
            picker.draw(&screen);
            let text = screen.to_text(true);
            // The column of `name` in its row, and the row.
            move |name: &str| {
                let (y, line) = text
                    .lines()
                    .enumerate()
                    .find(|(_, line)| line.contains(&format!("│ {name}")))
                    .unwrap_or_else(|| panic!("{name}:\n{text}"));
                let x = line[..line.find(name).unwrap()].chars().count() as u32;
                (x, y as u32)
            }
        };
        let fg = |(x, y): (u32, u32)| screen.fg_at(x, y).unwrap();
        let at = draw(&picker);
        // As the editor colors them: types, functions; modules plainly.
        assert_eq!(
            fg(at("Point")),
            SyntaxColor::of("type").unwrap().fg().unwrap()
        );
        let norm = at("norm");
        assert_eq!(fg(norm), SyntaxColor::of("function").unwrap().fg().unwrap());
        // What it's in is dimmed, not colored.
        assert_eq!(fg((norm.0 + 5, norm.1)), colors.muted);
        assert_eq!(fg(at("geometry")), colors.text);

        // A match keeps the name's color, bold and underlined.
        picker.edit(Edit::Insert("no"));
        let norm = draw(&picker)("norm");
        assert_eq!(fg(norm), SyntaxColor::of("function").unwrap().fg().unwrap());
        let attributes = screen.attributes_at(norm.0, norm.1).unwrap();
        assert_eq!(
            attributes.0 & (Attributes::BOLD | Attributes::UNDERLINE).0,
            (Attributes::BOLD | Attributes::UNDERLINE).0
        );
        assert_eq!(
            fg((norm.0 + 2, norm.1)),
            SyntaxColor::of("function").unwrap().fg().unwrap()
        );
    }
}
