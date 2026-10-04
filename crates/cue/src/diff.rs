//! Diff mode: a file shown as it changed since the last commit, staged or
//! not, with changes not yet saved: the lines taken out, on red, above the
//! lines put in their place, on green, with the words that changed
//! brighter. Unchanged lines more than a few away from a change are folded
//! away, and a click on a fold shows them.
//!
//! As the reader does, it lays the text out again whenever it, the panel's
//! width, or the last commit changes, so edits made elsewhere (in another
//! panel, or on disk by an agent) show as they happen.
//!
//! It can't be typed in. The wheel and the arrow and page keys scroll,
//! dragging selects text to copy, as written, without the line numbers,
//! and a double click goes to editing with the cursor on the line clicked.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use opentui::{Attributes, Buffer, Rgba};
use similar::{Algorithm, ChangeTag, DiffOp, InlineChangeOptions, TextDiff};
use unicode_width::UnicodeWidthChar;

use crate::config;
use crate::document::Document;
use crate::git::{Base, Kind, Tracked};
use crate::input::{Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::language::Language;
use crate::reader::columns;
use crate::status::Status;
use crate::syntax::{ExcerptHighlighter, LineColors};
use crate::theme::{self, Colors, Hue, SyntaxColor};

/// Unchanged lines shown before and after each change.
const CONTEXT: usize = 3;
const WHEEL_ROWS: i64 = 3;
/// How long diffing may take before it settles for a rougher diff.
const TIMEOUT: Duration = Duration::from_millis(200);
/// Line numbers are left out when they'd leave the text less room than
/// this.
const MIN_TEXT_WIDTH: usize = 20;
/// How much of a change's hue is behind its lines, and behind the words in
/// them that changed.
const LINE_TINT: f32 = 0.14;
const WORD_TINT: f32 = 0.34;

/// Which text a line is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// In both.
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq)]
struct Span {
    text: String,
    color: Option<SyntaxColor>,
    /// Part of what changed in the line.
    changed: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum What {
    /// A line, or the rest of one that didn't fit on the rows before. Its
    /// line numbers are on its first row only.
    Line {
        side: Side,
        first: bool,
        old: Option<usize>,
        new: Option<usize>,
        spans: Vec<Span>,
        /// The characters of the line on the row, as written, each with
        /// the column it starts at: a tab is one character, for the
        /// spaces it's drawn as.
        text: Vec<(usize, char)>,
    },
    /// Unchanged lines folded away: `hidden` of them from line `start` of
    /// the old text, which names the fold, to unfold it.
    Fold { start: usize, hidden: usize },
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    what: What,
    /// The line of the file it's at, or for lines taken out, where they
    /// were, to go to editing at.
    line: u32,
}

#[derive(Default)]
struct Layout {
    rows: Vec<Row>,
    /// Lines added and removed in all, folded or not.
    added: usize,
    removed: usize,
    /// Why there's nothing to show, if there isn't.
    note: Option<&'static str>,
    /// Columns for each line number; 0 when they're left out.
    digits: usize,
}

impl Layout {
    fn note(note: &'static str) -> Layout {
        Layout {
            note: Some(note),
            ..Layout::default()
        }
    }

    /// Columns left of the text: the line numbers, then the sign.
    fn gutter(&self) -> usize {
        match self.digits {
            0 => 2,
            digits => 2 * digits + 5,
        }
    }

