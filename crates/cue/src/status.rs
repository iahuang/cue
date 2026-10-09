//! The status bar along the bottom of the screen. It's the active panel's:
//! where its cursor is and what its file is written in, or what's wrong,
//! such as a query that isn't a valid regex. A click on an editor's
//! position, indentation, language, or wrapping changes it (see
//! [`StatusButton`]). Shortcut hints
//! fill the right, and in a session, a badge with its name, or if it has
//! none, its ID: a click on it offers to detach or end the session. In a
//! git repository, a badge at the left end names the branch (see
//! [`GitBadge`]).

use std::ops::Range;

use opentui::{Attributes, Buffer, Rgba};
use unicode_width::UnicodeWidthStr;

use crate::icons;
use crate::keymap::{Command, Keymap};
use crate::theme::{self, Hue};
use crate::tree::truncate;

/// A part of an editor's status that a click acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusButton {
    /// The cursor's line and column: goes to a line.
    Position,
    /// How the file indents: changes it.
    Indent,
    /// What the file is written in: changes its highlighting.
    Language,
    /// Whether lines wrap: toggles it.
    Wrap,
}

/// What the status bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The cursor's position and the file's details.
    Info(String),
    /// The cursor's position and the file's details, with the screen
    /// columns of the parts a click acts on, offset by the leading space.
    EditorInfo {
        text: String,
        buttons: Vec<(StatusButton, Range<u32>)>,
    },
    /// A terminal's name and what it's running.
    Terminal(String),
    /// What's wrong, or what to do next, while it's so.
    Message { text: String, error: bool },
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
        }
    }

    /// The button at column `x`, counted from where the text starts.
    pub fn button_at(&self, x: u32) -> Option<StatusButton> {
        let Status::EditorInfo { buttons, .. } = self else {
            return None;
        };
        buttons
            .iter()
            .find(|(_, columns)| columns.contains(&x))
            .map(|(button, _)| *button)
    }

    /// The columns `button` takes, counted from where the text starts.
    pub fn button(&self, button: StatusButton) -> Option<Range<u32>> {
        let Status::EditorInfo { buttons, .. } = self else {
            return None;
        };
        buttons
            .iter()
            .find(|(b, _)| *b == button)
            .map(|(_, columns)| columns.clone())
    }
}

/// Session names past this many characters are cut short in its badge.
const MAX_SESSION: usize = 24;
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
/// `status`, if it shows `badge`: not over a message, nor
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

/// What the badge at the right end of the status bar says for a session
/// labeled `session`.
fn session_text(session: &str) -> String {
    format!(" {} ", truncate(session, MAX_SESSION))
}

/// The columns the badge for a session labeled `session` takes in a
/// status bar `width` wide showing `status`, if it shows one: not over
/// a message.
pub fn session_badge(status: &Status, session: Option<&str>, width: u32) -> Option<Range<u32>> {
    let len = session_text(session?).width() as u32;
    match status {
        Status::Info(_) | Status::Terminal(_) | Status::EditorInfo { .. } if width >= len * 2 => {
            Some(width - len..width)
        }
        _ => None,
    }
}

/// Draws `status` across row `y` of `frame`, `width` wide, with the
/// badge labeled `session` if in a session, and `git`'s badge if
/// there's room. A terminal's hint says whether cue's shortcuts
/// (`terminal_cue_keys`) or the shell gets keys.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &Buffer,
    status: &Status,
    y: u32,
    width: u32,
    keymap: &Keymap,
    session: Option<&str>,
    git: Option<&GitBadge>,
    terminal_cue_keys: bool,
) {
    let colors = theme::colors();
    match status {
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
            let badge = session_badge(status, session, width);
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
            if let (Some(badge), Some(session)) = (badge, session) {
                let (fg, bg) = (colors.on_accent, Some(colors.accent));
                let text = session_text(session);
                frame.draw_text(&text, badge.start, y, fg, bg, Attributes::BOLD);
            }
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
