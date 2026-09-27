//! Syntax highlighting with tree-sitter.
//!
//! A [`Highlighter`] keeps a syntax tree of a buffer's text and highlights
//! the lines on screen, and a screen's worth either side, with its
//! language's highlight query. After an edit it reparses only what changed,
//! which it finds by comparing the text before and after, so typing, undo,
//! and replacing all go the same way.
//!
//! An [`ExcerptHighlighter`] colors a few lines of a file at a time, for
//! search results, on any thread, and remembers them for the next search.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::{ControlFlow, Range};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use opentui::{EditBuffer, Highlight};
use tree_sitter::{
    InputEdit, ParseOptions, ParseState, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::language::Language;
use crate::theme::{SyntaxColor, Theme};

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
/// How many stretches of lines stay highlighted at once: one per panel
/// showing the text, so panels don't take turns repainting.
const MAX_PAINTED: usize = 4;
/// Larger files' excerpts are left plain: search results come from many
/// files, and each is parsed whole.
pub const MAX_EXCERPT_BYTES: usize = 2 << 20;
/// The excerpt cache forgets the files used longest ago past this many
/// spans, about 24 bytes each.
const MAX_CACHED_SPANS: usize = 1 << 20;

/// A language's grammar and compiled highlight query.
struct Grammar {
    language: tree_sitter::Language,
    query: Query,
    /// Colors by capture index; `None` for captures left uncolored.
    colors: Vec<Option<SyntaxColor>>,
}

/// Grammars by language name; `None` for one whose query doesn't compile.
type Grammars = HashMap<&'static str, Option<Arc<Grammar>>>;

/// Compiled on first use: a big query takes a while. Shared by every thread.
static GRAMMARS: LazyLock<Mutex<Grammars>> = LazyLock::new(Default::default);

fn grammar(language: &'static Language) -> Option<Arc<Grammar>> {
    let syntax = language.syntax.as_ref()?;
    let mut grammars = GRAMMARS.lock().unwrap_or_else(|e| e.into_inner());
    grammars
        .entry(language.name)
        .or_insert_with(|| {
            let grammar = (syntax.grammar)();
            // Neovim's `#lua-match?` takes Lua patterns, which the queries
            // only use where they read the same as regexes.
            let source = syntax.highlights.concat().replace("#lua-match?", "#match?");
            let query = Query::new(&grammar, &source).ok()?;
            let colors = query
                .capture_names()
                .iter()
                .map(|name| SyntaxColor::of(name))
                .collect();
            Some(Arc::new(Grammar {
                language: grammar,
                query,
                colors,
            }))
        })
        .clone()
}

/// Parses `text` with `parser`, reusing `old`, the tree of the text before,
/// for what didn't change. Gives up after `budget`, if any, or once `stop`
/// is set.
fn parse(
    parser: &mut Parser,
    text: &str,
    old: Option<&Tree>,
    budget: Option<Duration>,
    stop: &AtomicBool,
) -> Option<Tree> {
    // Small texts are parsed before the first check of progress.
    if stop.load(Ordering::Relaxed) {
        return None;
    }
    let deadline = budget.map(|budget| Instant::now() + budget);
    let mut progress = |_: &ParseState| {
        let late = deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if !late && !stop.load(Ordering::Relaxed) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    };
    let bytes = text.as_bytes();
    parser.parse_with_options(
        &mut |at, _| &bytes[at.min(bytes.len())..],
        old,
        Some(ParseOptions::new().progress_callback(&mut progress)),
    )
}

/// The styled spans of `bytes` of `text`, which `tree` is the tree of, with
/// each capture's style from `styles`.
fn query_spans<S: Copy>(
    grammar: &Grammar,
    cursor: &mut QueryCursor,
    tree: &Tree,
    text: &str,
    bytes: Range<usize>,
    styles: &[Option<S>],
) -> Vec<Span<S>> {
    cursor.set_byte_range(bytes.clone());
    let mut captures = cursor.captures(&grammar.query, tree.root_node(), text.as_bytes());
    let mut found = Vec::new();
    while let Some((m, i)) = captures.next() {
        let capture = m.captures()[*i];
        found.push(Capture {
            bytes: capture.node.byte_range(),
            node: capture.node.id(),
            style: styles[capture.index as usize],
            pattern: m.pattern_index,
        });
        if found.len() == MAX_CAPTURES {
            break;
        }
    }
    flatten(found, bytes)
}

/// A line's syntax colors, as byte ranges in it, in order.
pub type LineColors = Vec<(Range<usize>, SyntaxColor)>;

/// Highlights a few lines of a file at a time, for search results: the
/// file's parsed whole, so lines inside a block comment or a long string
/// come out right. Runs on any thread.
pub struct ExcerptHighlighter {
    parser: Parser,
    cursor: QueryCursor,
}

impl ExcerptHighlighter {
    pub fn new() -> ExcerptHighlighter {
        ExcerptHighlighter {
            parser: Parser::new(),
            cursor: QueryCursor::new(),
        }
    }

    /// The colors of each of `lines` of `text`, the file at `path` in
    /// `language`. `lines` are byte ranges of `text`, in order, each within
    /// one line. `None` if qedit can't highlight the language, the text is
    /// too big, or parsing was stopped by `stop`. There's no time limit: how
    /// long is too long depends on the machine and the build.
    ///
    /// Lines colored before, of the same text, are remembered, so only
    /// files with lines not seen yet are parsed.
    pub fn highlight(
        &mut self,
        path: &Path,
        language: &'static Language,
        text: &str,
        lines: &[Range<usize>],
        stop: &AtomicBool,
    ) -> Option<Vec<LineColors>> {
        if text.len() > MAX_EXCERPT_BYTES {
            return None;
        }
        let hash = hash(text);
        let mut colors = EXCERPT_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path, hash, lines);
        let missing: Vec<Range<usize>> = lines
            .iter()
            .zip(&colors)
            .filter(|(_, colors)| colors.is_none())
            .map(|(line, _)| line.clone())
            .collect();
        if missing.is_empty() {
            return colors.into_iter().collect();
        }
        let found = self.parse_and_color(language, text, &missing, stop)?;
        EXCERPT_CACHE.lock().unwrap_or_else(|e| e.into_inner()).put(
            path,
            hash,
            missing.iter().cloned().zip(found.iter().cloned()),
        );
        let mut found = found.into_iter();
        for colors in &mut colors {
            if colors.is_none() {
                *colors = found.next();
            }
        }
        colors.into_iter().collect()
    }

    /// Parses `text` and colors `lines` of it, as [`Self::highlight`].
    fn parse_and_color(
        &mut self,
        language: &'static Language,
        text: &str,
        lines: &[Range<usize>],
        stop: &AtomicBool,
    ) -> Option<Vec<LineColors>> {
        let grammar = grammar(language)?;
        self.parser.set_language(&grammar.language).ok()?;
        let tree = parse(&mut self.parser, text, None, None, stop)?;
        let mut colors = Vec::with_capacity(lines.len());
        // Lines that follow each other are queried together: one query
        // per excerpt, not per line.
        for group in lines.chunk_by(|a, b| b.start <= a.end + 2) {
            let bytes = group[0].start..group[group.len() - 1].end;
            let spans = query_spans(
                &grammar,
                &mut self.cursor,
                &tree,
                text,
                bytes,
                &grammar.colors,
            );
            let mut first = 0;
            for line in group {
                // Spans are in order and don't overlap; one can run on into
                // the next line.
                while spans.get(first).is_some_and(|s| s.bytes.end <= line.start) {
                    first += 1;
                }
                let line_colors = spans[first..]
                    .iter()
                    .take_while(|s| s.bytes.start < line.end)
                    .map(|s| {
                        let start = s.bytes.start.max(line.start) - line.start;
                        let end = s.bytes.end.min(line.end) - line.start;
                        (start..end, s.style)
                    })
                    .collect();
                colors.push(line_colors);
            }
        }
        Some(colors)
    }
}

