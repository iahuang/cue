//! Reader mode: a Markdown file drawn as it reads rather than as it's
//! written. Markup is hidden, headings are bold and ruled, tables are drawn
//! with box characters, code blocks sit on a band of background with their
//! syntax colored, and prose wraps to the panel, at most [`MEASURE`]
//! columns wide. Math is drawn as images where the terminal shows them
//! (see [`math`]).
//!
//! The text is laid out into rows whenever it or the panel's width
//! changes, so edits made elsewhere (in another panel, or on disk by an
//! agent) show as they happen. Each row remembers the line of the file it
//! came from: scrolling keeps to the same line across a new layout, and
//! switching to and from editing keeps the same line at the top.
//!
//! It can't be typed in. The wheel and the arrow and page keys scroll,
//! dragging selects text to copy, a click on a link follows it, and a
//! double click goes to editing with the cursor on the line clicked.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use opentui::{Attributes, Buffer, EditBuffer};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::input::{Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::language;
use crate::math::{self, Formula, Typesetter};
use crate::status::Status;
use crate::syntax::ExcerptHighlighter;
use crate::theme::{self, Colors, Hue, SyntaxColor};

/// Prose wraps at most this many columns wide, however wide the panel is.
/// Tables and code blocks may use the panel's whole width.
const MEASURE: usize = 100;
/// Rows a step of the wheel scrolls.
const WHEEL_ROWS: usize = 3;
/// Columns a tab in a code block takes.
const TAB_WIDTH: usize = 4;
/// List bullets, by how deeply the list is nested.
const BULLETS: [&str; 3] = ["•", "◦", "▪"];
/// What a block quote's lines start with.
const QUOTE_BAR: &str = "▎ ";

// --- layout -----------------------------------------------------------------

/// A color, worked out from the theme in use when drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ink {
    Text,
    Muted,
    Faint,
    Border,
    Syntax(SyntaxColor),
    Hue(Hue),
}

impl Ink {
    fn rgba(self, colors: &Colors) -> opentui::Rgba {
        match self {
            Ink::Text => colors.text,
            Ink::Muted => colors.muted,
            Ink::Faint => colors.faint,
            Ink::Border => colors.border,
            Ink::Syntax(color) => color.fg().unwrap_or(colors.text),
            Ink::Hue(hue) => colors.hue(hue),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
    fg: Ink,
    /// On a code block's or code span's background.
    code: bool,
    attributes: Attributes,
}

impl Style {
    const TEXT: Style = Style {
        fg: Ink::Text,
        code: false,
        attributes: Attributes::NONE,
    };

    fn ink(fg: Ink) -> Style {
        Style { fg, ..Style::TEXT }
    }

    /// Text that a Markdown highlight query captures as `capture`, as the
    /// source view colors it.
    fn capture(capture: &str) -> Style {
        match SyntaxColor::of(capture) {
            Some(color) => Style {
                fg: Ink::Syntax(color),
                code: false,
                attributes: color.attributes(),
            },
            None => Style::TEXT,
        }
    }

    fn with(self, attributes: Attributes) -> Style {
        Style {
            attributes: self.attributes | attributes,
            ..self
        }
    }

    fn on_code(self) -> Style {
        Style { code: true, ..self }
    }
}

/// A run of text in one style, maybe a link, or a formula's place.
#[derive(Debug, Clone, PartialEq)]
struct Span {
    /// For a formula, spaces as wide as it.
    text: String,
    style: Style,
    link: Option<Rc<str>>,
    math: Option<Mark>,
}

/// A row of a formula's image, drawn over its span.
#[derive(Debug, Clone)]
struct Mark {
    formula: Rc<Formula>,
    row: u32,
}

impl PartialEq for Mark {
    fn eq(&self, other: &Mark) -> bool {
        Rc::ptr_eq(&self.formula, &other.formula) && self.row == other.row
    }
}

/// A row of the laid out text.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    spans: Vec<Span>,
    /// The line of the file it came from, 0-based.
    source: u32,
}

impl Line {
    #[cfg(test)]
    fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// The text in screen columns `range`, as copied: formulas as the
    /// LaTeX they're from, display math's on its first row.
    fn copy(&self, range: Range<usize>) -> String {
        let mut out = String::new();
        let mut col = 0;
        for span in &self.spans {
            let width = span.text.width();
            let (from, to) = (range.start.max(col), range.end.min(col + width));
            if from < to {
                match &span.math {
                    Some(mark) if mark.row > 0 => {}
                    Some(mark) if mark.formula.display => {
                        out.push_str(&format!("$${}$$", mark.formula.tex.trim()))
                    }
                    Some(mark) => out.push_str(&format!("${}$", mark.formula.tex)),
                    None => out.push_str(columns(&span.text, from - col..to - col)),
                }
            }
            col += width;
        }
        out
    }
}

/// The text laid out in rows.
#[derive(Debug, Default)]
struct Layout {
    lines: Vec<Line>,
    /// Headings' anchors, as GitHub makes them, and their rows, for links
    /// to `#anchor`.
    anchors: Vec<(String, usize)>,
}

impl Layout {
    /// The first row from file line `source` or after it.
    fn row_of(&self, source: u32) -> usize {
        self.lines.partition_point(|line| line.source < source)
    }

    fn source_of(&self, row: usize) -> u32 {
        match self.lines.get(row).or(self.lines.last()) {
            Some(line) => line.source,
            None => 0,
        }
    }
}

/// A word, or part of one in another style, or what goes between words.
#[derive(Debug, Clone)]
enum Piece {
    Word {
        /// For a formula, spaces as wide as it.
        text: String,
        style: Style,
        link: Option<Rc<str>>,
        source: u32,
        math: Option<Rc<Formula>>,
    },
    Space,
    Break,
}

/// Text to be wrapped: a paragraph, heading, or table cell.
#[derive(Debug, Default)]
struct Inline {
    pieces: Vec<Piece>,
}

impl Inline {
    /// Adds `text`, broken into words.
    fn push(&mut self, text: &str, style: Style, link: Option<&Rc<str>>, source: u32) {
        for (i, word) in text.split([' ', '\t', '\n', '\r']).enumerate() {
            if i > 0 && !matches!(self.pieces.last(), Some(Piece::Space) | None) {
                self.pieces.push(Piece::Space);
            }
            if !word.is_empty() {
                self.pieces.push(Piece::Word {
                    text: word.to_string(),
                    style,
                    link: link.cloned(),
                    source,
                    math: None,
                });
            }
        }
    }

    /// Adds a formula, as a word.
    fn push_math(
        &mut self,
        formula: Rc<Formula>,
        style: Style,
        link: Option<&Rc<str>>,
        source: u32,
    ) {
        self.pieces.push(Piece::Word {
            text: " ".repeat(formula.cols as usize),
            style,
            link: link.cloned(),
            source,
            math: Some(formula),
        });
    }

    fn space(&mut self) {
        if !matches!(self.pieces.last(), Some(Piece::Space) | None) {
            self.pieces.push(Piece::Space);
        }
    }

    /// The text without styles, words separated by spaces.
    fn plain(&self) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Word {
                    math: Some(formula),
                    ..
                } => out.push_str(&formula.tex),
                Piece::Word { text, .. } => out.push_str(text),
                Piece::Space | Piece::Break => out.push(' '),
            }
        }
        out.trim().to_string()
    }
}

/// Words run together: what can't be broken between.
fn words(pieces: &[Piece]) -> impl Iterator<Item = &[Piece]> {
    pieces
        .split(|piece| !matches!(piece, Piece::Word { .. }))
        .filter(|word| !word.is_empty())
}

fn word_width(word: &[Piece]) -> usize {
    word.iter()
        .map(|piece| match piece {
            Piece::Word { text, .. } => text.width(),
            _ => 0,
        })
        .sum()
}

/// How wide `pieces` are on one line.
fn natural_width(pieces: &[Piece]) -> usize {
    let words: Vec<&[Piece]> = words(pieces).collect();
    words.iter().map(|word| word_width(word)).sum::<usize>() + words.len().saturating_sub(1)
}

/// Adds `text` to `spans`, joining the last span if it's styled the same.
fn push_span(spans: &mut Vec<Span>, text: &str, style: Style, link: Option<&Rc<str>>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.math.is_none()
            && last.style == style
            && last.link.as_deref() == link.map(|link| &**link)
        {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(Span {
        text: text.to_string(),
        style,
        link: link.cloned(),
        math: None,
    });
}

/// Adds `span` to `spans`, joined to the last if it's text styled the
/// same.
fn append(spans: &mut Vec<Span>, span: Span) {
    match span.math {
        Some(_) => spans.push(span),
        None => push_span(spans, &span.text, span.style, span.link.as_ref()),
    }
}

/// The longest start of `text` at most `width` columns wide, and the rest.
fn split_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > width {
            return text.split_at(i);
        }
    }
    (text, "")
}

