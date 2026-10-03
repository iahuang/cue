//! `cue --resume`: the sessions to go back to, full screen, before cue
//! starts. It lists those of the folder it's run in, or with Tab (or
//! `--all`), every one, most recent first, and narrows them as you type:
//! by folder, tab, file, program, or id.
//!
//! It runs in the `cue` the shell ran (see [`crate::client`]), which then
//! attaches to the session chosen, or starts a cue for it.

use std::io;
use std::path::Path;
use std::time::{Instant, SystemTime};

use opentui::{Attributes, Buffer, Output, Renderer, Rgba};

use crate::client::tilde;
use crate::config;
use crate::input::{Event, Key, KeyCode, Mouse, MouseButton, MouseKind, Parser, MULTI_CLICK};
use crate::keymap::{Command, Context, Keymap};
use crate::line_edit::{Caret, Edit};
use crate::session::Listing;
use crate::theme::{self, TerminalColors};
use crate::tree::truncate;
use crate::tty;

/// Rows a session takes in the list: two lines, and a blank one after.
const ITEM_ROWS: u32 = 3;
/// The first row of the list.
const LIST_TOP: u32 = 4;
/// Rows the mouse wheel moves.
const WHEEL_ROWS: usize = 1;
/// Files named in a session's details before the rest are counted.
const FILES_NAMED: usize = 4;

/// What a key, a click, or a paste did.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    /// Open the session at this index of those given.
    Open(usize),
    Cancel,
}

/// The list, and what's typed to narrow it.
pub struct Chooser {
    listings: Vec<Listing>,
    /// What each listing is found by, lowercase.
    haystacks: Vec<String>,
    /// The folder it's run in, as shown.
    folder: String,
    /// Which listings have the folder in their workspace.
    here: Vec<bool>,
    /// Every session, rather than the folder's.
    all: bool,
    query: String,
    caret: Caret,
    /// The listings shown, by index, most recent first.
    shown: Vec<usize>,
    /// Of those shown.
    selected: usize,
    /// The first shown in view.
    scroll: usize,
    width: u32,
    height: u32,
    keymap: Keymap,
    now: SystemTime,
    /// The last left press on a session, to tell a double click.
    last_click: Option<(Instant, usize)>,
}

