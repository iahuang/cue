//! One open file: editing commands on top of OpenTUI's `EditBuffer` and
//! `EditorView`, which own the text, cursor, selection, and scrolling.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use opentui::{
    Attributes, Buffer, EditBuffer, EditorView, Rgba, SelectionBehavior, SelectionColors,
    WidthMethod, WrapMode,
};

use crate::document::{self, LineEnding};
use crate::history::{EditKind, History};
use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Keymap};
use crate::words;
use crate::workspace::Workspace;

const STATUS_BG: Rgba = Rgba::rgb(49, 50, 68);
const STATUS_FG: Rgba = Rgba::rgb(205, 214, 244);
const STATUS_DIM: Rgba = Rgba::rgb(147, 153, 178);
const STATUS_ERROR_BG: Rgba = Rgba::rgb(180, 60, 80);
const LINE_NUMBER: Rgba = Rgba::rgb(108, 112, 134);
const LINE_NUMBER_CURRENT: Rgba = Rgba::rgb(205, 214, 244);
const SELECTION: SelectionColors = SelectionColors {
    bg: Rgba::rgb(69, 71, 110),
    fg: None,
};

const WHEEL_LINES: u32 = 3;
/// Line numbers are hidden when they would leave the text less room than this.
const MIN_TEXT_WIDTH: u32 = 20;

pub enum Action {
    Continue,
    /// Put this text on the system clipboard.
    Copy(String),
    /// The file was written, possibly to a new path.
    Saved,
    /// The "Save as" prompt was answered with this path, as typed. The app
    /// resolves it and calls [`Editor::save_as`].
    SaveAs(PathBuf),
}

/// The file being edited.
pub struct File {
    /// `None` until the buffer is first saved.
    pub path: Option<PathBuf>,
    pub line_ending: LineEnding,
}

struct Message {
    text: String,
    error: bool,
}

/// The "Save as" line editor shown in the status bar.
struct Prompt {
    input: String,
}

/// Which way a cursor movement goes through the text.
#[derive(Clone, Copy)]
enum Direction {
    Backward,
    Forward,
}

/// A mouse selection in progress.
struct Drag {
    origin: (u32, u32),
    focus: (u32, u32),
    behavior: SelectionBehavior,
}

struct Click {
    at: (u32, u32),
    time: Instant,
    count: u32,
}

pub struct Editor {
    buffer: Rc<EditBuffer>,
    view: EditorView<'static>,
    file: File,
    history: History,
    /// The fixed end of the selection that keyboard movement extends from.
    anchor: Option<u32>,
    drag: Option<Drag>,
    last_click: Option<Click>,
    wrap: WrapMode,
    /// The screen column of the editor's left edge.
    x: u32,
    width: u32,
    height: u32,
    message: Option<Message>,
    prompt: Option<Prompt>,
}

impl Editor {
    /// Opens `path`, or a new, unnamed buffer. The error is the reason
    /// alone, without the path.
    pub fn open(path: Option<PathBuf>, width: u32, height: u32) -> Result<Editor, String> {
        let loaded = match &path {
            Some(path) => Some(document::load(path).map_err(|e| e.to_string())?),
            None => None,
        };
        let buffer = Rc::new(EditBuffer::new(WidthMethod::Unicode).map_err(|e| e.to_string())?);
        buffer.set_tab_width(4);
        let mut file = File {
            path,
            line_ending: Default::default(),
        };
        let mut notice = None;
        if let Some(loaded) = loaded {
            buffer.set_text(&loaded.text);
            buffer.set_cursor(0, 0);
            file.line_ending = loaded.line_ending;
            if loaded.mixed_endings {
                notice = Some(format!(
                    "Mixed line endings; saving will use {}.",
                    loaded.line_ending.label()
                ));
            }
        }
        let mut editor = Editor::new(buffer, file, width, height).map_err(|e| e.to_string())?;
        if let Some(notice) = notice {
            editor.show_message(notice, false);
        }
        Ok(editor)
    }

    /// An editor `width` x `height`, with line numbers down the left and the
    /// last row used for the status bar.
    pub fn new(
        buffer: Rc<EditBuffer>,
        file: File,
        width: u32,
        height: u32,
    ) -> opentui::Result<Editor> {
        let (_, view_w, view_h) = text_area(width, height, buffer.line_count());
        let view = buffer.shared_view(view_w, view_h)?;
        let wrap = WrapMode::None;
        view.set_wrap_mode(wrap);
        Ok(Editor {
            buffer,
            view,
            file,
            history: History::new(),
            anchor: None,
            drag: None,
            last_click: None,
            wrap,
            x: 0,
            width,
            height,
            message: None,
            prompt: None,
        })
    }

    /// Places the editor at column `x`, `width` x `height`.
    pub fn set_area(&mut self, x: u32, width: u32, height: u32) {
        self.x = x;
        self.width = width;
        self.height = height;
        self.sync_view_size();
    }

    /// Fits the view to the text area, which narrows or widens as the line
    /// count gains or loses digits.
    fn sync_view_size(&self) {
        let (_, width, height) = self.text_area();
        let vp = self.view.viewport();
        if (vp.width, vp.height) != (width, height) {
            self.view.set_viewport_size(width, height);
        }
    }

    /// The text area as (x, width, height).
    fn text_area(&self) -> (u32, u32, u32) {
        text_area(self.width, self.height, self.buffer.line_count())
    }

    /// The file's path; `None` until it is first saved.
    pub fn path(&self) -> Option<&Path> {
        self.file.path.as_deref()
    }

    pub fn is_modified(&self) -> bool {
        self.history.is_modified()
    }

    /// An unnamed buffer that was never typed in, which opening a file may
    /// replace.
    pub fn is_blank(&self) -> bool {
        self.file.path.is_none() && !self.buffer.can_undo() && self.buffer.text().is_empty()
    }

