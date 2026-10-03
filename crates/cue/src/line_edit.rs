//! Editing a one-line text field, as in the picker, workspace search, and
//! the find bar: a cursor that typing goes in at, the keys that move it and
//! delete around it, the part of the text selected, and the part shown when
//! it doesn't fit.
//!
//! The text lives with its owner (a query is searched for, a replacement
//! remembered), so a [`Caret`] only knows where in it the cursor and the
//! selection are. Positions are bytes, always on a character boundary;
//! widths and columns are characters.

use std::cell::Cell;
use std::ops::Range;
use std::time::Instant;

use opentui::Buffer;

use crate::input::MULTI_CLICK;
use crate::theme;

/// An edit to a field, from a key or a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit<'a> {
    Insert(&'a str),
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    Left,
    Right,
    WordLeft,
    WordRight,
    Start,
    End,
}

/// Where the cursor is in a field's text.
#[derive(Debug, Clone, Default)]
pub struct Caret {
    /// The cursor's byte offset, or `None` at the end, where it stays as the
    /// text is replaced.
    at: Option<usize>,
    /// Where the selection started, the cursor being where it ends.
    anchor: Option<usize>,
    /// The first character shown, when the text doesn't fit.
    scroll: Cell<usize>,
    /// The last press of the mouse: where, when, and how many in a row.
    last_click: Option<(usize, Instant, u32)>,
    /// A press in the field is held, so dragging selects.
    pressed: bool,
}

impl Caret {
    /// The cursor's byte offset in `text`.
    pub fn at(&self, text: &str) -> usize {
        match self.at {
            Some(at) => floor_boundary(text, at),
            None => text.len(),
        }
    }

    /// Puts the cursor at the end, where it stays as the text is replaced.
    pub fn move_to_end(&mut self) {
        self.at = None;
        self.anchor = None;
    }

    fn set(&mut self, text: &str, at: usize) {
        self.at = (at < text.len()).then_some(at);
    }

    /// The part of `text` selected, if any.
    pub fn selection(&self, text: &str) -> Option<Range<usize>> {
        let anchor = floor_boundary(text, self.anchor?);
        let at = self.at(text);
        (anchor != at).then(|| anchor.min(at)..anchor.max(at))
    }

