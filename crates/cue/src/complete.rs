//! Completion as you type, without a language server: a popup under the
//! cursor of words that may finish the one being typed.
//!
//! The words are those of the file, then of the other open files in its
//! language, the language's keywords and builtins, and the names the
//! workspace's files define, as tags queries find them (see
//! [`crate::symbols`]). In LaTeX, a word may start with `\`, as commands
//! do. They're matched fuzzily, as in the picker, but must start where the
//! typed word does, at a word start of theirs: `gw` finds `get_width` and
//! `getWidth`, not `hogwash`. Words beginning as typed, near the cursor, or
//! completed recently rank higher.
//!
//! The popup opens once a word is two characters long, in code rather than
//! a comment or string, or at any time with Ctrl+Space. Tab takes the word
//! selected, Up and Down move through them, and Esc closes it until the
//! next word; Enter breaks the line as usual.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::Path;
use std::rc::{Rc, Weak};

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use opentui::{Attributes, Buffer};

use crate::definition;
use crate::document::Document;
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::language::{self, Language};
use crate::picker::Area;
use crate::symbols::{self, SymbolIndex};
use crate::theme;

/// The characters long a word is when the popup opens by itself.
pub const MIN_TYPED: usize = 2;
/// Rows of words shown at once.
const ROWS: usize = 8;
/// Words ranked and shown, at most, best first.
const MAX_SHOWN: usize = 200;
/// Files longer than this give no words, as reading them through for each
/// popup would take too long.
pub const MAX_LINES: u32 = 50_000;
/// Words completed, most recent first, to rank first next time.
const MAX_RECENT: usize = 64;
/// The widest the popup gets.
const MAX_WIDTH: u32 = 64;

/// Whether `c` can be part of a word to complete.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The word being typed: the word characters just before the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    /// The cursor's line.
    pub row: u32,
    /// The byte of the line the word starts at.
    pub start: usize,
    pub word: String,
    /// Whether it's in code, rather than a comment or string.
    pub in_code: bool,
    /// Whether a word character follows the cursor: typing in the middle
    /// of a word.
    pub word_after: bool,
}

/// A word that may finish the one typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// What the workspace says it is, as a [`symbols::Symbol`]'s kind;
    /// `keyword`; or empty, for words of open files only.
    pub kind: &'static str,
    /// Where it's defined: what it's in, or the file's name.
    pub detail: String,
    /// For a word of the file being typed in, the fewest lines it is from
    /// the cursor.
    pub near: Option<u32>,
}

impl Entry {
    fn word(name: String, near: Option<u32>) -> Entry {
        Entry {
            name,
            kind: "",
            detail: String::new(),
            near,
        }
    }
}

/// The words of `text`, but for those overlapping `skip` (the one being
/// typed), with how near each is to line `line`, if given. A word right
/// after `prefix` starts with it (see [`Language::command_prefix`]).
pub fn words(
    text: &str,
    skip: Range<usize>,
    line: Option<u32>,
    prefix: Option<char>,
) -> Vec<Entry> {
    let mut found: HashMap<&str, Option<u32>> = HashMap::new();
    let mut row = 0u32;
    let mut start = None;
    let mut add = |mut at: usize, end: usize, row: u32| {
        if let Some(prefix) = prefix.filter(|&p| text[..at].ends_with(p)) {
            at -= prefix.len_utf8();
        }
        let word = &text[at..end];
        let overlaps = at < skip.end && skip.start < end;
        let name = word.trim_start_matches(|c: char| Some(c) == prefix);
        if overlaps || name.chars().count() < 2 || name.starts_with(|c: char| c.is_numeric()) {
            return;
        }
        let near = line.map(|line| line.abs_diff(row));
        let seen = found.entry(word).or_insert(near);
        if let (Some(old), Some(new)) = (*seen, near) {
            *seen = Some(old.min(new));
        }
    };
    for (i, c) in text.char_indices() {
        match (is_word_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                add(from, i, row);
                start = None;
            }
            _ => {}
        }
        if c == '\n' {
            row += 1;
        }
    }
    if let Some(from) = start {
        add(from, text.len(), row);
    }
    found
        .into_iter()
        .map(|(word, near)| Entry::word(word.to_string(), near))
        .collect()
}

