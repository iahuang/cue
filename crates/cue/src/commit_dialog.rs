//! The commit dialog: where what's staged in a repository is committed,
//! popped up over the middle of the screen by the changes view's Commit
//! button, or `c`.
//!
//! At the top is the box the message is written in, which grows with it,
//! then a switch to amend the last commit instead. Under them are the
//! repository and branch committed to, with the lines what's committed
//! adds and removes, or that it changes none, and as whom, above
//! the buttons that cancel and commit. With nothing staged, it commits all
//! the changes, staging them first, as VS Code's smart commit does.
//!
//! Enter starts a new line of the message, and Ctrl+Enter commits. Tab and
//! Shift+Tab go between the message, the switch, and the buttons, which
//! Space or Enter press. Esc, or a click outside, closes it; what's written
//! is kept for next time, a draft for each repository.
//!
//! git commits in the background, since hooks may take a while; once it
//! has, the dialog closes.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use opentui::{Attributes, Buffer, Rgba};

use crate::git::{self, Repo, Staged};
use crate::icons;
use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind};
use crate::line_edit::{Caret, Edit};
use crate::picker::Area;
use crate::theme::{self, Hue};
use crate::tree::truncate;

/// The widest the dialog gets.
const MAX_WIDTH: u32 = 64;
/// The most rows the message box grows to; past them, it scrolls.
const MAX_MESSAGE_ROWS: usize = 8;
/// The switch to amend the last commit, after its mark.
const AMEND: &str = "Amend last commit";
/// Shown in the message box while it's empty.
const PLACEHOLDER: &str = "Message";
const CANCEL: &str = "Cancel";
/// What went wrong if git went away without saying.
const STOPPED: &str = "git stopped unexpectedly.";
/// Between the buttons.
const GAP: u32 = 2;

/// What the app should do after the dialog handled input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitAction {
    Continue,
    /// Close it, keeping what's written.
    Close,
    /// Commit, as [`CommitDialog::plan`] says.
    Commit,
}

/// What committing takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Commit what's staged.
    Staged,
    /// Stage all the changes, nothing being staged, then commit them.
    All,
}

/// Why there's nothing to commit yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CantCommit {
    NoMessage,
    NothingToCommit,
    /// A commit is being made already.
    Committing,
}

/// What the keyboard is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Message,
    Amend,
    Cancel,
    Commit,
}

const FIELDS: [Field; 4] = [Field::Message, Field::Amend, Field::Cancel, Field::Commit];

/// The commit's message as it's written, and where in it the cursor is.
#[derive(Default)]
struct Message {
    text: String,
    caret: Caret,
    /// The first of its rows shown in the box.
    scroll: usize,
}

/// A row of the message as it wraps: the text on it, and whether it wraps
/// onto the next row, rather than ending at a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    range: Range<usize>,
    wraps: bool,
}

pub struct CommitDialog {
    root: PathBuf,
    /// What the repository is called.
    name: String,
    branch: String,
    /// The commit checked out, abbreviated; `None` before the first, when
    /// there's none to amend.
    head: Option<String>,
    /// A merge can be committed without any staged file differences.
    merging: bool,
    /// The files changed, and those staged, wholly or partly.
    changed: usize,
    staged: usize,
    /// The lines all the changes add and remove, and the staged ones.
    lines: (usize, usize),
    staged_lines: (usize, usize),
    /// Who the commit's by, or what git said if it doesn't know.
    author: Result<String, String>,
    message: Message,
    focus: Field,
    amend: bool,
    /// The last commit's message, as turning on Amend filled it in, which
    /// turning it off takes away again, if it's still as it was.
    filled: Option<String>,
    /// The commit git is making, if it is: then its hash, or what went
    /// wrong.
    committing: Option<Receiver<Result<String, String>>>,
    screen_width: u32,
    screen_height: u32,
}

impl CommitDialog {
    /// The dialog for committing to `repo`, which is called `name`, on a
    /// screen `width` x `height`.
    pub fn new(repo: &Repo, name: String, width: u32, height: u32) -> CommitDialog {
        let mut dialog = CommitDialog {
            root: repo.root.clone(),
            name,
            branch: String::new(),
            head: None,
            merging: false,
            changed: 0,
            staged: 0,
            lines: (0, 0),
            staged_lines: (0, 0),
            author: Ok(String::new()),
            message: Message::default(),
            focus: Field::Message,
            amend: false,
            filled: None,
            committing: None,
            screen_width: width,
            screen_height: height,
        };
        dialog.set_repo(repo);
        dialog
    }

    /// The repository's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Catches up with what git says of the repository now.
    pub fn set_repo(&mut self, repo: &Repo) {
        if self.branch != repo.head.name() {
            self.clear_amend();
        }
        self.branch = repo.head.name().to_string();
        self.head = repo
            .commit
            .as_ref()
            .map(|hash| hash.chars().take(7).collect());
        self.merging = repo.merging;
        if self.head.is_none() {
            self.amend = false;
        }
        self.changed = repo.changes.len();
        self.staged = repo
            .changes
            .iter()
            .filter(|change| change.staged != Staged::No)
            .count();
        self.lines = (repo.added, repo.removed);
    }

