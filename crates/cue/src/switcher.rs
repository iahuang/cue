//! A panel's recently viewed list (Ctrl+Shift+-): what the panel showed,
//! most recent first, each file or terminal once however many places in it
//! the history keeps, dropped down from the panel's header.
//!
//! It works as macOS's Cmd+Tab does. It opens on what was shown before
//! what's on screen, and stays open while Ctrl is held; letting go goes
//! there, so a tap goes back one. With Ctrl still held, `-` selects the one
//! before, and `+` (`=`) the one after. Up and Down, Enter, Esc, and clicks
//! work too, and any other key goes to the selection and then does what it
//! does. Opened without a modifier held (from the command palette), or in a
//! terminal that can't say when keys go up, it stays open until one of
//! those.
//!
//! Held open, it shows only after a moment, as Cmd+Tab does, so a tap
//! doesn't flash it on screen; going to another row shows it at once.
//!
//! Going there goes back through the panel's history as Go Back would, so
//! Go Forward comes back.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use opentui::{Attributes, Buffer};

use crate::input::{Key, KeyCode, Mods, Mouse, MouseButton, MouseKind};
use crate::layout::{PanelId, Rect};
use crate::picker::{self, Area, Item};
use crate::theme;

/// The widest the list gets, in columns.
const MAX_WIDTH: u32 = 72;
/// The narrowest, screen permitting.
const MIN_WIDTH: u32 = 32;
/// The most rows shown at once.
const MAX_ROWS: u32 = 14;
/// How long it's held open before it shows.
const SHOW_DELAY: Duration = Duration::from_millis(150);

/// A row of the list.
pub struct Entry {
    pub item: Item,
    /// Its place among the panel's places to go back to (see
    /// [`crate::panel::Panel::history`]); `None` for what's on screen.
    pub back: Option<usize>,
}

/// What the app should do after the list handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum SwitcherAction {
    Continue,
    Close,
    /// Go back to the place in the history, if any (see [`Entry::back`]).
    Accept(Option<usize>),
    /// Go back to it, then handle the key as if the list weren't open.
    AcceptAndPass(Option<usize>),
}

pub struct Switcher {
    /// The panel whose history it lists.
    pub panel: PanelId,
    entries: Vec<Entry>,
    selected: usize,
    /// The first row on screen.
    scroll: usize,
    /// The modifiers held to keep it open; none if it stays open.
    held: Mods,
    /// When it opened.
    opened: Instant,
    /// Whether it's on screen yet.
    shown: bool,
    /// The panel's header, which the list drops down from.
    header: Rect,
    screen_width: u32,
    screen_height: u32,
}

impl Switcher {
    /// The list of `entries`, what's on screen first if anything is, for
    /// the panel with the `header`, on a screen `width` x `height`. It
    /// starts on what was shown before. It stays open while a modifier in
    /// `held` is (Shift aside); then it shows a moment after `now`.
    pub fn new(
        panel: PanelId,
        entries: Vec<Entry>,
        held: Mods,
        header: Rect,
        (width, height): (u32, u32),
        now: Instant,
    ) -> Switcher {
        let selected = match entries.first() {
            Some(Entry { back: None, .. }) => 1,
            _ => 0,
        };
        let mut switcher = Switcher {
            panel,
            entries,
            selected: 0,
            scroll: 0,
            held: Mods {
                shift: false,
                ..held
            },
            opened: now,
            shown: held.is_plain(),
            header,
            screen_width: width,
            screen_height: height,
        };
        switcher.select(selected);
        switcher
    }

