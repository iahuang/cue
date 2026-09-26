use std::marker::PhantomData;
use std::ops::Deref;

use opentui_sys as sys;

use crate::color::opt_ptr;
use crate::edit::EditorView;
use crate::renderer::Renderer;
use crate::text::{TextBufferView, WidthMethod};
use crate::thread::Claim;
use crate::{ffi_len, read_native_string, Attributes, Error, Result, Rgba};

/// A grid of styled cells (`OptimizedBuffer`).
///
/// This is only reachable through [`OwnedBuffer`] or [`FrameBuffer`]. Drawing
/// takes `&self`: the cells live in native memory, and the type is `!Sync`, so
/// mutation through a shared reference is sound here (like `Cell`).
pub struct Buffer {
    handle: sys::Handle,
    _not_send: PhantomData<*const ()>,
}

impl Buffer {
    pub(crate) fn from_handle(handle: sys::Handle) -> Buffer {
        Buffer {
            handle,
            _not_send: PhantomData,
        }
    }

    pub fn width(&self) -> u32 {
        unsafe { sys::getBufferWidth(self.handle) }
    }

    pub fn height(&self) -> u32 {
        unsafe { sys::getBufferHeight(self.handle) }
    }

    pub fn clear(&self, bg: Rgba) {
        unsafe { sys::bufferClear(self.handle, bg.as_ptr()) }
    }

    /// Draws `text` starting at cell (`x`, `y`), clipped to the buffer. `bg`
    /// of `None` keeps each cell's existing background.
    pub fn draw_text(
        &self,
        text: &str,
        x: u32,
        y: u32,
        fg: Rgba,
        bg: Option<Rgba>,
        attributes: Attributes,
    ) {
        let len = ffi_len(text.len(), "text");
        unsafe {
            sys::bufferDrawText(
                self.handle,
                text.as_ptr(),
                len,
                x,
                y,
                fg.as_ptr(),
                opt_ptr(&bg),
                attributes.0,
            )
        }
    }

    pub fn draw_char(&self, ch: char, x: u32, y: u32, fg: Rgba, bg: Rgba, attributes: Attributes) {
        unsafe {
            sys::bufferDrawChar(
                self.handle,
                ch as u32,
                x,
                y,
                fg.as_ptr(),
                bg.as_ptr(),
                attributes.0,
            )
        }
    }

    pub fn fill_rect(&self, x: u32, y: u32, width: u32, height: u32, bg: Rgba) {
        unsafe { sys::bufferFillRect(self.handle, x, y, width, height, bg.as_ptr()) }
    }

    /// Draws a text buffer view with its top-left cell at (`x`, `y`).
    pub fn draw_text_buffer_view(&self, view: &TextBufferView<'_>, x: i32, y: i32) {
        unsafe { sys::bufferDrawTextBufferView(self.handle, view.raw_handle(), x, y) }
    }

    /// Draws an editor view's visible lines with the top-left cell at (`x`, `y`).
    pub fn draw_editor_view(&self, view: &EditorView<'_>, x: i32, y: i32) {
        unsafe { sys::bufferDrawEditorView(self.handle, view.raw_handle(), x, y) }
    }

    /// Runs `draw` with drawing clipped to the `width` x `height` rectangle
    /// at (`x`, `y`): nothing drawn meanwhile lands outside it.
    pub fn with_clip<R>(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        draw: impl FnOnce() -> R,
    ) -> R {
        unsafe { sys::bufferPushScissorRect(self.handle, x as i32, y as i32, width, height) };
        // Pop even if `draw` panics, so the clip can't outlive this call.
        struct Pop(sys::Handle);
        impl Drop for Pop {
            fn drop(&mut self) {
                unsafe { sys::bufferPopScissorRect(self.0) }
            }
        }
        let _pop = Pop(self.handle);
        draw()
    }

    /// The buffer's characters as text, one row per line if `line_breaks`.
    pub fn to_text(&self, line_breaks: bool) -> String {
        let cells = self.width() as usize * self.height() as usize;
        read_native_string(cells * 4 + self.height() as usize, |out| unsafe {
            sys::bufferWriteResolvedChars(
                self.handle,
                out.as_mut_ptr(),
                ffi_len(out.len(), "output"),
                line_breaks,
            )
        })
    }

    /// The background of cell (`x`, `y`), or `None` outside the buffer.
    pub fn bg_at(&self, x: u32, y: u32) -> Option<Rgba> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let index = y as usize * self.width() as usize + x as usize;
        let bg = unsafe { sys::bufferGetBgPtr(self.handle) };
        (!bg.is_null()).then(|| Rgba(unsafe { *bg.add(index) }))
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

/// A standalone buffer, e.g. for offscreen composition.
pub struct OwnedBuffer {
    buffer: Buffer,
    _claim: Claim,
}

impl OwnedBuffer {
    /// `respect_alpha` makes drawing blend with the existing cell colors.
    /// `id` is a debugging label.
    pub fn new(
        width: u32,
        height: u32,
        respect_alpha: bool,
        width_method: WidthMethod,
        id: &str,
    ) -> Result<Self> {
        let claim = Claim::acquire()?;
        let handle = unsafe {
            sys::createOptimizedBuffer(
                width,
                height,
                respect_alpha as u8,
                width_method as u8,
                id.as_ptr(),
                ffi_len(id.len(), "buffer id"),
            )
        };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("buffer"));
        }
        Ok(OwnedBuffer {
            buffer: Buffer::from_handle(handle),
            _claim: claim,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        unsafe { sys::bufferResize(self.buffer.handle, width, height) }
    }
}

impl Deref for OwnedBuffer {
    type Target = Buffer;
    fn deref(&self) -> &Buffer {
        &self.buffer
    }
}

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        unsafe { sys::destroyOptimizedBuffer(self.buffer.handle) }
    }
}

/// A renderer's back buffer, borrowed until the next [`Renderer::render`].
pub struct FrameBuffer<'r> {
    buffer: Buffer,
    _renderer: PhantomData<&'r mut Renderer>,
}

impl FrameBuffer<'_> {
    pub(crate) fn new(buffer: Buffer) -> Self {
        FrameBuffer {
            buffer,
            _renderer: PhantomData,
        }
    }
}

impl Deref for FrameBuffer<'_> {
    type Target = Buffer;
    fn deref(&self) -> &Buffer {
        &self.buffer
    }
}
