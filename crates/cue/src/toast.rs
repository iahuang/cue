//! Toasts: messages, such as that a file was saved or why it couldn't be,
//! stacked over the bottom right of the screen, above the status bar, the
//! newest at the bottom. They don't take the keyboard. Each goes after a
//! few seconds, an error after a few more, or when it's clicked.

use std::time::{Duration, Instant};

use opentui::{Attributes, Buffer};

use crate::alert;
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

    /// The toast at (`x`, `y`) on a screen `width` x `height`, if any.
    pub fn at(&self, x: u32, y: u32, width: u32, height: u32) -> Option<usize> {
        self.areas(width, height)
            .into_iter()
            .position(|(_, area)| area.contains(x, y))
            .map(|index| self.toasts.len() - 1 - index)
    }

    /// The area of toast `index`, as [`Toasts::at`] says, on a screen
    /// `width` x `height`.
    pub fn area(&self, index: usize, width: u32, height: u32) -> Option<Area> {
        let areas = self.areas(width, height);
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

    /// Each toast's lines and where it goes, newest first, on a screen
    /// `width` x `height`: right-aligned, stacked up from above the status
    /// bar, as many as fit.
    fn areas(&self, width: u32, height: u32) -> Vec<(Vec<String>, Area)> {
        let room = (width.saturating_sub(6) as usize).min(MAX_TEXT);
        if room < 8 {
            return Vec::new();
        }
        let mut bottom = height.saturating_sub(1);
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
            let Some(y) = bottom.checked_sub(h) else {
                break;
            };
            let area = Area {
                x: width - w - 1,
                y,
                width: w,
                height: h,
            };
            areas.push((lines, area));
            bottom = y;
        }
        areas
    }

    pub fn draw(&self, frame: &Buffer, width: u32, height: u32) {
        let colors = theme::colors();
        for (index, (lines, area)) in self.areas(width, height).into_iter().enumerate() {
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
            frame.fill_rect(x, y, width, height, colors.surface);
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

    #[test]
    fn stacked_above_the_status_bar_newest_lowest_and_clicked_away() {
        let _serial = crate::test_serial();
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("First".into(), false, now);
        toasts.push("Second".into(), false, now);
        let frame = OwnedBuffer::new(40, 12, false, WidthMethod::Unicode, "test").unwrap();
        frame.clear(theme::colors().bg);
        toasts.draw(&frame, 40, 12);
        let text = frame.to_text(true);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[9].trim_end().ends_with("│ Second │"), "{text}");
        assert!(lines[10].trim_end().ends_with("╰────────╯"), "{text}");
        assert!(lines[6].trim_end().ends_with("│ First │"), "{text}");

        assert_eq!(toasts.at(36, 9, 40, 12), Some(1));
        assert_eq!(toasts.at(36, 6, 40, 12), Some(0));
        assert_eq!(toasts.at(2, 9, 40, 12), None);
        let area = toasts.area(1, 40, 12).unwrap();
        assert_eq!((area.y, area.height), (8, 3));
        toasts.dismiss(1);
        assert_eq!(toasts.texts(), ["First"]);
    }

    #[test]
    fn long_messages_wrap() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("word ".repeat(20).trim().into(), true, now);
        let areas = toasts.areas(80, 24);
        assert_eq!(areas[0].0.len(), 3);
        assert!(areas[0].1.width as usize <= MAX_TEXT + 4);
        toasts.push("word ".repeat(60).trim().into(), true, now);
        let areas = toasts.areas(80, 24);
        assert_eq!(areas[0].0.len(), MAX_LINES);
        assert!(areas[0].0[MAX_LINES - 1].ends_with("word…"));
    }
}
