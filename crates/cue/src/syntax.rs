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
//!
//! Both color text injected into a language as the language it is, like a
//! Markdown code block's, or its paragraphs' inline markup: those are
//! parsed as the lines they're in are highlighted.

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
    InputEdit, Node, ParseOptions, ParseState, Parser, Point, Query, QueryCursor,
    StreamingIterator, Tree,
};

use crate::language::{self, Language};
use crate::theme::{SyntaxColor, SyntaxStyles, Theme};

/// Tags syntax highlights.
pub const HIGHLIGHTS: u16 = 2;
/// Larger files are left plain.
const MAX_BYTES: usize = 8 << 20;
/// Parsing that takes longer gives up and leaves the file plain, rather than
/// hold up the editor. So does parsing the text injected into the lines on
/// screen, which leaves what's left of it the colors of the text it's in.
const PARSE_BUDGET: Duration = Duration::from_millis(300);
/// How deep injections nest: Markdown's inline markup is one deep, HTML in
/// it two, and a script in that three.
const MAX_INJECTION_DEPTH: u32 = 3;
/// At most this many injections are highlighted at once, which bounds the
/// work for a screen of very long lines.
const MAX_INJECTIONS: usize = 1000;
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

/// A language's grammar and compiled queries.
struct Grammar {
    language: tree_sitter::Language,
    query: Query,
    /// How each capture affects highlighting, by capture index.
    captures: Vec<CaptureRule>,
    /// Where other languages are injected into it, if anywhere.
    injections: Option<InjectionQuery>,
}

/// Helper captures must not erase a color; unstyled captures can.
#[derive(Clone, Copy)]
enum CaptureRule {
    Ignore,
    Uncolored,
    Styled(SyntaxColor),
}

impl CaptureRule {
    fn of(name: &str) -> Self {
        if name.starts_with('_') || ["spell", "nospell", "conceal"].contains(&name) {
            Self::Ignore
        } else {
            SyntaxColor::of(name).map_or(Self::Uncolored, Self::Styled)
        }
    }
}

#[derive(Clone, Copy)]
enum QueryKind {
    Highlights,
    Injections,
}

struct InjectionQuery {
    query: Query,
    /// The capture of the injected text.
    content: u32,
    /// The capture of the injected language's name, if the query names
    /// languages by text, like a code block's info string.
    language: Option<u32>,
}

/// Grammars by language name; `None` for one whose query doesn't compile.
type Grammars = HashMap<&'static str, Option<Arc<Grammar>>>;

/// Compiled on first use: a big query takes a while. Shared by every thread.
static GRAMMARS: LazyLock<Mutex<Grammars>> = LazyLock::new(Default::default);

fn grammar_of(language: &'static Language) -> Option<Arc<Grammar>> {
    let syntax = language.syntax.as_ref()?;
    let mut grammars = GRAMMARS.lock().unwrap_or_else(|e| e.into_inner());
    grammars
        .entry(language.name)
        .or_insert_with(|| {
            let grammar = (syntax.grammar)();
            let query = compile_query(&grammar, syntax.highlights, QueryKind::Highlights).ok()?;
            let captures = query
                .capture_names()
                .iter()
                .map(|name| CaptureRule::of(name))
                .collect();
            let injections = if syntax.injections.is_empty() {
                None
            } else {
                let query =
                    compile_query(&grammar, syntax.injections, QueryKind::Injections).ok()?;
                Some(InjectionQuery {
                    content: query.capture_index_for_name("injection.content")?,
                    language: query.capture_index_for_name("injection.language"),
                    query,
                })
            };
            Some(Arc::new(Grammar {
                language: grammar,
                query,
                captures,
                injections,
            }))
        })
        .clone()
}

/// Tree-sitter evaluates text predicates itself, but leaves other predicates
/// and properties to its caller. Reject unknown ones instead of silently
/// treating their patterns as unconditional. Imported queries must use regex
/// predicates directly; Lua patterns are not translated at runtime.
fn compile_query(
    grammar: &tree_sitter::Language,
    sources: &[&str],
    kind: QueryKind,
) -> Result<Query, String> {
    let query = Query::new(grammar, &sources.join("\n")).map_err(|e| e.to_string())?;
    for pattern in 0..query.pattern_count() {
        if let Some(predicate) = query.general_predicates(pattern).first() {
            return Err(format!(
                "pattern {pattern}: unsupported #{}",
                predicate.operator
            ));
        }
        for (property, positive) in query.property_predicates(pattern) {
            // No locals query: all identifiers are treated as nonlocal. This
            // preserves upstream builtin-name highlighting without scope tracking.
            if !matches!(kind, QueryKind::Highlights)
                || property.key.as_ref() != "local"
                || *positive
                || property.value.is_some()
                || property.capture_id.is_some()
            {
                return Err(format!(
                    "pattern {pattern}: unsupported property predicate {property:?}"
                ));
            }
        }
        for property in query.property_settings(pattern) {
            let supported = property.capture_id.is_none()
                && match kind {
                    // cue resolves overlaps by nesting and query order, ignoring
                    // Neovim's numeric priority annotation deliberately.
                    QueryKind::Highlights => {
                        property.key.as_ref() == "priority"
                            && property
                                .value
                                .as_deref()
                                .is_some_and(|v| v.parse::<u32>().is_ok())
                    }
                    QueryKind::Injections => match property.key.as_ref() {
                        "injection.language" => property.value.is_some(),
                        "injection.include-children" => property.value.is_none(),
                        // Each captured node is parsed separately; combined injections
                        // are an explicit approximation, not concatenated documents.
                        "injection.combined" => property.value.is_none(),
                        _ => false,
                    },
                };
            if !supported {
                return Err(format!(
                    "pattern {pattern}: unsupported setting {property:?}"
                ));
            }
        }
    }
    Ok(query)
}

