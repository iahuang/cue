//! What the read-only views of a file, the reader and the diff, do alike:
//! scroll through the rows the text is laid out in, select them with the
//! mouse, and refuse to be edited.

use std::cell::Cell;
use std::time::Instant;

use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::Command;

/// A row, and a column of the text on it.
pub type Spot = (usize, usize);

struct Click {
    at: Spot,
    time: Instant,
    count: u32,
}

/// Where a view is scrolled to, and what the mouse selected in it.
#[derive(Default)]
pub struct Scroller {
    /// The row at the top of the view.
    pub top: Cell<usize>,
    /// Where a selection started, and where it goes to.
    pub selection: Cell<Option<(Spot, Spot)>>,
    /// Where the left button went down, until it's released.
    press: Option<Spot>,
    dragged: bool,
    last_click: Option<Click>,
}

impl Scroller {
    /// The selection, start first.
    pub fn ordered_selection(&self) -> Option<(Spot, Spot)> {
        let (a, b) = self.selection.get()?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The spot at column `col` of the text on screen row `y`.
    pub fn spot(&self, y: u32, col: usize) -> Spot {
        (self.top.get() + y as usize, col)
    }

    /// Counts a left click at `at`: 2 for a double click there.
    pub fn count_click(&mut self, at: Spot, now: Instant) -> u32 {
        let count = match &self.last_click {
            Some(click) if click.at == at && now.duration_since(click.time) < MULTI_CLICK => {
                click.count + 1
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            at,
            time: now,
            count,
        });
        count
    }

    /// The left button went down at `at`: with Shift, the selection goes
    /// there; without, it's dropped, and a drag starts another.
    pub fn press(&mut self, at: Spot, shift: bool) {
        match (shift, self.selection.get()) {
            (true, Some((anchor, _))) => self.selection.set(Some((anchor, at))),
            _ => self.selection.set(None),
        }
        self.press = Some(at);
        self.dragged = false;
    }
}

/// What a command did that the view can't see to itself.
pub enum Ran {
    Done,
    /// Copy the selected text, if there is any.
    Copy(Option<String>),
    /// It would edit, and can't.
    ReadOnly,
}

/// A view of text laid out in rows that can be read but not edited.
pub trait Scrolled {
    fn scroller(&self) -> &Scroller;
    fn scroller_mut(&mut self) -> &mut Scroller;
    /// Lays the text out again, if it changed.
    fn sync(&self);
    /// How many rows the text is laid out in.
    fn rows(&self) -> usize;
    /// How many rows the view is high.
    fn height(&self) -> u32;
    fn selected_text(&self) -> Option<String>;

    fn max_top(&self) -> usize {
        self.rows().saturating_sub(self.height() as usize)
    }

    fn scroll(&mut self, rows: i64) {
        self.sync();
        let top = &self.scroller().top;
        let to = top.get() as i64 + rows;
        top.set(to.clamp(0, self.max_top() as i64) as usize);
    }

    fn page(&self) -> i64 {
        self.height().saturating_sub(1).max(1) as i64
    }

    fn scroll_to_end(&mut self, end: bool) {
        self.sync();
        let top = if end { self.max_top() } else { 0 };
        self.scroller().top.set(top);
    }

    fn has_selection(&self) -> bool {
        self.scroller().selection.get().is_some_and(|(a, b)| a != b)
    }

    fn clear_selection(&mut self) {
        self.scroller().selection.set(None);
    }

    fn select_all(&mut self) {
        self.sync();
        let rows = self.rows();
        self.scroller().selection.set(Some(((0, 0), (rows, 0))));
    }

    /// How far down the view is, for the status bar.
    fn position(&self) -> String {
        match (self.scroller().top.get(), self.max_top()) {
            (_, 0) => "All".to_string(),
            (0, _) => "Top".to_string(),
            (top, max) if top >= max => "Bot".to_string(),
            (top, max) => format!("{}%", top * 100 / max),
        }
    }

    /// The mouse at column `col` of the text, but for a left press, which
    /// the view sees to: a drag selects, scrolling past the top or bottom,
    /// and the wheel scrolls. A release that ended no drag says where the
    /// button went down.
    fn drag_or_scroll(&mut self, mouse: Mouse, col: usize) -> Option<Spot> {
        match mouse.kind {
            MouseKind::Drag(MouseButton::Left) => {
                let origin = self.scroller().press?;
                let height = self.height();
                if mouse.y >= height {
                    self.scroll(1);
                } else if mouse.y == 0 {
                    self.scroll(-1);
                }
                let scroller = self.scroller_mut();
                let at = scroller.spot(mouse.y.min(height.saturating_sub(1)), col);
                scroller.dragged = scroller.dragged || at != origin;
                if scroller.dragged {
                    scroller.selection.set(Some((origin, at)));
                }
            }
            MouseKind::Release(MouseButton::Left) => {
                let scroller = self.scroller_mut();
                let origin = scroller.press.take()?;
                return (!scroller.dragged).then_some(origin);
            }
            MouseKind::ScrollUp => self.scroll(-(crate::config::get().scroll_lines as i64)),
            MouseKind::ScrollDown => self.scroll(crate::config::get().scroll_lines as i64),
            _ => {}
        }
        None
    }

    /// Runs an editor command: those that move the cursor scroll, and
    /// those that edit don't.
    fn run(&mut self, command: Command) -> Ran {
        match command {
            Command::Copy => return Ran::Copy(self.selected_text()),
            Command::SelectAll => self.select_all(),
            Command::ClearSelection => self.clear_selection(),
            Command::CursorUp => self.scroll(-1),
            Command::CursorDown => self.scroll(1),
            Command::CursorPageUp => self.scroll(-self.page()),
            Command::CursorPageDown => self.scroll(self.page()),
            Command::DocumentStart => self.scroll_to_end(false),
            Command::DocumentEnd => self.scroll_to_end(true),
            Command::Undo
            | Command::Redo
            | Command::Cut
            | Command::Paste
            | Command::NewLine
            | Command::InsertTab
            | Command::Indent
            | Command::Outdent
            | Command::ToggleComment
            | Command::DeleteBackward
            | Command::DeleteForward
            | Command::DeleteWordBackward
            | Command::DeleteWordForward
            | Command::MoveLinesUp
            | Command::MoveLinesDown
            | Command::Replace
            | Command::ReplaceAll => return Ran::ReadOnly,
            _ => {}
        }
        Ran::Done
    }

    /// A key bound to no command: Space pages down, and Shift+Space up.
    /// Whether it would have typed, and can't.
    fn type_key(&mut self, key: Key) -> bool {
        match key.code {
            KeyCode::Char(' ') if key.mods.is_plain() => {
                let page = self.page();
                self.scroll(if key.mods.shift { -page } else { page });
                false
            }
            KeyCode::Char(_) => key.mods.is_plain(),
            _ => false,
        }
    }
}