    /// Reads, from git, who the commit's by and the lines what's staged
    /// adds and removes, as when the dialog opens.
    pub fn read(&mut self) {
        self.author = git::author(&self.root);
        self.read_lines();
    }

    /// Reads, from git, the lines what's staged adds and removes, as while
    /// the dialog's open.
    pub fn read_lines(&mut self) {
        self.staged_lines = match self.staged {
            0 => (0, 0),
            _ => git::staged_lines(&self.root, self.head.is_none()),
        };
    }

    /// Opens it with the keyboard in the message box.
    pub fn reset_focus(&mut self) {
        self.focus = Field::Message;
    }

    pub fn set_size(&mut self, width: u32, height: u32) {
        self.screen_width = width;
        self.screen_height = height;
        self.show_cursor();
    }

    pub fn committing(&self) -> bool {
        self.committing.is_some()
    }

    /// Whether the keyboard is in the message box, where keys edit it.
    #[cfg(test)]
    pub fn editing(&self) -> bool {
        self.focus == Field::Message
    }

    // --- the message ----------------------------------------------------------

    /// What's written in the message box.
    #[cfg(test)]
    pub fn message(&self) -> &str {
        &self.message.text
    }

    /// Applies `edit` to the message, with Shift held if `select`.
    pub fn edit(&mut self, edit: Edit, select: bool) {
        if self.committing() {
            return;
        }
        self.focus = Field::Message;
        let Message { text, caret, .. } = &mut self.message;
        let edit = match edit {
            // Terminals send line breaks in pastes as CR.
            Edit::Insert(inserted) if inserted.contains('\r') => {
                let inserted = inserted.replace("\r\n", "\n").replace('\r', "\n");
                caret.edit(text, Edit::Insert(&inserted));
                self.show_cursor();
                return;
            }
            edit => edit,
        };
        match select {
            true => caret.select(text, edit),
            false => caret.edit(text, edit),
        };
        self.show_cursor();
    }

    /// Moves the cursor `by` rows of the message as it wraps: from the
    /// first row up, to the start; from the last down, to the end.
    pub fn move_rows(&mut self, by: isize, select: bool) {
        let room = self.room();
        let text = &self.message.text;
        let lines = wrap(text, room);
        let at = self.message.caret.at(text);
        let row = row_of(&lines, at);
        let column = text[lines[row].range.start..at].chars().count();
        let at = match row.checked_add_signed(by) {
            Some(row) if row < lines.len() => offset_at(text, &lines[row], column),
            _ if by < 0 => 0,
            _ => text.len(),
        };
        self.message.caret.move_to(text, at, select);
        self.show_cursor();
    }

    /// Moves the cursor to the start of its row of the message, or with
    /// `end`, to the end.
    pub fn move_to_row_edge(&mut self, end: bool, select: bool) {
        let room = self.room();
        let text = &self.message.text;
        let lines = wrap(text, room);
        let line = &lines[row_of(&lines, self.message.caret.at(text))];
        let at = match end {
            true => offset_at(text, line, usize::MAX),
            false => line.range.start,
        };
        self.message.caret.move_to(text, at, select);
        self.show_cursor();
    }

    pub fn select_all(&mut self) {
        self.message.caret.select_all(&self.message.text);
    }

    /// Takes away the selection. Returns false if there was none.
    pub fn clear_selection(&mut self) -> bool {
        let text = &self.message.text;
        if self.message.caret.selection(text).is_none() {
            return false;
        }
        let at = self.message.caret.at(text);
        self.message.caret.move_to(text, at, false);
        true
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.message.caret.selected_text(&self.message.text)
    }

    /// Turns amending the last commit on or off. On, an empty message box
    /// gets the last commit's message; off, it's taken away again if it
    /// wasn't changed.
    pub fn toggle_amend(&mut self) {
        if self.head.is_none() || self.committing() {
            return;
        }
        self.amend = !self.amend;
        if self.amend && self.message.text.trim().is_empty() {
            if let Some(last) = git::last_message(&self.root) {
                self.message = Message::default();
                self.message.text = last.clone();
                self.message.caret.move_to_end();
                self.filled = Some(last);
            }
        } else if !self.amend && self.filled.take().as_ref() == Some(&self.message.text) {
            self.message = Message::default();
        }
        self.show_cursor();
    }

    /// Leaves Amend after switching branches, before the next repository
    /// refresh. Keeps a message the user edited, as turning Amend off does.
    pub fn clear_amend(&mut self) {
        if self.amend {
            self.toggle_amend();
        }
    }

    // --- committing -----------------------------------------------------------

    /// What committing takes now, or why it can't be done yet.
    pub fn plan(&self) -> Result<Plan, CantCommit> {
        if self.committing() {
            return Err(CantCommit::Committing);
        }
        if self.message.text.trim().is_empty() {
            return Err(CantCommit::NoMessage);
        }
        match self.staged {
            0 if self.amend || self.merging => Ok(Plan::Staged),
            0 if self.changed > 0 => Ok(Plan::All),
            0 => Err(CantCommit::NothingToCommit),
            _ => Ok(Plan::Staged),
        }
    }

    /// Starts committing what's staged, with the message written, in the
    /// background.
    pub fn start(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let root = self.root.clone();
        let message = self.message.text.clone();
        let amend = self.amend;
        std::thread::spawn(move || {
            let _ = sender.send(git::commit(&root, &message, amend));
        });
        self.committing = Some(receiver);
    }