fn hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Excerpts' colors from earlier searches, so typing a longer query, which
/// finds the same lines or fewer, parses nothing again. Keeping the trees
/// would save parsing for any line, but they take ~35 times the text's size.
static EXCERPT_CACHE: LazyLock<Mutex<ExcerptCache>> = LazyLock::new(Default::default);

#[derive(Default)]
struct ExcerptCache {
    files: HashMap<PathBuf, CachedFile>,
    /// Spans in all files.
    spans: usize,
    /// Counts up with each use, for forgetting the files used longest ago.
    clock: u64,
}

struct CachedFile {
    /// Of the text the colors are for.
    hash: u64,
    used: u64,
    /// By the line's byte range.
    lines: HashMap<Range<usize>, LineColors>,
    spans: usize,
}

impl ExcerptCache {
    /// The colors of each of `lines` known for the text with `hash` of the
    /// file at `path`.
    fn get(&mut self, path: &Path, hash: u64, lines: &[Range<usize>]) -> Vec<Option<LineColors>> {
        self.clock += 1;
        let Some(file) = self.files.get_mut(path).filter(|file| file.hash == hash) else {
            return vec![None; lines.len()];
        };
        file.used = self.clock;
        lines
            .iter()
            .map(|line| file.lines.get(line).cloned())
            .collect()
    }

