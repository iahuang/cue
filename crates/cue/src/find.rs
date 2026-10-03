//! Finding and replacing in the open file (Ctrl+F, Ctrl+H), and finding in
//! a terminal's output: the find bar, which floats over the top right of
//! the text as in most editors, and the matches of its query in the text.
//!
//! The bar has a row for the query, with the match count, the search
//! options as toggles, and a close button, and for replacing, a row for the
//! replacement with buttons to replace one match or all. Every match is
//! highlighted as you type. The editor or terminal selects the current
//! match, and the editor does the replacing; this module finds the matches
//! and draws the bar.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::time::Instant;

use grep_matcher::{Captures, Matcher};
use grep_regex::RegexMatcher;
use opentui::{Attributes, Buffer, Rgba};

use crate::keymap::{Command, Context, Keymap};
use crate::line_edit::{self, Caret, Edit};
use crate::search::{Query, Toggle};
use crate::theme;

/// Past this many matches, the rest aren't found.
pub const MAX_MATCHES: usize = 100_000;

/// The widest the bar gets, in columns.
pub const MAX_WIDTH: u32 = 60;
/// Left of the fields: the button that shows or hides the replacement.
const EXPANDER_WIDTH: u32 = 3;
/// Each toggle takes this many columns.
const TOGGLE_WIDTH: u32 = 4;
/// Room kept for the match count, so the query field doesn't jump around.
const STATUS_WIDTH: u32 = 14;
/// Below this, the replace buttons leave out their shortcuts.
const MIN_FIELD_WIDTH: u32 = 12;

/// What's kept from one find to the next, and between files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    pub query: Query,
    pub replacement: String,
}

/// The bar's text fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Find,
    Replace,
}

impl Field {
    /// Where the field's keys apply.
    pub fn context(self) -> Context {
        match self {
            Field::Find => Context::Find,
            Field::Replace => Context::Replace,
        }
    }
}

/// A match, in the text's bytes and in the editor's positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// In the text with `\n` line breaks.
    pub bytes: Range<usize>,
    /// In a terminal, counted from the top of the scrollback.
    pub row: u32,
    /// Display columns in the row.
    pub cols: Range<u32>,
    /// Cursor offsets; in a terminal, `bytes`.
    pub offsets: Range<u32>,
}

/// What a click on the bar landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Field(Field),
    Toggle(Toggle),
    /// Shows or hides the replacement row.
    Expander,
    Close,
    Replace,
    ReplaceAll,
}

/// Where the parts of the bar are, in columns, for drawing and clicking.
struct Layout {
    /// The columns each field takes, including a column of padding on
    /// either side of the text.
    fields: Range<u32>,
    status: (String, Rgba, u32),
    /// Clickable parts, as (row, columns, target, label).
    buttons: Vec<(u32, Range<u32>, Target, String)>,
}

pub struct FindBar {
    pub memory: Memory,
    /// Whether the replacement row can show: not for a terminal's output.
    pub replaceable: bool,
    /// Whether the replacement row shows.
    pub replacing: bool,
    /// The field with the keyboard, or `None` while the text has it.
    pub focus: Option<Field>,
    /// The cursor and selection in the query, and in the replacement.
    pub carets: [Caret; 2],
    /// In order. Never empty ranges.
    pub matches: Vec<Match>,
    /// The text has more matches than were found.
    pub truncated: bool,
    /// Why the query can't be searched: an invalid regex.
    pub error: Option<String>,
    /// The text version the matches are for; `None` when they need finding
    /// again.
    pub epoch: Option<u64>,
    /// Where typing a query finds the next match from: the cursor offset,
    /// or in a terminal, the row.
    pub origin: u32,
    /// The buttons as last drawn, as (row, screen columns, target), for
    /// clicks.
    drawn: RefCell<Vec<(u32, Range<u32>, Target)>>,
    /// The screen column the fields' text starts at, as last drawn.
    text_x: Cell<u32>,
}