thread_local! {
    static KEYWORDS: std::cell::RefCell<HashMap<&'static str, Rc<Vec<String>>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// `language`'s keywords: the words its grammar spells out, such as `fn`
/// and `match`, rather than names it matches by pattern; and its
/// [builtins](Language::builtins).
pub fn keywords(language: &'static Language) -> Rc<Vec<String>> {
    KEYWORDS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let found = cache.entry(language.name).or_insert_with(|| {
            let Some(syntax) = &language.syntax else {
                return Rc::default();
            };
            let grammar = (syntax.grammar)();
            let mut words: Vec<String> = (0..grammar.node_kind_count() as u16)
                .filter(|&id| grammar.node_kind_is_visible(id) && !grammar.node_kind_is_named(id))
                .filter_map(|id| grammar.node_kind_for_id(id))
                .filter(|kind| {
                    kind.chars().count() >= 2
                        && kind.starts_with(|c: char| c.is_alphabetic())
                        && kind.chars().all(is_word_char)
                })
                .map(str::to_string)
                .chain(language.builtins().map(str::to_string))
                .collect();
            words.sort();
            words.dedup();
            Rc::new(words)
        });
        Rc::clone(found)
    })
}

/// The names the workspace's files in one family of languages define, each
/// once.
#[derive(Debug, Default)]
pub struct Vocabulary {
    entries: Vec<Entry>,
    by_name: HashMap<String, usize>,
}

impl Vocabulary {
    /// The names `symbols` found in the files of `family` (see
    /// [`definition::family`]).
    fn of(symbols: &SymbolIndex, family: &str) -> Vocabulary {
        let mut files: Vec<_> = symbols
            .files()
            .filter(|(path, _)| {
                language::detect(Some(path.as_path()), String::new)
                    .is_some_and(|language| definition::family(language.name) == family)
            })
            .collect();
        // The same file defines a name each time, whichever does it first.
        files.sort_by(|a, b| a.0.cmp(b.0));
        let mut vocabulary = Vocabulary::default();
        for (path, defined) in files {
            for symbol in defined {
                if symbol.kind == "impl" || vocabulary.by_name.contains_key(&symbol.name) {
                    continue;
                }
                let detail = match symbol.container.rsplit('.').next() {
                    Some(container) if !container.is_empty() => container.to_string(),
                    _ => file_name(path),
                };
                vocabulary
                    .by_name
                    .insert(symbol.name.clone(), vocabulary.entries.len());
                vocabulary.entries.push(Entry {
                    name: symbol.name.clone(),
                    kind: symbol.kind,
                    detail,
                    near: None,
                });
            }
        }
        vocabulary
    }

    #[cfg(test)]
    fn from(entries: Vec<Entry>) -> Vocabulary {
        let by_name = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.name.clone(), i))
            .collect();
        Vocabulary { entries, by_name }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// What's kept from one popup to the next: the workspace's names in the
/// language last completed in, other files' words, and the words
/// completed recently.
pub struct Completer {
    vocabulary: Option<(String, u64, Rc<Vocabulary>)>,
    /// Each open file's words, by document, as of a content epoch.
    file_words: HashMap<*const Document, (u64, Rc<Vec<Entry>>)>,
    recent: VecDeque<String>,
    matcher: Matcher,
    /// Where typed characters are in a word, kept to be used again.
    placed: Vec<u32>,
}

impl Completer {
    pub fn new() -> Completer {
        Completer {
            vocabulary: None,
            file_words: HashMap::new(),
            recent: VecDeque::new(),
            matcher: Matcher::new(Config::DEFAULT),
            placed: Vec::new(),
        }
    }

    /// The names the workspace's files of `family` define, as of the last
    /// indexing to complete.
    pub fn vocabulary(&mut self, symbols: &SymbolIndex, family: &str) -> Rc<Vocabulary> {
        let generation = symbols.generation();
        match &self.vocabulary {
            Some((seen, at, vocabulary)) if seen == family && *at == generation => {
                Rc::clone(vocabulary)
            }
            _ => {
                let vocabulary = Rc::new(Vocabulary::of(symbols, family));
                self.vocabulary = Some((family.to_string(), generation, Rc::clone(&vocabulary)));
                vocabulary
            }
        }
    }