    /// The first row at file line `line` or after it.
    fn row_of(&self, line: u32) -> usize {
        self.rows
            .iter()
            .position(|row| row.line >= line)
            .unwrap_or(self.rows.len())
    }
}

/// What the text was laid out from: when it changes, it's laid out again.
#[derive(Debug, Clone, PartialEq)]
struct Key {
    epoch: u64,
    /// The git record in use, by address: a new one is a new commit.
    tracked: Option<usize>,
    kind: Option<Kind>,
    width: u32,
    unfolded: usize,
}

struct Click {
    row: usize,
    time: Instant,
    count: u32,
}

/// A row, and a column of the text on it.
type Spot = (usize, usize);

pub enum DiffEvent {
    /// Go to editing, with the cursor on file line `line`, which was at
    /// `row` of the view.
    Edit { line: u32, row: u32 },
}

pub struct DiffView {
    doc: Rc<Document>,
    highlighter: RefCell<ExcerptHighlighter>,
    laid: RefCell<Option<(Key, Layout)>>,
    /// The row at the top of the view.
    top: Cell<usize>,
    /// A file line to scroll to once the text is laid out, and how many
    /// rows down the view to put it.
    pending: Cell<Option<(u32, u32)>>,
    /// The folds opened, by the old line they start at.
    unfolded: HashSet<usize>,
    width: u32,
    height: u32,
    last_click: Option<Click>,
    /// Where a selection started, and where it goes to. Laid out again,
    /// the rows are others, and it's gone.
    selection: Cell<Option<(Spot, Spot)>>,
    /// Where the left button went down, until it's released.
    press: Option<Spot>,
    dragged: bool,
}

impl DiffView {
    /// A diff of `doc` that starts with file line `top`, or the first row
    /// after it, at the top.
    pub fn new(doc: Rc<Document>, top: u32) -> DiffView {
        DiffView {
            doc,
            highlighter: RefCell::new(ExcerptHighlighter::new()),
            laid: RefCell::new(None),
            top: Cell::new(0),
            pending: Cell::new(Some((top, 0))),
            unfolded: HashSet::new(),
            width: 0,
            height: 0,
            last_click: None,
            selection: Cell::new(None),
            press: None,
            dragged: false,
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    fn key(&self, tracked: Option<&Rc<Tracked>>) -> Key {
        Key {
            epoch: self.doc.buffer.content_epoch(),
            tracked: tracked.map(|tracked| Rc::as_ptr(tracked) as usize),
            kind: tracked.and_then(|tracked| tracked.kind.get()),
            width: self.width,
            unfolded: self.unfolded.len(),
        }
    }

    /// Lays the text out again if it, the width, or the last commit
    /// changed, keeping the same file line at the top.
    fn sync(&self) {
        let tracked = self.doc.tracked();
        let key = self.key(tracked.as_ref());
        let current = matches!(&*self.laid.borrow(), Some((laid, _)) if *laid == key);
        if !current {
            let old = self.laid.borrow_mut().take();
            let keep = old.as_ref().and_then(|(_, layout)| {
                let row = layout.rows.get(self.top.get())?;
                Some((row.line, self.top.get() - layout.row_of(row.line)))
            });
            let layout = self.lay_out(tracked.as_deref());
            self.selection.set(None);
            if let Some((line, within)) = keep {
                self.top.set(layout.row_of(line) + within);
            }
            *self.laid.borrow_mut() = Some((key, layout));
        }
        if let Some((line, down)) = self.pending.take() {
            let row = self.with_layout(|layout| layout.row_of(line));
            self.top.set(row.saturating_sub(down as usize));
        }
        self.top.set(self.top.get().min(self.max_top()));
    }

    fn lay_out(&self, tracked: Option<&Tracked>) -> Layout {
        let Some(tracked) = tracked else {
            return Layout::note("Not in a git repository.");
        };
        let old = match tracked.base() {
            Base::Text(text) => text.as_str(),
            Base::Binary => return Layout::note("Binary in the last commit."),
            // Ignored, or so git says.
            Base::Missing if tracked.kind.get().is_none() => {
                return Layout::note("Not in the last commit.")
            }
            Base::Missing => "",
        };
        let path = self.doc.path().unwrap_or_else(|| PathBuf::from("untitled"));
        let mut colorer = Colorer {
            highlighter: &mut self.highlighter.borrow_mut(),
            path,
            language: self.doc.language.get(),
        };
        let width = self.width.max(1) as usize;
        let tab = config::get().tab_width.max(1) as usize;
        lay_out(
            old,
            &self.doc.buffer.text(),
            width,
            tab,
            &self.unfolded,
            &mut colorer,
        )
    }

    fn with_layout<R>(&self, f: impl FnOnce(&Layout) -> R) -> R {
        match &*self.laid.borrow() {
            Some((_, layout)) => f(layout),
            None => f(&Layout::default()),
        }
    }

    fn rows(&self) -> usize {
        self.with_layout(|layout| layout.rows.len())
    }

    fn max_top(&self) -> usize {
        self.rows().saturating_sub(self.height as usize)
    }

    /// Lays the text out again, as when the theme changed.
    pub fn invalidate(&mut self) {
        *self.laid.get_mut() = None;
    }

    /// The file line at the top of the view.
    pub fn top_line(&self) -> u32 {
        self.sync();
        self.with_layout(|layout| layout.rows.get(self.top.get()).map_or(0, |row| row.line))
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

    fn has_selection(&self) -> bool {
        self.selection.get().is_some_and(|(a, b)| a != b)
    }

    pub fn clear_selection(&mut self) {
        self.selection.set(None);
    }

    pub fn select_all(&mut self) {
        self.sync();
        self.selection.set(Some(((0, 0), (self.rows(), 0))));
    }

    /// The selection, start first.
    fn ordered_selection(&self) -> Option<(Spot, Spot)> {
        let (a, b) = self.selection.get()?;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The selected text, as written: tabs as tabs, a line that wrapped
    /// as one, and folds left out.
    pub fn selected_text(&self) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        self.sync();
        let (start, end) = self.ordered_selection()?;
        let text = self.with_layout(|layout| {
            let mut out = String::new();
            let mut any = false;
            let last = end.0.min(layout.rows.len().saturating_sub(1));
            for (index, row) in layout.rows.iter().enumerate().take(last + 1).skip(start.0) {
                let What::Line { first, text, .. } = &row.what else {
                    continue;
                };
                // The rest of a line that wrapped goes on with it.
                if any && *first {
                    out.push('\n');
                }
                any = true;
                let range = selected_columns(index, (start, end));
                let chars = text.iter().filter(|(col, _)| range.contains(col));
                out.extend(chars.map(|&(_, c)| c));
            }
            out
        });
        Some(text)
    }

    /// The row and text column at cell (`x`, `y`) of the view: the line
    /// numbers count as the text's start.
    fn spot(&self, x: u32, y: u32) -> Spot {
        let gutter = self.with_layout(Layout::gutter);
        (
            self.top.get() + y as usize,
            (x as usize).saturating_sub(gutter),
        )
    }

    /// A mouse event at cell (`mouse.x`, `mouse.y`) of the view.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> Option<DiffEvent> {
        self.sync();
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                let at = self.spot(mouse.x, mouse.y);
                let row = at.0;
                let count = match &self.last_click {
                    Some(click)
                        if click.row == row && now.duration_since(click.time) < MULTI_CLICK =>
                    {
                        click.count + 1
                    }
                    _ => 1,
                };
                self.last_click = Some(Click {
                    row,
                    time: now,
                    count,
                });
                let what = self.with_layout(|layout| {
                    layout.rows.get(row).map(|row| (row.what.clone(), row.line))
                });
                match what {
                    Some((What::Fold { start, .. }, _)) => {
                        self.unfolded.insert(start);
                        return None;
                    }
                    Some((What::Line { .. }, line)) if count == 2 => {
                        self.press = None;
                        return Some(DiffEvent::Edit { line, row: mouse.y });
                    }
                    _ => {}
                }
                match (mouse.mods.shift, self.selection.get()) {
                    (true, Some((anchor, _))) => self.selection.set(Some((anchor, at))),
                    _ => self.selection.set(None),
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
                let at = self.spot(mouse.x, mouse.y.min(self.height.saturating_sub(1)));
                self.dragged = self.dragged || at != origin;
                if self.dragged {
                    self.selection.set(Some((origin, at)));
                }
            }
            MouseKind::Release(MouseButton::Left) => self.press = None,
            MouseKind::ScrollUp => self.scroll(-WHEEL_ROWS),
            MouseKind::ScrollDown => self.scroll(WHEEL_ROWS),
            _ => {}
        }
        None
    }

    /// Draws the view at (`x`, `y`).
    pub fn draw(&self, frame: &Buffer, x: u32, y: u32) {
        self.sync();
        let colors = theme::colors();
        frame.fill_rect(x, y, self.width, self.height, colors.bg);
        let laid = self.laid.borrow();
        let Some((_, layout)) = &*laid else {
            return;
        };
        let note = layout.note.or_else(|| {
            layout
                .rows
                .is_empty()
                .then_some("No changes since the last commit.")
        });
        if let Some(note) = note {
            frame.draw_text(note, x + 1, y, colors.muted, None, Attributes::NONE);
            return;
        }
        let selection = self.ordered_selection();
        let rows = layout.rows.iter().enumerate().skip(self.top.get());
        for ((index, row), screen_y) in rows.zip(y..y + self.height) {
            let selected = selection
                .map(|selection| selected_columns(index, selection))
                .filter(|range| !range.is_empty());
            draw_row(
                frame,
                &colors,
                layout,
                row,
                selected,
                (x, screen_y, self.width),
            );
        }
    }

    pub fn status(&self) -> Status {
        self.sync();
        let top = self.top.get();
        let max_top = self.max_top();
        let position = match (top, max_top) {
            (_, 0) => "All".to_string(),
            (0, _) => "Top".to_string(),
            (top, max) if top >= max => "Bot".to_string(),
            (top, max) => format!("{}%", top * 100 / max),
        };
        let (added, removed) = self.with_layout(|layout| (layout.added, layout.removed));
        let line = self.top_line() + 1;
        Status::Info(format!("Diff  +{added} −{removed}  Ln {line}  {position}"))
    }
}

/// The text columns of row `index` within `selection`, start first.
fn selected_columns(index: usize, (start, end): (Spot, Spot)) -> Range<usize> {
    if index < start.0 || index > end.0 {
        return 0..0;
    }
    let from = if index == start.0 { start.1 } else { 0 };
    let to = if index == end.0 { end.1 } else { usize::MAX };
    from..to.max(from)
}

/// Draws `row` at (`x`, `y`), `width` wide, with the text columns
/// `selected` selected.
fn draw_row(
    frame: &Buffer,
    colors: &Colors,
    layout: &Layout,
    row: &Row,
    selected: Option<Range<usize>>,
    (x, y, width): (u32, u32, u32),
) {
    let gutter = layout.gutter() as u32;
    match &row.what {
        What::Fold { hidden, .. } => {
            frame.fill_rect(x, y, width, 1, colors.surface_inactive);
            let lines = if *hidden == 1 { "line" } else { "lines" };
            let text = format!("⋯ {hidden} unchanged {lines}");
            let text_x = x + gutter.saturating_sub(2);
            frame.draw_text(&text, text_x, y, colors.muted, None, Attributes::NONE);
        }
        What::Line {
            side,
            first,
            old,
            new,
            spans,
            ..
        } => {
            let (band, strong, sign) = match side {
                Side::Same => (colors.bg, colors.bg, ' '),
                Side::Added => tints(colors, Hue::Green, '+'),
                Side::Removed => tints(colors, Hue::Red, '-'),
            };
            frame.fill_rect(x, y, width, 1, band);
            if *first {
                let digits = layout.digits;
                if digits > 0 {
                    let number =
                        |n: Option<usize>| n.map_or(String::new(), |n| (n + 1).to_string());
                    let numbers = format!("{:>digits$} {:>digits$}", number(*old), number(*new));
                    frame.draw_text(&numbers, x + 1, y, colors.faint, None, Attributes::NONE);
                }
                let fg = match side {
                    Side::Added => colors.hue(Hue::Green),
                    Side::Removed => colors.hue(Hue::Red),
                    Side::Same => colors.faint,
                };
                let mut utf8 = [0u8; 4];
                let sign = sign.encode_utf8(&mut utf8);
                frame.draw_text(sign, x + gutter - 2, y, fg, None, Attributes::NONE);
            }
            let mut col = 0;
            for span in spans {
                let fg = span.color.and_then(SyntaxColor::fg).unwrap_or(colors.text);
                let attributes = span.color.map_or(Attributes::NONE, SyntaxColor::attributes);
                let bg = if span.changed { strong } else { band };
                let span_width = text_width(&span.text);
                let end = col + span_width;
                // Split where the selection starts and ends.
                let (from, to) = selected.as_ref().map_or((col, col), |s| {
                    (s.start.clamp(col, end), s.end.clamp(col, end))
                });
                for (n, (a, b)) in [(col, from), (from, to), (to, end)].into_iter().enumerate() {
                    if a >= b {
                        continue;
                    }
                    let bg = if n == 1 { colors.selection } else { bg };
                    let text = columns(&span.text, a - col..b - col);
                    let screen_x = x + gutter + a as u32;
                    frame.draw_text(text, screen_x, y, fg, Some(bg), attributes);
                }
                col = end;
            }
        }
    }
}

/// Behind a changed line, and behind what changed in it, in `hue`, and its
/// sign.
fn tints(colors: &Colors, hue: Hue, sign: char) -> (Rgba, Rgba, char) {
    let hue = colors.hue(hue);
    (
        theme::mix(colors.bg, hue, LINE_TINT),
        theme::mix(colors.bg, hue, WORD_TINT),
        sign,
    )
}

/// Colors lines of the two texts, as the file's language has them.
struct Colorer<'a> {
    highlighter: &'a mut ExcerptHighlighter,
    path: PathBuf,
    language: Option<&'static Language>,
}

impl Colorer<'_> {
    /// The colors of `lines` of `text`, the `old` text or the new, by line
    /// index; none for those cue can't color.
    fn colors(
        &mut self,
        old: bool,
        text: &str,
        lines: &[(usize, Range<usize>)],
    ) -> HashMap<usize, LineColors> {
        let Some(language) = self.language.filter(|_| !lines.is_empty()) else {
            return HashMap::new();
        };
        // The cache knows texts by path: the old one is another.
        let path = match old {
            true => PathBuf::from(format!("{}#HEAD", self.path.display())),
            false => self.path.clone(),
        };
        let ranges: Vec<Range<usize>> = lines.iter().map(|(_, range)| range.clone()).collect();
        let stop = AtomicBool::new(false);
        let colors = self
            .highlighter
            .highlight(&path, language, text, &ranges, &stop)
            .unwrap_or_default();
        lines.iter().map(|(index, _)| *index).zip(colors).collect()
    }
}

