use std::marker::PhantomData;

use opentui_sys as sys;

use crate::color::opt_ptr;
use crate::thread::Claim;
use crate::{ffi_len, read_native_string, Error, Result, Rgba, WidthMethod, WrapMode};

/// How a selection made from viewport cells snaps (`SelectionBehavior`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionBehavior {
    #[default]
    Cell,
    /// Whole words, as for a double click.
    Word,
    /// Whole lines, as for a triple click.
    Line,
}

/// Selection highlight colors. `fg` of `None` keeps the text's own color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionColors {
    pub bg: Rgba,
    pub fg: Option<Rgba>,
}

/// An [`EditorView`]'s visible region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

fn local_selection_flags(move_cursor: bool, behavior: SelectionBehavior) -> u8 {
    let behavior = match behavior {
        SelectionBehavior::Cell => 0,
        SelectionBehavior::Word => 1,
        SelectionBehavior::Line => 2,
    };
    // bit 0: update cursor, bit 1: follow cursor, bits 2+: behavior
    u8::from(move_cursor) | behavior << 2
}

/// A cursor position in the underlying text: `row` is the line and `col` the
/// display column within it. `offset` is in the native cursor-offset units
/// accepted by [`EditorView::set_cursor_by_offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalCursor {
    pub row: u32,
    pub col: u32,
    pub offset: u32,
}

/// A cursor position after wrapping, relative to the view's viewport, plus
/// the logical position it corresponds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualCursor {
    pub row: u32,
    pub col: u32,
    pub logical_row: u32,
    pub logical_col: u32,
    pub offset: u32,
}

impl From<sys::ExternalVisualCursor> for VisualCursor {
    fn from(c: sys::ExternalVisualCursor) -> Self {
        VisualCursor {
            row: c.visual_row,
            col: c.visual_col,
            logical_row: c.logical_row,
            logical_col: c.logical_col,
            offset: c.offset,
        }
    }
}

/// Editable text with a cursor and undo history (`EditBuffer`).
///
/// Like [`TextBuffer`](crate::TextBuffer), mutation takes `&self` so the text
/// can change while [`EditorView`]s borrow it.
///
/// # Undo history
///
/// The native buffer snapshots the text before every edit; [`undo`](Self::undo)
/// and [`redo`](Self::redo) step one snapshot at a time. Edits return how many
/// snapshots they recorded, so callers can group several edits (a typed word,
/// a replaced selection) into one user-visible undo step. The count is not
/// always 1: nothing is recorded for a no-op, and a forward delete records an
/// extra snapshot natively.
pub struct EditBuffer {
    handle: sys::Handle,
    _claim: Claim,
}

