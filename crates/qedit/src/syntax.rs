//! Syntax highlighting with tree-sitter.
//!
//! A [`Highlighter`] keeps a syntax tree of a buffer's text and highlights
//! the lines on screen, and a screen's worth either side, with its
//! language's highlight query. After an edit it reparses only what changed,
//! which it finds by comparing the text before and after, so typing, undo,
//! and replacing all go the same way.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};
use std::rc::Rc;
use std::time::{Duration, Instant};

use opentui::{EditBuffer, Highlight};
use tree_sitter::{
    InputEdit, ParseOptions, ParseState, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::language::Language;
use crate::theme::Theme;

/// Tags syntax highlights.
pub const HIGHLIGHTS: u16 = 2;
/// Larger files are left plain.
const MAX_BYTES: usize = 8 << 20;
/// Parsing that takes longer gives up and leaves the file plain, rather than
/// hold up the editor.
const PARSE_BUDGET: Duration = Duration::from_millis(300);
/// At most this many captures are highlighted at once, which bounds the work
/// for a screen of very long lines.
const MAX_CAPTURES: usize = 20_000;

/// A language's grammar and compiled highlight query.
struct Grammar {
    language: tree_sitter::Language,
    query: Query,
}

thread_local! {
    /// By language name, compiled on first use: a big query takes a while.
    static GRAMMARS: RefCell<HashMap<&'static str, Option<Rc<Grammar>>>> =
        RefCell::default();
}

fn grammar(language: &'static Language) -> Option<Rc<Grammar>> {
    let syntax = language.syntax.as_ref()?;
    GRAMMARS.with_borrow_mut(|grammars| {
        grammars
            .entry(language.name)
            .or_insert_with(|| {
                let grammar = (syntax.grammar)();
                // Neovim's `#lua-match?` takes Lua patterns, which the
                // queries only use where they read the same as regexes.
                let source = syntax.highlights.concat().replace("#lua-match?", "#match?");
                let query = Query::new(&grammar, &source).ok()?;
                Some(Rc::new(Grammar {
                    language: grammar,
                    query,
                }))
            })
            .clone()
    })
}

pub struct Highlighter {
    grammar: Rc<Grammar>,
    /// Style ids by capture index; `None` for captures left uncolored.
    styles: Vec<Option<u32>>,
    parser: Parser,
    cursor: QueryCursor,
    /// The tree of `text`.
    tree: Option<Tree>,
    text: String,
    /// Where each line of `text` starts.
    line_starts: Vec<usize>,
    /// The buffer's content epoch when `text` was taken.
    epoch: Option<u64>,
    /// The lines highlighted since.
    painted: Option<Range<u32>>,
    /// The text was too big, or took too long to parse; it stays plain.
    gave_up: bool,
}

impl Highlighter {
    /// A highlighter for text in `language`, if qedit can highlight it.
    pub fn new(language: &'static Language, theme: &Theme) -> Option<Highlighter> {
        let grammar = grammar(language)?;
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).ok()?;
        let styles = grammar
            .query
            .capture_names()
            .iter()
            .map(|name| theme.capture_style(name))
            .collect();
        Some(Highlighter {
            grammar,
            styles,
            parser,
            cursor: QueryCursor::new(),
            tree: None,
            text: String::new(),
            line_starts: vec![0],
            epoch: None,
            painted: None,
            gave_up: false,
        })
    }

    /// Highlights `buffer`'s lines `visible`, and a screen's worth either
    /// side, reparsing first if its text changed.
    pub fn sync(&mut self, buffer: &EditBuffer, visible: Range<u32>) {
        if self.gave_up || visible.is_empty() {
            return;
        }
        let epoch = buffer.content_epoch();
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.painted = None;
            if !self.reparse(buffer.text()) {
                self.gave_up = true;
                self.tree = None;
                self.text = String::new();
                self.line_starts = vec![0];
                buffer.remove_highlights(HIGHLIGHTS);
                return;
            }
        }
        if self
            .painted
            .as_ref()
            .is_some_and(|p| p.start <= visible.start && visible.end <= p.end)
        {
            return;
        }
        let margin = visible.len() as u32;
        let lines = visible.start.saturating_sub(margin)
            ..(visible.end + margin).min(self.line_starts.len() as u32);
        let highlights = self.highlights(buffer, lines.clone());
        buffer.replace_highlights(HIGHLIGHTS, &highlights);
        self.painted = Some(lines);
    }

    /// Parses `text`, reusing the tree of the text before for what didn't
    /// change. False if the text is too big or took too long to parse.
    fn reparse(&mut self, text: String) -> bool {
        if text.len() > MAX_BYTES {
            return false;
        }
        let line_starts = line_starts(&text);
        if let Some(tree) = &mut self.tree {
            match edit(&self.text, &self.line_starts, &text, &line_starts) {
                Some(edit) => tree.edit(&edit),
                None => return true,
            }
        }
        let deadline = Instant::now() + PARSE_BUDGET;
        let mut progress = |_: &ParseState| {
            if Instant::now() < deadline {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        };
        let bytes = text.as_bytes();
        let tree = self.parser.parse_with_options(
            &mut |at, _| &bytes[at.min(bytes.len())..],
            self.tree.as_ref(),
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        self.tree = tree;
        self.text = text;
        self.line_starts = line_starts;
        self.tree.is_some()
    }

    fn line_start(&self, line: u32) -> usize {
        self.line_starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.text.len())
    }

    /// The styled spans of `bytes` of the text.
    fn spans(&mut self, bytes: Range<usize>) -> Vec<Span> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        self.cursor.set_byte_range(bytes.clone());
        let mut captures =
            self.cursor
                .captures(&self.grammar.query, tree.root_node(), self.text.as_bytes());
        let mut found = Vec::new();
        while let Some((m, i)) = captures.next() {
            let capture = m.captures()[*i];
            found.push(Capture {
                bytes: capture.node.byte_range(),
                node: capture.node.id(),
                style: self.styles[capture.index as usize],
                pattern: m.pattern_index,
            });
            if found.len() == MAX_CAPTURES {
                break;
            }
        }
        flatten(found, bytes)
    }

    /// The highlights for lines `lines`.
    fn highlights(&mut self, buffer: &EditBuffer, lines: Range<u32>) -> Vec<Highlight> {
        let bytes = self.line_start(lines.start)..self.line_start(lines.end);
        let spans = self.spans(bytes);

        // Highlights are by line and column: tabs and wide characters make
        // columns differ from bytes.
        let offsets: Vec<u32> = spans
            .iter()
            .flat_map(|span| [span.bytes.start as u32, span.bytes.end as u32])
            .collect();
        let cursors = buffer.bytes_to_cursors(&offsets);
        let mut highlights = Vec::new();
        for (span, ends) in spans.iter().zip(cursors.chunks(2)) {
            let (start, end) = (ends[0], ends[1]);
            // A span over several lines, like a block comment, takes the
            // rest of its first line and the start of its last.
            for line in start.row.max(lines.start)..=end.row.min(lines.end - 1) {
                highlights.push(Highlight {
                    line,
                    start: if line == start.row { start.col } else { 0 },
                    end: if line == end.row { end.col } else { u32::MAX },
                    style: span.style,
                    priority: 0,
                    tag: HIGHLIGHTS,
                });
            }
        }
        highlights
    }
}

