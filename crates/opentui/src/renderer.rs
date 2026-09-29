use std::ptr;

use opentui_sys as sys;

use crate::buffer::{Buffer, FrameBuffer};
use crate::thread::Claim;
use crate::tty::{self, RawMode};
use crate::{ffi_len, Error, Result, Rgba};

/// Where a [`Renderer`] writes its terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The process's stdout.
    Stdout,
    /// Discarded after rendering. Useful for tests and headless rendering,
    /// where frames are inspected through [`Buffer`] instead.
    Memory,
}

/// The result of [`Renderer::render`] (`renderer.RenderStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStatus {
    Rendered,
    /// Nothing changed since the last frame.
    Skipped,
    Failed,
}

/// A double-buffered terminal renderer (`CliRenderer`).
///
/// Draw into [`next_buffer`](Renderer::next_buffer), then call
/// [`render`](Renderer::render) to diff it against the previous frame and write
/// the changes.
pub struct Renderer {
    handle: sys::Handle,
    raw_mode: Option<RawMode>,
    terminal_is_setup: bool,
    _claim: Claim,
}

impl Renderer {
    pub fn new(width: u32, height: u32, output: Output) -> Result<Renderer> {
        let claim = Claim::acquire()?;
        let kind = match output {
            Output::Stdout => 0,
            Output::Memory => 1,
        };
        // Remote mode 0 = auto-detect (SSH etc.).
        let handle = unsafe { sys::createRenderer(width, height, kind, 0, ptr::null_mut()) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("renderer"));
        }
        let renderer = Renderer {
            handle,
            raw_mode: None,
            terminal_is_setup: false,
            _claim: claim,
        };
        if output == Output::Stdout {
            renderer.forward_environment();
        }
        Ok(renderer)
    }

    /// Passes the process environment to the native terminal detection,
    /// which learns from `TERM`, `COLORTERM`, `TERM_PROGRAM`, and the like
    /// what the terminal supports. Without it, no palette colors are sent:
    /// every color goes out as RGB.
    fn forward_environment(&self) {
        for (key, value) in std::env::vars_os() {
            let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
                continue;
            };
            unsafe {
                sys::setTerminalEnvVar(
                    self.handle,
                    key.as_ptr(),
                    ffi_len(key.len(), "variable name"),
                    value.as_ptr(),
                    ffi_len(value.len(), "variable value"),
                );
            }
        }
    }

    /// Puts stdin in raw mode, switches terminal modes, and sends capability
    /// queries. Everything is restored when the renderer is dropped.
    ///
    /// The terminal answers the queries on stdin. Pass those replies to
    /// [`process_capability_response`](Renderer::process_capability_response),
    /// or call [`poll_terminal_responses`](Renderer::poll_terminal_responses) if
    /// nothing else reads stdin. Unread replies are discarded on drop.
    pub fn setup_terminal(&mut self, alternate_screen: bool) {
        if self.raw_mode.is_none() {
            self.raw_mode = RawMode::enable();
        }
        unsafe { sys::setupTerminal(self.handle, alternate_screen) }
        self.terminal_is_setup = true;
    }

    /// Feeds terminal replies (DECRPM, XTVERSION, OSC colors, cursor reports,
    /// ...) to the capability detector.
    pub fn process_capability_response(&mut self, bytes: &[u8]) {
        let len = ffi_len(bytes.len(), "response");
        unsafe { sys::processCapabilityResponse(self.handle, bytes.as_ptr(), len) }
    }

    /// Sends Kitty graphics images to the terminal as files it reads,
    /// instead of as base64 through the terminal, once a probe shows it
    /// can (it may take a moment, and never happens over SSH or in a
    /// multiplexer). Until then, and if it can't, they go through the
    /// terminal as before. Call after
    /// [`setup_terminal`](Renderer::setup_terminal), then pass the
    /// terminal's replies to
    /// [`process_kitty_image_reply`](Renderer::process_kitty_image_reply)
    /// and call
    /// [`poll_kitty_image_transport`](Renderer::poll_kitty_image_transport)
    /// now and then.
    pub fn use_kitty_image_files(&mut self) {
        // `kitty_transport.Mode.file`.
        unsafe { sys::setKittyImageTransport(self.handle, 2) };
    }

    /// Takes a terminal reply to a Kitty graphics file transfer or probe.
    /// Returns false if it's not one, for
    /// [`process_capability_response`](Renderer::process_capability_response)
    /// instead.
    pub fn process_kitty_image_reply(&mut self, bytes: &[u8]) -> bool {
        let len = ffi_len(bytes.len(), "response");
        unsafe { sys::processKittyImageReply(self.handle, bytes.as_ptr(), len) != 0 }
    }

    /// Gives up on Kitty graphics files the terminal hasn't answered for,
    /// going back to sending images through it. Returns true if images
    /// need drawing again, as the next render does.
    pub fn poll_kitty_image_transport(&mut self) -> bool {
        unsafe { sys::pollKittyImageTransport(self.handle) != 0 }
    }

    /// Reads everything buffered on stdin without blocking and feeds it to
    /// [`process_capability_response`](Renderer::process_capability_response).
    /// This consumes keyboard input too, so use it only when nothing else
    /// reads stdin. Returns the bytes read.
    pub fn poll_terminal_responses(&mut self) -> std::io::Result<Vec<u8>> {
        let mut input = Vec::new();
        if tty::read_available(&mut input)? > 0 {
            self.process_capability_response(&input);
        }
        Ok(input)
    }

    /// The back buffer to draw the next frame into.
    pub fn next_buffer(&mut self) -> Result<FrameBuffer<'_>> {
        let handle = unsafe { sys::getNextBuffer(self.handle) };
        if handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("frame buffer handle"));
        }
        Ok(FrameBuffer::new(Buffer::from_handle(handle)))
    }

    /// Diffs the back buffer against the last frame and writes the changes.
    /// With `force`, the whole frame is repainted.
    pub fn render(&mut self, force: bool) -> RenderStatus {
        match unsafe { sys::render(self.handle, force) } {
            0 => RenderStatus::Rendered,
            1 => RenderStatus::Skipped,
            _ => RenderStatus::Failed,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        unsafe { sys::resizeRenderer(self.handle, width, height) }
    }

    pub fn set_background_color(&mut self, color: Rgba) {
        unsafe { sys::setBackgroundColor(self.handle, color.as_ptr()) }
    }

    /// Positions the terminal cursor (1-based, like the native API).
    pub fn set_cursor_position(&mut self, x: i32, y: i32, visible: bool) {
        unsafe { sys::setCursorPosition(self.handle, x, y, visible) }
    }

    /// Enables mouse reporting (clicks, drags, and wheel, in SGR encoding).
    /// With `movement`, motion without a button pressed is reported too.
    /// Disabled again when the renderer is dropped.
    pub fn enable_mouse(&mut self, movement: bool) {
        unsafe { sys::enableMouse(self.handle, movement) }
    }

    pub fn disable_mouse(&mut self) {
        unsafe { sys::disableMouse(self.handle) }
    }

    /// Copies `text` to the system clipboard through the terminal (OSC 52).
    /// Terminals may ignore or refuse the request; there is no confirmation.
    pub fn copy_to_clipboard(&mut self, text: &str) -> bool {
        // Target 0 is the clipboard ("c").
        unsafe {
            sys::copyToClipboardOSC52(self.handle, 0, text.as_ptr(), ffi_len(text.len(), "text"))
        }
    }

    /// Renders on a native background thread instead of in `render`.
    pub fn set_use_thread(&mut self, use_thread: bool) {
        unsafe { sys::setUseThread(self.handle, use_thread) }
    }

    pub fn suspend(&mut self) {
        unsafe { sys::suspendRenderer(self.handle) }
    }

    pub fn resume(&mut self) {
        unsafe { sys::resumeRenderer(self.handle) }
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Restores terminal modes and invalidates the borrowed frame-buffer
        // handles. Flushing discards query replies that were never read, which
        // would otherwise reach the next program to read the terminal.
        unsafe { sys::destroyRenderer(self.handle, self.terminal_is_setup) }
        // After the native teardown, as in the TypeScript host.
        self.raw_mode = None;
    }
}