/// Wraps `pieces` into lines `width` wide, breaking between words, and
/// within words longer than a line. Each line comes with the file line
/// its first word is from.
fn wrap(pieces: &[Piece], width: usize) -> Vec<(Vec<Span>, Option<u32>)> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = Vec::new();
    let mut col = 0;
    let mut source = None;
    // Whether a space goes before the next word.
    let mut space = false;
    // The style the last word ended in, which a space after it takes if
    // the next word starts in it too, so links and code stay unbroken.
    let mut last: Option<(Style, Option<Rc<str>>)> = None;
    let mut i = 0;
    while i < pieces.len() {
        match &pieces[i] {
            Piece::Space => {
                space = col > 0;
                i += 1;
            }
            Piece::Break => {
                if col > 0 {
                    out.push((std::mem::take(&mut line), source.take()));
                }
                col = 0;
                space = false;
                i += 1;
            }
            // Display math: rows of its own, centered.
            Piece::Word {
                text,
                style,
                link,
                source: from,
                math: Some(formula),
            } if formula.display => {
                if col > 0 {
                    out.push((std::mem::take(&mut line), source.take()));
                }
                let pad = width.saturating_sub(text.width()) / 2;
                for row in 0..formula.rows {
                    let mut spans = Vec::new();
                    push_span(&mut spans, &" ".repeat(pad), Style::TEXT, None);
                    spans.push(Span {
                        text: text.clone(),
                        style: *style,
                        link: link.clone(),
                        math: Some(Mark {
                            formula: formula.clone(),
                            row,
                        }),
                    });
                    out.push((spans, Some(*from)));
                }
                col = 0;
                space = false;
                i += 1;
            }
            Piece::Word { .. } => {
                let end = pieces[i..]
                    .iter()
                    .position(|piece| !matches!(piece, Piece::Word { .. }))
                    .map_or(pieces.len(), |n| i + n);
                let word = &pieces[i..end];
                let w = word_width(word);
                if col > 0 && col + usize::from(space) + w > width {
                    out.push((std::mem::take(&mut line), source.take()));
                    col = 0;
                    space = false;
                }
                let Piece::Word {
                    style: first_style,
                    link: first_link,
                    ..
                } = &word[0]
                else {
                    unreachable!()
                };
                if space {
                    let style = match &last {
                        Some((style, link)) if style == first_style && link == first_link => {
                            (*style, link.clone())
                        }
                        _ => (Style::TEXT, None),
                    };
                    push_span(&mut line, " ", style.0, style.1.as_ref());
                    col += 1;
                }
                for piece in word {
                    let Piece::Word {
                        text,
                        style,
                        link,
                        source: from,
                        math,
                    } = piece
                    else {
                        continue;
                    };
                    // A formula isn't broken, even if it's wider than a
                    // line.
                    if let Some(formula) = math {
                        if col > 0 && col + text.width() > width {
                            out.push((std::mem::take(&mut line), source.take()));
                            col = 0;
                        }
                        line.push(Span {
                            text: text.clone(),
                            style: *style,
                            link: link.clone(),
                            math: Some(Mark {
                                formula: formula.clone(),
                                row: 0,
                            }),
                        });
                        source.get_or_insert(*from);
                        col += text.width();
                        last = Some((*style, link.clone()));
                        continue;
                    }
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        if col >= width {
                            out.push((std::mem::take(&mut line), source.take()));
                            col = 0;
                        }
                        let (mut head, mut tail) = split_width(rest, width - col);
                        if head.is_empty() {
                            if col > 0 {
                                out.push((std::mem::take(&mut line), source.take()));
                                col = 0;
                                continue;
                            }
                            // A character wider than the line.
                            let n = rest.chars().next().map_or(rest.len(), char::len_utf8);
                            (head, tail) = rest.split_at(n);
                        }
                        push_span(&mut line, head, *style, link.as_ref());
                        source.get_or_insert(*from);
                        col += head.width();
                        rest = tail;
                    }
                    last = Some((*style, link.clone()));
                }
                space = false;
                i = end;
            }
        }
    }
    if col > 0 {
        out.push((line, source));
    }
    out
}

/// Breaks `spans`, one line, into lines at most `width` wide.
fn hard_wrap(spans: Vec<Span>, width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = Vec::new();
    let mut col = 0;
    for span in spans {
        let mut rest = span.text.as_str();
        while !rest.is_empty() {
            if col >= width {
                out.push(std::mem::take(&mut line));
                col = 0;
            }
            let (mut head, mut tail) = split_width(rest, width - col);
            if head.is_empty() {
                if col > 0 {
                    out.push(std::mem::take(&mut line));
                    col = 0;
                    continue;
                }
                let n = rest.chars().next().map_or(rest.len(), char::len_utf8);
                (head, tail) = rest.split_at(n);
            }
            push_span(&mut line, head, span.style, span.link.as_ref());
            col += head.width();
            rest = tail;
        }
    }
    out.push(line);
    out
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.text.width()).sum()
}

/// Pads `spans` with spaces in `style` to `width` columns.
fn pad(spans: &mut Vec<Span>, width: usize, style: Style) {
    let used = spans_width(spans);
    if used < width {
        push_span(spans, &" ".repeat(width - used), style, None);
    }
}

/// A heading's anchor, as GitHub makes them: lowercase, punctuation
/// dropped, spaces as hyphens.
fn slug(text: &str) -> String {
    text.trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// A block that holds others, whose lines start with something.
#[derive(Debug)]
enum Container {
    Quote {
        bar: Ink,
        /// A plain quote's text is muted; an alert's isn't.
        muted: bool,
    },
    List {
        /// The next item's number, if it's numbered.
        next: Option<u64>,
        /// Whether its items are paragraphs, with blank lines between.
        loose: bool,
        items: usize,
    },
    /// A list item, or a footnote: its marker starts its first line, and
    /// its other lines are indented as far.
    Item {
        marker: Vec<Span>,
        width: usize,
        /// Whether its first line is still to come.
        pending: bool,
    },
}

/// What the text being gathered is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Block {
    Paragraph,
    Heading,
    Cell,
    /// A tight list item's text, which isn't in a paragraph.
    Tight,
}

struct Code {
    /// As the fence names it.
    label: String,
    text: String,
    /// The file line of its first line of code.
    source: u32,
    end: u32,
    math: bool,
}

struct Row {
    cells: Vec<Vec<Piece>>,
    head: bool,
    source: u32,
}

struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Row>,
    row: Option<Row>,
}

/// Lays out Markdown text into rows.
struct Builder<'a> {
    line_starts: Vec<usize>,
    /// How wide prose may be, and tables and code.
    measure: usize,
    wide: usize,
    layout: Layout,
    containers: Vec<Container>,
    /// Whether a blank line goes before the next block.
    gap: bool,
    /// The file line of the event being laid out.
    current: u32,
    inline: Option<(Inline, Block)>,
    /// Styles that inline markup adds, innermost last.
    styles: Vec<Attributes>,
    links: Vec<Rc<str>>,
    /// An image's alt text, while in one, and where it points.
    image: Option<(String, Rc<str>)>,
    code: Option<Code>,
    html: Option<(String, u32)>,
    metadata: Option<(String, u32)>,
    table: Option<Table>,
    highlighter: &'a mut ExcerptHighlighter,
    /// Names the text, for the highlighter's cache.
    name: &'a str,
    code_blocks: usize,
    /// Lays out math, if it's drawn as images.
    typesetter: Option<&'a mut Typesetter>,
}

/// Lays out `text` for a panel `width` columns wide. `name` names it in
/// the highlighter's cache. Math is drawn as images if there's a
/// `typesetter`, and is text otherwise.
fn lay_out(
    text: &str,
    width: usize,
    highlighter: &mut ExcerptHighlighter,
    name: &str,
    typesetter: Option<&mut Typesetter>,
) -> Layout {
    let line_starts = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut builder = Builder {
        line_starts,
        measure: width.min(MEASURE),
        wide: width,
        layout: Layout::default(),
        containers: Vec::new(),
        gap: false,
        current: 0,
        inline: None,
        styles: Vec::new(),
        links: Vec::new(),
        image: None,
        code: None,
        html: None,
        metadata: None,
        table: None,
        highlighter,
        name,
        code_blocks: 0,
        typesetter,
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        builder.event(event, range);
    }
    builder.flush_tight();
    if let Some(typesetter) = builder.typesetter {
        typesetter.finish();
    }
    builder.layout
}