    /// Takes how the commit went, once git's done: the new commit's
    /// abbreviated hash, or what went wrong. Once it's made, the message
    /// box is emptied, and Amend turned off.
    pub fn poll(&mut self) -> Option<Result<String, String>> {
        let result = match self.committing.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(STOPPED.to_string()),
        };
        Some(self.finish(result))
    }

    /// Waits for the commit being made, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) -> Option<Result<String, String>> {
        let result = self.committing.as_ref()?.recv();
        Some(self.finish(result.unwrap_or_else(|_| Err(STOPPED.to_string()))))
    }

    fn finish(&mut self, result: Result<String, String>) -> Result<String, String> {
        self.committing = None;
        if result.is_ok() {
            self.message = Message::default();
            self.amend = false;
            self.filled = None;
            self.focus = Field::Message;
        }
        result
    }

    // --- input ----------------------------------------------------------------

    /// Handles `key` if it's for the dialog rather than the message box:
    /// Tab and Shift+Tab, and off the message, Space and Enter, which press
    /// what the keyboard's on, the arrows, which go between the buttons,
    /// and Esc. `None` if it's the message box's.
    pub fn handle_key(&mut self, key: Key) -> Option<CommitAction> {
        let plain = key.mods.is_plain();
        let at = FIELDS.iter().position(|&field| field == self.focus)?;
        let step = |by: usize| FIELDS[(at + by) % FIELDS.len()];
        match key.code {
            KeyCode::Tab if plain && key.mods.shift => self.focus = step(FIELDS.len() - 1),
            KeyCode::Tab if plain => self.focus = step(1),
            _ if self.focus == Field::Message => return None,
            KeyCode::Esc => return Some(CommitAction::Close),
            KeyCode::Enter | KeyCode::Char(' ') if plain => return Some(self.press(self.focus)),
            KeyCode::Left | KeyCode::Up if self.focus == Field::Commit => {
                self.focus = Field::Cancel
            }
            KeyCode::Right | KeyCode::Down if self.focus == Field::Cancel => {
                self.focus = Field::Commit
            }
            _ => {}
        }
        Some(CommitAction::Continue)
    }

    /// What pressing `field` does.
    fn press(&mut self, field: Field) -> CommitAction {
        match field {
            Field::Message => {}
            Field::Amend => self.toggle_amend(),
            Field::Cancel => return CommitAction::Close,
            Field::Commit => return CommitAction::Commit,
        }
        CommitAction::Continue
    }

    /// A press puts the cursor in the message box, toggles Amend, or
    /// presses a button; one outside closes the dialog.
    pub fn handle_mouse(&mut self, mouse: Mouse) -> CommitAction {
        let MouseKind::Press(button) = mouse.kind else {
            return CommitAction::Continue;
        };
        let area = self.area();
        if !area.contains(mouse.x, mouse.y) {
            return CommitAction::Close;
        }
        if button != MouseButton::Left {
            return CommitAction::Continue;
        }
        let layout = self.layout(area);
        let (x, y) = (mouse.x, mouse.y);
        if layout.message.contains(&y) {
            if !self.committing() {
                self.focus = Field::Message;
                self.place_cursor(
                    x.saturating_sub(layout.text_x) as usize,
                    y - layout.message.start,
                );
            }
            return CommitAction::Continue;
        }
        let hit = self
            .controls(area, &layout)
            .into_iter()
            .find(|(_, at)| at.contains(x, y));
        match hit {
            Some((field, _)) => self.press(field),
            None => CommitAction::Continue,
        }
    }

    /// The switch or button under the pointer, if any.
    pub fn hover_area(&self, x: u32, y: u32) -> Option<Area> {
        let area = self.area();
        let layout = self.layout(area);
        self.controls(area, &layout)
            .into_iter()
            .map(|(_, at)| at)
            .find(|at| at.contains(x, y))
    }

    /// Puts the cursor at `column` of the message box's row `row`, as it
    /// shows it.
    fn place_cursor(&mut self, column: usize, row: u32) {
        let lines = wrap(&self.message.text, self.room());
        let row = (row as usize + self.message.scroll).min(lines.len() - 1);
        let at = offset_at(&self.message.text, &lines[row], column);
        self.message.caret.move_to(&self.message.text, at, false);
    }

    // --- layout ---------------------------------------------------------------

    /// Where it is: across the middle of the screen, its top where it stays
    /// as the message grows.
    pub fn area(&self) -> Area {
        let room = self.screen_height.saturating_sub(1);
        let width = self.area_width();
        let (others, _) = self.other_rows();
        let rows = self.message_rows() as u32;
        let tallest = (MAX_MESSAGE_ROWS as u32 + others).min(room);
        Area {
            x: (self.screen_width - width) / 2,
            y: (room - tallest) / 2,
            width,
            height: (rows + others).min(room),
        }
    }

    /// The rows it has besides the message's: the borders, the message
    /// box's, Amend, what's committed and by whom, the buttons, and if the
    /// screen has room, a blank row above what's committed and above the
    /// buttons, which it says too.
    fn other_rows(&self) -> (u32, bool) {
        let room = self.screen_height.saturating_sub(1);
        let spacious = 11 <= room;
        (if spacious { 10 } else { 8 }, spacious)
    }

    /// Where its parts are, in `area`.
    fn layout(&self, area: Area) -> Layout {
        let rows = self.message_rows() as u32;
        let message = area.y + 2..area.y + 2 + rows;
        let amend = message.end + 1;
        let summary = amend + 1 + self.other_rows().1 as u32;
        Layout {
            text_x: area.x + 4,
            message,
            amend,
            summary,
            author: summary + 1,
            buttons: area.y + area.height.saturating_sub(2),
        }
    }

    /// The switch and the buttons, and where each is.
    fn controls(&self, area: Area, layout: &Layout) -> Vec<(Field, Area)> {
        let row = |x, width, y| Area {
            x,
            y,
            width,
            height: 1,
        };
        let mut controls = vec![(
            Field::Amend,
            row(area.x + 2, 2 + AMEND.len() as u32, layout.amend),
        )];
        for (field, x, width) in self.button_spans(area) {
            controls.push((field, row(x, width, layout.buttons)));
        }
        controls
    }

    /// Where the buttons are on their row, right-aligned, as (x, width).
    fn button_spans(&self, area: Area) -> Vec<(Field, u32, u32)> {
        let commit = self.commit_label().chars().count() as u32 + 2;
        let cancel = CANCEL.len() as u32 + 2;
        let right = area.x + area.width.saturating_sub(2);
        let commit_x = right.saturating_sub(commit);
        let cancel_x = commit_x.saturating_sub(GAP + cancel);
        vec![
            (Field::Cancel, cancel_x, cancel),
            (Field::Commit, commit_x, commit),
        ]
    }

    /// The columns a row of the message has, inside the box's borders and
    /// the space along each.
    fn room(&self) -> usize {
        (self.area_width().saturating_sub(8) as usize).max(1)
    }

    /// The dialog's width, as [`CommitDialog::area`] has it.
    fn area_width(&self) -> u32 {
        self.screen_width
            .saturating_sub(4)
            .min(MAX_WIDTH)
            .max(self.screen_width.min(30))
    }

    /// The rows the message box shows: as many as the message takes, up to
    /// [`MAX_MESSAGE_ROWS`], or fewer if the screen's short.
    fn message_rows(&self) -> usize {
        let room = self.screen_height.saturating_sub(1);
        let (others, _) = self.other_rows();
        let most = (room.saturating_sub(others) as usize).clamp(1, MAX_MESSAGE_ROWS);
        wrap(&self.message.text, self.room()).len().clamp(1, most)
    }

    /// Scrolls the message box to the cursor's row.
    fn show_cursor(&mut self) {
        let text = &self.message.text;
        let lines = wrap(text, self.room());
        let row = row_of(&lines, self.message.caret.at(text));
        let rows = self.message_rows();
        let scroll = &mut self.message.scroll;
        if row < *scroll {
            *scroll = row;
        } else if row >= *scroll + rows {
            *scroll = row + 1 - rows;
        }
        *scroll = (*scroll).min(lines.len().saturating_sub(rows));
    }

    // --- what it says ---------------------------------------------------------

    /// The lines what's committed adds and removes: what's staged, or with
    /// nothing staged, all the changes. `None` if it changes no files, as
    /// for only a new message, or a merge.
    fn committed_lines(&self) -> Option<(usize, usize)> {
        match self.staged {
            0 if self.amend || self.merging || self.changed == 0 => None,
            0 => Some(self.lines),
            _ => Some(self.staged_lines),
        }
    }

    /// What the button that commits says.
    fn commit_label(&self) -> &'static str {
        match self.plan() {
            Err(CantCommit::Committing) => "Committing…",
            _ if self.amend => "Amend",
            Ok(Plan::All) => "Commit All",
            Err(CantCommit::NoMessage) if self.staged == 0 && self.changed > 0 => "Commit All",
            _ => "Commit",
        }
    }

    // --- drawing --------------------------------------------------------------

    /// Draws it, and returns where the cursor goes, if the keyboard's in the
    /// message box. Draws nothing on a screen too small for it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let area = self.area();
        if area.width < 12 || area.height < 8 {
            return None;
        }
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            self.draw_dialog(frame, area)
        })
    }

    fn draw_dialog(&self, frame: &Buffer, area: Area) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let layout = self.layout(area);
        let Area {
            x,
            y,
            width,
            height,
        } = area;
        frame.fill_rect(x, y, width, height, colors.bg);
        let border = |s: &str, x: u32, y: u32, fg: Rgba| {
            frame.draw_text(s, x, y, fg, None, Attributes::NONE)
        };
        let rule = "─".repeat(width.saturating_sub(2) as usize);
        border(&format!("╭{rule}╮"), x, y, colors.border);
        border(&format!("╰{rule}╯"), x, y + height - 1, colors.border);
        for row in y + 1..y + height - 1 {
            border("│", x, row, colors.border);
            border("│", x + width - 1, row, colors.border);
        }
        frame.draw_text(" Commit ", x + 2, y, colors.text, None, Attributes::BOLD);

        let cursor = self.draw_message(frame, area, &layout);

        let (mark, mark_fg, label_fg) = match (self.amend, self.head.is_some()) {
            (true, _) => ("●", colors.accent, colors.text),
            (false, true) => ("○", colors.muted, colors.text),
            (false, false) => ("○", colors.faint, colors.faint),
        };
        let amend_bg = (self.focus == Field::Amend).then_some(colors.selected);
        if let Some(bg) = amend_bg {
            frame.fill_rect(x + 2, layout.amend, 2 + AMEND.len() as u32, 1, bg);
        }
        frame.draw_text(
            mark,
            x + 2,
            layout.amend,
            mark_fg,
            amend_bg,
            Attributes::NONE,
        );
        frame.draw_text(
            AMEND,
            x + 4,
            layout.amend,
            label_fg,
            amend_bg,
            Attributes::NONE,
        );

        let room = width.saturating_sub(4) as usize;
        self.draw_summary(frame, x + 2, layout.summary, room);
        let (author, author_fg) = match &self.author {
            Ok(author) => (format!("as {author}"), colors.muted),
            Err(_) => (
                "git doesn't know who you are: set user.name and user.email.".to_string(),
                colors.error,
            ),
        };
        let author = truncate(&author, room);
        frame.draw_text(
            &author,
            x + 2,
            layout.author,
            author_fg,
            None,
            Attributes::NONE,
        );

        let ready = self.plan().is_ok();
        for (field, bx, bw) in self.button_spans(area) {
            let focused = self.focus == field;
            let (label, fg, bg) = match field {
                Field::Commit if ready => (self.commit_label(), colors.on_accent, colors.accent),
                Field::Commit => (self.commit_label(), colors.faint, colors.surface),
                _ if focused => (CANCEL, colors.text, colors.selected),
                _ => (CANCEL, colors.text, colors.surface),
            };
            let attributes = match focused {
                true => Attributes::BOLD,
                false => Attributes::NONE,
            };
            frame.fill_rect(bx, layout.buttons, bw, 1, bg);
            frame.draw_text(label, bx + 1, layout.buttons, fg, Some(bg), attributes);
        }
        cursor
    }

    /// Draws, at (`x`, `y`), in `room` columns, the repository and branch
    /// committed to, and the lines added, in green, and removed, in red,
    /// or that none are.
    fn draw_summary(&self, frame: &Buffer, x: u32, y: u32, room: usize) {
        let colors = theme::colors();
        let end = x + room as u32;
        let mut at = x;
        let put = |at: &mut u32, text: &str, fg: Rgba, attributes: Attributes| {
            let text = truncate(text, end.saturating_sub(*at) as usize);
            frame.draw_text(&text, *at, y, fg, None, attributes);
            *at += text.chars().count() as u32;
        };
        put(&mut at, &self.name, colors.text, Attributes::BOLD);
        put(&mut at, "  ", colors.text, Attributes::NONE);
        if icons::enabled() && at + icons::WIDTH < end {
            at = icons::branch().draw(frame, at, y, None);
        }
        put(&mut at, &self.branch, colors.muted, Attributes::NONE);
        match self.committed_lines() {
            Some((added, removed)) => {
                let green = colors.hue(Hue::Green);
                put(&mut at, &format!("  +{added}"), green, Attributes::NONE);
                let red = colors.hue(Hue::Red);
                put(&mut at, &format!(" -{removed}"), red, Attributes::NONE);
            }
            None => put(&mut at, "  (No changes)", colors.faint, Attributes::NONE),
        }
    }

    /// The message box, bordered, with the cursor in it if the keyboard
    /// is.
    fn draw_message(&self, frame: &Buffer, area: Area, layout: &Layout) -> Option<(u32, u32)> {
        let colors = theme::colors();
        let editing = self.focus == Field::Message && !self.committing();
        let border = match editing {
            true => colors.accent,
            false => colors.border,
        };
        let (left, right) = (area.x + 2, area.x + area.width.saturating_sub(3));
        let edge = "─".repeat(area.width.saturating_sub(6) as usize);
        let top = layout.message.start - 1;
        let bottom = layout.message.end;
        frame.draw_text(
            &format!("╭{edge}╮"),
            left,
            top,
            border,
            None,
            Attributes::NONE,
        );
        frame.draw_text(
            &format!("╰{edge}╯"),
            left,
            bottom,
            border,
            None,
            Attributes::NONE,
        );
        for y in layout.message.clone() {
            frame.draw_text("│", left, y, border, None, Attributes::NONE);
            frame.draw_text("│", right, y, border, None, Attributes::NONE);
        }
        let text = &self.message.text;
        let text_x = layout.text_x;
        if text.is_empty() {
            let placeholder = truncate(PLACEHOLDER, self.room());
            let y = layout.message.start;
            frame.draw_text(
                &placeholder,
                text_x,
                y,
                colors.faint,
                None,
                Attributes::NONE,
            );
        }
        let lines = wrap(text, self.room());
        let selection = self.message.caret.selection(text);
        let shown = lines.iter().skip(self.message.scroll);
        for (line, y) in shown.zip(layout.message.clone()) {
            if let Some(selection) = &selection {
                let start = selection.start.max(line.range.start);
                let end = selection.end.min(line.range.end);
                if start < end {
                    let column = text[line.range.start..start].chars().count() as u32;
                    let count = text[start..end].chars().count().max(1) as u32;
                    frame.fill_rect(text_x + column, y, count, 1, colors.selection);
                }
            }
            let shown = text[line.range.clone()].trim_end_matches('\n');
            frame.draw_text(shown, text_x, y, colors.text, None, Attributes::NONE);
        }
        if !editing {
            return None;
        }
        let at = self.message.caret.at(text);
        let row = row_of(&lines, at);
        let column = text[lines[row].range.start..at].chars().count() as u32;
        let y = layout.message.start + (row - self.message.scroll.min(row)) as u32;
        layout.message.contains(&y).then_some((text_x + column, y))
    }
}