impl FindBar {
    /// A bar with `memory`'s query selected, so typing replaces it.
    pub fn new(memory: Memory, origin: u32) -> FindBar {
        let mut bar = FindBar {
            memory,
            replaceable: true,
            replacing: false,
            focus: Some(Field::Find),
            carets: Default::default(),
            matches: Vec::new(),
            truncated: false,
            error: None,
            epoch: None,
            origin,
            drawn: RefCell::default(),
            text_x: Cell::new(0),
        };
        bar.select_query();
        bar
    }

    /// Rows the bar takes.
    pub fn rows(&self) -> u32 {
        if self.replacing {
            2
        } else {
            1
        }
    }

    // --- editing the fields ------------------------------------------------------

    /// Edits the focused field: typing, pasting, deleting, or moving the
    /// cursor. Changing the query marks the matches for finding again.
    pub fn edit(&mut self, edit: Edit) {
        self.edit_field(edit, false);
    }

    /// Edits the focused field with Shift held: moving the cursor selects.
    pub fn edit_selecting(&mut self, edit: Edit) {
        self.edit_field(edit, true);
    }

    fn edit_field(&mut self, edit: Edit, select: bool) {
        let Some(field) = self.focus else {
            return;
        };
        let (caret, text) = self.field_mut(field);
        let changed = match select {
            true => caret.select(text, edit),
            false => caret.edit(text, edit),
        };
        if changed && field == Field::Find {
            self.epoch = None;
        }
    }

    fn field_mut(&mut self, field: Field) -> (&mut Caret, &mut String) {
        let caret = &mut self.carets[field as usize];
        match field {
            Field::Find => (caret, &mut self.memory.query.text),
            Field::Replace => (caret, &mut self.memory.replacement),
        }
    }

    fn text(&self, field: Field) -> &str {
        match field {
            Field::Find => &self.memory.query.text,
            Field::Replace => &self.memory.replacement,
        }
    }

    /// Selects all of the query, so typing replaces it.
    pub fn select_query(&mut self) {
        let (caret, text) = self.field_mut(Field::Find);
        caret.select_all(text);
    }

    /// Selects all of the focused field.
    pub fn select_all(&mut self) {
        if let Some(field) = self.focus {
            let (caret, text) = self.field_mut(field);
            caret.select_all(text);
        }
    }

    /// The part of the focused field selected.
    pub fn selected_text(&self) -> Option<&str> {
        let field = self.focus?;
        self.carets[field as usize].selected_text(self.text(field))
    }

    /// A press of the mouse on `field` at screen column `x`: focuses it and
    /// puts the cursor there, or selects (see [`Caret::press`]).
    pub fn press(&mut self, field: Field, x: u32, now: Instant) {
        self.focus = Some(field);
        let column = x.saturating_sub(self.text_x.get()) as usize;
        let (caret, text) = self.field_mut(field);
        caret.press(text, column, now);
    }

    /// The mouse dragged to screen column `x`, selecting in the field
    /// pressed, if any.
    pub fn drag(&mut self, x: u32) {
        let column = x.saturating_sub(self.text_x.get()) as usize;
        for field in [Field::Find, Field::Replace] {
            let (caret, text) = self.field_mut(field);
            caret.drag(text, column);
        }
    }

    /// The mouse was released.
    pub fn release(&mut self) {
        for field in [Field::Find, Field::Replace] {
            let (caret, text) = self.field_mut(field);
            caret.release(text);
        }
    }

    /// Whether a press in a field is held, so drags select there.
    pub fn pressed(&self) -> bool {
        self.carets.iter().any(Caret::pressed)
    }

    pub fn toggle(&mut self, toggle: Toggle) {
        toggle.flip(&mut self.memory.query);
        self.epoch = None;
    }

    // --- matches -----------------------------------------------------------------

    /// The first match at or after `offset`, wrapping around to the first.
    pub fn next_from(&self, offset: u32) -> Option<usize> {
        let i = self.matches.partition_point(|m| m.offsets.start < offset);
        (!self.matches.is_empty()).then(|| i % self.matches.len())
    }

