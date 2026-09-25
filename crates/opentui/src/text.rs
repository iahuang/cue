use std::marker::PhantomData;

use opentui_sys as sys;

use crate::color::opt_ptr;
use crate::thread::Claim;
use crate::{ffi_len, read_native_string, Attributes, Error, Result, Rgba};

/// How display width is computed for text (`utf8.WidthMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum WidthMethod {
    Wcwidth = 0,
    #[default]
    Unicode = 1,
    NoZwj = 2,
    UnicodeWide = 3,
}

/// Line wrapping for a [`TextBufferView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum WrapMode {
    #[default]
    None = 0,
    Char = 1,
    Word = 2,
}

/// Rope-backed text storage (`UnifiedTextBuffer`).
///
/// Mutation takes `&self` so text can change while [`TextBufferView`]s borrow
/// the buffer; views pick up changes on their next layout.
pub struct TextBuffer {
    handle: sys::Handle,
    _claim: Claim,
}

impl TextBuffer {
    pub fn new(width_method: WidthMethod) -> Result<TextBuffer> {
        let claim = Claim::acquire()?;
        let handle = unsafe { sys::createTextBuffer(width_method as u8) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("text buffer"));
        }
        Ok(TextBuffer {
            handle,
            _claim: claim,
        })
    }

    pub fn append(&self, text: &str) {
        unsafe { sys::textBufferAppend(self.handle, text.as_ptr(), ffi_len(text.len(), "text")) }
    }

    /// Replaces the text, keeping highlights and default styles.
    pub fn set_text(&self, text: &str) {
        unsafe { sys::textBufferClear(self.handle) };
        self.append(text);
    }

    pub fn text(&self) -> String {
        let size = unsafe { sys::textBufferGetByteSize(self.handle) } as usize;
        if size == 0 {
            return String::new();
        }
        // +1 so a complete result never fills the buffer exactly.
        read_native_string(size + 1, |out| unsafe {
            sys::textBufferGetPlainText(self.handle, out.as_mut_ptr(), ffi_len(out.len(), "output"))
        })
    }

    pub fn line_count(&self) -> u32 {
        unsafe { sys::textBufferGetLineCount(self.handle) }
    }

    /// Total display width in terminal columns, summed over all lines.
    pub fn width_cols(&self) -> u32 {
        unsafe { sys::textBufferGetLength(self.handle) }
    }

    pub fn set_default_fg(&self, color: Option<Rgba>) {
        unsafe { sys::textBufferSetDefaultFg(self.handle, opt_ptr(&color)) }
    }

    pub fn set_default_bg(&self, color: Option<Rgba>) {
        unsafe { sys::textBufferSetDefaultBg(self.handle, opt_ptr(&color)) }
    }

    pub fn set_default_attributes(&self, attributes: Option<Attributes>) {
        let raw = attributes.map(|a| a.0);
        let ptr = raw.as_ref().map_or(std::ptr::null(), |a| a as *const u32);
        unsafe { sys::textBufferSetDefaultAttributes(self.handle, ptr) }
    }

    /// A new view for laying out and drawing this buffer.
    pub fn view(&self) -> Result<TextBufferView<'_>> {
        let handle = unsafe { sys::createTextBufferView(self.handle) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("text buffer view"));
        }
        Ok(TextBufferView {
            handle,
            _buffer: PhantomData,
        })
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for TextBuffer {
    fn drop(&mut self) {
        unsafe { sys::destroyTextBuffer(self.handle) }
    }
}

/// Layout state over a [`TextBuffer`]: wrapping, viewport, and selection
/// (`UnifiedTextBufferView`). Draw it with [`Buffer::draw_text_buffer_view`](crate::Buffer::draw_text_buffer_view).
pub struct TextBufferView<'tb> {
    handle: sys::Handle,
    // Natively the view is an owned child of the buffer; the borrow keeps it
    // from outliving it.
    _buffer: PhantomData<&'tb TextBuffer>,
}

impl TextBufferView<'_> {
    /// `None` disables wrapping regardless of the wrap mode.
    pub fn set_wrap_width(&self, width: Option<u32>) {
        unsafe { sys::textBufferViewSetWrapWidth(self.handle, width.unwrap_or(0)) }
    }

    pub fn set_wrap_mode(&self, mode: WrapMode) {
        unsafe { sys::textBufferViewSetWrapMode(self.handle, mode as u8) }
    }

    pub fn set_viewport_size(&self, width: u32, height: u32) {
        unsafe { sys::textBufferViewSetViewportSize(self.handle, width, height) }
    }

    /// Lines after wrapping.
    pub fn virtual_line_count(&self) -> u32 {
        unsafe { sys::textBufferViewGetVirtualLineCount(self.handle) }
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for TextBufferView<'_> {
    fn drop(&mut self) {
        unsafe { sys::destroyTextBufferView(self.handle) }
    }
}
