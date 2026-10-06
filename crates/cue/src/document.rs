//! Open files, and reading and writing them.
//!
//! A [`Document`] is an open file's text, undo history, and highlighting.
//! Every editor showing the file shares it, so an edit in one panel shows
//! in the others; each editor keeps its own cursor, selection, and scroll.
//!
//! The native edit buffer splits lines on `\n`, `\r\n`, and `\r`, and returns
//! text joined with `\n`. A file's line ending is detected on load and
//! restored on save, so CRLF files stay CRLF.
//!
//! A document remembers the file as it last loaded or saved it, to tell
//! when something else changes it (see [`Document::check_disk`]).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use opentui::{EditBuffer, LogicalCursor, WidthMethod};

use crate::config;
use crate::git::Tracked;
use crate::history::{EditKind, History};
use crate::indent::Indent;
use crate::language::{self, Language};
use crate::syntax::Highlighter;
use crate::theme::{self, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// The file a document is saved to.
pub struct File {
    /// `None` until the document is first saved.
    pub path: Option<PathBuf>,
    pub line_ending: LineEnding,
}

/// The file on disk as last read or written, to tell whether it changed
/// since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    /// Of the contents, for when only the time changed, or it changed to
    /// what it was.
    hash: u64,
}

/// A hash of a file's contents, as documents keep of what's on disk (see
/// [`Document::disk_hash`]).
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

impl Stamp {
    fn new(meta: &fs::Metadata, bytes: &[u8]) -> Stamp {
        Stamp {
            modified: meta.modified().ok(),
            len: meta.len(),
            hash: content_hash(bytes),
        }
    }

    /// Whether a file with `meta` is surely this one, without reading it.
    fn matches(&self, meta: &fs::Metadata) -> bool {
        self.modified.is_some() && self.modified == meta.modified().ok() && self.len == meta.len()
    }
}

/// How the file on disk compares with the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Disk {
    /// As the document last loaded or saved it.
    #[default]
    Same,
    /// Changed since, while the document had unsaved changes: saving asks
    /// whether to overwrite it.
    Changed,
    /// Gone since; saving writes it again.
    Deleted,
}

/// What [`Document::check_disk`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskChange {
    /// The file changed, and the document, with nothing unsaved, took the
    /// change.
    Reloaded,
    /// The file changed, but the document has unsaved changes.
    Conflict,
    Deleted,
    /// The file is back as the document last had it.
    Restored,
}

/// The file as the document last loaded or saved it, and how it compares.
#[derive(Default)]
struct OnDisk {
    /// `None` if there was no file.
    stamp: Option<Stamp>,
    disk: Disk,
}

/// The text as last loaded or saved, so that edits which put it back, like
/// typing a character and deleting it, leave nothing unsaved.
#[derive(Default)]
struct Saved {
    /// Its length and [`content_hash`]; `None` if unknown.
    text: Option<(usize, u64)>,
    /// The buffer's content epoch when last compared, and whether it
    /// differed then.
    compared: Option<(u64, bool)>,
}

impl Saved {
    fn of(text: &str) -> Saved {
        Saved {
            text: Some((text.len(), content_hash(text.as_bytes()))),
            compared: None,
        }
    }
}

/// An open file, shared by the editors showing it.
pub struct Document {
    pub buffer: Rc<EditBuffer>,
    pub file: RefCell<File>,
    /// What the file is written in, if known.
    pub language: Cell<Option<&'static Language>>,
    language_override: Cell<bool>,
    /// What Tab inserts: inferred from the text as it was opened.
    pub indent: Cell<Indent>,
    /// Highlights the text on screen as it's drawn, if cue knows how.
    pub syntax: RefCell<Option<Highlighter>>,
    pub history: RefCell<History>,
    pub theme: Rc<Theme>,
    /// Tells documents not yet saved apart: 1 for `Untitled-1`, and so on.
    pub untitled: Cell<u32>,
    /// The editor whose cursor the buffer holds, if any.
    pub cursor_owner: Cell<Option<u64>>,
    /// The other editors' cursors, by editor, while they wait for it.
    parked: RefCell<HashMap<u64, Parked>>,
    /// The text the parked cursors point into, and its content epoch.
    parked_text: RefCell<(u64, String)>,
    on_disk: RefCell<OnDisk>,
    saved: RefCell<Saved>,
    /// What git says of the file, if it's in a repository, as the app last
    /// heard (see [`Document::set_tracked`]).
    tracked: RefCell<Option<Rc<Tracked>>>,
}

