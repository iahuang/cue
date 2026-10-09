//! The input box: a popup asking for one line of text, such as a new name
//! for a tab, where the picker opens, as in VS Code's input box. The name
//! it starts with is selected, so typing replaces it.
//!
//! Enter takes what's typed and Esc cancels; the app sends those, and the
//! editor's keys for moving, selecting, and deleting go to the field. A
//! press in the field puts the cursor there, and a click outside cancels.

use std::time::Instant;

use opentui::{Attributes, Buffer};

use crate::input::{Mouse, MouseButton, MouseKind};
use crate::line_edit::{self, Caret, Edit};
use crate::picker::{self, Area};
use crate::theme;

/// The widest it gets.
const MAX_WIDTH: u32 = 60;

/// What the app should do after the box handled the mouse.
#[derive(Debug, PartialEq, Eq)]
pub enum InputAction {
    Continue,
    Cancel,
}

pub struct InputBox {
    title: &'static str,
    /// Shown, dimmed, while the field is empty: what an empty one means.
    placeholder: &'static str,
    text: String,
    caret: Caret,
    screen_width: u32,
    screen_height: u32,
}

impl InputBox {
    /// A box titled `title` ("Rename Tab"), starting with `text` selected,
    /// on a screen `width` x `height`.
    pub fn new(
        title: &'static str,
        placeholder: &'static str,
        text: &str,
        width: u32,
        height: u32,
    ) -> InputBox {
        let mut caret = Caret::default();
        caret.select_all(text);
        InputBox {
            title,
            placeholder,
            text: text.to_string(),
            caret,
            screen_width: width,
            screen_height: height,
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
    }

    /// What's typed, trimmed.
    pub fn text(&self) -> &str {
        self.text.trim()
    }

    pub fn edit(&mut self, edit: Edit) {
        self.caret.edit(&mut self.text, edit);
    }

    /// An edit with Shift held: moving the cursor selects.
    pub fn edit_selecting(&mut self, edit: Edit) {
        self.caret.select(&mut self.text, edit);
    }

    pub fn select_all(&mut self) {
        self.caret.select_all(&self.text);
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.caret.selected_text(&self.text)
    }

    /// Takes the first line of `text`, as typed.
    pub fn paste(&mut self, text: &str) {
        // Terminals send newlines in pastes as CR.
        let line = text.split(['\r', '\n']).next().unwrap_or("");
        self.edit(Edit::Insert(line));
    }

    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> InputAction {
        let area = self.area();
        let column = |x: u32| x.saturating_sub(area.x + 2) as usize;
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) if area.contains(mouse.x, mouse.y) => {
                if mouse.y == area.y + 1 {
                    self.caret.press(&self.text, column(mouse.x), now);
                }
            }
            MouseKind::Press(_) if !area.contains(mouse.x, mouse.y) => return InputAction::Cancel,
            MouseKind::Drag(MouseButton::Left) if self.caret.pressed() => {
                self.caret.drag(&self.text, column(mouse.x));
            }
            MouseKind::Release(_) => {
                self.caret.release(&self.text);
            }
            _ => {}
        }
        InputAction::Continue
    }

    /// Draws it over the top middle of the screen and returns where the
    /// terminal cursor goes. Draws nothing on a screen too small for it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let area = self.area();
        if area.width < 8 || area.height < 3 {
            return None;
        }
        Some(
            frame.with_clip(area.x, area.y, area.width, area.height, || {
                self.draw_box(frame, area)
            }),
        )
    }

    /// Where the picker would be, three rows high: borders and the field.
    pub fn area(&self) -> Area {
        let width = self
            .screen_width
            .saturating_sub(4)
            .min(MAX_WIDTH)
            .max(self.screen_width.min(24));
        Area {
            x: (self.screen_width - width) / 2,
            y: self.screen_height.min(1),
            width,
            height: 3.min(self.screen_height),
        }
    }

    fn draw_box(&self, frame: &Buffer, area: Area) -> (u32, u32) {
        let colors = theme::colors();
        picker::draw_frame(frame, area, self.title);
        let (x, y) = (area.x + 2, area.y + 1);
        let room = area.width.saturating_sub(4) as usize;
        let (shown, column) = self.caret.view(&self.text, room);
        line_edit::draw_selection(frame, &self.caret, &self.text, x, y, room);
        frame.draw_text(&shown, x, y, colors.text, None, Attributes::NONE);
        if self.text.is_empty() {
            let placeholder: String = self.placeholder.chars().take(room - 1).collect();
            frame.draw_text(&placeholder, x + 1, y, colors.muted, None, Attributes::NONE);
        }
        (x + column as u32, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;
    use opentui::{OwnedBuffer, WidthMethod};

    fn screen(input: &InputBox) -> String {
        let colors = theme::colors();
        let frame = OwnedBuffer::new(80, 24, false, WidthMethod::Unicode, "test").unwrap();
        frame.clear(colors.bg);
        input.draw(&frame);
        frame.to_text(true)
    }

    fn press(x: u32, y: u32) -> Mouse {
        Mouse {
            kind: MouseKind::Press(MouseButton::Left),
            x,
            y,
            mods: Mods::NONE,
        }
    }

    #[test]
    fn typing_replaces_the_name_it_starts_with() {
        let mut input = InputBox::new("Rename Tab", "", "build", 80, 24);
        assert_eq!(input.selected_text(), Some("build"));
        input.edit(Edit::Insert("t"));
        input.edit(Edit::Insert("ests "));
        assert_eq!(input.text(), "tests");
        input.edit(Edit::WordLeft);
        input.edit(Edit::Insert("unit "));
        assert_eq!(input.text(), "unit tests");
        input.paste("e2e\rignored");
        assert_eq!(input.text(), "unit e2etests");
    }

    #[test]
    fn draws_a_titled_box_and_the_placeholder_while_empty() {
        let _serial = crate::test_serial();
        let mut input = InputBox::new("Rename Tab", "Leave empty to unname it", "build", 80, 24);
        let text = screen(&input);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains("╭─ Rename Tab ─"), "{text}");
        assert!(lines[2].contains("│ build"), "{text}");
        assert!(lines[3].contains("╰"), "{text}");
        input.edit(Edit::DeleteBackward);
        let text = screen(&input);
        assert!(text.contains("Leave empty to unname it"), "{text}");
    }

    #[test]
    fn a_click_outside_cancels_and_inside_moves_the_cursor() {
        let mut input = InputBox::new("Rename Tab", "", "build", 80, 24);
        let area = input.area();
        let now = Instant::now();
        let field = press(area.x + 3, area.y + 1);
        assert_eq!(input.handle_mouse(field, now), InputAction::Continue);
        assert_eq!(input.selected_text(), None);
        input.edit(Edit::Insert("-"));
        assert_eq!(input.text(), "b-uild");
        assert_eq!(input.handle_mouse(press(0, 20), now), InputAction::Cancel);
    }
}
