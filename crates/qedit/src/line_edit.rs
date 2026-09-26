//! Editing a one-line text field, as in the picker, workspace search, and
//! the find bar: a cursor that typing goes in at, the keys that move it and
//! delete around it, and the part of the text shown when it doesn't fit.
//!
//! The text lives with its owner (a query is searched for, a replacement
//! remembered), so a [`Caret`] only knows where in it the cursor is. Positions
//! are bytes, always on a character boundary; widths are characters.

use std::cell::Cell;

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
    /// The first character shown, when the text doesn't fit.
    scroll: Cell<usize>,
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
    }

    fn set(&mut self, text: &str, at: usize) {
        self.at = (at < text.len()).then_some(at);
    }

    /// Applies `edit` to `text`, and returns whether the text changed.
    pub fn edit(&mut self, text: &mut String, edit: Edit) -> bool {
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
        text.len() != len
    }

    /// Applies `edit` to `text`, all of it selected: typing replaces it,
    /// deleting clears it, and moving goes to its start or end. Returns
    /// whether the text changed.
    pub fn edit_selected(&mut self, text: &mut String, edit: Edit) -> bool {
        let len = text.len();
        match edit {
            Edit::Left | Edit::WordLeft | Edit::Start => {
                self.edit(text, Edit::Start);
            }
            Edit::Right | Edit::WordRight | Edit::End => self.move_to_end(),
            _ => {
                text.clear();
                self.move_to_end();
                if let Edit::Insert(_) = edit {
                    self.edit(text, edit);
                }
            }
        }
        // Replaced with itself, it changed too: the selection went.
        text.len() != len || matches!(edit, Edit::Insert(inserted) if !inserted.is_empty())
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

    #[test]
    fn edits_a_selected_text_as_a_whole() {
        let edited = |edit| {
            let mut text = "abc".to_string();
            let mut caret = Caret::default();
            let changed = caret.edit_selected(&mut text, edit);
            let at = caret.at(&text);
            (format!("{}|{}", &text[..at], &text[at..]), changed)
        };
        assert_eq!(edited(Edit::Insert("x")), ("x|".to_string(), true));
        assert_eq!(edited(Edit::Insert("abc")), ("abc|".to_string(), true));
        assert_eq!(edited(Edit::DeleteForward), ("|".to_string(), true));
        assert_eq!(edited(Edit::Left), ("|abc".to_string(), false));
        assert_eq!(edited(Edit::WordRight), ("abc|".to_string(), false));
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
