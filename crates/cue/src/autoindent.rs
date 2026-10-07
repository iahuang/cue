//! Indenting as you type, by a few rules most editors share rather than a
//! language's grammar:
//!
//! - A line break carries the line's indentation onto the new line.
//! - After an opening bracket, or a `:` in languages that open blocks with
//!   one, the new line goes one level deeper; between a pair of brackets,
//!   the closing one goes on a line of its own, back at the first's level.
//! - A closing bracket typed on a line with nothing else before it moves
//!   the line to the indentation of the line with its opening bracket.
//!
//! Brackets in comments and strings don't count, as the syntax tree has
//! them; text that isn't highlighted is all code.

use std::ops::Range;

use crate::syntax::Region;

/// Bracket scans give up past this many bytes back.
const MAX_SCAN: usize = 1 << 20;

/// A line break, as text to put in place of some of the line.
#[derive(Debug, PartialEq, Eq)]
pub struct LineBreak {
    /// The bytes of the text to replace.
    pub replace: Range<usize>,
    pub insert: String,
    /// Where the cursor goes, in bytes into `insert`.
    pub cursor: usize,
}

/// Breaks the line of `text` at byte `at`. The new line takes the
/// indentation before `at`, and `unit` more after an opening bracket, or a
/// `:` with `colon_opens`. Spaces either side of `at` go, and so a line
/// with nothing before `at` is left empty.
pub fn line_break(
    text: &str,
    at: usize,
    unit: &str,
    colon_opens: bool,
    region: impl Fn(usize) -> Region,
) -> LineBreak {
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    let before = &text[line_start..at];
    let after = &text[at..line_end];
    let blank_before = before.trim_start_matches([' ', '\t']).is_empty();
    // On a line with nothing before the cursor, the line keeps its
    // indentation as it moves down.
    let base = leading_space(if blank_before {
        &text[line_start..line_end]
    } else {
        before
    });
    let start = line_start + before.trim_end_matches([' ', '\t']).len();
    let end = at + leading_space(after).len();
    let rest = &text[end..line_end];

    let mut indent = base.to_string();
    let mut insert = String::from("\n");
    match last_code(before, line_start, &region) {
        Some(b @ (b'(' | b'[' | b'{')) => {
            indent.push_str(unit);
            insert.push_str(&indent);
            if rest.as_bytes().first() == Some(&closer_of(b)) {
                insert.push('\n');
                insert.push_str(base);
            }
            return LineBreak {
                replace: start..end,
                cursor: 1 + indent.len(),
                insert,
            };
        }
        Some(b':') if colon_opens => indent.push_str(unit),
        _ => {}
    }
    insert.push_str(&indent);
    LineBreak {
        replace: start..end,
        cursor: insert.len(),
        insert,
    }
}

/// The indentation a line should have when `closer` is typed at byte `at`
/// of `text`, with only spaces before it on the line: that of the line with
/// the bracket it closes. `None` if it closes none, or a different kind,
/// or `at` is in a comment or string.
pub fn closer_indent(
    text: &str,
    at: usize,
    closer: char,
    region: impl Fn(usize) -> Region,
) -> Option<&str> {
    let opener = match closer {
        ')' => b'(',
        ']' => b'[',
        '}' => b'{',
        _ => return None,
    };
    if region(at) != Region::Code {
        return None;
    }
    let from = at.saturating_sub(MAX_SCAN);
    let mut depth = 0usize;
    for (i, &b) in text.as_bytes()[from..at].iter().enumerate().rev() {
        let i = from + i;
        let found = match b {
            b')' | b']' | b'}' => false,
            b'(' | b'[' | b'{' => true,
            _ => continue,
        };
        if region(i) != Region::Code {
            continue;
        }
        if !found {
            depth += 1;
        } else if depth > 0 {
            depth -= 1;
        } else if b != opener {
            return None;
        } else {
            let line_start = text[..i].rfind('\n').map_or(0, |n| n + 1);
            return Some(leading_space(&text[line_start..]));
        }
    }
    None
}

/// The byte line `row` of `text` starts at.
pub fn line_start(text: &str, row: u32) -> usize {
    match row {
        0 => 0,
        row => text
            .match_indices('\n')
            .nth(row as usize - 1)
            .map_or(text.len(), |(i, _)| i + 1),
    }
}

fn closer_of(opener: u8) -> u8 {
    match opener {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    }
}