    /// The text selected, if any.
    pub fn selected_text<'a>(&self, text: &'a str) -> Option<&'a str> {
        self.selection(text).map(|range| &text[range])
    }

    /// Selects all of `text`, the cursor at its end.
    pub fn select_all(&mut self, text: &str) {
        self.at = None;
        self.anchor = (!text.is_empty()).then_some(0);
    }

    /// Applies `edit` to `text` with Shift held: a movement extends the
    /// selection; anything else is as [`Caret::edit`]. Returns whether the
    /// text changed.
    pub fn select(&mut self, text: &mut String, edit: Edit) -> bool {
        if !edit.moves() {
            return self.edit(text, edit);
        }
        let anchor = match self.anchor {
            Some(anchor) => floor_boundary(text, anchor),
            None => self.at(text),
        };
        self.anchor = None;
        self.edit(text, edit);
        self.anchor = (anchor != self.at(text)).then_some(anchor);
        false
    }

    /// Applies `edit` to `text`, and returns whether the text changed.
    /// With text selected, typing replaces it, deleting deletes it, and
    /// Left and Right go to its start and end.
    pub fn edit(&mut self, text: &mut String, edit: Edit) -> bool {
        let mut replaced = false;
        if let Some(selection) = self.selection(text) {
            self.anchor = None;
            match edit {
                Edit::Insert(_) => {
                    text.replace_range(selection.clone(), "");
                    self.set(text, selection.start);
                    replaced = true;
                }
                Edit::DeleteBackward
                | Edit::DeleteForward
                | Edit::DeleteWordBackward
                | Edit::DeleteWordForward => {
                    text.replace_range(selection.clone(), "");
                    self.set(text, selection.start);
                    return true;
                }
                Edit::Left => {
                    self.set(text, selection.start);
                    return false;
                }
                Edit::Right => {
                    self.set(text, selection.end);
                    return false;
                }
                _ => {}
            }
        }
        self.anchor = None;
        // Every change adds or removes something.
        let len = text.len();
        let at = self.at(text);
        let at = match edit {
            Edit::Insert(inserted) => {
                text.insert_str(at, inserted);
                at + inserted.len()
            }
            Edit::DeleteBackward => {
                let start = prev_char(text, at);
                text.replace_range(start..at, "");
                start
            }
            Edit::DeleteForward => {
                text.replace_range(at..next_char(text, at), "");
                at
            }
            Edit::DeleteWordBackward => {
                let start = word_start(text, at);
                text.replace_range(start..at, "");
                start
            }
            Edit::DeleteWordForward => {
                text.replace_range(at..word_end(text, at), "");
                at
            }
            Edit::Left => prev_char(text, at),
            Edit::Right => next_char(text, at),
            Edit::WordLeft => word_start(text, at),
            Edit::WordRight => word_end(text, at),
            Edit::Start => 0,
            Edit::End => text.len(),
        };
        self.set(text, at);
        replaced || text.len() != len
    }

    /// The part of `text` shown in `room` columns, and the cursor's column
    /// in it. The view scrolls only as far as it takes to keep the cursor in
    /// view, with a column after the text for it at the end.
    pub fn view(&self, text: &str, room: usize) -> (String, usize) {
        let len = text.chars().count();
        let cursor = text[..self.at(text)].chars().count();
        let mut scroll = self.scroll.get().min((len + 1).saturating_sub(room));
        if cursor < scroll {
            scroll = cursor;
        } else if room > 0 && cursor >= scroll + room {
            scroll = cursor + 1 - room;
        }
        self.scroll.set(scroll);
        let shown = text.chars().skip(scroll).take(room).collect();
        (shown, cursor - scroll)
    }

    /// The columns of the view last drawn that are selected, which may run
    /// past its right edge.
    pub fn selected_columns(&self, text: &str) -> Option<Range<usize>> {
        let selection = self.selection(text)?;
        let scroll = self.scroll.get();
        let start = text[..selection.start].chars().count();
        let end = start + text[selection].chars().count();
        (end > scroll).then(|| start.saturating_sub(scroll)..end - scroll)
    }

    /// Where in `text` column `column` of the view last drawn is: before
    /// the character there, or at the end past the text.
    fn offset_at(&self, text: &str, column: usize) -> usize {
        let index = self.scroll.get() + column;
        text.char_indices()
            .nth(index)
            .map_or(text.len(), |(i, _)| i)
    }

    /// A press of the mouse at `column` of the view last drawn: puts the
    /// cursor there, ready to drag a selection. A second press in a row
    /// there selects the word, and a third, all of the text.
    pub fn press(&mut self, text: &str, column: usize, now: Instant) {
        let at = self.offset_at(text, column);
        let count = match self.last_click {
            Some((last, time, count)) if last == at && now.duration_since(time) < MULTI_CLICK => {
                count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((at, now, count));
        self.pressed = count == 1;
        match count {
            1 => {
                self.anchor = None;
                self.set(text, at);
            }
            2 => {
                let word = word_around(text, at);
                self.anchor = Some(word.start);
                self.set(text, word.end);
            }
            _ => self.select_all(text),
        }
    }

    /// The mouse dragged, after a press in the field, to `column` of the
    /// view last drawn: selects from the press to there.
    pub fn drag(&mut self, text: &str, column: usize) {
        if !self.pressed {
            return;
        }
        let anchor = match self.anchor {
            Some(anchor) => floor_boundary(text, anchor),
            None => self.at(text),
        };
        let at = self.offset_at(text, column);
        self.set(text, at);
        self.anchor = (anchor != at).then_some(anchor);
    }

    /// Whether a press in the field is held, so dragging selects.
    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// The mouse was released. Returns whether a press in the field was a
    /// plain click: once, without dragging a selection.
    pub fn release(&mut self, text: &str) -> bool {
        let clicked = std::mem::take(&mut self.pressed) && self.selection(text).is_none();
        clicked && matches!(self.last_click, Some((_, _, 1)))
    }
}

impl Edit<'_> {
    /// Whether this only moves the cursor.
    fn moves(self) -> bool {
        matches!(
            self,
            Edit::Left | Edit::Right | Edit::WordLeft | Edit::WordRight | Edit::Start | Edit::End
        )
    }
}

/// Shades the part of a field's `text` that `caret` has selected, under
/// where the text is drawn from `x`, in `room` columns.
pub fn draw_selection(frame: &Buffer, caret: &Caret, text: &str, x: u32, y: u32, room: usize) {
    if let Some(columns) = caret.selected_columns(text) {
        let end = columns.end.min(room);
        if columns.start < end {
            let width = (end - columns.start) as u32;
            let x = x + columns.start as u32;
            frame.fill_rect(x, y, width, 1, theme::colors().selection);
        }
    }
}

fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn prev_char(text: &str, at: usize) -> usize {
    text[..at].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(at, |c| at + c.len_utf8())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Back over whitespace, then a word, or a single other character, such as
/// the `/` between a path's folders.
fn word_start(text: &str, at: usize) -> usize {
    let trimmed = text[..at].trim_end_matches(char::is_whitespace);
    match trimmed.chars().next_back() {
        Some(c) if is_word(c) => trimmed.trim_end_matches(is_word).len(),
        Some(c) => trimmed.len() - c.len_utf8(),
        None => 0,
    }
}

/// The word at `at`, as a double click selects it: the run of word
/// characters there, or the one other character. At the end, the word
/// before.
fn word_around(text: &str, at: usize) -> Range<usize> {
    let at = match text[at..].chars().next() {
        Some(c) if !is_word(c) => return at..at + c.len_utf8(),
        Some(_) => at,
        None => match text[..at].chars().next_back() {
            Some(c) if is_word(c) => at,
            Some(c) => return at - c.len_utf8()..at,
            None => return at..at,
        },
    };
    let start = text[..at].trim_end_matches(is_word).len();
    let end = text.len() - text[at..].trim_start_matches(is_word).len();
    start..end
}

/// [`word_start`] mirrored.
fn word_end(text: &str, at: usize) -> usize {
    let trimmed = text[at..].trim_start_matches(char::is_whitespace);
    let rest = match trimmed.chars().next() {
        Some(c) if is_word(c) => trimmed.trim_start_matches(is_word),
        Some(c) => &trimmed[c.len_utf8()..],
        None => "",
    };
    text.len() - rest.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` edited by `edits` from its end, with `|` at the cursor.
    fn edited(text: &str, edits: &[Edit]) -> String {
        let mut text = text.to_string();
        let mut caret = Caret::default();
        for &edit in edits {
            caret.edit(&mut text, edit);
        }
        let at = caret.at(&text);
        format!("{}|{}", &text[..at], &text[at..])
    }

    #[test]
    fn edits_at_the_cursor() {
        use Edit::*;
        assert_eq!(edited("mian", &[Left, Left, DeleteBackward]), "m|an");
        assert_eq!(edited("man", &[Left, Left, Insert("ai")]), "mai|an");
        assert_eq!(edited("abc", &[Start, DeleteForward, End]), "bc|");
        assert_eq!(edited("abc", &[Start, Left, Right, Right]), "ab|c");
        assert_eq!(edited("abc", &[Right, DeleteForward]), "abc|");
        assert_eq!(edited("héllo", &[Left, Left, Left, Left]), "h|éllo");
        assert_eq!(
            edited("héllo", &[Start, Right, Right, DeleteBackward]),
            "h|llo"
        );
    }

    #[test]
    fn moves_and_deletes_by_word() {
        use Edit::*;
        assert_eq!(edited("src/main.rs", &[WordLeft]), "src/main.|rs");
        assert_eq!(edited("src/main.rs", &[WordLeft, WordLeft]), "src/main|.rs");
        assert_eq!(edited("src/main.rs", &[Start, WordRight]), "src|/main.rs");
        assert_eq!(
            edited("foo  bar", &[Start, WordRight, WordRight]),
            "foo  bar|"
        );
        assert_eq!(edited("foo bar", &[WordLeft, DeleteWordBackward]), "|bar");
        assert_eq!(edited("foo bar", &[Start, DeleteWordForward]), "| bar");
        assert_eq!(edited("src/", &[DeleteWordBackward]), "src|");
    }

    /// `text` after `edits` from its end, Shift held for those marked, with
    /// `[` and `]` around the selection or `|` at the cursor.
    fn selected(text: &str, edits: &[(Edit, bool)]) -> String {
        let mut text = text.to_string();
        let mut caret = Caret::default();
        for &(edit, select) in edits {
            match select {
                true => caret.select(&mut text, edit),
                false => caret.edit(&mut text, edit),
            };
        }
        shown(&text, &caret)
    }

    fn shown(text: &str, caret: &Caret) -> String {
        match caret.selection(text) {
            Some(range) => format!(
                "{}[{}]{}",
                &text[..range.start],
                &text[range.clone()],
                &text[range.end..]
            ),
            None => {
                let at = caret.at(text);
                format!("{}|{}", &text[..at], &text[at..])
            }
        }
    }

    #[test]
    fn shift_selects_and_edits_replace_the_selection() {
        use Edit::*;
        let shift = |edit| (edit, true);
        let plain = |edit| (edit, false);
        assert_eq!(selected("abc", &[shift(Left), shift(Left)]), "a[bc]");
        assert_eq!(selected("abc", &[shift(Left), shift(Right)]), "abc|");
        assert_eq!(selected("src/main.rs", &[shift(WordLeft)]), "src/main.[rs]");
        assert_eq!(
            selected("abc", &[plain(Start), shift(Right), shift(End)]),
            "[abc]"
        );
        assert_eq!(
            selected("abc", &[shift(Left), shift(Left), plain(Insert("x"))]),
            "ax|"
        );
        assert_eq!(
            selected("abc", &[shift(Left), shift(Left), plain(DeleteBackward)]),
            "a|"
        );
        assert_eq!(
            selected("abc", &[shift(Left), shift(Left), plain(Left)]),
            "a|bc"
        );
        assert_eq!(
            selected("abc", &[plain(Start), shift(Right), plain(Right)]),
            "a|bc"
        );
        assert_eq!(
            selected("abc", &[shift(Start), shift(DeleteWordBackward)]),
            "|"
        );

        let mut caret = Caret::default();
        caret.select_all("abc");
        assert_eq!(shown("abc", &caret), "[abc]");
        assert_eq!(caret.selected_text("abc"), Some("abc"));
        caret.move_to_end();
        assert_eq!(caret.selected_text("abc"), None);
    }

    #[test]
    fn the_mouse_places_the_cursor_and_selects() {
        let text = "src/main.rs";
        let mut caret = Caret::default();
        caret.view(text, 20);
        let now = Instant::now();
        caret.press(text, 5, now);
        assert_eq!(shown(text, &caret), "src/m|ain.rs");
        caret.drag(text, 8);
        assert_eq!(shown(text, &caret), "src/m[ain].rs");
        caret.drag(text, 2);
        assert_eq!(shown(text, &caret), "sr[c/m]ain.rs");
        caret.drag(text, 40);
        assert_eq!(shown(text, &caret), "src/m[ain.rs]");
        assert!(!caret.release(text), "a drag isn't a click");
        caret.drag(text, 0);
        assert_eq!(
            shown(text, &caret),
            "src/m[ain.rs]",
            "not after the release"
        );
        assert_eq!(caret.selected_columns(text), Some(5..11));

        // Two clicks in a row select a word, three all of it.
        caret.press(text, 6, now);
        assert!(caret.release(text));
        caret.press(text, 6, now);
        assert_eq!(shown(text, &caret), "src/[main].rs");
        caret.press(text, 6, now);
        assert_eq!(shown(text, &caret), "[src/main.rs]");
        caret.press(text, 3, now);
        assert_eq!(shown(text, &caret), "src|/main.rs");
        caret.press(text, 3, now);
        assert_eq!(shown(text, &caret), "src[/]main.rs");
        caret.press(text, 30, now - MULTI_CLICK / 2);
        caret.press(text, 30, now);
        assert_eq!(
            shown(text, &caret),
            "src/main.[rs]",
            "past the end, the last word"
        );
    }

    #[test]
    fn stays_at_the_end_as_the_text_is_replaced() {
        let mut caret = Caret::default();
        assert_eq!(caret.at("abc"), 3);
        let mut text = "abc".to_string();
        caret.edit(&mut text, Edit::Left);
        assert_eq!(caret.at("longer"), 2);
        assert_eq!(caret.at("x"), 1, "clamped");
        assert_eq!(caret.at("aé"), 1, "on a character boundary");
        caret.move_to_end();
        assert_eq!(caret.at("longer"), 6);
    }

    #[test]
    fn scrolls_to_keep_the_cursor_in_view() {
        let mut text = "abcdefghij".to_string();
        let mut caret = Caret::default();
        // The end, with a column for the cursor after it.
        assert_eq!(caret.view(&text, 4), ("hij".to_string(), 3));
        // Moving within the view doesn't scroll it.
        caret.edit(&mut text, Edit::Left);
        caret.edit(&mut text, Edit::Left);
        assert_eq!(caret.view(&text, 4), ("hij".to_string(), 1));
        // Past its left edge, it scrolls a column at a time.
        for _ in 0..3 {
            caret.edit(&mut text, Edit::Left);
        }
        assert_eq!(caret.view(&text, 4), ("fghi".to_string(), 0));
        caret.edit(&mut text, Edit::Right);
        assert_eq!(caret.view(&text, 4), ("fghi".to_string(), 1));
        // Deleting brings more of the text into view.
        caret.edit(&mut text, Edit::End);
        for _ in 0..3 {
            caret.edit(&mut text, Edit::DeleteBackward);
        }
        assert_eq!(caret.view(&text, 4), ("efg".to_string(), 3));
        // Text that fits is shown whole.
        caret.edit(&mut text, Edit::Start);
        assert_eq!(caret.view(&text, 20), ("abcdefg".to_string(), 0));
    }
}
