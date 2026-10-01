//! A terminal: a shell running in a pty, and the emulator that keeps its
//! screen, shown in a panel.
//!
//! Like open files, terminals belong to the app and outlive the panels that
//! show them, but a terminal is in at most one panel at a time: its pty has
//! one size. Keys cue doesn't keep for itself go to the shell (see
//! [`crate::keymap::Keymap::lookup_terminal`]), encoded as the program in
//! the foreground asked for.
//!
//! The mouse goes to the program if it asked for it (as vim and htop do),
//! unless Shift is held. Otherwise dragging selects text, and the wheel
//! scrolls back through the output, or in a full-screen program that
//! didn't ask for the mouse, sends arrow keys, as terminals do.
//!
//! When the shell exits, the terminal keeps its last screen until Enter
//! starts a new shell.
//!
//! The find bar (Cmd+F or Ctrl+Shift+F) finds in the output and the
//! history above it, as the editor's does in a file, without replacing.
//! As in other terminals, the next match is the one above: finding starts
//! from the bottom of the view and goes back through the history. Matches
//! are found again as output arrives.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use opentui::{
    Anchor, Buffer, EmbeddedTerminal, KeyEvent, KeyMods, LineAt, MouseAction,
    MouseButton as TermButton, MouseEvent,
};

use crate::config;
use crate::editor::current_match;
use crate::find::{self, Field, FindBar, Match};
use crate::input::{Key, KeyCode, Mods, Mouse, MouseButton, MouseKind};
use crate::keymap::Keymap;
use crate::layout::Rect;
use crate::line_edit::Edit;
use crate::location::{self, Target};
use crate::pty::Pty;
use crate::search::Toggle;
use crate::status::{Prompt, PromptKey, Status};
use crate::theme;

/// At most this much output is read per poll, so a flood of it can't keep
/// the screen from being drawn, or keys from being read.
const READ_BUDGET: usize = 256 * 1024;
/// Rows the wheel scrolls.
const WHEEL_ROWS: i32 = 3;

pub struct Terminal {
    /// Numbers terminals, from 1, in the order they were started.
    id: u32,
    /// The name it was given, if it was renamed.
    name: Option<String>,
    /// The prompt for a new name, while it's open.
    prompt: Option<Prompt>,
    vt: EmbeddedTerminal,
    pty: Pty,
    cwd: PathBuf,
    /// The screen's size and place; the pty's size too.
    area: Rect,
    /// The shell ended, and everything it started closed the pty.
    closed: bool,
    exit: Option<ExitStatus>,
    /// The program is getting the mouse, from a press to its release.
    forwarding_mouse: bool,
    /// Where a drag to select text started.
    selecting_from: Option<(u16, u16)>,
    /// The find bar, while it's open.
    find: Option<Find>,
    /// Counts changes to the output, so the find bar knows when to find
    /// its matches again.
    epoch: u64,
}

/// The find bar, and where its matches are.
struct Find {
    bar: FindBar,
    /// Where each match is, to tell it again once the output changes and
    /// it's found again.
    anchors: Vec<Anchor>,
    current: Option<usize>,
}

impl Find {
    /// The nearest match on or above `row`, or else the last.
    fn above(&self, row: u32) -> Option<usize> {
        let matches = &self.bar.matches;
        let i = matches.partition_point(|m| m.row <= row);
        (!matches.is_empty()).then(|| i.checked_sub(1).unwrap_or(matches.len() - 1))
    }

    /// The nearest match on or below `row`, or else the first.
    fn below(&self, row: u32) -> Option<usize> {
        let matches = &self.bar.matches;
        let i = matches.partition_point(|m| m.row < row);
        (!matches.is_empty()).then(|| i % matches.len())
    }
}

/// Gives `vt` the theme's colors: its palette, or the terminal's own.
fn restyle(vt: &mut EmbeddedTerminal) {
    let colors = theme::colors();
    match colors.terminal {
        Some(palette) => {
            vt.set_host_palette(palette.ansi.is_none());
            vt.set_default_colors(palette.fg, palette.bg, palette.ansi.as_ref());
        }
        None => vt.set_host_palette(true),
    }
    let current = current_match();
    vt.set_search_colors((None, colors.match_bg), (current.fg, current.bg));
}

impl Terminal {
    /// Puts the theme in use.
    pub fn restyle(&mut self) {
        restyle(&mut self.vt);
    }

