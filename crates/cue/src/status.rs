//! The status bar along the bottom of the screen. It's the active panel's:
//! where its cursor is and what its file is written in, a message after a
//! key press, or a prompt, such as a terminal's new name. Shortcut hints
//! fill the right, and in a session, a badge saying so: a click on it
//! offers to detach or end the session. In a git repository, a badge at
//! the left end names the branch (see [`GitBadge`]).

use std::ops::Range;

use opentui::{Attributes, Buffer, Rgba};

use crate::icons;
use crate::input::{Key, KeyCode};
use crate::keymap::{Command, Keymap};
use crate::theme::{self, Hue};
use crate::tree::truncate;

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

/// The badge at the right end of the status bar in a session.
const SESSION_BADGE: &str = " session ";
/// Branch names past this many characters are cut short in the git badge.
const MAX_BRANCH: usize = 32;
/// The git badge shows only if it leaves this many columns for the rest.
const MIN_REST: u32 = 40;

/// The badge at the left end of the status bar in a git repository. A
/// click on it shows the changes in the sidebar, or goes back to the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitBadge {
    /// The branch, and if anything changed since the last commit, the lines
    /// added and removed.
    Branch {
        name: String,
        lines: Option<(usize, usize)>,
    },
    /// While the sidebar shows the changes: a button back to the files.
    Files,
}

impl GitBadge {
    /// What it says after the icon, in pieces, each with its color: its
    /// label, then the lines added, in green, and removed, in red.
    fn parts(&self) -> Vec<(String, Rgba)> {
        let colors = theme::colors();
        match self {
            GitBadge::Branch { name, lines } => {
                let mut parts = vec![(truncate(name, MAX_BRANCH), colors.text)];
                if let Some((added, removed)) = lines {
                    let (green, red) = (colors.hue(Hue::Green), colors.hue(Hue::Red));
                    parts.push((format!(" +{}", short_count(*added)), green));
                    parts.push((format!(" -{}", short_count(*removed)), red));
                }
                parts
            }
            GitBadge::Files => vec![("‹ Files".to_string(), colors.text)],
        }
    }

    fn icon(&self) -> bool {
        matches!(self, GitBadge::Branch { .. }) && icons::enabled()
    }

    fn width(&self) -> u32 {
        let icon = if self.icon() { icons::WIDTH } else { 0 };
        let text: usize = self
            .parts()
            .iter()
            .map(|(part, _)| part.chars().count())
            .sum();
        2 + icon + text as u32
    }

    /// Draws it from the left end of row `y`.
    fn draw(&self, frame: &Buffer, y: u32) {
        let colors = theme::colors();
        // A shade lighter than the status bar: toward the text on a dark
        // theme, and back toward the background on a light one.
        let lighter = match colors.light {
            true => theme::mix(colors.surface, colors.bg, 0.5),
            false => theme::mix(colors.surface, colors.text, 0.08),
        };
        frame.fill_rect(0, y, self.width(), 1, lighter);
        let mut x = 1;
        if self.icon() {
            x = icons::branch().draw(frame, x, y, None);
        }
        for (part, fg) in self.parts() {
            frame.draw_text(&part, x, y, fg, None, Attributes::NONE);
            x += part.chars().count() as u32;
        }
    }
}

/// `n` in at most about five columns: `12345` is `12k`.
fn short_count(n: usize) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..10_000_000 => format!("{}k", n / 1000),
        _ => format!("{}M", n / 1_000_000),
    }
}

/// The columns the git badge takes in a status bar `width` wide showing
/// `status`, if it shows `badge`: not over a message or a prompt, nor
/// where it would crowd out the rest. What the status bar says starts
/// where it ends.
pub fn git_badge(status: &Status, badge: Option<&GitBadge>, width: u32) -> Option<Range<u32>> {
    let badge = badge?;
    match status {
        Status::Info(_) | Status::Terminal(_) | Status::EditorInfo { .. }
            if width >= badge.width() + MIN_REST =>
        {
            Some(0..badge.width())
        }
        _ => None,
    }
}

/// The columns the session badge takes in a status bar `width` wide
/// showing `status`, if it shows one: not over a message or a prompt.
pub fn session_badge(status: &Status, width: u32) -> Option<Range<u32>> {
    let len = SESSION_BADGE.len() as u32;
    match status {
        Status::Info(_) | Status::Terminal(_) | Status::EditorInfo { .. } if width >= len * 2 => {
            Some(width - len..width)
        }
        _ => None,
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

/// Draws `status` across row `y` of `frame`, `width` wide, with the
/// session badge if `session`, and `git`'s badge if there's room. A
/// terminal's hint says whether cue's shortcuts (`terminal_cue_keys`) or
/// the shell gets keys. Returns where the terminal cursor goes while the
/// prompt is open.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &Buffer,
    status: &Status,
    y: u32,
    width: u32,
    keymap: &Keymap,
    session: bool,
    git: Option<&GitBadge>,
    terminal_cue_keys: bool,
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
            // In a terminal, whether cue's shortcuts or the shell get keys.
            let hints: &[(Command, &str)] = match status {
                Status::Terminal(_) if terminal_cue_keys => {
                    &[(Command::ToggleTerminalKeys, "keys: cue")]
                }
                Status::Terminal(_) => &[(Command::ToggleTerminalKeys, "keys: shell")],
                _ => &[(Command::Palette, "commands"), (Command::Quit, "quit")],
            };
            let hints: String = hints
                .iter()
                .filter_map(|&(command, name)| {
                    Some(format!("{:#} {name}  ", keymap.shortcut(command)?))
                })
                .collect();
            let hints = hints.strip_suffix(' ').unwrap_or(&hints);
            let badge = session_badge(status, width).filter(|_| session);
            // The hints end in a space; another sets them off the badge.
            let right = badge.as_ref().map_or(width, |badge| badge.start - 1);
            let hints_x = right.saturating_sub(hints.len() as u32);
            let x = match git.filter(|_| git_badge(status, git, width).is_some()) {
                Some(git) => {
                    git.draw(frame, y);
                    git.width()
                }
                None => 0,
            };
            let mut left = format!(" {info}");
            // The title a shell sets can run long; the hint is worth more.
            if let Status::Terminal(_) = status {
                left = truncate(&left, hints_x.saturating_sub(x + 1) as usize);
            }
            frame.draw_text(&left, x, y, colors.text, None, Attributes::NONE);
            if hints_x as usize > x as usize + left.chars().count() {
                // Stands out while the shell has every key.
                let fg = match status {
                    Status::Terminal(_) if !terminal_cue_keys => colors.accent,
                    _ => colors.muted,
                };
                frame.draw_text(hints, hints_x, y, fg, None, Attributes::NONE);
            }
            if let Some(badge) = badge {
                let (fg, bg) = (colors.on_accent, Some(colors.accent));
                frame.draw_text(SESSION_BADGE, badge.start, y, fg, bg, Attributes::BOLD);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_counts_are_short() {
        let counts = [0, 9_999, 10_000, 123_456, 12_345_678].map(short_count);
        assert_eq!(counts, ["0", "9999", "10k", "123k", "12M"]);
    }
}
