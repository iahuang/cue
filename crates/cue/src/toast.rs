//! Toasts: messages, such as that a file was saved or why it couldn't be,
//! stacked in a corner of the screen (`ui.toast_position`), between the
//! tab bar and the status bar, the newest nearest the corner. They don't
//! take the keyboard. Each goes after a few seconds, an error after a few
//! more, or when it's clicked.

use std::time::{Duration, Instant};

use opentui::{Attributes, Buffer};

use crate::alert;
use crate::config;
use crate::picker::Area;
use crate::theme;

/// How long a message stays.
const SHOWN: Duration = Duration::from_secs(3);
/// How long an error stays.
const ERROR_SHOWN: Duration = Duration::from_secs(8);
/// The most shown at once; a newer one pushes the oldest out.
const MAX: usize = 3;
/// The widest a line of one gets before wrapping.
const MAX_TEXT: usize = 48;
/// The most lines one shows.
const MAX_LINES: usize = 4;

/// The corner toasts stack in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

impl ToastPosition {
    /// The position named `name`, as in the settings, such as
    /// `"bottom-right"`.
    pub fn named(name: &str) -> Option<ToastPosition> {
        Some(match name {
            "top-left" => ToastPosition::TopLeft,
            "top-right" => ToastPosition::TopRight,
            "bottom-left" => ToastPosition::BottomLeft,
            "bottom-right" => ToastPosition::BottomRight,
            _ => return None,
        })
    }

    fn top(self) -> bool {
        matches!(self, ToastPosition::TopLeft | ToastPosition::TopRight)
    }

    fn left(self) -> bool {
        matches!(self, ToastPosition::TopLeft | ToastPosition::BottomLeft)
    }
}

struct Toast {
    text: String,
    error: bool,
    /// When it goes.
    until: Instant,
    /// How many times in a row it was shown.
    count: u32,
}

impl Toast {
    /// What it says, with how many times if more than once.
    fn label(&self) -> String {
        match self.count {
            1 => self.text.clone(),
            count => format!("{} (×{count})", self.text),
        }
    }
}

#[derive(Default)]
pub struct Toasts {
    /// Oldest first.
    toasts: Vec<Toast>,
}

impl Toasts {
    /// Shows `text` from `now`. The same text as the newest shows that one
    /// again instead.
    pub fn push(&mut self, text: String, error: bool, now: Instant) {
        let until = now + if error { ERROR_SHOWN } else { SHOWN };
        if let Some(last) = self.toasts.last_mut() {
            if last.text == text && last.error == error {
                last.count += 1;
                last.until = until;
                return;
            }
        }
        self.toasts.push(Toast {
            text,
            error,
            until,
            count: 1,
        });
        if self.toasts.len() > MAX {
            self.toasts.remove(0);
        }
    }