/// Parses `text` with `parser`, reusing `old`, the tree of the text before,
/// for what didn't change. Gives up at `deadline`, if any, or once `stop`
/// is set.
fn parse(
    parser: &mut Parser,
    text: &str,
    old: Option<&Tree>,
    deadline: Option<Instant>,
    stop: &AtomicBool,
) -> Option<Tree> {
    // Small texts are parsed before the first check of progress.
    if stop.load(Ordering::Relaxed) {
        return None;
    }
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

/// What `grammar`'s highlight query captures in `bytes` of `text`, which
/// `tree` is the tree of.
fn query_captures(
    grammar: &Grammar,
    cursor: &mut QueryCursor,
    tree: &Tree,
    text: &str,
    bytes: Range<usize>,
) -> Vec<Capture<SyntaxColor>> {
    cursor.set_byte_range(bytes);
    let mut captures = cursor.captures(&grammar.query, tree.root_node(), text.as_bytes());
    let mut found = Vec::new();
    while let Some((m, i)) = captures.next() {
        let capture = m.captures()[*i];
        let style = match grammar.captures[capture.index as usize] {
            CaptureRule::Ignore => continue,
            CaptureRule::Uncolored => None,
            CaptureRule::Styled(color) => Some(color),
        };
        found.push(Capture {
            bytes: capture.node.byte_range(),
            node: capture.node.id(),
            style,
            pattern: m.pattern_index,
        });
        if found.len() == MAX_CAPTURES {
            break;
        }
    }
    found
}

/// Text in another language: a node of the text it's in.
struct Injection<'tree> {
    language: &'static Language,
    node: Node<'tree>,
    /// The pattern that found it, in the query's order.
    pattern: usize,
    /// Whether the node's children are in the language too.
    include_children: bool,
}

/// The injections `query` finds in `bytes` of `text`, which `tree` is the
/// tree of.
fn find_injections<'tree>(
    query: &InjectionQuery,
    cursor: &mut QueryCursor,
    tree: &'tree Tree,
    text: &str,
    bytes: Range<usize>,
) -> Vec<Injection<'tree>> {
    cursor.set_byte_range(bytes);
    let mut matches = cursor.matches(&query.query, tree.root_node(), text.as_bytes());
    let mut found = Vec::new();
    while let Some(m) = matches.next() {
        let mut content = None;
        let mut name = None;
        for capture in m.captures() {
            if capture.index == query.content {
                content = Some(capture.node);
            } else if Some(capture.index) == query.language {
                name = text.get(capture.node.byte_range());
            }
        }
        let mut include_children = false;
        for property in query.query.property_settings(m.pattern_index) {
            match &*property.key {
                "injection.language" => name = name.or(property.value.as_deref()),
                "injection.include-children" => include_children = true,
                _ => {}
            }
        }
        let (Some(node), Some(language)) = (content, name.and_then(language::injected)) else {
            continue;
        };
        found.push(Injection {
            language,
            node,
            pattern: m.pattern_index,
            include_children,
        });
        if found.len() == MAX_INJECTIONS {
            break;
        }
    }
    // Where patterns inject into the same node, as a script as JavaScript
    // and, with `lang="ts"`, TypeScript, the last one wins, as with
    // highlights.
    found.sort_by_key(|injection| (injection.node.id(), Reverse(injection.pattern)));
    found.dedup_by_key(|injection| injection.node.id());
    found.sort_by_key(|injection| injection.node.start_byte());
    found
}

/// The ranges of `node` that are injected text: all of it with `children`,
/// or else what's between its named children, such as the `>` starting
/// each line of a quote. Only what's also within `parent`, if not empty.
fn included_ranges(
    node: Node,
    children: bool,
    parent: &[tree_sitter::Range],
) -> Vec<tree_sitter::Range> {
    let mut ranges = Vec::new();
    let mut rest = node.range();
    if !children {
        let mut cursor = node.walk();
        // Empty children, like the block continuations of a code block
        // not in a list or quote, split nothing.
        let children = node.named_children(&mut cursor);
        for child in children.filter(|child| child.start_byte() < child.end_byte()) {
            ranges.push(tree_sitter::Range {
                end_byte: child.start_byte(),
                end_point: child.start_position(),
                ..rest
            });
            rest.start_byte = child.end_byte();
            rest.start_point = child.end_position();
        }
    }
    ranges.push(rest);
    ranges.retain(|range| range.start_byte < range.end_byte);
    if parent.is_empty() {
        return ranges;
    }
    let mut within = Vec::new();
    for range in &ranges {
        for outer in parent {
            let start = if range.start_byte < outer.start_byte {
                outer
            } else {
                range
            };
            let end = if range.end_byte > outer.end_byte {
                outer
            } else {
                range
            };
            if start.start_byte < end.end_byte {
                within.push(tree_sitter::Range {
                    start_byte: start.start_byte,
                    start_point: start.start_point,
                    end_byte: end.end_byte,
                    end_point: end.end_point,
                });
            }
        }
    }
    within
}

