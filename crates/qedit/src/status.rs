//! The status bar along the bottom of the screen. It's the active panel's:
//! where its cursor is and what its file is written in, a message after a
//! key press, or the "Save as" prompt. Shortcut hints fill the right.

use opentui::{Attributes, Buffer};

use crate::editor::{STATUS_BG, STATUS_DIM, STATUS_ERROR_BG, STATUS_FG};
use crate::keymap::{Command, Keymap};

/// What the status bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The cursor's position and the file's details.
    Info(String),
    /// Shown until the next key press.
    Message { text: String, error: bool },
    /// The "Save as" prompt, with what's been typed.
    Prompt(String),
}

impl Status {
    /// The status bar's text, `width` wide, as drawn, without hints.
    #[cfg(test)]
    pub fn text(&self) -> String {
        match self {
            Status::Info(info) => format!(" {info}"),
            Status::Message { text, .. } => format!(" {text}"),
            Status::Prompt(input) => format!("{PROMPT}{input}"),
        }
    }
}

const PROMPT: &str = " Save as: ";

/// Draws `status` across row `y` of `frame`, `width` wide. Returns where the
/// terminal cursor goes while the prompt is open.
pub fn draw(
    frame: &Buffer,
    status: &Status,
    y: u32,
    width: u32,
    keymap: &Keymap,
) -> Option<(u32, u32)> {
    match status {
        Status::Prompt(input) => {
            frame.fill_rect(0, y, width, 1, STATUS_BG);
            frame.draw_text(PROMPT, 0, y, STATUS_DIM, None, Attributes::NONE);
            let x = PROMPT.len() as u32;
            // Keep the end of a long path visible.
            let room = width.saturating_sub(x + 1) as usize;
            let chars: Vec<char> = input.chars().collect();
            let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
            frame.draw_text(&shown, x, y, STATUS_FG, None, Attributes::NONE);
            Some((x + shown.chars().count() as u32, y))
        }
        Status::Message { text, error } => {
            let bg = if *error { STATUS_ERROR_BG } else { STATUS_BG };
            frame.fill_rect(0, y, width, 1, bg);
            frame.draw_text(&format!(" {text}"), 0, y, STATUS_FG, None, Attributes::BOLD);
            None
        }
        Status::Info(info) => {
            frame.fill_rect(0, y, width, 1, STATUS_BG);
            let left = format!(" {info}");
            frame.draw_text(&left, 0, y, STATUS_FG, None, Attributes::NONE);
            let hints: String = [
                (Command::Save, "save"),
                (Command::FocusTree, "files"),
                (Command::Palette, "commands"),
                (Command::Quit, "quit"),
            ]
            .iter()
            .filter_map(|&(command, name)| {
                Some(format!("{:#} {name}  ", keymap.shortcut(command)?))
            })
            .collect();
            let hints = hints.strip_suffix(' ').unwrap_or(&hints);
            let hints_x = width.saturating_sub(hints.len() as u32);
            if hints_x as usize > left.chars().count() {
                frame.draw_text(hints, hints_x, y, STATUS_DIM, None, Attributes::NONE);
            }
            None
        }
    }
}
