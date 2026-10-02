//! A view of an open file: editing commands on top of OpenTUI's
//! `EditBuffer` and `EditorView`, which own the text, cursor, selection, and
//! scrolling.
//!
//! Several editors, in different panels, may show one [`Document`]. The
//! buffer has one cursor, which belongs to the editor last used (see
//! [`Editor::attach`]); the others keep theirs parked in the document.
//!
//! Over the top right of the text, the find bar (Ctrl+F) finds and replaces
//! in the file: it highlights every match and selects the current one.

use std::cell::RefMut;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use opentui::{
    Attributes, Buffer, EditBuffer, EditorView, Highlight, SelectionBehavior, SelectionColors,
    Viewport, WrapMode,
};

use crate::config::{self, Config};
#[cfg(test)]
use crate::document::File;
use crate::document::{Disk, Document};
use crate::find::{self, Field, FindBar, Match, Target};
use crate::history::{EditKind, History};
use crate::indent::Indent;
use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Keymap};
use crate::line_edit::Edit;
use crate::location::Position;
use crate::search::Toggle;
use crate::status::Status;
use crate::theme;
#[cfg(test)]
use crate::theme::Theme;
use crate::words;

/// Selected text's colors, in the theme in use.
fn selection_colors() -> SelectionColors {
    SelectionColors {
        bg: theme::colors().selection,
        fg: None,
    }
}

/// The find bar's current match, which is selected.
pub fn current_match() -> SelectionColors {
    let colors = theme::colors();
    SelectionColors {
        bg: colors.current_match_bg,
        fg: Some(colors.current_match_fg),
    }
}
/// Tags the find bar's highlights.
const FIND_HIGHLIGHTS: u16 = 1;

const WHEEL_LINES: u32 = 3;
/// Line numbers are hidden when they would leave the text less room than this.
const MIN_TEXT_WIDTH: u32 = 20;

pub enum Action {
    Continue,
    /// Put this text on the system clipboard.
    Copy(String),
    /// The file was written, possibly to a new path.
    Saved,
    /// The file has no name yet: the app asks where to save it, and calls
    /// [`Editor::save_as`].
    SaveAs,
    /// The file changed on disk while this had unsaved changes: the app
    /// asks whether to overwrite it or take the file's text.
    Conflict,
}

struct Message {
    text: String,
    error: bool,
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
    /// Tells the editors of a document apart, for its cursor.
    id: u64,
    doc: Rc<Document>,
    /// The document's buffer.
    buffer: Rc<EditBuffer>,
    view: EditorView<'static>,
    /// The fixed end of the selection that keyboard movement extends from.
    anchor: Option<u32>,
    drag: Option<Drag>,
    last_click: Option<Click>,
    wrap: WrapMode,
    /// The screen column of the editor's left edge, and row of its top.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    message: Option<Message>,
    find: Option<FindBar>,
}

impl Editor {
    /// An editor `width` x `height` of `buffer`'s text, saved to `file`.
    #[cfg(test)]
    pub fn new(
        buffer: Rc<EditBuffer>,
        file: File,
        theme: Rc<Theme>,
        width: u32,
        height: u32,
    ) -> opentui::Result<Editor> {
        Editor::show(Rc::new(Document::new(buffer, file, theme)), width, height)
    }

