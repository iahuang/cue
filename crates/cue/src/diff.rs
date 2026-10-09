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
use similar::{Algorithm, ChangeTag, DiffOp, DiffTag, InlineChangeOptions, TextDiff};
use unicode_width::UnicodeWidthChar;

use crate::config;
use crate::document::Document;
use crate::git::{Base, Kind, Tracked};
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::language::Language;
use crate::reader::columns;
use crate::scroller::{Scrolled, Scroller, Spot};
use crate::status::Status;
use crate::syntax::{ExcerptHighlighter, LineColors};
use crate::theme::{self, Colors, Hue, SyntaxColor};

/// Unchanged lines shown before and after each change.
const CONTEXT: usize = 3;
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

pub enum DiffEvent {
    /// Go to editing, with the cursor on file line `line`, which was at
    /// `row` of the view.
    Edit { line: u32, row: u32 },
}

/// What a diff is of.
enum Source {
    /// A document as it is now, against what it was in the last commit.
    Live(Rc<Document>),
    /// A file as a commit changed it: what it was before, and after.
    Fixed {
        old: Base,
        new: Base,
        path: PathBuf,
        language: Option<&'static Language>,
    },
}

pub struct DiffView {
    source: Source,
    highlighter: RefCell<ExcerptHighlighter>,
    laid: RefCell<Option<(Key, Layout)>>,
    /// Laid out again, the rows are others, and the selection is gone.
    scroller: Scroller,
    /// A file line to scroll to once the text is laid out, and how many
    /// rows down the view to put it.
    pending: Cell<Option<(u32, u32)>>,
    /// The folds opened, by the old line they start at.
    unfolded: HashSet<usize>,
    width: u32,
    height: u32,
}

impl DiffView {
    /// A diff of `doc` that starts with file line `top`, or the first row
    /// after it, at the top.
    pub fn new(doc: Rc<Document>, top: u32) -> DiffView {
        DiffView::of(Source::Live(doc), top)
    }

    /// A diff of the file at `path` from `old` to `new`, as a commit
    /// changed it, from the top.
    pub fn fixed(old: Base, new: Base, path: PathBuf) -> DiffView {
        let first_line = |base: &Base| match base {
            Base::Text(text) => text.lines().next().unwrap_or_default().to_string(),
            _ => String::new(),
        };
        let language = crate::language::detect(Some(&path), || first_line(&new));
        let source = Source::Fixed {
            old,
            new,
            path,
            language,
        };
        DiffView::of(source, 0)
    }