impl EditBuffer {
    pub fn new(width_method: WidthMethod) -> Result<EditBuffer> {
        let claim = Claim::acquire()?;
        let handle = unsafe { sys::createEditBuffer(width_method as u8, sys::INVALID_HANDLE) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("edit buffer"));
        }
        Ok(EditBuffer {
            handle,
            _claim: claim,
        })
    }

    /// Replaces the whole text, resets the cursor, and clears the undo history.
    pub fn set_text(&self, text: &str) {
        unsafe { sys::editBufferSetText(self.handle, text.as_ptr(), ffi_len(text.len(), "text")) }
    }

    pub fn text(&self) -> String {
        let text_buffer = unsafe { sys::editBufferGetTextBuffer(self.handle) };
        let size = unsafe { sys::textBufferGetByteSize(text_buffer) } as usize;
        if size == 0 {
            return String::new();
        }
        read_native_string(size + 1, |out| unsafe {
            sys::editBufferGetText(self.handle, out.as_mut_ptr(), ffi_len(out.len(), "output"))
        })
    }

    pub fn line_count(&self) -> u32 {
        unsafe { sys::textBufferGetLineCount(sys::editBufferGetTextBuffer(self.handle)) }
    }

    /// Inserts at the cursor and moves the cursor past the inserted text.
    /// Returns the undo snapshots recorded.
    pub fn insert_text(&self, text: &str) -> u32 {
        unsafe {
            sys::editBufferInsertText(self.handle, text.as_ptr(), ffi_len(text.len(), "text"))
        }
        u32::from(!text.is_empty())
    }

    /// Splits the line at the cursor. Returns the undo snapshots recorded.
    pub fn new_line(&self) -> u32 {
        unsafe { sys::editBufferNewLine(self.handle) }
        1
    }

    /// Backspace: deletes the grapheme before the cursor, joining lines at
    /// the start of a line. Returns the undo snapshots recorded.
    pub fn delete_char_backward(&self) -> u32 {
        let before = self.byte_size();
        unsafe { sys::editBufferDeleteCharBackward(self.handle) }
        // Natively a snapshot is taken only when a range is actually deleted.
        u32::from(self.byte_size() != before)
    }

    /// Delete: deletes the grapheme after the cursor, joining lines at the end
    /// of a line. Returns the undo snapshots recorded.
    pub fn delete_char(&self) -> u32 {
        let before = self.byte_size();
        unsafe { sys::editBufferDeleteChar(self.handle) }
        // `deleteForward` snapshots unconditionally, then `deleteRange`
        // snapshots again when something is deleted.
        1 + u32::from(self.byte_size() != before)
    }

    /// Restores the text and cursor from before the last snapshot. Returns
    /// false when there is nothing to undo.
    pub fn undo(&self) -> bool {
        if !self.can_undo() {
            return false;
        }
        // The output is cursor metadata that the buffer has already applied.
        let mut meta = [0u8; 64];
        unsafe { sys::editBufferUndo(self.handle, meta.as_mut_ptr(), meta.len() as u32) };
        true
    }

    /// Reapplies the last undone snapshot. Returns false when there is
    /// nothing to redo.
    pub fn redo(&self) -> bool {
        if !self.can_redo() {
            return false;
        }
        let mut meta = [0u8; 64];
        unsafe { sys::editBufferRedo(self.handle, meta.as_mut_ptr(), meta.len() as u32) };
        true
    }

    pub fn can_undo(&self) -> bool {
        unsafe { sys::editBufferCanUndo(self.handle) }
    }

    pub fn can_redo(&self) -> bool {
        unsafe { sys::editBufferCanRedo(self.handle) }
    }

    pub fn clear_history(&self) {
        unsafe { sys::editBufferClearHistory(self.handle) }
    }

    fn byte_size(&self) -> u32 {
        unsafe { sys::textBufferGetByteSize(sys::editBufferGetTextBuffer(self.handle)) }
    }

    pub fn move_cursor_left(&self) {
        unsafe { sys::editBufferMoveCursorLeft(self.handle) }
    }

    pub fn move_cursor_right(&self) {
        unsafe { sys::editBufferMoveCursorRight(self.handle) }
    }

    /// Moves by logical line. Use [`EditorView::move_up_visual`] to move by
    /// wrapped line.
    pub fn move_cursor_up(&self) {
        unsafe { sys::editBufferMoveCursorUp(self.handle) }
    }

    pub fn move_cursor_down(&self) {
        unsafe { sys::editBufferMoveCursorDown(self.handle) }
    }

    pub fn cursor(&self) -> LogicalCursor {
        let mut out = sys::ExternalLogicalCursor {
            row: 0,
            col: 0,
            offset: 0,
        };
        unsafe { sys::editBufferGetCursorPosition(self.handle, &mut out) };
        LogicalCursor {
            row: out.row,
            col: out.col,
            offset: out.offset,
        }
    }

    /// Places the cursor at a native offset (see [`LogicalCursor::offset`]).
    pub fn set_cursor_by_offset(&self, offset: u32) {
        unsafe { sys::editBufferSetCursorByOffset(self.handle, offset) }
    }

    /// The offset of a line and display column. Offsets count display
    /// columns, plus one for each line break before the position.
    pub fn position_to_offset(&self, row: u32, col: u32) -> u32 {
        unsafe { sys::editBufferPositionToOffset(self.handle, row, col) }
    }

    /// The line and display column of an offset, if it is inside the text.
    pub fn offset_to_position(&self, offset: u32) -> Option<LogicalCursor> {
        let mut out = sys::ExternalLogicalCursor {
            row: 0,
            col: 0,
            offset: 0,
        };
        let ok = unsafe { sys::editBufferOffsetToPosition(self.handle, offset, &mut out) };
        ok.then_some(LogicalCursor {
            row: out.row,
            col: out.col,
            offset: out.offset,
        })
    }

    /// The text between two offsets.
    pub fn text_range(&self, start: u32, end: u32) -> String {
        let (start, end) = (start.min(end), start.max(end));
        if start == end {
            return String::new();
        }
        // Offsets count columns; a column holds at most one grapheme, which
        // rarely exceeds 16 bytes.
        read_native_string((end - start) as usize * 16 + 1, |out| unsafe {
            sys::editBufferGetTextRange(
                self.handle,
                start,
                end,
                out.as_mut_ptr(),
                ffi_len(out.len(), "output"),
            )
        })
    }

    /// Deletes between two (row, col) positions and leaves the cursor at the
    /// start. Returns the undo snapshots recorded.
    pub fn delete_range(&self, start: (u32, u32), end: (u32, u32)) -> u32 {
        unsafe { sys::editBufferDeleteRange(self.handle, start.0, start.1, end.0, end.1) }
        u32::from(start != end)
    }

    /// Places the cursor at a line and display column, clamped to the text.
    pub fn set_cursor(&self, row: u32, col: u32) {
        unsafe { sys::editBufferSetCursorToLineCol(self.handle, row, col) }
    }

    pub fn set_tab_width(&self, width: u8) {
        unsafe { sys::editBufferSetTabWidth(self.handle, width) }
    }

    /// A new view for laying out and drawing this buffer.
    pub fn view(&self, width: u32, height: u32) -> Result<EditorView<'_>> {
        let handle = unsafe { sys::createEditorView(self.handle, width, height) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("editor view"));
        }
        Ok(EditorView {
            handle,
            edit_buffer: self.handle,
            _buffer: PhantomData,
        })
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for EditBuffer {
    fn drop(&mut self) {
        unsafe { sys::destroyEditBuffer(self.handle) }
    }
}

