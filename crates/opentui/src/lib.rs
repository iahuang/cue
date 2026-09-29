//! Safe Rust bindings to the [OpenTUI](https://github.com/sst/opentui) Zig core.
//!
//! This wraps the native rendering layer: a double-buffered cell renderer that
//! diffs frames into minimal terminal output, standalone cell buffers, and
//! rope-backed text buffers with wrapping views, images, and a terminal
//! emulator to embed. OpenTUI's component tree,
//! layout glue, and input parsing live in its TypeScript layer and are not
//! part of this crate.
//!
//! # Threading
//!
//! The native handle registry is unsynchronized, so every OpenTUI object must
//! be used from a single thread. All types here are `!Send + !Sync`, and
//! constructors fail with [`Error::WrongThread`] if objects are still alive on
//! another thread.
//!
//! # Example
//!
//! ```no_run
//! use opentui::{Attributes, Output, Renderer, Rgba};
//!
//! let mut renderer = Renderer::new(80, 24, Output::Stdout)?;
//! renderer.setup_terminal(true);
//! {
//!     let frame = renderer.next_buffer()?;
//!     frame.clear(Rgba::BLACK);
//!     frame.draw_text("hello", 2, 1, Rgba::WHITE, None, Attributes::BOLD);
//! }
//! renderer.render(false);
//! # Ok::<(), opentui::Error>(())
//! ```

mod buffer;
mod color;
mod edit;
mod error;
mod image;
mod renderer;
mod style;
mod terminal;
mod text;
mod thread;
mod tty;

pub use buffer::{Buffer, FrameBuffer, OwnedBuffer};
pub use color::{Attributes, Rgba};
pub use edit::{
    EditBuffer, EditorView, Highlight, LogicalCursor, SelectionBehavior, SelectionColors, Viewport,
    VisibleLine, VisualCursor,
};
pub use error::{Error, Result};
pub use image::Image;
pub use renderer::{Output, RenderStatus, Renderer};
pub use style::SyntaxStyle;
pub use terminal::{
    Cursor, CursorStyle, EmbeddedTerminal, KeyAction, KeyEvent, KeyMods, MouseAction, MouseButton,
    MouseEvent,
};
pub use text::{TextBuffer, TextBufferView, WidthMethod, WrapMode};

/// Raw bindings, for functionality this crate does not wrap yet.
pub use opentui_sys as sys;

/// Converts a Rust length to the `u32` the C ABI uses.
fn ffi_len(len: usize, what: &str) -> u32 {
    u32::try_from(len)
        .unwrap_or_else(|_| panic!("{what} is {len} bytes; OpenTUI accepts at most u32::MAX"))
}

/// Reads a native string through a copy-into-caller-buffer function, growing
/// the buffer until the result fits. `write` returns the number of bytes
/// written, and 0 when the buffer is too small.
fn read_native_string(initial: usize, mut write: impl FnMut(&mut [u8]) -> u32) -> String {
    let mut capacity = initial.max(64);
    loop {
        let mut buf = vec![0u8; capacity];
        let written = write(&mut buf) as usize;
        // A buffer that is not full is complete; a zero result or a full one may be truncated.
        if (written > 0 && written < capacity) || capacity >= (u32::MAX as usize) {
            buf.truncate(written);
            return String::from_utf8_lossy(&buf).into_owned();
        }
        if written == 0 && capacity >= initial.max(64) * 16 {
            return String::new();
        }
        capacity = capacity.saturating_mul(2).min(u32::MAX as usize);
    }
}