/// A new id, for an editor or a [`Spot`]: none is used twice.
pub fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A place in a document's text to go back to, which moves along with
/// edits as parked cursors do. Once the document is closed, it stays where
/// it was last seen.
#[derive(Debug)]
pub struct Spot {
    doc: Weak<Document>,
    id: u64,
    seen: Cell<(u32, u32)>,
}

impl Spot {
    /// The place at `row` and `col` in `doc`, or as near as the text has.
    pub fn new(doc: &Rc<Document>, row: u32, col: u32) -> Spot {
        let id = next_id();
        let buffer = &doc.buffer;
        let row = row.min(buffer.line_count().saturating_sub(1));
        // Past the end of the line, its start.
        let offset = match buffer.position_to_offset(row, col) {
            0 => buffer.position_to_offset(row, 0),
            offset => offset,
        };
        if let Some(cursor) = buffer.offset_to_position(offset) {
            doc.keep(id, cursor);
        }
        let seen = doc.parked(id).map_or((row, col), |p| (p.row, p.col));
        Spot {
            doc: Rc::downgrade(doc),
            id,
            seen: Cell::new(seen),
        }
    }

    /// The place at `row` and `col` in a file that isn't open.
    pub fn closed(row: u32, col: u32) -> Spot {
        Spot {
            doc: Weak::new(),
            id: next_id(),
            seen: Cell::new((row, col)),
        }
    }

    /// Its row and column.
    pub fn at(&self) -> (u32, u32) {
        if let Some(parked) = self.doc.upgrade().and_then(|doc| doc.parked(self.id)) {
            self.seen.set((parked.row, parked.col));
        }
        self.seen.get()
    }
}

impl Drop for Spot {
    fn drop(&mut self) {
        if let Some(doc) = self.doc.upgrade() {
            doc.forget(self.id);
        }
    }
}

/// An editor's cursor while another editor of the same document has the
/// buffer's. It moves along with edits made meanwhile (see
/// [`Document::follow_edits`]).
#[derive(Debug, Clone, Copy)]
pub struct Parked {
    /// Where it is in the text, in bytes.
    byte: usize,
    pub row: u32,
    pub col: u32,
    /// The buffer's content epoch when parked: if it changed, the text
    /// under the editor's selection may have moved.
    pub epoch: u64,
}

impl Document {
    /// Opens the file at `path`, or a new, unnamed document, and a notice
    /// for the status bar if loading it had anything to say. The error is
    /// the reason alone, without the path.
    pub fn open(
        path: Option<PathBuf>,
        theme: Rc<Theme>,
    ) -> Result<(Rc<Document>, Option<String>), String> {
        let loaded = match &path {
            Some(path) => Some(load(path).map_err(|e| e.to_string())?),
            None => None,
        };
        let buffer = Rc::new(EditBuffer::new(WidthMethod::Unicode).map_err(|e| e.to_string())?);
        buffer.set_tab_width(config::get().tab_width as u8);
        let mut file = File {
            path,
            line_ending: Default::default(),
        };
        let mut notice = None;
        let mut stamp = None;
        if let Some(loaded) = loaded {
            stamp = loaded.stamp;
            buffer.set_text(&loaded.text);
            buffer.set_cursor(0, 0);
            file.line_ending = loaded.line_ending;
            if loaded.mixed_endings {
                notice = Some(format!(
                    "Mixed line endings; saving will use {}.",
                    loaded.line_ending.label()
                ));
            }
        }
        let doc = Document::new(buffer, file, theme);
        doc.on_disk.borrow_mut().stamp = stamp;
        Ok((Rc::new(doc), notice))
    }