    /// The rows' text, the selected one's marked with `>`.
    #[cfg(test)]
    pub fn listed(&self) -> Vec<String> {
        let mark = |index| if index == self.selected { ">" } else { "" };
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| format!("{}{}", mark(index), entry.item.text))
            .collect()
    }

    /// Whether it waits for a modifier to go up (see [`Switcher::release`]).
    pub fn is_held(&self) -> bool {
        !self.held.is_plain()
    }

    /// Whether it's on screen.
    #[cfg(test)]
    pub fn is_shown(&self) -> bool {
        self.shown
    }

    /// How long from `now` until it shows, if it hasn't yet.
    pub fn shows_in(&self, now: Instant) -> Option<Duration> {
        let shows = self.opened + SHOW_DELAY;
        (!self.shown).then(|| shows.saturating_duration_since(now))
    }

    /// Shows it if it's been held open long enough by `now`. Returns
    /// whether it showed.
    pub fn reveal(&mut self, now: Instant) -> bool {
        if self.shows_in(now).is_some_and(|left| left.is_zero()) {
            self.shown = true;
            return true;
        }
        false
    }

    pub fn set_size(&mut self, width: u32, height: u32, header: Rect) {
        self.screen_width = width;
        self.screen_height = height;
        self.header = header;
        self.scroll_into_view();
    }

    pub fn handle_key(&mut self, key: Key) -> SwitcherAction {
        let shift = key.mods.shift;
        match key.code {
            KeyCode::Char('-' | '_') | KeyCode::Down => self.step(1),
            KeyCode::Tab if !shift => self.step(1),
            KeyCode::Char('=' | '+') | KeyCode::Up | KeyCode::Tab => self.step(-1),
            KeyCode::Enter => return self.accept(),
            KeyCode::Esc => return SwitcherAction::Close,
            _ => return SwitcherAction::AcceptAndPass(self.chosen()),
        }
        SwitcherAction::Continue
    }

    /// A key went up, with `mods` held still: once none it was held open
    /// by is, it goes to the selection.
    pub fn release(&self, mods: Mods) -> SwitcherAction {
        let held = |held: bool, still: bool| held && still;
        let still = held(self.held.ctrl, mods.ctrl)
            || held(self.held.alt, mods.alt)
            || held(self.held.sup, mods.sup);
        match self.is_held() && !still {
            true => self.accept(),
            false => SwitcherAction::Continue,
        }
    }

    /// A click on a row goes there; one outside closes the list.
    pub fn handle_mouse(&mut self, mouse: Mouse) -> SwitcherAction {
        let area = self.area();
        let inside = self.shown && area.contains(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::Press(_) if !inside => SwitcherAction::Close,
            MouseKind::Press(MouseButton::Left) => match self.row_at(mouse.y) {
                Some(index) => {
                    self.selected = index;
                    self.accept()
                }
                None => SwitcherAction::Continue,
            },
            MouseKind::ScrollUp if inside => {
                self.step(-1);
                SwitcherAction::Continue
            }
            MouseKind::ScrollDown if inside => {
                self.step(1);
                SwitcherAction::Continue
            }
            _ => SwitcherAction::Continue,
        }
    }

    /// The row under the pointer, to highlight.
    pub fn hover_area(&self, x: u32, y: u32) -> Option<Area> {
        let area = self.area();
        if !self.shown || !area.contains(x, y) || x == area.x || x + 1 == area.x + area.width {
            return None;
        }
        self.row_at(y)?;
        Some(Area {
            x: area.x + 1,
            y,
            width: area.width.saturating_sub(2),
            height: 1,
        })
    }

    pub fn draw(&self, frame: &Buffer) {
        let area = self.area();
        if !self.shown || area.width < 8 || area.height < 3 {
            return;
        }
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            self.draw_list(frame, area)
        });
    }

    // --- selection --------------------------------------------------------------

    fn step(&mut self, step: isize) {
        self.shown = true;
        self.select(self.selected.saturating_add_signed(step));
    }

    /// Selects row `index` (clamped) and scrolls it into view.
    fn select(&mut self, index: usize) {
        self.selected = index.min(self.entries.len().saturating_sub(1));
        self.scroll_into_view();
    }

    fn scroll_into_view(&mut self) {
        let rows = self.rows().max(1) as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(rows));
    }

    /// Where the selection is in the history.
    fn chosen(&self) -> Option<usize> {
        self.entries.get(self.selected).and_then(|entry| entry.back)
    }

    fn accept(&self) -> SwitcherAction {
        SwitcherAction::Accept(self.chosen())
    }

    /// The entry on screen row `y`.
    fn row_at(&self, y: u32) -> Option<usize> {
        let area = self.area();
        if y <= area.y || y + 1 >= area.y + area.height {
            return None;
        }
        let index = self.scroll + (y - area.y - 1) as usize;
        (index < self.entries.len()).then_some(index)
    }

    // --- layout and drawing -----------------------------------------------------

    /// Rows on screen: one per entry, up to what fits below the header,
    /// above the status bar.
    fn rows(&self) -> u32 {
        let below = self.header.y + 1;
        let room = self.screen_height.saturating_sub(below + 3);
        (self.entries.len() as u32).min(MAX_ROWS).min(room)
    }

    /// Below the panel's header, centered on it, and kept on screen.
    fn area(&self) -> Area {
        let widest = self
            .entries
            .iter()
            .map(|entry| entry.item.width())
            .max()
            .unwrap_or(0) as u32;
        // The borders, and a space inside each.
        let width = (widest + 4)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .min(self.screen_width);
        let center = self.header.x + self.header.width / 2;
        let x = center
            .saturating_sub(width / 2)
            .min(self.screen_width - width);
        Area {
            x,
            y: self.header.y + 1,
            width,
            height: self.rows() + 2,
        }
    }

    fn draw_list(&self, frame: &Buffer, area: Area) {
        let colors = theme::colors();
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        frame.fill_rect(x, y, width, height, colors.bg);
        let rule = "─".repeat(width.saturating_sub(2) as usize);
        let text =
            |s: &str, x: u32, y: u32, fg| frame.draw_text(s, x, y, fg, None, Attributes::NONE);
        text(&format!("╭{rule}╮"), x, y, colors.border);
        text(&format!("╰{rule}╯"), x, y + height - 1, colors.border);
        frame.draw_text(
            " Recently Viewed ",
            x + 2,
            y,
            colors.text,
            None,
            Attributes::BOLD,
        );
        let room = width.saturating_sub(4) as usize;
        let none = HashSet::new();
        let visible = self.entries.iter().enumerate().skip(self.scroll);
        for ((index, entry), row) in visible.zip(y + 1..y + height - 1) {
            text("│", x, row, colors.border);
            text("│", x + width - 1, row, colors.border);
            if index == self.selected {
                frame.fill_rect(x + 1, row, width - 2, 1, colors.selected);
            }
            picker::draw_item(frame, &entry.item, x + 2, row, room, &none);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switcher(entries: usize, held: Mods) -> Switcher {
        let entries = (0..entries)
            .map(|i| Entry {
                item: Item::untitled(i as u32 + 1),
                back: i.checked_sub(1).map(|i| 10 - i),
            })
            .collect();
        let header = Rect {
            x: 20,
            y: 1,
            width: 40,
            height: 1,
        };
        Switcher::new(PanelId::default(), entries, held, header, (80, 24), now())
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn key(code: KeyCode, mods: Mods) -> Key {
        Key::new(code, mods)
    }

    #[test]
    fn opens_on_what_was_shown_before_and_steps_both_ways() {
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        let mut switcher = switcher(4, ctrl_shift);
        assert_eq!(switcher.chosen(), Some(10), "one back");
        switcher.handle_key(key(KeyCode::Char('-'), ctrl_shift));
        assert_eq!(switcher.chosen(), Some(9), "and another");
        switcher.handle_key(key(KeyCode::Char('-'), Mods::CTRL));
        switcher.handle_key(key(KeyCode::Char('_'), ctrl_shift));
        assert_eq!(switcher.chosen(), Some(8), "the last stays selected");
        switcher.handle_key(key(KeyCode::Char('='), ctrl_shift));
        switcher.handle_key(key(KeyCode::Char('+'), ctrl_shift));
        switcher.handle_key(key(KeyCode::Char('='), ctrl_shift));
        assert_eq!(switcher.chosen(), None, "up to what's on screen");
    }

    #[test]
    fn letting_go_of_ctrl_goes_to_the_selection() {
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        let switcher = switcher(3, ctrl_shift);
        assert_eq!(switcher.release(ctrl_shift), SwitcherAction::Continue);
        assert_eq!(switcher.release(Mods::CTRL), SwitcherAction::Continue);
        assert_eq!(
            switcher.release(Mods::SHIFT),
            SwitcherAction::Accept(Some(10))
        );
    }

    #[test]
    fn opened_without_a_modifier_it_stays_open() {
        let mut switcher = switcher(3, Mods::NONE);
        assert!(!switcher.is_held());
        assert_eq!(switcher.release(Mods::NONE), SwitcherAction::Continue);
        switcher.handle_key(key(KeyCode::Down, Mods::NONE));
        assert_eq!(
            switcher.handle_key(key(KeyCode::Enter, Mods::NONE)),
            SwitcherAction::Accept(Some(9))
        );
        assert_eq!(
            switcher.handle_key(key(KeyCode::Esc, Mods::NONE)),
            SwitcherAction::Close
        );
        assert_eq!(
            switcher.handle_key(key(KeyCode::Char('w'), Mods::CTRL)),
            SwitcherAction::AcceptAndPass(Some(9)),
            "another key goes there, then does what it does"
        );
    }

    #[test]
    fn held_open_it_shows_after_a_moment_or_on_moving() {
        let mut held = switcher(3, Mods::CTRL);
        let opened = held.opened;
        assert!(!held.is_shown(), "a tap doesn't flash it");
        assert!(!held.reveal(opened + SHOW_DELAY / 2));
        assert_eq!(held.shows_in(opened + SHOW_DELAY / 2), Some(SHOW_DELAY / 2));
        assert!(held.reveal(opened + SHOW_DELAY));
        assert!(held.is_shown() && held.shows_in(opened).is_none());

        let mut held = switcher(3, Mods::CTRL);
        held.handle_key(key(KeyCode::Char('-'), Mods::CTRL));
        assert!(held.is_shown(), "going to another row shows it");
        assert!(switcher(3, Mods::NONE).is_shown(), "not held, at once");
    }

    #[test]
    fn drops_down_centered_on_the_header() {
        let switcher = switcher(3, Mods::CTRL);
        let area = switcher.area();
        assert_eq!((area.y, area.height), (2, 5));
        assert_eq!(area.x + area.width / 2, 40);
    }
}