    /// Remembers the colors of `lines` of the text with `hash`.
    fn put(
        &mut self,
        path: &Path,
        hash: u64,
        lines: impl Iterator<Item = (Range<usize>, LineColors)>,
    ) {
        let file = self.file(path, hash);
        let mut added = 0;
        for (line, colors) in lines {
            added += colors.len().max(1);
            if let Some(old) = file.lines.insert(line, colors) {
                added -= old.len().max(1);
            }
        }
        file.spans += added;
        self.spans += added;
        self.forget_old();
    }

    /// The entry for the text with `hash` of the file at `path`, emptied if
    /// it was for another text.
    fn file(&mut self, path: &Path, hash: u64) -> &mut CachedFile {
        self.clock += 1;
        let file = self.files.entry(path.to_path_buf()).or_insert(CachedFile {
            hash,
            used: 0,
            lines: HashMap::new(),
            spans: 0,
        });
        if file.hash != hash {
            self.spans -= file.spans;
            *file = CachedFile {
                hash,
                used: 0,
                lines: HashMap::new(),
                spans: 0,
            };
        }
        file.used = self.clock;
        file
    }

    fn forget_old(&mut self) {
        while self.spans > MAX_CACHED_SPANS {
            let Some(oldest) = self
                .files
                .iter()
                .min_by_key(|(_, file)| file.used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            let file = self.files.remove(&oldest).unwrap();
            self.spans -= file.spans;
        }
    }
}

pub struct Highlighter {
    grammar: Arc<Grammar>,
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
    /// The stretches of lines highlighted since, least recently asked for
    /// first.
    painted: Vec<Range<u32>>,
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
            painted: Vec::new(),
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
            self.painted.clear();
            if !self.reparse(buffer.text()) {
                self.gave_up = true;
                self.tree = None;
                self.text = String::new();
                self.line_starts = vec![0];
                buffer.remove_highlights(HIGHLIGHTS);
                return;
            }
        }
        if let Some(i) = self
            .painted
            .iter()
            .position(|p| p.start <= visible.start && visible.end <= p.end)
        {
            let painted = self.painted.remove(i);
            self.painted.push(painted);
            return;
        }
        let margin = visible.len() as u32;
        let lines = visible.start.saturating_sub(margin)
            ..(visible.end + margin).min(self.line_starts.len() as u32);
        self.painted.push(lines);
        if self.painted.len() > MAX_PAINTED {
            self.painted.remove(0);
        }
        // Overlapping stretches are highlighted once.
        let mut stretches = self.painted.clone();
        stretches.sort_by_key(|lines| lines.start);
        let mut merged: Vec<Range<u32>> = Vec::new();
        for lines in stretches {
            match merged.last_mut() {
                Some(last) if lines.start <= last.end => last.end = last.end.max(lines.end),
                _ => merged.push(lines),
            }
        }
        let highlights: Vec<Highlight> = merged
            .into_iter()
            .flat_map(|lines| self.highlights(buffer, lines))
            .collect();
        buffer.replace_highlights(HIGHLIGHTS, &highlights);
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
        let never = AtomicBool::new(false);
        self.tree = parse(
            &mut self.parser,
            &text,
            self.tree.as_ref(),
            Some(PARSE_BUDGET),
            &never,
        );
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
    fn spans(&mut self, bytes: Range<usize>) -> Vec<Span<u32>> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        query_spans(
            &self.grammar,
            &mut self.cursor,
            tree,
            &self.text,
            bytes,
            &self.styles,
        )
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

/// A node a query captured, and the style `S` it gets.
struct Capture<S> {
    bytes: Range<usize>,
    /// Which node, as [`tree_sitter::Node::id`].
    node: usize,
    /// `None` for captures the theme leaves uncolored.
    style: Option<S>,
    /// The pattern that captured it, in the query's order.
    pattern: usize,
}

#[derive(Debug, PartialEq)]
struct Span<S> {
    bytes: Range<usize>,
    style: S,
}

/// Captures as spans that don't overlap, in order, clipped to `within`.
/// Where captures nest, the inner one's style shows. Where several patterns
/// capture the same node, the last one's does, as in tree-sitter's own
/// highlighter: queries go from general patterns to specific ones. That
/// holds even when the theme leaves the last one uncolored.
fn flatten<S: Copy>(mut captures: Vec<Capture<S>>, within: Range<usize>) -> Vec<Span<S>> {
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
    let mut push = |start: usize, end: usize, style: S| {
        let bytes = start.max(within.start)..end.min(within.end);
        if !bytes.is_empty() {
            spans.push(Span { bytes, style });
        }
    };
    // The captures around `at`, innermost last, as (end, style).
    let mut open: Vec<(usize, S)> = Vec::new();
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

    fn capture(bytes: Range<usize>, style: Option<u32>, pattern: usize) -> Capture<u32> {
        Capture {
            node: bytes.start << 16 | bytes.end,
            bytes,
            style,
            pattern,
        }
    }

    fn spans(captures: Vec<Capture<u32>>, within: Range<usize>) -> Vec<(Range<usize>, u32)> {
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
    fn keeps_several_stretches_highlighted() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let mut highlighter = Highlighter::new(rust(), &theme).unwrap();
        let buffer = EditBuffer::new(opentui::WidthMethod::Unicode).unwrap();
        buffer.set_text(&"fn f() {}\n".repeat(300));
        // As two panels draw, far apart, and draw again.
        highlighter.sync(&buffer, 0..10);
        highlighter.sync(&buffer, 200..210);
        let painted = highlighter.painted.clone();
        assert_eq!(painted, [0..20, 190..220]);
        highlighter.sync(&buffer, 0..10);
        assert_eq!(
            highlighter.painted,
            [190..220, 0..20],
            "nothing new to paint"
        );
        // The least recently drawn goes first.
        for start in [50, 100, 250] {
            highlighter.sync(&buffer, start..start + 10);
        }
        assert_eq!(highlighter.painted.len(), MAX_PAINTED);
        assert!(!highlighter.painted.contains(&(190..220)));
    }

    #[test]
    fn excerpts_are_highlighted_from_the_whole_file() {
        let text = "fn a() {}\n/* one\ntwo\nthree */\nlet s = \"x\";\nfn b() {}\n";
        let line = |n: usize| {
            let start = line_starts(text)[n];
            start..start + text[start..].find('\n').unwrap()
        };
        // Lines 2 and 3 are one excerpt, 5 another.
        let lines = [line(2), line(3), line(5)];
        let never = AtomicBool::new(false);
        let path = Path::new("/excerpts/whole_file.rs");
        let colors = ExcerptHighlighter::new()
            .highlight(path, rust(), text, &lines, &never)
            .unwrap();
        let comment = SyntaxColor::of("comment");
        let keyword = SyntaxColor::of("keyword");
        // Inside the comment, though the excerpt doesn't show where it
        // starts; clipped to each line.
        assert_eq!(colors[0], [(0..3, comment.unwrap())]);
        assert_eq!(colors[1], [(0..8, comment.unwrap())]);
        assert_eq!(colors[2].first(), Some(&(0..2, keyword.unwrap())));

        let stopped = AtomicBool::new(true);
        let path = Path::new("/excerpts/stopped.rs");
        let colors = ExcerptHighlighter::new().highlight(path, rust(), text, &lines, &stopped);
        assert!(colors.is_none());
    }

    #[test]
    fn excerpts_are_remembered_for_the_same_text() {
        let text = "/* a\nb */\nfn c() {}\nfn d() {}\n";
        let line = |n: usize| {
            let start = line_starts(text)[n];
            start..start + text[start..].find('\n').unwrap()
        };
        let path = Path::new("/excerpts/remembered.rs");
        let mut highlighter = ExcerptHighlighter::new();
        let never = AtomicBool::new(false);
        let first = highlighter
            .highlight(path, rust(), text, &[line(1), line(2)], &never)
            .unwrap();
        // Stopped, so nothing can be parsed: only what's remembered comes back.
        let stopped = AtomicBool::new(true);
        let mut again = |lines: &[Range<usize>], text: &str| {
            highlighter.highlight(path, rust(), text, lines, &stopped)
        };
        assert_eq!(again(&[line(1), line(2)], text).as_ref(), Some(&first));
        assert_eq!(again(&[line(2)], text).as_deref(), Some(&first[1..]));
        assert!(again(&[line(3)], text).is_none(), "not seen yet");
        let changed = text.replace("fn c", "fn e");
        assert!(again(&[line(1)], &changed).is_none(), "another text");

        // Parsing the new lines keeps the ones seen before.
        let both = highlighter
            .highlight(path, rust(), text, &[line(2), line(3)], &never)
            .unwrap();
        assert_eq!(both[0], first[1]);
        assert_eq!(both[1].first().map(|c| c.1), SyntaxColor::of("keyword"));
    }

    #[test]
    fn the_excerpt_cache_forgets_what_was_used_longest_ago() {
        let mut cache = ExcerptCache::default();
        let color = SyntaxColor::of("comment").unwrap();
        let spans = |n: usize| vec![(0..1, color); n];
        let (a, b, c) = (Path::new("a"), Path::new("b"), Path::new("c"));
        cache.put(a, 1, [(0..1, spans(MAX_CACHED_SPANS / 2))].into_iter());
        cache.put(b, 1, [(0..1, spans(MAX_CACHED_SPANS / 2))].into_iter());
        cache.get(a, 1, std::slice::from_ref(&(0..1)));
        cache.put(c, 1, [(0..1, spans(10))].into_iter());
        assert!(cache.files.contains_key(a) && cache.files.contains_key(c));
        assert!(!cache.files.contains_key(b));
        assert_eq!(cache.spans, MAX_CACHED_SPANS / 2 + 10);

        // Another text replaces a file's colors.
        cache.put(c, 2, [(5..6, spans(3))].into_iter());
        assert_eq!(cache.get(c, 2, &[0..1, 5..6]), [None, Some(spans(3))]);
        assert_eq!(cache.spans, MAX_CACHED_SPANS / 2 + 3);
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