    /// An editor `width` x `height` of `doc`, with line numbers down the
    /// left. It starts with the buffer's cursor.
    pub fn show(doc: Rc<Document>, width: u32, height: u32) -> opentui::Result<Editor> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let buffer = doc.buffer.clone();
        let (_, view_w, view_h) = text_area(width, height, buffer.line_count());
        let view = buffer.shared_view(view_w, view_h)?;
        let config = config::get();
        let wrap = wrap_mode(config.wrap);
        view.set_wrap_mode(wrap);
        view.set_scroll_margin(config.scroll_margin);
        let mut editor = Editor {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            doc,
            buffer,
            view,
            anchor: None,
            drag: None,
            last_click: None,
            wrap,
            x: 0,
            y: 0,
            width,
            height,
            message: None,
            find: None,
        };
        editor.attach();
        Ok(editor)
    }

    /// The document shown.
    pub fn document(&self) -> &Rc<Document> {
        &self.doc
    }

    /// Gives this editor the buffer's cursor, if another editor of the
    /// document had it, and keeps the view on it. Call it before editing,
    /// moving, or scrolling; the other editors' views stay where they are.
    pub fn attach(&mut self) {
        let doc = &self.doc;
        if doc.cursor_owner.get() == Some(self.id) {
            return;
        }
        if let Some(owner) = doc.cursor_owner.get() {
            doc.park(owner);
        }
        doc.cursor_owner.set(Some(self.id));
        self.view.take_cursor();
        // Typing in one editor and then another makes two undo steps.
        doc.history.borrow_mut().break_group();
        if let Some(parked) = doc.unpark(self.id) {
            if parked.epoch != self.buffer.content_epoch() {
                // Edited elsewhere: the selection may cover other text now.
                self.anchor = None;
                self.view.clear_selection();
            }
            self.buffer.set_cursor(parked.row, parked.col);
        }
        // The document's find highlights are this editor's matches.
        match &mut self.find {
            Some(bar) => {
                bar.epoch = bar.epoch.map(|_| u64::MAX);
                self.sync_find();
            }
            None => self.buffer.remove_highlights(FIND_HIGHLIGHTS),
        }
    }

    /// The cursor's row and column: the buffer's, or where it was parked
    /// while another editor of the document has it.
    fn cursor(&self) -> (u32, u32) {
        match self.doc.parked(self.id) {
            Some(parked) if self.doc.cursor_owner.get() != Some(self.id) => {
                (parked.row, parked.col)
            }
            _ => {
                let cursor = self.buffer.cursor();
                (cursor.row, cursor.col)
            }
        }
    }

    fn history(&self) -> RefMut<'_, History> {
        self.doc.history.borrow_mut()
    }

    /// Places the editor at column `x`, row `y`, `width` x `height`.
    pub fn set_area(&mut self, x: u32, y: u32, width: u32, height: u32) {
        self.x = x;
        self.y = y;
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

    /// Where the find bar floats, if open: the top right of the text, a
    /// column in from the edge, as (screen column, width, rows).
    fn find_area(&self) -> Option<(u32, u32, u32)> {
        let bar = self.find.as_ref()?;
        let (gutter, text_w, _) = self.text_area();
        let width = find::MAX_WIDTH.min(text_w.saturating_sub(1)).max(1);
        let x = self.x + gutter + text_w.saturating_sub(width + 1);
        Some((x, width, bar.rows()))
    }

    /// The file's path; `None` until it is first saved.
    pub fn path(&self) -> Option<PathBuf> {
        self.doc.path()
    }

    #[cfg(test)]
    pub fn is_modified(&self) -> bool {
        self.doc.is_modified()
    }

    /// An unnamed document that was never typed in, which opening a file
    /// may replace.
    pub fn is_blank(&self) -> bool {
        self.doc.is_blank()
    }

    /// The whole text, with `\n` line breaks.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// The selected text, if anything is selected.
    pub fn selected_text(&self) -> Option<String> {
        self.view
            .selection()
            .filter(|(s, e)| s != e)
            .map(|_| self.view.selected_text())
    }

    /// Selects the bytes `range` of line `row` (0-based), or puts the
    /// cursor at its start if it's empty. A line off screen is scrolled to
    /// a third of the way down.
    pub fn select_in_line(&mut self, row: u32, range: Range<usize>) {
        self.history().break_group();
        self.anchor = None;
        self.view.clear_selection();
        // Before the cursor moves: the view follows it.
        let vp = self.view.viewport();
        let eb = &*self.buffer;
        let row = row.min(eb.line_count().saturating_sub(1));
        eb.set_cursor(row, 0);
        let start = step_bytes(eb, range.start);
        let end = step_bytes(eb, range.end.saturating_sub(range.start));
        if start != end {
            self.anchor = Some(start);
            self.view.set_selection(start, end, selection_colors());
        }
        self.reveal(vp);
    }

    /// Puts the cursor at `position`, as a compiler printed it: the line's
    /// start without a column, and its end past it.
    pub fn go_to(&mut self, position: Position) {
        let text = self.buffer.text();
        let row = position
            .line
            .min(self.buffer.line_count().saturating_sub(1));
        let line = text.split('\n').nth(row as usize).unwrap_or_default();
        let column = position.column.unwrap_or(0) as usize;
        let byte = line
            .char_indices()
            .nth(column)
            .map_or(line.len(), |(byte, _)| byte);
        self.select_in_line(row, byte..byte);
    }

    /// Where it is in the file, for a session to keep: the cursor's row
    /// and column, and the row at the top of the view.
    pub fn place(&self) -> (u32, u32, u32) {
        let (row, col) = self.cursor();
        (row, col, self.view.viewport().y)
    }

    /// Goes back to a place [`Editor::place`] gave, as near as the text has
    /// it now.
    pub fn set_place(&mut self, row: u32, col: u32, top: u32) {
        self.attach();
        self.anchor = None;
        self.view.clear_selection();
        let row = row.min(self.buffer.line_count().saturating_sub(1));
        self.buffer.set_cursor(row, col);
        let max_top = self.view.total_virtual_line_count().saturating_sub(1);
        self.view.scroll_to(0, top.min(max_top), false);
    }

    /// Scrolls the cursor's row a third of the way down if it was off screen
    /// in `vp`, the viewport before the cursor moved.
    fn reveal(&self, vp: Viewport) {
        // Wrapped, lines and rows differ: find the cursor's row among the
        // wrapped ones. With a selection, the view doesn't follow the cursor,
        // so its row relative to the viewport can't be above it.
        let row = self.view.visual_cursor_absolute().row;
        if !(vp.y..vp.y + vp.height).contains(&row) {
            let max_y = self
                .view
                .total_virtual_line_count()
                .saturating_sub(vp.height);
            let y = row.saturating_sub(vp.height / 3).min(max_y);
            // Without moving the cursor, the view scrolls sideways to it.
            self.view.scroll_to(0, y, false);
        }
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

    /// Types a key bound to no command. Keys with Ctrl/Alt/Cmd held never
    /// type.
    pub fn type_key(&mut self, key: Key) {
        if let KeyCode::Char(c) = key.code {
            if key.mods.is_plain() {
                let mut utf8 = [0u8; 4];
                let text: &str = c.encode_utf8(&mut utf8);
                self.edit(EditKind::Type(c), |eb| eb.insert_text(text));
                self.sync_find();
            }
        }
    }

    /// Runs an editing or find bar command; others are ignored. With
    /// `select`, a cursor movement extends the selection. `clipboard` is
    /// shared by all editors.
    pub fn run(
        &mut self,
        command: Command,
        select: bool,
        clipboard: &mut Option<String>,
    ) -> Action {
        let action = self.run_command(command, select, clipboard);
        self.sync_find();
        action
    }

    fn run_command(
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
            // With nothing selected, Esc closes the find bar.
            Command::ClearSelection if !self.has_selection() && self.find.is_some() => {
                self.close_find()
            }
            Command::ClearSelection => {
                self.anchor = None;
                self.view.clear_selection();
            }
            Command::FindSwitchField => self.switch_find_field(),
            Command::FindClose => self.close_find(),
            Command::Replace => self.replace(),
            Command::ReplaceAll => self.replace_all(),
            command if Toggle::for_command(command).is_some() => {
                if let (Some(bar), Some(toggle)) = (&mut self.find, Toggle::for_command(command)) {
                    bar.toggle(toggle);
                }
            }
            Command::ToggleWrap => self.toggle_wrap(),
            Command::NewLine => self.edit(EditKind::Other, EditBuffer::new_line),
            // With lines selected, Tab indents them, as in most editors.
            Command::InsertTab if self.selection_spans_lines() => self.indent_lines(Forward),
            Command::InsertTab => self.insert_indent(),
            Command::Indent => self.indent_lines(Forward),
            Command::Outdent => self.indent_lines(Backward),
            Command::DeleteBackward if self.delete_indent() => {}
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
        let (text_x, text_w, text_h) = self.text_area();
        // A drag that started in the text stays with it.
        let pressed = matches!(mouse.kind, MouseKind::Press(_));
        if (self.drag.is_none() || pressed) && self.handle_find_mouse(mouse) {
            self.drag = None;
            return;
        }
        // Clicks on the line numbers go to the start of the line.
        let at = (
            mouse.x.saturating_sub(text_x).min(text_w - 1),
            mouse.y.min(text_h - 1),
        );
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) if mouse.y < text_h => {
                self.message = None;
                self.history().break_group();
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
                self.view.set_local_selection(
                    cell(at),
                    cell(at),
                    behavior,
                    true,
                    selection_colors(),
                );
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
                        selection_colors(),
                    );
                }
            }
            MouseKind::Release(MouseButton::Left) => {
                if let Some(drag) = self.drag.take() {
                    self.finish_drag(drag);
                }
            }
            MouseKind::ScrollUp if mouse.mods.shift => self.scroll(-(WHEEL_LINES as i64), 0),
            MouseKind::ScrollDown if mouse.mods.shift => self.scroll(WHEEL_LINES as i64, 0),
            MouseKind::ScrollUp => self.scroll(0, -(WHEEL_LINES as i64)),
            MouseKind::ScrollDown => self.scroll(0, WHEEL_LINES as i64),
            MouseKind::ScrollLeft => self.scroll(-(WHEEL_LINES as i64), 0),
            MouseKind::ScrollRight => self.scroll(WHEEL_LINES as i64, 0),
            _ => {}
        }
    }

    /// A mouse event on the find bar, which it handles; returns false if
    /// it's elsewhere. A press elsewhere gives the text the keyboard.
    fn handle_find_mouse(&mut self, mouse: Mouse) -> bool {
        let Some((x, width, rows)) = self.find_area() else {
            return false;
        };
        let screen_x = self.x + mouse.x;
        let inside = (x..x + width).contains(&screen_x) && mouse.y < rows;
        let press = matches!(mouse.kind, MouseKind::Press(MouseButton::Left));
        let Some(bar) = &mut self.find else {
            return false;
        };
        if !inside {
            if let MouseKind::Press(_) = mouse.kind {
                bar.focus = None;
            }
            return false;
        }
        // The wheel scrolls the text under the bar.
        if matches!(
            mouse.kind,
            MouseKind::ScrollUp
                | MouseKind::ScrollDown
                | MouseKind::ScrollLeft
                | MouseKind::ScrollRight
        ) {
            return false;
        }
        if !press {
            return true;
        }
        match bar.target(screen_x, mouse.y) {
            Target::Field(field) => bar.focus = Some(field),
            Target::Toggle(toggle) => bar.toggle(toggle),
            Target::Expander => {
                bar.replacing = !bar.replacing;
                bar.focus = Some(if bar.replacing {
                    Field::Replace
                } else {
                    Field::Find
                });
            }
            Target::Close => self.close_find(),
            Target::Replace => self.replace(),
            Target::ReplaceAll => self.replace_all(),
        }
        self.sync_find();
        self.keep_clear_of_find();
        true
    }

    pub fn paste(&mut self, text: &str) {
        // Terminals send newlines in pastes as CR.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.edit(EditKind::Other, |eb| eb.insert_text(&text));
        self.sync_find();
    }

    /// Draws the editor in its area and returns the terminal cursor
    /// position (0-based column, row). The keymap labels the find bar's
    /// buttons.
    pub fn draw(&self, frame: &Buffer, keymap: &Keymap) -> (u32, u32) {
        self.sync_view_size();
        self.sync_syntax();
        let (gutter, _, _) = self.text_area();
        let text_x = self.x + gutter;
        frame.draw_editor_view(&self.view, text_x as i32, self.y as i32);
        self.draw_line_numbers(frame, gutter);
        let find_cursor = self
            .find
            .as_ref()
            .zip(self.find_area())
            .and_then(|(bar, area)| {
                let (x, width, _) = area;
                bar.draw(frame, (x, self.y, width), self.current_match(), keymap)
            });
        find_cursor.unwrap_or_else(|| {
            let cursor = self.view.visual_cursor();
            (text_x + cursor.col, self.y + cursor.row)
        })
    }

    /// Highlights the lines about to be drawn.
    fn sync_syntax(&self) {
        let mut syntax = self.doc.syntax.borrow_mut();
        let Some(highlighter) = syntax.as_mut() else {
            return;
        };
        let lines = self.view.visible_lines();
        if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
            highlighter.sync(&self.buffer, first.line..last.line + 1);
        }
    }

    /// Numbers the first row of each visible line in the `gutter` columns
    /// left of the text, highlighting the cursor's line.
    fn draw_line_numbers(&self, frame: &Buffer, gutter: u32) {
        let colors = theme::colors();
        if gutter == 0 {
            return;
        }
        let (current, _) = self.cursor();
        let digits = gutter as usize - 3;
        for (y, row) in self.view.visible_lines().iter().enumerate() {
            if row.wrap != 0 {
                continue;
            }
            let (fg, attributes) = if row.line == current {
                (colors.line_number_current, Attributes::BOLD)
            } else {
                (colors.faint, Attributes::NONE)
            };
            let number = format!("{:>digits$}", row.line + 1);
            frame.draw_text(&number, self.x + 1, self.y + y as u32, fg, None, attributes);
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
            self.history().break_group();
        }
        let steps = replaced + f(&self.buffer);
        self.history().record(kind, steps);
    }

    /// Backspace/Delete: removes the selection if there is one, otherwise a
    /// character.
    fn delete(&mut self, delete_char: impl FnOnce(&EditBuffer) -> u32) {
        let removed = self.delete_selection();
        if removed > 0 {
            self.history().break_group();
            self.history().record(EditKind::Other, removed);
        } else {
            let steps = delete_char(&self.buffer);
            self.history().record(EditKind::Delete, steps);
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
        self.history().break_group();
        let steps = eb.delete_range(start, end);
        self.history().record(EditKind::Other, steps);
    }

    /// Tab: inserts a tab, or spaces to the next multiple of the indent
    /// width, as the file indents.
    fn insert_indent(&mut self) {
        match self.doc.indent.get() {
            Indent::Tabs => self.edit(EditKind::Type('\t'), |eb| eb.insert_text("\t")),
            Indent::Spaces(width) => self.edit(EditKind::Type(' '), |eb| {
                let col = eb.cursor().col;
                eb.insert_text(&" ".repeat((width - col % width) as usize))
            }),
        }
    }

    /// Backspace in indentation of spaces deletes back to the previous
    /// multiple of the indent width, as Tab inserted it. Returns false,
    /// deleting nothing, anywhere else.
    fn delete_indent(&mut self) -> bool {
        let Indent::Spaces(width) = self.doc.indent.get() else {
            return false;
        };
        let eb = &*self.buffer;
        let cursor = eb.cursor();
        if cursor.col == 0 || self.has_selection() {
            return false;
        }
        let line_start = eb.position_to_offset(cursor.row, 0);
        let before = eb.text_range(line_start, cursor.offset);
        if !before.bytes().all(|b| b == b' ') {
            return false;
        }
        let spaces = before.len() as u32;
        let count = match spaces % width {
            0 => width,
            over => over,
        };
        let steps = eb.delete_range((cursor.row, spaces - count), (cursor.row, spaces));
        self.history().record(EditKind::Delete, steps);
        true
    }

    /// Whether the selection covers more than one line.
    fn selection_spans_lines(&self) -> bool {
        let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) else {
            return false;
        };
        let row = |offset| self.buffer.offset_to_position(offset).map(|p| p.row);
        row(start) != row(end)
    }

    /// Tab with lines selected, Shift+Tab, and Ctrl+] / Ctrl+[: indents
    /// (`Forward`) or outdents the lines holding the cursor or selection one
    /// level, as one undo step. The cursor and selection stay on their text;
    /// a selection from the start of a line takes in the indentation added.
    fn indent_lines(&mut self, direction: Direction) {
        let indent = self.doc.indent.get();
        let buffer = self.buffer.clone();
        let eb = &*buffer;
        let cursor = eb.cursor().offset;
        let selection = self.view.selection().filter(|(s, e)| s != e);
        let anchor = selection.map(|(start, end)| {
            self.anchor
                .unwrap_or(if cursor == start { end } else { start })
        });
        // Where an offset is, as its line and the bytes before it there.
        let locate = |offset: u32| {
            let p = eb.offset_to_position(offset)?;
            let line_start = eb.position_to_offset(p.row, 0);
            Some((p.row, eb.text_range(line_start, offset).len()))
        };
        let Some(at_cursor) = locate(cursor) else {
            return;
        };
        let at_anchor = match anchor.map(locate) {
            Some(None) => return,
            located => located.flatten(),
        };
        let (start, end) = match at_anchor {
            Some(at) => (at.min(at_cursor), at.max(at_cursor)),
            None => (at_cursor, at_cursor),
        };
        let first = start.0;
        // A selection ending at the start of a line doesn't take it in.
        let last = match end {
            (row, 0) if row > first => row - 1,
            (row, _) => row,
        };

        let text = eb.text();
        let mut changed = String::with_capacity(text.len());
        // Bytes each changed line gained (or with a minus, lost), by row.
        let mut shifts: Vec<(u32, isize)> = Vec::new();
        for (row, line) in text.split('\n').enumerate() {
            let row = row as u32;
            if row > 0 {
                changed.push('\n');
            }
            let new = match direction {
                _ if !(first..=last).contains(&row) => None,
                Direction::Forward => indent.indent(line),
                Direction::Backward => {
                    let cut = indent.outdent(line);
                    (cut > 0).then(|| line[cut..].to_string())
                }
            };
            match new {
                Some(new) => {
                    shifts.push((row, new.len() as isize - line.len() as isize));
                    changed.push_str(&new);
                }
                None => changed.push_str(line),
            }
        }
        if shifts.is_empty() {
            return;
        }
        self.history().break_group();
        self.view.clear_selection();
        let steps = eb.replace_changed_lines(&changed);
        self.history().record(EditKind::Other, steps);
        self.history().break_group();

        // A selection's start at the start of its line stays there.
        let shift = |(row, byte): (u32, usize)| {
            let keep_start = selection.is_some() && (row, byte) == start && byte == 0;
            let byte = match shifts.iter().find(|&&(r, _)| r == row) {
                Some(_) if keep_start => 0,
                Some(&(_, gained)) => byte.saturating_add_signed(gained),
                None => byte,
            };
            eb.set_cursor(row, 0);
            step_bytes(eb, byte)
        };
        match at_anchor {
            Some(at_anchor) => {
                let anchor = shift(at_anchor);
                let cursor = shift(at_cursor);
                self.view.set_cursor_by_offset(cursor);
                self.view
                    .set_selection(anchor.min(cursor), anchor.max(cursor), selection_colors());
                self.anchor = Some(anchor);
            }
            None => {
                self.anchor = None;
                let cursor = shift(at_cursor);
                self.view.set_cursor_by_offset(cursor);
            }
        }
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

        self.history().break_group();
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
        self.history().record(EditKind::Other, steps);

        let shift = |(row, col): (u32, u32)| if up { (row - 1, col) } else { (row + 1, col) };
        let (row, col) = shift((cursor.row, cursor.col));
        eb.set_cursor(row, col);
        if let (Some(start), Some(end)) = (sel_start, sel_end) {
            let (start, end) = (shift(start), shift(end));
            let start = eb.position_to_offset(start.0, start.1);
            let end = eb.position_to_offset(end.0, end.1);
            self.view.set_selection(start, end, selection_colors());
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
        let undo = self.history().undo();
        match undo {
            Some(steps) => (0..steps).for_each(|_| {
                self.buffer.undo();
            }),
            None => self.show_message("Nothing to undo.", false),
        }
        self.anchor = None;
        self.view.clear_selection();
    }

    fn redo(&mut self) {
        let redo = self.history().redo();
        match redo {
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
            self.history().break_group();
            self.history().record(EditKind::Other, steps);
        }
        action
    }

    fn paste_clipboard(&mut self, clipboard: Option<&str>) {
        match clipboard {
            Some(text) => self.edit(EditKind::Other, |eb| eb.insert_text(text)),
            None => self.show_message(
                "Nothing copied yet. Use your terminal's paste command to paste from the system clipboard.",
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
        self.history().break_group();
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
        self.history().break_group();
        self.anchor = None;
        self.view.clear_selection();
        self.view
            .set_cursor_by_offset(if to_start { start } else { end });
        true
    }

    fn select_all(&mut self) {
        self.history().break_group();
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
            self.view.set_selection(anchor, cursor, selection_colors());
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
        self.view.set_local_selection(
            cell(at),
            cell(at),
            SelectionBehavior::Cell,
            true,
            selection_colors(),
        );
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
        self.view.set_selection(start, end, selection_colors());
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
    /// It scrolls past the end of the text, until the last line is near the
    /// top.
    fn scroll(&mut self, dx: i64, dy: i64) {
        if dx != 0 && self.wrap != WrapMode::None {
            return;
        }
        let vp = self.view.viewport();
        // The cursor, dragged to the last line, stays the scroll margin
        // from the top, or the view would scroll back to it.
        let margin = ((vp.height as f32 * config::get().scroll_margin) as u32).max(1);
        let max_y = self
            .view
            .total_virtual_line_count()
            .saturating_sub(1 + margin);
        let y = (vp.y as i64 + dy).clamp(0, max_y as i64) as u32;
        let x = (vp.x as i64 + dx).max(0) as u32;
        if (x, y) != (vp.x, vp.y) {
            self.history().break_group();
            self.view.scroll_to(x, y, true);
        }
    }

    fn has_selection(&self) -> bool {
        self.view.selection().is_some_and(|(s, e)| s != e)
    }

    // --- find and replace --------------------------------------------------

    /// Opens the find bar with `memory`'s query, or the selection if it's on
    /// one line, and focuses the query. With the query focused already, it
    /// closes the bar. With `replacing`, it shows the replacement too and
    /// focuses it if there's a query; with the replacement focused already,
    /// it hides it.
    pub fn show_find(&mut self, memory: &find::Memory, replacing: bool) {
        let focus = self.find.as_ref().and_then(|bar| bar.focus);
        match focus {
            Some(Field::Find) if !replacing => return self.close_find(),
            Some(Field::Replace) if replacing => {
                if let Some(bar) = &mut self.find {
                    bar.replacing = false;
                    bar.focus = Some(Field::Find);
                }
                return;
            }
            _ => {}
        }
        let selected = self.selected_text().filter(|text| !text.contains('\n'));
        let origin = self.selection_start();
        let bar = self
            .find
            .get_or_insert_with(|| FindBar::new(memory.clone(), origin));
        if let Some(text) = selected {
            if text != bar.memory.query.text {
                bar.memory.query.text = text;
                bar.carets[Field::Find as usize].move_to_end();
                bar.epoch = None;
            }
            bar.origin = origin;
        }
        bar.replacing |= replacing;
        let has_query = !bar.memory.query.text.is_empty();
        bar.focus = Some(if replacing && has_query {
            Field::Replace
        } else {
            Field::Find
        });
        // Typing replaces the query, as if it were selected.
        bar.replace_query = has_query && bar.focus == Some(Field::Find);
        self.sync_find();
        self.keep_clear_of_find();
    }

    /// Selects the next match, or with `forward` false the previous one,
    /// after the current match or the cursor. With the find bar closed, it
    /// opens with `memory`'s query, keeping focus in the text, or if there's
    /// no query, to type one.
    pub fn find_step(&mut self, memory: &find::Memory, forward: bool) {
        if self.find.is_none() {
            if memory.query.text.is_empty() {
                return self.show_find(memory, false);
            }
            let mut bar = FindBar::new(memory.clone(), self.selection_start());
            bar.focus = None;
            self.find = Some(bar);
        }
        self.sync_find();
        let Some(bar) = &self.find else {
            return;
        };
        let cursor = self.buffer.cursor().offset;
        let count = bar.matches.len();
        let index = match self.current_match() {
            Some(i) if forward => Some((i + 1) % count),
            Some(i) => Some((i + count - 1) % count),
            None if forward => bar.next_from(cursor),
            None => bar.previous_from(cursor),
        };
        if let Some(index) = index {
            self.select_match(index);
        }
    }

    /// The find bar's query and replacement, while it's open.
    pub fn find_memory(&self) -> Option<&find::Memory> {
        self.find.as_ref().map(|bar| &bar.memory)
    }

    /// The find bar field with the keyboard, if any.
    pub fn find_field(&self) -> Option<Field> {
        self.find.as_ref().and_then(|bar| bar.focus)
    }

    pub fn find_open(&self) -> bool {
        self.find.is_some()
    }

    /// Gives the keyboard back to the text.
    pub fn blur_find(&mut self) {
        if let Some(bar) = &mut self.find {
            bar.focus = None;
        }
    }

    /// Edits the focused find bar field: typing, pasting, deleting, or
    /// moving the cursor.
    pub fn find_edit(&mut self, edit: Edit) {
        if let Some(bar) = &mut self.find {
            bar.edit(edit);
        }
        self.sync_find();
    }

    /// Closes the find bar, leaving the current match selected.
    fn close_find(&mut self) {
        if self.find.take().is_none() {
            return;
        }
        self.buffer.remove_highlights(FIND_HIGHLIGHTS);
        if let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) {
            self.view.set_selection(start, end, selection_colors());
        }
    }

    fn switch_find_field(&mut self) {
        let Some(bar) = self.find.as_mut().filter(|bar| bar.replacing) else {
            return;
        };
        bar.focus = match bar.focus {
            Some(Field::Find) => Some(Field::Replace),
            _ => Some(Field::Find),
        };
        bar.replace_query = false;
    }

    /// Finds the matches again if the text or the query changed. After the
    /// query changes, the first match from where finding started is selected.
    fn sync_find(&mut self) {
        let epoch = self.buffer.content_epoch();
        if self
            .find
            .as_ref()
            .is_none_or(|bar| bar.epoch == Some(epoch))
        {
            return;
        }
        let style = self.doc.theme.find_match;
        let Some(bar) = &mut self.find else {
            return;
        };
        // Opening the bar doesn't move the cursor; a new query does.
        let jump = bar.epoch.is_none() && bar.focus == Some(Field::Find);
        bar.epoch = Some(epoch);
        let text = self.buffer.text();
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
        let bytes: Vec<u32> = ranges
            .iter()
            .flat_map(|r| [r.start as u32, r.end as u32])
            .collect();
        let cursors = self.buffer.bytes_to_cursors(&bytes);
        bar.matches = ranges
            .into_iter()
            .zip(cursors.chunks(2))
            .map(|(bytes, ends)| Match {
                bytes,
                row: ends[0].row,
                cols: ends[0].col..ends[1].col,
                offsets: ends[0].offset..ends[1].offset,
            })
            .collect();
        let highlights: Vec<Highlight> = bar
            .matches
            .iter()
            .map(|m| Highlight {
                line: m.row,
                start: m.cols.start,
                end: m.cols.end,
                style,
                priority: 1,
                tag: FIND_HIGHLIGHTS,
            })
            .collect();
        self.buffer.replace_highlights(FIND_HIGHLIGHTS, &highlights);
        if jump {
            let origin = bar.origin;
            match bar.next_from(origin) {
                Some(index) => self.select_match(index),
                None => {
                    self.anchor = None;
                    self.view.clear_selection();
                    self.view.set_cursor_by_offset(origin);
                }
            }
        }
    }

    /// The match that is selected, if any.
    fn current_match(&self) -> Option<usize> {
        let bar = self.find.as_ref()?;
        let (start, end) = self.view.selection()?;
        bar.at(start.min(end), start.max(end))
    }

    /// Selects match `index`, with the cursor at its end, scrolling to it.
    fn select_match(&mut self, index: usize) {
        let Some(bar) = &mut self.find else {
            return;
        };
        let Some(m) = bar.matches.get(index).cloned() else {
            return;
        };
        bar.origin = m.offsets.start;
        self.history().break_group();
        let vp = self.view.viewport();
        self.view.set_cursor_by_offset(m.offsets.end);
        self.anchor = Some(m.offsets.start);
        self.view
            .set_selection(m.offsets.start, m.offsets.end, current_match());
        self.reveal(vp);
        self.keep_clear_of_find();
    }

    /// Scrolls the text down if the find bar covers the cursor, as it does
    /// a match near the top right. At the top of the file there's no room
    /// to, and the bar stays over it.
    fn keep_clear_of_find(&self) {
        let Some((x, _, rows)) = self.find_area() else {
            return;
        };
        let (gutter, _, _) = self.text_area();
        // The bar's first column, in the viewport.
        let left = x - (self.x + gutter);
        // Laying out scrolls the cursor into view first.
        let cursor = self.view.visual_cursor();
        if cursor.row >= rows || cursor.col <= left {
            return;
        }
        let vp = self.view.viewport();
        let y = vp.y.saturating_sub(rows - cursor.row);
        if y != vp.y {
            self.view.scroll_to(vp.x, y, false);
        }
    }

    /// The start of the selection, or the cursor.
    fn selection_start(&self) -> u32 {
        match self.view.selection() {
            Some((start, end)) if start != end => start.min(end),
            _ => self.buffer.cursor().offset,
        }
    }

    /// Replaces the current match and selects the next one. Without a
    /// current match, it just selects the next one.
    fn replace(&mut self) {
        self.sync_find();
        let Some(bar) = &self.find else {
            return;
        };
        if let Some(index) = self.current_match() {
            let m = &bar.matches[index];
            let text = self.buffer.text();
            let replacer = find::Replacer::new(&bar.memory.query, &bar.memory.replacement);
            let replacement = replacer.expand(&text, m.bytes.clone());
            self.history().break_group();
            self.edit(EditKind::Other, |eb| eb.insert_text(&replacement));
            self.history().break_group();
            self.sync_find();
        }
        let Some(bar) = &self.find else {
            return;
        };
        if let Some(next) = bar.next_from(self.buffer.cursor().offset) {
            self.select_match(next);
        }
    }

    /// Replaces every match, as one undo step.
    fn replace_all(&mut self) {
        self.sync_find();
        let Some(bar) = &self.find else {
            return;
        };
        if bar.matches.is_empty() {
            return self.show_message("Nothing to replace.", false);
        }
        let text = self.buffer.text();
        let replacer = find::Replacer::new(&bar.memory.query, &bar.memory.replacement);
        let mut replaced = String::with_capacity(text.len());
        let mut end = 0;
        for m in &bar.matches {
            replaced.push_str(&text[end..m.bytes.start]);
            replaced.push_str(&replacer.expand(&text, m.bytes.clone()));
            end = m.bytes.end;
        }
        replaced.push_str(&text[end..]);
        let count = bar.matches.len();
        let more = if bar.truncated {
            " (there are more)"
        } else {
            ""
        };
        // Matches never span lines, so the cursor's line keeps its place.
        let cursor = self.buffer.cursor();
        self.history().break_group();
        let steps = self.buffer.replace_changed_lines(&replaced);
        self.history().record(EditKind::Other, steps);
        self.history().break_group();
        self.anchor = None;
        self.view.clear_selection();
        self.buffer.set_cursor(cursor.row, cursor.col);
        let noun = if count == 1 { "match" } else { "matches" };
        self.show_message(format!("Replaced {count} {noun}{more}."), false);
        self.sync_find();
    }

    // --- status, files, misc --------------------------------------------------

    /// What the status bar shows while this editor is active.
    pub fn status(&self) -> Status {
        if let Some(message) = &self.message {
            return Status::Message {
                text: message.text.clone(),
                error: message.error,
            };
        }
        // While typing a query that isn't a valid regex, why.
        if let Some(error) = self
            .find
            .as_ref()
            .filter(|bar| bar.focus == Some(Field::Find))
            .and_then(FindBar::error)
        {
            return Status::Message {
                text: format!("Invalid regex: {error}"),
                error: true,
            };
        }
        let (row, col) = self.cursor();
        let selected = match self.view.selection() {
            Some((start, end)) if start != end => format!(" ({} sel)", end - start),
            _ => String::new(),
        };
        let indent = self.doc.indent.get().label();
        let language = self.doc.language.get().map_or("Plain Text", |l| l.name);
        let line_ending = self.doc.file.borrow().line_ending.label();
        let wrap = match self.wrap {
            WrapMode::None => "nowrap",
            _ => "wrap",
        };
        let prefix = format!("Ln {}, Col {}{selected}  {indent}  ", row + 1, col + 1);
        let start = 1 + prefix.chars().count() as u32;
        Status::EditorInfo {
            text: format!("{prefix}{language}  {line_ending}  {wrap}"),
            language: start..start + language.chars().count() as u32,
        }
    }

    /// Writes the file to `path` from now on.
    pub fn save_as(&mut self, path: PathBuf) -> Action {
        self.doc.rename(path.clone());
        self.write(path)
    }

    /// Writes the file, or asks for a name if it has none, or asks what to
    /// do if it changed on disk.
    fn save(&mut self) -> Action {
        let Some(path) = self.path() else {
            return Action::SaveAs;
        };
        // In case the change hasn't been heard of yet.
        self.doc.check_disk();
        if self.doc.disk() == Disk::Changed {
            return Action::Conflict;
        }
        self.write(path)
    }

    /// Writes the file to `path`, whatever is there.
    pub fn write(&mut self, path: PathBuf) -> Action {
        match self.doc.save(&path) {
            Ok(()) => {
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

    /// Takes up the `editor.wrap` and `editor.scroll_margin` settings,
    /// changed from `old`. Wrapping changes only if its setting did, since
    /// Toggle Word Wrap may have set it here.
    /// Recolors the selection, in the theme in use.
    pub fn restyle(&mut self) {
        let Some((start, end)) = self.view.selection().filter(|(s, e)| s != e) else {
            return;
        };
        let colors = match self.current_match() {
            Some(_) => current_match(),
            None => selection_colors(),
        };
        self.view.set_selection(start, end, colors);
    }

    pub fn follow_settings(&mut self, old: &Config, new: &Config) {
        if new.wrap != old.wrap {
            self.set_wrap(wrap_mode(new.wrap));
        }
        self.view.set_scroll_margin(new.scroll_margin);
    }

    fn toggle_wrap(&mut self) {
        self.set_wrap(match self.wrap {
            WrapMode::None => WrapMode::Word,
            _ => WrapMode::None,
        });
    }

    fn set_wrap(&mut self, wrap: WrapMode) {
        self.wrap = wrap;
        self.view.set_wrap_mode(wrap);
    }

    fn page(&self) -> u32 {
        self.text_area().2.saturating_sub(1).max(1)
    }
}

/// How the `editor.wrap` setting wraps lines.
fn wrap_mode(wrap: bool) -> WrapMode {
    match wrap {
        true => WrapMode::Word,
        false => WrapMode::None,
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        self.doc.forget(self.id);
    }
}

/// Moves the cursor right over `bytes` bytes of its line, a grapheme at a
/// time so that tabs and wide characters are counted as the engine lays
/// them out, and returns its offset. Stops at the end of the line.
fn step_bytes(eb: &EditBuffer, bytes: usize) -> u32 {
    let mut passed = 0;
    loop {
        let at = eb.cursor().offset;
        if passed >= bytes {
            return at;
        }
        eb.move_cursor_right();
        let next = eb.cursor().offset;
        let grapheme = eb.text_range(at, next);
        if next == at || grapheme.contains('\n') {
            eb.set_cursor_by_offset(at);
            return at;
        }
        passed += grapheme.len();
    }
}

fn cell((x, y): (u32, u32)) -> (i32, i32) {
    (x as i32, y as i32)
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
/// numbers.
fn text_area(width: u32, height: u32, line_count: u32) -> (u32, u32, u32) {
    let gutter = gutter_width(width, line_count);
    (gutter, width.saturating_sub(gutter).max(1), height.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::LineEnding;
    use crate::input::Mods;
    use crate::keymap::Context;
    use crate::theme;
    use opentui::{OwnedBuffer, Rgba, WidthMethod};
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
        editor.draw(screen, &Keymap::default())
    }

    fn theme() -> Rc<Theme> {
        Rc::new(Theme::new().unwrap())
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

    /// The status bar's text while `editor` is active.
    fn status(editor: &Editor) -> String {
        editor.status().text()
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cue-editor-{}", std::process::id()));
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
        let mut editor = Editor::new(eb.clone(), file, theme(), 60, 4).unwrap();
        assert!(!editor.is_modified());

        eb.set_cursor(1, 0);
        press(&mut editor, "two");
        assert!(editor.is_modified());

        ctrl(&mut editor, 's');
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\ntwo");
        assert!(
            status(&editor).starts_with(" Wrote "),
            "{}",
            status(&editor)
        );
        key(&mut editor, KeyCode::Left);
        assert!(!editor.is_modified());
    }

    #[test]
    fn saving_an_unnamed_file_asks_where() {
        let _serial = serial();
        let path = temp_path("named.txt");
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
        press(&mut editor, "hi");
        assert!(matches!(ctrl(&mut editor, 's'), Action::SaveAs));
        assert!(editor.is_modified());

        assert!(matches!(editor.save_as(path.clone()), Action::Saved));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        assert!(status(&editor).starts_with(" Wrote "));
        assert_eq!(editor.path(), Some(path));
    }

    #[test]
    fn status_shows_the_language() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let file = File {
            path: Some(PathBuf::from("main.rs")),
            line_ending: LineEnding::Lf,
        };
        let editor = Editor::new(eb, file, theme(), 60, 4).unwrap();
        assert!(
            status(&editor).contains("  Rust  LF"),
            "{}",
            status(&editor)
        );

        // A new file has none until it's named; then its #! line counts.
        let path = temp_path("script");
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("#!/usr/bin/env bash\n");
        let mut editor = Editor::new(eb, unnamed(), theme(), 60, 4).unwrap();
        assert!(
            status(&editor).contains("  Plain Text  LF"),
            "{}",
            status(&editor)
        );
        editor.save_as(path);
        editor.clear_message();
        assert!(
            status(&editor).contains("  Shell  LF"),
            "{}",
            status(&editor)
        );
    }

    #[test]
    fn undo_and_redo_step_through_words_and_restore_the_cursor() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        assert!(!editor.is_modified(), "back at the (empty) saved state");
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 8).unwrap();
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

    /// An editor of `text`, which is how it infers its indentation.
    fn editor_of(text: &str) -> (Rc<EditBuffer>, Editor) {
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text(text);
        let editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 8).unwrap();
        (eb, editor)
    }

    #[test]
    fn tab_indents_as_the_file_does() {
        let _serial = serial();
        let (eb, mut editor) = editor_of("fn a() {\n  b();\n}\n");
        assert!(
            status(&editor).contains("  Spaces: 2  "),
            "{}",
            status(&editor)
        );
        // To the next multiple of the width.
        eb.set_cursor(1, 3);
        key(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "fn a() {\n  b ();\n}\n");
        key(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "fn a() {\n  b   ();\n}\n");
        // Backspace in indentation deletes back to the previous stop.
        eb.set_cursor(1, 0);
        press(&mut editor, " ");
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(eb.text(), "fn a() {\n  b   ();\n}\n");
        eb.set_cursor(1, 2);
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(eb.text(), "fn a() {\nb   ();\n}\n");
        // Past the indentation, a character at a time.
        eb.set_cursor(1, 4);
        key(&mut editor, KeyCode::Backspace);
        assert_eq!(eb.text(), "fn a() {\nb  ();\n}\n");

        let (eb, mut editor) = editor_of("fn a() {\n\tb();\n}\n");
        assert!(status(&editor).contains("  Tabs  "), "{}", status(&editor));
        eb.set_cursor(0, 0);
        key(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "\tfn a() {\n\tb();\n}\n");
    }

    #[test]
    fn tab_and_shift_tab_indent_and_outdent_selected_lines() {
        let _serial = serial();
        let (eb, mut editor) = editor_of("a\n    b\n\n  c\nd");
        editor.doc.indent.set(Indent::Spaces(4));
        // From inside line 1 to inside line 3, backward.
        eb.set_cursor(3, 3);
        shift(&mut editor, KeyCode::Up);
        shift(&mut editor, KeyCode::Up);
        shift(&mut editor, KeyCode::Left);
        assert_eq!(editor.view.selected_text(), "  b\n\n  c");
        key(&mut editor, KeyCode::Tab);
        // Empty lines stay empty; the selection stays on its text.
        assert_eq!(eb.text(), "a\n        b\n\n      c\nd");
        assert_eq!(editor.view.selected_text(), "  b\n\n      c");
        assert_eq!(
            pos(&eb),
            (1, 6),
            "the cursor stays at the selection's start"
        );
        shift(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "a\n    b\n\n    c\nd", "to the previous stop");
        shift(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "a\nb\n\nc\nd");
        assert_eq!(editor.view.selected_text(), "b\n\nc");
        // Each was one undo step.
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "a\n    b\n\n    c\nd");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "a\n        b\n\n      c\nd");

        // Whole lines selected from the start of the first keep the
        // indentation added in the selection, and leave the next line be.
        let (eb, mut editor) = editor_of("a\nb\nc");
        eb.set_cursor(0, 0);
        shift(&mut editor, KeyCode::Down);
        shift(&mut editor, KeyCode::Down);
        key(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "    a\n    b\nc");
        assert_eq!(editor.view.selected_text(), "    a\n    b\n");

        // Without a selection, Shift+Tab and Ctrl+[ / Ctrl+] act on the
        // cursor's line, keeping the cursor on its text.
        let (eb, mut editor) = editor_of("    x = 1");
        eb.set_cursor(0, 6);
        shift(&mut editor, KeyCode::Tab);
        assert_eq!((eb.text().as_str(), pos(&eb)), ("x = 1", (0, 2)));
        ctrl(&mut editor, ']');
        assert_eq!((eb.text().as_str(), pos(&eb)), ("    x = 1", (0, 6)));
        ctrl(&mut editor, '[');
        assert_eq!(eb.text(), "x = 1");
        // A single-line selection is typed over.
        eb.set_cursor(0, 0);
        shift(&mut editor, KeyCode::Right);
        key(&mut editor, KeyCode::Tab);
        assert_eq!(eb.text(), "     = 1");
    }

    #[test]
    fn word_jumps_select_and_delete() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("let x = foo(bar);\nnext");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 6).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 8).unwrap();

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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 8).unwrap();

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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 8).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 4).unwrap();
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
    }

    #[test]
    fn wheel_scrolls_and_keeps_cursor_in_view() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let text: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        eb.set_text(&text.join("\n"));
        eb.set_cursor(0, 0);
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 60, 6).unwrap();
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

    #[test]
    fn horizontal_wheel_and_shift_wheel_survive_rendering() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text(&"0123456789".repeat(20));
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 6).unwrap();
        editor.set_wrap(WrapMode::None);
        let now = Instant::now();
        let wheel = |editor: &mut Editor, kind, mods| {
            editor.handle_mouse(
                Mouse {
                    kind,
                    x: 20,
                    y: 0,
                    mods,
                },
                now,
            );
            screen_lines(editor, 40, 6)
        };
        let (lines, _) = wheel(&mut editor, MouseKind::ScrollRight, Mods::NONE);
        assert_eq!(editor.view.viewport().x, WHEEL_LINES);
        assert!(lines[0].starts_with(" 1  3456789"), "{lines:?}");
        wheel(&mut editor, MouseKind::ScrollDown, Mods::SHIFT);
        assert_eq!(editor.view.viewport().x, 2 * WHEEL_LINES);
        assert_eq!(editor.view.viewport().y, 0);
        wheel(&mut editor, MouseKind::ScrollUp, Mods::SHIFT);
        assert_eq!(editor.view.viewport().x, WHEEL_LINES);
        wheel(&mut editor, MouseKind::ScrollLeft, Mods::NONE);
        assert_eq!(editor.view.viewport().x, 0);
        wheel(&mut editor, MouseKind::ScrollLeft, Mods::NONE);
        assert_eq!(editor.view.viewport().x, 0);

        // A floating find bar must let horizontal wheel events through.
        editor.show_find(&find::Memory::default(), false);
        wheel(&mut editor, MouseKind::ScrollRight, Mods::NONE);
        assert_eq!(editor.view.viewport().x, WHEEL_LINES);
        editor.close_find();

        // Wrapping already fits the text to the viewport.
        editor.set_wrap(WrapMode::Word);
        wheel(&mut editor, MouseKind::ScrollRight, Mods::NONE);
        wheel(&mut editor, MouseKind::ScrollDown, Mods::SHIFT);
        assert_eq!(editor.view.viewport().x, 0);
        assert_eq!(eb.text(), "0123456789".repeat(20));
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
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 30, 6).unwrap();
        editor.set_wrap(WrapMode::None);
        let (lines, cursor) = screen_lines(&editor, 30, 6);
        assert_eq!(
            lines[..3],
            [" 1  one", " 2  two two two two two two tw", " 3  three"]
        );
        assert_eq!(cursor, (GUTTER + 2, 1), "cursor is right of the numbers");

        // Wrapped rows are left unnumbered.
        editor.set_wrap(WrapMode::Word);
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
        editor.set_wrap(WrapMode::None);
        eb.set_cursor(2, 5);
        for _ in 0..7 {
            key(&mut editor, KeyCode::Enter);
        }
        let (lines, _) = screen_lines(&editor, 30, 6);
        assert_eq!(lines[5], " 10", "{lines:?}");
        assert_eq!(editor.view.viewport().width, 30 - 5);

        // Too narrow to spare the columns: no numbers.
        editor.set_area(0, 0, 20, 6);
        editor.handle_key(Key::new(KeyCode::Home, Mods::CTRL));
        let (lines, cursor) = screen_lines(&editor, 20, 6);
        assert_eq!(lines[0], "one", "{lines:?}");
        assert_eq!(cursor, (0, 0));
    }

    #[test]
    fn select_in_line_selects_bytes_and_scrolls_to_them() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        // A tab and a wide character before the match, which is in bytes.
        lines[30] = "\t日x = needle;".to_string();
        eb.set_text(&lines.join("\n"));
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 10).unwrap();
        let start = lines[30].find("needle").unwrap();
        editor.select_in_line(30, start..start + 6);
        assert_eq!(editor.selected_text().as_deref(), Some("needle"));
        assert_eq!(eb.cursor().row, 30);
        let (_, cursor) = screen_lines(&editor, 40, 10);
        let top = editor.view.viewport().y;
        assert_eq!(top, 30 - 9 / 3, "a third of the way down");
        assert_eq!(cursor.1, 30 - top);

        // An empty range puts the cursor there; past the end, at the end.
        editor.select_in_line(2, 5..5);
        assert_eq!(editor.selected_text(), None);
        assert_eq!((eb.cursor().row, eb.cursor().col), (2, 5));
        editor.select_in_line(99, 50..60);
        assert_eq!(eb.cursor().row, 39);
        assert_eq!(editor.selected_text(), None);
    }

    #[test]
    fn select_in_line_scrolls_a_third_down_past_wrapped_lines() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let lines: Vec<String> = (0..40)
            .map(|i| format!("{i} {}", "word ".repeat(20)))
            .collect();
        eb.set_text(&lines.join("\n"));
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 10).unwrap();
        let _ = screen_lines(&editor, 40, 10);
        editor.select_in_line(30, 0..2);
        assert_eq!(editor.selected_text().as_deref(), Some("30"));
        let (lines, cursor) = screen_lines(&editor, 40, 10);
        assert_eq!(cursor.1, 9 / 3, "a third of the way down: {lines:?}");
        assert!(
            lines[cursor.1 as usize].contains("31  30 word"),
            "{lines:?}"
        );

        // Already on screen: the view stays put.
        let top = editor.view.viewport().y;
        editor.select_in_line(31, 0..2);
        let _ = screen_lines(&editor, 40, 10);
        assert_eq!(editor.view.viewport().y, top);

        // Above the view, it scrolls back up: the selection keeps the view
        // from following the cursor there.
        editor.select_in_line(5, 0..1);
        let (lines, cursor) = screen_lines(&editor, 40, 10);
        assert_eq!(cursor.1, 9 / 3, "a third of the way down: {lines:?}");
        assert!(lines[cursor.1 as usize].contains("6  5 word"), "{lines:?}");
    }

    #[test]
    fn wrapping_drops_the_sideways_scroll_and_keeps_the_top_line() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let long = "abcdefghij ".repeat(20);
        let mut lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        for line in &mut lines[..5] {
            *line = long.clone();
        }
        lines[30] = long.clone();
        eb.set_text(&lines.join("\n"));
        eb.set_cursor(30, 0);
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 16).unwrap();
        editor.set_wrap(WrapMode::None);
        // End goes to the end of a line wider than the view.
        key(&mut editor, KeyCode::End);
        assert_eq!(eb.cursor().col, long.len() as u32);
        key(&mut editor, KeyCode::Home);
        eb.set_cursor(30, 150);
        let _ = screen_lines(&editor, 40, 16);
        let vp = editor.view.viewport();
        assert!(vp.x > 0, "scrolled sideways to the cursor: {vp:?}");
        editor.view.scroll_to(vp.x, 26, false);
        let (lines, _) = screen_lines(&editor, 40, 16);
        assert_eq!(lines[0], " 27", "{lines:?}");

        editor.toggle_wrap();
        let (lines, cursor) = screen_lines(&editor, 40, 16);
        assert_eq!(editor.view.viewport().x, 0);
        assert_eq!(lines[0], " 27  line 26", "{lines:?}");
        assert!(lines[cursor.1 as usize].contains("abcdefghij"), "{lines:?}");

        editor.toggle_wrap();
        let (lines, _) = screen_lines(&editor, 40, 16);
        assert!(lines[0].starts_with(" 27"), "{lines:?}");
    }

    #[test]
    fn the_wheel_scrolls_past_long_wrapped_lines() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let long = "abcdefghij ".repeat(60);
        let mut lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        lines[10] = long.clone();
        lines[25] = long;
        eb.set_text(&lines.join("\n"));
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 10).unwrap();
        editor.set_wrap(WrapMode::Word);
        let _ = screen_lines(&editor, 40, 10);
        // Past the end, until the last line is a row from the top, where
        // the cursor dragged along stays clear of the scroll margin.
        let max_y = editor.view.total_virtual_line_count() - 2;
        let mut y = 0;
        while y < max_y {
            editor.scroll(0, 3);
            let _ = screen_lines(&editor, 40, 10);
            let next = editor.view.viewport().y;
            assert_eq!(next, (y + 3).min(max_y), "scrolling down from {y}");
            y = next;
        }
        let (lines, cursor) = screen_lines(&editor, 40, 10);
        assert_eq!(lines[1], " 40  line 39", "{lines:?}");
        assert_eq!(lines[2], "", "{lines:?}");
        assert_eq!(cursor.1, 1, "the cursor came along to the last line");
        // Unwrapped, and resized, it stays there.
        editor.toggle_wrap();
        editor.set_area(0, 0, 40, 12);
        let (lines, _) = screen_lines(&editor, 40, 12);
        assert_eq!(lines[1], " 40  line 39", "{lines:?}");
        editor.toggle_wrap();
        editor.set_area(0, 0, 40, 10);
        let _ = screen_lines(&editor, 40, 10);
        y = editor.view.viewport().y;
        while y > 0 {
            editor.scroll(0, -3);
            let _ = screen_lines(&editor, 40, 10);
            let next = editor.view.viewport().y;
            assert_eq!(next, y.saturating_sub(3), "scrolling up from {y}");
            y = next;
        }
    }

    /// The text of `row`, trimmed, and which of its columns have `bg`.
    fn row_with_bg(
        editor: &Editor,
        width: u32,
        height: u32,
        row: u32,
        bg: Rgba,
    ) -> (String, String) {
        let screen = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
        draw(editor, &screen);
        let text = screen
            .to_text(true)
            .lines()
            .nth(row as usize)
            .unwrap()
            .trim_end()
            .to_string();
        let marks = (0..width)
            .map(|x| {
                if screen.bg_at(x, row) == Some(bg) {
                    '#'
                } else {
                    ' '
                }
            })
            .collect::<String>()
            .trim_end()
            .to_string();
        (text, marks)
    }

    fn find_query(editor: &mut Editor, text: &str) {
        editor.show_find(&find::Memory::default(), false);
        editor.find_edit(Edit::Insert(text));
    }

    #[test]
    fn find_highlights_matches_and_selects_as_you_type() {
        let colors = theme::colors();
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        // A tab and a wide character before a match: highlights are in columns.
        eb.set_text("ab\tab 漢ab\nnone\nxab");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 100, 6).unwrap();
        eb.set_cursor(0, 1);
        find_query(&mut editor, "a");
        editor.find_edit(Edit::Insert("b"));
        assert_eq!(editor.selected_text().as_deref(), Some("ab"));
        assert_eq!(pos(&eb), (0, 6), "the first match after the cursor");
        let (text, current) = row_with_bg(&editor, 100, 6, 0, current_match().bg);
        assert!(text.starts_with(" 1  ab  ab 漢ab "), "{text}");
        assert_eq!(current, "        ##");
        let (_, others) = row_with_bg(&editor, 100, 6, 0, colors.match_bg);
        assert_eq!(others, "    ##       ##");
        // The bar floats at the top right, a column in from the edge.
        let bar = &text[text.find(" ▸ ").unwrap()..];
        assert!(bar.starts_with(" ▸  ab"), "{bar}");
        assert!(bar.ends_with("2 of 4  Aa  ab  .*  ×"), "{bar}");
        let (_, bar_cells) = row_with_bg(&editor, 100, 6, 0, colors.surface);
        assert_eq!(bar_cells.find('#'), Some(100 - 61));
        assert_eq!(bar_cells.len(), 99);

        let memory = find::Memory::default();
        editor.find_step(&memory, true);
        assert_eq!(pos(&eb), (0, 11));
        editor.find_step(&memory, true);
        assert_eq!(pos(&eb), (2, 3));
        editor.find_step(&memory, true);
        assert_eq!(pos(&eb), (0, 2), "wraps around");
        editor.find_step(&memory, false);
        assert_eq!(pos(&eb), (2, 3), "and back");

        // No match: nothing selected, the cursor back where finding started.
        editor.find_edit(Edit::Insert("zz"));
        assert_eq!(editor.selected_text(), None);
        assert_eq!(pos(&eb), (2, 1));
        assert!(row_with_bg(&editor, 100, 6, 0, colors.match_bg)
            .0
            .contains("no matches"));
        editor.find_edit(Edit::DeleteBackward);
        editor.find_edit(Edit::DeleteBackward);
        assert_eq!(pos(&eb), (2, 3));

        // Closing leaves the match selected, without highlights.
        editor.run(Command::FindClose, false, &mut None);
        assert!(!editor.find_open());
        assert_eq!(editor.selected_text().as_deref(), Some("ab"));
        let (text, others) = row_with_bg(&editor, 100, 6, 0, colors.match_bg);
        assert_eq!(others, "");
        assert!(!text.contains('▸'), "{text}");
    }

    #[test]
    fn find_matches_keep_the_color_of_highlighted_text() {
        let colors = theme::colors();
        const KEYWORD: Rgba = Rgba::rgb(200, 100, 250);
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("let a = let");
        let theme = theme();
        let keyword = theme.register("keyword", Some(KEYWORD), None);
        let mut editor = Editor::new(eb.clone(), unnamed(), theme, 100, 4).unwrap();
        eb.add_highlights(&[0, 8].map(|start| Highlight {
            line: 0,
            start,
            end: start + 3,
            style: keyword,
            priority: 0,
            tag: 2,
        }));
        // The first match is selected; the second is only highlighted.
        find_query(&mut editor, "et");
        let screen = OwnedBuffer::new(100, 4, false, WidthMethod::Unicode, "test").unwrap();
        draw(&editor, &screen);
        let matched: Vec<u32> = (0..100)
            .filter(|&x| screen.bg_at(x, 0) == Some(colors.match_bg))
            .collect();
        assert_eq!(matched.len(), 2, "{matched:?}");
        for x in matched {
            assert_eq!(screen.fg_at(x, 0), Some(KEYWORD), "column {x}");
        }
    }

    /// The color of the first `needle` on screen row `row`.
    fn fg_of(editor: &Editor, width: u32, height: u32, row: u32, needle: &str) -> Option<Rgba> {
        style_of(editor, width, height, row, needle).map(|(fg, _)| fg)
    }

    /// The color and attributes of the first `needle` on screen row `row`.
    fn style_of(
        editor: &Editor,
        width: u32,
        height: u32,
        row: u32,
        needle: &str,
    ) -> Option<(Rgba, Attributes)> {
        let screen = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
        draw(editor, &screen);
        let text = screen.to_text(true).lines().nth(row as usize)?.to_string();
        let x = text.find(needle)?;
        // Wide characters (CJK, in tests) take two cells but appear once.
        let col = text[..x]
            .chars()
            .map(|c| if c >= '\u{2e80}' { 2 } else { 1 })
            .sum();
        Some((screen.fg_at(col, row)?, screen.attributes_at(col, row)?))
    }

    fn rust_file() -> File {
        File {
            path: Some(PathBuf::from("main.rs")),
            line_ending: LineEnding::Lf,
        }
    }

    const KEYWORD: Rgba = Rgba::indexed(5);
    const FUNCTION: Rgba = Rgba::indexed(4);
    /// Comments are the text's color, dimmed.
    /// Comments, as Terminal colors them: the text's color, dimmed.
    fn comment() -> (Rgba, Attributes) {
        (theme::colors().text, Attributes::DIM)
    }
    const STRING: Rgba = Rgba::indexed(2);

    #[test]
    fn changing_language_recolors_existing_text() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("fn main() {}");
        let editor = Editor::new(eb, unnamed(), theme(), 40, 6).unwrap();
        assert_eq!(fg_of(&editor, 40, 6, 0, "fn"), Some(theme::colors().text));
        let rust = crate::language::all().find(|language| language.name == "Rust");
        editor.document().set_language(rust);
        assert_eq!(fg_of(&editor, 40, 6, 0, "fn"), Some(KEYWORD));
        editor.document().set_language(None);
        assert_eq!(fg_of(&editor, 40, 6, 0, "fn"), Some(theme::colors().text));
    }

    #[test]
    fn highlights_syntax_as_the_text_changes() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("fn main() {}\n// done");
        let mut editor = Editor::new(eb.clone(), rust_file(), theme(), 40, 6).unwrap();
        assert_eq!(fg_of(&editor, 40, 6, 0, "fn"), Some(KEYWORD));
        assert_eq!(fg_of(&editor, 40, 6, 0, "main"), Some(FUNCTION));
        assert_eq!(style_of(&editor, 40, 6, 1, "done"), Some(comment()));
        // Punctuation isn't colored: it's the terminal's own text color.
        assert_eq!(fg_of(&editor, 40, 6, 0, "()"), Some(theme::colors().text));

        // A new line above: the colors move down with their text.
        eb.set_cursor(0, 0);
        key(&mut editor, KeyCode::Enter);
        assert_eq!(fg_of(&editor, 40, 6, 1, "fn"), Some(KEYWORD));
        assert_eq!(style_of(&editor, 40, 6, 2, "done"), Some(comment()));

        // Typing changes what the text is, and undoing changes it back.
        eb.set_cursor(1, 0);
        press(&mut editor, "//");
        assert_eq!(style_of(&editor, 40, 6, 1, "main"), Some(comment()));
        ctrl(&mut editor, 'z');
        assert_eq!(fg_of(&editor, 40, 6, 1, "main"), Some(FUNCTION));
        eb.set_cursor(1, 0);
        press(&mut editor, "let s = \"");
        eb.set_cursor(1, 99);
        press(&mut editor, "\";");
        assert_eq!(fg_of(&editor, 40, 6, 1, "main"), Some(STRING));

        // Unnamed buffers stay plain.
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("fn main() {}");
        let editor = Editor::new(eb, unnamed(), theme(), 40, 6).unwrap();
        assert_eq!(fg_of(&editor, 40, 6, 0, "fn"), Some(theme::colors().text));
    }

    #[test]
    fn highlights_across_lines_and_after_tabs() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("/* one\ntwo */ fn a() {}\n\t漢 fn b() {}");
        let editor = Editor::new(eb.clone(), rust_file(), theme(), 40, 6).unwrap();
        assert_eq!(style_of(&editor, 40, 6, 0, "one"), Some(comment()));
        assert_eq!(style_of(&editor, 40, 6, 1, "two"), Some(comment()));
        assert_eq!(fg_of(&editor, 40, 6, 1, "fn"), Some(KEYWORD));
        assert_eq!(fg_of(&editor, 40, 6, 1, "a()"), Some(FUNCTION));
        // Columns, not bytes: the tab and wide character come before.
        assert_eq!(fg_of(&editor, 40, 6, 2, "fn"), Some(KEYWORD));
        assert_eq!(fg_of(&editor, 40, 6, 2, "b()"), Some(FUNCTION));
    }

    #[test]
    fn highlights_where_the_view_scrolls_to() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let text: String = (0..300).map(|i| format!("fn f{i}() {{}}\n")).collect();
        eb.set_text(&text);
        let editor = Editor::new(eb.clone(), rust_file(), theme(), 40, 11).unwrap();
        // Every row on screen, however far down, and back up.
        for line in [0, 290, 150, 0] {
            eb.set_cursor(line, 0);
            for row in 0..10 {
                assert_eq!(
                    fg_of(&editor, 40, 11, row, "fn"),
                    Some(KEYWORD),
                    "row {row}"
                );
            }
            let name = format!("f{line}()");
            let shown = (0..10).find_map(|row| fg_of(&editor, 40, 11, row, &name));
            assert_eq!(shown, Some(FUNCTION), "{name}");
        }
    }

    #[test]
    fn find_follows_edits_and_undo() {
        let colors = theme::colors();
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("one x\ntwo x");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 6).unwrap();
        find_query(&mut editor, "x");
        editor.blur_find();
        let count = |editor: &Editor| editor.find.as_ref().unwrap().matches.len();
        assert_eq!(count(&editor), 2);
        // Typing in the text adds a line before the matches.
        key(&mut editor, KeyCode::Esc);
        eb.set_cursor(0, 0);
        key(&mut editor, KeyCode::Enter);
        press(&mut editor, "x");
        assert_eq!(count(&editor), 3);
        let (_, marks) = row_with_bg(&editor, 40, 6, 2, colors.match_bg);
        assert_eq!(marks, "        #", "highlights moved down with their line");
        ctrl(&mut editor, 'z');
        ctrl(&mut editor, 'z');
        assert_eq!(count(&editor), 2);
        let (text, marks) = row_with_bg(&editor, 40, 6, 1, colors.match_bg);
        assert_eq!((text.as_str(), marks.as_str()), (" 2  two x", "        #"));
        // Esc with nothing selected closes the bar.
        key(&mut editor, KeyCode::Esc);
        assert!(!editor.find_open());
    }

    #[test]
    fn replace_one_at_a_time_and_all_at_once() {
        let colors = theme::colors();
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("a1 a22\nb3 a4");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 8).unwrap();
        let memory = find::Memory {
            query: crate::search::Query {
                text: r"a(\d+)".to_string(),
                regex: true,
                ..Default::default()
            },
            replacement: "<$1>".to_string(),
        };
        editor.show_find(&memory, true);
        assert_eq!(editor.find_field(), Some(Field::Replace));
        let mut clipboard = None;
        // No current match yet: Replace selects the first.
        editor.run(Command::Replace, false, &mut clipboard);
        assert_eq!(editor.selected_text().as_deref(), Some("a1"));
        editor.run(Command::Replace, false, &mut clipboard);
        assert_eq!(eb.text(), "<1> a22\nb3 a4");
        assert_eq!(editor.selected_text().as_deref(), Some("a22"), "the next");
        let (bar, _) = row_with_bg(&editor, 40, 8, 0, colors.match_bg);
        assert!(bar.contains("1 of 2"), "{bar}");
        let (replace_row, _) = row_with_bg(&editor, 40, 8, 1, colors.match_bg);
        assert!(
            replace_row.contains("<$1>") && replace_row.contains(" all"),
            "{replace_row}"
        );

        editor.run(Command::ReplaceAll, false, &mut clipboard);
        assert_eq!(eb.text(), "<1> <22>\nb3 <4>");
        assert_eq!(status(&editor), " Replaced 2 matches.");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "<1> a22\nb3 a4", "one undo step");
        ctrl(&mut editor, 'z');
        assert_eq!(eb.text(), "a1 a22\nb3 a4");

        // A replacement that contains the query isn't matched again.
        let memory = find::Memory {
            query: crate::search::Query {
                text: "a".to_string(),
                ..Default::default()
            },
            replacement: "aa".to_string(),
        };
        editor.close_find();
        editor.show_find(&memory, true);
        editor.run(Command::ReplaceAll, false, &mut clipboard);
        assert_eq!(eb.text(), "aa1 aa22\nb3 aa4");
        eb.set_cursor(0, 0);
        editor.run(Command::Replace, false, &mut clipboard);
        editor.run(Command::Replace, false, &mut clipboard);
        assert_eq!(eb.text(), "aaa1 aa22\nb3 aa4");
    }

    #[test]
    fn the_selection_seeds_the_query_and_the_shortcut_toggles_the_bar() {
        let colors = theme::colors();
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("foo bar foo");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 6).unwrap();
        let memory = find::Memory {
            query: crate::search::Query {
                text: "old".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        editor.show_find(&memory, false);
        assert_eq!(editor.find_memory().unwrap().query.text, "old");
        assert_eq!(
            editor.selected_text(),
            None,
            "opening doesn't move the cursor"
        );
        editor.find_edit(Edit::Insert("f"));
        assert_eq!(
            editor.find_memory().unwrap().query.text,
            "f",
            "typing replaced it"
        );
        editor.show_find(&memory, false);
        assert!(!editor.find_open(), "pressed again, it closes");

        editor.select_in_line(0, 8..11);
        editor.show_find(&memory, false);
        assert_eq!(editor.find_memory().unwrap().query.text, "foo");
        let (bar, _) = row_with_bg(&editor, 40, 6, 0, colors.match_bg);
        assert!(bar.contains("2 of 2"), "{bar}");

        // An invalid regex says why in the status bar.
        let mut clipboard = None;
        editor.run(Command::SearchToggleRegex, false, &mut clipboard);
        editor.find_edit(Edit::Insert("("));
        assert!(
            status(&editor).starts_with(" Invalid regex: "),
            "{}",
            status(&editor)
        );
        assert!(row_with_bg(&editor, 40, 6, 0, colors.match_bg)
            .0
            .contains("invalid regex"));
    }

    #[test]
    fn clicks_on_the_find_bar() {
        let colors = theme::colors();
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("Foo foo");
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 6).unwrap();
        find_query(&mut editor, "foo");
        assert_eq!(editor.find.as_ref().unwrap().matches.len(), 2);
        let now = Instant::now();
        let click = |editor: &mut Editor, x, y| {
            editor.handle_mouse(
                Mouse {
                    kind: MouseKind::Press(MouseButton::Left),
                    x,
                    y,
                    mods: Mods::NONE,
                },
                now,
            );
        };
        // Clicks land on what was drawn.
        let (row, _) = row_with_bg(&editor, 40, 6, 0, colors.match_bg);
        let column = |row: &str, label: &str| {
            let byte = row.find(label).unwrap();
            row[..byte].chars().count() as u32
        };
        click(&mut editor, column(&row, "Aa"), 0);
        assert!(editor.find_memory().unwrap().query.case_sensitive);
        assert_eq!(editor.find.as_ref().unwrap().matches.len(), 1);
        // A click in the text gives it the keyboard; one on the query takes it back.
        click(&mut editor, GUTTER, 1);
        assert_eq!(editor.find_field(), None);
        click(&mut editor, column(&row, "foo"), 0);
        assert_eq!(editor.find_field(), Some(Field::Find));

        // The expander shows the replacement; its buttons replace.
        click(&mut editor, column(&row, "▸"), 0);
        assert_eq!(editor.find_field(), Some(Field::Replace));
        editor.find_edit(Edit::Insert("bar"));
        let (replace_row, _) = row_with_bg(&editor, 40, 6, 1, colors.match_bg);
        click(&mut editor, column(&replace_row, "all"), 1);
        assert_eq!(eb.text(), "Foo bar");
        click(&mut editor, column(&row, "×"), 0);
        assert!(!editor.find_open());
    }

    #[test]
    fn a_match_under_the_bar_scrolls_into_view() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        lines[20] = format!("{}needle", " ".repeat(70));
        eb.set_text(&lines.join("\n"));
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 100, 10).unwrap();
        // Scrolled so line 20 is at the top, where the bar will be.
        editor.view.scroll_to(0, 20, true);
        screen_lines(&editor, 100, 10);
        assert_eq!(editor.view.viewport().y, 20);
        find_query(&mut editor, "needle");
        let (_, cursor) = screen_lines(&editor, 100, 10);
        assert_eq!(editor.view.viewport().y, 19, "one row down, below the bar");
        assert_eq!(cursor.1, 0, "the cursor is in the query");
    }

    #[test]
    fn stepping_to_a_wrapped_match_above_the_view_scrolls_up_to_it() {
        let _serial = serial();
        let eb = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        let mut lines: Vec<String> = (0..40)
            .map(|i| format!("{i} {}", "word ".repeat(20)))
            .collect();
        lines[5] = format!("5 needle {}", "word ".repeat(20));
        lines[30] = format!("30 needle {}", "word ".repeat(20));
        eb.set_text(&lines.join("\n"));
        eb.set_cursor(20, 0);
        let mut editor = Editor::new(eb.clone(), unnamed(), theme(), 40, 10).unwrap();
        find_query(&mut editor, "needle");
        assert_eq!(eb.cursor().row, 30);
        let _ = screen_lines(&editor, 40, 10);
        // From one match to the next, wrapping around to above the view.
        editor.find_step(&find::Memory::default(), true);
        assert_eq!(eb.cursor().row, 5);
        // A third of the way down; the query keeps the cursor.
        let (lines, _) = screen_lines(&editor, 40, 10);
        assert!(lines[9 / 3].contains("6  5 needle"), "{lines:?}");
    }
}