    /// Starts a shell in `cwd`, on a screen the size of `area`.
    pub fn new(id: u32, cwd: &Path, area: Rect) -> io::Result<Terminal> {
        let (cols, rows) = size(area);
        let mut vt = EmbeddedTerminal::new(cols, rows, config::get().scrollback)
            .map_err(|e| io::Error::other(e.to_string()))?;
        restyle(&mut vt);
        let pty = Pty::shell(cwd, cols, rows)?;
        Ok(Terminal {
            id,
            name: None,
            prompt: None,
            vt,
            pty,
            cwd: cwd.to_path_buf(),
            area,
            closed: false,
            exit: None,
            forwarding_mouse: false,
            selecting_from: None,
            find: None,
            epoch: 0,
        })
    }

    /// The folder it started in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Starts a new shell, after the last one exited, on a clear screen
    /// with none of the modes the last one left on.
    pub fn restart(&mut self) -> io::Result<()> {
        let (cols, rows) = size(self.area);
        self.pty = Pty::shell(&self.cwd, cols, rows)?;
        self.closed = false;
        self.exit = None;
        let _ = self.vt.write(b"\x1bc");
        self.output_changed();
        Ok(())
    }

    /// What to poll for this terminal, if anything: its pty, and whether
    /// input is waiting to be taken.
    pub fn watch(&self) -> Option<(i32, bool)> {
        (!self.closed).then(|| (self.pty.fd(), self.pty.has_unsent()))
    }