/// What a diff shows, in order, before it's laid out into rows.
enum Piece {
    /// Line `old` of the old text, the same as line `new` of the new.
    Same { old: usize, new: usize },
    /// The changed lines of the diff's op `op`.
    Changed(usize),
    Fold {
        start: usize,
        hidden: usize,
        line: usize,
    },
}

/// Lays out the diff of `old` and `new`, `width` columns wide, with tabs
/// `tab` wide, the folds starting at the old lines `unfolded` open.
fn lay_out(
    old: &str,
    new: &str,
    width: usize,
    tab: usize,
    unfolded: &HashSet<usize>,
    colorer: &mut Colorer,
) -> Layout {
    // Whether the last line ends with a newline isn't shown.
    let (old, new) = (with_newline(old), with_newline(new));
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(TIMEOUT)
        .diff_lines(old.as_str(), new.as_str());
    let ops = diff.ops();
    let old_lines = line_ranges(&old);
    let new_lines = line_ranges(&new);
    let mut layout = Layout::default();
    let mut pieces = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                let head = if i == 0 { 0 } else { CONTEXT };
                let tail = if i + 1 == ops.len() { 0 } else { CONTEXT };
                let same = |k: usize| Piece::Same {
                    old: old_index + k,
                    new: new_index + k,
                };
                if len <= head + tail + 1 || unfolded.contains(&old_index) {
                    pieces.extend((0..len).map(same));
                    continue;
                }
                pieces.extend((0..head).map(same));
                pieces.push(Piece::Fold {
                    start: old_index,
                    hidden: len - head - tail,
                    line: new_index + head,
                });
                pieces.extend((len - tail..len).map(same));
            }
            _ => {
                let (_, old_range, new_range) = op.as_tag_tuple();
                layout.removed += old_range.len();
                layout.added += new_range.len();
                pieces.push(Piece::Changed(i));
            }
        }
    }
    if layout.added + layout.removed == 0 {
        return layout;
    }
    // Only the lines shown are colored.
    let mut wanted_old = Vec::new();
    let mut wanted_new = Vec::new();
    for piece in &pieces {
        match piece {
            Piece::Same { new, .. } => wanted_new.push((*new, new_lines[*new].clone())),
            Piece::Changed(i) => {
                let (_, old_range, new_range) = ops[*i].as_tag_tuple();
                wanted_old.extend(old_range.map(|k| (k, old_lines[k].clone())));
                wanted_new.extend(new_range.map(|k| (k, new_lines[k].clone())));
            }
            Piece::Fold { .. } => {}
        }
    }
    let old_colors = colorer.colors(true, &old, &wanted_old);
    let new_colors = colorer.colors(false, &new, &wanted_new);
    let none = LineColors::new();
    let total = old_lines.len().max(new_lines.len());
    layout.digits = total.max(1).to_string().len();
    if width < layout.gutter() + MIN_TEXT_WIDTH {
        layout.digits = 0;
    }
    let text_width = width.saturating_sub(layout.gutter()).max(1);
    let mut options = InlineChangeOptions::new();
    options.semantic_cleanup(true);
    let deadline = Instant::now() + TIMEOUT;
    for piece in pieces {
        match piece {
            Piece::Fold {
                start,
                hidden,
                line,
            } => layout.rows.push(Row {
                what: What::Fold { start, hidden },
                line: line as u32,
            }),
            Piece::Same { old, new: index } => {
                let text = &new[new_lines[index].clone()];
                let colors = new_colors.get(&index).unwrap_or(&none);
                let cells = cells(&[(false, text)], colors, tab);
                push_line(
                    &mut layout.rows,
                    cells,
                    Side::Same,
                    (Some(old), Some(index)),
                    index,
                    text_width,
                );
            }
            Piece::Changed(i) => {
                let op = &ops[i];
                let changes =
                    diff.iter_inline_changes_with_options_deadline(op, options, Some(deadline));
                for change in changes {
                    let segments: Vec<(bool, &str)> = change
                        .values()
                        .iter()
                        .map(|&(changed, text)| (changed, text))
                        .collect();
                    let (side, colors, numbers, line) = match change.tag() {
                        ChangeTag::Delete => {
                            let index = change.old_index().unwrap_or(0);
                            let colors = old_colors.get(&index).unwrap_or(&none);
                            (
                                Side::Removed,
                                colors,
                                (Some(index), None),
                                op.new_range().start,
                            )
                        }
                        _ => {
                            let index = change.new_index().unwrap_or(0);
                            let colors = new_colors.get(&index).unwrap_or(&none);
                            (Side::Added, colors, (None, Some(index)), index)
                        }
                    };
                    let cells = cells(&segments, colors, tab);
                    push_line(&mut layout.rows, cells, side, numbers, line, text_width);
                }
            }
        }
    }
    layout
}

