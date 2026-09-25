use std::marker::PhantomData;

use opentui_sys as sys;

use crate::thread::Claim;
use crate::{ffi_len, read_native_string, Error, Result, WidthMethod, WrapMode};

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

    /// Replaces the whole text and resets the cursor.
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
    pub fn insert_text(&self, text: &str) {
        unsafe {
            sys::editBufferInsertText(self.handle, text.as_ptr(), ffi_len(text.len(), "text"))
        }
    }

    /// Splits the line at the cursor.
    pub fn new_line(&self) {
        unsafe { sys::editBufferNewLine(self.handle) }
    }

    /// Backspace: deletes the grapheme before the cursor, joining lines at
    /// the start of a line.
    pub fn delete_char_backward(&self) {
        unsafe { sys::editBufferDeleteCharBackward(self.handle) }
    }

    /// Delete: deletes the grapheme after the cursor, joining lines at the end
    /// of a line.
    pub fn delete_char(&self) {
        unsafe { sys::editBufferDeleteChar(self.handle) }
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
