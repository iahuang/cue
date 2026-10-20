//! Closing brackets and quotes as they're typed, by the pairs a language
//! has:
//!
//! - An opening one types its closer after the cursor, where what follows
//!   is a space, the line's end, or punctuation that tends to end an
//!   expression. Quotes pair only in code, and not after a word, so an
//!   apostrophe stays one; a string prefix such as Python's `f` is fine.
//! - Typing the closer just before one typed for it steps over it, and
//!   Backspace between the two deletes both.
//! - With text selected, an opening one goes around it instead.

use crate::complete::is_word_char;
use crate::language::Language;
use crate::syntax::Region;

/// What typing a character does.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    /// Types it, and this closer after the cursor.
    Pair(char),
    /// Steps over the closer after the cursor, typed for it.
    Over,
    Plain,
}

const BRACKETS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

const CODE: &[(char, char)] = &[
    BRACKETS[0],
    BRACKETS[1],
    BRACKETS[2],
    ('"', '"'),
    ('\'', '\''),
];
/// Where `'` is a lifetime or a quote rather than a string.
const NO_SINGLE: &[(char, char)] = &[BRACKETS[0], BRACKETS[1], BRACKETS[2], ('"', '"')];
const BACKTICK: &[(char, char)] = &[
    BRACKETS[0],
    BRACKETS[1],
    BRACKETS[2],
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];
const MARKUP: &[(char, char)] = &[
    BRACKETS[0],
    BRACKETS[1],
    BRACKETS[2],
    ('"', '"'),
    ('\'', '\''),
    ('<', '>'),
];
/// Prose, where `'` is an apostrophe.
const MARKDOWN: &[(char, char)] = &[
    BRACKETS[0],
    BRACKETS[1],
    BRACKETS[2],
    ('"', '"'),
    ('`', '`'),
];
const ONLY_BRACKETS: &[(char, char)] = &BRACKETS;

/// The pairs of `language`, or of text in none.
pub fn of(language: Option<&Language>) -> &'static [(char, char)] {
    let Some(language) = language else {
        return NO_SINGLE;
    };
    match language.name {
        "Rust" | "Clojure" | "Scheme" | "Common Lisp" | "Typst" => NO_SINGLE,
        "JavaScript" | "TypeScript" | "TSX" | "Go" | "Shell" | "Kotlin" | "Dart" | "Ruby"
        | "Perl" | "SQL" | "PowerShell" => BACKTICK,
        "HTML" | "Svelte" => MARKUP,
        "Markdown" => MARKDOWN,
        // `"` starts a comment; LaTeX quotes with ``…''.
        "Vim script" | "LaTeX" | "BibTeX" => ONLY_BRACKETS,
        "Diff" => &[],
        _ => CODE,
    }
}

/// The closer of `opener` among `pairs`.
pub fn closer(pairs: &[(char, char)], opener: char) -> Option<char> {
    pairs.iter().find(|&&(o, _)| o == opener).map(|&(_, c)| c)
}

/// Characters a pair may close before, other than spaces.
const CLOSE_BEFORE: &str = ";:.,=)]}>";

/// What typing `c` does, between `before` and `after` on its line, where
/// the character before is in `region`. `closed` says whether the one
/// after was typed for an opener.
pub fn typed(
    pairs: &[(char, char)],
    c: char,
    before: &str,
    after: &str,
    region: Region,
    closed: bool,
) -> Typed {
    let next = after.chars().next();
    if closed && next == Some(c) && pairs.iter().any(|&(_, close)| close == c) {
        return Typed::Over;
    }
    let Some(close) = closer(pairs, c) else {
        return Typed::Plain;
    };
    if next.is_some_and(|n| !n.is_whitespace() && !CLOSE_BEFORE.contains(n)) {
        return Typed::Plain;
    }
    if close == c {
        let prev = before.chars().next_back();
        let word = &before[before
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_word_char(c))
            .last()
            .map_or(before.len(), |(i, _)| i)..];
        // Two quotes and a third opens a triple-quoted string, unclosed.
        if region != Region::Code || prev == Some(c) || !(word.is_empty() || is_prefix(word)) {
            return Typed::Plain;
        }
    }
    Typed::Pair(close)
}