/// A node a query captured, and the style it gets.
struct Capture {
    bytes: Range<usize>,
    /// Which node, as [`tree_sitter::Node::id`].
    node: usize,
    /// `None` for captures the theme leaves uncolored.
    style: Option<u32>,
    /// The pattern that captured it, in the query's order.
    pattern: usize,
}

#[derive(Debug, PartialEq)]
struct Span {
    bytes: Range<usize>,
    style: u32,
}

/// Captures as spans that don't overlap, in order, clipped to `within`.
/// Where captures nest, the inner one's style shows. Where several patterns
/// capture the same node, the last one's does, as in tree-sitter's own
/// highlighter: queries go from general patterns to specific ones. That
/// holds even when the theme leaves the last one uncolored.
fn flatten(mut captures: Vec<Capture>, within: Range<usize>) -> Vec<Span> {
    // Nodes with the same text, like a node and its only child, come in no
    // particular order.
    captures.sort_by_key(|c| (c.bytes.start, Reverse(c.bytes.end), c.node, c.pattern));
    captures.dedup_by(|later, earlier| {
        let same = later.node == earlier.node;
        if same {
            std::mem::swap(later, earlier);
        }
        same
    });
    let captures = captures
        .into_iter()
        .filter_map(|c| Some((c.bytes, c.style?)));
    let mut spans = Vec::new();
    let mut push = |start: usize, end: usize, style: u32| {
        let bytes = start.max(within.start)..end.min(within.end);
        if !bytes.is_empty() {
            spans.push(Span { bytes, style });
        }
    };
    // The captures around `at`, innermost last, as (end, style).
    let mut open: Vec<(usize, u32)> = Vec::new();
    let mut at = 0;
    for (bytes, style) in captures {
        while let Some(&(end, outer)) = open.last() {
            if end > bytes.start {
                break;
            }
            push(at, end, outer);
            at = at.max(end);
            open.pop();
        }
        if let Some(&(_, outer)) = open.last() {
            push(at, bytes.start, outer);
        }
        at = at.max(bytes.start);
        let end = open
            .last()
            .map_or(bytes.end, |&(outer, _)| bytes.end.min(outer));
        open.push((end, style));
    }
    while let Some((end, style)) = open.pop() {
        push(at, end, style);
        at = at.max(end);
    }
    spans
}