    /// The words of `doc`, another open file than the one typed in, read
    /// again only once it changed. Forgets those of files that closed,
    /// with `open` the open ones.
    pub fn words_of(&mut self, doc: &Rc<Document>, open: &[Rc<Document>]) -> Rc<Vec<Entry>> {
        self.file_words
            .retain(|&key, _| open.iter().any(|doc| Rc::as_ptr(doc) == key));
        let epoch = doc.buffer.content_epoch();
        let key = Rc::as_ptr(doc);
        if let Some((seen, words)) = self.file_words.get(&key) {
            if *seen == epoch {
                return Rc::clone(words);
            }
        }
        let found = match doc.buffer.line_count() <= MAX_LINES {
            true => {
                let prefix = doc.language.get().and_then(Language::command_prefix);
                Rc::new(words(&doc.text(), 0..0, None, prefix))
            }
            false => Rc::default(),
        };
        self.file_words.insert(key, (epoch, Rc::clone(&found)));
        found
    }

    /// Notes `name` was completed, to rank it first next time.
    pub fn completed(&mut self, name: &str) {
        self.recent.retain(|recent| recent != name);
        self.recent.push_front(name.to_string());
        self.recent.truncate(MAX_RECENT);
    }
}

/// Which word of a [`Completion`]'s: one of its own, or of the workspace's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Local(usize),
    Workspace(usize),
}

/// A word that matches, as ranked.
#[derive(Debug, Clone)]
struct Matched {
    source: Source,
    score: i64,
    /// The characters of it that match, for the words shown.
    chars: Vec<u32>,
}

/// What the app should do after the popup handled a mouse event.
#[derive(Debug, PartialEq, Eq)]
pub enum MouseAction {
    Continue,
    /// Complete with the word clicked.
    Accept,
    /// Close it, and handle the event as if it hadn't been open.
    CloseAndPass,
    /// Not its event: handle it as if it weren't open, but keep it open.
    Pass,
}

/// The popup, while open: the words that may finish the word typed in one
/// place of a file.
pub struct Completion {
    /// The document typed in.
    pub doc: Weak<Document>,
    /// The line, and byte of the line, the word typed starts at.
    pub row: u32,
    pub start: usize,
    /// Opened with Ctrl+Space, so it stays open with nothing typed.
    pub asked: bool,
    /// Closed with Esc: it stays out of sight until the cursor leaves the
    /// word, rather than popping up again as it's typed on.
    dismissed: bool,
    typed: String,
    local: Vec<Entry>,
    workspace: Rc<Vocabulary>,
    /// The workspace's words that are also local ones, which those stand
    /// for.
    shadowed: HashSet<usize>,
    /// Those that match what's typed, best first.
    matched: Vec<Matched>,
    selected: usize,
    /// The first shown.
    top: usize,
    /// Where it was last drawn.
    area: std::cell::Cell<Option<Area>>,
}

impl Completion {
    /// The popup for `typed`, in `doc`, of `local` words and those of
    /// `workspace`. Words in both are listed once, as the workspace has
    /// them, but as near as the local ones are.
    pub fn new(
        doc: Weak<Document>,
        typed: &Typed,
        local: Vec<Entry>,
        workspace: Rc<Vocabulary>,
        asked: bool,
    ) -> Completion {
        let mut merged: HashMap<String, Entry> = HashMap::new();
        for entry in local {
            match merged.get_mut(&entry.name) {
                Some(seen) => {
                    seen.near = match (seen.near, entry.near) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    if seen.kind.is_empty() {
                        seen.kind = entry.kind;
                    }
                }
                None => {
                    merged.insert(entry.name.clone(), entry);
                }
            }
        }
        let mut shadowed = HashSet::new();
        for entry in merged.values_mut() {
            if let Some(&i) = workspace.by_name.get(&entry.name) {
                shadowed.insert(i);
                let defined = &workspace.entries[i];
                entry.kind = defined.kind;
                entry.detail = defined.detail.clone();
            }
        }
        let mut local: Vec<Entry> = merged.into_values().collect();
        local.sort_by(|a, b| a.name.cmp(&b.name));
        Completion {
            doc,
            row: typed.row,
            start: typed.start,
            asked,
            dismissed: false,
            typed: String::new(),
            local,
            workspace,
            shadowed,
            matched: Vec::new(),
            selected: 0,
            top: 0,
            area: std::cell::Cell::new(None),
        }
    }

