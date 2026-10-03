use std::marker::PhantomData;

use opentui_sys as sys;

use crate::thread::Claim;
use crate::{ffi_len, Error, Result};

/// A decoded image (PNG, JPEG, WebP, or GIF's first frame), to draw with
/// [`Buffer::draw_image`](crate::Buffer::draw_image).
///
/// The renderer keeps its own reference to an image it has drawn, so
/// dropping this while it's still on screen is fine.
pub struct Image {
    handle: sys::Handle,
    info: sys::ImageInfo,
    _claim: Claim,
    _not_send: PhantomData<*const ()>,
}

impl Image {
    /// Decodes an encoded image, oriented as its EXIF data says.
    pub fn decode(bytes: &[u8]) -> Result<Image> {
        let claim = Claim::acquire()?;
        if bytes.is_empty() {
            return Err(Error::Image(status_reason(7)));
        }
        let mut handle = sys::INVALID_HANDLE;
        let len = ffi_len(bytes.len(), "image");
        let status = unsafe { sys::imageDecode(bytes.as_ptr(), len, &mut handle) };
        if status != 0 || handle == sys::INVALID_HANDLE {
            return Err(Error::Image(status_reason(status)));
        }
        let mut info = std::mem::MaybeUninit::<sys::ImageInfo>::zeroed();
        let status = unsafe { sys::imageGetInfo(handle, info.as_mut_ptr()) };
        if status != 0 {
            unsafe { sys::imageDestroy(handle) };
            return Err(Error::Image(status_reason(status)));
        }
        Ok(Image {
            handle,
            info: unsafe { info.assume_init() },
            _claim: claim,
            _not_send: PhantomData,
        })
    }

    /// An image of `width` x `height` pixels, given as rows of RGBA bytes
    /// (straight alpha), top first.
    pub fn from_rgba(width: u32, height: u32, pixels: &[u8]) -> Result<Image> {
        let claim = Claim::acquire()?;
        let mut handle = sys::INVALID_HANDLE;
        let status = unsafe {
            sys::imageCreateFromRgba(
                pixels.as_ptr(),
                pixels.len() as u64,
                width,
                height,
                width * 4,
                &mut handle,
            )
        };
        if status != 0 || handle == sys::INVALID_HANDLE {
            return Err(Error::Image(status_reason(status)));
        }
        let mut info = std::mem::MaybeUninit::<sys::ImageInfo>::zeroed();
        let status = unsafe { sys::imageGetInfo(handle, info.as_mut_ptr()) };
        if status != 0 {
            unsafe { sys::imageDestroy(handle) };
            return Err(Error::Image(status_reason(status)));
        }
        Ok(Image {
            handle,
            info: unsafe { info.assume_init() },
            _claim: claim,
            _not_send: PhantomData,
        })
    }

    /// Width in pixels, after orientation.
    pub fn width(&self) -> u32 {
        self.info.width
    }

    /// Height in pixels, after orientation.
    pub fn height(&self) -> u32 {
        self.info.height
    }

    /// The encoded format's name, such as "PNG", or `None` if unknown.
    pub fn format(&self) -> Option<&'static str> {
        match self.info.format {
            1 => Some("PNG"),
            3 => Some("JPEG"),
            4 => Some("WebP"),
            5 => Some("GIF"),
            _ => None,
        }
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        unsafe { sys::imageDestroy(self.handle) }
    }
}

/// Why decoding failed, from the native `image.Status`.
fn status_reason(status: u32) -> &'static str {
    match status {
        2 => "unsupported image format",
        3 => "unsupported color space",
        4 => "malformed image",
        5 => "image too large",
        6 | 8 => "out of memory",
        7 => "empty image",
        11 => "unsupported image feature",
        _ => "image decoding failed",
    }
}