/// Where each line of `text` starts.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', text.as_bytes()).map(|i| i + 1))
        .collect()
}

/// The edit that turns `old` into `new`: the bytes between what they start
/// and end with in common. `None` if they're the same.
fn edit(old: &str, old_lines: &[usize], new: &str, new_lines: &[usize]) -> Option<InputEdit> {
    if old == new {
        return None;
    }
    let boundary =
        |old_at: usize, new_at: usize| old.is_char_boundary(old_at) && new.is_char_boundary(new_at);
    let mut start = common_prefix(old.as_bytes(), new.as_bytes());
    while !boundary(start, start) {
        start -= 1;
    }
    let mut suffix = common_suffix(&old.as_bytes()[start..], &new.as_bytes()[start..]);
    while !boundary(old.len() - suffix, new.len() - suffix) {
        suffix -= 1;
    }
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;
    Some(InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: point(old_lines, start),
        old_end_position: point(old_lines, old_end),
        new_end_position: point(new_lines, new_end),
    })
}

/// How many bytes `a` and `b` start with in common. Compares blocks first,
/// which is much faster than byte by byte.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const BLOCK: usize = 64;
    let blocks = a
        .chunks_exact(BLOCK)
        .zip(b.chunks_exact(BLOCK))
        .take_while(|(a, b)| a == b)
        .count();
    let at = blocks * BLOCK;
    at + a[at..]
        .iter()
        .zip(&b[at..])
        .take_while(|(a, b)| a == b)
        .count()
}

/// How many bytes `a` and `b` end with in common.
fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    const BLOCK: usize = 64;
    let blocks = a
        .rchunks_exact(BLOCK)
        .zip(b.rchunks_exact(BLOCK))
        .take_while(|(a, b)| a == b)
        .count();
    let at = blocks * BLOCK;
    let (a, b) = (&a[..a.len() - at], &b[..b.len() - at]);
    at + a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
}

