//! A panel: one area of the layout, showing an open file, an image, a
//! terminal, how a commit changed a file, or nothing yet.
//!
//! Panels have no tabs. Opening a file in a panel replaces what it shows,
//! but the panel keeps an editor for each file it has shown, so going back
//! to one finds the cursor and scroll position where they were. Files and
//! terminals stay open when a panel moves on from them; the app owns them,
//! and closing a panel closes only what it shows.
//!
//! A header across the top names the file or terminal, brighter on the
//! active panel, with buttons at its right end to go back, go forward, and
//! close the panel. An empty panel, as a new split starts, lists how to open
//! something.
//!
//! Like a browser tab, a panel keeps a history of what it showed, to go
//! back and forward through (Ctrl+- and Ctrl+=). Popping (Ctrl+0) closes
//! what it shows and goes back, as from the top of a stack.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Instant;

use opentui::{Attributes, Buffer};

use crate::commit_diff::CommitDiff;
use crate::document::{Disk, Document};
use crate::editor::Editor;
use crate::git::{Change, Commit};
use crate::icons::{self, Icon};
use crate::image::ImageView;
use crate::input::{Mouse, MouseKind};
use crate::keymap::{Command, Keymap};
use crate::layout::{PanelId, Rect};
use crate::status::Status;
use crate::terminal::{self, Terminal};
use crate::theme;
use crate::workspace::Workspace;

/// What an empty panel suggests.
const SUGGESTIONS: &[Command] = &[Command::GoToFile, Command::NewFile, Command::NewTerminal];
/// How far back a panel's history goes.
const HISTORY: usize = 50;
/// The header's buttons, left to right, and their labels.
const BUTTONS: [(HeaderButton, &str); 3] = [
    (HeaderButton::Back, " < "),
    (HeaderButton::Forward, " > "),
    (HeaderButton::Close, " × "),
];
/// Headers narrower than this leave the buttons out, for the name.
const MIN_BUTTONS_WIDTH: u32 = 24;
/// Headers narrower than this leave out the buttons that go to and from
/// reader and diff mode.
const MIN_MODE_BUTTON_WIDTH: u32 = 32;
/// Headers narrower than this have room for one of them only: the one to
/// leave the mode the editor's in, or otherwise the diff's.
const MIN_MODE_BUTTONS_WIDTH: u32 = 40;

/// A button at the right end of a panel's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderButton {
    Back,
    Forward,
    Close,
    /// Goes to reader mode, or back to editing, on a Markdown file.
    Reader,
    /// Goes to diff mode, or back to editing, on a file with changes.
    Diff,
}

/// Something a panel showed, to go back or forward to.
#[derive(Debug, Clone)]
pub enum Visit {
    /// A file: its document while it's open, and its path, to open it
    /// again once it's closed.
    File(Weak<Document>, Option<PathBuf>),
    /// A terminal, by id.
    Terminal(u32),
    /// An image file.
    Image(PathBuf),
    /// How a commit, in the repository at `root`, changed a file.
    Commit {
        root: PathBuf,
        commit: Commit,
        change: Change,
    },
}

impl Visit {
    fn is(&self, other: &Visit) -> bool {
        match (self, other) {
            (Visit::File(a, a_path), Visit::File(b, b_path)) => {
                a.ptr_eq(b) || (a_path.is_some() && a_path == b_path)
            }
            (Visit::Terminal(a), Visit::Terminal(b)) => a == b,
            (Visit::Image(a), Visit::Image(b)) => a == b,
            (
                Visit::Commit {
                    root,
                    commit,
                    change,
                },
                Visit::Commit {
                    root: b_root,
                    commit: b_commit,
                    change: b_change,
                },
            ) => (root, &commit.hash, &change.path) == (b_root, &b_commit.hash, &b_change.path),
            _ => false,
        }
    }
}

/// What a panel showed before what it shows now, most recent last, and
/// what it went back from, most recent last.
#[derive(Debug, Default)]
pub struct History {
    back: Vec<Visit>,
    forward: Vec<Visit>,
}