    /// Reads the shell's output into the screen and answers its queries.
    /// Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        self.pty.flush();
        let mut buf = [0u8; 64 * 1024];
        let mut read = 0;
        let mut output = false;
        while !self.closed && read < READ_BUDGET {
            match self.pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    read += n;
                    // Anything it can't make sense of is dropped, as
                    // terminals do.
                    let _ = self.vt.write(&buf[..n]);
                    changed = true;
                    output = true;
                }
                Err(_) => self.closed = true,
            }
        }
        if output {
            self.output_changed();
        }
        let responses = self.vt.drain_responses();
        if !responses.is_empty() {
            self.pty.write(&responses);
        }
        if self.exit.is_none() {
            // A shell can exit leaving jobs that hold the pty open.
            if let Some(status) = self.pty.exit_status() {
                self.exit = Some(status);
                changed = true;
            }
        }
        changed
    }

    /// The text the program put on the clipboard (OSC 52) since the last
    /// call, if any. Programs do this over ssh, where they can't reach the
    /// system clipboard themselves.
    pub fn take_copied(&mut self) -> Option<String> {
        self.vt.take_clipboard()
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// What to call it: the name it was given, or "Terminal 2".
    pub fn name(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => format!("Terminal {}", self.id),
        }
    }

    /// Whether it was given a name.
    pub fn is_renamed(&self) -> bool {
        self.name.is_some()
    }

    /// Asks for a new name in the status bar, starting from the one it
    /// was given, if any.
    pub fn show_rename(&mut self) {
        let name = self.name.as_deref().unwrap_or_default();
        self.prompt = Some(Prompt::new("Rename terminal", name));
    }

    /// The rename prompt has the keyboard.
    pub fn prompt_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    /// A key for the rename prompt, while it's open. An empty name goes
    /// back to the numbered one.
    pub fn handle_prompt_key(&mut self, key: Key) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match prompt.handle_key(key) {
            PromptKey::Continue => {}
            PromptKey::Cancel => self.prompt = None,
            PromptKey::Submit(name) => {
                self.prompt = None;
                self.name = (!name.is_empty()).then_some(name);
            }
        }
    }

    /// How the shell ended, once it has.
    pub fn exit(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// Whether a program the shell started is running in it.
    pub fn is_busy(&self) -> bool {
        self.exit.is_none() && self.pty.is_busy()
    }

    /// The program the shell is running, or the shell, such as `vim`.
    pub fn program(&self) -> Option<String> {
        if self.exit.is_some() {
            return None;
        }
        self.pty.foreground_name()
    }

    /// The title the program set, if any.
    pub fn title(&self) -> String {
        self.vt.title()
    }

    pub fn set_area(&mut self, area: Rect) {
        let old = size(self.area);
        self.area = area;
        let (cols, rows) = size(area);
        if (cols, rows) != old && self.vt.resize(cols, rows).is_ok() {
            self.pty.resize(cols, rows);
            self.selecting_from = None;
            // Lines rewrap.
            self.output_changed();
        }
    }

    /// Sends `key` to the program, and brings the view back to the live
    /// screen.
    pub fn send_key(&mut self, key: Key) {
        let Some(event) = key_event(key) else {
            return;
        };
        let bytes = self.vt.encode_key(&event.as_event());
        self.send(&bytes);
    }

    /// Pastes `text` into the program, or into the rename prompt while
    /// it's open.
    pub fn paste(&mut self, text: &str) {
        if let Some(prompt) = &mut self.prompt {
            prompt.paste(text);
            return;
        }
        let bytes = self.vt.encode_paste(text);
        self.send(&bytes);
    }

    /// Tells the program it got or lost the keyboard, if it asked to know.
    pub fn focus(&mut self, focused: bool) {
        let bytes = self.vt.encode_focus(focused);
        if !bytes.is_empty() && !self.closed {
            self.pty.write(&bytes);
        }
    }

    /// Clears the screen and the history above it, as Cmd+K does in macOS
    /// terminals. A shell at its prompt redraws it, keeping what was typed.
    /// A full-screen program keeps its screen.
    pub fn clear(&mut self) {
        self.vt.clear_selection();
        self.vt.scroll_to_bottom();
        if self.vt.is_alternate_screen() {
            return;
        }
        // Home, erase the screen, then the history, so nothing erased is
        // kept in it.
        let _ = self.vt.write(b"\x1b[H\x1b[2J\x1b[3J");
        self.output_changed();
        if self.exit.is_none() && !self.pty.is_busy() {
            // Ctrl+L: the shell clears the screen and redraws its prompt.
            self.pty.write(b"\x0c");
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.closed {
            return;
        }
        self.vt.clear_selection();
        self.vt.scroll_to_bottom();
        self.pty.write(bytes);
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        Some(self.vt.selected_text()).filter(|text| !text.is_empty())
    }

    /// A mouse event at screen cell (`mouse.x`, `mouse.y`), in the
    /// terminal or dragged from it.
    pub fn handle_mouse(&mut self, mouse: Mouse) {
        if self.handle_find_mouse(mouse) {
            return;
        }
        let (cols, rows) = size(self.area);
        let x = mouse.x.saturating_sub(self.area.x).min(cols as u32 - 1);
        let y = mouse.y.saturating_sub(self.area.y).min(rows as u32 - 1);
        let mods = key_mods(mouse.mods);
        let event = |action, button, any_button_pressed| MouseEvent {
            action,
            button,
            mods,
            x,
            y,
            any_button_pressed,
        };
        // Shift keeps the mouse for selecting, as in other terminals.
        let offer = !mouse.mods.shift && !self.closed;
        match mouse.kind {
            MouseKind::Press(button) => {
                self.forwarding_mouse = false;
                if offer {
                    let event = event(MouseAction::Press, term_button(button), true);
                    let bytes = self.vt.encode_mouse(&event);
                    if !bytes.is_empty() {
                        self.forwarding_mouse = true;
                        self.pty.write(&bytes);
                        return;
                    }
                }
                self.vt.clear_selection();
                self.selecting_from = (button == MouseButton::Left).then_some((x as u16, y as u16));
            }
            MouseKind::Drag(button) if self.forwarding_mouse => {
                let event = event(MouseAction::Motion, term_button(button), true);
                let bytes = self.vt.encode_mouse(&event);
                self.pty.write(&bytes);
            }
            MouseKind::Drag(_) => {
                if let Some(from) = self.selecting_from {
                    let _ = self.vt.set_selection(from, (x as u16, y as u16));
                }
            }
            MouseKind::Release(button) if self.forwarding_mouse => {
                self.forwarding_mouse = false;
                let event = event(MouseAction::Release, term_button(button), false);
                let bytes = self.vt.encode_mouse(&event);
                self.pty.write(&bytes);
            }
            MouseKind::Release(_) => self.selecting_from = None,
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let up = mouse.kind == MouseKind::ScrollUp;
                if offer {
                    let button = if up {
                        TermButton::Four
                    } else {
                        TermButton::Five
                    };
                    let bytes =
                        self.vt
                            .encode_mouse(&event(MouseAction::Press, Some(button), false));
                    if !bytes.is_empty() {
                        self.pty.write(&bytes);
                        return;
                    }
                }
                if self.vt.is_alternate_screen() {
                    // Full-screen programs have no scrollback; less and man
                    // scroll with the arrow keys.
                    let code = if up { KeyCode::Up } else { KeyCode::Down };
                    for _ in 0..WHEEL_ROWS {
                        self.send_key(Key::new(code, Mods::NONE));
                    }
                } else {
                    self.vt.scroll(if up { -WHEEL_ROWS } else { WHEEL_ROWS });
                }
            }
            MouseKind::ScrollLeft | MouseKind::ScrollRight | MouseKind::Move => {}
        }
    }

    /// What's at screen cell (`x`, `y`), for a Ctrl+click to open: a link
    /// (OSC 8), a URL, or the path of a file that exists, with the position
    /// printed after it. Relative paths are looked for where the program
    /// and the shell are working, then where the terminal started, then in
    /// `roots`.
    pub fn target_at(&self, x: u32, y: u32, roots: &[PathBuf]) -> Option<Target> {
        if !self.area.contains(x, y) {
            return None;
        }
        let at = self
            .vt
            .line_at((x - self.area.x) as u16, (y - self.area.y) as u16)?;
        let mut folders = match self.exit {
            None => self.pty.working_folders(),
            Some(_) => Vec::new(),
        };
        for folder in std::iter::once(&self.cwd).chain(roots) {
            if !folders.contains(folder) {
                folders.push(folder.clone());
            }
        }
        let resolve = |path: &str| location::find_file(path, &folders);
        match at {
            LineAt::Link(uri) => location::target_of_link(&uri, resolve),
            LineAt::Text {
                text,
                offset: Some(offset),
            } => location::target_in_line(&text, offset, resolve),
            LineAt::Text { offset: None, .. } => None,
        }
    }

    /// Draws the screen in its area, and returns where the terminal cursor
    /// goes if `focused`.
    pub fn draw(&self, frame: &Buffer, focused: bool, keymap: &Keymap) -> Option<(u32, u32)> {
        let area = self.area;
        self.vt.draw(frame, area.x as i32, area.y as i32);
        if let Some(find) = &self.find {
            let (x, width) = self.find_area();
            let bar = find
                .bar
                .draw(frame, (x, area.y, width), find.current, keymap);
            if let Some(cursor) = bar {
                return focused.then_some(cursor);
            }
        }
        let cursor = self.vt.cursor();
        let (x, y) = cursor.position?;
        (focused && cursor.visible && self.exit.is_none())
            .then_some((area.x + x as u32, area.y + y as u32))
    }

    // --- finding ---------------------------------------------------------------

    /// Opens the find bar with `memory`'s query, or the selection if it's on
    /// one line, and focuses the query, finding the nearest match above the
    /// bottom of the view. With the query focused already, it closes the
    /// bar.
    pub fn show_find(&mut self, memory: &find::Memory) {
        if self.find_focused() {
            return self.close_find();
        }
        let selected = self.selected_text().filter(|text| !text.contains('\n'));
        let bottom = self.bottom_row();
        let find = self.find.get_or_insert_with(|| Find {
            bar: find_bar(memory.clone(), bottom),
            anchors: Vec::new(),
            current: None,
        });
        let bar = &mut find.bar;
        if let Some(text) = selected {
            if text != bar.memory.query.text {
                bar.memory.query.text = text;
                bar.carets[Field::Find as usize].move_to_end();
                bar.epoch = None;
            }
            bar.origin = bottom;
        }
        bar.focus = Some(Field::Find);
        // Typing replaces the query, as if it were selected.
        bar.replace_query = !bar.memory.query.text.is_empty();
        self.sync_find();
    }

    /// Goes to the next match up, or with `older` false, down, from the
    /// current one or the view. With the find bar closed, it opens with
    /// `memory`'s query, keeping the keyboard in the terminal, or if
    /// there's no query, to type one.
    pub fn find_step(&mut self, memory: &find::Memory, older: bool) {
        if self.find.is_none() {
            if memory.query.text.is_empty() {
                return self.show_find(memory);
            }
            let mut bar = find_bar(memory.clone(), self.bottom_row());
            bar.focus = None;
            self.find = Some(Find {
                bar,
                anchors: Vec::new(),
                current: None,
            });
        }
        self.sync_find();
        let top = self.vt.scroll_position().top;
        let bottom = self.bottom_row();
        let Some(find) = &self.find else {
            return;
        };
        let count = find.bar.matches.len();
        let index = match find.current {
            Some(i) if older => Some((i + count - 1) % count),
            Some(i) => Some((i + 1) % count),
            None if older => find.above(bottom),
            None => find.below(top),
        };
        if let Some(index) = index {
            self.select_match(index);
        }
    }

    /// The find bar's query, while it's open.
    pub fn find_memory(&self) -> Option<&find::Memory> {
        self.find.as_ref().map(|find| &find.bar.memory)
    }

    pub fn find_open(&self) -> bool {
        self.find.is_some()
    }

    /// Whether the find bar has the keyboard.
    pub fn find_focused(&self) -> bool {
        self.find
            .as_ref()
            .is_some_and(|find| find.bar.focus.is_some())
    }

    /// Gives the keyboard back to the program.
    pub fn blur_find(&mut self) {
        if let Some(find) = &mut self.find {
            find.bar.focus = None;
        }
    }

    /// Edits the query: typing, pasting, deleting, or moving the cursor.
    pub fn find_edit(&mut self, edit: Edit) {
        if let Some(find) = &mut self.find {
            find.bar.edit(edit);
        }
        self.sync_find();
    }

    pub fn find_toggle(&mut self, toggle: Toggle) {
        if let Some(find) = &mut self.find {
            find.bar.toggle(toggle);
        }
        self.sync_find();
    }

    /// Closes the find bar, leaving the view where it is.
    pub fn close_find(&mut self) {
        if self.find.take().is_some() {
            self.vt.clear_search();
        }
    }

    /// Finds the matches again after the output changed.
    fn output_changed(&mut self) {
        self.epoch += 1;
        self.sync_find();
    }

    /// Finds the matches again if the output or the query changed. New
    /// output keeps the current match; a new query goes to the nearest
    /// one above where finding started.
    fn sync_find(&mut self) {
        let epoch = self.epoch;
        let Some(find) = &mut self.find else {
            return;
        };
        let bar = &mut find.bar;
        if bar.epoch == Some(epoch) {
            return;
        }
        let jump = bar.epoch.is_none() && bar.focus == Some(Field::Find);
        bar.epoch = Some(epoch);
        let text = self.vt.search_text().unwrap_or_default();
        let (ranges, truncated) = match find::find(&text, &bar.memory.query) {
            Ok(found) => {
                bar.error = None;
                found
            }
            Err(error) => {
                bar.error = Some(error);
                (Vec::new(), false)
            }
        };
        bar.truncated = truncated;
        let found = self.vt.set_search_matches(&ranges).unwrap_or_default();
        bar.matches = ranges
            .into_iter()
            .zip(&found)
            .map(|(bytes, found)| Match {
                row: found.row,
                cols: found.col as u32..found.col as u32,
                offsets: bytes.start as u32..bytes.end as u32,
                bytes,
            })
            .collect();
        let current = find.current.map(|i| find.anchors[i]);
        find.anchors = found.iter().map(|found| found.anchor).collect();
        find.current = current.and_then(|anchor| find.anchors.iter().position(|&a| a == anchor));
        self.vt.set_search_current(find.current);
        if jump {
            if let Some(index) = find.above(find.bar.origin) {
                self.select_match(index);
            }
        }
    }

    /// Makes match `index` the current one, scrolling to it if it's out of
    /// view.
    fn select_match(&mut self, index: usize) {
        let Some(find) = &mut self.find else {
            return;
        };
        let Some(row) = find.bar.matches.get(index).map(|m| m.row) else {
            return;
        };
        find.current = Some(index);
        find.bar.origin = row;
        self.vt.set_search_current(Some(index));
        let rows = self.area.height;
        let top = self.vt.scroll_position().top;
        // The bar covers the top row.
        let first = if rows > 2 { top + 1 } else { top };
        if !(first..top + rows).contains(&row) {
            self.vt.scroll_to_row(row.saturating_sub(rows / 2));
        }
    }

    /// The bottom row of the view, counted from the top of the history.
    fn bottom_row(&self) -> u32 {
        let top = self.vt.scroll_position().top;
        top + self.area.height.saturating_sub(1)
    }

    /// Where the find bar floats: the top right, a column in from the
    /// edge, as (screen column, width).
    fn find_area(&self) -> (u32, u32) {
        let area = self.area;
        let width = find::MAX_WIDTH.min(area.width.saturating_sub(1)).max(1);
        (area.x + area.width.saturating_sub(width + 1), width)
    }

    /// A mouse event on the find bar, which it handles; returns false if
    /// it's elsewhere. A press elsewhere gives the program the keyboard.
    fn handle_find_mouse(&mut self, mouse: Mouse) -> bool {
        let (x, width) = self.find_area();
        let area = self.area;
        let dragging = self.selecting_from.is_some() || self.forwarding_mouse;
        let Some(find) = &mut self.find else {
            return false;
        };
        let inside = (x..x + width).contains(&mouse.x)
            && (area.y..area.y + find.bar.rows()).contains(&mouse.y);
        let press = matches!(mouse.kind, MouseKind::Press(_));
        if !inside || (dragging && !press) {
            if press {
                find.bar.focus = None;
            }
            return false;
        }
        match mouse.kind {
            // The wheel scrolls the output under the bar.
            MouseKind::ScrollUp
            | MouseKind::ScrollDown
            | MouseKind::ScrollLeft
            | MouseKind::ScrollRight => return false,
            MouseKind::Press(MouseButton::Left) => {}
            _ => return true,
        }
        match find.bar.target(mouse.x, mouse.y - area.y) {
            find::Target::Field(_) => find.bar.focus = Some(Field::Find),
            find::Target::Toggle(toggle) => self.find_toggle(toggle),
            find::Target::Close => self.close_find(),
            find::Target::Expander | find::Target::Replace | find::Target::ReplaceAll => {}
        }
        true
    }

    /// What the status bar shows while this terminal is in the active panel.
    pub fn status(&self) -> Status {
        if let Some(prompt) = &self.prompt {
            return prompt.status();
        }
        // While typing a query that isn't a valid regex, why.
        if let Some(error) = self
            .find
            .as_ref()
            .filter(|find| find.bar.focus.is_some())
            .and_then(|find| find.bar.error())
        {
            return Status::Message {
                text: format!("Invalid regex: {error}"),
                error: true,
            };
        }
        match self.exit {
            Some(status) => Status::Message {
                text: format!("{}. Enter starts a new shell.", describe_exit(status)),
                error: false,
            },
            None => match self.program() {
                Some(program) => Status::Terminal(format!("{}  {program}", self.name())),
                None => Status::Terminal(self.name()),
            },
        }
    }
}

