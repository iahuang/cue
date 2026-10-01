//! The status bar along the bottom of the screen. It's the active panel's:
//! where its cursor is and what its file is written in, a message after a
//! key press, or a prompt, such as a terminal's new name. Shortcut hints
//! fill the right.

use opentui::{Attributes, Buffer};

use crate::input::{Key, KeyCode};
use crate::keymap::{Command, Keymap};
use crate::theme;

/// What the status bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The cursor's position and the file's details.
    Info(String),
    /// File details and the language's screen columns, offset by the leading space.
    EditorInfo {
        text: String,
        language: std::ops::Range<u32>,
    },
    /// A terminal's name and what it's running.
    Terminal(String),
    /// Shown until the next key press.
    Message { text: String, error: bool },
    /// A prompt, with what's been typed.
    Prompt { label: &'static str, input: String },
}

impl Status {
    /// The status bar's text, `width` wide, as drawn, without hints.
    #[cfg(test)]
    pub fn text(&self) -> String {
        match self {
            Status::Info(info) | Status::Terminal(info) | Status::EditorInfo { text: info, .. } => {
                format!(" {info}")
            }
            Status::Message { text, .. } => format!(" {text}"),
            Status::Prompt { label, input } => format!(" {label}: {input}"),
        }
    }
}

/// A line of text asked for in the status bar, such as a terminal's name.
pub struct Prompt {
    label: &'static str,
    input: String,
}

/// What a key did to a [`Prompt`].
#[derive(Debug, PartialEq, Eq)]
pub enum PromptKey {
    Continue,
    Cancel,
    /// Enter, with what was typed, trimmed.
    Submit(String),
}

impl Prompt {
    /// A prompt labeled `label` ("Rename terminal"), starting with `input`
    /// typed.
    pub fn new(label: &'static str, input: &str) -> Prompt {
        Prompt {
            label,
            input: input.to_string(),
        }
    }

    pub fn handle_key(&mut self, Key { code, mods }: Key) -> PromptKey {
        match code {
            KeyCode::Esc => return PromptKey::Cancel,
            KeyCode::Char('c' | 'q') if mods.ctrl || mods.sup => return PromptKey::Cancel,
            KeyCode::Enter => return PromptKey::Submit(self.input.trim().to_string()),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if mods.is_plain() => self.input.push(c),
            _ => {}
        }
        PromptKey::Continue
    }

    /// Takes the first line of `text`.
    pub fn paste(&mut self, text: &str) {
        // Terminals send newlines in pastes as CR.
        self.input
            .push_str(text.split(['\r', '\n']).next().unwrap_or(""));
    }

    pub fn status(&self) -> Status {
        Status::Prompt {
            label: self.label,
            input: self.input.clone(),
        }
    }
}

/// Draws `status` across row `y` of `frame`, `width` wide. Returns where the
/// terminal cursor goes while the prompt is open.
pub fn draw(
    frame: &Buffer,
    status: &Status,
    y: u32,
    width: u32,
    keymap: &Keymap,
) -> Option<(u32, u32)> {
    let colors = theme::colors();
    match status {
        Status::Prompt { label, input } => {
            frame.fill_rect(0, y, width, 1, colors.surface);
            let label = format!(" {label}: ");
            frame.draw_text(&label, 0, y, colors.muted, None, Attributes::NONE);
            let x = label.chars().count() as u32;
            // Keep the end of a long path visible.
            let room = width.saturating_sub(x + 1) as usize;
            let chars: Vec<char> = input.chars().collect();
            let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
            frame.draw_text(&shown, x, y, colors.text, None, Attributes::NONE);
            Some((x + shown.chars().count() as u32, y))
        }
        Status::Message { text, error } => {
            let bg = if *error {
                colors.error_bg
            } else {
                colors.surface
            };
            frame.fill_rect(0, y, width, 1, bg);
            frame.draw_text(
                &format!(" {text}"),
                0,
                y,
                colors.text,
                None,
                Attributes::BOLD,
            );
            None
        }
        Status::Info(info) | Status::Terminal(info) | Status::EditorInfo { text: info, .. } => {
            frame.fill_rect(0, y, width, 1, colors.surface);
            // In a terminal, those keys are the shell's.
            let hints: &[(Command, &str)] = match status {
                Status::Terminal(_) => &[(Command::TerminalPrefix, "cue keys")],
                _ => &[
                    (Command::Save, "save"),
                    (Command::FocusTree, "files"),
                    (Command::Palette, "commands"),
                    (Command::Quit, "quit"),
                ],
            };
            let hints: String = hints
                .iter()
                .filter_map(|&(command, name)| {
                    Some(format!("{:#} {name}  ", keymap.shortcut(command)?))
                })
                .collect();
            let hints = hints.strip_suffix(' ').unwrap_or(&hints);
            let hints_x = width.saturating_sub(hints.len() as u32);
            let mut left = format!(" {info}");
            // The title a shell sets can run long; the hint is worth more.
            if let Status::Terminal(_) = status {
                left = crate::tree::truncate(&left, (hints_x as usize).saturating_sub(1));
            }
            frame.draw_text(&left, 0, y, colors.text, None, Attributes::NONE);
            if hints_x as usize > left.chars().count() {
                frame.draw_text(hints, hints_x, y, colors.muted, None, Attributes::NONE);
            }
            None
        }
    }
}