pub struct Panel {
    pub id: PanelId,
    /// An editor for each file shown, most recently opened last.
    editors: Vec<Editor>,
    /// The editor on screen; `None` while empty or showing a terminal or
    /// an image.
    current: Option<usize>,
    /// The terminal on screen, if any.
    terminal: Option<Rc<RefCell<Terminal>>>,
    /// The image on screen, if any. Unlike files and terminals, the panel
    /// owns it: it's gone once the panel moves on.
    image: Option<ImageView>,
    /// How a commit changed a file, if that's on screen. The panel owns
    /// it, as it does an image.
    commit: Option<CommitDiff>,
    /// While empty, a message for the status bar until the next key press.
    message: Option<(String, bool)>,
    area: Rect,
    history: History,
}

impl Panel {
    pub fn new(id: PanelId) -> Panel {
        Panel {
            id,
            editors: Vec::new(),
            current: None,
            terminal: None,
            image: None,
            commit: None,
            message: None,
            area: Rect::default(),
            history: History::default(),
        }
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        let body = self.body();
        for editor in &mut self.editors {
            editor.set_area(body.x, body.y, body.width, body.height);
        }
        if let Some(terminal) = &self.terminal {
            terminal.borrow_mut().set_area(body);
        }
        if let Some(commit) = &mut self.commit {
            commit.set_size(body.width, body.height);
        }
    }