/// Injected text's tree, kept for the next time its lines are highlighted.
struct Injected {
    /// The bytes of the node it's in.
    node: Range<usize>,
    /// What it was parsed from.
    ranges: Vec<tree_sitter::Range>,
    /// `None` if parsing took too long: it keeps the colors of the text
    /// it's in from then on, as a file that took too long stays plain.
    tree: Option<Tree>,
    /// The text changed since: `tree` is edited to match, to parse again.
    edited: bool,
    /// Highlighted since [`Colorer::forget_unused`].
    used: bool,
}

/// Colors text with its language's highlight query, and injected text with
/// its own language's.
struct Colorer {
    /// Parses injected text.
    parser: Parser,
    cursor: QueryCursor,
    /// Injected text's trees, by language and where the text starts.
    injected: HashMap<(&'static str, usize), Injected>,
}

impl Colorer {
    fn new() -> Colorer {
        Colorer {
            parser: Parser::new(),
            cursor: QueryCursor::new(),
            injected: HashMap::new(),
        }
    }

    /// Keeps the trees of injected text through `edit` to the text, to
    /// parse again from.
    fn edit(&mut self, edit: &InputEdit) {
        self.injected = std::mem::take(&mut self.injected)
            .into_iter()
            .map(|((language, start), mut injected)| {
                if let Some(tree) = &mut injected.tree {
                    tree.edit(edit);
                }
                injected.edited = true;
                let start = if start >= edit.old_end_byte {
                    start - edit.old_end_byte + edit.new_end_byte
                } else {
                    start
                };
                ((language, start), injected)
            })
            .collect();
    }

    /// Forgets the trees of injected text not highlighted since last time.
    fn forget_unused(&mut self) {
        self.injected
            .retain(|_, injected| std::mem::take(&mut injected.used));
    }

    /// The tree of `injection` in `grammar`'s language, of `text`, within
    /// `parent`, the ranges of the text it's in, if not all of `text`; and
    /// the ranges it's of. The tree from before if the text's the same, or
    /// else parsed, from the one before if any, by `deadline`. `Err` if
    /// stopped by `stop`.
    fn injected_tree(
        &mut self,
        injection: &Injection,
        parent: &[tree_sitter::Range],
        grammar: &Grammar,
        text: &str,
        deadline: Option<Instant>,
        stop: &AtomicBool,
    ) -> Result<Option<(Tree, Vec<tree_sitter::Range>)>, ()> {
        let node = injection.node;
        let key = (injection.language.name, node.start_byte());
        let before = self.injected.remove(&key);
        let same = (before.as_ref()).is_some_and(|b| !b.edited && b.node == node.byte_range());
        // Working the ranges out walks the node's children, and a code
        // block has one for each line.
        let ranges = match &before {
            Some(before) if same => before.ranges.clone(),
            _ => included_ranges(node, injection.include_children, parent),
        };
        if ranges.is_empty() {
            return Ok(None);
        }
        let tree = match before {
            Some(before) if same || before.tree.is_none() => before.tree,
            before => {
                let old = before.and_then(|before| before.tree);
                let parser = &mut self.parser;
                if parser.set_language(&grammar.language).is_err()
                    || parser.set_included_ranges(&ranges).is_err()
                {
                    return Ok(None);
                }
                let tree = parse(parser, text, old.as_ref(), deadline, stop);
                if tree.is_none() && stop.load(Ordering::Relaxed) {
                    return Err(());
                }
                tree
            }
        };
        self.injected.insert(
            key,
            Injected {
                node: node.byte_range(),
                ranges: ranges.clone(),
                tree: tree.clone(),
                edited: false,
                used: true,
            },
        );
        Ok(tree.map(|tree| (tree, ranges)))
    }

