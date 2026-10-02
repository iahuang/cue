//! A terminal emulator to embed in the screen (`EmbeddedTerminal`, backed by
//! Ghostty's VT core).
//!
//! It keeps a terminal's screen and scrollback from the output of a program,
//! draws them into a [`Buffer`], and turns keys, mouse events, and pastes
//! into the bytes that program expects, for the modes it has asked for. It
//! runs no process: the caller connects it to one, usually through a pty,
//! and passes [`drain_responses`](EmbeddedTerminal::drain_responses) back to
//! it (replies to queries such as the cursor position).

use std::marker::PhantomData;
use std::ops::{BitOr, BitOrAssign, Range};

use opentui_sys as sys;

use crate::buffer::Buffer;
use crate::color::Rgba;
use crate::thread::Claim;
use crate::{ffi_len, Error, Result};

/// Status codes from the native calls.
const OUT_OF_SPACE: i32 = -4;

/// Modifier keys, combinable with `|` (`input.KeyMods` in Ghostty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyMods(pub u16);

impl KeyMods {
    pub const NONE: KeyMods = KeyMods(0);
    pub const SHIFT: KeyMods = KeyMods(1 << 0);
    pub const CTRL: KeyMods = KeyMods(1 << 1);
    pub const ALT: KeyMods = KeyMods(1 << 2);
    pub const SUPER: KeyMods = KeyMods(1 << 3);

    pub const fn contains(self, other: KeyMods) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for KeyMods {
    type Output = KeyMods;
    fn bitor(self, rhs: KeyMods) -> KeyMods {
        KeyMods(self.0 | rhs.0)
    }
}

impl BitOrAssign for KeyMods {
    fn bitor_assign(&mut self, rhs: KeyMods) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Release = 0,
    Press = 1,
    Repeat = 2,
}

/// A key event to encode, as a keyboard reports it.
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent<'a> {
    pub action: KeyAction,
    /// The physical key, as a W3C `KeyboardEvent.code`: `KeyA`, `Digit1`,
    /// `Enter`, `ArrowUp`, `F5`. Unknown codes encode by `text` alone.
    pub code: &'a str,
    pub mods: KeyMods,
    /// The text the key typed, if any: `A` for Shift+A.
    pub text: &'a str,
    /// The key's character without Shift: `a` for Shift+A.
    pub unshifted: Option<char>,
}

impl<'a> KeyEvent<'a> {
    pub fn press(code: &'a str, mods: KeyMods, text: &'a str, unshifted: Option<char>) -> Self {
        KeyEvent {
            action: KeyAction::Press,
            code,
            mods,
            text,
            unshifted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press = 0,
    Release = 1,
    Motion = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    /// The wheel, up.
    Four = 4,
    /// The wheel, down.
    Five = 5,
    /// The wheel, left.
    Six = 6,
    /// The wheel, right.
    Seven = 7,
}

/// A mouse event at a cell of the terminal, 0-based.
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// `None` for motion with no button held.
    pub button: Option<MouseButton>,
    pub mods: KeyMods,
    pub x: u32,
    pub y: u32,
    /// Whether any button is held, for motion.
    pub any_button_pressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Bar,
    Block,
    Underline,
    HollowBlock,
}

/// Where the terminal's cursor is and how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The cell, 0-based, or `None` when it's scrolled out of view.
    pub position: Option<(u16, u16)>,
    /// Whether the program shows it.
    pub visible: bool,
    pub blinking: bool,
    pub style: CursorStyle,
}

/// What's at a cell of a terminal's screen (see
/// [`EmbeddedTerminal::line_at`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineAt {
    /// An OSC 8 hyperlink's URI.
    Link(String),
    /// The text of the cell's line, joined across soft wraps, and the byte
    /// in it where the cell's text starts, unless the cell is past it.
    Text { text: String, offset: Option<usize> },
}

/// Where the viewport is in the scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollPosition {
    /// The viewport's top row, counted from the top of the scrollback.
    pub top: u32,
    /// The rows of the scrollback and the screen, in all.
    pub total: u32,
}