fn leading_space(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// The last byte of `before`, at byte `offset` of the text, that isn't a
/// space or in a comment, if it's code.
fn last_code(before: &str, offset: usize, region: impl Fn(usize) -> Region) -> Option<u8> {
    before
        .bytes()
        .enumerate()
        .rev()
        .filter(|&(_, b)| b != b' ' && b != b'\t')
        .map(|(i, b)| (region(offset + i), b))
        .find(|&(region, _)| region != Region::Comment)
        .filter(|&(region, _)| region == Region::Code)
        .map(|(_, b)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(_: usize) -> Region {
        Region::Code
    }

    /// The text after breaking `text`'s line at `|`, with `|` where the
    /// cursor goes.
    fn broken(text: &str, colon_opens: bool) -> String {
        broken_with(text, colon_opens, code)
    }

    fn broken_with(text: &str, colon_opens: bool, region: impl Fn(usize) -> Region) -> String {
        let at = text.find('|').unwrap();
        let text = text.replacen('|', "", 1);
        let b = line_break(&text, at, "    ", colon_opens, region);
        let mut out = text.clone();
        let mut insert = b.insert.clone();
        insert.insert(b.cursor, '|');
        out.replace_range(b.replace, &insert);
        out
    }

    #[test]
    fn keeps_indentation() {
        assert_eq!(broken("    foo|", false), "    foo\n    |");
        assert_eq!(broken("\t\tfoo|\nbar", false), "\t\tfoo\n\t\t|\nbar");
        assert_eq!(broken("foo|", false), "foo\n|");
    }

    #[test]
    fn splits_mid_line_dropping_spaces_around_the_break() {
        assert_eq!(broken("    foo(a,   |  b)", false), "    foo(a,\n    |b)");
    }

    #[test]
    fn moves_an_indented_line_down_whole() {
        assert_eq!(broken("|    foo", false), "\n    |foo");
        assert_eq!(broken("  |  foo", false), "\n    |foo");
    }

    #[test]
    fn leaves_no_spaces_on_a_blank_line() {
        assert_eq!(broken("    |", false), "\n    |");
        assert_eq!(broken("x\n        |\ny", false), "x\n\n        |\ny");
    }

    #[test]
    fn indents_after_an_opening_bracket() {
        assert_eq!(broken("fn f() {|", false), "fn f() {\n    |");
        assert_eq!(broken("  call(|", false), "  call(\n      |");
        assert_eq!(broken("x = [  |", false), "x = [\n    |");
        assert_eq!(broken("f(a)|", false), "f(a)\n|");
    }

    #[test]
    fn puts_a_closing_bracket_on_its_own_line() {
        assert_eq!(broken("  if x {|}", false), "  if x {\n      |\n  }");
        assert_eq!(broken("f(| )", false), "f(\n    |\n)");
        // Only the bracket's own pair.
        assert_eq!(broken("f(|]", false), "f(\n    |]");
    }

    #[test]
    fn indents_after_a_colon_only_where_it_opens_blocks() {
        assert_eq!(broken("def f():|", true), "def f():\n    |");
        assert_eq!(broken("a: b|", true), "a: b\n|");
        assert_eq!(broken("label:|", false), "label:\n|");
    }

    #[test]
    fn ignores_brackets_in_comments_and_strings() {
        // `// {` is a comment, `"{"` a string.
        let text = "x { // y {|";
        let comment_from = text.find("//").unwrap();
        let in_comment = |i| {
            if i >= comment_from {
                Region::Comment
            } else {
                Region::Code
            }
        };
        assert_eq!(broken_with(text, false, in_comment), "x { // y {\n    |");

        let text = "x // y {|";
        let in_comment = |i| {
            if i >= 2 {
                Region::Comment
            } else {
                Region::Code
            }
        };
        assert_eq!(broken_with(text, false, in_comment), "x // y {\n|");

        let text = "s = \"{|";
        let in_string = |i| if i >= 4 { Region::String } else { Region::Code };
        assert_eq!(broken_with(text, false, in_string), "s = \"{\n|");
    }

    #[test]
    fn closer_takes_its_openers_indentation() {
        let text = "fn f() {\n    if x {\n        y\n    }\n    ";
        assert_eq!(closer_indent(text, text.len(), '}', code), Some(""));
        let text = "  foo(\n    a,\n    b,\n      ";
        assert_eq!(closer_indent(text, text.len(), ')', code), Some("  "));
        let text = "\tx = [\n\t\t";
        assert_eq!(closer_indent(text, text.len(), ']', code), Some("\t"));
    }

    #[test]
    fn closer_of_another_kind_or_none_is_left() {
        let text = "f(\n    ";
        assert_eq!(closer_indent(text, text.len(), '}', code), None);
        let text = "    ";
        assert_eq!(closer_indent(text, text.len(), '}', code), None);
    }

    #[test]
    fn closer_skips_brackets_in_strings() {
        let text = "a {\n  s = \"}\"\n  ";
        let quoted = text.find('"').unwrap()..text.rfind('"').unwrap() + 1;
        let region = |i| {
            if quoted.contains(&i) {
                Region::String
            } else {
                Region::Code
            }
        };
        assert_eq!(closer_indent(text, text.len(), '}', region), Some(""));
        // Nor is a bracket typed in one moved.
        let all_string = |_| Region::String;
        assert_eq!(closer_indent(text, text.len(), '}', all_string), None);
    }
}