    /// Restores saved edits when the file on disk cannot be read. Keep
    /// its path, but never treat the empty starting buffer as saved.
    pub fn from_unsaved(
        path: PathBuf,
        text: &str,
        theme: Rc<Theme>,
    ) -> Result<Rc<Document>, String> {
        let (doc, _) = Self::open(None, theme)?;
        doc.restore_text(text);
        doc.rename(path);
        doc.history.borrow_mut().mark_unsaved();
        *doc.saved.borrow_mut() = Saved::default();
        Ok(doc)
    }

    /// A document of `buffer`'s text, saved to `file`.
    pub fn new(buffer: Rc<EditBuffer>, file: File, theme: Rc<Theme>) -> Document {
        buffer.set_syntax_style(Some(theme.syntax_style()));
        buffer.set_default_fg(Some(theme::colors().text));
        let language = detect_language(&buffer, file.path.as_deref());
        let syntax = language.and_then(|language| Highlighter::new(language, &theme));
        let text = buffer.text();
        let indent = Indent::infer(&text, language);
        Document {
            buffer,
            file: RefCell::new(file),
            language: Cell::new(language),
            language_override: Cell::new(false),
            indent: Cell::new(indent),
            syntax: RefCell::new(syntax),
            history: RefCell::new(History::new()),
            theme,
            untitled: Cell::new(0),
            cursor_owner: Cell::new(None),
            parked: RefCell::default(),
            parked_text: RefCell::default(),
            on_disk: RefCell::default(),
            saved: RefCell::new(Saved::of(&text)),
            tracked: RefCell::default(),
        }
    }

    /// What git says of the file, if it's in a repository.
    pub fn tracked(&self) -> Option<Rc<Tracked>> {
        self.tracked.borrow().clone()
    }

    /// Takes what git says of the file now. What it was in the last commit
    /// is kept, unless that's another commit or file now.
    pub fn set_tracked(&self, tracked: Option<Tracked>) {
        let mut current = self.tracked.borrow_mut();
        match (&*current, tracked) {
            (Some(old), Some(new)) if old.same_base(&new) => old.kind.set(new.kind.get()),
            (_, new) => *current = new.map(Rc::new),
        }
    }

    /// Parks the buffer's cursor for editor `id`, which is giving it up.
    pub fn park(&self, id: u64) {
        self.keep(id, self.buffer.cursor());
    }

    /// Keeps the place `cursor` is at for `id`, moving it along with edits
    /// as a parked cursor (see [`Document::follow_edits`]).
    fn keep(&self, id: u64, cursor: LogicalCursor) {
        self.follow_edits();
        let epoch = self.buffer.content_epoch();
        let mut snapshot = self.parked_text.borrow_mut();
        if snapshot.0 != epoch || self.parked.borrow().is_empty() {
            *snapshot = (epoch, self.buffer.text());
        }
        let parked = Parked {
            byte: self.buffer.text_range(0, cursor.offset).len(),
            row: cursor.row,
            col: cursor.col,
            epoch,
        };
        self.parked.borrow_mut().insert(id, parked);
    }

    /// Takes editor `id`'s parked cursor, if it has one, to put back in the
    /// buffer.
    pub fn unpark(&self, id: u64) -> Option<Parked> {
        self.follow_edits();
        self.parked.borrow_mut().remove(&id)
    }

    /// Editor `id`'s parked cursor, if it has one.
    pub fn parked(&self, id: u64) -> Option<Parked> {
        self.parked.borrow().get(&id).copied()
    }