    /// Shows `text` in the status bar until the next key press.
    pub fn show_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: text.into(),
            error,
        });
    }

    pub fn clear_message(&mut self) {
        self.message = None;
    }

    /// The "Save as" prompt has the keyboard.
    pub fn prompt_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    /// Types a key bound to no command. Keys with Ctrl/Alt/Cmd held never
    /// type.
    pub fn type_key(&mut self, key: Key) {
        if let KeyCode::Char(c) = key.code {
            if key.mods.is_plain() {
                let mut utf8 = [0u8; 4];
                let text: &str = c.encode_utf8(&mut utf8);
                self.edit(EditKind::Type(c), |eb| eb.insert_text(text));
            }
        }
    }

    /// Runs an editing command; others are ignored. With `select`, a cursor
    /// movement extends the selection. `clipboard` is shared by all editors.
    pub fn run(
        &mut self,
        command: Command,
        select: bool,
        clipboard: &mut Option<String>,
    ) -> Action {
        use Direction::{Backward, Forward};
        match command {
            Command::Save => return self.save(),
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::Copy => return self.copy(clipboard),
            Command::Cut => return self.cut(clipboard),
            Command::Paste => self.paste_clipboard(clipboard.as_deref()),
            Command::SelectAll => self.select_all(),
            Command::ClearSelection => {
                self.anchor = None;
                self.view.clear_selection();
            }
            Command::ToggleWrap => self.toggle_wrap(),
            Command::NewLine => self.edit(EditKind::Other, EditBuffer::new_line),
            Command::InsertTab => self.edit(EditKind::Type('\t'), |eb| eb.insert_text("\t")),
            Command::DeleteBackward => self.delete(EditBuffer::delete_char_backward),
            Command::DeleteForward => self.delete(EditBuffer::delete_char),
            Command::DeleteWordBackward => self.delete_word(Backward),
            Command::DeleteWordForward => self.delete_word(Forward),
            Command::MoveLinesUp => self.move_lines(Backward),
            Command::MoveLinesDown => self.move_lines(Forward),
            Command::CursorLeft if !select && self.collapse_selection(true) => {}
            Command::CursorRight if !select && self.collapse_selection(false) => {}
            Command::CursorLeft => {
                self.move_cursor(select, Backward, |ed| ed.buffer.move_cursor_left())
            }
            Command::CursorRight => {
                self.move_cursor(select, Forward, |ed| ed.buffer.move_cursor_right())
            }
            Command::CursorUp => self.move_cursor(select, Backward, |ed| ed.view.move_up_visual()),
            Command::CursorDown => {
                self.move_cursor(select, Forward, |ed| ed.view.move_down_visual())
            }
            Command::WordLeft => self.move_cursor(select, Backward, |ed| {
                words::word_left(&ed.buffer);
            }),
            Command::WordRight => self.move_cursor(select, Forward, |ed| {
                words::word_right(&ed.buffer);
            }),
            Command::LineStart => {
                self.move_cursor(select, Backward, |ed| ed.view.move_to_visual_line_start())
            }
            Command::LineEnd => {
                self.move_cursor(select, Forward, |ed| ed.view.move_to_visual_line_end())
            }
            Command::DocumentStart => {
                self.move_cursor(select, Backward, Self::move_to_document_start)
            }
            Command::DocumentEnd => self.move_cursor(select, Forward, Self::move_to_document_end),
            Command::CursorPageUp => self.move_cursor(select, Backward, |ed| {
                (0..ed.page()).for_each(|_| ed.view.move_up_visual())
            }),
            Command::CursorPageDown => self.move_cursor(select, Forward, |ed| {
                (0..ed.page()).for_each(|_| ed.view.move_down_visual())
            }),
            _ => {}
        }
        Action::Continue
    }

    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) {
        if self.prompt.is_some() {
            return;
        }
        let (text_x, text_w, text_h) = self.text_area();
        // Clicks on the line numbers go to the start of the line.
        let at = (
            mouse.x.saturating_sub(text_x).min(text_w - 1),
            mouse.y.min(text_h - 1),
        );
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) if mouse.y < text_h => {
                self.message = None;
                self.history.break_group();
                let count = self.click_count(at, now);
                if mouse.mods.shift && count == 1 {
                    self.extend_selection_to(at);
                    return;
                }
                let behavior = match count {
                    2 => SelectionBehavior::Word,
                    3 => SelectionBehavior::Line,
                    _ => SelectionBehavior::Cell,
                };
                self.anchor = None;
                self.view.clear_selection();
                self.view
                    .set_local_selection(cell(at), cell(at), behavior, true, SELECTION);
                self.drag = Some(Drag {
                    origin: at,
                    focus: at,
                    behavior,
                });
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let Some(drag) = &mut self.drag {
                    drag.focus = at;
                    self.view.update_local_selection(
                        cell(drag.origin),
                        cell(at),
                        drag.behavior,
                        true,
                        SELECTION,
                    );
                }
            }
            MouseKind::Release(MouseButton::Left) => {
                if let Some(drag) = self.drag.take() {
                    self.finish_drag(drag);
                }
            }
            MouseKind::ScrollUp => self.scroll(0, -(WHEEL_LINES as i64)),
            MouseKind::ScrollDown => self.scroll(0, WHEEL_LINES as i64),
            MouseKind::ScrollLeft => self.scroll(-(WHEEL_LINES as i64), 0),
            MouseKind::ScrollRight => self.scroll(WHEEL_LINES as i64, 0),
            _ => {}
        }
    }

    pub fn paste(&mut self, text: &str) {
        // Terminals send newlines in pastes as CR.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        match &mut self.prompt {
            // Only the first line makes sense in a file name.
            Some(prompt) => prompt.input.push_str(text.lines().next().unwrap_or("")),
            None => self.edit(EditKind::Other, |eb| eb.insert_text(&text)),
        }
    }

    /// Draws the editor at its column and returns the terminal cursor
    /// position (0-based column, row). The keymap labels the status bar's
    /// shortcut hints, and the workspace shortens the file's path.
    pub fn draw(&self, frame: &Buffer, keymap: &Keymap, workspace: &Workspace) -> (u32, u32) {
        self.sync_view_size();
        let (gutter, _, _) = self.text_area();
        let text_x = self.x + gutter;
        frame.draw_editor_view(&self.view, text_x as i32, 0);
        self.draw_line_numbers(frame, gutter);
        match self.draw_status(frame, keymap, workspace) {
            Some(prompt_cursor) => prompt_cursor,
            None => {
                let cursor = self.view.visual_cursor();
                (text_x + cursor.col, cursor.row)
            }
        }
    }

    /// Numbers the first row of each visible line in the `gutter` columns
    /// left of the text, highlighting the cursor's line.
    fn draw_line_numbers(&self, frame: &Buffer, gutter: u32) {
        if gutter == 0 {
            return;
        }
        let current = self.buffer.cursor().row;
        let digits = gutter as usize - 3;
        for (y, row) in self.view.visible_lines().iter().enumerate() {
            if row.wrap != 0 {
                continue;
            }
            let (fg, attributes) = if row.line == current {
                (LINE_NUMBER_CURRENT, Attributes::BOLD)
            } else {
                (LINE_NUMBER, Attributes::NONE)
            };
            let number = format!("{:>digits$}", row.line + 1);
            frame.draw_text(&number, self.x + 1, y as u32, fg, None, attributes);
        }
    }

    // --- editing -----------------------------------------------------------

    /// Applies an edit that replaces the selection, if any, and records it
    /// for undo.
    fn edit(&mut self, kind: EditKind, f: impl FnOnce(&EditBuffer) -> u32) {
        let replaced = self.delete_selection();
        if replaced > 0 {
            // Typing over a selection starts a new undo step that includes
            // the deletion.
            self.history.break_group();
        }
        let steps = replaced + f(&self.buffer);
        self.history.record(kind, steps);
    }

    /// Backspace/Delete: removes the selection if there is one, otherwise a
    /// character.
    fn delete(&mut self, delete_char: impl FnOnce(&EditBuffer) -> u32) {
        let removed = self.delete_selection();
        if removed > 0 {
            self.history.break_group();
            self.history.record(EditKind::Other, removed);
        } else {
            let steps = delete_char(&self.buffer);
            self.history.record(EditKind::Delete, steps);
        }
    }

    /// Alt+Backspace/Delete: deletes to the previous word start or next word
    /// end, or just the selection if there is one.
    fn delete_word(&mut self, direction: Direction) {
        if self.view.selection().is_some_and(|(s, e)| s != e) {
            self.delete(|_| 0);
            return;
        }
        let eb = &*self.buffer;
        let from = eb.cursor();
        let target = match direction {
            Direction::Backward => words::word_left(eb),
            Direction::Forward => words::word_right(eb),
        };
        let Some(to) = eb
            .offset_to_position(target)
            .filter(|_| target != from.offset)
        else {
            return;
        };
        let (start, end) = match direction {
            Direction::Backward => ((to.row, to.col), (from.row, from.col)),
            Direction::Forward => ((from.row, from.col), (to.row, to.col)),
        };
        self.history.break_group();
        let steps = eb.delete_range(start, end);
        self.history.record(EditKind::Other, steps);
    }

    /// Alt+Up/Down: swaps the lines holding the cursor or selection with the
    /// line above or below, keeping the cursor and selection on the moved text.
    fn move_lines(&mut self, direction: Direction) {
        let eb = &*self.buffer;
        let cursor = eb.cursor();
        let selection = self.view.selection().filter(|(s, e)| s != e);
        let pos = |offset: u32| eb.offset_to_position(offset).map(|p| (p.row, p.col));
        let (sel_start, sel_end) = match selection {
            Some((s, e)) => match (pos(s), pos(e)) {
                (Some(a), Some(b)) => (Some(a), Some(b)),
                _ => return,
            },
            None => (None, None),
        };
        let first = sel_start.map_or(cursor.row, |p| p.0);
        // A selection ending at the start of a line doesn't include that line.
        let last = match sel_end {
            Some((row, 0)) if row > first => row - 1,
            Some((row, _)) => row,
            None => cursor.row,
        };
        let up = matches!(direction, Direction::Backward);
        if (up && first == 0) || (!up && last + 1 >= eb.line_count()) {
            return;
        }
        let anchor_is_start = self
            .anchor
            .is_some_and(|a| Some(a) == selection.map(|(s, _)| s));

        self.history.break_group();
        let steps = if up {
            // Remove the line above, then reinsert it after the block.
            let prev_start = eb.position_to_offset(first - 1, 0);
            let block_start = eb.position_to_offset(first, 0);
            let prev_text = eb.text_range(prev_start, block_start - 1);
            let block_end = self.line_end_offset(last);
            let removed = eb.delete_range((first - 1, 0), (first, 0));
            eb.set_cursor_by_offset(block_end - (block_start - prev_start));
            removed + eb.insert_text(&format!("\n{prev_text}"))
        } else {
            // Remove the line below (with the break before it), then reinsert
            // it before the block.
            let next_start = eb.position_to_offset(last + 1, 0);
            let next_end = self.line_end_offset(last + 1);
            let next_text = eb.text_range(next_start, next_end);
            let (Some(from), Some(to)) = (pos(next_start - 1), pos(next_end)) else {
                return;
            };
            let removed = eb.delete_range(from, to);
            eb.set_cursor(first, 0);
            removed + eb.insert_text(&format!("{next_text}\n"))
        };
        self.history.record(EditKind::Other, steps);

        let shift = |(row, col): (u32, u32)| if up { (row - 1, col) } else { (row + 1, col) };
        let (row, col) = shift((cursor.row, cursor.col));
        eb.set_cursor(row, col);
        if let (Some(start), Some(end)) = (sel_start, sel_end) {
            let (start, end) = (shift(start), shift(end));
            let start = eb.position_to_offset(start.0, start.1);
            let end = eb.position_to_offset(end.0, end.1);
            self.view.set_selection(start, end, SELECTION);
            self.anchor = Some(if anchor_is_start { start } else { end });
        }
    }

    /// The offset of the end of `row` (before its line break).
    fn line_end_offset(&self, row: u32) -> u32 {
        let eb = &*self.buffer;
        if row + 1 < eb.line_count() {
            return eb.position_to_offset(row + 1, 0) - 1;
        }
        // The last line: let the engine clamp a column past its end.
        let saved = eb.cursor().offset;
        eb.set_cursor(row, u32::MAX);
        let end = eb.cursor().offset;
        eb.set_cursor_by_offset(saved);
        end
    }

    /// Deletes the selected text; returns the undo snapshots recorded.
    fn delete_selection(&mut self) -> u32 {
        self.anchor = None;
        let steps = match self.view.selection() {
            Some((start, end)) if start != end => self.view.delete_selected_text(),
            _ => 0,
        };
        self.view.clear_selection();
        steps
    }

    fn undo(&mut self) {
        match self.history.undo() {
            Some(steps) => (0..steps).for_each(|_| {
                self.buffer.undo();
            }),
            None => self.show_message("Nothing to undo.", false),
        }
        self.anchor = None;
        self.view.clear_selection();
    }

    fn redo(&mut self) {
        match self.history.redo() {
            Some(steps) => (0..steps).for_each(|_| {
                self.buffer.redo();
            }),
            None => self.show_message("Nothing to redo.", false),
        }
        self.anchor = None;
        self.view.clear_selection();
    }

    // --- clipboard ---------------------------------------------------------

    fn copy(&mut self, clipboard: &mut Option<String>) -> Action {
        let text = self.view.selected_text();
        if text.is_empty() {
            self.show_message("Nothing selected.", false);
            return Action::Continue;
        }
        self.show_message(
            format!("Copied {} characters.", text.chars().count()),
            false,
        );
        *clipboard = Some(text.clone());
        Action::Copy(text)
    }

    fn cut(&mut self, clipboard: &mut Option<String>) -> Action {
        let action = self.copy(clipboard);
        if let Action::Copy(_) = action {
            let steps = self.delete_selection();
            self.history.break_group();
            self.history.record(EditKind::Other, steps);
        }
        action
    }

    fn paste_clipboard(&mut self, clipboard: Option<&str>) {
        match clipboard {
            Some(text) => self.edit(EditKind::Other, |eb| eb.insert_text(text)),
            None => self.show_message(
                "Nothing copied yet. Use your terminal's paste for the system clipboard.",
                false,
            ),
        }
    }

    // --- selection and movement --------------------------------------------

    /// Runs a cursor movement. With `select`, the selection extends from the
    /// anchor to the new cursor. Otherwise any selection is dropped, and the
    /// movement starts from the selection edge it heads away from: its start
    /// when moving backward (Up, Home, ...), its end when moving forward.
    fn move_cursor(&mut self, select: bool, direction: Direction, movement: impl FnOnce(&Self)) {
        self.history.break_group();
        if !select {
            if let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) {
                let edge = match direction {
                    Direction::Backward => start,
                    Direction::Forward => end,
                };
                self.view.set_cursor_by_offset(edge);
            }
            self.anchor = None;
            self.view.clear_selection();
            movement(self);
            return;
        }
        let cursor = self.buffer.cursor().offset;
        let anchor = *self.anchor.get_or_insert(cursor);
        movement(self);
        self.select_to_cursor(anchor);
    }

    /// Unshifted Left/Right with a selection moves to its start/end instead
    /// of stepping. Returns false when there is no selection.
    fn collapse_selection(&mut self, to_start: bool) -> bool {
        let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) else {
            return false;
        };
        self.history.break_group();
        self.anchor = None;
        self.view.clear_selection();
        self.view
            .set_cursor_by_offset(if to_start { start } else { end });
        true
    }

    fn select_all(&mut self) {
        self.history.break_group();
        self.move_to_document_end();
        self.anchor = Some(0);
        self.select_to_cursor(0);
    }

    fn move_to_document_start(&self) {
        self.buffer.set_cursor(0, 0);
    }

    fn move_to_document_end(&self) {
        let last_line = self.buffer.line_count().saturating_sub(1);
        self.buffer.set_cursor(last_line, u32::MAX);
    }

    /// Selects from `anchor` to the cursor, or nothing if they coincide.
    fn select_to_cursor(&mut self, anchor: u32) {
        let cursor = self.buffer.cursor().offset;
        if cursor == anchor {
            self.view.clear_selection();
        } else {
            self.view.set_selection(anchor, cursor, SELECTION);
        }
    }

    /// Shift+click: moves the cursor to `at`, selecting from the anchor.
    fn extend_selection_to(&mut self, at: (u32, u32)) {
        let anchor = match (self.anchor, self.view.selection()) {
            (Some(anchor), _) => anchor,
            _ => self.buffer.cursor().offset,
        };
        self.anchor = Some(anchor);
        // A zero-width local selection just moves the cursor.
        self.view
            .set_local_selection(cell(at), cell(at), SelectionBehavior::Cell, true, SELECTION);
        self.select_to_cursor(anchor);
    }

    /// Turns the mouse selection into an offset selection that survives
    /// scrolling, with the anchor where the drag started.
    fn finish_drag(&mut self, drag: Drag) {
        let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) else {
            self.view.clear_selection();
            self.anchor = None;
            return;
        };
        let backwards = (drag.focus.1, drag.focus.0) < (drag.origin.1, drag.origin.0);
        let (anchor, cursor) = if backwards {
            (end, start)
        } else {
            (start, end)
        };
        self.view.set_cursor_by_offset(cursor);
        self.view.set_selection(start, end, SELECTION);
        self.anchor = Some(anchor);
    }

    fn click_count(&mut self, at: (u32, u32), now: Instant) -> u32 {
        let count = match &self.last_click {
            Some(last) if last.at == at && now.duration_since(last.time) < MULTI_CLICK => {
                last.count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            at,
            time: now,
            count,
        });
        count
    }

    /// Scrolls the viewport, dragging the cursor along so it stays visible.
    fn scroll(&mut self, dx: i64, dy: i64) {
        if dx != 0 && self.wrap != WrapMode::None {
            return;
        }
        let vp = self.view.viewport();
        let max_y = self
            .view
            .total_virtual_line_count()
            .saturating_sub(vp.height);
        let y = (vp.y as i64 + dy).clamp(0, max_y as i64) as u32;
        let x = (vp.x as i64 + dx).max(0) as u32;
        if (x, y) != (vp.x, vp.y) {
            self.history.break_group();
            self.view.scroll_to(x, y, true);
        }
    }

    // --- status bar, files, misc --------------------------------------------

    /// Returns the cursor position when the prompt has focus.
    fn draw_status(
        &self,
        frame: &Buffer,
        keymap: &Keymap,
        workspace: &Workspace,
    ) -> Option<(u32, u32)> {
        if self.height < 2 {
            return None;
        }
        let y = self.height - 1;
        let x0 = self.x;

        if let Some(prompt) = &self.prompt {
            frame.fill_rect(x0, y, self.width, 1, STATUS_BG);
            let label = " Save as: ";
            frame.draw_text(label, x0, y, STATUS_DIM, None, Attributes::NONE);
            let x = x0 + label.len() as u32;
            // Keep the end of a long path visible.
            let room = (x0 + self.width).saturating_sub(x + 1) as usize;
            let chars: Vec<char> = prompt.input.chars().collect();
            let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
            frame.draw_text(&shown, x, y, STATUS_FG, None, Attributes::NONE);
            return Some((x + shown.chars().count() as u32, y));
        }

        if let Some(message) = &self.message {
            let bg = if message.error {
                STATUS_ERROR_BG
            } else {
                STATUS_BG
            };
            frame.fill_rect(x0, y, self.width, 1, bg);
            frame.draw_text(
                &format!(" {}", message.text),
                x0,
                y,
                STATUS_FG,
                None,
                Attributes::BOLD,
            );
            return None;
        }

        frame.fill_rect(x0, y, self.width, 1, STATUS_BG);
        let cursor = self.buffer.cursor();
        let name = match &self.file.path {
            Some(path) => workspace.display_path(path),
            None => "[new file]".to_string(),
        };
        let dirty = if self.history.is_modified() {
            " [+]"
        } else {
            ""
        };
        let wrap = match self.wrap {
            WrapMode::None => "nowrap",
            _ => "wrap",
        };
        let selected = match self.view.selection() {
            Some((start, end)) if start != end => format!(" ({} sel)", end - start),
            _ => String::new(),
        };
        let info = format!(
            "{dirty}  Ln {}, Col {}{selected}  {}  {wrap}",
            cursor.row + 1,
            cursor.col + 1,
            self.file.line_ending.label(),
        );
        // Shorten the path from the left so the rest of the status stays visible.
        let room = (self.width as usize).saturating_sub(info.chars().count() + 1);
        let left = format!(" {}{info}", truncate_left(&name, room));
        frame.draw_text(&left, x0, y, STATUS_FG, None, Attributes::BOLD);
        let hints: String = [
            (Command::Save, "save"),
            (Command::FocusTree, "files"),
            (Command::Palette, "commands"),
            (Command::Quit, "quit"),
        ]
        .iter()
        .filter_map(|&(command, name)| Some(format!("{:#} {name}  ", keymap.shortcut(command)?)))
        .collect();
        let hints = hints.strip_suffix(' ').unwrap_or(&hints);
        let hints_x = self.width.saturating_sub(hints.len() as u32);
        if hints_x as usize > left.chars().count() {
            frame.draw_text(hints, x0 + hints_x, y, STATUS_DIM, None, Attributes::NONE);
        }
        None
    }

    /// Writes the file to `path` from now on.
    pub fn save_as(&mut self, path: PathBuf) -> Action {
        self.file.path = Some(path);
        self.save()
    }

    /// Writes the file, or asks for a name if it has none.
    fn save(&mut self) -> Action {
        let Some(path) = self.file.path.clone() else {
            self.prompt = Some(Prompt {
                input: String::new(),
            });
            return Action::Continue;
        };
        let text = self.buffer.text();
        match document::save(&path, &text, self.file.line_ending) {
            Ok(()) => {
                self.history.mark_saved();
                let lines = self.buffer.line_count();
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                self.show_message(format!("Wrote {name} ({lines} lines)"), false);
                Action::Saved
            }
            // Reason first: the status bar clips long paths on the right.
            Err(err) => {
                self.show_message(format!("Can't save: {err} ({})", path.display()), true);
                Action::Continue
            }
        }
    }

    /// A key for the "Save as" prompt, while it is open.
    pub fn handle_prompt_key(&mut self, Key { code, mods }: Key) -> Action {
        let Some(prompt) = self.prompt.as_mut() else {
            return Action::Continue;
        };
        match code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Char('c' | 'q') if mods.ctrl || mods.sup => self.prompt = None,
            KeyCode::Enter => {
                let input = prompt.input.trim().to_string();
                self.prompt = None;
                if !input.is_empty() {
                    return Action::SaveAs(PathBuf::from(input));
                }
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Char(c) if mods.is_plain() => prompt.input.push(c),
            _ => {}
        }
        Action::Continue
    }

    fn toggle_wrap(&mut self) {
        self.wrap = match self.wrap {
            WrapMode::None => WrapMode::Word,
            _ => WrapMode::None,
        };
        self.view.set_wrap_mode(self.wrap);
    }

    fn page(&self) -> u32 {
        self.text_area().2.saturating_sub(1).max(1)
    }
}