/// A find bar for a terminal: it finds, without replacing.
fn find_bar(memory: find::Memory, origin: u32) -> FindBar {
    let mut bar = FindBar::new(memory, origin);
    bar.replaceable = false;
    bar
}

/// "The shell exited", with its code or signal if it failed.
pub fn describe_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => "The shell exited".to_string(),
        (Some(code), _) => format!("The shell exited with code {code}"),
        (None, Some(signal)) => format!("The shell was ended by signal {signal}"),
        (None, None) => "The shell exited".to_string(),
    }
}

fn size(area: Rect) -> (u16, u16) {
    let clamp = |n: u32| n.clamp(1, u16::MAX as u32) as u16;
    (clamp(area.width), clamp(area.height))
}

fn key_mods(mods: Mods) -> KeyMods {
    let mut out = KeyMods::NONE;
    for (held, bit) in [
        (mods.shift, KeyMods::SHIFT),
        (mods.ctrl, KeyMods::CTRL),
        (mods.alt, KeyMods::ALT),
        (mods.sup, KeyMods::SUPER),
    ] {
        if held {
            out |= bit;
        }
    }
    out
}

/// The button as programs are told of it; cue keeps the back and forward
/// buttons.
fn term_button(button: MouseButton) -> Option<TermButton> {
    match button {
        MouseButton::Left => Some(TermButton::Left),
        MouseButton::Middle => Some(TermButton::Middle),
        MouseButton::Right => Some(TermButton::Right),
        MouseButton::Back | MouseButton::Forward => None,
    }
}

