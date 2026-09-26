use opentui_sys as sys;

use crate::color::opt_ptr;
use crate::thread::Claim;
use crate::{ffi_len, Attributes, Error, Result, Rgba};

/// Named styles that highlights refer to by id (`SyntaxStyle`). Attach one
/// to an [`EditBuffer`](crate::EditBuffer) with
/// [`set_syntax_style`](crate::EditBuffer::set_syntax_style).
pub struct SyntaxStyle {
    handle: sys::Handle,
    _claim: Claim,
}

impl SyntaxStyle {
    pub fn new() -> Result<SyntaxStyle> {
        let claim = Claim::acquire()?;
        let handle = unsafe { sys::createSyntaxStyle() };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("syntax style"));
        }
        Ok(SyntaxStyle {
            handle,
            _claim: claim,
        })
    }

    /// Defines the style `name`, or redefines it, and returns its id. Colors
    /// left `None` keep the text's own.
    pub fn register(
        &self,
        name: &str,
        fg: Option<Rgba>,
        bg: Option<Rgba>,
        attributes: Attributes,
    ) -> u32 {
        unsafe {
            sys::syntaxStyleRegister(
                self.handle,
                name.as_ptr(),
                ffi_len(name.len(), "style name"),
                opt_ptr(&fg),
                opt_ptr(&bg),
                attributes.0,
            )
        }
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for SyntaxStyle {
    fn drop(&mut self) {
        unsafe { sys::destroySyntaxStyle(self.handle) }
    }
}