    /// The colored spans of `bytes` of `text`, which `tree` is the tree in
    /// `grammar`'s language of, parsed from `ranges` if not all of `text`,
    /// injected `depth` deep. Injected text left to parse at `deadline`, if
    /// any, keeps the colors of the text it's in. `None` if stopped by
    /// `stop`.
    #[allow(clippy::too_many_arguments)]
    fn spans(
        &mut self,
        grammar: &Grammar,
        tree: &Tree,
        text: &str,
        bytes: Range<usize>,
        ranges: &[tree_sitter::Range],
        depth: u32,
        deadline: Option<Instant>,
        stop: &AtomicBool,
    ) -> Option<Vec<Span<SyntaxColor>>> {
        let captures = query_captures(grammar, &mut self.cursor, tree, text, bytes.clone());
        let injections = match &grammar.injections {
            Some(query) if depth < MAX_INJECTION_DEPTH => {
                find_injections(query, &mut self.cursor, tree, text, bytes.clone())
            }
            _ => Vec::new(),
        };
        // Injected text shows the color captured for the node it's in,
        // under its own: a heading keeps its color, and a code block with
        // `@none` shows only its language's.
        let bases: Vec<Option<SyntaxColor>> = injections
            .iter()
            .map(|injection| {
                let own = captures.iter().filter(|c| c.node == injection.node.id());
                own.max_by_key(|c| c.pattern).and_then(|c| c.style)
            })
            .collect();
        let mut spans = flatten(captures, bytes.clone());
        for (injection, base) in injections.iter().zip(bases) {
            let Some(injected) = grammar_of(injection.language) else {
                continue;
            };
            let tree = self.injected_tree(injection, ranges, &injected, text, deadline, stop);
            let Some((tree, injected_ranges)) = tree.ok()? else {
                continue;
            };
            let layer = self.spans(
                &injected,
                &tree,
                text,
                bytes.clone(),
                &injected_ranges,
                depth + 1,
                deadline,
                stop,
            )?;
            for range in &injected_ranges {
                let range = range.start_byte.max(bytes.start)..range.end_byte.min(bytes.end);
                if !range.is_empty() {
                    paint(&mut spans, range, base, &layer);
                }
            }
        }
        Some(spans)
    }
}

/// A line's syntax colors, as byte ranges in it, in order.
pub type LineColors = Vec<(Range<usize>, SyntaxColor)>;

/// Highlights a few lines of a file at a time, for search results: the
/// file's parsed whole, so lines inside a block comment or a long string
/// come out right. Runs on any thread.
pub struct ExcerptHighlighter {
    parser: Parser,
    colorer: Colorer,
}

impl ExcerptHighlighter {
    pub fn new() -> ExcerptHighlighter {
        ExcerptHighlighter {
            parser: Parser::new(),
            colorer: Colorer::new(),
        }
    }