    /// The word typed, as last matched.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Whether any word matches, so the popup shows.
    pub fn is_shown(&self) -> bool {
        !self.dismissed && !self.matched.is_empty()
    }

    /// Whether it's out of sight for the rest of the word.
    pub fn is_dismissed(&self) -> bool {
        self.dismissed
    }

    /// Puts it out of sight for the rest of the word.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.matched.clear();
    }

    /// Matches the words to `typed`, the word as typed now. Typing on, only
    /// those that matched before can still match.
    pub fn filter(&mut self, typed: &str, completer: &mut Completer) {
        if self.dismissed {
            self.typed = typed.to_string();
            return;
        }
        let narrowing = !self.typed.is_empty() && typed.starts_with(self.typed.as_str());
        let sources: Vec<Source> = match narrowing {
            true => self.matched.iter().map(|m| m.source).collect(),
            false => (0..self.local.len())
                .map(Source::Local)
                .chain(
                    (0..self.workspace.entries.len())
                        .filter(|i| !self.shadowed.contains(i))
                        .map(Source::Workspace),
                )
                .collect(),
        };
        self.typed = typed.to_string();
        let pattern = Pattern::new(
            typed,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buf = Vec::new();
        let mut matched: Vec<Matched> = sources
            .into_iter()
            .filter_map(|source| {
                let entry = self.entry(source);
                let score = score(&pattern, typed, entry, completer, &mut buf)?;
                Some(Matched {
                    source,
                    score,
                    chars: Vec::new(),
                })
            })
            .collect();
        matched.sort_by(|a, b| {
            let (x, y) = (self.entry(a.source), self.entry(b.source));
            b.score
                .cmp(&a.score)
                .then(x.name.len().cmp(&y.name.len()))
                .then_with(|| x.name.cmp(&y.name))
        });
        for m in matched.iter_mut().take(MAX_SHOWN) {
            m.chars = abbreviates(&self.entry(m.source).name, typed).unwrap_or_default();
        }
        self.matched = matched;
        self.selected = 0;
        self.top = 0;
    }

    fn entry(&self, source: Source) -> &Entry {
        match source {
            Source::Local(i) => &self.local[i],
            Source::Workspace(i) => &self.workspace.entries[i],
        }
    }

    /// The word selected, if any matches.
    pub fn selected(&self) -> Option<&str> {
        let m = self.matched.get(self.selected)?;
        Some(&self.entry(m.source).name)
    }

    /// The words shown, best first, for tests.
    #[cfg(test)]
    fn names(&self) -> Vec<&str> {
        self.matched
            .iter()
            .take(MAX_SHOWN)
            .map(|m| self.entry(m.source).name.as_str())
            .collect()
    }

    /// Moves the selection `step` words, around the ends.
    pub fn step(&mut self, step: isize) {
        let count = self.matched.len().min(MAX_SHOWN) as isize;
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + step).rem_euclid(count) as usize;
        self.keep_selected_shown();
    }

    /// Moves the selection a page of words, stopping at the ends.
    pub fn page(&mut self, down: bool) {
        let last = self.matched.len().min(MAX_SHOWN).saturating_sub(1);
        self.selected = match down {
            true => (self.selected + ROWS).min(last),
            false => self.selected.saturating_sub(ROWS),
        };
        self.keep_selected_shown();
    }

    fn keep_selected_shown(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + ROWS {
            self.top = self.selected + 1 - ROWS;
        }
    }

    fn scroll(&mut self, rows: isize) {
        let count = self.matched.len().min(MAX_SHOWN);
        let max = count.saturating_sub(ROWS) as isize;
        self.top = (self.top as isize + rows).clamp(0, max) as usize;
    }

    pub fn handle_mouse(&mut self, mouse: Mouse) -> MouseAction {
        let Some(area) = self.area.get() else {
            return MouseAction::Pass;
        };
        let inside = area.contains(mouse.x, mouse.y);
        match mouse.kind {
            MouseKind::ScrollUp if inside => self.scroll(-1),
            MouseKind::ScrollDown if inside => self.scroll(1),
            MouseKind::Press(MouseButton::Left) if inside => {
                let row = mouse.y as usize;
                let first = area.y as usize + 1;
                let index = self.top + row.saturating_sub(first);
                if row >= first && index < (self.top + ROWS).min(self.matched.len()) {
                    self.selected = index;
                    return MouseAction::Accept;
                }
            }
            MouseKind::Press(_) | MouseKind::ScrollUp | MouseKind::ScrollDown if !inside => {
                return MouseAction::CloseAndPass
            }
            _ if !inside => return MouseAction::Pass,
            _ => {}
        }
        MouseAction::Continue
    }

    // --- drawing ----------------------------------------------------------------

    /// Notes it isn't on screen, as when the cursor scrolled out of view.
    pub fn hide(&self) {
        self.area.set(None);
    }

    /// Draws the popup with its words' first characters under `(x, y)`,
    /// the cell the word typed starts at, below it or, short of room,
    /// above, on a screen `width` x `height` (the status bar not counted).
    pub fn draw(&self, frame: &Buffer, (x, y): (u32, u32), width: u32, height: u32) {
        let shown: Vec<&Matched> = self.matched.iter().skip(self.top).take(ROWS).collect();
        if shown.is_empty() || width < 8 {
            self.area.set(None);
            return;
        }
        // The widest of all that can be shown, so the popup doesn't change
        // size as it scrolls.
        let (names, details) = self
            .matched
            .iter()
            .take(MAX_SHOWN)
            .map(|m| {
                let entry = self.entry(m.source);
                (entry.name.chars().count(), detail_width(entry))
            })
            .fold((0, 0), |(n, d), (name, detail)| {
                (n.max(name), d.max(detail))
            });
        let gap = if details > 0 { 2 } else { 0 };
        let popup_width = ((names + gap + details) as u32 + 4)
            .min(MAX_WIDTH)
            .min(width);
        let popup_height = shown.len() as u32 + 2;
        if height < popup_height {
            self.area.set(None);
            return;
        }
        // The border and a space before the words.
        let left = x.saturating_sub(2).min(width - popup_width);
        let top = if y + 1 + popup_height <= height {
            y + 1
        } else if y >= popup_height {
            y - popup_height
        } else {
            height - popup_height
        };
        let area = Area {
            x: left,
            y: top,
            width: popup_width,
            height: popup_height,
        };
        self.area.set(Some(area));
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            self.draw_words(frame, area, &shown)
        });
    }

    fn draw_words(&self, frame: &Buffer, area: Area, shown: &[&Matched]) {
        let colors = theme::colors();
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        frame.fill_rect(x, y, width, height, colors.bg);
        let rule = "─".repeat(width.saturating_sub(2) as usize);
        let text = |s: &str, x: u32, y: u32, fg, attributes| {
            frame.draw_text(s, x, y, fg, None, attributes)
        };
        text(&format!("╭{rule}╮"), x, y, colors.border, Attributes::NONE);
        text(
            &format!("╰{rule}╯"),
            x,
            y + height - 1,
            colors.border,
            Attributes::NONE,
        );
        for (i, m) in shown.iter().enumerate() {
            let row = y + 1 + i as u32;
            text("│", x, row, colors.border, Attributes::NONE);
            text("│", x + width - 1, row, colors.border, Attributes::NONE);
            if self.top + i == self.selected {
                frame.fill_rect(x + 1, row, width - 2, 1, colors.selected);
            }
            let entry = self.entry(m.source);
            let fg = symbols::color(entry.kind)
                .and_then(theme::SyntaxColor::fg)
                .unwrap_or(colors.text);
            for (j, c) in entry.name.chars().enumerate() {
                let hit = m.chars.binary_search(&(j as u32)).is_ok();
                let attributes = if hit {
                    Attributes::BOLD
                } else {
                    Attributes::NONE
                };
                let fg = if hit { colors.accent } else { fg };
                let mut utf8 = [0u8; 4];
                text(
                    c.encode_utf8(&mut utf8),
                    x + 2 + j as u32,
                    row,
                    fg,
                    attributes,
                );
            }
            let detail = detail_of(entry);
            let detail_x = (x + width).saturating_sub(detail.chars().count() as u32 + 2);
            let name_end = x + 2 + entry.name.chars().count() as u32;
            if detail_x > name_end {
                text(&detail, detail_x, row, colors.muted, Attributes::NONE);
            }
        }
    }
}