    /// The area below the header.
    pub fn body(&self) -> Rect {
        let area = self.area;
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width.max(1),
            height: area.height.saturating_sub(1),
        }
    }

    /// The editor on screen, if any.
    pub fn editor(&self) -> Option<&Editor> {
        self.editors.get(self.current?)
    }

    /// The editor on screen, if any, with the buffer's cursor.
    pub fn editor_mut(&mut self) -> Option<&mut Editor> {
        let editor = self.editors.get_mut(self.current?)?;
        editor.attach();
        Some(editor)
    }

    /// Scrolls the editor on screen along with a view of `doc` in the
    /// other mode (see [`Editor::follow`]), if it shows `doc` in reader
    /// mode and `reading` is false, or the other way around.
    pub fn follow(&mut self, doc: &Rc<Document>, reading: bool, anchor: (u32, u32)) {
        if !self.shows(doc) {
            return;
        }
        let Some(editor) = self.current.and_then(|i| self.editors.get_mut(i)) else {
            return;
        };
        if editor.reading() != reading && !editor.diffing() {
            editor.follow(anchor);
            // Moved along, it didn't move of its own accord.
            editor.moved();
        }
    }

    /// A link clicked in the editor on screen, in reader mode, to follow.
    pub fn take_link(&mut self) -> Option<String> {
        let editor = self.editors.get_mut(self.current?)?;
        editor.take_link()
    }

    /// The terminal on screen, if any.
    /// Every editor it has, shown or not.
    pub fn editors_mut(&mut self) -> impl Iterator<Item = &mut Editor> {
        self.editors.iter_mut()
    }

    pub fn terminal(&self) -> Option<&Rc<RefCell<Terminal>>> {
        self.terminal.as_ref()
    }

    /// Shows `terminal`, sized to fit. An unnamed document left behind that
    /// was never typed in is dropped.
    pub fn show_terminal(&mut self, terminal: Rc<RefCell<Terminal>>) {
        let id = terminal.borrow().id();
        if !matches!(self.visit(), Some(Visit::Terminal(shown)) if shown == id) {
            self.leave();
        }
        terminal.borrow_mut().set_area(self.body());
        self.terminal = Some(terminal);
        self.image = None;
        self.commit = None;
        self.leave_editor();
    }

    /// The image on screen, if any.
    pub fn image(&self) -> Option<&ImageView> {
        self.image.as_ref()
    }

    pub fn image_mut(&mut self) -> Option<&mut ImageView> {
        self.image.as_mut()
    }

    /// Shows `image`. An unnamed document left behind that was never typed
    /// in is dropped.
    pub fn show_image(&mut self, image: ImageView) {
        if !matches!(self.visit(), Some(Visit::Image(shown)) if shown == image.path()) {
            self.leave();
        }
        self.image = Some(image);
        self.terminal = None;
        self.commit = None;
        self.leave_editor();
    }

    /// Stops showing an image, leaving the panel empty.
    pub fn hide_image(&mut self) {
        self.leave();
        self.image = None;
    }

    /// How a commit changed a file, if that's on screen.
    pub fn commit(&self) -> Option<&CommitDiff> {
        self.commit.as_ref()
    }

    pub fn commit_mut(&mut self) -> Option<&mut CommitDiff> {
        self.commit.as_mut()
    }

    /// Shows how a commit changed a file. An unnamed document left behind
    /// that was never typed in is dropped.
    pub fn show_commit(&mut self, mut commit: CommitDiff) {
        let same = matches!(&self.commit, Some(shown)
            if shown.is(commit.root(), &commit.commit().hash, &commit.change().path));
        if !same {
            self.leave();
        }
        let body = self.body();
        commit.set_size(body.width, body.height);
        self.commit = Some(commit);
        self.image = None;
        self.terminal = None;
        self.leave_editor();
    }

    /// Stops showing how a commit changed a file, leaving the panel empty.
    pub fn hide_commit(&mut self) {
        self.leave();
        self.commit = None;
    }

    /// Takes the editor off screen, for a terminal or an image, dropping it
    /// if it's of an unnamed document that was never typed in.
    fn leave_editor(&mut self) {
        self.message = None;
        if let Some(left) = self.current.take() {
            if self.editors[left].is_blank() {
                self.editors.remove(left);
            }
        }
    }

    /// Stops showing a terminal, leaving the panel empty.
    pub fn hide_terminal(&mut self) {
        self.leave();
        self.terminal = None;
    }

    /// The document on screen, if any.
    pub fn document(&self) -> Option<&Rc<Document>> {
        self.editor().map(Editor::document)
    }

    /// Whether the panel shows nothing.
    pub fn is_empty(&self) -> bool {
        self.current.is_none()
            && self.terminal.is_none()
            && self.image.is_none()
            && self.commit.is_none()
    }

    /// What's on screen, briefly, as the tab bar names it: the file's name,
    /// or the name the terminal was given, or else the program running in
    /// it, as tmux names windows. (Shells set titles such as `user@host:~`.)
    /// `None` while empty.
    pub fn title(&self) -> Option<String> {
        if let Some(terminal) = &self.terminal {
            let terminal = terminal.borrow();
            return Some(match terminal.program() {
                Some(program) if !terminal.is_renamed() => program,
                _ => terminal.name(),
            });
        }
        if let Some(commit) = &self.commit {
            let path = &commit.change().path;
            let name = path.file_name().unwrap_or(path.as_os_str());
            return Some(format!(
                "{} @ {}",
                name.to_string_lossy(),
                commit.commit().short
            ));
        }
        let path = match (&self.image, self.document()) {
            (Some(image), _) => image.path().to_path_buf(),
            (None, Some(doc)) => match doc.path() {
                Some(path) => path,
                None => return Some(doc.untitled_name().unwrap_or_default()),
            },
            (None, None) => return None,
        };
        Some(path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ))
    }

    /// The path of the file or image on screen, if any.
    pub fn path(&self) -> Option<PathBuf> {
        match &self.image {
            Some(image) => Some(image.path().to_path_buf()),
            None => self.document()?.path(),
        }
    }

    /// Whether `doc` is on screen here.
    pub fn shows(&self, doc: &Rc<Document>) -> bool {
        self.document().is_some_and(|shown| Rc::ptr_eq(shown, doc))
    }

    /// Whether the panel has an editor of `doc`, on screen or not.
    pub fn has(&self, doc: &Rc<Document>) -> bool {
        self.editors
            .iter()
            .any(|editor| Rc::ptr_eq(editor.document(), doc))
    }

    /// Shows `doc`, where it was left if it was shown here before. An
    /// unnamed document left behind that was never typed in is dropped.
    pub fn show(&mut self, doc: &Rc<Document>) -> opentui::Result<()> {
        if !self.shows(doc) {
            self.leave();
        }
        let index = match self
            .editors
            .iter()
            .position(|editor| Rc::ptr_eq(editor.document(), doc))
        {
            Some(index) => index,
            None => {
                let body = self.body();
                let mut editor = Editor::show(doc.clone(), body.width, body.height)?;
                editor.set_area(body.x, body.y, body.width, body.height);
                self.editors.push(editor);
                self.editors.len() - 1
            }
        };
        let left = self.current.replace(index);
        self.terminal = None;
        self.image = None;
        self.commit = None;
        self.message = None;
        if let Some(left) = left.filter(|&left| left != index && self.editors[left].is_blank()) {
            self.editors.remove(left);
            if index > left {
                self.current = Some(index - 1);
            }
        }
        if let Some(editor) = self.editor_mut() {
            editor.clear_message();
        }
        Ok(())
    }

    /// Drops the editor of `doc`. If it was on screen, the panel is left
    /// empty.
    pub fn forget(&mut self, doc: &Rc<Document>) {
        let Some(index) = self
            .editors
            .iter()
            .position(|editor| Rc::ptr_eq(editor.document(), doc))
        else {
            return;
        };
        if self.current == Some(index) {
            self.leave();
        }
        self.editors.remove(index);
        self.current = match self.current {
            Some(current) if current == index => None,
            Some(current) if current > index => Some(current - 1),
            current => current,
        };
    }

    /// Empties the panel, dropping its editors. Its history stays.
    pub fn clear(&mut self) {
        self.leave();
        self.editors.clear();
        self.current = None;
        self.terminal = None;
        self.image = None;
        self.commit = None;
        self.message = None;
    }

    // --- history ------------------------------------------------------------------

    /// What's on screen, as the history keeps it. An unnamed document that
    /// was never typed in isn't worth going back to.
    pub fn visit(&self) -> Option<Visit> {
        if let Some(terminal) = &self.terminal {
            return Some(Visit::Terminal(terminal.borrow().id()));
        }
        if let Some(image) = &self.image {
            return Some(Visit::Image(image.path().to_path_buf()));
        }
        if let Some(commit) = &self.commit {
            return Some(Visit::Commit {
                root: commit.root().to_path_buf(),
                commit: commit.commit().clone(),
                change: commit.change().clone(),
            });
        }
        let doc = self.document().filter(|doc| !doc.is_blank())?;
        Some(Visit::File(Rc::downgrade(doc), doc.path()))
    }

    /// Notes what's on screen, which is about to go, as the place to go
    /// back to. Going somewhere new, there's no going forward.
    fn leave(&mut self) {
        let Some(visit) = self.visit() else {
            return;
        };
        let History { back, forward } = &mut self.history;
        forward.clear();
        back.retain(|old| !old.is(&visit));
        back.push(visit);
        if back.len() > HISTORY {
            back.remove(0);
        }
    }

    /// Goes back through the history (or forward, if not `back`): takes the
    /// last place there that `usable` accepts, and puts what's on screen
    /// on the other side, for going the other way. Places it doesn't
    /// accept, which are gone, are dropped. The caller shows the place,
    /// with the history taken out meanwhile (see [`Panel::take_history`]).
    pub fn step_history(&mut self, back: bool, usable: impl Fn(&Visit) -> bool) -> Option<Visit> {
        let current = self.visit();
        let History {
            back: behind,
            forward: ahead,
        } = &mut self.history;
        let (from, to) = if back {
            (behind, ahead)
        } else {
            (ahead, behind)
        };
        let visit = loop {
            let visit = from.pop()?;
            let here = current.as_ref().is_some_and(|current| current.is(&visit));
            if !here && usable(&visit) {
                break visit;
            }
        };
        to.extend(current);
        Some(visit)
    }

    /// Drops `visit` from the places to go back to, as when it was closed
    /// for good.
    pub fn drop_visit(&mut self, visit: &Visit) {
        self.history.back.retain(|old| !old.is(visit));
    }

    /// Whether there's a place to go back to (or forward to, if not
    /// `back`). It may turn out to be gone.
    pub fn can_go(&self, back: bool) -> bool {
        match back {
            true => !self.history.back.is_empty(),
            false => !self.history.forward.is_empty(),
        }
    }

    /// Every editor's document and place in it (see [`Editor::place`]),
    /// and in reader mode, the file line at the top (see
    /// [`Editor::reading_line`]), most recently opened last.
    pub fn places(&self) -> impl Iterator<Item = (&Rc<Document>, (u32, u32, u32), Option<u32>)> {
        self.editors
            .iter()
            .map(|editor| (editor.document(), editor.place(), editor.reading_line()))
    }

    /// What it showed before, most recent last, and went back from.
    pub fn history(&self) -> (&[Visit], &[Visit]) {
        (&self.history.back, &self.history.forward)
    }

    /// Takes a history kept from before, as [`Panel::history`] gave it.
    pub fn set_history(&mut self, back: Vec<Visit>, forward: Vec<Visit>) {
        self.history = History { back, forward };
    }

    /// Takes whatever it shows off screen, leaving it empty but keeping
    /// its editors, without noting it in the history.
    pub fn show_nothing(&mut self) {
        self.leave_editor();
        self.terminal = None;
        self.image = None;
        self.commit = None;
    }

    /// Takes the history out, so that showing a place from it doesn't
    /// count as going somewhere new, until it's put back.
    pub fn take_history(&mut self) -> History {
        std::mem::take(&mut self.history)
    }

    pub fn restore_history(&mut self, history: History) {
        self.history = history;
    }

    /// Shows `text` in the status bar until the next key press.
    pub fn show_message(&mut self, text: String, error: bool) {
        match self.editor_mut() {
            Some(editor) => editor.show_message(text, error),
            None => self.message = Some((text, error)),
        }
    }

    pub fn clear_message(&mut self) {
        self.message = None;
        if let Some(editor) = self.current.and_then(|i| self.editors.get_mut(i)) {
            editor.clear_message();
        }
    }

    /// What the status bar shows while this panel is active.
    pub fn status(&self) -> Status {
        if let (Some(terminal), None) = (&self.terminal, &self.message) {
            return terminal.borrow().status();
        }
        if let (Some(image), None) = (&self.image, &self.message) {
            return image.status(self.body());
        }
        if let (Some(commit), None) = (&self.commit, &self.message) {
            return commit.status();
        }
        match (self.editor(), &self.message) {
            (Some(editor), _) => editor.status(),
            (None, Some((text, error))) => Status::Message {
                text: text.clone(),
                error: *error,
            },
            (None, None) => Status::Info(String::new()),
        }
    }

    /// A mouse event at screen cell (`mouse.x`, `mouse.y`), in the panel or
    /// dragged from it. Clicks on the header don't reach the text.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) {
        let body = self.body();
        if mouse.y < body.y && matches!(mouse.kind, MouseKind::Press(_)) {
            return;
        }
        if let Some(terminal) = &self.terminal {
            terminal.borrow_mut().handle_mouse(mouse);
            return;
        }
        if let Some(image) = &mut self.image {
            image.handle_mouse(mouse, body);
            return;
        }
        let local = Mouse {
            x: mouse.x.saturating_sub(body.x),
            y: mouse.y.saturating_sub(body.y),
            ..mouse
        };
        if let Some(commit) = &mut self.commit {
            commit.handle_mouse(local, now);
            return;
        }
        if let Some(editor) = self.editor_mut() {
            editor.handle_mouse(local, now);
        }
    }

    /// Draws the panel in its area, and returns where the terminal cursor
    /// goes. The `active` panel has the keyboard, or gets it back from the
    /// file tree. A `preview`'s name is in italics, as in the file tree.
    pub fn draw(
        &self,
        frame: &Buffer,
        keymap: &Keymap,
        workspace: &Workspace,
        active: bool,
        preview: bool,
    ) -> Option<(u32, u32)> {
        let area = self.area;
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            if let Some(terminal) = &self.terminal {
                self.draw_terminal_header(frame, &terminal.borrow(), active);
                self.draw_buttons(frame, active);
                let body = self.body();
                return frame.with_clip(body.x, body.y, body.width, body.height, || {
                    terminal.borrow().draw(frame, active, keymap)
                });
            }
            self.draw_header(frame, workspace, active, preview);
            self.draw_buttons(frame, active);
            if let Some(image) = &self.image {
                let body = self.body();
                frame.with_clip(body.x, body.y, body.width, body.height, || {
                    image.draw(frame, body)
                });
                return None;
            }
            if let Some(commit) = &self.commit {
                let body = self.body();
                frame.with_clip(body.x, body.y, body.width, body.height, || {
                    commit.draw(frame, body)
                });
                return None;
            }
            match self.editor() {
                // Reader mode has no cursor.
                Some(editor) => editor.draw(frame, keymap),
                None => {
                    self.draw_empty(frame, keymap);
                    None
                }
            }
        })
    }

    /// The header's buttons, with the screen column each starts at, if the
    /// header is wide enough for them. A Markdown file's has one to go to
    /// reader mode, or back to editing, first, and a changed file's one to
    /// go to diff mode, or back.
    fn buttons(&self) -> Vec<(HeaderButton, &'static str, u32)> {
        let area = self.area;
        if area.width < MIN_BUTTONS_WIDTH {
            return Vec::new();
        }
        let mut buttons = Vec::new();
        if let Some(editor) = self
            .editor()
            .filter(|_| area.width >= MIN_MODE_BUTTON_WIDTH)
        {
            let reader = editor.is_markdown().then(|| match editor.reading() {
                true => (HeaderButton::Reader, " Edit "),
                false => (HeaderButton::Reader, " Read "),
            });
            let diff = editor.has_diff().then(|| match editor.diffing() {
                true => (HeaderButton::Diff, " Edit "),
                false => (HeaderButton::Diff, " Diff "),
            });
            match (reader, diff) {
                (Some(reader), Some(_))
                    if area.width < MIN_MODE_BUTTONS_WIDTH && editor.reading() =>
                {
                    buttons.push(reader)
                }
                (Some(_), Some(diff)) if area.width < MIN_MODE_BUTTONS_WIDTH => buttons.push(diff),
                (reader, diff) => buttons.extend(reader.into_iter().chain(diff)),
            }
        }
        buttons.extend(BUTTONS);
        let width: u32 = buttons.iter().map(|(_, label)| label_width(label)).sum();
        let mut x = (area.x + area.width).saturating_sub(width);
        buttons
            .into_iter()
            .map(|(button, label)| {
                let start = x;
                x += label_width(label);
                (button, label, start)
            })
            .collect()
    }

    /// The header's columns left of its buttons.
    fn title_width(&self) -> u32 {
        let buttons: u32 = self
            .buttons()
            .iter()
            .map(|(_, label, _)| label_width(label))
            .sum();
        self.area.width.saturating_sub(buttons)
    }

    /// The header button at screen column `x`, if any.
    pub fn header_button(&self, x: u32) -> Option<HeaderButton> {
        self.buttons()
            .into_iter()
            .find(|&(_, label, start)| (start..start + label_width(label)).contains(&x))
            .map(|(button, ..)| button)
    }

    /// Draws the header's buttons over it, in its color, and dimmer where
    /// there's nothing to go back or forward to.
    fn draw_buttons(&self, frame: &Buffer, active: bool) {
        let colors = theme::colors();
        let fg = if active { colors.text } else { colors.muted };
        for (button, label, x) in self.buttons() {
            let fg = match button {
                HeaderButton::Back if !self.can_go(true) => colors.border,
                HeaderButton::Forward if !self.can_go(false) => colors.border,
                _ => fg,
            };
            frame.draw_text(label, x, self.area.y, fg, None, Attributes::NONE);
        }
    }

    /// The title the program set, or the program running, then dimmed,
    /// whether the shell exited.
    fn draw_terminal_header(&self, frame: &Buffer, terminal: &Terminal, active: bool) {
        let colors = theme::colors();
        let area = self.area;
        let (bg, fg) = if active {
            (colors.surface, colors.text)
        } else {
            (colors.surface_inactive, colors.muted)
        };
        frame.fill_rect(area.x, area.y, area.width, 1, bg);
        let width = self.title_width();
        let title = terminal.title();
        let name = if !title.trim().is_empty() {
            title
        } else {
            terminal.program().unwrap_or_else(|| terminal.name())
        };
        let icon = header_icon(frame, icons::terminal(), area, active);
        let room = width.saturating_sub(2 + icon) as usize;
        let name = truncate_left(&name, room);
        frame.draw_text(&name, area.x + 1 + icon, area.y, fg, None, Attributes::BOLD);
        if let Some(status) = terminal.exit() {
            let used = (1 + icon) as usize + name.chars().count() + 2;
            let note = format!("[{}]", terminal::describe_exit(status).to_lowercase());
            if used + note.chars().count() < width as usize {
                let x = area.x + used as u32;
                frame.draw_text(&note, x, area.y, colors.muted, None, Attributes::NONE);
            }
        }
    }

    /// The file's name, with [+] if it has unsaved changes and a note if
    /// it changed on disk meanwhile or is gone, then dimmed, the folder
    /// it's in. Images have only the name and folder, and a file as a
    /// commit changed it the commit after its name.
    fn draw_header(&self, frame: &Buffer, workspace: &Workspace, active: bool, preview: bool) {
        let colors = theme::colors();
        let area = self.area;
        let (bg, fg) = if active {
            (colors.surface, colors.text)
        } else {
            (colors.surface_inactive, colors.muted)
        };
        frame.fill_rect(area.x, area.y, area.width, 1, bg);
        let width = self.title_width();
        let (path, untitled, notes) = match (&self.commit, &self.image, self.document()) {
            (Some(commit), _, _) => {
                let notes = format!(" @ {}", commit.commit().short);
                (Some(commit.change().path.clone()), None, notes)
            }
            (None, Some(image), _) => (Some(image.path().to_path_buf()), None, String::new()),
            (None, None, Some(doc)) => {
                let dirty = if doc.is_modified() { " [+]" } else { "" };
                let disk = match doc.disk() {
                    Disk::Same => "",
                    Disk::Changed => " [changed on disk]",
                    Disk::Deleted => " [deleted]",
                };
                (doc.path(), doc.untitled_name(), format!("{dirty}{disk}"))
            }
            (None, None, None) => {
                frame.draw_text(
                    " No file",
                    area.x,
                    area.y,
                    colors.muted,
                    None,
                    Attributes::NONE,
                );
                return;
            }
        };
        let (name, folder) = match path {
            Some(path) => {
                let shown = workspace.display_path(&path);
                let shown = Path::new(&shown);
                let name = shown.file_name().map_or_else(
                    || shown.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                let folder = shown
                    .parent()
                    .map(|folder| folder.display().to_string())
                    .unwrap_or_default();
                (name, folder)
            }
            None => (untitled.unwrap_or_default(), String::new()),
        };
        let icon = header_icon(frame, icons::file(&name), area, active);
        let room = width.saturating_sub(2 + icon) as usize;
        let name = truncate_left(&format!("{name}{notes}"), room);
        let mut attributes = Attributes::BOLD;
        if preview {
            attributes |= Attributes::ITALIC;
        }
        frame.draw_text(&name, area.x + 1 + icon, area.y, fg, None, attributes);
        let used = (1 + icon) as usize + name.chars().count() + 2;
        let room = (width as usize).saturating_sub(used + 1);
        if !folder.is_empty() && room > 1 {
            let folder = truncate_left(&folder, room);
            let x = area.x + used as u32;
            frame.draw_text(&folder, x, area.y, colors.muted, None, Attributes::NONE);
        }
    }

    /// Lists shortcuts to open something, centered below the header.
    fn draw_empty(&self, frame: &Buffer, keymap: &Keymap) {
        let colors = theme::colors();
        let body = self.body();
        let lines: Vec<(&str, String)> = SUGGESTIONS
            .iter()
            .map(|&command| {
                let key = keymap
                    .shortcut(command)
                    .map_or(String::new(), |key| key.to_string());
                (command.title(), key)
            })
            .collect();
        let title_width = lines
            .iter()
            .map(|(title, _)| title.len())
            .max()
            .unwrap_or(0);
        let key_width = lines.iter().map(|(_, key)| key.len()).max().unwrap_or(0);
        let width = (title_width + 3 + key_width) as u32;
        if body.height >= lines.len() as u32 && body.width > width {
            let x = body.x + (body.width - width) / 2;
            let y = body.y + (body.height - lines.len() as u32) / 2;
            for (i, (title, key)) in lines.iter().enumerate() {
                let y = y + i as u32;
                frame.draw_text(title, x, y, colors.muted, None, Attributes::NONE);
                let key_x = x + width - key.len() as u32;
                frame.draw_text(key, key_x, y, colors.text, None, Attributes::NONE);
            }
        }
    }
}

/// The last `max` characters of `s`, marked with a leading ellipsis if cut.
/// Draws `icon` at the start of a header, dimmed unless the panel is
/// `active`, if icons are shown and there's room. Returns the columns it
/// took.
fn header_icon(frame: &Buffer, icon: Icon, area: Rect, active: bool) -> u32 {
    if !icons::enabled() || area.width < 1 + 3 * icons::WIDTH {
        return 0;
    }
    let dim = (!active).then(|| theme::colors().muted);
    icon.draw(frame, area.x + 1, area.y, dim);
    icons::WIDTH
}

fn label_width(label: &str) -> u32 {
    label.chars().count() as u32
}

fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("…{tail}")
}