    /// The colors of each of `lines` of `text`, the file at `path` in
    /// `language`. `lines` are byte ranges of `text`, in order, each within
    /// one line. `None` if cue can't highlight the language, the text is
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
        let grammar = grammar_of(language)?;
        self.parser.set_language(&grammar.language).ok()?;
        let tree = parse(&mut self.parser, text, None, None, stop)?;
        // Injected text's trees are only kept for this text's excerpts.
        self.colorer.injected.clear();
        let mut colors = Vec::with_capacity(lines.len());
        // Lines that follow each other are queried together: one query
        // per excerpt, not per line.
        for group in lines.chunk_by(|a, b| b.start <= a.end + 2) {
            let bytes = group[0].start..group[group.len() - 1].end;
            let spans = self
                .colorer
                .spans(&grammar, &tree, text, bytes, &[], 0, None, stop)?;
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
    styles: SyntaxStyles,
    parser: Parser,
    colorer: Colorer,
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
    /// A highlighter for text in `language`, if cue can highlight it.
    pub fn new(language: &'static Language, theme: &Theme) -> Option<Highlighter> {
        let grammar = grammar_of(language)?;
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).ok()?;
        Some(Highlighter {
            grammar,
            styles: theme.syntax_styles(),
            parser,
            colorer: Colorer::new(),
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
        if visible.is_empty() || !self.catch_up(buffer) {
            return;
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
        self.colorer.forget_unused();
        buffer.replace_highlights(HIGHLIGHTS, &highlights);
    }

    /// The text as the tree has it, and what's at each byte of it, brought
    /// up to date with `buffer` first. `None` if it isn't parsed.
    pub fn regions<'a>(
        &'a mut self,
        buffer: &EditBuffer,
    ) -> Option<(&'a str, impl Fn(usize) -> Region + 'a)> {
        if !self.catch_up(buffer) {
            return None;
        }
        let tree = self.tree.as_ref()?;
        Some((self.text.as_str(), |byte| region_at(tree, byte)))
    }

    /// Reparses `buffer`'s text if it changed since. False, leaving it
    /// plain, if it's given up on.
    fn catch_up(&mut self, buffer: &EditBuffer) -> bool {
        if self.gave_up {
            return false;
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
                return false;
            }
        }
        true
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
                Some(edit) => {
                    tree.edit(&edit);
                    self.colorer.edit(&edit);
                }
                None => return true,
            }
        }
        let never = AtomicBool::new(false);
        self.tree = parse(
            &mut self.parser,
            &text,
            self.tree.as_ref(),
            Some(Instant::now() + PARSE_BUDGET),
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
        let never = AtomicBool::new(false);
        let deadline = Instant::now() + PARSE_BUDGET;
        let spans = self.colorer.spans(
            &self.grammar,
            tree,
            &self.text,
            bytes,
            &[],
            0,
            Some(deadline),
            &never,
        );
        spans
            .unwrap_or_default()
            .into_iter()
            .map(|span| Span {
                bytes: span.bytes,
                style: self.styles.of(span.style),
            })
            .collect()
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

/// What a byte of text is part of, as far as indenting goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Code,
    Comment,
    String,
}

/// What byte `byte` of the text `tree` is the tree of is in, going by the
/// names of the nodes around it: grammars name theirs `line_comment`,
/// `string_literal`, and so on. Code interpolated into a string, as in
/// `f"{x}"`, is code.
fn region_at(tree: &Tree, byte: usize) -> Region {
    let mut node = tree.root_node().descendant_for_byte_range(byte, byte);
    while let Some(n) = node {
        if n.start_byte() <= byte && byte < n.end_byte() {
            let kind = n.kind();
            if kind.contains("interpolation") || kind.contains("substitution") {
                return Region::Code;
            }
            if kind.contains("comment") {
                return Region::Comment;
            }
            if kind.contains("string") {
                return Region::String;
            }
        }
        node = n.parent();
    }
    Region::Code
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
    // particular order. Of a pattern's captures of one node, as in
    // `@variable @function`, a colored one counts. Helpers were filtered out.
    captures.sort_by_key(|c| {
        let colored = c.style.is_some();
        (
            c.bytes.start,
            Reverse(c.bytes.end),
            c.node,
            c.pattern,
            colored,
        )
    });
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

/// Paints `layer` over `spans` across `range`: what was there gives way to
/// `base`, if any, and that to `layer`'s spans. Spans are in order and don't
/// overlap, before and after.
fn paint<S: Copy>(
    spans: &mut Vec<Span<S>>,
    range: Range<usize>,
    base: Option<S>,
    layer: &[Span<S>],
) {
    let first = spans.partition_point(|s| s.bytes.end <= range.start);
    let last = spans.partition_point(|s| s.bytes.start < range.end);
    let mut painted = Vec::new();
    // What sticks out either side stays.
    if let Some(s) = spans.get(first).filter(|s| s.bytes.start < range.start) {
        painted.push(Span {
            bytes: s.bytes.start..range.start,
            style: s.style,
        });
    }
    let mut at = range.start;
    let fill = |painted: &mut Vec<Span<S>>, bytes: Range<usize>| {
        if let Some(style) = base.filter(|_| !bytes.is_empty()) {
            painted.push(Span { bytes, style });
        }
    };
    let from = layer.partition_point(|s| s.bytes.end <= range.start);
    for s in layer[from..]
        .iter()
        .take_while(|s| s.bytes.start < range.end)
    {
        let bytes = s.bytes.start.max(range.start)..s.bytes.end.min(range.end);
        fill(&mut painted, at..bytes.start);
        at = bytes.end;
        painted.push(Span {
            bytes,
            style: s.style,
        });
    }
    fill(&mut painted, at..range.end);
    if let Some(s) = spans[first..last]
        .last()
        .filter(|s| s.bytes.end > range.end)
    {
        painted.push(Span {
            bytes: range.end..s.bytes.end,
            style: s.style,
        });
    }
    spans.splice(first..last, painted);
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
    fn every_query_compiles_and_uses_known_directives() {
        let mut wrong = Vec::new();
        for language in language::all() {
            let Some(syntax) = &language.syntax else {
                continue;
            };
            let grammar = (syntax.grammar)();
            if let Err(e) = Parser::new().set_language(&grammar) {
                wrong.push(format!("{}: {e}", language.name));
            }
            for (label, sources, kind) in [
                ("highlights", syntax.highlights, QueryKind::Highlights),
                ("injections", syntax.injections, QueryKind::Injections),
            ] {
                if let Err(e) = compile_query(&grammar, sources, kind) {
                    wrong.push(format!("{} {label}: {e}", language.name));
                }
            }
            if grammar_of(language).is_none() {
                wrong.push(format!("{}: grammar initialization failed", language.name));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn rejects_unhandled_query_predicates_and_directives() {
        let grammar = (rust().syntax.as_ref().unwrap().grammar)();
        for directive in [
            "(#lua-match? @name \"^%u\")",
            "(#unknown? @name)",
            "(#offset! @name 0 1 0 -1)",
            "(#set! unknown \"value\")",
            "(#is? local)",
        ] {
            let source = format!("((identifier) @name {directive})");
            Query::new(&grammar, &source).expect("valid tree-sitter query");
            assert!(
                compile_query(&grammar, &[&source], QueryKind::Highlights).is_err(),
                "{directive}"
            );
        }
        let source = "((identifier) @name (#set! injection.language \"rust\"))";
        assert!(compile_query(&grammar, &[source], QueryKind::Highlights).is_err());
        assert!(compile_query(&grammar, &[source], QueryKind::Injections).is_ok());
    }

    #[test]
    fn helper_captures_preserve_colors_but_uncolored_captures_override_them() {
        let language = (rust().syntax.as_ref().unwrap().grammar)();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let text = "fn example() {}";
        let tree = parser.parse(text, None).unwrap();
        let function = SyntaxColor::of("function").unwrap();
        for (suffix, expected) in [
            ("(identifier) @_helper", Some(function)),
            ("(identifier) @spell @nospell @conceal", Some(function)),
            ("(identifier) @variable", None),
            ("(identifier) @variable @function", Some(function)),
        ] {
            let query = compile_query(
                &language,
                &["(identifier) @function", suffix],
                QueryKind::Highlights,
            )
            .unwrap();
            let captures = query
                .capture_names()
                .iter()
                .map(|name| CaptureRule::of(name))
                .collect();
            let grammar = Grammar {
                language: language.clone(),
                query,
                captures,
                injections: None,
            };
            let captures = query_captures(
                &grammar,
                &mut QueryCursor::new(),
                &tree,
                text,
                0..text.len(),
            );
            let spans = flatten(captures, 0..text.len());
            assert_eq!(spans.first().map(|span| span.style), expected, "{suffix}");
        }
    }

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
        assert_highlights(&[
            ("a.rs", "const MAX: u8 = 1;", "MAX", "constant"),
            ("a.rs", "fn f(v: Vec<u8>) {}", "Vec", "type"),
            ("a.toml", "[package]\nname = \"q\" # c", "\"q\"", "string"),
            ("a.toml", "[package]\nname = \"q\" # c", "# c", "comment"),
            ("a.md", "# Title\n\ntext\n", "Title", "text.title"),
            ("a.json", "{\"a\": 1, \"b\": true}", "1", "number"),
            ("a.json", "{\"a\": 1, \"b\": true}", "true", "constant"),
            ("a.py", "def f():\n    return \"x\"", "def", "keyword"),
            ("a.py", "def f():\n    return \"x\"", "f(", "function"),
            ("a.py", "def f():\n    return \"x\"", "\"x\"", "string"),
            ("a.js", "function f() { return 1; }", "function", "keyword"),
            ("a.js", "function f() { return 1; }", "f(", "function"),
            ("a.js", "const s = `a${b}`;", "`a", "string"),
            ("a.jsx", "const a = <div id=\"x\" />;", "div", "tag"),
            ("a.ts", "interface A { x: number }", "interface", "keyword"),
            ("a.ts", "interface A { x: number }", "number", "type"),
            ("a.tsx", "const a = <div id={1} />;", "div", "tag"),
            ("a.tsx", "let n: string = f<T>();", "string", "type"),
            (
                "a.go",
                "package main\nfunc f() int { return 0 }",
                "func",
                "keyword",
            ),
            (
                "a.go",
                "package main\nfunc f() int { return 0 }",
                "int",
                "type",
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "return",
                "keyword",
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "main",
                "function",
            ),
            (
                "a.c",
                "int main(void) { return 0; } // c",
                "// c",
                "comment",
            ),
            ("a.cpp", "class A { public: int x; };", "class", "keyword"),
            ("a.cpp", "class A { public: int x; };", "int", "type"),
            ("a.sh", "echo \"hi\" # c", "\"hi\"", "string"),
            ("a.sh", "echo \"hi\" # c", "# c", "comment"),
            ("a.sh", "if true; then echo; fi", "then", "keyword"),
            ("a.yaml", "a: \"b\" # c", "\"b\"", "string"),
            ("a.yaml", "a: \"b\" # c", "# c", "comment"),
            ("a.html", "<p class=\"x\">hi</p>", "p ", "tag"),
            ("a.html", "<p class=\"x\">hi</p>", "class", "attribute"),
            ("a.html", "<p class=\"x\">hi</p>", "hi", UNCOLORED),
            ("a.css", "a { color: red; } /* c */", "/* c", "comment"),
            ("a.css", "a { color: red; } /* c */", "a ", "tag"),
            ("a.zig", "const std = @import(\"std\");", "const", "keyword"),
            (
                "a.zig",
                "const std = @import(\"std\");",
                "\"std\"",
                "string",
            ),
            ("a.zig", "pub fn main() void {}", "main", "function"),
            (
                "a.java",
                "class A { void m() { m(); } }",
                "class",
                "keyword",
            ),
            (
                "a.java",
                "class A { void m() { m(); } }",
                "m();",
                "function",
            ),
            ("a.kt", "fun main() { val s = \"hi\" }", "main", "function"),
            ("a.kt", "fun main() { val s = \"hi\" }", "\"hi\"", "string"),
            ("a.swift", "func f() -> Int { return 1 }", "Int", "type"),
            ("a.dart", "void main() { print('hi'); }", "'hi'", "string"),
            ("a.rb", "def f\n  puts \"x\"\nend", "def", "keyword"),
            ("a.php", "<p>hi</p><?php echo \"x\"; ?>", "echo", "keyword"),
            ("a.pl", "my $x = \"a\"; # c", "my", "keyword"),
            ("a.pl", "my $x = \"a\"; # c", "# c", "comment"),
            ("a.ex", "defmodule A do\nend", "defmodule", "keyword"),
            (
                "a.hs",
                "main = putStrLn \"hi\"\nf x = x\n",
                "putStrLn",
                "function",
            ),
            ("a.r", "f <- function(x) x", "function", "keyword"),
            (
                "a.sql",
                "SELECT a FROM t WHERE b = 'x';",
                "SELECT",
                "keyword",
            ),
            ("a.graphql", "type T { f: Int }", "T ", "type"),
            (
                "a.ps1",
                "function F { Write-Host \"x\" }",
                "Write-Host",
                "function",
            ),
            ("a.clj", "(defn f [x] \"s\")", "defn", "keyword"),
            ("a.clj", "(defn f [x] \"s\")", "f ", "function"),
            ("a.scm", "(define (f x) \"s\")", "define", "keyword"),
            ("a.lisp", "(defun f (x) \"s\")", "defun", "keyword"),
            (
                "a.gd",
                "func _ready():\n\tvar x = 1\n",
                "_ready",
                "function",
            ),
            ("a.vim", "let g:x = 1\n", "let", "keyword"),
            ("a.s", "mov eax, 1 ; c", "; c", "comment"),
            ("Dockerfile", "FROM alpine:3\n", "FROM", "keyword"),
            ("Makefile", "all: b\n\techo hi\n# c\n", "# c", "comment"),
            ("Makefile", "all: b\nb:\n\techo hi\n", "b:", "function"),
            ("Makefile", "all: b\nb:\n\techo hi\n", "all", "constant"),
            ("Makefile", "CC = gcc\n", "CC", "constant"),
            (
                "CMakeLists.txt",
                "add_executable(a b.c)",
                "add_executable",
                "function",
            ),
            (
                "nginx.conf",
                "server {\n  listen 80;\n}\n",
                "listen",
                "keyword",
            ),
            ("a.diff", "--- a\n+++ b\n-x\n+y\n", "-x", "diff.minus"),
            ("a.diff", "--- a\n+++ b\n-x\n+y\n", "+y", "diff.plus"),
            ("requirements.txt", "requests==2.0 # c\n", "# c", "comment"),
            ("a.typ", "= Heading\n*bold*\n", "Heading", "text.title"),
            ("a.tex", "\\section{Intro} % c", "Intro", "text.title"),
            ("a.tex", "\\section{Intro} % c", "% c", "comment"),
            ("a.bib", "@article{k, title = {T}}", "@article", "keyword"),
            ("a.mmd", "flowchart TD\n  A --> B\n", "flowchart", "keyword"),
            ("a.cu", "int main() { return 0; }", "return", "keyword"),
            ("a.svelte", "<p>hi</p>", "p>", "tag"),
            ("a.frag", "void main() { return; }", "return", "keyword"),
        ]);
    }

    #[test]
    fn markdown_injections_preserve_markup_and_fallback_colors() {
        assert_highlights(&[
            ("a.md", "text\n\n```\ncode\n```\n", "code", "text.literal"),
            // Inline markup, injected into paragraphs, headings, and cells.
            ("a.md", "a `code` b\n", "code", "text.literal"),
            ("a.md", "a `code` b\n", "`code", "text.delimiter"),
            ("a.md", "a *em* b\n", "em", "text.emphasis"),
            ("a.md", "a **st** b\n", "st", "text.strong"),
            ("a.md", "a ~~del~~ b\n", "del", "text.strike"),
            ("a.md", "[t](http://x)\n", "t]", "text.reference"),
            ("a.md", "[t](http://x)\n", "http", "text.uri"),
            ("a.md", "a <b>hi</b>\n", "b>", "tag"),
            ("a.md", "a\\*b\n", "\\*", "string.escape"),
            // A heading keeps its color around inline markup.
            ("a.md", "## A `b` c\n", "##", "text.title"),
            ("a.md", "## A `b` c\n", "c\n", "text.title"),
            ("a.md", "## A `b` c\n", "b`", "text.literal"),
            ("a.md", "- [x] a\n", "- ", "text.list"),
            ("a.md", "- [x] a\n", "[x]", "text.list"),
            // Quotes' markers aren't inline markup, though inside it.
            ("a.md", "> a\n> `b`\n", "> `", "text.delimiter"),
            ("a.md", "> a\n> `b`\n", "b`", "text.literal"),
            ("a.md", "| a |\n|---|\n| `b` |\n", "a ", "text.strong"),
            ("a.md", "| a |\n|---|\n| `b` |\n", "b`", "text.literal"),
            // Code blocks in a language cue knows are highlighted as it,
            // and those in others as code.
            ("a.md", "```rust\nlet x;\n```\n", "let", "keyword"),
            ("a.md", "```rust\nlet x;\n```\n", "x;", UNCOLORED),
            ("a.md", "```rust\nlet x;\n```\n", "rust", "text.delimiter"),
            ("a.md", "```wat\nlet x;\n```\n", "x;", "text.literal"),
            ("a.md", "---\na: \"b\"\n---\n", "\"b\"", "string"),
            // Injections in injections.
            (
                "a.md",
                "```html\n<script>let x;</script>\n```\n",
                "let",
                "keyword",
            ),
            // Markdown's code blocks, in any of those.
            ("a.md", "```kotlin\nval x = 1\n```\n", "val", "keyword"),
        ]);
    }

    #[test]
    fn embedded_languages_use_their_own_highlighting() {
        assert_highlights(&[
            ("a.html", "<style>/* c */</style>", "/* c", "comment"),
            // Outside `<?php ?>`, HTML.
            ("a.php", "<p>hi</p><?php echo \"x\"; ?>", "p>", "tag"),
            // Raw blocks, in the language they name.
            ("a.typ", "```rust\nfn f() {}\n```\n", "fn", "keyword"),
        ]);
    }

    #[test]
    fn svelte_script_language_overrides_the_default_injection() {
        assert_highlights(&[
            ("a.svelte", "<script>let x = 1;</script>", "let", "keyword"),
            (
                "a.svelte",
                "<script lang=\"ts\">let x: number;</script>",
                "number",
                "type",
            ),
            ("a.svelte", "<style>p { color: red; }</style>", "p ", "tag"),
        ]);
    }

    #[test]
    fn capture_precedence_preserves_specific_styles() {
        assert_highlights(&[
            // As in tree-sitter's own highlighter: the call pattern comes
            // after the one for uppercase names.
            ("a.rs", "let x = Some(1);", "Some", "function"),
            // Keys are `type` inside a `property`: tree-sitter's own
            // highlighter drops the inner capture, as they start together.
            ("a.toml", "[package]\nname = \"q\" # c", "name", "type"),
            // Keys are strings: `@string` comes after `@string.special.key`.
            ("a.json", "{\"a\": 1, \"b\": true}", "\"a\"", "string"),
            (
                "a.hs",
                "main = putStrLn \"hi\"\nf x = x\n",
                "x\n",
                UNCOLORED,
            ),
            // GLSL's queries add to C's.
            ("a.frag", "void main() {}", "main", "function"),
        ]);
    }

    #[test]
    fn regex_predicates_distinguish_zig_identifiers() {
        assert_highlights(&[
            // Lowercase names do not match the type-name regex.
            ("a.zig", "const std = @import(\"std\");", "std ", UNCOLORED),
            ("a.zig", "const T = struct {};", "T ", "type"),
        ]);
    }

    /// What [`assert_highlights`] expects of text left uncolored.
    const UNCOLORED: &str = "";

    /// (file, source text, first text to inspect, expected capture style).
    fn assert_highlights(cases: &[(&str, &str, &str, &str)]) {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let mut wrong = Vec::new();
        for &(name, text, needle, capture) in cases {
            let expected = theme.capture_style(capture);
            assert!(
                capture == UNCOLORED || expected.is_some(),
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
    fn paint_covers_what_was_there() {
        let span = |bytes: Range<usize>, style: u32| Span { bytes, style };
        let painted = |base: Option<u32>, layer: &[Span<u32>]| {
            let mut spans = vec![span(0..4, 1), span(6..12, 2), span(14..20, 3)];
            paint(&mut spans, 2..16, base, layer);
            spans
                .into_iter()
                .map(|s| (s.bytes, s.style))
                .collect::<Vec<_>>()
        };
        // Clipped to the range; what sticks out either side stays.
        let layer = [span(0..3, 7), span(8..9, 8), span(15..30, 9)];
        assert_eq!(
            painted(None, &layer),
            [(0..2, 1), (2..3, 7), (8..9, 8), (15..16, 9), (16..20, 3)]
        );
        // The base fills the gaps.
        assert_eq!(
            painted(Some(5), &layer),
            [
                (0..2, 1),
                (2..3, 7),
                (3..8, 5),
                (8..9, 8),
                (9..15, 5),
                (15..16, 9),
                (16..20, 3)
            ]
        );
        assert_eq!(painted(Some(5), &[]), [(0..2, 1), (2..16, 5), (16..20, 3)]);
    }

    #[test]
    fn excerpts_are_highlighted_with_injections() {
        let text = "# T\n\nsome `code`\n\n```rust\nfn f() {}\n```\n";
        let markdown = language::detect(Some(Path::new("a.md")), String::new).unwrap();
        let line = |n: usize| {
            let start = line_starts(text)[n];
            start..start + text[start..].find('\n').unwrap()
        };
        let never = AtomicBool::new(false);
        let path = Path::new("/excerpts/injections.md");
        let colors = ExcerptHighlighter::new()
            .highlight(path, markdown, text, &[line(2), line(5)], &never)
            .unwrap();
        let literal = SyntaxColor::of("text.literal").unwrap();
        let keyword = SyntaxColor::of("keyword").unwrap();
        assert!(colors[0].contains(&(6..10, literal)), "{:?}", colors[0]);
        assert_eq!(colors[1].first(), Some(&(0..2, keyword)));
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
        // Of one pattern's captures of a node, the colored one.
        let captures = vec![capture(9..14, Some(3), 1), capture(9..14, None, 1)];
        assert_eq!(spans(captures, 0..100), [(9..14, 3)]);
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
    fn injected_text_highlights_the_same_after_edits() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let markdown = language::detect(Some(Path::new("a.md")), String::new).unwrap();
        let mut highlighter = Highlighter::new(markdown, &theme).unwrap();
        let steps = [
            "# A\n\nsome `code`\n\n```rust\nfn f() {}\n```\n",
            "# A\n\nsome `code` *more*\n\n```rust\nfn f() {}\n```\n",
            "# A\n\nsome `code` *more*\n\n```rust\nfn f() { let x = 1; }\n```\n",
            "# A `b`\n\nsome `code` *more*\n\n```rust\nfn f() { let x = 1; }\n```\n",
            "# A `b`\n\nsome `co\n\n```python\nfn f() { let x = 1; }\n```\n",
            "```rust\nfn f() { let x = 1; }\n```\n",
            "> ```rust\n> fn f() {}\n> ```\n",
        ];
        for text in steps {
            assert!(highlighter.reparse(text.to_string()));
            let spans = highlighter.spans(0..text.len());
            let mut fresh = Highlighter::new(markdown, &theme).unwrap();
            assert!(fresh.reparse(text.to_string()));
            assert_eq!(spans, fresh.spans(0..text.len()), "{text:?}");
        }
        // Only what was highlighted last is kept.
        assert!(!highlighter.colorer.injected.is_empty());
        highlighter.colorer.forget_unused();
        highlighter.colorer.forget_unused();
        assert!(highlighter.colorer.injected.is_empty());
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