    /// Drops those whose time is up by `now`. Returns whether any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let len = self.toasts.len();
        self.toasts.retain(|toast| toast.until > now);
        self.toasts.len() != len
    }

    /// The toast at (`x`, `y`), stacked `within` an area, if any.
    pub fn at(&self, x: u32, y: u32, within: Area) -> Option<usize> {
        self.areas(within)
            .into_iter()
            .position(|(_, area)| area.contains(x, y))
            .map(|index| self.toasts.len() - 1 - index)
    }

    /// The area of toast `index`, as [`Toasts::at`] says, stacked `within`
    /// an area.
    pub fn area(&self, index: usize, within: Area) -> Option<Area> {
        let areas = self.areas(within);
        let from_newest = self.toasts.len().checked_sub(index + 1)?;
        areas.get(from_newest).map(|(_, area)| *area)
    }

    pub fn dismiss(&mut self, index: usize) {
        if index < self.toasts.len() {
            self.toasts.remove(index);
        }
    }

    #[cfg(test)]
    pub fn texts(&self) -> Vec<&str> {
        self.toasts
            .iter()
            .map(|toast| toast.text.as_str())
            .collect()
    }

    /// Each toast's lines and where it goes, newest first, stacked
    /// `within` an area from the corner `ui.toast_position` says, a column
    /// in from the side, as many as fit.
    fn areas(&self, within: Area) -> Vec<(Vec<String>, Area)> {
        let position = config::get().toast_position;
        let room = (within.width.saturating_sub(6) as usize).min(MAX_TEXT);
        if room < 8 {
            return Vec::new();
        }
        // How much of `within` is still free, from the top.
        let (mut top, mut bottom) = (within.y, within.y + within.height);
        let mut areas = Vec::new();
        for toast in self.toasts.iter().rev() {
            let mut lines = alert::wrap(&toast.label(), room);
            if lines.len() > MAX_LINES {
                lines.truncate(MAX_LINES);
                let last = &mut lines[MAX_LINES - 1];
                if last.chars().count() >= room {
                    last.pop();
                }
                last.push('…');
            }
            let inner = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u32;
            let (w, h) = (inner + 4, lines.len() as u32 + 2);
            if bottom - top < h {
                break;
            }
            let y = match position.top() {
                true => top,
                false => bottom - h,
            };
            let x = match position.left() {
                true => within.x + 1,
                false => within.x + within.width - w - 1,
            };
            let area = Area {
                x,
                y,
                width: w,
                height: h,
            };
            areas.push((lines, area));
            match position.top() {
                true => top += h,
                false => bottom -= h,
            }
        }
        areas
    }

    pub fn draw(&self, frame: &Buffer, within: Area) {
        let colors = theme::colors();
        for (index, (lines, area)) in self.areas(within).into_iter().enumerate() {
            let toast = &self.toasts[self.toasts.len() - 1 - index];
            let Area {
                x,
                y,
                width,
                height,
            } = area;
            let border = match toast.error {
                true => colors.error,
                false => colors.border,
            };
            frame.fill_rect(x, y, width, height, colors.bg);
            let rule = "─".repeat(width as usize - 2);
            let draw = |text: &str, x, y, fg| {
                frame.draw_text(text, x, y, fg, None, Attributes::NONE);
            };
            draw(&format!("╭{rule}╮"), x, y, border);
            draw(&format!("╰{rule}╯"), x, y + height - 1, border);
            for (row, line) in (y + 1..).zip(&lines) {
                draw("│", x, row, border);
                draw("│", x + width - 1, row, border);
                draw(line, x + 2, row, colors.text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentui::{OwnedBuffer, WidthMethod};

    #[test]
    fn toasts_go_in_time_and_errors_stay_longer() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("Saved.".into(), false, now);
        toasts.push("Can't save.".into(), true, now);
        assert!(!toasts.expire(now + Duration::from_secs(1)));
        assert!(toasts.expire(now + SHOWN));
        assert_eq!(toasts.texts(), ["Can't save."]);
        assert!(toasts.expire(now + ERROR_SHOWN));
        assert!(toasts.texts().is_empty());
    }

    #[test]
    fn repeats_count_up_and_the_oldest_make_way() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("Nothing to undo.".into(), false, now);
        toasts.push(
            "Nothing to undo.".into(),
            false,
            now + Duration::from_secs(2),
        );
        assert_eq!(toasts.texts(), ["Nothing to undo."]);
        assert_eq!(toasts.toasts[0].label(), "Nothing to undo. (×2)");
        // Shown again, it stays from then.
        assert!(!toasts.expire(now + SHOWN));
        for n in 0..MAX {
            toasts.push(format!("{n}"), false, now);
        }
        assert_eq!(toasts.texts(), ["0", "1", "2"]);
    }

    /// A 40 x 12 screen, less the status bar.
    const SCREEN: Area = Area {
        x: 0,
        y: 0,
        width: 40,
        height: 11,
    };

    #[test]
    fn stacked_above_the_status_bar_newest_lowest_and_clicked_away() {
        let _serial = crate::test_serial();
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("First".into(), false, now);
        toasts.push("Second".into(), false, now);
        let frame = OwnedBuffer::new(40, 12, false, WidthMethod::Unicode, "test").unwrap();
        frame.clear(theme::colors().bg);
        toasts.draw(&frame, SCREEN);
        let text = frame.to_text(true);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[9].trim_end().ends_with("│ Second │"), "{text}");
        assert!(lines[10].trim_end().ends_with("╰────────╯"), "{text}");
        assert!(lines[6].trim_end().ends_with("│ First │"), "{text}");

        assert_eq!(toasts.at(36, 9, SCREEN), Some(1));
        assert_eq!(toasts.at(36, 6, SCREEN), Some(0));
        assert_eq!(toasts.at(2, 9, SCREEN), None);
        let area = toasts.area(1, SCREEN).unwrap();
        assert_eq!((area.y, area.height), (8, 3));
        toasts.dismiss(1);
        assert_eq!(toasts.texts(), ["First"]);
    }

    #[test]
    fn stacked_from_the_corner_the_settings_say() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("First".into(), false, now);
        toasts.push("Second".into(), false, now);
        // Below a tab bar.
        let within = Area {
            y: 1,
            height: 10,
            ..SCREEN
        };
        let corner = |position| {
            crate::config::set(config::Config {
                toast_position: position,
                ..Default::default()
            });
            let areas = toasts.areas(within);
            let first = areas[1].1;
            let second = areas[0].1;
            (first.x, first.y, second.x, second.y)
        };
        assert_eq!(corner(ToastPosition::BottomRight), (30, 5, 29, 8));
        assert_eq!(corner(ToastPosition::BottomLeft), (1, 5, 1, 8));
        assert_eq!(corner(ToastPosition::TopRight), (30, 4, 29, 1));
        assert_eq!(corner(ToastPosition::TopLeft), (1, 4, 1, 1));
        assert_eq!(
            ToastPosition::named("top-left"),
            Some(ToastPosition::TopLeft)
        );
        assert_eq!(ToastPosition::named("top"), None);
    }

    #[test]
    fn long_messages_wrap() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("word ".repeat(20).trim().into(), true, now);
        let screen = Area {
            width: 80,
            height: 23,
            ..SCREEN
        };
        let areas = toasts.areas(screen);
        assert_eq!(areas[0].0.len(), 3);
        assert!(areas[0].1.width as usize <= MAX_TEXT + 4);
        toasts.push("word ".repeat(60).trim().into(), true, now);
        let areas = toasts.areas(screen);
        assert_eq!(areas[0].0.len(), MAX_LINES);
        assert!(areas[0].0[MAX_LINES - 1].ends_with("word…"));
    }
}