/// The row and byte column of byte `at`.
fn point(line_starts: &[usize], at: usize) -> Point {
    let row = line_starts.partition_point(|&start| start <= at) - 1;
    Point {
        row,
        column: at - line_starts[row],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language;
    use std::path::Path;

    fn rust() -> &'static Language {
        language::detect(Some(Path::new("a.rs")), String::new).unwrap()
    }

    #[test]
    fn every_highlight_query_compiles() {
        for language in language::all() {
            if language.syntax.is_some() {
                assert!(grammar(language).is_some(), "{}", language.name);
            }
        }
    }

    /// A capture of the node with `bytes`, standing in for its id.
    /// The style of the first `needle` in `text`, highlighted as a file
    /// called `name`.
    fn style_of(theme: &Theme, name: &str, text: &str, needle: &str) -> Option<u32> {
        let language = language::detect(Some(Path::new(name)), String::new).unwrap();
        let mut highlighter = Highlighter::new(language, theme).unwrap();
        assert!(highlighter.reparse(text.to_string()));
        let at = text.find(needle).unwrap();
        let spans = highlighter.spans(0..text.len());
        spans
            .into_iter()
            .find(|span| span.bytes.contains(&at))
            .map(|span| span.style)
    }

    #[test]
    fn highlights_every_language() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        // (file, text, what to look at, the capture it should look like)
        let cases: &[(&str, &str, &str, Option<&str>)] = &[
            ("a.rs", "const MAX: u8 = 1;", "MAX", Some("constant")),
            // As in tree-sitter's own highlighter: the call pattern comes
            // after the one for uppercase names.
            ("a.rs", "let x = Some(1);", "Some", Some("function")),
            ("a.rs", "fn f(v: Vec<u8>) {}", "Vec", Some("type")),
            (
                "a.toml",
                "[package]\nname = \"q\" # c",
                "\"q\"",
                Some("string"),
            ),
            // Keys are `type` inside a `property`: tree-sitter's own
            // highlighter drops the inner capture, as they start together.
            (
                "a.toml",
                "[package]\nname = \"q\" # c",
                "name",
                Some("type"),
            ),
            (
                "a.toml",
                "[package]\nname = \"q\" # c",
                "# c",
                Some("comment"),
            ),
            ("a.md", "# Title\n\ntext\n", "Title", Some("text.title")),
            (
                "a.md",
                "text\n\n```\ncode\n```\n",
                "code",
                Some("text.literal"),
            ),
            // Keys are strings: `@string` comes after `@string.special.key`.
            ("a.json", "{\"a\": 1, \"b\": true}", "\"a\"", Some("string")),
            ("a.json", "{\"a\": 1, \"b\": true}", "1", Some("number")),
            (
                "a.json",
                "{\"a\": 1, \"b\": true}",
                "true",
                Some("constant"),
            ),
            ("a.py", "def f():\n    return \"x\"", "def", Some("keyword")),
            ("a.py", "def f():\n    return \"x\"", "f(", Some("function")),
            (
                "a.py",
                "def f():\n    return \"x\"",
                "\"x\"",
                Some("string"),
            ),
            (
                "a.js",
                "function f() { return 1; }",
                "function",
                Some("keyword"),
            ),
            ("a.js", "function f() { return 1; }", "f(", Some("function")),
            ("a.js", "const s = `a${b}`;", "`a", Some("string")),
            ("a.jsx", "const a = <div id=\"x\" />;", "div", Some("tag")),
            (
                "a.ts",
                "interface A { x: number }",
                "interface",
                Some("keyword"),
            ),
            ("a.ts", "interface A { x: number }", "number", Some("type")),
            ("a.tsx", "const a = <div id={1} />;", "div", Some("tag")),
            ("a.tsx", "let n: string = f<T>();", "string", Some("type")),
            (
                "a.go",
                "package main\nfunc f() int { return 0 }",
                "func",
                Some("keyword"),
            ),
            (
                "a.go",
                "package main\nfunc f() int { return 0 }",
                "int",
                Some("type"),
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "return",
                Some("keyword"),
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "main",
                Some("function"),
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "// c",
                Some("comment"),
            ),
            (
                "a.cpp",
                "class A { public: int x; };",
                "class",
                Some("keyword"),
            ),
            ("a.cpp", "class A { public: int x; };", "int", Some("type")),
            ("a.sh", "echo \"hi\" # c", "\"hi\"", Some("string")),
            ("a.sh", "echo \"hi\" # c", "# c", Some("comment")),
            ("a.sh", "if true; then echo; fi", "then", Some("keyword")),
            ("a.yaml", "a: \"b\" # c", "\"b\"", Some("string")),
            ("a.yaml", "a: \"b\" # c", "# c", Some("comment")),
            ("a.html", "<p class=\"x\">hi</p>", "p ", Some("tag")),
            (
                "a.html",
                "<p class=\"x\">hi</p>",
                "class",
                Some("attribute"),
            ),
            ("a.html", "<p class=\"x\">hi</p>", "hi", None),
            (
                "a.css",
                "a { color: red; } /* c */",
                "/* c",
                Some("comment"),
            ),
            ("a.css", "a { color: red; } /* c */", "a ", Some("tag")),
            (
                "a.zig",
                "const std = @import(\"std\");",
                "const",
                Some("keyword"),
            ),
            (
                "a.zig",
                "const std = @import(\"std\");",
                "\"std\"",
                Some("string"),
            ),
            // `#lua-match?` works: lowercase names aren't types.
            ("a.zig", "const std = @import(\"std\");", "std ", None),
            ("a.zig", "const T = struct {};", "T ", Some("type")),
            ("a.zig", "pub fn main() void {}", "main", Some("function")),
        ];
        let mut wrong = Vec::new();
        for &(name, text, needle, capture) in cases {
            let expected = capture.and_then(|c| theme.capture_style(c));
            assert!(
                capture.is_none() || expected.is_some(),
                "{capture:?} has no style"
            );
            let got = style_of(&theme, name, text, needle);
            if got != expected {
                wrong.push(format!(
                    "{name} {needle:?}: want {capture:?}, got style {got:?}"
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    fn capture(bytes: Range<usize>, style: Option<u32>, pattern: usize) -> Capture {
        Capture {
            node: bytes.start << 16 | bytes.end,
            bytes,
            style,
            pattern,
        }
    }

    fn spans(captures: Vec<Capture>, within: Range<usize>) -> Vec<(Range<usize>, u32)> {
        flatten(captures, within)
            .into_iter()
            .map(|s| (s.bytes, s.style))
            .collect()
    }

    #[test]
    fn flatten_shows_inner_captures_and_last_patterns() {
        // `#[derive(Debug)]`: an attribute around a bracket and a type.
        let captures = vec![
            capture(9..14, Some(3), 0),
            capture(0..16, Some(1), 2),
            capture(8..9, Some(2), 1),
            // The same node again, by a later pattern.
            capture(9..14, Some(4), 5),
        ];
        assert_eq!(
            spans(captures, 0..100),
            [(0..8, 1), (8..9, 2), (9..14, 4), (14..16, 1)]
        );
        // The same, from the other order.
        let captures = vec![capture(9..14, Some(4), 5), capture(9..14, Some(3), 0)];
        assert_eq!(spans(captures, 0..100), [(9..14, 4)]);
        // A last pattern the theme doesn't color leaves the node uncolored,
        // showing what's around it.
        let captures = vec![
            capture(0..16, Some(1), 0),
            capture(9..14, Some(3), 1),
            capture(9..14, None, 2),
        ];
        assert_eq!(spans(captures, 0..100), [(0..16, 1)]);
        // Clipped to where lines are highlighted.
        let captures = vec![capture(0..10, Some(1), 0), capture(12..20, Some(2), 0)];
        assert_eq!(spans(captures, 5..15), [(5..10, 1), (12..15, 2)]);
    }

    fn edited(old: &str, new: &str) -> Option<(Range<usize>, usize, Point, Point, Point)> {
        edit(old, &line_starts(old), new, &line_starts(new)).map(|e| {
            (
                e.start_byte..e.old_end_byte,
                e.new_end_byte,
                e.start_position,
                e.old_end_position,
                e.new_end_position,
            )
        })
    }

    #[test]
    fn edits_are_what_changed_between_texts() {
        let p = |row, column| Point { row, column };
        assert_eq!(edited("same", "same"), None);
        assert_eq!(
            edited("ab\ncd", "ab\nxcd"),
            Some((3..3, 4, p(1, 0), p(1, 0), p(1, 1)))
        );
        assert_eq!(
            edited("ab\ncd\nef", "ab\nef"),
            Some((3..6, 3, p(1, 0), p(2, 0), p(1, 0)))
        );
        // Replacing é with è: they share a lead byte, but edits are whole
        // characters.
        assert_eq!(
            edited("aé", "aè"),
            Some((1..3, 3, p(0, 1), p(0, 3), p(0, 3)))
        );
        // Long common parts, past the block size.
        let a = "x".repeat(200);
        assert_eq!(
            edited(&format!("{a}1{a}"), &format!("{a}22{a}")),
            Some((200..201, 202, p(0, 200), p(0, 201), p(0, 202)))
        );
    }

    #[test]
    fn incremental_parses_match_parsing_from_scratch() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let mut highlighter = Highlighter::new(rust(), &theme).unwrap();
        let steps = [
            "fn main() {}\n",
            "fn main() {\n    let s = \"é\";\n}\n",
            "/* open\nfn main() {\n    let s = \"é\";\n}\n",
            "/* open */\nfn main() {\n    let s = \"è\";\n}\n",
            "fn main() {\n    let s = \"è\";\n}\n",
            "",
            "struct S;",
        ];
        for text in steps {
            assert!(highlighter.reparse(text.to_string()));
            let mut parser = Parser::new();
            parser.set_language(&highlighter.grammar.language).unwrap();
            let fresh = parser.parse(text, None).unwrap();
            assert_eq!(
                highlighter.tree.as_ref().unwrap().root_node().to_sexp(),
                fresh.root_node().to_sexp(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn gives_up_on_huge_files() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let mut highlighter = Highlighter::new(rust(), &theme).unwrap();
        let huge = "// comment\n".repeat(MAX_BYTES / 10);
        assert!(!highlighter.reparse(huge));
    }
}