    /// Moves the parked cursors along with the edits made since they were
    /// last moved, so they stay on the same text. An edit before one moves
    /// it; one after doesn't. What changed is taken to be one stretch of
    /// text, as an edit, undo, or moving lines changes: a cursor inside a
    /// stretch changed several edits apart goes to its start.
    pub fn follow_edits(&self) {
        let mut parked = self.parked.borrow_mut();
        let epoch = self.buffer.content_epoch();
        let mut snapshot = self.parked_text.borrow_mut();
        if parked.is_empty() || snapshot.0 == epoch {
            return;
        }
        let text = self.buffer.text();
        let old = &snapshot.1;
        let prefix = common_prefix(old, &text);
        let suffix = common_suffix(&old[prefix..], &text[prefix..]);
        let (old_end, new_end) = (old.len() - suffix, text.len() - suffix);
        let mut moved: Vec<(u64, usize)> = parked
            .iter()
            .map(|(&id, parked)| {
                let byte = match parked.byte {
                    byte if byte <= prefix => byte,
                    byte if byte >= old_end => byte - old_end + new_end,
                    _ => prefix,
                };
                (id, byte)
            })
            .collect();
        moved.sort_by_key(|&(_, byte)| byte);
        let bytes: Vec<u32> = moved.iter().map(|&(_, byte)| byte as u32).collect();
        for ((id, byte), cursor) in moved.into_iter().zip(self.buffer.bytes_to_cursors(&bytes)) {
            if let Some(parked) = parked.get_mut(&id) {
                parked.byte = byte;
                parked.row = cursor.row;
                parked.col = cursor.col;
            }
        }
        *snapshot = (epoch, text);
    }

    /// Forgets editor `id`, which is closing.
    pub fn forget(&self, id: u64) {
        self.parked.borrow_mut().remove(&id);
        if self.cursor_owner.get() == Some(id) {
            self.cursor_owner.set(None);
        }
    }

    /// The file's path; `None` until it is first saved.
    pub fn path(&self) -> Option<PathBuf> {
        self.file.borrow().path.clone()
    }

    /// The file's name as shown, if it has none yet: `Untitled-1`.
    pub fn untitled_name(&self) -> Option<String> {
        self.file
            .borrow()
            .path
            .is_none()
            .then(|| untitled_name(self.untitled.get()))
    }

    /// Whether the document is the file at `path`, however it's named.
    pub fn is_file(&self, path: &Path) -> bool {
        self.file
            .borrow()
            .path
            .as_deref()
            .is_some_and(|open| same_file(open, path))
    }

    /// Whether the text differs from the saved state: undoing back to it,
    /// or editing the text back to what it was, leaves it unmodified.
    pub fn is_modified(&self) -> bool {
        self.history.borrow().is_modified() && self.differs_from_saved()
    }

    /// Whether the text differs from the text as last loaded or saved, or
    /// that is unknown or in use.
    fn differs_from_saved(&self) -> bool {
        let Ok(mut saved) = self.saved.try_borrow_mut() else {
            return true;
        };
        let Some((len, hash)) = saved.text else {
            return true;
        };
        let epoch = self.buffer.content_epoch();
        if let Some((_, differs)) = saved.compared.filter(|&(at, _)| at == epoch) {
            return differs;
        }
        let text = self.buffer.text();
        let differs = text.len() != len || content_hash(text.as_bytes()) != hash;
        saved.compared = Some((epoch, differs));
        differs
    }

    /// The text, if it has unsaved changes. Safe to call while unwinding
    /// from a panic: taken to have them if its history is in use.
    pub fn unsaved_text(&self) -> Option<String> {
        let modified = self
            .history
            .try_borrow()
            .map_or(true, |history| history.is_modified())
            && self.differs_from_saved();
        modified.then(|| self.buffer.text())
    }

    /// The file's path, or `None` if it has none or it's in use; see
    /// [`Document::unsaved_text`].
    pub fn try_path(&self) -> Option<PathBuf> {
        self.file.try_borrow().ok()?.path.clone()
    }

    /// Takes `text`, recovered after a crash, as an unsaved edit, one undo
    /// step.
    pub fn restore_text(&self, text: &str) {
        if let Some(owner) = self.cursor_owner.take() {
            self.park(owner);
        }
        let steps = self.buffer.replace_changed_lines(text);
        let mut history = self.history.borrow_mut();
        history.break_group();
        history.record(EditKind::Other, steps);
        history.break_group();
        drop(history);
        self.follow_edits();
    }

    /// An unnamed document that was never typed in, which opening a file
    /// may replace.
    pub fn is_blank(&self) -> bool {
        self.file.borrow().path.is_none()
            && !self.buffer.can_undo()
            && self.buffer.text().is_empty()
    }

    /// The whole text, with `\n` line breaks.
    pub fn text(&self) -> String {
        self.buffer.text()
    }