/// Where a search match starts (see
/// [`EmbeddedTerminal::set_search_matches`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    /// Counted from the top of the scrollback, so it shifts as the oldest
    /// rows are dropped.
    pub row: u32,
    pub col: u16,
    /// The same for as long as the row is kept, to tell a match found
    /// before.
    pub anchor: Anchor,
}

/// A place in the scrollback that stays put as rows are added and
/// dropped (a page and a row in it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Anchor {
    page: u64,
    page_row: u16,
    col: u16,
}

/// A terminal's screen, scrollback, and modes (`EmbeddedTerminal`).
pub struct EmbeddedTerminal {
    handle: sys::Handle,
    _claim: Claim,
    _not_send: PhantomData<*const ()>,
}

impl EmbeddedTerminal {
    /// A blank terminal `cols` by `rows`, keeping up to `max_scrollback`
    /// bytes of history.
    pub fn new(cols: u16, rows: u16, max_scrollback: u32) -> Result<EmbeddedTerminal> {
        let claim = Claim::acquire()?;
        let mut handle = sys::INVALID_HANDLE;
        let status =
            unsafe { sys::createEmbeddedTerminal(cols, rows, max_scrollback, &mut handle) };
        if status != 0 || handle == sys::INVALID_HANDLE {
            return Err(Error::CreateFailed("embedded terminal"));
        }
        Ok(EmbeddedTerminal {
            handle,
            _claim: claim,
            _not_send: PhantomData,
        })
    }