/// `text`, ending with a newline unless it's empty.
fn with_newline(text: &str) -> String {
    let mut text = text.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The byte range of each line of `text`, without its newline.
fn line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut start = 0;
    text.split_inclusive('\n')
        .map(|line| {
            let range = start..start + line.trim_end_matches('\n').len();
            start += line.len();
            range
        })
        .collect()
}

/// A character to draw, its color, whether it's part of what changed,
/// and the character it's of in the text, on the first of the spaces a
/// tab is drawn as.
type Glyph = (char, Option<SyntaxColor>, bool, Option<char>);

/// The characters of a line, given as `segments` that changed or didn't,
/// colored by `colors`, by byte, with tabs as spaces to the next stop.
fn cells(segments: &[(bool, &str)], colors: &LineColors, tab: usize) -> Vec<Glyph> {
    let mut cells = Vec::new();
    let mut byte = 0;
    let mut col = 0;
    for &(changed, text) in segments {
        for c in text.chars() {
            let color = colors
                .iter()
                .find(|(range, _)| range.contains(&byte))
                .map(|(_, color)| *color);
            byte += c.len_utf8();
            match c {
                '\n' | '\r' => {}
                '\t' => {
                    let spaces = tab - col % tab;
                    cells.push((' ', color, changed, Some('\t')));
                    cells.extend(std::iter::repeat_n((' ', color, changed, None), spaces - 1));
                    col += spaces;
                }
                c => {
                    cells.push((c, color, changed, Some(c)));
                    col += c.width().unwrap_or(0);
                }
            }
        }
    }
    cells
}