    /// Writes the text to `path`, with the file's line endings, and marks
    /// it saved.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let line_ending = self.file.borrow().line_ending;
        let text = self.buffer.text();
        let stamp = save(path, &text, line_ending)?;
        self.history.borrow_mut().mark_saved();
        *self.saved.borrow_mut() = Saved::of(&text);
        *self.on_disk.borrow_mut() = OnDisk {
            stamp: Some(stamp),
            disk: Disk::Same,
        };
        Ok(())
    }

    /// How the file on disk compared with the document when last checked.
    pub fn disk(&self) -> Disk {
        self.on_disk.borrow().disk
    }

    /// The [`content_hash`] of the file as last read or written, if there
    /// was one.
    pub fn disk_hash(&self) -> Option<u64> {
        self.on_disk.borrow().stamp.map(|stamp| stamp.hash)
    }

    /// Catches up with the file on disk, which something else may have
    /// changed. With no unsaved changes, the document takes the change, as
    /// one undo step; with some, it keeps them, and saving asks first. A
    /// file that can't be read is left for later.
    pub fn check_disk(&self) -> Option<DiskChange> {
        let path = self.path()?;
        let OnDisk { stamp, disk } = *self.on_disk.borrow();
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if stamp.is_none() || disk == Disk::Deleted {
                    return None;
                }
                self.on_disk.borrow_mut().disk = Disk::Deleted;
                return Some(DiskChange::Deleted);
            }
            Err(_) => return None,
        };
        if stamp.is_some_and(|stamp| stamp.matches(&meta)) {
            return self.restored();
        }
        let loaded = load(&path).ok()?;
        if loaded.stamp.map(|s| s.hash) == stamp.map(|s| s.hash) {
            // Touched, or changed back.
            self.on_disk.borrow_mut().stamp = loaded.stamp;
            return self.restored();
        }
        if !self.is_modified() {
            if self.reload(loaded, true) {
                return Some(DiskChange::Reloaded);
            }
            // Only partly taken, so not the file.
            return Some(DiskChange::Conflict);
        }
        if disk == Disk::Changed {
            return None;
        }
        self.on_disk.borrow_mut().disk = Disk::Changed;
        Some(DiskChange::Conflict)
    }

    /// The file is as last loaded or saved.
    fn restored(&self) -> Option<DiskChange> {
        let mut on_disk = self.on_disk.borrow_mut();
        if on_disk.disk == Disk::Same {
            return None;
        }
        on_disk.disk = Disk::Same;
        Some(DiskChange::Restored)
    }

    /// Loads the file again, dropping unsaved changes, as one undo step.
    pub fn revert(&self) -> io::Result<()> {
        let Some(path) = self.path() else {
            return Ok(());
        };
        if !self.reload(load(&path)?, false) {
            return Err(io::Error::other("Couldn't load the entire file."));
        }
        Ok(())
    }

    /// Takes `loaded` as the text, changing only the lines that differ, as
    /// one undo step, and as saved. With `join`, it joins the undo step of
    /// the reload just before, if nothing came between. Every editor's
    /// cursor is parked, to follow the change.
    ///
    /// Returns false if the buffer didn't take all of it: then it keeps
    /// what it took as an unsaved edit, and saving asks first.
    fn reload(&self, loaded: Loaded, join: bool) -> bool {
        if let Some(owner) = self.cursor_owner.take() {
            self.park(owner);
        }
        let steps = self.buffer.replace_changed_lines(&loaded.text);
        let took = self.buffer.text() == loaded.text;
        let mut history = self.history.borrow_mut();
        if took {
            history.record_reload(steps, join);
            self.file.borrow_mut().line_ending = loaded.line_ending;
            *self.saved.borrow_mut() = Saved::of(&loaded.text);
            *self.on_disk.borrow_mut() = OnDisk {
                stamp: loaded.stamp,
                disk: Disk::Same,
            };
        } else {
            history.break_group();
            history.record(EditKind::Other, steps);
            history.break_group();
            self.on_disk.borrow_mut().disk = Disk::Changed;
        }
        drop(history);
        self.follow_edits();
        took
    }

    /// Overrides highlighting for every view of this document, including after saving.
    pub fn set_language(&self, language: Option<&'static Language>) {
        self.language_override.set(true);
        self.language.set(language);
        self.buffer.remove_highlights(crate::syntax::HIGHLIGHTS);
        *self.syntax.borrow_mut() =
            language.and_then(|language| Highlighter::new(language, &self.theme));
    }

    /// Saves to `path` from now on, highlighting and indenting for its
    /// language.
    pub fn rename(&self, path: PathBuf) {
        self.file.borrow_mut().path = Some(path);
        if self.language_override.get() {
            return;
        }
        let language = detect_language(&self.buffer, self.path().as_deref());
        if language.map(|l| l.name) != self.language.get().map(|l| l.name) {
            self.language.set(language);
            self.indent
                .set(Indent::infer(&self.buffer.text(), language));
            self.buffer.remove_highlights(crate::syntax::HIGHLIGHTS);
            let highlighter = language.and_then(|language| Highlighter::new(language, &self.theme));
            *self.syntax.borrow_mut() = highlighter;
        }
    }
}