/// What's shown beside a word: its kind and where it's defined.
fn detail_of(entry: &Entry) -> String {
    match (entry.kind, entry.detail.as_str()) {
        ("", _) => String::new(),
        (kind, "") => kind.to_string(),
        (kind, detail) => format!("{kind} {detail}"),
    }
}

fn detail_width(entry: &Entry) -> usize {
    detail_of(entry).chars().count()
}

/// How well `entry` finishes `typed`, which `pattern` matches fuzzily, if
/// it does: it must abbreviate it (see [`abbreviates`]), and not be `typed`
/// itself.
fn score(
    pattern: &Pattern,
    typed: &str,
    entry: &Entry,
    completer: &mut Completer,
    buf: &mut Vec<char>,
) -> Option<i64> {
    // Starting as typed counts most, then being near, then completed
    // recently.
    const PREFIX: i64 = 200;
    const NEAR: i64 = 100;
    const RECENT: i64 = 150;
    let name = entry.name.as_str();
    if name == typed {
        return None;
    }
    let mut score = 0;
    if !typed.is_empty() {
        if !abbreviates_into(name, typed, &mut completer.placed) {
            return None;
        }
        score += pattern.score(Utf32Str::new(name, buf), &mut completer.matcher)? as i64;
        if name.starts_with(typed) {
            score += PREFIX;
        } else if name.to_lowercase().starts_with(&typed.to_lowercase()) {
            score += PREFIX / 2;
        }
    }
    if let Some(near) = entry.near {
        score += NEAR / 2 + (NEAR / 2 - (near as i64).min(NEAR / 2));
    }
    if let Some(i) = completer.recent.iter().position(|recent| recent == name) {
        score += RECENT - 2 * i as i64;
    }
    if entry.kind == "keyword" {
        score -= 1;
    }
    Some(score)
}