/// Adds the rows of a line of `cells`, wrapped at `width` columns.
fn push_line(
    rows: &mut Vec<Row>,
    cells: Vec<Glyph>,
    side: Side,
    (old, new): (Option<usize>, Option<usize>),
    line: usize,
    width: usize,
) {
    let mut chunks: Vec<Vec<Glyph>> = vec![Vec::new()];
    let mut col = 0;
    for cell in cells {
        let w = cell.0.width().unwrap_or(0);
        if col + w > width && col > 0 {
            chunks.push(Vec::new());
            col = 0;
        }
        col += w;
        chunks.last_mut().expect("a chunk").push(cell);
    }
    for (i, chunk) in chunks.into_iter().enumerate() {
        let first = i == 0;
        let mut col = 0;
        let mut text = Vec::new();
        for &(c, _, _, source) in &chunk {
            text.extend(source.map(|source| (col, source)));
            col += c.width().unwrap_or(0);
        }
        rows.push(Row {
            what: What::Line {
                side,
                first,
                old: old.filter(|_| first),
                new: new.filter(|_| first),
                spans: spans(chunk),
                text,
            },
            line: line as u32,
        });
    }
}

/// `cells` as runs of one color and change.
fn spans(cells: Vec<Glyph>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (c, color, changed, _) in cells {
        match spans.last_mut() {
            Some(span) if span.color == color && span.changed == changed => span.text.push(c),
            _ => spans.push(Span {
                text: c.to_string(),
                color,
                changed,
            }),
        }
    }
    spans
}