impl Chooser {
    /// Lists `listings`, most recent first, those with `folder` in their
    /// workspace unless `all`, on a screen `width` x `height`.
    pub fn new(
        listings: Vec<Listing>,
        folder: &Path,
        all: bool,
        keymap: Keymap,
        width: u32,
        height: u32,
    ) -> Chooser {
        let haystacks = listings.iter().map(haystack).collect();
        let here = listings.iter().map(|listing| listing.has(folder)).collect();
        let mut chooser = Chooser {
            listings,
            haystacks,
            folder: tilde(folder),
            here,
            all,
            query: String::new(),
            caret: Caret::default(),
            shown: Vec::new(),
            selected: 0,
            scroll: 0,
            width,
            height,
            keymap,
            now: SystemTime::now(),
            last_click: None,
        };
        chooser.refilter();
        chooser
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.keep_in_view();
    }

    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                // Terminals send newlines in pastes as CR.
                let line = text.split(['\r', '\n']).next().unwrap_or("");
                self.edit(Edit::Insert(line));
                Action::Continue
            }
            Event::Reply(_) => Action::Continue,
        }
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key.code == KeyCode::Tab {
            self.toggle_all();
            return Action::Continue;
        }
        if key.code == KeyCode::Char('c') && key.mods.ctrl {
            return Action::Cancel;
        }
        match self.keymap.lookup(key, Context::Picker).map(|(c, _)| c) {
            Some(Command::PickerUp) => self.step(-1),
            Some(Command::PickerDown) => self.step(1),
            Some(Command::PickerPageUp) => self.step(-(self.rows_in_view() as isize)),
            Some(Command::PickerPageDown) => self.step(self.rows_in_view() as isize),
            Some(Command::PickerAccept) => return self.open_selected(),
            // A full screen to leave: Esc clears what's typed first.
            Some(Command::PickerClose) if !self.query.is_empty() => {
                self.query.clear();
                self.caret.move_to_end();
                self.refilter();
            }
            Some(Command::PickerClose) => return Action::Cancel,
            _ => self.edit_key(key),
        }
        Action::Continue
    }

    /// A key for the query: typing, or the editor's keys for moving the
    /// cursor and deleting.
    fn edit_key(&mut self, key: Key) {
        let mut buf = [0; 4];
        let edit = match self.keymap.lookup(key, Context::Editor).map(|(c, _)| c) {
            Some(Command::DeleteBackward) => Edit::DeleteBackward,
            Some(Command::DeleteForward) => Edit::DeleteForward,
            Some(Command::DeleteWordBackward) => Edit::DeleteWordBackward,
            Some(Command::DeleteWordForward) => Edit::DeleteWordForward,
            Some(Command::CursorLeft) => Edit::Left,
            Some(Command::CursorRight) => Edit::Right,
            Some(Command::WordLeft) => Edit::WordLeft,
            Some(Command::WordRight) => Edit::WordRight,
            Some(Command::LineStart | Command::DocumentStart) => Edit::Start,
            Some(Command::LineEnd | Command::DocumentEnd) => Edit::End,
            Some(_) => return,
            None => match key.code {
                KeyCode::Char(c) if key.mods.is_plain() => Edit::Insert(c.encode_utf8(&mut buf)),
                _ => return,
            },
        };
        self.edit(edit);
    }

    fn edit(&mut self, edit: Edit) {
        if self.caret.edit(&mut self.query, edit) {
            self.refilter();
        }
    }

    fn handle_mouse(&mut self, mouse: Mouse) -> Action {
        match mouse.kind {
            MouseKind::ScrollUp => self.step(-(WHEEL_ROWS as isize)),
            MouseKind::ScrollDown => self.step(WHEEL_ROWS as isize),
            MouseKind::Press(MouseButton::Left) => {
                if mouse.y == 0 {
                    let (here, all) = self.scope_tabs();
                    if here.contains(&mouse.x) && self.all || all.contains(&mouse.x) && !self.all {
                        self.toggle_all();
                    }
                    return Action::Continue;
                }
                let Some(index) = self.item_at(mouse.y) else {
                    return Action::Continue;
                };
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(time, clicked)| {
                    clicked == index && now.duration_since(time) < MULTI_CLICK
                });
                self.selected = index;
                self.keep_in_view();
                if double {
                    self.last_click = None;
                    return self.open_selected();
                }
                self.last_click = Some((now, index));
            }
            _ => {}
        }
        Action::Continue
    }

    fn open_selected(&self) -> Action {
        match self.shown.get(self.selected) {
            Some(&index) => Action::Open(index),
            None => Action::Continue,
        }
    }

    fn toggle_all(&mut self) {
        self.all = !self.all;
        self.refilter();
    }

    /// Lists the sessions in scope that have every word typed, keeping the
    /// one selected if it still is.
    fn refilter(&mut self) {
        let selected = self.shown.get(self.selected).copied();
        let words: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.shown = (0..self.listings.len())
            .filter(|&i| self.all || self.here[i])
            .filter(|&i| words.iter().all(|word| self.haystacks[i].contains(word)))
            .collect();
        self.selected = selected
            .and_then(|selected| self.shown.iter().position(|&i| i == selected))
            .unwrap_or(0);
        self.scroll = 0;
        self.keep_in_view();
    }

    fn step(&mut self, step: isize) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() as isize - 1;
        self.selected = (self.selected as isize + step).clamp(0, last) as usize;
        self.keep_in_view();
    }

    /// Sessions that fit in the list.
    fn rows_in_view(&self) -> usize {
        let rows = self.height.saturating_sub(LIST_TOP + 1);
        // The last needs no blank row after it.
        ((rows + 1) / ITEM_ROWS).max(1) as usize
    }

    fn keep_in_view(&mut self) {
        let rows = self.rows_in_view();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.scroll = self.scroll.min(self.shown.len().saturating_sub(rows));
    }

    /// The session shown on row `y`, by its place among those shown.
    fn item_at(&self, y: u32) -> Option<usize> {
        let row = y.checked_sub(LIST_TOP)?;
        if y + 1 >= self.height || row % ITEM_ROWS == ITEM_ROWS - 1 {
            return None;
        }
        let index = self.scroll + (row / ITEM_ROWS) as usize;
        (index < self.shown.len()).then_some(index)
    }

    /// The columns of the header's two scopes: this folder's sessions, and
    /// all of them.
    fn scope_tabs(&self) -> (std::ops::Range<u32>, std::ops::Range<u32>) {
        let all_x = self.width.saturating_sub(SCOPE_ALL.len() as u32 + 1);
        let here_x = all_x.saturating_sub(SCOPE_HERE.len() as u32 + 1);
        (
            here_x..here_x + SCOPE_HERE.len() as u32,
            all_x..all_x + SCOPE_ALL.len() as u32,
        )
    }

    // --- drawing ------------------------------------------------------------------

    /// Draws the whole screen, and returns where the cursor goes: in the
    /// query.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let (width, height) = (self.width, self.height);
        frame.clear(colors.bg);
        if width < 10 || height < LIST_TOP + 2 {
            return None;
        }

        // The header: what this is, and the scopes.
        frame.fill_rect(0, 0, width, 1, colors.surface);
        let (here, all) = self.scope_tabs();
        let title = " Resume Session";
        frame.draw_text(title, 0, 0, colors.text, None, Attributes::BOLD);
        let folder = format!("  {}", self.folder);
        let room = (here.start as usize).saturating_sub(title.len() + 1);
        let folder = truncate(&folder, room);
        frame.draw_text(
            &folder,
            title.len() as u32,
            0,
            colors.muted,
            None,
            Attributes::NONE,
        );
        for (range, label, on) in [(here, SCOPE_HERE, !self.all), (all, SCOPE_ALL, self.all)] {
            let (fg, bg, attributes) = match on {
                true => (colors.on_accent, Some(colors.accent), Attributes::BOLD),
                false => (colors.muted, None, Attributes::NONE),
            };
            frame.draw_text(label, range.start, 0, fg, bg, attributes);
        }

        // The query, and how many sessions it leaves.
        let in_scope = (0..self.listings.len())
            .filter(|&i| self.all || self.here[i])
            .count();
        let count = match self.query.is_empty() {
            true => plural(in_scope, "session"),
            false => format!("{} of {}", self.shown.len(), in_scope),
        };
        let count_x = width.saturating_sub(count.chars().count() as u32 + 2);
        frame.draw_text(&count, count_x, 2, colors.muted, None, Attributes::NONE);
        let prompt = " › ";
        frame.draw_text(prompt, 0, 2, colors.accent, None, Attributes::BOLD);
        let x = prompt.chars().count() as u32;
        let room = count_x.saturating_sub(x + 2) as usize;
        let (shown, cursor) = self.caret.view(&self.query, room);
        match self.query.is_empty() {
            true => {
                let placeholder = truncate("Search folders, tabs, files, and programs", room);
                frame.draw_text(&placeholder, x, 2, colors.faint, None, Attributes::NONE);
            }
            false => frame.draw_text(&shown, x, 2, colors.text, None, Attributes::NONE),
        }

        self.draw_list(frame);
        self.draw_hints(frame);
        Some((x + cursor as u32, 2))
    }

    fn draw_list(&self, frame: &Buffer) {
        let colors = theme::colors();
        let width = self.width;
        let bottom = self.height - 1;
        if self.shown.is_empty() {
            let text = if !self.query.is_empty() {
                format!("No sessions match “{}”.", self.query)
            } else if !self.all {
                format!("No sessions in {}. Press Tab to show all.", self.folder)
            } else {
                "There are no sessions.".to_string()
            };
            let text = truncate(&text, width.saturating_sub(4) as usize);
            frame.draw_text(&text, 3, LIST_TOP, colors.muted, None, Attributes::NONE);
            return;
        }
        let words: Vec<Vec<char>> = self
            .query
            .split_whitespace()
            .map(|word| word.chars().map(lower).collect())
            .collect();
        for (row, &index) in self.shown[self.scroll..]
            .iter()
            .enumerate()
            .take(self.rows_in_view())
        {
            let y = LIST_TOP + row as u32 * ITEM_ROWS;
            if y + 1 >= bottom {
                break;
            }
            let listing = &self.listings[index];
            let selected = self.scroll + row == self.selected;
            let bg = selected.then_some(colors.selected);
            if let Some(bg) = bg {
                frame.fill_rect(0, y, width, 2, bg);
                frame.draw_text("▌", 0, y, colors.accent, None, Attributes::NONE);
                frame.draw_text("▌", 0, y + 1, colors.accent, None, Attributes::NONE);
            }

            // The folders, and when it was saved and how it is now.
            let status = listing.status();
            let age = format!("{} · ", listing.age(self.now));
            let right_len = (age.chars().count() + status.len()) as u32;
            let right_x = width.saturating_sub(right_len + 2);
            frame.draw_text(&age, right_x, y, colors.muted, None, Attributes::NONE);
            let status_color = match listing.live {
                true => colors.accent,
                false => colors.muted,
            };
            let status_x = right_x + age.chars().count() as u32;
            frame.draw_text(status, status_x, y, status_color, None, Attributes::NONE);
            let room = right_x.saturating_sub(4) as usize;
            let title_attributes = match selected {
                true => Attributes::BOLD,
                false => Attributes::NONE,
            };
            let folders = truncate(&listing.folders(), room);
            draw_found(frame, &folders, 2, y, colors.text, title_attributes, &words);

            // What's open in it, and its id.
            let id = &listing.id;
            let id_x = width.saturating_sub(id.chars().count() as u32 + 2);
            frame.draw_text(id, id_x, y + 1, colors.faint, None, Attributes::NONE);
            let room = id_x.saturating_sub(4) as usize;
            let details = truncate(&details(listing), room);
            draw_found(
                frame,
                &details,
                2,
                y + 1,
                colors.muted,
                Attributes::NONE,
                &words,
            );
        }
    }

    fn draw_hints(&self, frame: &Buffer) {
        let colors = theme::colors();
        let y = self.height - 1;
        frame.fill_rect(0, y, self.width, 1, colors.surface);
        let key = |command| {
            self.keymap
                .shortcut_in(command, Context::Picker)
                .map(|key| format!("{key:#}"))
        };
        let scope = match self.all {
            true => "this folder",
            false => "all sessions",
        };
        let mut hints = Vec::new();
        if let (Some(up), Some(down)) = (key(Command::PickerUp), key(Command::PickerDown)) {
            hints.push(format!("{up}/{down} select"));
        }
        if let Some(accept) = key(Command::PickerAccept) {
            hints.push(format!("{accept} open"));
        }
        hints.push(format!("Tab {scope}"));
        if let Some(close) = key(Command::PickerClose) {
            hints.push(format!("{close} cancel"));
        }
        let hints = truncate(&format!(" {}", hints.join("  ")), self.width as usize);
        frame.draw_text(&hints, 0, y, colors.muted, None, Attributes::NONE);
    }
}