/// A scrolling, optionally wrapping viewport onto an [`EditBuffer`]
/// (`EditorView`). It keeps the cursor visible, honoring the scroll margin.
/// Draw it with [`Buffer::draw_editor_view`](crate::Buffer::draw_editor_view).
pub struct EditorView<'eb> {
    handle: sys::Handle,
    edit_buffer: sys::Handle,
    // An owned child of the edit buffer natively; the borrow keeps it from
    // outliving it.
    _buffer: PhantomData<&'eb EditBuffer>,
}

impl EditorView<'_> {
    pub fn set_viewport_size(&self, width: u32, height: u32) {
        unsafe { sys::editorViewSetViewportSize(self.handle, width, height) }
    }

    pub fn set_wrap_mode(&self, mode: WrapMode) {
        unsafe { sys::editorViewSetWrapMode(self.handle, mode as u8) }
    }

    /// Fraction of the viewport (0.0–0.5) kept between the cursor and the
    /// viewport's edges when scrolling.
    pub fn set_scroll_margin(&self, margin: f32) {
        unsafe { sys::editorViewSetScrollMargin(self.handle, margin) }
    }

    /// The cursor relative to the viewport, after scrolling it into view.
    pub fn visual_cursor(&self) -> VisualCursor {
        self.query(sys::editorViewGetVisualCursor)
    }

    /// Moves up one wrapped line, keeping the visual column where possible.
    pub fn move_up_visual(&self) {
        unsafe { sys::editorViewMoveUpVisual(self.handle) }
    }

    pub fn move_down_visual(&self) {
        unsafe { sys::editorViewMoveDownVisual(self.handle) }
    }

    /// Moves to the start of the current wrapped line.
    pub fn move_to_visual_line_start(&self) {
        let sol = self.query(sys::editorViewGetVisualSOL);
        self.set_cursor_by_offset(sol.offset);
    }

    /// Moves to the end of the current wrapped line.
    pub fn move_to_visual_line_end(&self) {
        let eol = self.query(sys::editorViewGetVisualEOL);
        self.set_cursor_by_offset(eol.offset);
    }

    pub fn set_cursor_by_offset(&self, offset: u32) {
        unsafe { sys::editorViewSetCursorByOffset(self.handle, offset) }
    }

    /// Visible lines after wrapping.
    pub fn virtual_line_count(&self) -> u32 {
        unsafe { sys::editorViewGetVirtualLineCount(self.handle) }
    }

    /// The selected range as `(start, end)` offsets, if any.
    pub fn selection(&self) -> Option<(u32, u32)> {
        let packed = unsafe { sys::editorViewGetSelection(self.handle) };
        (packed != u64::MAX).then_some(((packed >> 32) as u32, packed as u32))
    }

    /// Selects `start..end` (cursor offsets; either order).
    pub fn set_selection(&self, start: u32, end: u32, colors: SelectionColors) {
        self.reset_local_selection();
        let (start, end) = (start.min(end), start.max(end));
        unsafe {
            sys::editorViewSetSelection(
                self.handle,
                start,
                end,
                colors.bg.as_ptr(),
                opt_ptr(&colors.fg),
            )
        }
    }

    /// Starts a selection from viewport cells, as for a mouse press. With
    /// `move_cursor`, the cursor moves to `focus`. Returns whether anything
    /// changed.
    pub fn set_local_selection(
        &self,
        anchor: (i32, i32),
        focus: (i32, i32),
        behavior: SelectionBehavior,
        move_cursor: bool,
        colors: SelectionColors,
    ) -> bool {
        unsafe {
            sys::editorViewSetLocalSelection(
                self.handle,
                anchor.0,
                anchor.1,
                focus.0,
                focus.1,
                colors.bg.as_ptr(),
                opt_ptr(&colors.fg),
                local_selection_flags(move_cursor, behavior),
            )
        }
    }

    /// Moves the focus of a selection started with
    /// [`set_local_selection`](Self::set_local_selection), as for a mouse drag.
    pub fn update_local_selection(
        &self,
        anchor: (i32, i32),
        focus: (i32, i32),
        behavior: SelectionBehavior,
        move_cursor: bool,
        colors: SelectionColors,
    ) -> bool {
        unsafe {
            sys::editorViewUpdateLocalSelection(
                self.handle,
                anchor.0,
                anchor.1,
                focus.0,
                focus.1,
                colors.bg.as_ptr(),
                opt_ptr(&colors.fg),
                local_selection_flags(move_cursor, behavior),
            )
        }
    }

    fn reset_local_selection(&self) {
        unsafe { sys::editorViewResetLocalSelection(self.handle) }
    }

    /// Removes any selection. The text is unchanged.
    pub fn clear_selection(&self) {
        self.reset_local_selection();
        unsafe { sys::editorViewResetSelection(self.handle) }
    }

    pub fn selected_text(&self) -> String {
        if self.selection().is_none() {
            return String::new();
        }
        let size =
            unsafe { sys::textBufferGetByteSize(sys::editBufferGetTextBuffer(self.edit_buffer)) };
        read_native_string(size as usize + 1, |out| unsafe {
            sys::editorViewGetSelectedTextBytes(
                self.handle,
                out.as_mut_ptr(),
                ffi_len(out.len(), "output"),
            )
        })
    }

    /// Deletes the selected text, leaving the cursor at its start. Returns the
    /// undo snapshots recorded.
    pub fn delete_selected_text(&self) -> u32 {
        let has_range = matches!(self.selection(), Some((start, end)) if start != end);
        unsafe { sys::editorViewDeleteSelectedText(self.handle) }
        u32::from(has_range)
    }

    /// The visible region, in wrapped lines and columns.
    pub fn viewport(&self) -> Viewport {
        let (mut x, mut y, mut width, mut height) = (0, 0, 0, 0);
        unsafe { sys::editorViewGetViewport(self.handle, &mut x, &mut y, &mut width, &mut height) };
        Viewport {
            x,
            y,
            width,
            height,
        }
    }

    /// Scrolls to `x`, `y`. With `move_cursor`, the cursor is moved into the
    /// new viewport; otherwise the next layout scrolls back to the cursor.
    pub fn scroll_to(&self, x: u32, y: u32, move_cursor: bool) {
        let vp = self.viewport();
        unsafe { sys::editorViewSetViewport(self.handle, x, y, vp.width, vp.height, move_cursor) }
    }

    /// Wrapped lines in the whole document.
    pub fn total_virtual_line_count(&self) -> u32 {
        unsafe { sys::editorViewGetTotalVirtualLineCount(self.handle) }
    }

    fn query(
        &self,
        f: unsafe extern "C" fn(sys::Handle, *mut sys::ExternalVisualCursor),
    ) -> VisualCursor {
        let mut out = sys::ExternalVisualCursor {
            visual_row: 0,
            visual_col: 0,
            logical_row: 0,
            logical_col: 0,
            offset: 0,
        };
        unsafe { f(self.handle, &mut out) };
        out.into()
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for EditorView<'_> {
    fn drop(&mut self) {
        unsafe { sys::destroyEditorView(self.handle) }
    }
}
