use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

use opentui_sys as sys;

use crate::color::opt_ptr;
use crate::thread::Claim;
use crate::{ffi_len, read_native_string, Error, Result, Rgba, SyntaxStyle, WidthMethod, WrapMode};

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

/// Which cells a selection made from viewport cells covers
/// (`SelectionOccupancy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionOccupancy {
    /// Both end cells, for a block cursor.
    #[default]
    Cell,
    /// From the boundary before one end cell to the boundary before the
    /// other, for a bar cursor.
    Boundary,
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

/// A row of an [`EditorView`]'s viewport after wrapping: the text line it
/// shows, and which wrapped segment of that line (0 for its first row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleLine {
    pub line: u32,
    pub wrap: u32,
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

/// Where `old` and `new` differ, in bytes: the start, the same in both,
/// then the end in each, after which they're the same again. The ends are
/// next to line breaks, so never inside a grapheme: the start is a line's,
/// and the text after the ends starts a line in both or with a line break.
fn changed_lines(old: &str, new: &str) -> (usize, usize, usize) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let start = a[..prefix]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let most = a.len().min(b.len()) - start;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(most)
        .take_while(|(x, y)| x == y)
        .count();
    let starts_line = |s: &[u8], end: usize| end == 0 || s[end - 1] == b'\n';
    let suffix = if starts_line(a, a.len() - suffix) && starts_line(b, b.len() - suffix) {
        suffix
    } else {
        // From the first line break in it, if any.
        let tail = &a[a.len() - suffix..];
        tail.iter()
            .position(|&c| c == b'\n')
            .map_or(0, |i| suffix - i)
    };
    (start, a.len() - suffix, b.len() - suffix)
}

#[cfg(test)]
mod tests {
    use super::changed_lines;

    #[test]
    fn changed_lines_are_whole_lines_or_line_breaks() {
        // Appending a line only inserts it.
        assert_eq!(changed_lines("a\nb\n", "a\nb\nc\n"), (4, 4, 6));
        // Removing or adding a line in the middle only touches that line.
        assert_eq!(changed_lines("a\nb\nc\n", "a\nc\n"), (2, 4, 2));
        assert_eq!(changed_lines("a\nc\n", "a\nb\nc\n"), (2, 2, 4));
        // A change within a line replaces the line up to its break.
        assert_eq!(changed_lines("a\nbxb\nc", "a\nbyb\nc"), (2, 5, 5));
        // Never inside a grapheme: "e" plus an accent differs from "e".
        assert_eq!(changed_lines("xe\u{301}", "xe"), (0, 4, 2));
        assert_eq!(changed_lines("same", "same"), (0, 0, 0));
        assert_eq!(changed_lines("", "new"), (0, 0, 3));
        assert_eq!(changed_lines("old", ""), (0, 3, 0));
    }
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

/// A styled range of one line, in display columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    pub line: u32,
    pub start: u32,
    pub end: u32,
    /// A style id from the buffer's [`SyntaxStyle`].
    pub style: u32,
    /// Where highlights overlap, the higher priority wins.
    pub priority: u8,
    /// Groups highlights for [`EditBuffer::remove_highlights`].
    pub tag: u16,
}