fn cell((x, y): (u32, u32)) -> (i32, i32) {
    (x as i32, y as i32)
}

/// The last `max` characters of `s`, marked with a leading ellipsis if cut.
fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("…{tail}")
}

/// Columns for line numbers: the widest number with a space before it and
/// two after, or none when the screen is too narrow to spare them.
fn gutter_width(width: u32, line_count: u32) -> u32 {
    let gutter = line_count.max(1).ilog10() + 4;
    if width >= gutter + MIN_TEXT_WIDTH {
        gutter
    } else {
        0
    }
}

/// The text area, as (x, width, height): everything right of the line
/// numbers and above the status bar row.
fn text_area(width: u32, height: u32, line_count: u32) -> (u32, u32, u32) {
    let gutter = gutter_width(width, line_count);
    (
        gutter,
        width.saturating_sub(gutter).max(1),
        height.saturating_sub(1).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use crate::keymap::Context;
    use opentui::{OwnedBuffer, WidthMethod};
    use std::fs;
    use std::sync::MutexGuard;
    use std::time::Duration;

    /// The native core is single-threaded and the harness runs tests in parallel.
    fn serial() -> MutexGuard<'static, ()> {
        let guard = crate::test_serial();
        CLIPBOARD.with(|clipboard| clipboard.borrow_mut().take());
        guard
    }

    thread_local! {
        /// The clipboard the app would share between editors.
        static CLIPBOARD: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    /// Key handling as the app does it for a focused editor.
    trait HandleKey {
        fn handle_key(&mut self, key: Key) -> Action;
    }

    impl HandleKey for Editor {
        fn handle_key(&mut self, key: Key) -> Action {
            self.clear_message();
            if self.prompt_open() {
                return match self.handle_prompt_key(key) {
                    Action::SaveAs(path) => self.save_as(path),
                    action => action,
                };
            }
            match Keymap::default().lookup(key, Context::Editor) {
                Some((command, select)) => CLIPBOARD
                    .with(|clipboard| self.run(command, select, &mut clipboard.borrow_mut())),
                None => {
                    self.type_key(key);
                    Action::Continue
                }
            }
        }
    }

    /// Draws `editor` on a cleared `screen`, as the app does.
    fn draw(editor: &Editor, screen: &OwnedBuffer) -> (u32, u32) {
        screen.clear(Rgba::terminal_default([0, 0, 0]));
        let workspace = Workspace::new([]).unwrap();
        editor.draw(screen, &Keymap::default(), &workspace)
    }

    fn unnamed() -> File {
        File {
            path: None,
            line_ending: LineEnding::Lf,
        }
    }

    fn press(editor: &mut Editor, keys: &str) {
        for c in keys.chars() {
            editor.handle_key(Key::new(KeyCode::Char(c), Mods::NONE));
        }
    }

    fn key(editor: &mut Editor, code: KeyCode) {
        editor.handle_key(Key::new(code, Mods::NONE));
    }

    fn shift(editor: &mut Editor, code: KeyCode) {
        editor.handle_key(Key::new(code, Mods::SHIFT));
    }

    fn ctrl(editor: &mut Editor, c: char) -> Action {
        editor.handle_key(Key::new(KeyCode::Char(c), Mods::CTRL))
    }

    /// Line numbers take this many columns while the text has under 10 lines.
    const GUTTER: u32 = 4;

    /// A mouse event at text column `x` (right of the line numbers).
    fn mouse(editor: &mut Editor, kind: MouseKind, x: u32, y: u32, now: Instant) {
        editor.handle_mouse(
            Mouse {
                kind,
                x: GUTTER + x,
                y,
                mods: Mods::NONE,
            },
            now,
        );
    }

    fn status(editor: &Editor) -> String {
        let screen = OwnedBuffer::new(60, 4, false, WidthMethod::Unicode, "test").unwrap();
        draw(editor, &screen);
        screen
            .to_text(true)
            .lines()
            .nth(3)
            .unwrap()
            .trim_end()
            .to_string()
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qedit-editor-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn save_writes_the_file_and_clears_modified() {
        let _serial = serial();
        let path = temp_path("save.txt");
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("one\n");
        let file = File {
            path: Some(path.clone()),
            line_ending: LineEnding::CrLf,
        };
        let mut editor = Editor::new(eb.clone(), file, 60, 4).unwrap();
        assert!(!status(&editor).contains("[+]"));

        eb.set_cursor(1, 0);
        press(&mut editor, "two");
        assert!(
            status(&editor).contains("save.txt [+]"),
            "{}",
            status(&editor)
        );

        ctrl(&mut editor, 's');
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\ntwo");
        assert!(
            status(&editor).starts_with(" Wrote "),
            "{}",
            status(&editor)
        );
        key(&mut editor, KeyCode::Left);
        assert!(!status(&editor).contains("[+]"), "{}", status(&editor));
    }

    #[test]
    fn save_as_prompt_names_a_new_file() {
        let _serial = serial();
        let path = temp_path("prompted.txt");
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        press(&mut editor, "hi");
        ctrl(&mut editor, 's');
        assert!(status(&editor).starts_with(" Save as:"));

        // Typing goes to the prompt, not the buffer; Esc cancels.
        press(&mut editor, "zzz");
        key(&mut editor, KeyCode::Esc);
        assert_eq!(eb.text(), "hi");

        ctrl(&mut editor, 's');
        editor.paste(path.to_str().unwrap());
        key(&mut editor, KeyCode::Enter);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        assert!(status(&editor).starts_with(" Wrote "));
    }

    #[test]
    fn undo_and_redo_step_through_words_and_restore_the_cursor() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        press(&mut editor, "hello world");
        key(&mut editor, KeyCode::Enter);
        press(&mut editor, "again");
        key(&mut editor, KeyCode::Backspace);
        key(&mut editor, KeyCode::Backspace);

        let expect = [
            "hello world\naga",
            "hello world\nagain",
            "hello world\n",
            "hello world",
            "hello ",
            "",
        ];
        for text in expect.iter().skip(1) {
            ctrl(&mut editor, 'z');
            assert_eq!(eb.text(), *text);
        }
        assert!(
            !status(&editor).contains("[+]"),
            "back at the (empty) saved state"
        );
        ctrl(&mut editor, 'z');
        assert_eq!(status(&editor), " Nothing to undo.");

        for text in expect.iter().rev().skip(1) {
            ctrl(&mut editor, 'y');
            assert_eq!(eb.text(), *text);
        }
        assert_eq!(eb.cursor().col, 3, "cursor restored with the text");
    }

    #[test]
    fn delete_forward_is_one_undo_step() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("abcdef");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        eb.set_cursor(0, 1);
        key(&mut editor, KeyCode::Delete);
        key(&mut editor, KeyCode::Delete);
        key(&mut editor, KeyCode::End);
        key(&mut editor, KeyCode::Delete); // at end: no change, still one native snapshot
        assert_eq!(eb.text(), "adef");
        ctrl(&mut editor, 'z');
        assert_eq!(
            eb.text(),
            "adef",
            "the no-op delete is its own (empty) step"
        );
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "abcdef");
        assert!(!eb.can_undo());
    }

    #[test]
    fn shift_arrows_select_and_typing_replaces() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("hello world");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        eb.set_cursor(0, 6);
        for _ in 0..5 {
            shift(&mut editor, KeyCode::Right);
        }
        assert!(status(&editor).contains("(5 sel)"), "{}", status(&editor));
        press(&mut editor, "there");
        assert_eq!(eb.text(), "hello there");

        // One undo restores the selected word.
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "hello world");

        // Shrinking back to the anchor clears the selection; Left collapses.
        eb.set_cursor(0, 5);
        shift(&mut editor, KeyCode::Left);
        shift(&mut editor, KeyCode::Left);
        shift(&mut editor, KeyCode::Right);
        assert_eq!(editor.view.selected_text(), "o");
        key(&mut editor, KeyCode::Left);
        assert_eq!(editor.view.selection(), None);
        assert_eq!(eb.cursor().col, 4);
    }

    #[test]
    fn vertical_moves_leave_a_selection_from_the_matching_edge() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("0123456789\nabcdefghij\nklmnopqrst\nuvwxyz");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 8).unwrap();
        let pos = |eb: &EditBuffer| (eb.cursor().row, eb.cursor().col);

        // Select forward (1,2) -> (2,5), cursor at the end: select `from` to
        // `to` with Shift+Down/Right steps.
        let select = |editor: &mut Editor, from: (u32, u32), downs: u32, rights: i32| {
            editor.handle_key(Key::new(KeyCode::Esc, Mods::NONE));
            editor.buffer.set_cursor(from.0, from.1);
            for _ in 0..downs {
                shift(editor, KeyCode::Down);
            }
            let step = if rights < 0 {
                KeyCode::Left
            } else {
                KeyCode::Right
            };
            for _ in 0..rights.unsigned_abs() {
                shift(editor, step);
            }
        };

        select(&mut editor, (1, 2), 1, 3);
        assert_eq!(editor.view.selected_text(), "cdefghij\nklmno");
        assert_eq!(pos(&eb), (2, 5), "cursor at the selection end");
        key(&mut editor, KeyCode::Up);
        assert_eq!(pos(&eb), (0, 2), "Up leaves from the selection start");
        assert_eq!(editor.view.selection(), None);

        // Backward: anchor (2,5), cursor at the start (1,2).
        select(&mut editor, (2, 5), 0, 0);
        editor.handle_key(Key::new(KeyCode::Esc, Mods::NONE));
        eb.set_cursor(2, 5);
        shift(&mut editor, KeyCode::Up);
        for _ in 0..3 {
            shift(&mut editor, KeyCode::Left);
        }
        assert_eq!(editor.view.selected_text(), "cdefghij\nklmno");
        assert_eq!(pos(&eb), (1, 2), "cursor at the selection start");
        key(&mut editor, KeyCode::Down);
        assert_eq!(pos(&eb), (3, 5), "Down leaves from the selection end");

        // Home/End and PageUp/PageDown follow the same rule.
        select(&mut editor, (1, 2), 1, 3);
        key(&mut editor, KeyCode::Home);
        assert_eq!(pos(&eb), (1, 0));
        select(&mut editor, (2, 5), 0, 0);
        eb.set_cursor(2, 5);
        shift(&mut editor, KeyCode::Up);
        key(&mut editor, KeyCode::End);
        assert_eq!(pos(&eb), (2, 10));
        select(&mut editor, (1, 2), 1, 3);
        key(&mut editor, KeyCode::PageUp);
        assert_eq!(pos(&eb).0, 0);
    }

    fn alt(editor: &mut Editor, code: KeyCode, shift: bool) {
        editor.handle_key(Key::new(
            code,
            Mods {
                alt: true,
                shift,
                ..Mods::NONE
            },
        ));
    }

    fn pos(eb: &EditBuffer) -> (u32, u32) {
        (eb.cursor().row, eb.cursor().col)
    }

    #[test]
    fn word_jumps_select_and_delete() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("let x = foo(bar);\nnext");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 6).unwrap();
        alt(&mut editor, KeyCode::Right, false);
        assert_eq!(pos(&eb), (0, 3));
        alt(&mut editor, KeyCode::Right, true);
        alt(&mut editor, KeyCode::Right, true);
        assert_eq!(editor.view.selected_text(), " x =");
        // Ctrl+Right is the same on Linux/Windows; Alt+F is the emacs form.
        editor.handle_key(Key::new(KeyCode::Right, Mods::CTRL));
        assert_eq!(pos(&eb), (0, 11), "from the selection end over 'foo'");
        alt(&mut editor, KeyCode::Char('f'), false);
        assert_eq!(pos(&eb), (0, 12), "'(' is a punctuation run");
        alt(&mut editor, KeyCode::Char('b'), false);
        assert_eq!(pos(&eb), (0, 11));

        // Alt+Backspace deletes back to the word start: one undo step each.
        eb.set_cursor(0, 15); // after "bar"
        alt(&mut editor, KeyCode::Backspace, false);
        assert_eq!(eb.text(), "let x = foo();\nnext");
        alt(&mut editor, KeyCode::Backspace, false);
        assert_eq!(eb.text(), "let x = foo);\nnext", "'(' is its own run");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "let x = foo();\nnext");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "let x = foo(bar);\nnext");

        // Alt+Delete deletes forward, across the line break.
        eb.set_cursor(0, 17);
        alt(&mut editor, KeyCode::Delete, false);
        assert_eq!(eb.text(), "let x = foo(bar);");
        assert_eq!(pos(&eb), (0, 17));
        eb.set_cursor(0, 0);
        alt(&mut editor, KeyCode::Char('d'), false);
        assert_eq!(eb.text(), " x = foo(bar);");
        alt(&mut editor, KeyCode::Backspace, false);
        assert_eq!(eb.text(), " x = foo(bar);", "nothing before the start");
    }

    #[test]
    fn alt_up_down_move_lines() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("one\ntwo\nthree\nfour");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 8).unwrap();

        eb.set_cursor(1, 2);
        alt(&mut editor, KeyCode::Down, false);
        assert_eq!(eb.text(), "one\nthree\ntwo\nfour");
        assert_eq!(pos(&eb), (2, 2), "cursor stays on the moved line");
        alt(&mut editor, KeyCode::Down, false);
        assert_eq!(eb.text(), "one\nthree\nfour\ntwo");
        alt(&mut editor, KeyCode::Down, false);
        assert_eq!(eb.text(), "one\nthree\nfour\ntwo", "already last");
        for _ in 0..4 {
            alt(&mut editor, KeyCode::Up, false);
        }
        assert_eq!(eb.text(), "two\none\nthree\nfour");
        assert_eq!(pos(&eb), (0, 2));

        // One undo per move.
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "one\ntwo\nthree\nfour");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "one\nthree\ntwo\nfour");
    }

    #[test]
    fn alt_up_down_move_selected_lines_and_keep_the_selection() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("a\nbb\ncc\nd\ne");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 8).unwrap();

        // Select from (1,1) to (2,1): lines "bb" and "cc".
        eb.set_cursor(1, 1);
        shift(&mut editor, KeyCode::Down);
        alt(&mut editor, KeyCode::Down, false);
        assert_eq!(eb.text(), "a\nd\nbb\ncc\ne");
        assert_eq!(editor.view.selected_text(), "b\nc");
        alt(&mut editor, KeyCode::Up, false);
        alt(&mut editor, KeyCode::Up, false);
        assert_eq!(eb.text(), "bb\ncc\na\nd\ne");
        assert_eq!(editor.view.selected_text(), "b\nc");
        // The anchor stays where the selection started.
        shift(&mut editor, KeyCode::Right);
        assert_eq!(editor.view.selected_text(), "b\ncc");

        // A selection ending at column 0 doesn't drag that line along.
        editor.handle_key(Key::new(KeyCode::Esc, Mods::NONE));
        eb.set_cursor(0, 0);
        shift(&mut editor, KeyCode::Down);
        alt(&mut editor, KeyCode::Down, false);
        assert_eq!(eb.text(), "cc\nbb\na\nd\ne");
    }

    #[test]
    fn moving_the_last_line_keeps_line_breaks_consistent() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("x\n漢字 wide\nlast");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 8).unwrap();
        eb.set_cursor(2, 4);
        alt(&mut editor, KeyCode::Up, false);
        assert_eq!(eb.text(), "x\nlast\n漢字 wide");
        assert_eq!(pos(&eb), (1, 4));
        eb.set_cursor(2, 3);
        alt(&mut editor, KeyCode::Up, false);
        alt(&mut editor, KeyCode::Up, false);
        assert_eq!(eb.text(), "漢字 wide\nx\nlast");
        assert_eq!(pos(&eb), (0, 3));
    }

    #[test]
    fn select_all_cut_and_paste() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("one\ntwo");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        ctrl(&mut editor, 'a');
        assert_eq!(editor.view.selected_text(), "one\ntwo");
        let Action::Copy(text) = ctrl(&mut editor, 'x') else {
            panic!("cut should copy");
        };
        assert_eq!(text, "one\ntwo");
        assert_eq!(eb.text(), "");
        ctrl(&mut editor, 'v');
        ctrl(&mut editor, 'v');
        assert_eq!(eb.text(), "one\ntwoone\ntwo");
        assert!(
            matches!(ctrl(&mut editor, 'c'), Action::Continue),
            "nothing selected"
        );
    }

    #[test]
    fn backspace_deletes_only_the_selection() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("abcdef");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        eb.set_cursor(0, 2);
        shift(&mut editor, KeyCode::Right);
        shift(&mut editor, KeyCode::Right);
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(eb.text(), "abef");
        assert_eq!(eb.cursor().col, 2);
    }

    fn cmd(editor: &mut Editor, c: char) -> Action {
        editor.handle_key(Key::new(KeyCode::Char(c), Mods::SUPER))
    }

    #[test]
    fn cmd_shortcuts_match_ctrl() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        press(&mut editor, "one two");
        cmd(&mut editor, 'a');
        let Action::Copy(text) = cmd(&mut editor, 'c') else {
            panic!("Cmd+C should copy the selection");
        };
        assert_eq!(text, "one two");
        assert!(matches!(cmd(&mut editor, 'x'), Action::Copy(_)));
        assert_eq!(eb.text(), "");
        cmd(&mut editor, 'z');
        assert_eq!(eb.text(), "one two", "Cmd+Z undoes the cut");
        let cmd_shift_z = Key::new(
            KeyCode::Char('z'),
            Mods {
                shift: true,
                ..Mods::SUPER
            },
        );
        editor.handle_key(cmd_shift_z);
        assert_eq!(eb.text(), "", "Cmd+Shift+Z redoes");
        cmd(&mut editor, 'v');
        assert_eq!(eb.text(), "one two");

        // modifyOtherKeys may report Ctrl+Shift+Z with an uppercase letter.
        let ctrl_shift_upper_z = Key::new(
            KeyCode::Char('Z'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "");
        editor.handle_key(ctrl_shift_upper_z);
        assert_eq!(eb.text(), "one two");
    }

    #[test]
    fn modified_keys_never_type() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        for mods in [Mods::SUPER, Mods::CTRL, alt] {
            for c in ['b', 'k', 'é', '1', '\t'] {
                let code = if c == '\t' {
                    KeyCode::Tab
                } else {
                    KeyCode::Char(c)
                };
                editor.handle_key(Key::new(code, mods));
            }
        }
        assert_eq!(eb.text(), "");
        assert!(!eb.can_undo());
    }

    #[test]
    fn cmd_arrows_follow_macos_conventions() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("first line\nsecond line\nthird");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        eb.set_cursor(1, 3);
        let cmd_key = |editor: &mut Editor, code, shift| {
            editor.handle_key(Key::new(
                code,
                Mods {
                    shift,
                    ..Mods::SUPER
                },
            ));
        };
        cmd_key(&mut editor, KeyCode::Right, false);
        assert_eq!((eb.cursor().row, eb.cursor().col), (1, 11));
        cmd_key(&mut editor, KeyCode::Left, true);
        assert_eq!(editor.view.selected_text(), "second line");
        cmd_key(&mut editor, KeyCode::Down, false);
        assert_eq!((eb.cursor().row, eb.cursor().col), (2, 5));
        cmd_key(&mut editor, KeyCode::Up, true);
        assert_eq!(
            editor.view.selected_text(),
            "first line\nsecond line\nthird"
        );
        editor.handle_key(Key::new(KeyCode::End, Mods::CTRL));
        assert_eq!((eb.cursor().row, eb.cursor().col), (2, 5));
        editor.handle_key(Key::new(KeyCode::Home, Mods::CTRL));
        assert_eq!((eb.cursor().row, eb.cursor().col), (0, 0));
    }

    #[test]
    fn mouse_click_drag_and_multi_click() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("hello world\nsecond line");
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 4).unwrap();
        let t0 = Instant::now();
        let left = MouseButton::Left;

        // Click places the cursor.
        mouse(&mut editor, MouseKind::Press(left), 4, 1, t0);
        mouse(&mut editor, MouseKind::Release(left), 4, 1, t0);
        assert_eq!((eb.cursor().row, eb.cursor().col), (1, 4));
        assert_eq!(editor.view.selection(), None);

        // Drag backwards from (8,1) to (2,0). Both end cells are included, as
        // in terminal selection; the cursor ends at the start.
        let t1 = t0 + Duration::from_secs(1);
        mouse(&mut editor, MouseKind::Press(left), 8, 1, t1);
        mouse(&mut editor, MouseKind::Drag(left), 5, 0, t1);
        mouse(&mut editor, MouseKind::Drag(left), 2, 0, t1);
        mouse(&mut editor, MouseKind::Release(left), 2, 0, t1);
        assert_eq!(editor.view.selected_text(), "llo world\nsecond li");
        assert_eq!((eb.cursor().row, eb.cursor().col), (0, 2));
        // Shift+Right moves the start; the drag origin stays the anchor.
        shift(&mut editor, KeyCode::Right);
        assert_eq!(editor.view.selected_text(), "lo world\nsecond li");

        // Double click selects a word; a third click the line.
        let t2 = t1 + Duration::from_secs(1);
        for i in 0..2 {
            mouse(
                &mut editor,
                MouseKind::Press(left),
                8,
                0,
                t2 + Duration::from_millis(100 * i),
            );
            mouse(
                &mut editor,
                MouseKind::Release(left),
                8,
                0,
                t2 + Duration::from_millis(100 * i),
            );
        }
        assert_eq!(editor.view.selected_text(), "world");
        mouse(
            &mut editor,
            MouseKind::Press(left),
            8,
            0,
            t2 + Duration::from_millis(200),
        );
        mouse(
            &mut editor,
            MouseKind::Release(left),
            8,
            0,
            t2 + Duration::from_millis(200),
        );
        assert!(editor.view.selected_text().starts_with("hello world"));

        // Shift+click extends from the cursor.
        let t3 = t2 + Duration::from_secs(1);
        mouse(&mut editor, MouseKind::Press(left), 0, 0, t3);
        mouse(&mut editor, MouseKind::Release(left), 0, 0, t3);
        editor.handle_mouse(
            Mouse {
                kind: MouseKind::Press(left),
                x: GUTTER + 5,
                y: 1,
                mods: Mods::SHIFT,
            },
            t3 + Duration::from_secs(1),
        );
        assert_eq!(editor.view.selected_text(), "hello world\nsecon");

        // Clicks on the status bar are ignored.
        mouse(
            &mut editor,
            MouseKind::Press(left),
            1,
            3,
            t3 + Duration::from_secs(2),
        );
        assert_eq!(editor.view.selected_text(), "hello world\nsecon");
    }

    #[test]
    fn wheel_scrolls_and_keeps_cursor_in_view() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let text: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        eb.set_text(&text.join("\n"));
        eb.set_cursor(0, 0);
        let mut editor = Editor::new(eb.clone(), unnamed(), 60, 6).unwrap();
        let now = Instant::now();
        for _ in 0..4 {
            mouse(&mut editor, MouseKind::ScrollDown, 0, 0, now);
        }
        let screen = OwnedBuffer::new(60, 6, false, WidthMethod::Unicode, "test").unwrap();
        let (_, cursor_row) = draw(&editor, &screen);
        let first = screen
            .to_text(true)
            .lines()
            .next()
            .unwrap()
            .trim_end()
            .to_string();
        assert_eq!(first, " 13  line 12");
        assert!(cursor_row < 5);
        for _ in 0..10 {
            mouse(&mut editor, MouseKind::ScrollUp, 0, 0, now);
        }
        draw(&editor, &screen);
        assert_eq!(
            screen.to_text(true).lines().next().unwrap().trim_end(),
            "  1  line 0"
        );
    }

    fn screen_lines(editor: &Editor, width: u32, height: u32) -> (Vec<String>, (u32, u32)) {
        let screen = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
        let cursor = draw(editor, &screen);
        let lines = screen
            .to_text(true)
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        (lines, cursor)
    }

    #[test]
    fn line_numbers_label_first_rows_and_widen_with_the_text() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("one\ntwo two two two two two two\nthree");
        eb.set_cursor(1, 2);
        let mut editor = Editor::new(eb.clone(), unnamed(), 30, 6).unwrap();
        let (lines, cursor) = screen_lines(&editor, 30, 6);
        assert_eq!(
            lines[..3],
            [" 1  one", " 2  two two two two two two tw", " 3  three"]
        );
        assert_eq!(cursor, (GUTTER + 2, 1), "cursor is right of the numbers");

        // Wrapped rows are left unnumbered.
        ctrl(&mut editor, 'w');
        let (lines, _) = screen_lines(&editor, 30, 6);
        assert_eq!(
            lines[..4],
            [
                " 1  one",
                " 2  two two two two two two",
                "    two",
                " 3  three"
            ]
        );

        // A tenth line adds a digit, narrowing the text.
        ctrl(&mut editor, 'w');
        eb.set_cursor(2, 5);
        for _ in 0..7 {
            key(&mut editor, KeyCode::Enter);
        }
        let (lines, _) = screen_lines(&editor, 30, 6);
        assert_eq!(lines[4], " 10", "{lines:?}");
        assert_eq!(editor.view.viewport().width, 30 - 5);

        // Too narrow to spare the columns: no numbers.
        editor.set_area(0, 20, 6);
        editor.handle_key(Key::new(KeyCode::Home, Mods::CTRL));
        let (lines, cursor) = screen_lines(&editor, 20, 6);
        assert_eq!(lines[0], "one", "{lines:?}");
        assert_eq!(cursor, (0, 0));
    }
}