/// A key as the emulator takes it: the physical key, its modifiers, and
/// the text it types.
#[derive(Debug, PartialEq, Eq)]
struct KeyInput {
    code: String,
    mods: KeyMods,
    text: String,
    unshifted: Option<char>,
}

impl KeyInput {
    fn as_event(&self) -> KeyEvent<'_> {
        KeyEvent::press(&self.code, self.mods, &self.text, self.unshifted)
    }
}

fn key_event(key: Key) -> Option<KeyInput> {
    let mods = key_mods(key.mods);
    let named = |code: &str| KeyInput {
        code: code.to_string(),
        mods,
        text: String::new(),
        unshifted: None,
    };
    Some(match key.code {
        KeyCode::Char(c) => {
            let base = c.to_ascii_lowercase();
            // With Shift reported separately (Ctrl+Shift+C), the text is
            // the shifted letter, as the keyboard would type it.
            let typed = if key.mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            };
            KeyInput {
                code: char_code(base).unwrap_or_default(),
                mods,
                text: typed.to_string(),
                unshifted: char_code(base).is_some().then_some(base),
            }
        }
        KeyCode::Enter => named("Enter"),
        KeyCode::Tab => named("Tab"),
        KeyCode::Backspace => named("Backspace"),
        KeyCode::Delete => named("Delete"),
        KeyCode::Insert => named("Insert"),
        KeyCode::Esc => named("Escape"),
        KeyCode::Left => named("ArrowLeft"),
        KeyCode::Right => named("ArrowRight"),
        KeyCode::Up => named("ArrowUp"),
        KeyCode::Down => named("ArrowDown"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::F(n) => named(&format!("F{n}")),
    })
}