/// Where `typed`'s characters are in `name`, as indices of its
/// characters, if each is at the start of one of its snake_case or
/// camelCase words, or right after the one before, regardless of case:
/// `gw` and `getw` are in `get_width`, and `aocm` in `area_of_circle_made`,
/// but `gw` isn't in `hogwash`.
fn abbreviates(name: &str, typed: &str) -> Option<Vec<u32>> {
    let mut at = Vec::new();
    abbreviates_into(name, typed, &mut at).then_some(at)
}

/// As [`abbreviates`], into `at`, which is kept to be used again: whether
/// `typed` is in `name`.
fn abbreviates_into(name: &str, typed: &str, at: &mut Vec<u32>) -> bool {
    at.clear();
    if name.is_ascii() && typed.is_ascii() {
        return place(name.as_bytes(), typed.as_bytes(), at, &mut Vec::new());
    }
    let name: Vec<char> = name.chars().collect();
    let typed: Vec<char> = typed.chars().collect();
    place(&name, &typed, at, &mut Vec::new())
}

/// A character of a name, as bytes are of ASCII names.
trait Letter: Copy + PartialEq {
    fn same(self, other: Self) -> bool;
    fn lower(self) -> bool;
    fn upper(self) -> bool;
    fn digit(self) -> bool;
    fn underscore(self) -> bool;
}

impl Letter for u8 {
    fn same(self, other: u8) -> bool {
        self.eq_ignore_ascii_case(&other)
    }
    fn lower(self) -> bool {
        self.is_ascii_lowercase()
    }
    fn upper(self) -> bool {
        self.is_ascii_uppercase()
    }
    fn digit(self) -> bool {
        self.is_ascii_digit()
    }
    fn underscore(self) -> bool {
        self == b'_'
    }
}

impl Letter for char {
    fn same(self, other: char) -> bool {
        self == other || self.to_lowercase().eq(other.to_lowercase())
    }
    fn lower(self) -> bool {
        self.is_lowercase()
    }
    fn upper(self) -> bool {
        self.is_uppercase()
    }
    fn digit(self) -> bool {
        self.is_numeric()
    }
    fn underscore(self) -> bool {
        self == '_'
    }
}