/// What untitled file `number` is called.
pub fn untitled_name(number: u32) -> String {
    format!("Untitled-{number}")
}

/// The length in bytes of what `a` and `b` start with alike, ending on a
/// character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let bytes = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    (0..=bytes)
        .rev()
        .find(|&i| a.is_char_boundary(i))
        .unwrap_or(0)
}

/// The length in bytes of what `a` and `b` end with alike, starting on a
/// character boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    let bytes = a
        .bytes()
        .rev()
        .zip(b.bytes().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (0..=bytes)
        .rev()
        .find(|&i| a.is_char_boundary(a.len() - i))
        .unwrap_or(0)
}

/// The language of the file at `path`, holding `buffer`'s text.
fn detect_language(buffer: &EditBuffer, path: Option<&Path>) -> Option<&'static Language> {
    language::detect(path, || {
        let text = buffer.text();
        text.lines().next().unwrap_or_default().to_string()
    })
}

#[derive(Debug)]
pub struct Loaded {
    /// The text with `\n` line breaks.
    pub text: String,
    pub line_ending: LineEnding,
    /// The file mixed line endings (or had lone `\r`s); saving will normalize them.
    pub mixed_endings: bool,
    /// `None` if there was no file.
    pub stamp: Option<Stamp>,
}

/// Reads `path`. A missing file loads as empty, to be created on save.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let (bytes, stamp) = match fs::File::open(path) {
        Ok(mut file) => {
            let meta = file.metadata()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let stamp = Stamp::new(&meta, &bytes);
            (bytes, Some(stamp))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), None),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))?;

    let crlf = text.matches("\r\n").count();
    let cr = text.matches('\r').count();
    let lf = text.matches('\n').count();
    let line_ending = if crlf > 0 && crlf * 2 >= lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    // Every \r is part of a \r\n and every \n is (CRLF) or none is (LF).
    let mixed_endings = cr != crlf || (crlf > 0 && crlf != lf);

    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Ok(Loaded {
        text,
        line_ending,
        mixed_endings,
        stamp,
    })
}

/// Writes `text` (with `\n` line breaks) to `path` atomically: a sibling
/// temporary file is written, synced, and renamed over the target, so a failed
/// save never leaves a truncated file. Existing permissions are kept, and a
/// symlink's target is written rather than the link replaced. Returns the
/// file as written.
pub fn save(path: &Path, text: &str, line_ending: LineEnding) -> io::Result<Stamp> {
    let target = resolve_symlinks(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".cue-{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);

    let contents = match line_ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    };

    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        if let Ok(meta) = fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        let stamp = Stamp::new(&file.metadata()?, contents.as_bytes());
        fs::rename(&tmp, &target)?;
        Ok(stamp)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `path` made absolute, with `.`, `..`, and symlinked folders resolved, so
/// that however a file is named it gets the same path as in the file tree.
/// The file name is kept even if it is a symlink, as the tree shows it. A
/// path whose folder doesn't exist is only made absolute.
pub fn resolve(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => absolute,
        },
        _ => absolute,
    }
}