/// The rows the parts of the dialog are on, and the column the message's
/// text starts at.
struct Layout {
    text_x: u32,
    /// The message's rows, inside its box.
    message: Range<u32>,
    amend: u32,
    /// The first of what's committed.
    summary: u32,
    author: u32,
    buttons: u32,
}

/// `text` in rows of `room` columns: broken at line breaks, and where a
/// line's too long, after the last space that fits, or if there's none,
/// after the last character that does.
fn wrap(text: &str, room: usize) -> Vec<Line> {
    let room = room.max(1);
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        let end = start + line.len();
        let mut at = start;
        loop {
            let rest = &text[at..end];
            let Some((limit, next)) = rest.char_indices().nth(room) else {
                lines.push(Line {
                    range: at..end,
                    wraps: false,
                });
                break;
            };
            // A space just past the end can stay on the row, unseen.
            let window = &rest[..limit + next.len_utf8()];
            let cut = match window.rfind(' ') {
                Some(space) if space > 0 => space + 1,
                _ => limit,
            };
            lines.push(Line {
                range: at..at + cut,
                wraps: true,
            });
            at += cut;
        }
        start = end + 1;
    }
    lines
}

/// The row of `lines` the cursor at byte `at` is on. At the end of a row
/// that wraps, it's at the start of the next.
fn row_of(lines: &[Line], at: usize) -> usize {
    lines
        .iter()
        .rposition(|line| line.range.start <= at)
        .unwrap_or(0)
}