/// Places the typed characters after those in `at`: each right after the
/// one before, as in a run, or failing that, at a later word start.
/// `dead` holds the places known to lead nowhere, by typed character and
/// character of `name`, once any are.
fn place<L: Letter>(name: &[L], typed: &[L], at: &mut Vec<u32>, dead: &mut Vec<bool>) -> bool {
    let Some(&c) = typed.get(at.len()) else {
        return true;
    };
    let from = at.last().map_or(0, |&last| last as usize + 1);
    for i in from..name.len() {
        let run = i == from && !at.is_empty();
        if !(run || starts_word(name, i)) || !name[i].same(c) {
            continue;
        }
        let key = at.len() * name.len() + i;
        if dead.get(key) == Some(&true) {
            continue;
        }
        at.push(i as u32);
        if place(name, typed, at, dead) {
            return true;
        }
        at.pop();
        if dead.is_empty() {
            dead.resize(typed.len() * name.len(), false);
        }
        dead[key] = true;
    }
    false
}

/// Whether the character at `i` of `name` starts a word of it: its first,
/// or the first after `_`, the capital of camelCase, the `P` of
/// `HTMLParser`, or the first digit of a number.
fn starts_word<L: Letter>(name: &[L], i: usize) -> bool {
    let c = name[i];
    let Some(&before) = i.checked_sub(1).and_then(|b| name.get(b)) else {
        return true;
    };
    let next_lower = name.get(i + 1).is_some_and(|n| n.lower());
    (before.underscore() && !c.underscore())
        || (before.lower() && c.upper())
        || (before.upper() && c.upper() && next_lower)
        || (!before.digit() && c.digit())
}

