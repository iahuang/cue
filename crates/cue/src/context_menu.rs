//! A context menu: a short list of commands for one thing, popped up where
//! it was right-clicked, or from the keyboard (Shift+F10).
//!
//! As in the menus of macOS, a right click opens it and a left click picks
//! from it, or the right button can be held, dragged to an item, and
//! released on it. Up and Down, Enter, and Esc work too. A click outside
//! closes it; a right click outside opens another one there instead.

use opentui::{Attributes, Buffer};

use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::{Command, Keymap};
use crate::picker::Area;
use crate::theme;

/// A row of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// A command, labeled for the menu.
    Command(Command, String),
    /// A rule between groups of commands.
    Separator,
}

/// What the app should do after the menu handled input.
#[derive(Debug, PartialEq, Eq)]
pub enum MenuAction {
    Continue,
    Close,
    /// Close the menu, and handle the mouse event as if it hadn't been open.
    CloseAndPass,
    Accept(Command),
}

pub struct ContextMenu {
    items: Vec<MenuItem>,
    /// Shortcut labels, by item.
    shortcuts: Vec<String>,
    /// The cell it was opened at.
    x: u32,
    y: u32,
    selected: Option<usize>,
    /// The right button went down to open it and hasn't come up yet, nor
    /// left the cell it went down on.
    right_held: bool,
    screen_width: u32,
    screen_height: u32,
}

impl ContextMenu {
    /// A menu of `items` opened at cell (`x`, `y`), on a screen `width` x
    /// `height`. From the keyboard, the first item starts selected; from
    /// the mouse, the right button is still down.
    pub fn new(
        items: Vec<MenuItem>,
        keymap: &Keymap,
        x: u32,
        y: u32,
        from_mouse: bool,
        width: u32,
        height: u32,
    ) -> ContextMenu {
        let shortcuts = items
            .iter()
            .map(|item| match item {
                MenuItem::Command(command, _) => keymap
                    .shortcut(*command)
                    .map_or(String::new(), |key| key.to_string()),
                MenuItem::Separator => String::new(),
            })
            .collect();
        let mut menu = ContextMenu {
            items,
            shortcuts,
            x,
            y,
            selected: None,
            right_held: from_mouse,
            screen_width: width,
            screen_height: height,
        };
        if !from_mouse {
            menu.step(1);
        }
        menu
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
    }

    /// Runs a picker command: moving the selection, taking it, or closing.
    pub fn run(&mut self, command: Command) -> MenuAction {
        match command {
            Command::PickerUp => self.step(-1),
            Command::PickerDown => self.step(1),
            Command::PickerPageUp => self.select_end(false),
            Command::PickerPageDown => self.select_end(true),
            Command::PickerAccept => return self.accept(),
            Command::PickerClose => return MenuAction::Close,
            _ => {}
        }
        MenuAction::Continue
    }