    fn of(source: Source, top: u32) -> DiffView {
        DiffView {
            source,
            highlighter: RefCell::new(ExcerptHighlighter::new()),
            laid: RefCell::new(None),
            scroller: Scroller::default(),
            pending: Cell::new(Some((top, 0))),
            unfolded: HashSet::new(),
            width: 0,
            height: 0,
        }
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// What git says of the document, for a diff of one.
    fn tracked(&self) -> Option<Rc<Tracked>> {
        match &self.source {
            Source::Live(doc) => doc.tracked(),
            Source::Fixed { .. } => None,
        }
    }

    fn key(&self, tracked: Option<&Rc<Tracked>>) -> Key {
        Key {
            epoch: match &self.source {
                Source::Live(doc) => doc.buffer.content_epoch(),
                Source::Fixed { .. } => 0,
            },
            tracked: tracked.map(|tracked| Rc::as_ptr(tracked) as usize),
            kind: tracked.and_then(|tracked| tracked.kind.get()),
            width: self.width,
            unfolded: self.unfolded.len(),
        }
    }

    fn lay_out(&self, tracked: Option<&Tracked>) -> Layout {
        let width = self.width.max(1) as usize;
        let tab = config::get().tab_width.max(1) as usize;
        let lay_out = |old: &str, new: &str, path, language| {
            let mut colorer = Colorer {
                highlighter: &mut self.highlighter.borrow_mut(),
                path,
                language,
            };
            lay_out(old, new, width, tab, &self.unfolded, &mut colorer)
        };
        let doc = match &self.source {
            Source::Live(doc) => doc,
            Source::Fixed {
                old,
                new,
                path,
                language,
            } => {
                return match (fixed_text(old), fixed_text(new)) {
                    (Some(old), Some(new)) => lay_out(old, new, path.clone(), *language),
                    _ => Layout::note("Binary file."),
                };
            }
        };
        let Some(tracked) = tracked else {
            return Layout::note("Not in a git repository.");
        };
        let old = match tracked.base() {
            Base::Text(text) => text.as_str(),
            Base::Binary => return Layout::note("The file was binary in the last commit."),
            // Ignored, or so git says.
            Base::Missing if tracked.kind.get().is_none() => {
                return Layout::note("File not found in the last commit.")
            }
            Base::Missing => "",
        };
        let path = doc.path().unwrap_or_else(|| PathBuf::from("untitled"));
        lay_out(old, &doc.buffer.text(), path, doc.language.get())
    }

    fn with_layout<R>(&self, f: impl FnOnce(&Layout) -> R) -> R {
        match &*self.laid.borrow() {
            Some((_, layout)) => f(layout),
            None => f(&Layout::default()),
        }
    }

    /// Lays the text out again, as when the theme changed.
    pub fn invalidate(&mut self) {
        *self.laid.get_mut() = None;
    }

    /// The file line at the top of the view.
    pub fn top_line(&self) -> u32 {
        self.sync();
        self.with_layout(|layout| {
            layout
                .rows
                .get(self.scroller.top.get())
                .map_or(0, |row| row.line)
        })
    }

    /// A mouse event at cell (`mouse.x`, `mouse.y`) of the view.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> Option<DiffEvent> {
        self.sync();
        // The line numbers count as the text's start.
        let col = (mouse.x as usize).saturating_sub(self.with_layout(Layout::gutter));
        if mouse.kind != MouseKind::Press(MouseButton::Left) {
            self.drag_or_scroll(mouse, col);
            return None;
        }
        let at = self.scroller.spot(mouse.y, col);
        let row = at.0;
        let count = self.scroller.count_click((row, 0), now);
        let what =
            self.with_layout(|layout| layout.rows.get(row).map(|row| (row.what.clone(), row.line)));
        match what {
            Some((What::Fold { start, .. }, _)) => {
                self.unfolded.insert(start);
                return None;
            }
            Some((What::Line { .. }, line))
                if count == 2 && matches!(self.source, Source::Live(_)) =>
            {
                return Some(DiffEvent::Edit { line, row: mouse.y });
            }
            _ => {}
        }
        self.scroller.press(at, mouse.mods.shift);
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
            layout.rows.is_empty().then_some(match self.source {
                Source::Live(_) => "No changes since the last commit.",
                Source::Fixed { .. } => "No text changes.",
            })
        });
        if let Some(note) = note {
            frame.draw_text(note, x + 1, y, colors.muted, None, Attributes::NONE);
            return;
        }
        let selection = self.scroller.ordered_selection();
        let rows = layout.rows.iter().enumerate().skip(self.scroller.top.get());
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
        let position = self.position();
        let (added, removed) = self.with_layout(|layout| (layout.added, layout.removed));
        let line = self.top_line() + 1;
        Status::Info(format!("Diff  +{added} −{removed}  Ln {line}  {position}"))
    }
}

impl Scrolled for DiffView {
    fn scroller(&self) -> &Scroller {
        &self.scroller
    }

    fn scroller_mut(&mut self) -> &mut Scroller {
        &mut self.scroller
    }

    /// Lays the text out again if it, the width, or the last commit
    /// changed, keeping the same file line at the top.
    fn sync(&self) {
        let tracked = self.tracked();
        let key = self.key(tracked.as_ref());
        let current = matches!(&*self.laid.borrow(), Some((laid, _)) if *laid == key);
        if !current {
            let old = self.laid.borrow_mut().take();
            let keep = old.as_ref().and_then(|(_, layout)| {
                let row = layout.rows.get(self.scroller.top.get())?;
                Some((row.line, self.scroller.top.get() - layout.row_of(row.line)))
            });
            let layout = self.lay_out(tracked.as_deref());
            self.scroller.selection.set(None);
            if let Some((line, within)) = keep {
                self.scroller.top.set(layout.row_of(line) + within);
            }
            *self.laid.borrow_mut() = Some((key, layout));
        }
        if let Some((line, down)) = self.pending.take() {
            let row = self.with_layout(|layout| layout.row_of(line));
            self.scroller.top.set(row.saturating_sub(down as usize));
        }
        self.scroller
            .top
            .set(self.scroller.top.get().min(self.max_top()));
    }

    fn rows(&self) -> usize {
        self.with_layout(|layout| layout.rows.len())
    }

    fn height(&self) -> u32 {
        self.height
    }

    /// The selected text, as written: tabs as tabs, a line that wrapped
    /// as one, and folds left out.
    fn selected_text(&self) -> Option<String> {
        if !self.has_selection() {
            return None;
        }
        self.sync();
        let (start, end) = self.scroller.ordered_selection()?;
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
}

/// How a stretch of lines changed since the last commit, as the editor's
/// gutter marks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Added,
    Changed,
    /// Lines taken out from between two others.
    Removed,
}

impl Mark {
    /// As the tree colors files changed the same way.
    pub fn hue(self) -> Hue {
        match self {
            Mark::Added => Kind::Added.hue(),
            Mark::Changed => Kind::Modified.hue(),
            Mark::Removed => Kind::Deleted.hue(),
        }
    }
}

