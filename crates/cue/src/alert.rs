//! An alert: a question to answer before going on, such as whether to lose
//! unsaved changes, with a button for each answer and one to cancel.
//!
//! As in the alerts of macOS, Enter or Space takes the selected button, Esc
//! cancels, and a button's underlined letter takes it. Left and Right, or
//! Tab and Shift+Tab, move between the buttons. A click takes a button; a
//! click outside cancels.

use opentui::{Attributes, Buffer, Rgba};

use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind};
use crate::picker::Area;
use crate::theme;

/// The widest a line of the message gets before wrapping.
const MAX_TEXT: usize = 56;
/// Between buttons.
const GAP: u32 = 2;

/// An answer, as a button.
pub struct Button<T> {
    label: String,
    /// The letter that takes it, by its index in `label`.
    mnemonic: Option<(usize, char)>,
    value: T,
    danger: bool,
}

impl<T> Button<T> {
    /// A button labeled `label`, with `&` before the letter that takes it,
    /// as in `Do&n't Save`, that answers `value`.
    pub fn new(label: &str, value: T) -> Button<T> {
        let mut text = String::new();
        let mut mnemonic = None;
        let mut chars = label.chars();
        while let Some(c) = chars.next() {
            if c == '&' && mnemonic.is_none() {
                if let Some(next) = chars.next() {
                    mnemonic = Some((text.chars().count(), next.to_ascii_lowercase()));
                    text.push(next);
                }
                continue;
            }
            text.push(c);
        }
        Button {
            label: text,
            mnemonic,
            value,
            danger: false,
        }
    }

    /// Marks it as losing something, such as unsaved changes.
    pub fn danger(mut self) -> Button<T> {
        self.danger = true;
        self
    }
}

/// What the app should do after the alert handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum AlertAction<T> {
    Continue,
    Cancel,
    Answer(T),
}

pub struct Alert<T> {
    title: String,
    message: String,
    /// The answers; Cancel follows them.
    buttons: Vec<Button<T>>,
    /// The selected button; `buttons.len()` is Cancel.
    selected: usize,
    screen_width: u32,
    screen_height: u32,
}