/// Where in `text` column `column` of `line` is, or the end of the row,
/// if it's shorter: on a row that wraps, before its last character, which
/// is where the next row starts.
fn offset_at(text: &str, line: &Line, column: usize) -> usize {
    let mut end = line.range.end;
    if line.wraps {
        end = text[..end].char_indices().next_back().map_or(0, |(i, _)| i);
    }
    let row = &text[line.range.start..end];
    line.range.start + row.char_indices().nth(column).map_or(row.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{Change, Head, Kind};
    use crate::input::Mods;

    fn repo(changes: &[(&str, Kind, Staged)]) -> Repo {
        let root = PathBuf::from("/w/cue");
        Repo {
            git_dir: root.join(".git"),
            head: Head::Branch("main".into()),
            commit: Some("0123456789abcdef".into()),
            merging: false,
            changes: changes
                .iter()
                .map(|&(path, kind, staged)| Change {
                    path: root.join(path),
                    kind,
                    from: None,
                    staged,
                })
                .collect(),
            added: 42,
            removed: 7,
            stashes: Vec::new(),
            upstream: None,
            publish_to: None,
            root,
        }
    }

    fn dialog(changes: &[(&str, Kind, Staged)]) -> CommitDialog {
        let mut dialog = CommitDialog::new(&repo(changes), "cue".into(), 60, 24);
        dialog.author = Ok("Ada <ada@example.com>".into());
        dialog.staged_lines = (12, 3);
        dialog
    }

    fn drawn(dialog: &CommitDialog) -> Vec<String> {
        let screen = opentui::OwnedBuffer::new(
            dialog.screen_width,
            dialog.screen_height,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        screen.clear(Rgba::BLACK);
        dialog.draw(&screen);
        let area = dialog.area();
        screen
            .to_text(true)
            .lines()
            .skip(area.y as usize)
            .take(area.height as usize)
            .map(|line| line.trim().to_string())
            .collect()
    }

    fn press(x: u32, y: u32) -> Mouse {
        Mouse {
            kind: MouseKind::Press(MouseButton::Left),
            x,
            y,
            mods: Mods::NONE,
        }
    }

    /// `text` as [`wrap`] has it in `room` columns, a row a line, with `\`
    /// at the end of those that wrap.
    fn wrapped(text: &str, room: usize) -> Vec<String> {
        wrap(text, room)
            .into_iter()
            .map(|line| {
                let wraps = if line.wraps { "\\" } else { "" };
                format!("{}{wraps}", &text[line.range])
            })
            .collect()
    }

    #[test]
    fn says_what_is_committed_where_and_as_whom() {
        let _serial = crate::test_serial();
        let mut dialog = dialog(&[
            ("a.rs", Kind::Modified, Staged::Yes),
            ("b.rs", Kind::Modified, Staged::No),
        ]);
        assert_eq!(
            drawn(&dialog),
            [
                "╭─ Commit ─────────────────────────────────────────────╮",
                "│ ╭──────────────────────────────────────────────────╮ │",
                "│ │ Message                                          │ │",
                "│ ╰──────────────────────────────────────────────────╯ │",
                "│ ○ Amend last commit                                  │",
                "│                                                      │",
                "│ cue  main  +12 -3                                    │",
                "│ as Ada <ada@example.com>                             │",
                "│                                                      │",
                "│                                    Cancel    Commit  │",
                "╰──────────────────────────────────────────────────────╯",
            ]
        );
        assert_eq!(dialog.plan(), Err(CantCommit::NoMessage));
        dialog.edit(Edit::Insert("Fix it\n\nAll of it, really"), false);
        assert_eq!(
            drawn(&dialog)[1..6],
            [
                "│ ╭──────────────────────────────────────────────────╮ │",
                "│ │ Fix it                                           │ │",
                "│ │                                                  │ │",
                "│ │ All of it, really                                │ │",
                "│ ╰──────────────────────────────────────────────────╯ │",
            ],
            "the box grows"
        );
        assert_eq!(dialog.plan(), Ok(Plan::Staged));
    }

    #[test]
    fn with_nothing_staged_it_commits_everything() {
        let _serial = crate::test_serial();
        let mut all = dialog(&[
            ("a.rs", Kind::Modified, Staged::No),
            ("b.rs", Kind::Untracked, Staged::No),
        ]);
        all.edit(Edit::Insert("Everything"), false);
        assert_eq!(all.plan(), Ok(Plan::All));
        let shown = drawn(&all);
        assert_eq!(
            shown[6], "│ cue  main  +42 -7                                    │",
            "all the changes'"
        );
        assert!(shown[9].contains("Cancel    Commit All"), "{shown:?}");

        let mut clean = dialog(&[]);
        clean.edit(Edit::Insert("Nothing"), false);
        assert_eq!(clean.plan(), Err(CantCommit::NothingToCommit));
        assert_eq!(
            drawn(&clean)[6],
            "│ cue  main  (No changes)                              │"
        );
        let mut repo = repo(&[]);
        repo.merging = true;
        clean.set_repo(&repo);
        assert_eq!(clean.plan(), Ok(Plan::Staged), "a merge needs no changes");
    }

    #[test]
    fn tab_goes_round_and_space_presses() {
        let mut dialog = dialog(&[("a.rs", Kind::Modified, Staged::Yes)]);
        let key = |code, mods| Key::new(code, mods);
        let tab = key(KeyCode::Tab, Mods::NONE);
        assert_eq!(dialog.handle_key(key(KeyCode::Char(' '), Mods::NONE)), None);
        assert_eq!(dialog.handle_key(tab), Some(CommitAction::Continue));
        assert!(!dialog.editing());
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char(' '), Mods::NONE)),
            Some(CommitAction::Continue)
        );
        assert!(dialog.amend, "Space toggles Amend");
        assert_eq!(dialog.handle_key(tab), Some(CommitAction::Continue));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Right, Mods::NONE)),
            Some(CommitAction::Continue)
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter, Mods::NONE)),
            Some(CommitAction::Commit)
        );
        let back = key(KeyCode::Tab, Mods::SHIFT);
        assert_eq!(dialog.handle_key(back), Some(CommitAction::Continue));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter, Mods::NONE)),
            Some(CommitAction::Close)
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc, Mods::NONE)),
            Some(CommitAction::Close)
        );
        dialog.handle_key(tab);
        dialog.handle_key(tab);
        assert!(dialog.editing(), "round to the message");
    }

    #[test]
    fn clicks_type_amend_cancel_commit_and_close() {
        let mut dialog = dialog(&[("a.rs", Kind::Modified, Staged::Yes)]);
        dialog.edit(Edit::Insert("fix"), false);
        let area = dialog.area();
        let (x, y) = (area.x, area.y);
        dialog.handle_key(Key::new(KeyCode::Tab, Mods::NONE));
        assert_eq!(
            dialog.handle_mouse(press(x + 5, y + 2)),
            CommitAction::Continue
        );
        assert!(dialog.editing());
        dialog.edit(Edit::Insert("-"), false);
        assert_eq!(dialog.message(), "f-ix");
        assert_eq!(
            dialog.handle_mouse(press(x + 6, y + 4)),
            CommitAction::Continue
        );
        assert!(dialog.amend);
        assert_eq!(dialog.message(), "f-ix", "a message written stays");
        let buttons = y + area.height - 2;
        let right = x + area.width - 3;
        assert_eq!(
            dialog.handle_mouse(press(right, buttons)),
            CommitAction::Commit
        );
        assert_eq!(
            dialog.hover_area(right, buttons).map(|at| at.width),
            Some(7),
            "“ Amend ”"
        );
        assert_eq!(
            dialog.handle_mouse(press(right - 10, buttons)),
            CommitAction::Close
        );
        assert_eq!(dialog.handle_mouse(press(0, 0)), CommitAction::Close);
    }

    #[test]
    fn switching_branches_clears_amend_but_keeps_edited_messages() {
        let mut repo = repo(&[]);
        repo.root = git::tests::repo("amend-switch");
        for edited in [false, true] {
            repo.head = Head::Branch("main".into());
            let mut dialog = CommitDialog::new(&repo, "cue".into(), 60, 24);
            dialog.toggle_amend();
            assert_eq!(dialog.message(), "first");
            if edited {
                dialog.edit(Edit::Insert("My draft"), false);
            }
            let message = dialog.message().to_string();
            dialog.set_repo(&repo);
            assert!(dialog.amend, "ordinary refreshes keep amend enabled");
            repo.head = Head::Branch("other".into());
            dialog.set_repo(&repo);
            assert!(!dialog.amend);
            assert_eq!(dialog.message(), if edited { &message } else { "" });
            assert_eq!(
                dialog.plan(),
                Err(if edited {
                    CantCommit::NothingToCommit
                } else {
                    CantCommit::NoMessage
                })
            );
        }
    }

    #[test]
    fn wraps_after_spaces_or_else_anywhere() {
        assert_eq!(wrapped("", 5), [""]);
        assert_eq!(wrapped("fix the bug", 5), ["fix \\", "the \\", "bug"]);
        assert_eq!(wrapped("abcdefgh", 3), ["abc\\", "def\\", "gh"]);
        assert_eq!(
            wrapped("abc def", 3),
            ["abc \\", "def"],
            "a space past the end stays"
        );
        assert_eq!(wrapped("one\n\ntwo\n", 5), ["one", "", "two", ""]);
    }

    #[test]
    fn the_cursor_moves_by_rows_as_they_wrap() {
        let mut dialog = dialog(&[]);
        // Room for 5 columns.
        dialog.set_size(13, 24);
        assert_eq!(dialog.room(), 5);
        dialog.edit(Edit::Insert("fix the bug\nin it"), false);
        let cursor = |dialog: &CommitDialog| {
            let text = dialog.message();
            let at = dialog.message.caret.at(text);
            format!("{}|{}", &text[..at], &text[at..])
        };
        assert_eq!(cursor(&dialog), "fix the bug\nin it|");
        dialog.move_rows(-1, false);
        assert_eq!(
            cursor(&dialog),
            "fix the bug|\nin it",
            "to the end of a shorter row"
        );
        dialog.move_rows(-1, false);
        assert_eq!(
            cursor(&dialog),
            "fix the| bug\nin it",
            "rows that wrap end before the space"
        );
        dialog.move_to_row_edge(false, false);
        assert_eq!(cursor(&dialog), "fix |the bug\nin it");
        dialog.move_to_row_edge(true, true);
        assert_eq!(dialog.selected_text(), Some("the"));
        dialog.move_rows(-1, false);
        assert_eq!(cursor(&dialog), "fix| the bug\nin it");
        dialog.move_rows(-1, false);
        assert_eq!(
            cursor(&dialog),
            "|fix the bug\nin it",
            "up from the top goes to the start"
        );
        dialog.move_rows(5, false);
        assert_eq!(cursor(&dialog), "fix the bug\nin it|");
        dialog.edit(Edit::Insert("\r\nmore"), false);
        assert_eq!(
            dialog.message(),
            "fix the bug\nin it\nmore",
            "pasted CRs are line breaks"
        );
    }

    #[test]
    fn commits_a_merge_resolved_to_the_current_tree() {
        let root = git::tests::repo("commit-merge-ours");
        git::tests::commit_as_cue(&root);
        git::switch(&root, &git::SwitchTo::New("topic".into())).unwrap();
        std::fs::write(root.join("a.txt"), "topic\n").unwrap();
        git::tests::commit_all(&root, "topic");
        git::switch(&root, &git::SwitchTo::Branch("main".into())).unwrap();
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        git::tests::commit_all(&root, "ours");
        let merge = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["merge", "topic"])
            .output()
            .unwrap();
        assert!(!merge.status.success());
        assert!(root.join(".git/MERGE_HEAD").is_file());
        std::fs::write(root.join("a.txt"), "ours\n").unwrap();
        git::stage(&root, &[root.join("a.txt")]).unwrap();
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        assert!(git.repos()[0].changes.is_empty());
        let mut dialog = CommitDialog::new(&git.repos()[0], "cue".into(), 60, 24);
        dialog.edit(Edit::Insert("Resolve using ours"), false);
        assert_eq!(dialog.plan(), Ok(Plan::Staged));
        dialog.start();
        assert_eq!(dialog.plan(), Err(CantCommit::Committing));
        dialog.wait().unwrap().unwrap();
        assert!(!root.join(".git/MERGE_HEAD").exists());
        let parents = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-list", "--parents", "-1", "HEAD"])
            .output()
            .unwrap();
        assert!(parents.status.success());
        assert_eq!(
            String::from_utf8_lossy(&parents.stdout)
                .split_whitespace()
                .count(),
            3,
            "the commit has two parents"
        );
        git.restart();
        git.wait();
        dialog.set_repo(&git.repos()[0]);
        dialog.edit(Edit::Insert("No further changes"), false);
        assert_eq!(dialog.plan(), Err(CantCommit::NothingToCommit));
    }

    #[test]
    fn reads_who_commits() {
        let root = git::tests::repo("commit-author");
        let mut git = git::Git::new(std::slice::from_ref(&root));
        git.wait();
        let mut dialog = CommitDialog::new(&git.repos()[0], "cue".into(), 60, 24);
        dialog.read();
        let author = dialog.author.clone().unwrap();
        assert!(author.ends_with('>') && author.contains(" <"), "{author}");
    }
}
