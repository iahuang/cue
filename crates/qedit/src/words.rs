//! Word-wise cursor movement.
//!
//! The native word boundaries are line-wrap break points (they stop inside
//! leading whitespace and skip CJK words), so qedit uses the usual editor
//! rule instead: a forward jump skips whitespace, including line breaks, then
//! one run of word characters or of punctuation; a backward jump mirrors it.
//!
//! Movement steps through the text with the native cursor, one grapheme at a
//! time, so graphemes, wide characters, and tabs are handled exactly as the
//! engine lays them out.

use opentui::EditBuffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(grapheme: &str) -> Class {
    match grapheme.chars().next() {
        None => Class::Space,
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
        Some(_) => Class::Punct,
    }
}

/// Moves the cursor to the end of the next word and returns its offset.
pub fn word_right(eb: &EditBuffer) -> u32 {
    scan(eb, EditBuffer::move_cursor_right)
}

/// Moves the cursor to the start of the previous word and returns its offset.
pub fn word_left(eb: &EditBuffer) -> u32 {
    scan(eb, EditBuffer::move_cursor_left)
}

fn scan(eb: &EditBuffer, step: fn(&EditBuffer)) -> u32 {
    let mut run: Option<Class> = None;
    loop {
        let at = eb.cursor().offset;
        step(eb);
        let next = eb.cursor().offset;
        if next == at {
            return at; // start or end of the text
        }
        let grapheme_class = class(&eb.text_range(at, next));
        match run {
            None if grapheme_class == Class::Space => {}
            None => run = Some(grapheme_class),
            Some(run) if run == grapheme_class => {}
            Some(_) => {
                // Stepped past the end of the run: step back.
                eb.set_cursor_by_offset(at);
                return at;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentui::WidthMethod;
    use std::sync::MutexGuard;

    fn serial() -> MutexGuard<'static, ()> {
        crate::test_serial()
    }

    /// Cursor columns visited by repeated jumps from (row 0, col `start`).
    fn stops(text: &str, start: u32, forward: bool, n: usize) -> Vec<(u32, u32)> {
        let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
        eb.set_text(text);
        eb.set_cursor(0, start);
        (0..n)
            .map(|_| {
                if forward {
                    word_right(&eb);
                } else {
                    word_left(&eb);
                }
                (eb.cursor().row, eb.cursor().col)
            })
            .collect()
    }

    #[test]
    fn forward_stops_at_word_and_punctuation_ends() {
        let _serial = serial();
        //          0         1         2
        //          012345678901234567890123456
        let text = "  hello, wörld_x foo.bar  \n漢字 two";
        assert_eq!(
            stops(text, 0, true, 8),
            [
                (0, 7),
                (0, 8),
                (0, 16),
                (0, 20),
                (0, 21),
                (0, 24),
                (1, 4),
                (1, 8)
            ]
        );
    }

    #[test]
    fn backward_stops_at_word_and_punctuation_starts() {
        let _serial = serial();
        let text = "  hello, wörld_x foo.bar  \n漢字 two";
        let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
        eb.set_text(text);
        eb.set_cursor(1, 8);
        let mut seen = Vec::new();
        for _ in 0..9 {
            word_left(&eb);
            seen.push((eb.cursor().row, eb.cursor().col));
        }
        assert_eq!(
            seen,
            [
                (1, 5),
                (1, 0),
                (0, 21),
                (0, 20),
                (0, 17),
                (0, 9),
                (0, 7),
                (0, 2),
                (0, 0)
            ]
        );
    }

    #[test]
    fn stops_at_text_edges() {
        let _serial = serial();
        assert_eq!(stops("ab", 2, true, 2), [(0, 2), (0, 2)]);
        assert_eq!(stops("ab", 0, false, 1), [(0, 0)]);
        assert_eq!(stops("", 0, true, 1), [(0, 0)]);
        // Tabs and emoji are handled by the engine's own stepping.
        assert_eq!(stops("\tab 🙂🙂 cd", 0, true, 3), [(0, 4), (0, 9), (0, 12)]);
    }
}