/// The keywords of `language`, as entries.
pub fn keyword_entries(language: &'static Language) -> impl Iterator<Item = Entry> {
    let words = keywords(language);
    (0..words.len()).map(move |i| Entry {
        name: words[i].clone(),
        kind: "keyword",
        detail: String::new(),
        near: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(word: &str) -> Typed {
        Typed {
            row: 0,
            start: 0,
            word: word.to_string(),
            in_code: true,
            word_after: false,
        }
    }

    fn symbol(name: &str, kind: &'static str, detail: &str) -> Entry {
        Entry {
            name: name.to_string(),
            kind,
            detail: detail.to_string(),
            near: None,
        }
    }

    #[test]
    fn words_skip_the_one_typed_and_note_how_near() {
        let text = "let width = 1;\nlet w = width;\n// wid\nwidth_of(9x)\n";
        let typed_at = text.find("wid\n").unwrap();
        let mut found = words(text, typed_at..typed_at + 3, Some(2), None);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = found.iter().map(|e| (e.name.as_str(), e.near)).collect();
        assert_eq!(
            names,
            [("let", Some(1)), ("width", Some(1)), ("width_of", Some(1)),],
            "not the one typed, one-letter words, or ones starting with a digit"
        );
    }

    #[test]
    fn keywords_are_the_words_the_grammar_spells_out() {
        let rust = language::detect(Some(Path::new("a.rs")), String::new).unwrap();
        let words = keywords(rust);
        for keyword in ["fn", "let", "match", "impl", "pub", "struct"] {
            assert!(words.iter().any(|w| w == keyword), "{keyword}");
        }
        assert!(!words.iter().any(|w| w == "identifier"));
        assert!(!words.iter().any(|w| w.contains(|c: char| !is_word_char(c))));
    }

    #[test]
    fn latex_words_and_builtins_start_with_their_backslash() {
        let text = "\\section{Intro} \\\\ \\alpha_1 text";
        let mut found = words(text, 0..0, None, Some('\\'));
        found.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Intro", "\\alpha_1", "\\section", "text"]);

        let latex = language::detect(Some(Path::new("a.tex")), String::new).unwrap();
        let words = keywords(latex);
        for builtin in ["\\section", "\\frac", "\\textbf", "\\alpha"] {
            assert!(words.iter().any(|w| w == builtin), "{builtin}");
        }
    }

    #[test]
    fn keywords_take_in_the_builtins_the_grammar_leaves_out() {
        let cases = [
            ("a.rs", &["usize", "Some", "self", "Vec"][..]),
            ("a.py", &["True", "None", "len", "isinstance"]),
            ("a.js", &["true", "undefined", "console"]),
            ("a.ts", &["console", "Partial", "bigint"]),
            ("a.go", &["nil", "string", "append"]),
            ("a.c", &["int", "size_t", "printf"]),
            ("a.cpp", &["int", "static_cast", "unique_ptr"]),
        ];
        for (file, builtins) in cases {
            let language = language::detect(Some(Path::new(file)), String::new).unwrap();
            let words = keywords(language);
            for builtin in builtins {
                assert!(words.iter().any(|w| w == builtin), "{file}: {builtin}");
            }
        }
    }

    #[test]
    fn abbreviations_go_by_word_starts_and_runs() {
        let at = |name: &str, typed: &str| abbreviates(name, typed);
        assert_eq!(at("get_width", "gw"), Some(vec![0, 4]));
        assert_eq!(at("get_width", "getw"), Some(vec![0, 1, 2, 4]));
        assert_eq!(at("getWidth", "gwi"), Some(vec![0, 3, 4]));
        assert_eq!(at("HTMLParser", "hp"), Some(vec![0, 4]));
        assert_eq!(at("Width", "wid"), Some(vec![0, 1, 2]));
        assert_eq!(at("__init__", "in"), Some(vec![2, 3]));
        assert_eq!(at("hogwash", "w"), None);
        assert_eq!(at("hogwash", "gw"), None);
        // `t` of `the` is no run from `comple`, nor a word start.
        assert_eq!(
            at("completions_pop_up_as_words_are_typed", "completed"),
            None
        );
        assert_eq!(at("area_of_circle_made", "aocm"), Some(vec![0, 5, 8, 15]));
        // A run that leads nowhere gives way to word starts.
        assert_eq!(at("aa_ab", "aab"), Some(vec![0, 3, 4]));
    }

    #[test]
    fn ranks_prefixes_then_near_words_then_the_rest() {
        let local = vec![
            Entry::word("get_width".into(), Some(30)),
            Entry::word("gadget_width".into(), Some(0)),
            Entry::word("getter".into(), Some(1)),
            Entry::word("ge".into(), Some(0)),
        ];
        let workspace = Rc::new(Vocabulary::from(vec![
            symbol("get_width", "function", "Panel"),
            symbol("GetWindow", "function", "win.rs"),
            symbol("hogwash", "const", "a.rs"),
        ]));
        let mut completer = Completer::new();
        let mut completion = Completion::new(Weak::new(), &typed("gw"), local, workspace, false);
        completion.filter("gw", &mut completer);
        let names = completion.names();
        assert_eq!(names.len(), 3, "not hogwash: {names:?}");
        assert_eq!(names[0], "gadget_width", "the nearest");
        assert!(names.contains(&"GetWindow"), "regardless of case");
        // Listed once, as the workspace defines it.
        assert_eq!(
            completion
                .local
                .iter()
                .filter(|e| e.name == "get_width")
                .count(),
            1
        );
        let entry = completion
            .local
            .iter()
            .find(|e| e.name == "get_width")
            .unwrap();
        assert_eq!(detail_of(entry), "function Panel");

        completion.filter("ge", &mut completer);
        assert_eq!(
            completion.names()[..2],
            ["getter", "get_width"],
            "not the word typed; the nearer prefix first"
        );
        completion.filter("get", &mut completer);
        assert_eq!(completion.names()[0], "getter");
        completer.completed("get_width");
        completion.filter("ge", &mut completer);
        completion.filter("get", &mut completer);
        assert_eq!(completion.names()[0], "get_width", "completed recently");

        completion.filter("getx", &mut completer);
        assert!(!completion.is_shown());
        completion.filter("ge", &mut completer);
        assert!(completion.is_shown(), "backspacing widens it again");
    }

    #[test]
    fn the_selection_wraps_and_pages() {
        let local = (0..20)
            .map(|i| Entry::word(format!("word{i:02}"), None))
            .collect();
        let mut completer = Completer::new();
        let workspace = Rc::new(Vocabulary::default());
        let mut completion = Completion::new(Weak::new(), &typed("wo"), local, workspace, false);
        completion.filter("wo", &mut completer);
        assert_eq!(completion.selected(), Some("word00"));
        completion.step(-1);
        assert_eq!(completion.selected(), Some("word19"));
        assert_eq!(completion.top, 20 - ROWS);
        completion.step(1);
        assert_eq!((completion.selected(), completion.top), (Some("word00"), 0));
        completion.page(true);
        assert_eq!(completion.selected(), Some("word08"));
        assert_eq!(completion.top, 1);
    }
}