    /// Feeds the program's output to the terminal.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let len = ffi_len(bytes.len(), "output");
        check(
            unsafe { sys::embeddedTerminalWrite(self.handle, bytes.as_ptr(), len) },
            "terminal write",
        )
    }

    /// Resizes the screen, reflowing its lines. Zero sizes are refused.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        check(
            unsafe { sys::embeddedTerminalResize(self.handle, cols, rows) },
            "terminal resize",
        )
    }

    /// Composes default and palette colors as the host terminal's own, so
    /// they follow its theme, rather than as RGB from the built-in palette.
    pub fn set_host_palette(&mut self, enabled: bool) {
        unsafe { sys::embeddedTerminalSetHostPalette(self.handle, enabled as u8) };
    }

    /// The colors the program starts with: its default text and background,
    /// and palette slots 0-15, or the built-in ones without `ansi`. It can
    /// change them itself, and asking for them tells it these.
    pub fn set_default_colors(&mut self, fg: Rgba, bg: Rgba, ansi: Option<&[Rgba; 16]>) {
        let ansi = ansi.map_or(std::ptr::null(), |colors| colors.as_ptr().cast::<u16>());
        unsafe {
            sys::embeddedTerminalSetDefaultColors(self.handle, fg.as_ptr(), bg.as_ptr(), ansi)
        };
    }

    /// Scrolls the view through the scrollback by `delta` rows, up if
    /// negative.
    pub fn scroll(&mut self, delta: i32) {
        unsafe { sys::embeddedTerminalScroll(self.handle, delta) };
    }

    /// Scrolls the view back to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        unsafe { sys::embeddedTerminalScrollToBottom(self.handle) };
    }

    /// Where the viewport is in the scrollback.
    pub fn scroll_position(&self) -> ScrollPosition {
        let (mut top, mut total) = (0, 0);
        unsafe { sys::embeddedTerminalGetViewport(self.handle, &mut top, &mut total) };
        ScrollPosition { top, total }
    }

    /// Scrolls the viewport's top to `row`, counted from the top of the
    /// scrollback, or as near as it goes.
    pub fn scroll_to_row(&mut self, row: u32) {
        unsafe { sys::embeddedTerminalScrollToRow(self.handle, row) };
    }

    /// The text of the screen and the scrollback, to search: a line per
    /// line of output, with rows the output wrapped onto joined, and
    /// blanks at the ends of lines left out. Matches found in it are for
    /// [`set_search_matches`](EmbeddedTerminal::set_search_matches) until
    /// the terminal next changes.
    pub fn search_text(&mut self) -> Result<String> {
        let len = unsafe { sys::embeddedTerminalBuildSearch(self.handle) };
        if len < 0 {
            return Err(Error::CallFailed("terminal search"));
        }
        let mut out = vec![0u8; len as usize];
        let copied = unsafe {
            sys::embeddedTerminalCopySearchText(
                self.handle,
                out.as_mut_ptr(),
                ffi_len(out.len(), "search text"),
            )
        };
        if copied != len {
            return Err(Error::CallFailed("terminal search"));
        }
        // The terminal writes what it can't encode as U+FFFD.
        String::from_utf8(out).map_err(|_| Error::CallFailed("terminal search"))
    }

    /// What a new terminal of the same size replays to look like this one.
    /// With `full`, as near as it can be had: the primary screen's text
    /// and scrollback, scrolled as far as it was, the alternate screen if
    /// it's in use, the modes, the cursor, and the keyboard modes, but not
    /// the palette. Without, only the primary screen's text and
    /// scrollback, ending with a line break, to write more below.
    pub fn snapshot(&mut self, full: bool) -> Result<Vec<u8>> {
        let len = unsafe { sys::embeddedTerminalBuildSnapshot(self.handle, full as u8) };
        if len < 0 {
            return Err(Error::CallFailed("terminal snapshot"));
        }
        let mut out = vec![0u8; len as usize];
        let copied = unsafe {
            sys::embeddedTerminalCopySnapshot(
                self.handle,
                out.as_mut_ptr(),
                ffi_len(out.len(), "snapshot"),
            )
        };
        if copied != len {
            return Err(Error::CallFailed("terminal snapshot"));
        }
        Ok(out)
    }

    /// Highlights the matches at byte `ranges` of the last
    /// [`search_text`](EmbeddedTerminal::search_text), which are in order
    /// and not empty, and returns where each starts. Fails if the terminal
    /// changed since the text was read. The highlights stay on the rows
    /// they were found on, and go with them.
    pub fn set_search_matches(&mut self, ranges: &[Range<usize>]) -> Result<Vec<Found>> {
        let flat: Vec<u32> = ranges
            .iter()
            .flat_map(|r| [r.start, r.end])
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .collect();
        let empty = sys::ExternalEmbeddedTerminalMatch {
            serial: 0,
            row: 0,
            page_y: 0,
            x: 0,
        };
        let mut out = vec![empty; ranges.len()];
        check(
            unsafe {
                sys::embeddedTerminalSetSearchMatches(
                    self.handle,
                    flat.as_ptr(),
                    ffi_len(ranges.len(), "search matches"),
                    out.as_mut_ptr(),
                )
            },
            "terminal search matches",
        )?;
        Ok(out
            .into_iter()
            .map(|m| Found {
                row: m.row,
                col: m.x,
                anchor: Anchor {
                    page: m.serial,
                    page_row: m.page_y,
                    col: m.x,
                },
            })
            .collect())
    }

    /// Highlights match `index` as the current one, or none.
    pub fn set_search_current(&mut self, index: Option<usize>) {
        let index = index.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1);
        unsafe { sys::embeddedTerminalSetSearchCurrent(self.handle, index) };
    }

    /// Drops the search's text and highlights.
    pub fn clear_search(&mut self) {
        unsafe { sys::embeddedTerminalClearSearch(self.handle) };
    }

    /// The colors of search matches, and of the current one, as (text,
    /// background). Text without a color keeps its own.
    pub fn set_search_colors(
        &mut self,
        matched: (Option<Rgba>, Rgba),
        current: (Option<Rgba>, Rgba),
    ) {
        let fg = |color: &Option<Rgba>| color.as_ref().map_or(std::ptr::null(), Rgba::as_ptr);
        unsafe {
            sys::embeddedTerminalSetSearchColors(
                self.handle,
                fg(&matched.0),
                matched.1.as_ptr(),
                fg(&current.0),
                current.1.as_ptr(),
            )
        };
    }

    /// Whether the program switched to the alternate screen, as full-screen
    /// programs do. It has no scrollback.
    pub fn is_alternate_screen(&self) -> bool {
        unsafe { sys::embeddedTerminalIsAlternateScreen(self.handle) == 1 }
    }

    /// The title the program set (OSC 0 or 2), or "".
    pub fn title(&self) -> String {
        read_sized(|out, len, required| unsafe {
            sys::embeddedTerminalGetTitle(self.handle, out, len, required)
        })
    }

    /// Selects the cells from `start` to `end` (column, row) on screen,
    /// inclusive, in reading order.
    pub fn set_selection(&mut self, start: (u16, u16), end: (u16, u16)) -> Result<()> {
        check(
            unsafe {
                sys::embeddedTerminalSetSelection(self.handle, start.0, start.1, end.0, end.1)
            },
            "terminal selection",
        )
    }

    pub fn clear_selection(&mut self) {
        unsafe { sys::embeddedTerminalClearSelection(self.handle) };
    }

    /// Selects the word at a viewport cell.
    pub fn select_word(&mut self, at: (u16, u16)) -> Result<()> {
        check(
            unsafe { sys::embeddedTerminalSelectWord(self.handle, at.0, at.1) },
            "terminal word selection",
        )
    }

    /// The selected text, or "" with nothing selected.
    pub fn selected_text(&self) -> String {
        read_sized(|out, len, required| unsafe {
            sys::embeddedTerminalGetSelectedText(self.handle, out, len, required)
        })
    }

    /// What's at screen cell (`x`, `y`), for opening it.
    pub fn line_at(&self, x: u16, y: u16) -> Option<LineAt> {
        let mut offset = u32::MAX;
        let mut is_link = 0u8;
        let mut failed = false;
        let text = read_sized(|out, len, required| {
            let written = unsafe {
                sys::embeddedTerminalLineAt(
                    self.handle,
                    x,
                    y,
                    out,
                    len,
                    required,
                    &mut offset,
                    &mut is_link,
                )
            };
            failed = written < 0 && written != OUT_OF_SPACE;
            written
        });
        if failed {
            return None;
        }
        Some(if is_link != 0 {
            LineAt::Link(text)
        } else {
            LineAt::Text {
                text,
                offset: (offset != u32::MAX).then_some(offset as usize),
            }
        })
    }

    /// Draws the whole screen with its top-left cell at (`x`, `y`), within
    /// `buffer`'s clip.
    pub fn draw(&self, buffer: &Buffer, x: i32, y: i32) {
        // Composing draws only the rows changed since the last time, but
        // frames start out blank.
        unsafe {
            sys::embeddedTerminalInvalidate(self.handle);
            sys::embeddedTerminalCompose(self.handle, buffer.raw_handle(), x, y);
        }
    }

    /// The cursor as of the last [`draw`](EmbeddedTerminal::draw).
    pub fn cursor(&self) -> Cursor {
        let mut raw = sys::ExternalEmbeddedTerminalCursor {
            x: 0,
            y: 0,
            has_value: 0,
            visible: 0,
            blinking: 0,
            wide_tail: 0,
            style: 1,
            color_has_value: 0,
            color_r: 0,
            color_g: 0,
            color_b: 0,
            _padding: 0,
        };
        unsafe { sys::embeddedTerminalCursor(self.handle, &mut raw) };
        Cursor {
            position: (raw.has_value != 0).then_some((raw.x, raw.y)),
            visible: raw.visible != 0,
            blinking: raw.blinking != 0,
            style: match raw.style {
                0 => CursorStyle::Bar,
                2 => CursorStyle::Underline,
                3 => CursorStyle::HollowBlock,
                _ => CursorStyle::Block,
            },
        }
    }

    /// The bytes for `key`, in the encoding the program asked for (legacy,
    /// modifyOtherKeys, or the kitty keyboard protocol). Empty if the key
    /// sends nothing.
    pub fn encode_key(&self, key: &KeyEvent) -> Vec<u8> {
        let options = sys::ExternalEmbeddedTerminalKeyOptions {
            action: key.action as u8,
            composing: 0,
            mods: key.mods.0,
            consumed_mods: 0,
            _padding: 0,
            unshifted_codepoint: key.unshifted.map_or(0, u32::from),
        };
        let mut out = vec![0u8; 64];
        loop {
            let mut required = 0u32;
            let written = unsafe {
                sys::embeddedTerminalEncodeKey(
                    self.handle,
                    &options,
                    key.code.as_ptr(),
                    ffi_len(key.code.len(), "key code"),
                    key.text.as_ptr(),
                    ffi_len(key.text.len(), "key text"),
                    out.as_mut_ptr(),
                    ffi_len(out.len(), "output"),
                    &mut required,
                )
            };
            if written == OUT_OF_SPACE && required as usize > out.len() {
                out.resize(required as usize, 0);
                continue;
            }
            out.truncate(written.max(0) as usize);
            return out;
        }
    }

    /// The bytes for `mouse`, if the program asked for mouse reports of
    /// this kind; empty otherwise.
    pub fn encode_mouse(&self, mouse: &MouseEvent) -> Vec<u8> {
        let mut out = [0u8; 64];
        let written = unsafe {
            sys::embeddedTerminalEncodeMouse(
                self.handle,
                mouse.action as u8,
                mouse.button.map_or(-1, |button| button as i8),
                mouse.mods.0,
                mouse.x as f32,
                mouse.y as f32,
                mouse.any_button_pressed as u8,
                out.as_mut_ptr(),
                out.len() as u32,
            )
        };
        out[..written.max(0) as usize].to_vec()
    }

    /// `text` as a paste, bracketed if the program asked for that.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        // Bracketing adds 12 bytes; unbracketed, newlines become CRs.
        let mut out = vec![0u8; text.len() + 16];
        let written = unsafe {
            sys::embeddedTerminalEncodePaste(
                self.handle,
                text.as_ptr(),
                ffi_len(text.len(), "paste"),
                out.as_mut_ptr(),
                ffi_len(out.len(), "output"),
            )
        };
        out.truncate(written.max(0) as usize);
        out
    }

    /// The report of gaining or losing focus, if the program asked for them.
    pub fn encode_focus(&self, focused: bool) -> Vec<u8> {
        let mut out = [0u8; 16];
        let written = unsafe {
            sys::embeddedTerminalEncodeFocus(
                self.handle,
                focused as u8,
                out.as_mut_ptr(),
                out.len() as u32,
            )
        };
        out[..written.max(0) as usize].to_vec()
    }

    /// Takes the terminal's replies to the program's queries (cursor
    /// position, device attributes, ...), which go back to the program.
    pub fn drain_responses(&mut self) -> Vec<u8> {
        let mut responses = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let written = unsafe {
                sys::embeddedTerminalDrainResponses(
                    self.handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                )
            };
            if written <= 0 {
                return responses;
            }
            responses.extend_from_slice(&chunk[..written as usize]);
        }
    }

    /// Takes the text the program last put on the clipboard (OSC 52), if
    /// it did since the last call. Only writes: requests to read the
    /// clipboard are ignored, and so are requests to clear it.
    pub fn take_clipboard(&mut self) -> Option<String> {
        let text = read_sized(|out, len, required| unsafe {
            sys::embeddedTerminalTakeClipboard(self.handle, out, len, required)
        });
        (!text.is_empty()).then_some(text)
    }

    /// The native handle, for calling [`sys`](crate::sys) functions directly.
    pub fn raw_handle(&self) -> sys::Handle {
        self.handle
    }
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        unsafe { sys::destroyEmbeddedTerminal(self.handle) }
    }
}

fn check(status: i32, what: &'static str) -> Result<()> {
    if status < 0 {
        Err(Error::CallFailed(what))
    } else {
        Ok(())
    }
}

/// Reads a string through a native call that copies it out, and reports its
/// full length when it doesn't fit.
fn read_sized(mut read: impl FnMut(*mut u8, u32, *mut u32) -> i32) -> String {
    let mut out = vec![0u8; 256];
    loop {
        let mut required = 0u32;
        let written = read(
            out.as_mut_ptr(),
            ffi_len(out.len(), "output"),
            &mut required,
        );
        if written == OUT_OF_SPACE && required as usize > out.len() {
            out.resize(required as usize, 0);
            continue;
        }
        out.truncate(written.max(0) as usize);
        return String::from_utf8_lossy(&out).into_owned();
    }
}