impl<T: Clone> Alert<T> {
    /// An alert titled `title` saying `message`, with `buttons` and Cancel,
    /// the first selected, on a screen `width` x `height`.
    pub fn new(
        title: impl Into<String>,
        message: impl Into<String>,
        buttons: Vec<Button<T>>,
        width: u32,
        height: u32,
    ) -> Alert<T> {
        Alert {
            title: title.into(),
            message: message.into(),
            buttons,
            selected: 0,
            screen_width: width,
            screen_height: height,
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
    }

    pub fn handle_key(&mut self, key: Key) -> AlertAction<T> {
        let count = self.buttons.len() + 1;
        let plain = !(key.mods.ctrl || key.mods.alt || key.mods.sup);
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') if plain => return self.take(self.selected),
            KeyCode::Esc => return AlertAction::Cancel,
            KeyCode::Left | KeyCode::Up => self.selected = (self.selected + count - 1) % count,
            KeyCode::Tab if key.mods.shift => self.selected = (self.selected + count - 1) % count,
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1) % count
            }
            KeyCode::Char(c) if plain => {
                let c = c.to_ascii_lowercase();
                let index = self
                    .buttons
                    .iter()
                    .position(|button| button.mnemonic.is_some_and(|(_, m)| m == c));
                if let Some(index) = index {
                    return self.take(index);
                }
            }
            _ => {}
        }
        AlertAction::Continue
    }

    pub fn handle_mouse(&mut self, mouse: Mouse) -> AlertAction<T> {
        let MouseKind::Press(MouseButton::Left) = mouse.kind else {
            return AlertAction::Continue;
        };
        let area = self.area();
        if !area.contains(mouse.x, mouse.y) {
            return AlertAction::Cancel;
        }
        let row = area.y + area.height - 2;
        let hit = self
            .button_spans(area)
            .into_iter()
            .position(|(x, width)| mouse.y == row && (x..x + width).contains(&mouse.x));
        match hit {
            Some(index) => {
                self.selected = index;
                self.take(index)
            }
            None => AlertAction::Continue,
        }
    }

    /// Draws it in the middle of the screen, above the status bar.
    pub fn draw(&self, frame: &Buffer) {
        let area = self.area();
        if area.width < 4 || area.height < 3 {
            return;
        }
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            self.draw_alert(frame, area)
        });
    }

    fn take(&self, index: usize) -> AlertAction<T> {
        match self.buttons.get(index) {
            Some(button) => AlertAction::Answer(button.value.clone()),
            None => AlertAction::Cancel,
        }
    }

    // --- layout and drawing -----------------------------------------------------

    /// The labels of the buttons, Cancel last.
    fn labels(&self) -> impl Iterator<Item = &str> {
        self.buttons
            .iter()
            .map(|button| button.label.as_str())
            .chain(["Cancel"])
    }

    /// Each button is its label with a space either side.
    fn buttons_width(&self) -> u32 {
        let widths: u32 = self.labels().map(|l| l.chars().count() as u32 + 2).sum();
        widths + GAP * self.buttons.len() as u32
    }

    /// The width inside the borders and the space along each.
    fn text_width(&self) -> u32 {
        let longest = self
            .message
            .lines()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .min(MAX_TEXT) as u32;
        let title = self.title.chars().count() as u32 + 4;
        let widest = longest.max(title).max(self.buttons_width());
        widest.min(self.screen_width.saturating_sub(4))
    }

    /// The message's lines, wrapped to fit.
    fn lines(&self) -> Vec<String> {
        wrap(&self.message, self.text_width().max(1) as usize)
    }

    /// Centered on the screen above the status bar, with a blank row above
    /// and below the message where there's room.
    fn area(&self) -> Area {
        let width = (self.text_width() + 4).min(self.screen_width);
        let room = self.screen_height.saturating_sub(1);
        let lines = self.lines().len() as u32;
        let padded = lines + 5;
        let height = if padded <= room { padded } else { lines + 3 }.min(room);
        Area {
            x: (self.screen_width - width) / 2,
            y: (room - height) / 2,
            width,
            height,
        }
    }

    /// Where each button is on its row, as (x, width), right-aligned.
    fn button_spans(&self, area: Area) -> Vec<(u32, u32)> {
        let right = area.x + area.width - 2;
        let mut x = right.saturating_sub(self.buttons_width());
        self.labels()
            .map(|label| {
                let width = label.chars().count() as u32 + 2;
                let span = (x, width);
                x += width + GAP;
                span
            })
            .collect()
    }

    fn draw_alert(&self, frame: &Buffer, area: Area) {
        let colors = theme::colors();
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        frame.fill_rect(x, y, width, height, colors.bg);
        let inner = width.saturating_sub(2) as usize;
        let text = |s: &str, x: u32, y: u32, fg, bg: Option<Rgba>, attributes| {
            frame.draw_text(s, x, y, fg, bg, attributes)
        };
        let border = |s: &str, x: u32, y: u32| text(s, x, y, colors.border, None, Attributes::NONE);
        let rule = "─".repeat(inner);
        border(&format!("╭{rule}╮"), x, y);
        border(&format!("╰{rule}╯"), x, y + height - 1);
        for row in y + 1..y + height - 1 {
            border("│", x, row);
            border("│", x + width - 1, row);
        }
        text(
            &format!(" {} ", self.title),
            x + 2,
            y,
            colors.text,
            None,
            Attributes::BOLD,
        );

        // Messages cut short by a short screen keep their last line for
        // the buttons.
        let padded = height >= self.lines().len() as u32 + 5;
        let first = y + 1 + padded as u32;
        let buttons_row = y + height - 2;
        for (line, row) in self.lines().iter().zip(first..buttons_row) {
            text(line, x + 2, row, colors.text, None, Attributes::NONE);
        }

        let buttons = self.buttons.iter().map(|b| (b.mnemonic, b.danger));
        let buttons = buttons.chain([(None, false)]);
        let spans = self.button_spans(area);
        for (index, ((label, (mnemonic, danger)), (bx, bw))) in
            self.labels().zip(buttons).zip(spans).enumerate()
        {
            let selected = index == self.selected;
            let bg = if selected {
                colors.selected
            } else {
                colors.surface
            };
            let fg = if danger { colors.error } else { colors.text };
            let bold = match selected {
                true => Attributes::BOLD,
                false => Attributes::NONE,
            };
            frame.fill_rect(bx, buttons_row, bw, 1, bg);
            text(label, bx + 1, buttons_row, fg, Some(bg), bold);
            if let Some((at, _)) = mnemonic {
                if let Some(c) = label.chars().nth(at) {
                    let underlined = bold | Attributes::UNDERLINE;
                    text(
                        &c.to_string(),
                        bx + 1 + at as u32,
                        buttons_row,
                        fg,
                        Some(bg),
                        underlined,
                    );
                }
            }
        }
    }
}