/// Whether `a` and `b` name the same file, following symlinks.
pub fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => Ok(target),
            // A dangling link: write where it points.
            Err(_) => {
                let link = fs::read_link(path)?;
                Ok(path.parent().unwrap_or(Path::new("")).join(link))
            }
        },
        Ok(meta) if meta.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a directory", path.display()),
        )),
        _ => Ok(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_cursors_follow_edits() {
        let _serial = crate::test_serial();
        let buffer = Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        buffer.set_text("héllo\nwörld\n");
        let file = File {
            path: None,
            line_ending: LineEnding::Lf,
        };
        let doc = Document::new(buffer.clone(), file, Rc::new(Theme::new().unwrap()));
        let at = |doc: &Document| doc.parked(7).map(|p| (p.row, p.col));
        buffer.set_cursor(1, 3);
        doc.park(7);
        assert_eq!(at(&doc), Some((1, 3)));

        // Edits before it move it; edits after it don't.
        let edit = |row, col, text: &str| {
            buffer.set_cursor(row, col);
            buffer.insert_text(text);
            doc.follow_edits();
        };
        edit(0, 0, "¡");
        assert_eq!(at(&doc), Some((1, 3)));
        edit(1, 0, "a\n");
        assert_eq!(at(&doc), Some((2, 3)));
        edit(2, 1, "✓");
        assert_eq!(at(&doc), Some((2, 4)));
        edit(2, 4, "!");
        assert_eq!(at(&doc), Some((2, 4)), "at the cursor counts as after");
        // Undone, one input at a time, it goes back.
        for _ in 0..2 {
            buffer.undo();
            doc.follow_edits();
        }
        assert_eq!(at(&doc), Some((2, 3)));

        // Deleting around it leaves it where the text was.
        buffer.delete_range((2, 1), (2, 4));
        doc.follow_edits();
        assert_eq!(at(&doc), Some((2, 1)));
        assert_eq!(buffer.text(), "¡héllo\na\nwd\n");
        assert_eq!(doc.unpark(7).map(|p| (p.row, p.col)), Some((2, 1)));
        assert_eq!(at(&doc), None);
    }

    #[test]
    fn common_ends_stop_at_character_boundaries() {
        // "é" and "è" share their first byte.
        assert_eq!(common_prefix("aé", "aè"), 1);
        assert_eq!(common_suffix("éa", "èa"), 1);
        assert_eq!(common_prefix("abc", "abd"), 2);
        assert_eq!(common_suffix("", "x"), 0);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cue-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn takes_changes_on_disk_unless_it_has_unsaved_ones() {
        let _serial = crate::test_serial();
        let dir = temp_dir("disk");
        let path = dir.join("a.txt");
        fs::write(&path, "one\ntwo\n").unwrap();
        let theme = Rc::new(Theme::new().unwrap());
        let (doc, _) = Document::open(Some(path.clone()), theme).unwrap();
        assert_eq!(doc.check_disk(), None);
        doc.save(&path).unwrap();
        assert_eq!(doc.check_disk(), None, "its own save isn't a change");

        // Taken as one undo step; the cursor stays on "two".
        doc.buffer.set_cursor(1, 1);
        doc.cursor_owner.set(Some(7));
        fs::write(&path, "zero\none\ntwo\n").unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Reloaded));
        assert_eq!(doc.text(), "zero\none\ntwo\n");
        assert!(!doc.is_modified());
        assert_eq!(doc.cursor_owner.get(), None);
        let parked = doc.parked(7).unwrap();
        assert_eq!((parked.row, parked.col), (2, 1));
        fs::write(&path, "zero\none\ntwo\n").unwrap();
        assert_eq!(doc.check_disk(), None, "written again the same");

        // Changes in a row, with no edit between, are one undo step.
        fs::write(&path, "zero\none\ntwo\nthree\n").unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Reloaded));
        fs::write(&path, "zero\none\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Reloaded));
        undo(&doc);
        assert_eq!(doc.text(), "one\ntwo\n");
        assert!(doc.is_modified());
        fs::write(&path, "zero\none\ntwo\n").unwrap();
        doc.revert().unwrap();

        // With unsaved changes, a change is a conflict, once.
        doc.buffer.set_cursor(0, 0);
        let steps = doc.buffer.insert_text("x");
        doc.history.borrow_mut().record(EditKind::Type('x'), steps);
        fs::write(&path, "other\n").unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Conflict));
        assert_eq!(doc.disk(), Disk::Changed);
        assert_eq!(doc.text(), "xzero\none\ntwo\n");
        assert_eq!(doc.check_disk(), None);

        // Gone, then back as it last loaded it.
        fs::remove_file(&path).unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Deleted));
        assert_eq!(doc.disk(), Disk::Deleted);
        assert_eq!(doc.check_disk(), None);
        fs::write(&path, "zero\none\ntwo\n").unwrap();
        assert_eq!(doc.check_disk(), Some(DiskChange::Restored));
        assert_eq!(doc.disk(), Disk::Same);

        // Reverting drops the unsaved changes, as one undo step.
        fs::write(&path, "other\n").unwrap();
        doc.revert().unwrap();
        assert_eq!(doc.text(), "other\n");
        assert!(!doc.is_modified());
        assert_eq!(doc.disk(), Disk::Same);
        undo(&doc);
        assert_eq!(doc.text(), "xzero\none\ntwo\n");
    }

    /// Undoes one step, as an editor does.
    fn undo(doc: &Document) {
        let steps = doc.history.borrow_mut().undo().unwrap();
        for _ in 0..steps {
            doc.buffer.undo();
        }
    }

    #[test]
    fn crlf_round_trips() {
        let dir = temp_dir("crlf");
        let path = dir.join("win.txt");
        fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "one\ntwo\n");
        assert_eq!(loaded.line_ending, LineEnding::CrLf);
        assert!(!loaded.mixed_endings);
        save(&path, "one\ntwo\nthree\n", loaded.line_ending).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\r\ntwo\r\nthree\r\n"
        );
    }

    #[test]
    fn detects_mixed_endings_and_missing_files() {
        let dir = temp_dir("mixed");
        let path = dir.join("mixed.txt");
        fs::write(&path, "a\r\nb\nc\rd").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "a\nb\nc\nd");
        assert!(loaded.mixed_endings);

        let missing = load(&dir.join("new.txt")).unwrap();
        assert_eq!(missing.text, "");
        assert_eq!(missing.line_ending, LineEnding::Lf);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = temp_dir("utf8");
        let path = dir.join("bin");
        fs::write(&path, [0xff, 0xfe, b'a']).unwrap();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_gives_one_path_per_file() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("resolve").canonicalize().unwrap();
        fs::create_dir_all(dir.join("real")).unwrap();
        symlink(dir.join("real"), dir.join("linked-dir")).unwrap();
        fs::write(dir.join("real/a.txt"), "").unwrap();
        symlink(dir.join("real/a.txt"), dir.join("real/link.txt")).unwrap();

        let a = dir.join("real/a.txt");
        assert_eq!(resolve(&dir.join("real/../real/./a.txt")), a);
        assert_eq!(resolve(&dir.join("linked-dir/a.txt")), a);
        // A new file in an existing folder, and one in a missing folder.
        assert_eq!(
            resolve(&dir.join("linked-dir/new.txt")),
            dir.join("real/new.txt")
        );
        assert_eq!(resolve(&dir.join("gone/x.txt")), dir.join("gone/x.txt"));
        // Symlinked file names are kept, but still count as the same file.
        let link = dir.join("real/link.txt");
        assert_eq!(resolve(&link), link);
        assert!(same_file(&link, &a));
        assert!(!same_file(&a, &dir.join("real/new.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_permissions_and_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = temp_dir("perms");
        let real = dir.join("script.sh");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o751)).unwrap();
        let link = dir.join("link.sh");
        symlink(&real, &link).unwrap();

        save(&link, "new\n", LineEnding::Lf).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o751
        );
        // No temp files left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn save_into_missing_directory_fails_cleanly() {
        let dir = temp_dir("missing-dir");
        let err = save(&dir.join("nope/file.txt"), "x", LineEnding::Lf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
}