/// A stretch of a file that changed since the last commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub mark: Mark,
    /// The lines of the file it is. Lines taken out are none, at the line
    /// that's after where they were.
    pub lines: Range<u32>,
}

/// How a line of the file is marked in the editor's gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineMark {
    /// It's in a hunk added or changed.
    In(Mark),
    /// Lines were taken out right after it.
    RemovedBelow,
    /// Lines were taken out right before it, the first line.
    RemovedAbove,
}

/// How line `line` is marked, among `hunks`, in order. Being in a hunk
/// counts before lines taken out next to it.
pub fn line_mark(hunks: &[Hunk], line: u32) -> Option<LineMark> {
    let after = hunks.partition_point(|hunk| hunk.lines.start <= line);
    if let Some(hunk) = after.checked_sub(1).map(|i| &hunks[i]) {
        if hunk.lines.contains(&line) {
            return Some(LineMark::In(hunk.mark));
        }
    }
    let removed_at = |hunk: &Hunk, at: u32| hunk.mark == Mark::Removed && hunk.lines.start == at;
    if hunks
        .get(after)
        .is_some_and(|hunk| removed_at(hunk, line + 1))
    {
        return Some(LineMark::RemovedBelow);
    }
    if line == 0 && hunks.first().is_some_and(|hunk| removed_at(hunk, 0)) {
        return Some(LineMark::RemovedAbove);
    }
    None
}

/// What [`Hunks`] were found from: when it changes, they're found again.
struct HunksKey {
    epoch: u64,
    /// Held, not just compared by address, so a new record can't take the
    /// old one's place in memory and pass for it.
    tracked: Option<Rc<Tracked>>,
    kind: Option<Kind>,
    modified: bool,
}

impl PartialEq for HunksKey {
    fn eq(&self, other: &HunksKey) -> bool {
        let same_tracked = match (&self.tracked, &other.tracked) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        same_tracked
            && (self.epoch, self.kind, self.modified) == (other.epoch, other.kind, other.modified)
    }
}

/// A document's changes since the last commit, staged or not, with those
/// not yet saved, for the editor's gutter. They're found again only when
/// the text or what git says of it changes.
#[derive(Default)]
pub struct Hunks {
    found: RefCell<Option<(HunksKey, Rc<[Hunk]>)>>,
}

impl Hunks {
    /// `doc`'s hunks, in order: none if it isn't in a repository, git
    /// ignores it, or it was binary in the last commit.
    pub fn of(&self, doc: &Document) -> Rc<[Hunk]> {
        let tracked = doc.tracked();
        let key = HunksKey {
            epoch: doc.buffer.content_epoch(),
            kind: tracked.as_ref().and_then(|tracked| tracked.kind.get()),
            tracked,
            modified: doc.is_modified(),
        };
        if let Some((found, hunks)) = &*self.found.borrow() {
            if *found == key {
                return hunks.clone();
            }
        }
        let hunks: Rc<[Hunk]> = key
            .tracked
            .as_deref()
            .and_then(|tracked| hunk_base(tracked, key.modified))
            .map_or_else(Vec::new, |old| hunks(old, &doc.buffer.text()))
            .into();
        *self.found.borrow_mut() = Some((key, hunks.clone()));
        hunks
    }
}

/// What `tracked`'s file was in the last commit, to find hunks against.
/// git isn't asked for it while git and the editor both say the file's
/// the same as it was.
fn hunk_base(tracked: &Tracked, modified: bool) -> Option<&str> {
    let kind = tracked.kind.get();
    if kind.is_none() && !modified && tracked.base_if_read().is_none() {
        return None;
    }
    match tracked.base() {
        Base::Text(text) => Some(text),
        Base::Binary => None,
        // Ignored, or so git says.
        Base::Missing if kind.is_none() => None,
        Base::Missing => Some(""),
    }
}

/// The stretches of `new` that differ from `old`, by line, as the diff
/// view shows them.
fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let (old, new) = (with_newline(old), with_newline(new));
    // They're found as the text's typed in, and an edit leaves most of a
    // file as it was: only the lines between the first and last that
    // differ are diffed, which keeps a long file quick to type in.
    let (head, tail) = same_ends(&old, &new);
    let skipped = old[..head].matches('\n').count() as u32;
    let diff = diff_lines(&old[head..old.len() - tail], &new[head..new.len() - tail]);
    diff.ops()
        .iter()
        .filter_map(|op| {
            let (tag, _, lines) = op.as_tag_tuple();
            let lines = skipped + lines.start as u32..skipped + lines.end as u32;
            let mark = match tag {
                DiffTag::Equal => return None,
                DiffTag::Insert => Mark::Added,
                DiffTag::Delete => Mark::Removed,
                DiffTag::Replace => Mark::Changed,
            };
            Some(Hunk { mark, lines })
        })
        .collect()
}