const SCOPE_HERE: &str = " This Folder ";
const SCOPE_ALL: &str = " All ";

/// What a session is found by: its folders, tabs, files, terminals, and id,
/// lowercase.
fn haystack(listing: &Listing) -> String {
    let state = &listing.state;
    let mut parts = vec![listing.folders(), listing.id.clone()];
    parts.extend(state.roots.iter().map(|root| root.display().to_string()));
    parts.extend(state.tabs.iter().filter_map(|tab| tab.name.clone()));
    parts.extend(
        state
            .documents
            .iter()
            .filter_map(|doc| Some(doc.path.as_ref()?.display().to_string())),
    );
    for terminal in &state.terminals {
        parts.extend(terminal.name.clone());
        parts.extend(terminal.program.clone());
    }
    parts.join("\n").to_lowercase()
}

/// A line of what's open in a session: its tabs, files, programs, and
/// unsaved changes, as `2 tabs: api, docs · main.rs, lib.rs +3 · nvim`.
fn details(listing: &Listing) -> String {
    let state = &listing.state;
    let mut parts = Vec::new();
    if state.tabs.len() > 1 {
        let names: Vec<&str> = state
            .tabs
            .iter()
            .filter_map(|tab| tab.name.as_deref())
            .collect();
        parts.push(match names.is_empty() {
            true => format!("{} tabs", state.tabs.len()),
            false => format!("{} tabs: {}", state.tabs.len(), names.join(", ")),
        });
    }
    let files: Vec<String> = state
        .documents
        .iter()
        .filter_map(|doc| doc.path.as_deref())
        .filter_map(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if !files.is_empty() {
        let mut named = files[..files.len().min(FILES_NAMED)].join(", ");
        if files.len() > FILES_NAMED {
            named += &format!(" +{}", files.len() - FILES_NAMED);
        }
        parts.push(named);
    }
    let programs = state.programs();
    if !programs.is_empty() {
        parts.push(programs.join(", "));
    } else if !state.terminals.is_empty() {
        parts.push(plural(state.terminals.len(), "terminal"));
    }
    match listing.unsaved() {
        0 => {}
        n => parts.push(format!("{n} unsaved")),
    }
    match parts.is_empty() {
        true => "No files open".to_string(),
        false => parts.join(" · "),
    }
}

fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// A character as compared when searching.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Draws `text` at (`x`, `y`), with what `words` found in it in the accent
/// color.
fn draw_found(
    frame: &Buffer,
    text: &str,
    x: u32,
    y: u32,
    fg: Rgba,
    attributes: Attributes,
    words: &[Vec<char>],
) {
    let colors = theme::colors();
    let chars: Vec<char> = text.chars().collect();
    let lowered: Vec<char> = chars.iter().copied().map(lower).collect();
    let mut found = vec![false; chars.len()];
    for word in words.iter().filter(|word| !word.is_empty()) {
        for start in 0..lowered.len().saturating_sub(word.len() - 1) {
            if lowered[start..].starts_with(word) {
                found[start..start + word.len()].fill(true);
            }
        }
    }
    let mut start = 0;
    while start < chars.len() {
        let on = found[start];
        let end = (start..chars.len())
            .find(|&i| found[i] != on)
            .unwrap_or(chars.len());
        let run: String = chars[start..end].iter().collect();
        let (fg, attributes) = match on {
            true => (colors.accent, attributes | Attributes::BOLD),
            false => (fg, attributes),
        };
        frame.draw_text(&run, x + start as u32, y, fg, None, attributes);
        start = end;
    }
}

// --- the terminal -------------------------------------------------------------------

/// Shows `listings` full screen, those with `folder` in their workspace
/// unless `all`, and returns the one chosen, if any.
pub fn choose(mut listings: Vec<Listing>, folder: &Path, all: bool) -> io::Result<Option<Listing>> {
    let (config, _) = config::load();
    config::set(config);
    let keymap = Keymap::new(&config::get().keys);
    let (mut width, mut height) = tty::size();
    let mut renderer = Renderer::new(width, height, Output::Stdout)
        .map_err(|err| io::Error::other(err.to_string()))?;
    renderer.setup_terminal(true);
    renderer.enable_mouse(false);
    let mut chooser = Chooser::new(
        std::mem::take(&mut listings),
        folder,
        all,
        keymap,
        width,
        height,
    );

    // Drawn once the terminal says what its colors are, for the theme.
    write_to_terminal(&theme::color_queries());
    let asked = Instant::now();
    let mut terminal_colors = TerminalColors::default();
    let mut themed = false;
    let mut parser = Parser::new();
    let mut last_input = Instant::now();
    let mut dirty = true;
    let chosen = loop {
        if !themed && asked.elapsed() >= crate::COLORS_TIMEOUT {
            themed = true;
            use_theme(&terminal_colors);
        }
        if themed && dirty {
            {
                let frame = renderer
                    .next_buffer()
                    .map_err(|err| io::Error::other(err.to_string()))?;
                match chooser.draw(&frame) {
                    Some((x, y)) => renderer.set_cursor_position(x as i32 + 1, y as i32 + 1, true),
                    None => renderer.set_cursor_position(1, 1, false),
                }
            }
            renderer.render(false);
            dirty = false;
        }
        let mut timeout = crate::IDLE_POLL;
        if parser.has_pending() {
            timeout = timeout.min(crate::ESC_TIMEOUT.saturating_sub(last_input.elapsed()));
        }
        if !themed {
            timeout = timeout.min(crate::COLORS_TIMEOUT.saturating_sub(asked.elapsed()));
        }
        let Ok(bytes) = tty::read_input(timeout, &[]) else {
            break None;
        };
        let events = if !bytes.is_empty() {
            last_input = Instant::now();
            parser.feed(&bytes)
        } else if parser.has_pending() && last_input.elapsed() >= crate::ESC_TIMEOUT {
            parser.flush()
        } else {
            Vec::new()
        };
        let mut action = Action::Continue;
        for event in events {
            if let Event::Reply(bytes) = &event {
                if bytes == theme::ASKED_LAST && !themed {
                    themed = true;
                    use_theme(&terminal_colors);
                    dirty = true;
                } else if !terminal_colors.take_reply(bytes) {
                    renderer.process_capability_response(bytes);
                }
                continue;
            }
            dirty = true;
            action = chooser.handle(event);
            if action != Action::Continue {
                break;
            }
        }
        match action {
            Action::Continue => {}
            Action::Open(index) => break Some(index),
            Action::Cancel => break None,
        }
        if tty::size() != (width, height) {
            (width, height) = tty::size();
            renderer.resize(width, height);
            chooser.set_size(width, height);
            dirty = true;
        }
    };
    drop(renderer);
    Ok(chosen.map(|index| chooser.listings.swap_remove(index)))
}

/// Puts the theme the settings pick for the terminal's colors in use.
fn use_theme(terminal: &TerminalColors) {
    let id = config::get().theme.pick(terminal);
    theme::set(id.colors(terminal));
}

fn write_to_terminal(text: &str) {
    use std::io::Write;
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use crate::session::{Doc, State, TabState, TerminalState};
    use opentui::{OwnedBuffer, WidthMethod};
    use std::path::PathBuf;

    fn listing(id: &str, roots: &[&str], files: &[&str], saved: u64, live: bool) -> Listing {
        Listing {
            id: id.to_string(),
            dir: PathBuf::from("/sessions").join(id),
            live,
            state: State {
                roots: roots.iter().map(PathBuf::from).collect(),
                saved,
                documents: files
                    .iter()
                    .map(|file| Doc {
                        path: Some(PathBuf::from(file)),
                        ..Doc::default()
                    })
                    .collect(),
                ..State::default()
            },
        }
    }

    fn chooser(all: bool) -> Chooser {
        let mut cue = listing(
            "a1",
            &["/work/cue"],
            &["/work/cue/src/main.rs", "/work/cue/src/app.rs"],
            300,
            true,
        );
        cue.state.tabs = vec![
            TabState {
                name: Some("api".into()),
                ..tab()
            },
            tab(),
        ];
        cue.state.terminals = vec![TerminalState {
            program: Some("nvim".into()),
            ..TerminalState::default()
        }];
        let listings = vec![
            cue,
            listing("b2", &["/work/cue"], &["/work/cue/README.md"], 200, false),
            listing("c3", &["/notes"], &["/notes/todo.md"], 100, false),
        ];
        let keymap = Keymap::new(&[]);
        let mut chooser = Chooser::new(listings, Path::new("/work/cue/src"), all, keymap, 80, 20);
        chooser.now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(400);
        chooser
    }

    fn tab() -> TabState {
        TabState {
            name: None,
            layout: crate::layout::Layout::Panel(0),
            active: 0,
            panels: Vec::new(),
        }
    }

    fn key(chooser: &mut Chooser, code: KeyCode) -> Action {
        chooser.handle(Event::Key(Key::new(code, Mods::NONE)))
    }

    fn type_text(chooser: &mut Chooser, text: &str) {
        for c in text.chars() {
            key(chooser, KeyCode::Char(c));
        }
    }

    fn ids(chooser: &Chooser) -> Vec<&str> {
        chooser
            .shown
            .iter()
            .map(|&i| chooser.listings[i].id.as_str())
            .collect()
    }

    fn screen(chooser: &Chooser) -> String {
        let frame = OwnedBuffer::new(80, 20, false, WidthMethod::Unicode, "test").unwrap();
        chooser.draw(&frame);
        frame.to_text(true)
    }

    #[test]
    fn lists_the_folders_sessions_and_tab_shows_all() {
        let _serial = crate::test_serial();
        let mut chooser = chooser(false);
        assert_eq!(ids(&chooser), ["a1", "b2"]);
        let text = screen(&chooser);
        assert!(
            text.contains("2 tabs: api · main.rs, app.rs · nvim"),
            "{text}"
        );
        assert!(text.contains("running"), "{text}");
        assert!(!text.contains("/notes"), "{text}");
        key(&mut chooser, KeyCode::Tab);
        assert_eq!(ids(&chooser), ["a1", "b2", "c3"]);
        assert!(screen(&chooser).contains("/notes"));
        key(&mut chooser, KeyCode::Down);
        key(&mut chooser, KeyCode::Down);
        assert_eq!(key(&mut chooser, KeyCode::Enter), Action::Open(2));
    }

    #[test]
    fn typing_narrows_by_every_word_and_esc_clears_then_cancels() {
        let _serial = crate::test_serial();
        let mut chooser = chooser(true);
        type_text(&mut chooser, "TODO");
        assert_eq!(ids(&chooser), ["c3"]);
        assert!(screen(&chooser).contains("1 of 3"));
        key(&mut chooser, KeyCode::Esc);
        assert_eq!(ids(&chooser), ["a1", "b2", "c3"]);
        type_text(&mut chooser, "cue nvim");
        assert_eq!(ids(&chooser), ["a1"]);
        type_text(&mut chooser, " zzz");
        assert!(ids(&chooser).is_empty());
        assert!(screen(&chooser).contains("No sessions match"));
        assert_eq!(key(&mut chooser, KeyCode::Enter), Action::Continue);
        key(&mut chooser, KeyCode::Esc);
        assert_eq!(key(&mut chooser, KeyCode::Esc), Action::Cancel);
    }

    #[test]
    fn the_selection_stays_on_its_session_as_the_list_changes() {
        let _serial = crate::test_serial();
        let mut chooser = chooser(true);
        key(&mut chooser, KeyCode::Down);
        type_text(&mut chooser, "readme");
        assert_eq!(ids(&chooser), ["b2"]);
        key(&mut chooser, KeyCode::Esc);
        assert_eq!(key(&mut chooser, KeyCode::Enter), Action::Open(1));
    }

    #[test]
    fn a_double_click_opens_and_the_header_switches_scope() {
        let _serial = crate::test_serial();
        let mut chooser = chooser(false);
        let click = |chooser: &mut Chooser, x, y| {
            chooser.handle(Event::Mouse(Mouse {
                kind: MouseKind::Press(MouseButton::Left),
                x,
                y,
                mods: Mods::NONE,
            }))
        };
        let (_, all) = chooser.scope_tabs();
        click(&mut chooser, all.start, 0);
        assert!(chooser.all);
        // The second session's second line, then the blank row after it.
        let y = LIST_TOP + ITEM_ROWS + 1;
        assert_eq!(click(&mut chooser, 10, y), Action::Continue);
        assert_eq!(chooser.selected, 1);
        assert_eq!(click(&mut chooser, 10, y + 1), Action::Continue);
        assert_eq!(click(&mut chooser, 10, y), Action::Open(1));
    }

    #[test]
    fn the_folder_with_no_sessions_says_to_show_all() {
        let _serial = crate::test_serial();
        let keymap = Keymap::new(&[]);
        let listings = vec![listing("c3", &["/notes"], &[], 100, false)];
        let chooser = Chooser::new(listings, Path::new("/elsewhere"), false, keymap, 80, 20);
        let text = screen(&chooser);
        assert!(
            text.contains("No sessions in /elsewhere. Press Tab"),
            "{text}"
        );
    }
}