/// `text`'s lines, wrapped at spaces to `width` characters, and words too
/// long for a line broken where they reach its end.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let mut word: Vec<char> = word.chars().collect();
            let len = line.chars().count();
            if len > 0 && len + 1 + word.len() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            while line.chars().count() + word.len() > width {
                let fits = width - line.chars().count();
                line.extend(word.drain(..fits));
                lines.push(std::mem::take(&mut line));
            }
            line.extend(word);
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use opentui::{OwnedBuffer, WidthMethod};

    fn alert() -> Alert<&'static str> {
        let buttons = vec![
            Button::new("&Save", "save"),
            Button::new("Do&n't Save", "discard").danger(),
        ];
        Alert::new(
            "Close a.txt?",
            "a.txt has unsaved changes.",
            buttons,
            80,
            24,
        )
    }

    fn key(code: KeyCode) -> Key {
        Key::new(code, Mods::NONE)
    }

    fn click(x: u32, y: u32) -> Mouse {
        Mouse {
            kind: MouseKind::Press(MouseButton::Left),
            x,
            y,
            mods: Mods::NONE,
        }
    }

    fn screen(alert: &Alert<&str>, width: u32, height: u32) -> String {
        let colors = theme::colors();
        let frame = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
        frame.clear(colors.bg);
        alert.draw(&frame);
        frame.to_text(true)
    }

    #[test]
    fn ampersands_mark_the_letter_that_takes_a_button() {
        let button = Button::new("Do&n't Save", ());
        assert_eq!(button.label, "Don't Save");
        assert_eq!(button.mnemonic, Some((2, 'n')));
        assert_eq!(Button::new("Plain", ()).mnemonic, None);
    }

    #[test]
    fn keys_move_between_buttons_and_take_them() {
        let mut alert = alert();
        assert_eq!(
            alert.handle_key(key(KeyCode::Enter)),
            AlertAction::Answer("save")
        );
        alert.handle_key(key(KeyCode::Right));
        assert_eq!(
            alert.handle_key(key(KeyCode::Char(' '))),
            AlertAction::Answer("discard")
        );
        // Past Cancel, around to the first.
        alert.handle_key(key(KeyCode::Tab));
        assert_eq!(alert.handle_key(key(KeyCode::Enter)), AlertAction::Cancel);
        alert.handle_key(key(KeyCode::Right));
        alert.handle_key(key(KeyCode::Left));
        assert_eq!(alert.handle_key(key(KeyCode::Enter)), AlertAction::Cancel);
        assert_eq!(alert.handle_key(key(KeyCode::Esc)), AlertAction::Cancel);
    }

    #[test]
    fn a_buttons_letter_takes_it_in_either_case() {
        let mut alert = alert();
        assert_eq!(
            alert.handle_key(key(KeyCode::Char('N'))),
            AlertAction::Answer("discard")
        );
        assert_eq!(
            alert.handle_key(key(KeyCode::Char('x'))),
            AlertAction::Continue
        );
        let ctrl_s = Key::new(KeyCode::Char('s'), Mods::CTRL);
        assert_eq!(alert.handle_key(ctrl_s), AlertAction::Continue);
    }

    #[test]
    fn draws_a_box_with_the_buttons_right_aligned() {
        let _serial = crate::test_serial();
        let text = screen(&alert(), 80, 24);
        let lines: Vec<&str> = text.lines().collect();
        let top = lines
            .iter()
            .position(|l| l.contains("Close a.txt?"))
            .unwrap();
        assert!(lines[top].contains("╭─ Close a.txt? ─"), "{text}");
        assert!(
            lines[top + 2].contains("│ a.txt has unsaved changes."),
            "{text}"
        );
        assert!(
            lines[top + 4].contains(" Save    Don't Save    Cancel  │"),
            "{text}"
        );
        assert!(lines[top + 5].contains("╰"), "{text}");
    }

    #[test]
    fn clicks_take_buttons_and_outside_cancels() {
        let mut alert = alert();
        let area = alert.area();
        let row = area.y + area.height - 2;
        let spans = alert.button_spans(area);
        assert_eq!(
            alert.handle_mouse(click(spans[1].0, row)),
            AlertAction::Answer("discard")
        );
        assert_eq!(alert.selected, 1);
        assert_eq!(
            alert.handle_mouse(click(spans[2].0 + 1, row)),
            AlertAction::Cancel
        );
        assert_eq!(
            alert.handle_mouse(click(area.x + 1, area.y + 1)),
            AlertAction::Continue
        );
        assert_eq!(alert.handle_mouse(click(0, 0)), AlertAction::Cancel);
    }

    #[test]
    fn long_messages_wrap_and_tiny_screens_keep_the_buttons() {
        let _serial = crate::test_serial();
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);

        let message = "word ".repeat(40);
        let alert: Alert<&str> = Alert::new("Title", message.trim(), vec![], 80, 24);
        assert!(alert.lines().iter().all(|l| l.chars().count() <= MAX_TEXT));

        let mut alert = self::alert();
        alert.set_size(40, 6);
        let area = alert.area();
        assert!(area.x + area.width <= 40 && area.y + area.height <= 5);
        let text = screen(&alert, 40, 6);
        assert!(text.contains("Cancel"), "{text}");
    }
}