/// How many bytes of whole lines `old` and `new` start with that are the
/// same, and how many they end with after those, each line ending with a
/// newline.
fn same_ends(old: &str, new: &str) -> (usize, usize) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let same = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let head = a[..same]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |i| i + 1);
    let (a, b) = (&a[head..], &b[head..]);
    let mut tail = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    // The lines in common start after a newline in both, or at the start.
    let line_start = |text: &[u8], tail: usize| {
        let at = text.len() - tail;
        at == 0 || text[at - 1] == b'\n'
    };
    while tail > 0 && !(line_start(a, tail) && line_start(b, tail)) {
        let rest = &a[a.len() - tail..];
        tail -= rest
            .iter()
            .position(|&c| c == b'\n')
            .map_or(tail, |i| i + 1);
    }
    (head, tail)
}

/// A side of a commit's diff as text: none if it was binary, and empty if
/// the file wasn't there.
fn fixed_text(base: &Base) -> Option<&str> {
    match base {
        Base::Text(text) => Some(text),
        Base::Missing => Some(""),
        Base::Binary => None,
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
    let diff = diff_lines(&old, &new);
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

/// The diff of `old` and `new` by line, a line ending only at a newline,
/// as the editor and `line_ranges` see it: `similar`'s own `diff_lines`
/// also ends one at a lone carriage return.
fn diff_lines<'a>(old: &'a str, new: &'a str) -> TextDiff<'a, 'a, str> {
    let old: Vec<&str> = old.split_inclusive('\n').collect();
    let new: Vec<&str> = new.split_inclusive('\n').collect();
    TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(TIMEOUT)
        .diff_slices(&old, &new)
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
    fn a_lone_carriage_return_doesnt_end_a_line() {
        let old = "a\rb\nc\n";
        let new = "a\rb\nc\nd\re\n";
        let layout = layout(old, new, 80, &HashSet::new());
        assert_eq!(listing(&layout), ["  1 1 ab", "  2 2 c", "+ _ 3 de"]);
        let added = Hunk {
            mark: Mark::Added,
            lines: 2..3,
        };
        assert_eq!(hunks(old, new), [added]);
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

    #[test]
    fn hunks_say_which_lines_were_added_changed_and_taken_out() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "new\na\nB\nc\ne\n";
        let hunk = |mark, lines: Range<u32>| Hunk { mark, lines };
        let found = hunks(old, new);
        assert_eq!(
            found,
            [
                hunk(Mark::Added, 0..1),
                hunk(Mark::Changed, 2..3),
                hunk(Mark::Removed, 4..4),
            ]
        );
        let marks: Vec<_> = (0..5).map(|line| line_mark(&found, line)).collect();
        assert_eq!(
            marks,
            [
                Some(LineMark::In(Mark::Added)),
                None,
                Some(LineMark::In(Mark::Changed)),
                Some(LineMark::RemovedBelow),
                None,
            ]
        );
    }

    #[test]
    fn lines_taken_out_first_or_last_are_marked_at_the_edges() {
        let found = hunks("a\nb\nc\n", "b\nc\n");
        assert_eq!(line_mark(&found, 0), Some(LineMark::RemovedAbove));
        let found = hunks("a\nb\n", "a\n");
        assert_eq!(line_mark(&found, 0), Some(LineMark::RemovedBelow));
        let found = hunks("a\nb\n", "");
        assert_eq!(line_mark(&found, 0), Some(LineMark::RemovedAbove));
        // Changed lines with lines taken out after them show as changed.
        let found = hunks("a\nb\nc\n", "A\nc\n");
        assert_eq!(line_mark(&found, 0), Some(LineMark::In(Mark::Changed)));
        assert_eq!(hunks("a\nb\n", "a\nb"), []);
    }

    #[test]
    fn only_lines_between_the_first_and_last_that_differ_are_diffed() {
        assert_eq!(same_ends("a\nb\nc\n", "a\nB\nc\n"), (2, 2));
        // Lines that end the same but start differently aren't in common.
        assert_eq!(same_ends("a\nxb\nc\n", "a\nyb\nc\n"), (2, 2));
        assert_eq!(same_ends("ab\n", "b\n"), (0, 0));
        assert_eq!(same_ends("a\n", "a\n"), (2, 0));
        assert_eq!(same_ends("a\n", "b\na\n"), (0, 2));
        assert_eq!(same_ends("é\nb\n", "ê\nb\n"), (0, 2));
        // Lines taken out deep in a long file are found where they were.
        let old: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 50\nline 51\n", "");
        let removed = Hunk {
            mark: Mark::Removed,
            lines: 50..50,
        };
        assert_eq!(hunks(&old, &new), [removed]);
    }
}