fn text_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(old: &str, new: &str, width: usize, unfolded: &HashSet<usize>) -> Layout {
        let mut highlighter = ExcerptHighlighter::new();
        let mut colorer = Colorer {
            highlighter: &mut highlighter,
            path: PathBuf::from("/test.txt"),
            language: None,
        };
        lay_out(old, new, width, 4, unfolded, &mut colorer)
    }

    /// Each row as a sign, the line numbers, and the text, with what
    /// changed in brackets.
    fn listing(layout: &Layout) -> Vec<String> {
        layout
            .rows
            .iter()
            .map(|row| match &row.what {
                What::Fold { hidden, .. } => format!("~ {hidden}"),
                What::Line {
                    side,
                    old,
                    new,
                    spans,
                    first,
                    ..
                } => {
                    let sign = match (side, first) {
                        (_, false) => '.',
                        (Side::Same, _) => ' ',
                        (Side::Added, _) => '+',
                        (Side::Removed, _) => '-',
                    };
                    let number =
                        |n: &Option<usize>| n.map_or("_".to_string(), |n| (n + 1).to_string());
                    let text: String = spans
                        .iter()
                        .map(|span| match span.changed {
                            true => format!("[{}]", span.text),
                            false => span.text.clone(),
                        })
                        .collect();
                    format!("{sign} {} {} {text}", number(old), number(new))
                }
            })
            .collect()
    }

    fn numbered(lines: impl Iterator<Item = usize>) -> String {
        lines.map(|n| format!("line {n}\n")).collect()
    }

    #[test]
    fn shows_changes_with_what_changed_in_them() {
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let y = 1;\n    go();\n}\n";
        let layout = layout(old, new, 80, &HashSet::new());
        assert_eq!(
            listing(&layout),
            [
                "  1 1 fn main() {",
                "- 2 _     let [x] = 1;",
                "+ _ 2     let [y] = 1;",
                "+ _ 3 [    go();]",
                "  3 4 }",
            ]
        );
        assert_eq!((layout.added, layout.removed), (2, 1));
        // A line taken out goes to editing where it was.
        assert_eq!(layout.rows[1].line, 1);
    }

    #[test]
    fn folds_unchanged_lines_away_from_changes() {
        let old = numbered(1..=20);
        let new = old.replace("line 10\n", "line ten\n");
        let folded = layout(&old, &new, 80, &HashSet::new());
        let rows = listing(&folded);
        assert_eq!(rows[0], "~ 6");
        assert_eq!(rows[1], "  7 7 line 7");
        assert_eq!(rows[rows.len() - 1], "~ 7");
        assert_eq!(rows.len(), 1 + 3 + 2 + 3 + 1);
        // Clicked open, a fold shows its lines.
        let What::Fold { start, .. } = folded.rows[0].what else {
            panic!("a fold first");
        };
        let unfolded = layout(&old, &new, 80, &HashSet::from([start]));
        assert_eq!(listing(&unfolded)[0], "  1 1 line 1");
    }

    #[test]
    fn a_fold_of_one_line_shows_it_instead() {
        let old = numbered(1..=8);
        let new = old
            .replace("line 1\n", "line one\n")
            .replace("line 8\n", "line eight\n");
        let layout = layout(&old, &new, 80, &HashSet::new());
        assert!(listing(&layout).iter().all(|row| !row.starts_with('~')));
    }

    #[test]
    fn nothing_changed_has_no_rows() {
        let layout = layout("a\nb\n", "a\nb", 80, &HashSet::new());
        assert!(layout.rows.is_empty(), "a missing last newline isn't shown");
    }

    #[test]
    fn a_new_file_is_all_added() {
        let layout = layout("", "a\nb\n", 80, &HashSet::new());
        assert_eq!(listing(&layout), ["+ _ 1 a", "+ _ 2 b"]);
    }

    #[test]
    fn long_lines_wrap_and_tabs_expand() {
        let layout = layout("", "\tabcdefghijklmnopqrstuvwxyz\n", 30, &HashSet::new());
        // 30 columns: 2 * 1 + 5 for the gutter leaves 23 for the text.
        assert_eq!(
            listing(&layout),
            ["+ _ 1     abcdefghijklmnopqrs", ". _ _ tuvwxyz"]
        );
    }
}