impl Builder<'_> {
    fn line_of(&self, byte: usize) -> u32 {
        (self.line_starts.partition_point(|&start| start <= byte) - 1) as u32
    }

    /// The columns the containers' markers take at the start of each line.
    fn indent(&self) -> usize {
        self.containers
            .iter()
            .map(|container| match container {
                Container::Quote { .. } => QUOTE_BAR.width(),
                Container::List { .. } => 0,
                Container::Item { width, .. } => *width,
            })
            .sum()
    }

    /// Room for prose after the containers' markers.
    fn avail(&self) -> usize {
        self.measure.saturating_sub(self.indent()).max(1)
    }

    /// Room for tables and code after the containers' markers.
    fn avail_wide(&self) -> usize {
        self.wide.saturating_sub(self.indent()).max(1)
    }

    /// The containers' markers for the next line: a list item's marker if
    /// it's the item's first line and `first`, or else room for it.
    fn prefix(&mut self, first: bool) -> Vec<Span> {
        let mut spans = Vec::new();
        for container in &mut self.containers {
            match container {
                Container::Quote { bar, .. } => {
                    push_span(&mut spans, QUOTE_BAR, Style::ink(*bar), None)
                }
                Container::List { .. } => {}
                Container::Item {
                    marker,
                    width,
                    pending,
                } => {
                    if *pending && first {
                        *pending = false;
                        for span in marker.iter() {
                            push_span(&mut spans, &span.text, span.style, None);
                        }
                    } else {
                        push_span(&mut spans, &" ".repeat(*width), Style::TEXT, None);
                    }
                }
            }
        }
        spans
    }

    /// Adds a row: the containers' markers, then `spans`.
    fn emit(&mut self, spans: Vec<Span>, source: u32) {
        let mut line = self.prefix(true);
        for span in spans {
            append(&mut line, span);
        }
        self.layout.lines.push(Line {
            spans: line,
            source,
        });
    }

    /// Adds an empty row, but for quotes' bars, before the block on the
    /// current line: it's from the line before.
    fn blank(&mut self) {
        let before = self.layout.lines.last().map_or(0, |line| line.source);
        let source = self.current.saturating_sub(1).max(before);
        let mut spans = self.prefix(false);
        while spans.last().is_some_and(|span| span.text.trim().is_empty()) {
            spans.pop();
        }
        if let Some(last) = spans.last_mut() {
            let trimmed = last.text.trim_end().len();
            last.text.truncate(trimmed);
        }
        self.layout.lines.push(Line { spans, source });
    }

    /// Before a block: finishes a tight list item's text, and puts a blank
    /// line after the block before, if one's due.
    fn start_block(&mut self) {
        self.flush_tight();
        if self.gap && !self.layout.lines.is_empty() {
            self.blank();
        }
        self.gap = false;
    }

    /// Lays out a tight list item's text gathered so far.
    fn flush_tight(&mut self) {
        if matches!(self.inline, Some((_, Block::Tight))) {
            if let Some((inline, _)) = self.inline.take() {
                let base = self.base_style();
                self.emit_inline(&inline, base);
            }
        }
    }

    /// Wraps `inline` and adds its rows.
    fn emit_inline(&mut self, inline: &Inline, base: Style) {
        let fallback = self.layout.lines.last().map_or(0, |line| line.source);
        for (spans, source) in wrap(&inline.pieces, self.avail()) {
            let spans = spans
                .into_iter()
                .map(|span| Span {
                    style: merge(base, span.style),
                    ..span
                })
                .collect();
            self.emit(spans, source.unwrap_or(fallback));
        }
    }

    /// The style of plain text here: muted in a quote.
    fn base_style(&self) -> Style {
        let muted = self
            .containers
            .iter()
            .any(|container| matches!(container, Container::Quote { muted: true, .. }));
        match muted {
            true => Style::ink(Ink::Muted),
            false => Style::TEXT,
        }
    }

    /// The text being gathered, starting a tight list item's if there's
    /// none.
    fn inline(&mut self) -> &mut Inline {
        if self.inline.is_none() {
            self.start_block();
            self.inline = Some((Inline::default(), Block::Tight));
        }
        &mut self.inline.as_mut().unwrap().0
    }

    /// The style inline markup gives text here.
    fn inline_style(&self) -> Style {
        let mut style = Style::TEXT;
        for &attributes in &self.styles {
            style = style.with(attributes);
        }
        if !self.links.is_empty() {
            style = Style {
                fg: Style::capture("text.reference").fg,
                ..style.with(Attributes::UNDERLINE)
            };
        }
        style
    }

    /// `tex` laid out to draw, if math is drawn and it parses.
    fn formula(&mut self, tex: &str, display: bool, room: usize) -> Option<Rc<Formula>> {
        self.typesetter.as_mut()?.formula(tex, display, room)
    }

    /// How math that's text looks: as LaTeX, or as an error where it's
    /// drawn but this doesn't parse.
    fn math_style(&self) -> Style {
        match self.typesetter {
            Some(_) => Style::ink(Ink::Hue(Hue::Red)),
            None => Style::capture("text.literal"),
        }
    }

    fn push_text(&mut self, text: &str, style: Style, source: u32) {
        let link = self.links.last().cloned();
        self.inline().push(text, style, link.as_ref(), source);
    }

    /// The innermost list, for its items.
    fn list(&mut self) -> Option<&mut Container> {
        self.containers
            .iter_mut()
            .rev()
            .find(|container| matches!(container, Container::List { .. }))
    }

    fn event(&mut self, event: Event, range: Range<usize>) {
        let source = self.line_of(range.start);
        let last = self.line_of(range.end.saturating_sub(1));
        self.current = source;
        if let Some(code) = &mut self.code {
            match event {
                Event::Text(text) => code.text.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    code.end = last;
                    let code = self.code.take().unwrap();
                    self.code_block(code);
                    self.gap = true;
                }
                _ => {}
            }
            return;
        }
        if let Some((text, _)) = &mut self.metadata {
            match event {
                Event::Text(more) => text.push_str(&more),
                Event::End(TagEnd::MetadataBlock(_)) => {
                    let (text, start) = self.metadata.take().unwrap();
                    self.raw_lines(&text, start + 1, Style::ink(Ink::Faint));
                    self.gap = true;
                }
                _ => {}
            }
            return;
        }
        if let Some((alt, _)) = &mut self.image {
            match event {
                Event::Text(text) | Event::Code(text) | Event::InlineMath(text) => {
                    alt.push_str(&text)
                }
                Event::SoftBreak | Event::HardBreak => alt.push(' '),
                Event::End(TagEnd::Image) => {
                    let (alt, url) = self.image.take().unwrap();
                    let text = match alt.trim() {
                        "" => "[image]".to_string(),
                        alt => format!("[image: {alt}]"),
                    };
                    // An image in a link, as a badge is, goes where the
                    // link does.
                    let link = self.links.last().cloned().unwrap_or(url);
                    let style = Style::ink(Ink::Muted).with(Attributes::ITALIC);
                    self.inline().push(&text, style, Some(&link), source);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, source),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                let style = self.inline_style();
                self.push_text(&text, style, source);
            }
            Event::Code(text) => {
                let style = merge(self.inline_style(), Style::capture("text.literal")).on_code();
                self.push_text(&text, style, source);
            }
            Event::InlineMath(text) => {
                let room = self.avail();
                if let Some(formula) = self.formula(&text, false, room) {
                    let style = self.inline_style();
                    let link = self.links.last().cloned();
                    self.inline()
                        .push_math(formula, style, link.as_ref(), source);
                    return;
                }
                let style = merge(self.inline_style(), self.math_style());
                self.push_text(&text, style, source);
            }
            Event::DisplayMath(text) => {
                let room = self.avail_wide();
                if let Some(formula) = self.formula(&text, true, room) {
                    let inline = self.inline();
                    inline.pieces.push(Piece::Break);
                    inline.push_math(formula, Style::TEXT, None, source);
                    inline.pieces.push(Piece::Break);
                    return;
                }
                let style = self.math_style();
                self.inline().pieces.push(Piece::Break);
                for (i, line) in text.trim_matches('\n').split('\n').enumerate() {
                    self.inline().pieces.push(Piece::Break);
                    self.push_text(line, style, source + i as u32);
                }
                self.inline().pieces.push(Piece::Break);
            }
            Event::Html(html) => match &mut self.html {
                Some((text, _)) => text.push_str(&html),
                None => self.html = Some((html.to_string(), source)),
            },
            Event::InlineHtml(html) => {
                let tag = html.trim().to_ascii_lowercase();
                if tag.starts_with("<br") {
                    self.inline().pieces.push(Piece::Break);
                }
                // Other tags are hidden, and what they hold shows as text.
            }
            Event::FootnoteReference(label) => {
                let style = Style::capture("text.reference");
                self.push_text(&format!("[{label}]"), style, source);
            }
            Event::SoftBreak => self.inline().space(),
            Event::HardBreak => self.inline().pieces.push(Piece::Break),
            Event::Rule => {
                self.start_block();
                let rule = "─".repeat(self.avail());
                let spans = vec![Span {
                    text: rule,
                    style: Style::ink(Ink::Border),
                    link: None,
                    math: None,
                }];
                self.emit(spans, source);
                self.gap = true;
            }
            Event::TaskListMarker(checked) => {
                let (mark, style) = match checked {
                    true => ("☑ ", Style::ink(Ink::Hue(Hue::Green))),
                    false => ("☐ ", Style::ink(Ink::Muted)),
                };
                if let Some(Container::Item { marker, width, .. }) = self.containers.last_mut() {
                    *marker = vec![Span {
                        text: mark.to_string(),
                        style,
                        link: None,
                        math: None,
                    }];
                    *width = mark.width();
                }
            }
        }
    }

    fn start(&mut self, tag: Tag, source: u32) {
        match tag {
            Tag::Paragraph => {
                self.start_block();
                // A list whose items are paragraphs has blank lines
                // between them.
                if let Some(Container::Item { .. }) = self.containers.last() {
                    if let Some(Container::List { loose, .. }) = self.list() {
                        *loose = true;
                    }
                }
                self.inline = Some((Inline::default(), Block::Paragraph));
            }
            Tag::Heading { .. } => {
                self.start_block();
                self.inline = Some((Inline::default(), Block::Heading));
            }
            Tag::BlockQuote(kind) => {
                self.start_block();
                let (label, hue) = match kind {
                    None => ("", None),
                    Some(BlockQuoteKind::Note) => ("Note", Some(Hue::Blue)),
                    Some(BlockQuoteKind::Tip) => ("Tip", Some(Hue::Green)),
                    Some(BlockQuoteKind::Important) => ("Important", Some(Hue::Mauve)),
                    Some(BlockQuoteKind::Warning) => ("Warning", Some(Hue::Yellow)),
                    Some(BlockQuoteKind::Caution) => ("Caution", Some(Hue::Red)),
                };
                self.containers.push(Container::Quote {
                    bar: hue.map_or(Ink::Border, Ink::Hue),
                    muted: hue.is_none(),
                });
                if let Some(hue) = hue {
                    let style = Style::ink(Ink::Hue(hue)).with(Attributes::BOLD);
                    let spans = vec![Span {
                        text: label.to_string(),
                        style,
                        link: None,
                        math: None,
                    }];
                    self.emit(spans, source);
                }
            }
            Tag::List(start) => {
                self.start_block();
                self.containers.push(Container::List {
                    next: start,
                    loose: false,
                    items: 0,
                });
            }
            Tag::Item => {
                self.flush_tight();
                let depth = self
                    .containers
                    .iter()
                    .filter(|container| matches!(container, Container::List { .. }))
                    .count()
                    .saturating_sub(1);
                let Some(Container::List { next, loose, items }) = self.list() else {
                    return;
                };
                let gap = *loose && *items > 0;
                *items += 1;
                let text = match next {
                    Some(n) => {
                        let text = format!("{n}. ");
                        *n += 1;
                        text
                    }
                    None => format!("{} ", BULLETS[depth % BULLETS.len()]),
                };
                self.gap = gap;
                self.start_block();
                let width = text.width();
                self.containers.push(Container::Item {
                    marker: vec![Span {
                        text,
                        style: Style::capture("text.list"),
                        link: None,
                        math: None,
                    }],
                    width,
                    pending: true,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.start_block();
                let text = format!("[{label}] ");
                let width = text.width();
                self.containers.push(Container::Item {
                    marker: vec![Span {
                        text,
                        style: Style::capture("text.reference"),
                        link: None,
                        math: None,
                    }],
                    width,
                    pending: true,
                });
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let (label, source) = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let label = info.split_whitespace().next().unwrap_or("").to_string();
                        (label, source + 1)
                    }
                    CodeBlockKind::Indented => (String::new(), source),
                };
                let math = label.eq_ignore_ascii_case("math");
                self.code = Some(Code {
                    label,
                    text: String::new(),
                    source,
                    end: source,
                    math,
                });
            }
            Tag::HtmlBlock => {
                self.start_block();
                self.html = None;
            }
            Tag::Table(alignments) => {
                self.start_block();
                self.table = Some(Table {
                    alignments,
                    rows: Vec::new(),
                    row: None,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.row = Some(Row {
                        cells: Vec::new(),
                        head: matches!(tag, Tag::TableHead),
                        source,
                    });
                }
            }
            Tag::TableCell => self.inline = Some((Inline::default(), Block::Cell)),
            Tag::Emphasis => self.styles.push(Attributes::ITALIC),
            Tag::Strong => self.styles.push(Attributes::BOLD),
            Tag::Strikethrough => self.styles.push(Attributes::STRIKETHROUGH),
            Tag::Superscript | Tag::Subscript => self.styles.push(Attributes::NONE),
            Tag::Link { dest_url, .. } => self.links.push(Rc::from(&*dest_url)),
            Tag::Image { dest_url, .. } => self.image = Some((String::new(), Rc::from(&*dest_url))),
            Tag::MetadataBlock(_) => {
                self.start_block();
                self.metadata = Some((String::new(), source));
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.start_block()
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if let Some((inline, _)) = self.inline.take() {
                    let base = self.base_style();
                    self.emit_inline(&inline, base);
                }
                self.gap = true;
            }
            TagEnd::Heading(level) => {
                let Some((inline, _)) = self.inline.take() else {
                    return;
                };
                let title = Style::capture("text.title").with(Attributes::BOLD);
                let base = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3 => title,
                    HeadingLevel::H4 => Style::TEXT.with(Attributes::BOLD),
                    _ => Style::ink(Ink::Muted).with(Attributes::BOLD),
                };
                let row = self.layout.lines.len();
                self.emit_inline(&inline, base);
                let rule = match level {
                    HeadingLevel::H1 => Some("━"),
                    HeadingLevel::H2 => Some("─"),
                    _ => None,
                };
                if let Some(rule) = rule {
                    let source = self.layout.lines.last().map_or(0, |line| line.source);
                    let spans = vec![Span {
                        text: rule.repeat(self.avail()),
                        style: Style::ink(Ink::Border),
                        link: None,
                        math: None,
                    }];
                    self.emit(spans, source);
                }
                let mut anchor = slug(&inline.plain());
                let taken = |anchor: &str| self.layout.anchors.iter().any(|(a, _)| a == anchor);
                if taken(&anchor) {
                    let base = anchor.clone();
                    let mut n = 1;
                    while taken(&format!("{base}-{n}")) {
                        n += 1;
                    }
                    anchor = format!("{base}-{n}");
                }
                self.layout.anchors.push((anchor, row));
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_tight();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::List(_) => {
                self.flush_tight();
                self.containers.pop();
                self.gap = true;
            }
            TagEnd::Item | TagEnd::FootnoteDefinition => {
                self.flush_tight();
                // An empty item still shows its marker.
                if let Some(Container::Item { pending: true, .. }) = self.containers.last() {
                    let source = self.layout.lines.last().map_or(0, |line| line.source);
                    self.emit(Vec::new(), source);
                }
                self.containers.pop();
                if tag == TagEnd::FootnoteDefinition {
                    self.gap = true;
                }
            }
            TagEnd::HtmlBlock => {
                if let Some((html, source)) = self.html.take() {
                    let trimmed = html.trim();
                    let comment = trimmed.starts_with("<!--") && trimmed.ends_with("-->");
                    if !comment && !trimmed.is_empty() {
                        self.raw_lines(trimmed, source, Style::ink(Ink::Faint));
                        self.gap = true;
                    }
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_block(table);
                }
                self.gap = true;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    if let Some(row) = table.row.take() {
                        table.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                let pieces = self
                    .inline
                    .take()
                    .map(|(inline, _)| inline.pieces)
                    .unwrap_or_default();
                if let Some(Row { cells, head, .. }) =
                    self.table.as_mut().and_then(|table| table.row.as_mut())
                {
                    let mut pieces = pieces;
                    if *head {
                        for piece in &mut pieces {
                            if let Piece::Word { style, .. } = piece {
                                *style = style.with(Attributes::BOLD);
                            }
                        }
                    }
                    cells.push(pieces);
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => {
                self.flush_tight();
                self.gap = true;
            }
            TagEnd::CodeBlock | TagEnd::Image | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// Adds `text` line by line, broken where too long for the line,
    /// starting at file line `source`.
    fn raw_lines(&mut self, text: &str, source: u32, style: Style) {
        let width = self.avail_wide();
        for (i, line) in text.trim_end_matches('\n').split('\n').enumerate() {
            let line = line.trim_end_matches('\r');
            let spans = vec![Span {
                text: line.to_string(),
                style,
                link: None,
                math: None,
            }];
            for spans in hard_wrap(spans, width) {
                self.emit(spans, source + i as u32);
            }
        }
    }

    /// A code block, on a band of background: a row with its language,
    /// its lines colored as that language, and a row to close it.
    fn code_block(&mut self, code: Code) {
        let mut text = code.text;
        if text.ends_with('\n') {
            text.pop();
        }
        if code.math {
            let room = self.avail_wide();
            if let Some(formula) = self.formula(&text, true, room) {
                let mut inline = Inline::default();
                inline.push_math(formula, Style::TEXT, None, code.source);
                for (spans, _) in wrap(&inline.pieces, self.avail()) {
                    self.emit(spans, code.source);
                }
                return;
            }
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let longest = lines
            .iter()
            .map(|line| expand_tabs(line).width())
            .max()
            .unwrap_or(0);
        let band = (longest + 2).min(self.avail_wide()).max(self.avail());
        let colors = match code.math {
            true => None,
            false => language::injected(&code.label).and_then(|language| {
                let mut start = 0;
                let ranges: Vec<Range<usize>> = lines
                    .iter()
                    .map(|line| {
                        let range = start..start + line.len();
                        start += line.len() + 1;
                        range
                    })
                    .collect();
                self.code_blocks += 1;
                let path = PathBuf::from(format!("{}#code-{}", self.name, self.code_blocks));
                let stop = AtomicBool::new(false);
                self.highlighter
                    .highlight(&path, language, &text, &ranges, &stop)
            }),
        };
        let plain = match code.math {
            true => Style::capture("text.literal").on_code(),
            false => Style::TEXT.on_code(),
        };
        let mut top = Vec::new();
        let label = code.label.to_lowercase();
        let label_width = label.width();
        if label_width + 2 <= band {
            push_span(&mut top, &" ".repeat(band - label_width - 1), plain, None);
            push_span(&mut top, &label, Style::ink(Ink::Faint).on_code(), None);
        }
        pad(&mut top, band, plain);
        self.emit(top, code.source.saturating_sub(1));
        for (i, line) in lines.iter().enumerate() {
            let mut spans = Vec::new();
            let line_colors = colors.as_ref().and_then(|colors| colors.get(i));
            let mut at = 0;
            for (range, color) in line_colors.into_iter().flatten() {
                if range.start > at {
                    push_span(&mut spans, &line[at..range.start], plain, None);
                }
                let style = Style {
                    fg: Ink::Syntax(*color),
                    code: true,
                    attributes: color.attributes(),
                };
                push_span(&mut spans, &line[range.clone()], style, None);
                at = range.end;
            }
            if at < line.len() {
                push_span(&mut spans, &line[at..], plain, None);
            }
            for span in &mut spans {
                span.text = expand_tabs(&span.text);
            }
            for mut row in hard_wrap(spans, band.saturating_sub(2)) {
                row.insert(
                    0,
                    Span {
                        text: " ".to_string(),
                        style: plain,
                        link: None,
                        math: None,
                    },
                );
                pad(&mut row, band, plain);
                self.emit(row, code.source + i as u32);
            }
        }
        let mut bottom = Vec::new();
        pad(&mut bottom, band, plain);
        self.emit(bottom, code.end.max(code.source));
    }

    /// A table drawn with box characters. Columns too wide for the panel
    /// are narrowed, the widest first, and their text wrapped.
    fn table_block(&mut self, table: Table) {
        let columns = table
            .rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0)
            .max(table.alignments.len());
        if columns == 0 {
            return;
        }
        fn cell(row: &Row, c: usize) -> &[Piece] {
            row.cells.get(c).map_or(&[], |cell| &cell[..])
        }
        let natural: Vec<usize> = (0..columns)
            .map(|c| {
                table
                    .rows
                    .iter()
                    .map(|row| natural_width(cell(row, c)))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let longest_word: Vec<usize> = (0..columns)
            .map(|c| {
                table
                    .rows
                    .iter()
                    .flat_map(|row| words(cell(row, c)).map(word_width))
                    .max()
                    .unwrap_or(1)
                    .clamp(1, natural[c])
            })
            .collect();
        // Each column has a space either side and a bar after, and the
        // table a bar before.
        let budget = self.avail_wide().saturating_sub(3 * columns + 1);
        let mut widths = natural.clone();
        shrink(&mut widths, &longest_word, budget);
        shrink(&mut widths, &vec![1; columns], budget);

        // Each row's cells, as lines.
        let wrapped: Vec<Vec<Vec<Vec<Span>>>> = table
            .rows
            .iter()
            .map(|row| {
                (0..columns)
                    .map(|c| {
                        wrap(cell(row, c), widths[c])
                            .into_iter()
                            .map(|(spans, _)| spans)
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let tall = wrapped
            .iter()
            .any(|cells| cells.iter().any(|lines| lines.len() > 1));
        let border = Style::ink(Ink::Border);
        let rule = |left: &str, line: &str, cross: &str, right: &str| {
            let mut text = left.to_string();
            for (c, width) in widths.iter().enumerate() {
                text.push_str(&line.repeat(width + 2));
                text.push_str(if c + 1 < columns { cross } else { right });
            }
            vec![Span {
                text,
                style: border,
                link: None,
                math: None,
            }]
        };
        // Rules are from the line before the row after them, so that a
        // row's line finds its text: the header's rule from the line under
        // the header, `|---|`.
        let rows = &table.rows;
        let before = self.layout.lines.last().map_or(0, |line| line.source);
        let source = rows.first().map_or(0, |row| row.source);
        self.emit(
            rule("┌", "─", "┬", "┐"),
            source.saturating_sub(1).max(before),
        );
        for (r, (row, cells)) in rows.iter().zip(&wrapped).enumerate() {
            let source = row.source;
            if r > 0 {
                let above = source.saturating_sub(1).max(rows[r - 1].source);
                if rows[r - 1].head && !row.head {
                    self.emit(rule("╞", "═", "╪", "╡"), above);
                } else if tall {
                    self.emit(rule("├", "─", "┼", "┤"), above);
                }
            }
            let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
            for i in 0..height {
                let mut spans = Vec::new();
                push_span(&mut spans, "│", border, None);
                for (c, lines) in cells.iter().enumerate() {
                    let content = lines.get(i).cloned().unwrap_or_default();
                    let room = widths[c].saturating_sub(spans_width(&content));
                    let before = match table.alignments.get(c) {
                        Some(Alignment::Right) => room,
                        Some(Alignment::Center) => room / 2,
                        _ => 0,
                    };
                    push_span(&mut spans, &" ".repeat(1 + before), Style::TEXT, None);
                    for span in content {
                        append(&mut spans, span);
                    }
                    push_span(
                        &mut spans,
                        &" ".repeat(1 + room - before),
                        Style::TEXT,
                        None,
                    );
                    push_span(&mut spans, "│", border, None);
                }
                self.emit(spans, source);
            }
        }
        let source = rows.last().map_or(0, |row| row.source);
        self.emit(rule("└", "─", "┴", "┘"), source);
    }
}

/// `inner`'s style over `base`: its color unless it's plain text's, and
/// both's attributes.
fn merge(base: Style, inner: Style) -> Style {
    Style {
        fg: match inner.fg {
            Ink::Text => base.fg,
            fg => fg,
        },
        code: base.code || inner.code,
        attributes: base.attributes | inner.attributes,
    }
}

/// Narrows the widest of `widths`, down to `mins`, until they add up to at
/// most `budget`.
fn shrink(widths: &mut [usize], mins: &[usize], budget: usize) {
    while widths.iter().sum::<usize>() > budget {
        let Some(widest) = (0..widths.len())
            .filter(|&c| widths[c] > mins[c])
            .max_by_key(|&c| widths[c])
        else {
            return;
        };
        widths[widest] -= 1;
    }
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

// --- the view ---------------------------------------------------------------

/// A place in the laid out text: a row, and a column in it.
type Spot = (usize, usize);

/// What a mouse event in the reader asks of the editor.
#[derive(Debug, Clone, PartialEq)]
pub enum ReaderEvent {
    /// Follow a link somewhere outside the text.
    Link(String),
    /// Go to editing, with the cursor on file line `line`, which was at
    /// `row` of the view.
    Edit { line: u32, row: u32 },
}

struct Laid {
    layout: Layout,
    epoch: u64,
    width: u32,
    /// The cell size math was laid out for, if it's drawn.
    cell: Option<(u32, u32)>,
}

struct Click {
    at: Spot,
    time: Instant,
    count: u32,
}

pub struct Reader {
    buffer: Rc<EditBuffer>,
    /// Names the file, for the highlighter's cache.
    name: String,
    highlighter: RefCell<ExcerptHighlighter>,
    typesetter: RefCell<Option<Typesetter>>,
    laid: RefCell<Option<Laid>>,
    /// The row at the top of the view.
    top: Cell<usize>,
    /// A file line to scroll to once the text is laid out, and how many
    /// rows down the view to put it.
    pending: Cell<Option<(u32, u32)>>,
    width: u32,
    height: u32,
    /// Where a selection started, and where it goes to.
    selection: Option<(Spot, Spot)>,
    /// Where the left button went down, until it's released.
    press: Option<Spot>,
    dragged: bool,
    last_click: Option<Click>,
}

impl Reader {
    /// A reader of `buffer`'s text that starts with file line `top` at
    /// the top.
    pub fn new(buffer: Rc<EditBuffer>, name: String, top: u32) -> Reader {
        Reader {
            buffer,
            name,
            highlighter: RefCell::new(ExcerptHighlighter::new()),
            typesetter: RefCell::new(None),
            laid: RefCell::new(None),
            top: Cell::new(0),
            pending: Cell::new(Some((top, 0))),
            width: 0,
            height: 0,
            selection: None,
            press: None,
            dragged: false,
            last_click: None,
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// Columns left of the text.
    fn margin(&self) -> u32 {
        match self.width {
            0..40 => 1,
            _ => 2,
        }
    }

    fn text_width(&self) -> u32 {
        self.width.saturating_sub(self.margin() + 1).max(1)
    }

    /// Lays the text out again if it or the width changed, keeping the
    /// same file line at the top.
    fn sync(&self) {
        let epoch = self.buffer.content_epoch();
        let width = self.text_width();
        let cell = math::cell();
        let current = matches!(&*self.laid.borrow(), Some(laid) if laid.epoch == epoch && laid.width == width && laid.cell == cell);
        if !current {
            let old = self.laid.borrow_mut().take();
            let keep = old.as_ref().map(|old| {
                let top = self.top.get();
                let source = old.layout.source_of(top);
                (source, top.saturating_sub(old.layout.row_of(source)))
            });
            let mut typesetter = self.typesetter.borrow_mut();
            if typesetter.as_ref().map(Typesetter::cell) != cell {
                *typesetter = cell.map(Typesetter::new);
            }
            let layout = lay_out(
                &self.buffer.text(),
                width as usize,
                &mut self.highlighter.borrow_mut(),
                &self.name,
                typesetter.as_mut(),
            );
            if let Some((source, within)) = keep {
                let row = layout.row_of(source);
                let rows = layout.lines[row.min(layout.lines.len())..]
                    .iter()
                    .take_while(|line| line.source == source)
                    .count();
                self.top.set(row + within.min(rows.saturating_sub(1)));
            }
            *self.laid.borrow_mut() = Some(Laid {
                layout,
                epoch,
                width,
                cell,
            });
        }
        if let Some((source, down)) = self.pending.take() {
            let row = self.with_layout(|layout| layout.row_of(source));
            self.top.set(row.saturating_sub(down as usize));
        }
        self.top.set(self.top.get().min(self.max_top()));
    }

    fn with_layout<R>(&self, f: impl FnOnce(&Layout) -> R) -> R {
        match &*self.laid.borrow() {
            Some(laid) => f(&laid.layout),
            None => f(&Layout::default()),
        }
    }

    fn rows(&self) -> usize {
        self.with_layout(|layout| layout.lines.len())
    }

    fn max_top(&self) -> usize {
        self.rows().saturating_sub(self.height as usize)
    }

    /// Lays the text out again, as when the theme changed.
    pub fn invalidate(&mut self) {
        if let Some(laid) = self.laid.get_mut() {
            laid.epoch = u64::MAX;
        }
    }

    /// The row at the top of the view.
    pub fn top_row(&self) -> u32 {
        self.sync();
        self.top.get() as u32
    }

    /// The file line at the top of the view.
    pub fn top_line(&self) -> u32 {
        self.sync();
        self.with_layout(|layout| layout.source_of(self.top.get()))
    }

    /// Scrolls file line `line` to `row` rows down the view, as near as
    /// the text allows.
    pub fn scroll_line_to(&mut self, line: u32, row: u32) {
        self.pending.set(Some((line, row)));
    }

    pub fn scroll(&mut self, rows: i64) {
        self.sync();
        let top = self.top.get() as i64 + rows;
        self.top.set(top.clamp(0, self.max_top() as i64) as usize);
    }

    pub fn page(&self) -> i64 {
        self.height.saturating_sub(1).max(1) as i64
    }

    pub fn scroll_to_end(&mut self, end: bool) {
        self.sync();
        self.top.set(if end { self.max_top() } else { 0 });
    }

    pub fn has_selection(&self) -> bool {
        self.selection.is_some_and(|(a, b)| a != b)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn select_all(&mut self) {
        self.sync();
        let rows = self.rows();
        self.selection = Some(((0, 0), (rows, 0)));
    }

    /// The selection, start first.
    fn ordered_selection(&self) -> Option<(Spot, Spot)> {
        let (a, b) = self.selection?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The selected text, as shown, without spaces at the ends of lines.
    pub fn selected_text(&self) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        self.sync();
        let (start, end) = self.ordered_selection()?;
        let text = self.with_layout(|layout| {
            let mut out = Vec::new();
            for row in start.0..=end.0.min(layout.lines.len().saturating_sub(1)) {
                let line = &layout.lines[row];
                // Display math's rows after its first copy as nothing.
                if line
                    .spans
                    .iter()
                    .any(|span| span.math.as_ref().is_some_and(|mark| mark.row > 0))
                {
                    continue;
                }
                let from = if row == start.0 { start.1 } else { 0 };
                let to = if row == end.0 { end.1 } else { usize::MAX };
                out.push(line.copy(from..to).trim_end().to_string());
            }
            out.join("\n")
        });
        Some(text)
    }

    /// The row and column at cell (`x`, `y`) of the view.
    fn spot(&self, x: u32, y: u32) -> Spot {
        let row = self.top.get() + y as usize;
        let col = x.saturating_sub(self.margin()) as usize;
        (row, col)
    }

    /// The link at `spot`, if any.
    fn link_at(&self, (row, col): Spot) -> Option<Rc<str>> {
        self.with_layout(|layout| {
            let line = layout.lines.get(row)?;
            let mut at = 0;
            for span in &line.spans {
                let width = span.text.width();
                if (at..at + width).contains(&col) {
                    return span.link.clone();
                }
                at += width;
            }
            None
        })
    }

    /// Scrolls to the heading with `anchor`. Returns whether there is one.
    pub fn go_to_anchor(&mut self, anchor: &str) -> bool {
        self.sync();
        let anchor = slug(anchor);
        let row = self.with_layout(|layout| {
            layout
                .anchors
                .iter()
                .find(|(a, _)| *a == anchor)
                .map(|(_, row)| *row)
        });
        if let Some(row) = row {
            self.top.set(row.min(self.max_top()));
        }
        row.is_some()
    }

    /// A mouse event at cell (`mouse.x`, `mouse.y`) of the view.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> Option<ReaderEvent> {
        self.sync();
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                let at = self.spot(mouse.x, mouse.y);
                let count = match &self.last_click {
                    Some(click)
                        if click.at == at && now.duration_since(click.time) < MULTI_CLICK =>
                    {
                        click.count + 1
                    }
                    _ => 1,
                };
                self.last_click = Some(Click {
                    at,
                    time: now,
                    count,
                });
                if count == 2 {
                    self.press = None;
                    let line = self.with_layout(|layout| layout.source_of(at.0));
                    return Some(ReaderEvent::Edit { line, row: mouse.y });
                }
                match (mouse.mods.shift, self.selection) {
                    (true, Some((anchor, _))) => self.selection = Some((anchor, at)),
                    _ => self.selection = None,
                }
                self.press = Some(at);
                self.dragged = false;
            }
            MouseKind::Drag(MouseButton::Left) => {
                let origin = self.press?;
                // Dragged past the top or bottom, it scrolls.
                if mouse.y >= self.height {
                    self.scroll(1);
                } else if mouse.y == 0 {
                    self.scroll(-1);
                }
                let y = mouse.y.min(self.height.saturating_sub(1));
                let at = self.spot(mouse.x, y);
                self.dragged = self.dragged || at != origin;
                if self.dragged {
                    self.selection = Some((origin, at));
                }
            }
            MouseKind::Release(MouseButton::Left) => {
                let origin = self.press.take()?;
                if self.dragged {
                    return None;
                }
                let link = self.link_at(origin)?;
                if let Some(anchor) = link.strip_prefix('#') {
                    self.go_to_anchor(anchor);
                    return None;
                }
                return Some(ReaderEvent::Link(link.to_string()));
            }
            MouseKind::ScrollUp => self.scroll(-(WHEEL_ROWS as i64)),
            MouseKind::ScrollDown => self.scroll(WHEEL_ROWS as i64),
            _ => {}
        }
        None
    }

    /// Draws the view at (`x`, `y`).
    pub fn draw(&self, frame: &Buffer, x: u32, y: u32) {
        self.sync();
        let colors = theme::colors();
        frame.fill_rect(x, y, self.width, self.height, colors.bg);
        let margin = self.margin();
        let selection = self.ordered_selection();
        let laid = self.laid.borrow();
        let Some(laid) = &*laid else {
            return;
        };
        let lines = &laid.layout.lines;
        if lines.is_empty() {
            frame.draw_text(
                "Nothing to read.",
                x + margin,
                y,
                colors.muted,
                None,
                Attributes::NONE,
            );
            return;
        }
        let top = self.top.get();
        let mut formulas = Vec::new();
        for (i, line) in lines
            .iter()
            .skip(top)
            .take(self.height as usize)
            .enumerate()
        {
            let row = top + i;
            let selected = selection.and_then(|(start, end)| {
                if row < start.0 || row > end.0 {
                    return None;
                }
                let from = if row == start.0 { start.1 } else { 0 };
                let to = if row == end.0 { end.1 } else { usize::MAX };
                (from < to).then_some(from..to)
            });
            let screen_y = y + i as u32;
            let mut col = 0;
            for span in &line.spans {
                let width = span.text.width();
                let fg = span.style.fg.rgba(&colors);
                let bg = span.style.code.then_some(colors.surface_inactive);
                let attributes = span.style.attributes;
                // Split where the selection starts and ends.
                let bounds = [
                    col,
                    selected
                        .as_ref()
                        .map_or(col, |s| s.start.clamp(col, col + width)),
                    selected
                        .as_ref()
                        .map_or(col, |s| s.end.clamp(col, col + width)),
                    col + width,
                ];
                for (n, part) in bounds.windows(2).enumerate() {
                    let (from, to) = (part[0], part[1]);
                    if from >= to {
                        continue;
                    }
                    let text = columns(&span.text, from - col..to - col);
                    let bg = if n == 1 { Some(colors.selection) } else { bg };
                    let screen_x = x + margin + from as u32;
                    frame.draw_text(text, screen_x, screen_y, fg, bg, attributes);
                }
                // A formula, from its first row, or from the top of the
                // view if that's above it.
                if let Some(mark) = span.math.as_ref().filter(|mark| mark.row == 0 || i == 0) {
                    let at = (
                        (x + margin) as i32 + col as i32,
                        screen_y as i32 - mark.row as i32,
                    );
                    formulas.push((mark.formula.clone(), at, [fg.r(), fg.g(), fg.b()]));
                }
                col += width;
            }
        }
        // After the text, which would replace the cells of a formula's
        // rows after its first.
        frame.with_clip(x, y, self.width, self.height, || {
            for (formula, (image_x, image_y), fg) in formulas {
                if let Some(image) = formula.image(fg) {
                    let (cols, rows) = (formula.cols, formula.rows);
                    let (cell_w, cell_h) = formula.cell;
                    frame.draw_image(
                        &image,
                        image_x,
                        image_y,
                        cols,
                        rows,
                        cols * cell_w,
                        rows * cell_h,
                    );
                }
            }
        });
    }

    pub fn status(&self) -> Status {
        self.sync();
        let top = self.top.get();
        let line = self.with_layout(|layout| layout.source_of(top)) + 1;
        let max_top = self.max_top();
        let position = match (top, max_top) {
            (_, 0) => "All".to_string(),
            (0, _) => "Top".to_string(),
            (top, max) if top >= max => "Bot".to_string(),
            (top, max) => format!("{}%", top * 100 / max),
        };
        Status::Info(format!("Reader  Ln {line}  {position}"))
    }
}

/// The part of `text` in screen columns `range`.
pub fn columns(text: &str, range: Range<usize>) -> &str {
    let mut col = 0;
    let mut start = text.len();
    let mut end = text.len();
    for (i, c) in text.char_indices() {
        if col >= range.start && start == text.len() {
            start = i;
        }
        if col >= range.end {
            end = i;
            break;
        }
        col += c.width().unwrap_or(0);
    }
    if start > end {
        return "";
    }
    &text[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str, width: usize) -> Vec<String> {
        let mut highlighter = ExcerptHighlighter::new();
        lay_out(text, width, &mut highlighter, "test.md", None)
            .lines
            .iter()
            .map(|line| line.text().trim_end().to_string())
            .collect()
    }

    fn layout(text: &str, width: usize) -> Layout {
        let mut highlighter = ExcerptHighlighter::new();
        lay_out(text, width, &mut highlighter, "test.md", None)
    }

    #[test]
    fn markup_is_hidden_and_prose_wraps() {
        let lines = render("Some **bold** and *italic* and `code` text here.\n", 20);
        assert_eq!(lines, ["Some bold and italic", "and code text here."]);
    }

    #[test]
    fn prose_wraps_at_the_measure() {
        let text = "word ".repeat(60);
        let lines = render(&text, 200);
        assert!(
            lines.iter().all(|line| line.width() <= MEASURE),
            "{lines:?}"
        );
        assert!(lines.len() > 1);
    }

    #[test]
    fn long_words_break() {
        let lines = render("abcdefghijklmnop\n", 6);
        assert_eq!(lines, ["abcdef", "ghijkl", "mnop"]);
    }

    #[test]
    fn headings_lose_their_hashes_and_get_rules() {
        let lines = render("# Title\n\n## Part\n\n### Sub\n\ntext\n", 10);
        assert_eq!(
            lines,
            [
                "Title",
                "━━━━━━━━━━",
                "",
                "Part",
                "──────────",
                "",
                "Sub",
                "",
                "text"
            ]
        );
    }

    #[test]
    fn heading_styles() {
        let laid = layout("# Title\n\nplain\n", 20);
        let title = &laid.lines[0].spans[0];
        assert!(title.style.attributes.contains(Attributes::BOLD));
        assert_eq!(laid.lines[3].spans[0].style, Style::TEXT);
    }

    #[test]
    fn lists_have_bullets_and_hanging_indents() {
        let text = "- one two three four\n- two\n  - nested\n\n1. first\n2. second\n";
        let lines = render(text, 12);
        assert_eq!(
            lines,
            [
                "• one two",
                "  three four",
                "• two",
                "  ◦ nested",
                "",
                "1. first",
                "2. second"
            ]
        );
    }

    #[test]
    fn loose_lists_have_blank_lines_between_items() {
        let lines = render("- one\n\n- two\n", 20);
        assert_eq!(lines, ["• one", "", "• two"]);
    }

    #[test]
    fn task_lists_have_boxes() {
        let lines = render("- [ ] todo\n- [x] done\n", 20);
        assert_eq!(lines, ["☐ todo", "☑ done"]);
    }

    #[test]
    fn quotes_and_alerts_have_bars() {
        let lines = render("> quoted\n> text\n\n> [!NOTE]\n> heads up\n", 20);
        assert_eq!(lines, ["▎ quoted text", "", "▎ Note", "▎ heads up"]);
    }

    #[test]
    fn tables_are_boxed_and_aligned() {
        let text = "| a | b |\n|:-:|--:|\n| one | 2 |\n| three | 40 |\n";
        let lines = render(text, 40);
        assert_eq!(
            lines,
            [
                "┌───────┬────┐",
                "│   a   │  b │",
                "╞═══════╪════╡",
                "│  one  │  2 │",
                "│ three │ 40 │",
                "└───────┴────┘",
            ]
        );
    }

    #[test]
    fn narrow_tables_wrap_their_widest_column() {
        let text = "| k | description |\n|---|---|\n| a | a long description of it |\n";
        let lines = render(text, 24);
        assert!(lines.iter().all(|line| line.width() <= 24), "{lines:#?}");
        // Rows of several lines are ruled apart.
        assert!(lines.iter().any(|line| line.starts_with("│ a │ a long")));
        assert_eq!(lines.last().unwrap().chars().next(), Some('└'));
    }

    #[test]
    fn code_blocks_are_banded_and_labelled() {
        let laid = layout("```rust\nfn main() {}\n```\n", 30);
        let lines: Vec<String> = laid.lines.iter().map(Line::text).collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].trim_end().ends_with("rust"));
        assert_eq!(lines[1].trim_end(), " fn main() {}");
        assert!(lines.iter().all(|line| line.width() == 30), "{lines:?}");
        assert!(laid.lines[1].spans.iter().all(|span| span.style.code));
        // Colored as Rust.
        assert!(laid.lines[1]
            .spans
            .iter()
            .any(|span| matches!(span.style.fg, Ink::Syntax(_))));
    }

    #[test]
    fn rows_know_their_file_lines() {
        let text = "# Title\n\nfirst paragraph\n\n```\ncode\n```\n\n- item\n";
        let laid = layout(text, 40);
        let sources: Vec<(String, u32)> = laid
            .lines
            .iter()
            .map(|line| (line.text().trim().to_string(), line.source))
            .collect();
        let find = |text: &str| {
            sources
                .iter()
                .find(|(t, _)| t == text)
                .map(|(_, source)| *source)
        };
        assert_eq!(find("Title"), Some(0));
        assert_eq!(find("first paragraph"), Some(2));
        assert_eq!(find("code"), Some(5));
        assert_eq!(find("• item"), Some(8));
        assert_eq!(laid.row_of(2), 3);
    }

    #[test]
    fn table_rows_find_their_text_not_their_rules() {
        let text = "intro\n\n| a | b |\n|---|---|\n| one | x |\n| two two two two | y |\n";
        let laid = layout(text, 16);
        let row_text = |line: u32| laid.lines[laid.row_of(line)].text();
        assert!(row_text(2).contains(" a "), "{}", row_text(2));
        assert!(row_text(4).contains("one"), "{}", row_text(4));
        assert!(row_text(5).contains("two"), "{}", row_text(5));
    }

    #[test]
    fn math_and_escaped_dollars() {
        let lines = render("costs \\$5 and $x^2$ here\n", 40);
        assert_eq!(lines, ["costs $5 and x^2 here"]);
        let lines = render("$$\n\\int f\n$$\n", 40);
        assert_eq!(lines, ["\\int f"]);
    }

    /// Laid out with math drawn, for cells 10 x 20 pixels in size.
    fn layout_math(text: &str, width: usize) -> Layout {
        let mut highlighter = ExcerptHighlighter::new();
        let mut typesetter = Typesetter::new((10, 20));
        lay_out(
            text,
            width,
            &mut highlighter,
            "test.md",
            Some(&mut typesetter),
        )
    }

    fn marks(line: &Line) -> Vec<(String, u32)> {
        line.spans
            .iter()
            .filter_map(|span| span.math.as_ref())
            .map(|mark| (mark.formula.tex.clone(), mark.row))
            .collect()
    }

    #[test]
    fn inline_math_is_a_word_of_its_own_width() {
        let laid = layout_math("so $x_i^2$, and $y$ too\n", 40);
        assert_eq!(laid.lines.len(), 1);
        let line = &laid.lines[0];
        assert_eq!(
            marks(line),
            [("x_i^2".to_string(), 0), ("y".to_string(), 0)]
        );
        let span = line.spans.iter().find(|span| span.math.is_some()).unwrap();
        let formula = &span.math.as_ref().unwrap().formula;
        assert_eq!(span.text, " ".repeat(formula.cols as usize));
        // The comma stays with it.
        assert!(line.text().starts_with(&format!("so {},", span.text)));
        assert_eq!(line.copy(0..usize::MAX), "so $x_i^2$, and $y$ too");
    }

    #[test]
    fn inline_math_wraps_whole() {
        let laid = layout_math("aaaa bbbb $x + y + z$\n", 12);
        let rows: Vec<Vec<(String, u32)>> = laid.lines.iter().map(marks).collect();
        assert_eq!(rows.last().unwrap(), &[("x + y + z".to_string(), 0)]);
        assert!(rows[..rows.len() - 1].iter().all(Vec::is_empty));
    }

    #[test]
    fn display_math_takes_rows_of_its_own() {
        let laid = layout_math("Before\n$$\n\\frac{a}{b}\n$$\nafter\n", 40);
        let lines: Vec<String> = laid
            .lines
            .iter()
            .map(|line| line.text().trim().to_string())
            .collect();
        assert_eq!(lines.first().unwrap(), "Before");
        assert_eq!(lines.last().unwrap(), "after");
        let rows: Vec<u32> = laid
            .lines
            .iter()
            .flat_map(marks)
            .map(|(_, row)| row)
            .collect();
        assert!(rows.len() >= 2, "{rows:?}");
        assert_eq!(rows, (0..rows.len() as u32).collect::<Vec<_>>());
        // Centered.
        let first = &laid.lines[1];
        let pad = first.spans[0].text.width();
        let cols = first.spans[1].text.width();
        assert!(
            pad > 0 && pad.abs_diff(40 - pad - cols) <= 1,
            "{pad} {cols}"
        );
        assert_eq!(first.copy(0..usize::MAX).trim(), "$$\\frac{a}{b}$$");
    }

    #[test]
    fn math_code_blocks_are_display_math() {
        let laid = layout_math("```math\nx^2\n```\n", 40);
        assert!(
            laid.lines.iter().all(|line| marks(line).len() == 1),
            "{:?}",
            laid.lines
        );
    }

    #[test]
    fn math_in_tables_and_quotes() {
        let laid = layout_math("| a | b |\n|---|---|\n| $x$ | 1 |\n", 40);
        assert!(laid.lines.iter().any(|line| !marks(line).is_empty()));
        let laid = layout_math("> $$\n> x\n> $$\n", 40);
        let row = laid
            .lines
            .iter()
            .find(|line| !marks(line).is_empty())
            .unwrap();
        assert!(row.text().starts_with(QUOTE_BAR));
    }

    #[test]
    fn math_that_does_not_parse_is_text() {
        let laid = layout_math("see $\\left( x$ here\n", 40);
        assert_eq!(laid.lines[0].text(), "see \\left( x here");
        assert!(laid.lines[0].spans.iter().all(|span| span.math.is_none()));
        let error = laid.lines[0]
            .spans
            .iter()
            .find(|span| span.text.contains("left"))
            .unwrap();
        assert_eq!(error.style.fg, Ink::Hue(Hue::Red));
    }

    #[test]
    fn links_are_underlined_and_remember_where_they_go() {
        let laid = layout("see [the docs](docs/a.md) now\n", 40);
        let link = laid.lines[0]
            .spans
            .iter()
            .find(|span| span.link.is_some())
            .unwrap();
        assert_eq!(link.text, "the docs");
        assert_eq!(link.link.as_deref(), Some("docs/a.md"));
        assert!(link.style.attributes.contains(Attributes::UNDERLINE));
    }

    #[test]
    fn images_show_their_alt_text() {
        let lines = render("![a cat](cat.png)\n", 40);
        assert_eq!(lines, ["[image: a cat]"]);
    }

    #[test]
    fn html_comments_are_hidden_and_tags_dropped() {
        let lines = render("<!-- hidden -->\n\npress <kbd>Ctrl</kbd>\n", 40);
        assert_eq!(lines, ["press Ctrl"]);
    }

    #[test]
    fn anchors_follow_github() {
        let laid = layout("# Hello, World!\n\n# Hello, World!\n", 40);
        let anchors: Vec<&str> = laid.anchors.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(anchors, ["hello-world", "hello-world-1"]);
    }

    #[test]
    fn front_matter_is_faint() {
        let laid = layout("---\ntitle: x\n---\n\nbody\n", 40);
        assert_eq!(laid.lines[0].text(), "title: x");
        assert_eq!(laid.lines[0].spans[0].style.fg, Ink::Faint);
    }

    #[test]
    fn columns_cut_by_screen_width() {
        assert_eq!(columns("héllo", 1..3), "él");
        assert_eq!(columns("a日本", 1..3), "日");
        assert_eq!(columns("abc", 2..usize::MAX), "c");
        assert_eq!(columns("abc", 5..9), "");
    }
}