/// The W3C code of the US-layout key that types `c` unshifted.
fn char_code(c: char) -> Option<String> {
    let code = match c {
        'a'..='z' => return Some(format!("Key{}", c.to_ascii_uppercase())),
        '0'..='9' => return Some(format!("Digit{c}")),
        ' ' => "Space",
        '-' => "Minus",
        '=' => "Equal",
        '[' => "BracketLeft",
        ']' => "BracketRight",
        '\\' => "Backslash",
        ';' => "Semicolon",
        '\'' => "Quote",
        ',' => "Comma",
        '.' => "Period",
        '/' => "Slash",
        '`' => "Backquote",
        _ => return None,
    };
    Some(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use std::time::{Duration, Instant};

    fn encode(term: &Terminal, key: Key) -> Vec<u8> {
        term.vt.encode_key(&key_event(key).unwrap().as_event())
    }

    fn key(code: KeyCode, mods: Mods) -> Key {
        Key::new(code, mods)
    }

    #[test]
    fn keys_encode_as_a_terminal_sends_them() {
        let _serial = crate::test_serial();
        let root = std::env::temp_dir();
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 5,
        };
        let term = Terminal::new(1, &root, area).unwrap();
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        for (key, bytes) in [
            (key(KeyCode::Char('a'), Mods::NONE), &b"a"[..]),
            (key(KeyCode::Char('A'), Mods::NONE), b"A"),
            (key(KeyCode::Char('é'), Mods::NONE), "é".as_bytes()),
            (key(KeyCode::Char('c'), Mods::CTRL), b"\x03"),
            (key(KeyCode::Char('\\'), Mods::CTRL), b"\x1c"),
            (key(KeyCode::Char(']'), Mods::CTRL), b"\x1d"),
            (key(KeyCode::Char(' '), Mods::CTRL), b"\x00"),
            (key(KeyCode::Char('b'), alt), b"\x1bb"),
            (key(KeyCode::Char('x'), ctrl_shift), b"\x1b[120;6u"),
            (key(KeyCode::Enter, Mods::NONE), b"\r"),
            (key(KeyCode::Backspace, Mods::NONE), b"\x7f"),
            (key(KeyCode::Backspace, alt), b"\x1b\x7f"),
            (key(KeyCode::Tab, Mods::SHIFT), b"\x1b[Z"),
            (key(KeyCode::Esc, Mods::NONE), b"\x1b"),
            (key(KeyCode::Up, Mods::NONE), b"\x1b[A"),
            (key(KeyCode::Left, alt), b"\x1b[1;3D"),
            (key(KeyCode::PageDown, Mods::NONE), b"\x1b[6~"),
            (key(KeyCode::F(1), Mods::NONE), b"\x1bOP"),
            (key(KeyCode::F(5), Mods::NONE), b"\x1b[15~"),
        ] {
            assert_eq!(encode(&term, key), bytes, "{key:?}");
        }
    }

    /// What's on screen, as text.
    fn screen(term: &Terminal) -> String {
        let frame = opentui::OwnedBuffer::new(
            term.area.width,
            term.area.height,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        term.draw(&frame, true, &Keymap::default());
        frame.to_text(true)
    }

    /// Polls until a line on screen is `line`, or panics after a few
    /// seconds.
    fn wait_for(term: &mut Terminal, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            term.poll();
            let screen = screen(term);
            if screen.lines().any(|shown| shown.trim_end() == line) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no {line:?} on screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn runs_a_shell_that_takes_input_and_exits() {
        let _serial = crate::test_serial();
        // Short enough not to wrap.
        let root = Path::new("/tmp").canonicalize().unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 6,
        };
        let mut term = Terminal::new(1, &root, area).unwrap();
        // Whatever the login shell prints, it's at a prompt eventually.
        for c in "echo $((6*7)) $TERM $(stty size); pwd".chars() {
            term.send_key(key(KeyCode::Char(c), Mods::NONE));
        }
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "42 xterm-256color 6 40");
        wait_for(&mut term, &root.display().to_string());

        // The pty follows the panel's size.
        term.set_area(Rect {
            width: 30,
            height: 8,
            ..area
        });
        // A shell with bracketed paste on waits for Enter.
        term.paste("stty size");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "8 30");
        assert!(term.exit().is_none());

        term.paste("exit 3");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        let deadline = Instant::now() + Duration::from_secs(10);
        while term.exit().is_none() {
            assert!(Instant::now() < deadline, "the shell didn't exit");
            term.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(term.exit().unwrap().code(), Some(3));
        assert_eq!(
            term.status(),
            Status::Message {
                text: "The shell exited with code 3. Enter starts a new shell.".into(),
                error: false
            }
        );

        term.restart().unwrap();
        term.paste("echo again");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "again");
    }

    #[test]
    fn clearing_drops_the_screen_and_history_but_keeps_the_prompt() {
        let _serial = crate::test_serial();
        let root = Path::new("/tmp").canonicalize().unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 6,
        };
        let mut term = Terminal::new(1, &root, area).unwrap();
        // More than a screenful, so some goes into the history.
        term.paste("seq 1 20; echo do''ne");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "done");
        // Typed at the prompt, and still there after.
        term.paste("echo kept");

        term.clear();
        wait_for_line_ending(&mut term, "echo kept");
        let shown = screen(&term);
        assert!(!shown.contains("done"), "{shown}");
        term.vt.scroll(-100);
        let history = screen(&term);
        assert!(!history.contains("20"), "{history}");
        let first = history.lines().next().unwrap_or_default();
        assert!(first.trim_end().ends_with("echo kept"), "{history}");
    }

    #[test]
    fn finds_in_the_output_and_its_history() {
        let _serial = crate::test_serial();
        let root = Path::new("/tmp").canonicalize().unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 70,
            height: 6,
        };
        let mut term = Terminal::new(1, &root, area).unwrap();
        term.paste("for i in $(seq 1 30); do echo line$i; done; echo do''ne");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "done");
        let bottom = term.vt.scroll_position();

        // line2, and line20 to line29. The nearest above the bottom of the
        // view is current, and in view already.
        term.show_find(&find::Memory::default());
        term.find_edit(Edit::Insert("line2"));
        assert!(term.find_focused());
        let find = term.find.as_ref().unwrap();
        assert_eq!(find.bar.matches.len(), 11);
        assert_eq!(find.current, Some(10));
        assert_eq!(term.vt.scroll_position(), bottom);
        let shown = screen(&term);
        assert!(
            shown.lines().next().unwrap().contains("11 of 11"),
            "{shown}"
        );

        // The next match is the one above; far enough up, the view follows.
        let memory = term.find_memory().unwrap().clone();
        term.find_step(&memory, true);
        assert_eq!(term.find.as_ref().unwrap().current, Some(9));
        for _ in 0..9 {
            term.find_step(&memory, true);
        }
        let find = term.find.as_ref().unwrap();
        assert_eq!(find.current, Some(0));
        let row = find.bar.matches[0].row;
        let top = term.vt.scroll_position().top;
        assert!((top + 1..top + 6).contains(&row), "line2 in view: {top}");
        let shown = screen(&term);
        assert!(shown.lines().any(|l| l.trim_end() == "line2"), "{shown}");
        // Past the first, around to the last.
        term.find_step(&memory, true);
        assert_eq!(term.find.as_ref().unwrap().current, Some(10));
        term.find_step(&memory, false);
        assert_eq!(term.find.as_ref().unwrap().current, Some(0));

        // New output is found too, keeping the current match.
        term.blur_find();
        term.paste("echo line2x");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "line2x");
        // The command and its output, at least: a shell may suggest it
        // again from its history.
        let find = term.find.as_ref().unwrap();
        assert!(find.bar.matches.len() >= 13, "{}", find.bar.matches.len());
        assert_eq!(find.current, Some(0));
        assert_eq!(find.bar.matches[0].row, row);

        // A query that isn't a valid regex says why.
        term.show_find(&memory);
        term.find_toggle(Toggle::Regex);
        term.find_edit(Edit::Insert("("));
        assert!(matches!(term.status(), Status::Message { error: true, .. }));
        assert!(term.find.as_ref().unwrap().bar.matches.is_empty());

        // Showing it again with the query focused closes it.
        term.show_find(&memory);
        assert!(!term.find_open());
    }

    /// Polls until the first line on screen ends with `end`.
    fn wait_for_line_ending(term: &mut Terminal, end: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            term.poll();
            let screen = screen(term);
            if screen
                .lines()
                .next()
                .is_some_and(|first| first.trim_end().ends_with(end))
            {
                return;
            }
            assert!(Instant::now() < deadline, "no {end:?} first:\n{screen}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