    pub fn handle_mouse(&mut self, mouse: Mouse) -> MenuAction {
        let area = self.area();
        let item = self.item_at(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) if !area.contains(mouse.x, mouse.y) => {
                MenuAction::Close
            }
            MouseKind::Press(_) | MouseKind::ScrollUp | MouseKind::ScrollDown
                if !area.contains(mouse.x, mouse.y) =>
            {
                MenuAction::CloseAndPass
            }
            MouseKind::Press(MouseButton::Left) => match item {
                Some(index) => {
                    self.selected = Some(index);
                    self.accept()
                }
                None => MenuAction::Continue,
            },
            MouseKind::Drag(_) => {
                if (mouse.x, mouse.y) != (self.x, self.y) {
                    self.right_held = false;
                }
                self.selected = item;
                MenuAction::Continue
            }
            // Released where it went down, the right button only opened the
            // menu; dragged to an item, it takes it.
            MouseKind::Release(MouseButton::Right) => {
                let opened = std::mem::take(&mut self.right_held);
                match item {
                    Some(index) if !opened => {
                        self.selected = Some(index);
                        self.accept()
                    }
                    _ => MenuAction::Continue,
                }
            }
            _ => MenuAction::Continue,
        }
    }

    /// Draws the menu next to where it was opened.
    pub fn draw(&self, frame: &Buffer) {
        let area = self.area();
        if area.width < 4 || area.height < 3 {
            return;
        }
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            self.draw_menu(frame, area)
        });
    }

    // --- selection --------------------------------------------------------------

    /// Moves the selection `step` commands, around the ends, skipping rules.
    fn step(&mut self, step: isize) {
        let count = self.items.len() as isize;
        if count == 0 {
            return;
        }
        let mut index = match self.selected {
            Some(selected) => selected as isize,
            None if step > 0 => -1,
            None => count,
        };
        for _ in 0..count {
            index = (index + step.signum()).rem_euclid(count);
            if self.items[index as usize] != MenuItem::Separator {
                self.selected = Some(index as usize);
                return;
            }
        }
    }

    /// Selects the last command, or the first.
    fn select_end(&mut self, last: bool) {
        self.selected = None;
        self.step(if last { -1 } else { 1 });
    }

    fn accept(&mut self) -> MenuAction {
        match self.selected.and_then(|index| self.items.get(index)) {
            Some(MenuItem::Command(command, _)) => MenuAction::Accept(*command),
            _ => MenuAction::Continue,
        }
    }

    /// The command at cell (`x`, `y`), by position.
    fn item_at(&self, x: u32, y: u32) -> Option<usize> {
        let area = self.area();
        let inside = x > area.x && x + 1 < area.x + area.width;
        if !inside || y <= area.y {
            return None;
        }
        let index = (y - area.y - 1) as usize;
        match self.items.get(index) {
            Some(MenuItem::Command(..)) => Some(index),
            _ => None,
        }
    }

    // --- layout and drawing -----------------------------------------------------

    /// Right of and below where it was opened, or moved left or up to stay
    /// on screen, above the status bar.
    fn area(&self) -> Area {
        let widest = self
            .items
            .iter()
            .zip(&self.shortcuts)
            .map(|(item, shortcut)| match item {
                MenuItem::Command(_, label) => {
                    let gap = if shortcut.is_empty() { 0 } else { 4 };
                    label.chars().count() + gap + shortcut.chars().count()
                }
                MenuItem::Separator => 0,
            })
            .max()
            .unwrap_or(0) as u32;
        // The borders, and a space inside each.
        let width = (widest + 4).min(self.screen_width);
        let room = self.screen_height.saturating_sub(1);
        let height = (self.items.len() as u32 + 2).min(room);
        let x = self.x.min(self.screen_width - width);
        // Below the cell clicked, so the row stays in sight; failing that,
        // above it; failing that, as low as it fits.
        let y = if self.y + 1 + height <= room {
            self.y + 1
        } else if self.y >= height {
            self.y - height
        } else {
            room - height
        };
        Area {
            x,
            y,
            width,
            height,
        }
    }

    fn draw_menu(&self, frame: &Buffer, area: Area) {
        let colors = theme::colors();
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        frame.fill_rect(x, y, width, height, colors.bg);
        let inner = width.saturating_sub(2) as usize;
        let rule = "─".repeat(inner);
        let text =
            |s: &str, x: u32, y: u32, fg| frame.draw_text(s, x, y, fg, None, Attributes::NONE);
        text(&format!("╭{rule}╮"), x, y, colors.border);
        text(&format!("╰{rule}╯"), x, y + height - 1, colors.border);
        let rows = self.items.iter().zip(&self.shortcuts).enumerate();
        for ((index, (item, shortcut)), row) in rows.zip(y + 1..y + height - 1) {
            let MenuItem::Command(_, label) = item else {
                text(&format!("├{rule}┤"), x, row, colors.border);
                continue;
            };
            text("│", x, row, colors.border);
            text("│", x + width - 1, row, colors.border);
            if self.selected == Some(index) {
                frame.fill_rect(x + 1, row, width - 2, 1, colors.selected);
            }
            text(label, x + 2, row, colors.text);
            let shortcut_x = (x + width).saturating_sub(shortcut.chars().count() as u32 + 2);
            text(shortcut, shortcut_x, row, colors.muted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn menu(from_mouse: bool) -> ContextMenu {
        let items = vec![
            MenuItem::Command(Command::TreeOpen, "Open".into()),
            MenuItem::Separator,
            MenuItem::Command(Command::TreeRename, "Rename…".into()),
            MenuItem::Command(Command::TreeTrash, "Move to Trash".into()),
        ];
        ContextMenu::new(items, &Keymap::default(), 10, 2, from_mouse, 80, 24)
    }

    fn mouse(kind: MouseKind, x: u32, y: u32) -> Mouse {
        Mouse {
            kind,
            x,
            y,
            mods: Mods::NONE,
        }
    }

    #[test]
    fn keys_skip_rules_and_wrap_around() {
        let mut menu = menu(false);
        assert_eq!(menu.selected, Some(0), "from the keyboard, the first");
        menu.run(Command::PickerDown);
        assert_eq!(menu.selected, Some(2), "past the rule");
        menu.run(Command::PickerDown);
        menu.run(Command::PickerDown);
        assert_eq!(menu.selected, Some(0), "around the end");
        menu.run(Command::PickerUp);
        assert_eq!(
            menu.run(Command::PickerAccept),
            MenuAction::Accept(Command::TreeTrash)
        );
        assert_eq!(menu.run(Command::PickerClose), MenuAction::Close);
    }

    #[test]
    fn a_right_click_opens_it_and_a_left_click_picks() {
        let mut menu = menu(true);
        assert_eq!(menu.selected, None);
        // The menu's top border is under the cell clicked; items follow.
        let release = mouse(MouseKind::Release(MouseButton::Right), 10, 2);
        assert_eq!(menu.handle_mouse(release), MenuAction::Continue);
        let rule = mouse(MouseKind::Press(MouseButton::Left), 12, 5);
        assert_eq!(menu.handle_mouse(rule), MenuAction::Continue);
        let rename = mouse(MouseKind::Press(MouseButton::Left), 12, 6);
        assert_eq!(
            menu.handle_mouse(rename),
            MenuAction::Accept(Command::TreeRename)
        );
    }

    #[test]
    fn dragging_with_the_right_button_picks_on_release() {
        let mut menu = menu(true);
        menu.handle_mouse(mouse(MouseKind::Drag(MouseButton::Right), 12, 4));
        assert_eq!(menu.selected, Some(0));
        let release = mouse(MouseKind::Release(MouseButton::Right), 12, 7);
        assert_eq!(
            menu.handle_mouse(release),
            MenuAction::Accept(Command::TreeTrash)
        );
    }

    #[test]
    fn clicks_outside_close_it() {
        let mut menu = menu(true);
        let left = mouse(MouseKind::Press(MouseButton::Left), 0, 0);
        assert_eq!(menu.handle_mouse(left), MenuAction::Close);
        let right = mouse(MouseKind::Press(MouseButton::Right), 0, 0);
        assert_eq!(menu.handle_mouse(right), MenuAction::CloseAndPass);
    }

    #[test]
    fn it_stays_on_screen() {
        let items = vec![MenuItem::Command(Command::TreeOpen, "Open".into())];
        let menu = ContextMenu::new(items, &Keymap::default(), 79, 23, true, 80, 24);
        let area = menu.area();
        assert!(area.x + area.width <= 80);
        assert!(area.y + area.height <= 23, "above the status bar");
    }
}