    /// The last match before `offset`, wrapping around to the last.
    pub fn previous_from(&self, offset: u32) -> Option<usize> {
        let i = self.matches.partition_point(|m| m.offsets.start < offset);
        match i {
            _ if self.matches.is_empty() => None,
            0 => Some(self.matches.len() - 1),
            i => Some(i - 1),
        }
    }

    /// The match covering exactly the offsets `start..end`, if any.
    pub fn at(&self, start: u32, end: u32) -> Option<usize> {
        let i = self.matches.partition_point(|m| m.offsets.start < start);
        self.matches
            .get(i)
            .filter(|m| m.offsets == (start..end))
            .map(|_| i)
    }

    // --- drawing -----------------------------------------------------------------

    /// Draws the bar at (`x`, `y`), `width` wide, with `current` as the
    /// selected match, and returns where the terminal cursor goes if a
    /// field has focus.
    pub fn draw(
        &self,
        frame: &Buffer,
        (x, y, width): (u32, u32, u32),
        current: Option<usize>,
        keymap: &Keymap,
    ) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let layout = self.layout((x, width), current, keymap);
        *self.drawn.borrow_mut() = layout
            .buttons
            .iter()
            .map(|(row, columns, target, _)| (*row, columns.clone(), *target))
            .collect();
        frame.fill_rect(x, y, width, self.rows(), colors.surface);
        let (status, status_fg, status_x) = &layout.status;
        frame.draw_text(status, *status_x, y, *status_fg, None, Attributes::NONE);
        for (row, columns, target, label) in &layout.buttons {
            let on = matches!(target, Target::Toggle(t) if t.is_on(&self.memory.query));
            let (fg, bg, attributes) = if on {
                (colors.text, Some(colors.selected), Attributes::BOLD)
            } else {
                (colors.muted, None, Attributes::NONE)
            };
            frame.draw_text(label, columns.start, y + row, fg, bg, attributes);
        }
        let mut cursor = self.draw_field(frame, Field::Find, layout.fields.clone(), y);
        if self.replacing {
            let replace = self.draw_field(frame, Field::Replace, layout.fields, y + 1);
            cursor = cursor.or(replace);
        }
        cursor
    }

    /// Draws a field in `columns` of row `y`, and returns the cursor
    /// position if it has focus.
    fn draw_field(
        &self,
        frame: &Buffer,
        field: Field,
        columns: Range<u32>,
        y: u32,
    ) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let room = columns.len().saturating_sub(2);
        if room == 0 {
            return None;
        }
        frame.fill_rect(columns.start, y, columns.len() as u32, 1, colors.bg);
        let (text, placeholder) = match field {
            Field::Find => (&self.memory.query.text, "Find"),
            Field::Replace => (&self.memory.replacement, "Replace"),
        };
        let text_x = columns.start + 1;
        self.text_x.set(text_x);
        if text.is_empty() {
            let placeholder: String = placeholder.chars().take(room).collect();
            frame.draw_text(
                &placeholder,
                text_x,
                y,
                colors.muted,
                None,
                Attributes::NONE,
            );
        }
        let caret = &self.carets[field as usize];
        let (shown, column) = caret.view(text, room);
        let focused = self.focus == Some(field);
        if focused {
            line_edit::draw_selection(frame, caret, text, text_x, y, room);
        }
        frame.draw_text(&shown, text_x, y, colors.text, None, Attributes::NONE);
        focused.then_some((text_x + column as u32, y))
    }

    /// Lays out a bar at column `x`, `width` wide: on the first row, the
    /// expander, the query, the match count, the toggles, and the close
    /// button; on the second, the replacement and the replace buttons.
    fn layout(&self, (x, width): (u32, u32), current: Option<usize>, keymap: &Keymap) -> Layout {
        let end = x + width;
        let expander = if self.replacing { " ▾ " } else { " ▸ " };
        let mut buttons = Vec::new();
        if self.replaceable {
            buttons.push((
                0,
                x..x + EXPANDER_WIDTH,
                Target::Expander,
                expander.to_string(),
            ));
        }
        let close = end.saturating_sub(4)..end.saturating_sub(1);
        let toggles_x = close
            .start
            .saturating_sub(TOGGLE_WIDTH * Toggle::ALL.len() as u32);
        for (i, toggle) in Toggle::ALL.into_iter().enumerate() {
            let start = toggles_x + TOGGLE_WIDTH * i as u32;
            let label = format!(" {} ", toggle.label());
            buttons.push((
                0,
                start..start + TOGGLE_WIDTH,
                Target::Toggle(toggle),
                label,
            ));
        }
        buttons.push((0, close, Target::Close, " × ".to_string()));
        let (status, status_fg) = self.status(current);
        let status_width = STATUS_WIDTH.max(status.chars().count() as u32);
        let status_end = toggles_x.saturating_sub(1);
        let status_x = status_end.saturating_sub(status.chars().count() as u32);
        let fields_x = x + if self.replaceable { EXPANDER_WIDTH } else { 1 };
        let mut fields_end = status_end.saturating_sub(status_width + 1);
        if fields_end < fields_x + MIN_FIELD_WIDTH {
            // Narrow: the count gets only the room it needs.
            fields_end = status_x.saturating_sub(1);
        }
        fields_end = fields_end.max(fields_x);

        if self.replacing {
            // Right-aligned, with their shortcuts if there's room.
            let names = [(Command::Replace, "replace"), (Command::ReplaceAll, "all")];
            let make_labels = |shortcuts: bool| {
                names.map(|(command, name)| {
                    let key = keymap
                        .shortcut_in(command, Context::Replace)
                        .filter(|_| shortcuts);
                    let target = match command {
                        Command::Replace => Target::Replace,
                        _ => Target::ReplaceAll,
                    };
                    let label = match key {
                        Some(key) => format!(" {key:#} {name} "),
                        None => format!(" {name} "),
                    };
                    (target, label)
                })
            };
            let width_of = |labels: &[(Target, String); 2]| {
                labels
                    .iter()
                    .map(|(_, l)| l.chars().count() as u32)
                    .sum::<u32>()
            };
            let mut labels = make_labels(true);
            if end.saturating_sub(width_of(&labels) + 2) < fields_x + MIN_FIELD_WIDTH {
                labels = make_labels(false);
            }
            let mut start = end.saturating_sub(width_of(&labels) + 1);
            fields_end = fields_end.min(start.saturating_sub(1)).max(fields_x);
            for (target, label) in labels {
                let len = label.chars().count() as u32;
                buttons.push((1, start..start + len, target, label));
                start += len;
            }
        }
        Layout {
            fields: fields_x..fields_end,
            status: (status, status_fg, status_x),
            buttons,
        }
    }

    /// What the find row says about the matches, and its color.
    fn status(&self, current: Option<usize>) -> (String, Rgba) {
        let colors = theme::colors();
        if self.error.is_some() {
            return ("invalid regex".to_string(), colors.error);
        }
        let count = self.matches.len();
        let more = if self.truncated { "+" } else { "" };
        let text = match (count, current) {
            _ if self.memory.query.text.is_empty() => String::new(),
            (0, _) => return ("no matches".to_string(), colors.error),
            (_, Some(i)) => format!("{} of {count}{more}", i + 1),
            (1, None) => "1 match".to_string(),
            (_, None) => format!("{count}{more} matches"),
        };
        (text, colors.muted)
    }

    /// What's at screen column `x` of the bar's row `row`, as last drawn.
    pub fn target(&self, x: u32, row: u32) -> Target {
        let drawn = self.drawn.borrow();
        let button = drawn
            .iter()
            .find(|(r, columns, _)| *r == row && columns.contains(&x));
        match button {
            Some((_, _, target)) => *target,
            None if row == 0 => Target::Field(Field::Find),
            None => Target::Field(Field::Replace),
        }
    }

    /// The error, for the status bar.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// The matches of `query` in `text`, as byte ranges, and whether there