/// Whether Backspace between `before` and `after` deletes a pair: the
/// one after closes the one before, and was typed for it.
pub fn deletes_pair(pairs: &[(char, char)], before: &str, after: &str, closed: bool) -> bool {
    let (Some(open), Some(close)) = (before.chars().next_back(), after.chars().next()) else {
        return false;
    };
    closed && pairs.contains(&(open, close))
}

/// Whether `word` prefixes a string, as in Python's `f"…"` or Rust's
/// `b"…"`.
fn is_prefix(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "f" | "r" | "b" | "u" | "rb" | "br" | "fr" | "rf" | "l" | "u8" | "c" | "cr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(c: char, before: &str, after: &str) -> Typed {
        typed(CODE, c, before, after, Region::Code, false)
    }

    #[test]
    fn opening_brackets_close_before_spaces_and_punctuation() {
        assert_eq!(code('(', "f", ""), Typed::Pair(')'));
        assert_eq!(code('[', "x = ", " "), Typed::Pair(']'));
        assert_eq!(code('{', "f(", ")"), Typed::Pair('}'));
        assert_eq!(code('(', "", ";"), Typed::Pair(')'));
        // Not before a word, or a quote.
        assert_eq!(code('(', "", "x"), Typed::Plain);
        assert_eq!(code('(', "", "\""), Typed::Plain);
        // Brackets pair in strings and comments too.
        assert_eq!(
            typed(CODE, '(', "", "", Region::Comment, false),
            Typed::Pair(')')
        );
    }

    #[test]
    fn quotes_close_only_in_code_and_not_after_a_word() {
        assert_eq!(code('"', "x = ", ""), Typed::Pair('"'));
        assert_eq!(code('\'', "f(", ")"), Typed::Pair('\''));
        assert_eq!(code('\'', "don", ""), Typed::Plain);
        assert_eq!(code('"', "print(f", ")"), Typed::Pair('"'));
        assert_eq!(code('"', "let s = b", ""), Typed::Pair('"'));
        assert_eq!(
            typed(CODE, '"', "x", "", Region::String, false),
            Typed::Plain
        );
        assert_eq!(
            typed(CODE, '\'', "// ", "", Region::Comment, false),
            Typed::Plain
        );
        // The third of `"""`.
        assert_eq!(code('"', "\"\"", ""), Typed::Plain);
    }

    #[test]
    fn closers_typed_for_an_opener_are_stepped_over() {
        assert_eq!(typed(CODE, ')', "f(", ")", Region::Code, true), Typed::Over);
        assert_eq!(
            typed(CODE, '"', "\"a", "\"", Region::String, true),
            Typed::Over
        );
        // Ones typed some other way are typed again.
        assert_eq!(
            typed(CODE, ')', "f(", ")", Region::Code, false),
            Typed::Plain
        );
        assert_eq!(
            typed(CODE, ']', "f(", ")", Region::Code, true),
            Typed::Plain
        );
    }

    #[test]
    fn languages_pair_their_own() {
        let rust = crate::language::detect(Some(std::path::Path::new("a.rs")), String::new);
        assert_eq!(closer(of(rust), '\''), None);
        let html = crate::language::detect(Some(std::path::Path::new("a.html")), String::new);
        assert_eq!(closer(of(html), '<'), Some('>'));
        let js = crate::language::detect(Some(std::path::Path::new("a.js")), String::new);
        assert_eq!(closer(of(js), '`'), Some('`'));
        assert_eq!(closer(of(rust), '<'), None);
    }

    #[test]
    fn backspace_deletes_a_pair_typed_together() {
        assert!(deletes_pair(CODE, "f(", ")", true));
        assert!(deletes_pair(CODE, "\"", "\"", true));
        assert!(!deletes_pair(CODE, "f(", ")", false));
        assert!(!deletes_pair(CODE, "f(", "]", true));
    }
}