impl From<sys::ExternalLogicalCursor> for LogicalCursor {
    fn from(c: sys::ExternalLogicalCursor) -> Self {
        LogicalCursor {
            row: c.row,
            col: c.col,
            offset: c.offset,
        }
    }
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
///
/// # Highlights
///
/// Highlights style ranges of lines with a [`SyntaxStyle`]'s styles. They
/// belong to lines by index and don't move with edits, so callers redo them
/// when the text changes (see [`content_epoch`](Self::content_epoch)).
pub struct EditBuffer {
    handle: sys::Handle,
    /// Kept alive while the native buffer points at it.
    style: RefCell<Option<Rc<SyntaxStyle>>>,
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
            style: RefCell::new(None),
            _claim: claim,
        })
    }

    /// Replaces the whole text, resets the cursor, and clears the undo history.
    pub fn set_text(&self, text: &str) {
        unsafe { sys::editBufferSetText(self.handle, text.as_ptr(), ffi_len(text.len(), "text")) }
    }

    /// Replaces the whole text as one undoable edit, moving the cursor to
    /// the start. Returns the undo snapshots recorded: none if it failed.
    ///
    /// Each call keeps a copy of the text for undo in one of the buffer's
    /// 255 memory slots, which are never freed; after about 250 calls it
    /// fails. [`replace_changed_lines`](Self::replace_changed_lines) doesn't.
    pub fn replace_text(&self, text: &str) -> u32 {
        let replaced = unsafe {
            sys::editBufferReplaceText(self.handle, text.as_ptr(), ffi_len(text.len(), "text"))
        };
        u32::from(replaced)
    }

    /// Replaces the text with `text` by replacing only the lines that
    /// differ, as a deletion then an insertion, so that undo keeps just the
    /// change. Leaves the cursor after the insertion. Returns the undo
    /// snapshots recorded.
    pub fn replace_changed_lines(&self, text: &str) -> u32 {
        let old = self.text();
        let (start, old_end, new_end) = changed_lines(&old, text);
        if start == old_end && start == new_end {
            return 0;
        }
        let ends = self.bytes_to_cursors(&[start as u32, old_end as u32]);
        let (from, to) = (ends[0], ends[1]);
        let mut steps = self.delete_range((from.row, from.col), (to.row, to.col));
        self.set_cursor(from.row, from.col);
        steps += self.insert_text(&text[start..new_end]);
        steps
    }

    /// A number that changes whenever the text does.
    pub fn content_epoch(&self) -> u64 {
        unsafe { sys::editBufferGetContentEpoch(self.handle) }
    }

    /// The positions of byte offsets into [`text`](Self::text), which must
    /// be in increasing order. An offset inside a grapheme counts the whole
    /// grapheme; offsets past the end are the end of the text.
    pub fn bytes_to_cursors(&self, bytes: &[u32]) -> Vec<LogicalCursor> {
        assert!(bytes.is_sorted(), "byte offsets must be in order");
        let mut out = vec![
            sys::ExternalLogicalCursor {
                row: 0,
                col: 0,
                offset: 0,
            };
            bytes.len()
        ];
        unsafe {
            sys::editBufferBytesToCursors(
                self.handle,
                bytes.as_ptr(),
                ffi_len(bytes.len(), "offsets"),
                out.as_mut_ptr(),
            )
        }
        out.into_iter().map(LogicalCursor::from).collect()
    }

    /// The color of text no highlight colors. `None` is white.
    pub fn set_default_fg(&self, color: Option<Rgba>) {
        unsafe { sys::textBufferSetDefaultFg(self.text_buffer(), opt_ptr(&color)) }
    }

    /// Styles highlights with `style`'s styles, or with none.
    pub fn set_syntax_style(&self, style: Option<Rc<SyntaxStyle>>) {
        let handle = style
            .as_ref()
            .map_or(sys::INVALID_HANDLE, |style| style.raw_handle());
        unsafe { sys::textBufferSetSyntaxStyle(self.text_buffer(), handle) };
        *self.style.borrow_mut() = style;
    }

    /// Adds highlights. Those outside the text are ignored.
    pub fn add_highlights(&self, highlights: &[Highlight]) {
        let text_buffer = self.text_buffer();
        unsafe { sys::textBufferStartHighlightsTransaction(text_buffer) };
        for h in highlights {
            let external = sys::ExternalHighlight {
                start: h.start,
                end: h.end,
                style_id: h.style,
                priority: h.priority,
                hl_ref: h.tag,
            };
            unsafe { sys::textBufferAddHighlight(text_buffer, h.line, &external) };
        }
        unsafe { sys::textBufferEndHighlightsTransaction(text_buffer) };
    }

    /// Removes the highlights tagged `tag`.
    pub fn remove_highlights(&self, tag: u16) {
        unsafe { sys::textBufferRemoveHighlightsByRef(self.text_buffer(), tag) }
    }

    /// Replaces the highlights tagged `tag` with `highlights`, which should
    /// be tagged `tag` too. A line both lose and gain highlights on is
    /// restyled once.
    pub fn replace_highlights(&self, tag: u16, highlights: &[Highlight]) {
        let text_buffer = self.text_buffer();
        unsafe { sys::textBufferStartHighlightsTransaction(text_buffer) };
        self.remove_highlights(tag);
        self.add_highlights(highlights);
        unsafe { sys::textBufferEndHighlightsTransaction(text_buffer) };
    }

    fn text_buffer(&self) -> sys::Handle {
        unsafe { sys::editBufferGetTextBuffer(self.handle) }
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
        Ok(EditorView {
            handle: self.create_view(width, height)?,
            edit_buffer: self.handle,
            _owner: None,
            _buffer: PhantomData,
        })
    }

    /// Like [`view`](Self::view), but the view holds a reference to the
    /// buffer instead of borrowing it, so the two can be stored together.
    pub fn shared_view(self: &Rc<Self>, width: u32, height: u32) -> Result<EditorView<'static>> {
        Ok(EditorView {
            handle: self.create_view(width, height)?,
            edit_buffer: self.handle,
            _owner: Some(Rc::clone(self)),
            _buffer: PhantomData,
        })
    }

    fn create_view(&self, width: u32, height: u32) -> Result<sys::Handle> {
        let handle = unsafe { sys::createEditorView(self.handle, width, height) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("editor view"));
        }
        Ok(handle)
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
    // outliving it, or for a shared view, the reference does. `Drop`
    // destroys the view before the reference is released.
    _owner: Option<Rc<EditBuffer>>,
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

    /// Makes this the one view of its buffer that scrolls to keep the
    /// cursor in sight. The buffer's other views stay where they are as the
    /// cursor moves. Until a view takes it, every view follows the cursor.
    pub fn take_cursor(&self) {
        unsafe { sys::editorViewTakeCursor(self.handle) }
    }

    /// The cursor relative to the viewport, after scrolling it into view.
    pub fn visual_cursor(&self) -> VisualCursor {
        self.query(sys::editorViewGetVisualCursor)
    }

    /// The cursor relative to the whole document rather than the viewport,
    /// without scrolling it into view.
    pub fn visual_cursor_absolute(&self) -> VisualCursor {
        self.query(sys::editorViewGetVisualCursorAbsolute)
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

    /// Which cells selections from viewport cells cover, and where the
    /// cursor can stop at a soft wrap: with [`Boundary`], at the end of the
    /// wrapped row too.
    ///
    /// [`Boundary`]: SelectionOccupancy::Boundary
    pub fn set_selection_occupancy(&self, occupancy: SelectionOccupancy) {
        let occupancy = match occupancy {
            SelectionOccupancy::Cell => 0,
            SelectionOccupancy::Boundary => 1,
        };
        unsafe { sys::editorViewSetSelectionOccupancy(self.handle, occupancy) }
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

    /// Scrolls to `x`, `y`, leaving the cursor where it is, out of view if
    /// need be. The view stays put until the cursor moves.
    pub fn scroll_away_from_cursor(&self, x: u32, y: u32) {
        unsafe { sys::editorViewScrollAwayFromCursor(self.handle, x, y) }
    }

    /// Whether it scrolled away from the cursor (see
    /// [`EditorView::scroll_away_from_cursor`]), which hasn't moved since.
    pub fn cursor_left_behind(&self) -> bool {
        unsafe { sys::editorViewIsCursorLeftBehind(self.handle) }
    }

    /// The rows in the viewport, top to bottom, after scrolling the cursor
    /// into view.
    pub fn visible_lines(&self) -> Vec<VisibleLine> {
        let mut info = sys::ExternalLineInfo {
            start_cols_ptr: std::ptr::null(),
            start_cols_len: 0,
            width_cols_ptr: std::ptr::null(),
            width_cols_len: 0,
            sources_ptr: std::ptr::null(),
            sources_len: 0,
            wraps_ptr: std::ptr::null(),
            wraps_len: 0,
            width_cols_max: 0,
        };
        unsafe { sys::editorViewGetLineInfoDirect(self.handle, &mut info) };
        let len = info.sources_len.min(info.wraps_len) as usize;
        if len == 0 {
            return Vec::new();
        }
        // The arrays belong to the view's layout cache, which stays put until
        // the next layout; copy them out now.
        let (sources, wraps) = unsafe {
            (
                std::slice::from_raw_parts(info.sources_ptr, len),
                std::slice::from_raw_parts(info.wraps_ptr, len),
            )
        };
        sources
            .iter()
            .zip(wraps)
            .map(|(&line, &wrap)| VisibleLine { line, wrap })
            .collect()
    }

    /// The first wrapped row, counting from the top of the document, of
    /// logical line `line`; past the end, the row count.
    pub fn first_row_of_line(&self, line: u32) -> u32 {
        let mut info = sys::ExternalLineInfo {
            start_cols_ptr: std::ptr::null(),
            start_cols_len: 0,
            width_cols_ptr: std::ptr::null(),
            width_cols_len: 0,
            sources_ptr: std::ptr::null(),
            sources_len: 0,
            wraps_ptr: std::ptr::null(),
            wraps_len: 0,
            width_cols_max: 0,
        };
        unsafe { sys::editorViewGetLogicalLineInfoDirect(self.handle, &mut info) };
        let len = info.sources_len as usize;
        if len == 0 {
            return 0;
        }
        // Each row's logical line, in order, for the whole document.
        let sources = unsafe { std::slice::from_raw_parts(info.sources_ptr, len) };
        sources.partition_point(|&source| source < line) as u32
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