/// were more than [`MAX_MATCHES`]. Empty matches (of `^`, say) don't count.
/// Fails with a message if the query isn't a valid regex.
pub fn find(text: &str, query: &Query) -> Result<(Vec<Range<usize>>, bool), String> {
    if query.text.is_empty() {
        return Ok((Vec::new(), false));
    }
    let matcher = query.matcher()?;
    let mut matches = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        let _ = matcher.find_iter(line.as_bytes(), |m| {
            if !m.is_empty() {
                matches.push(start + m.start()..start + m.end());
            }
            matches.len() <= MAX_MATCHES
        });
        if matches.len() > MAX_MATCHES {
            matches.truncate(MAX_MATCHES);
            return Ok((matches, true));
        }
        start += line.len() + 1;
    }
    Ok((matches, false))
}

/// Makes the text that replaces each match.
pub struct Replacer<'r> {
    replacement: &'r str,
    /// For regex queries, to expand the groups they capture.
    matcher: Option<RegexMatcher>,
}

impl<'r> Replacer<'r> {
    pub fn new(query: &Query, replacement: &'r str) -> Replacer<'r> {
        Replacer {
            replacement,
            matcher: query.regex.then(|| query.matcher().ok()).flatten(),
        }
    }

    /// What replaces the match at `range` in `text`: the replacement, with
    /// `$1` or `${name}` standing for what the regex's groups captured.
    pub fn expand(&self, text: &str, range: Range<usize>) -> String {
        let Some(matcher) = &self.matcher else {
            return self.replacement.to_string();
        };
        // The match's line, so anchors see what's around it.
        let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[range.end..]
            .find('\n')
            .map_or(text.len(), |i| range.end + i);
        let line = &text.as_bytes()[line_start..line_end];
        self.expand_in_line(matcher, line, range.start - line_start)
            .unwrap_or_else(|| self.replacement.to_string())
    }

    fn expand_in_line(&self, matcher: &RegexMatcher, line: &[u8], at: usize) -> Option<String> {
        let mut captures = matcher.new_captures().ok()?;
        if !matcher.captures_at(line, at, &mut captures).ok()? {
            return None;
        }
        let mut out = Vec::new();
        captures.interpolate(
            |name| matcher.capture_index(name),
            line,
            self.replacement.as_bytes(),
            &mut out,
        );
        String::from_utf8(out).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Query {
        Query {
            text: text.to_string(),
            ..Query::default()
        }
    }

    fn found<'t>(text: &'t str, query: &Query) -> Vec<&'t str> {
        let (ranges, _) = find(text, query).unwrap();
        ranges.into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn finds_on_every_line_skipping_empty_matches() {
        let text = "Foo foo\nfoobar\n\nfoo";
        assert_eq!(found(text, &query("foo")), ["Foo", "foo", "foo", "foo"]);
        let (ranges, _) = find(text, &query("foo")).unwrap();
        assert_eq!(ranges[2], 8..11);
        assert_eq!(ranges[3], 16..19);
        let whole = Query {
            whole_word: true,
            case_sensitive: true,
            ..query("foo")
        };
        assert_eq!(found(text, &whole), ["foo", "foo"]);
        let anchored = Query {
            regex: true,
            ..query("^f?")
        };
        assert_eq!(
            found(text, &anchored),
            ["F", "f", "f"],
            "per line; no empty"
        );
        assert!(find(text, &query("")).unwrap().0.is_empty());
        let bad = Query {
            regex: true,
            ..query("(")
        };
        assert!(find(text, &bad).is_err());
    }

    #[test]
    fn stops_after_the_most_matches() {
        let text = "x".repeat(MAX_MATCHES + 5);
        let (ranges, truncated) = find(&text, &query("x")).unwrap();
        assert_eq!(ranges.len(), MAX_MATCHES);
        assert!(truncated);
    }

    #[test]
    fn replacements_expand_groups_only_for_regexes() {
        let text = "let a = 1;\nlet bc = 22;";
        let regex = Query {
            regex: true,
            ..query(r"(\w+) = (?P<n>\d+)")
        };
        let (ranges, _) = find(text, &regex).unwrap();
        let replacer = Replacer::new(&regex, "${n} = $1");
        assert_eq!(replacer.expand(text, ranges[1].clone()), "22 = bc");
        let literal = Replacer::new(&query("a = 1"), "$1");
        assert_eq!(literal.expand(text, 4..9), "$1");
        // Anchors see the rest of the line.
        let anchored = Query {
            regex: true,
            ..query(r"^let (\w)")
        };
        let (ranges, _) = find(text, &anchored).unwrap();
        let replacer = Replacer::new(&anchored, "$1");
        assert_eq!(replacer.expand(text, ranges[1].clone()), "b");
    }

    #[test]
    fn steps_through_matches_wrapping_around() {
        let mut bar = FindBar::new(Memory::default(), 0);
        bar.matches = [(2, 4), (6, 8)]
            .into_iter()
            .map(|(start, end)| Match {
                bytes: start as usize..end as usize,
                row: 0,
                cols: start..end,
                offsets: start..end,
            })
            .collect();
        assert_eq!(bar.next_from(0), Some(0));
        assert_eq!(bar.next_from(2), Some(0), "a match at the cursor");
        assert_eq!(bar.next_from(3), Some(1));
        assert_eq!(bar.next_from(7), Some(0), "wraps");
        assert_eq!(bar.previous_from(6), Some(0));
        assert_eq!(bar.previous_from(2), Some(1), "wraps");
        assert_eq!(bar.at(6, 8), Some(1));
        assert_eq!(bar.at(6, 7), None);
        bar.matches.clear();
        assert_eq!(bar.next_from(0), None);
        assert_eq!(bar.previous_from(0), None);
    }

    #[test]
    fn fields_select_with_the_keyboard_and_the_mouse() {
        let _serial = crate::test_serial();
        let memory = Memory {
            query: query("needle"),
            replacement: "pin".to_string(),
        };
        let mut bar = FindBar::new(memory, 0);
        assert_eq!(bar.selected_text(), Some("needle"), "typing replaces it");
        bar.edit(Edit::Insert("hay"));
        assert_eq!(bar.memory.query.text, "hay");
        bar.edit_selecting(Edit::WordLeft);
        assert_eq!(bar.selected_text(), Some("hay"));
        bar.edit(Edit::Right);
        assert_eq!(bar.selected_text(), None);

        // Drawn from column 10, the fields' text starts at a column after.
        bar.replacing = true;
        let keymap = Keymap::default();
        let screen =
            opentui::OwnedBuffer::new(80, 4, false, opentui::WidthMethod::Unicode, "test").unwrap();
        bar.draw(&screen, (10, 0, 60), None, &keymap);
        let text_x = 10 + EXPANDER_WIDTH + 1;
        let now = Instant::now();
        bar.press(Field::Replace, text_x + 1, now);
        assert_eq!(bar.focus, Some(Field::Replace));
        assert!(bar.pressed());
        bar.drag(text_x + 3);
        bar.release();
        assert!(!bar.pressed());
        assert_eq!(bar.selected_text(), Some("in"));
        bar.drag(text_x);
        assert_eq!(bar.selected_text(), Some("in"), "not after the release");
        bar.draw(&screen, (10, 0, 60), None, &keymap);
        let selection = theme::colors().selection;
        let shaded: Vec<bool> = (text_x..text_x + 4)
            .map(|x| screen.bg_at(x, 1) == Some(selection))
            .collect();
        assert_eq!(shaded, [false, true, true, false]);
        // Only the focused field shows its selection.
        bar.press(Field::Find, text_x, now);
        bar.release();
        bar.draw(&screen, (10, 0, 60), None, &keymap);
        assert_ne!(screen.bg_at(text_x + 1, 1), Some(selection));
    }
}
