//! Open files, and reading and writing them.
//!
//! A [`Document`] is an open file's text, undo history, and highlighting.
//! Every editor showing the file shares it, so an edit in one panel shows
//! in the others; each editor keeps its own cursor, selection, and scroll.
//!
//! The native edit buffer splits lines on `\n`, `\r\n`, and `\r`, and returns
//! text joined with `\n`. A file's line ending is detected on load and
//! restored on save, so CRLF files stay CRLF.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use opentui::{EditBuffer, WidthMethod};

use crate::history::History;
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

/// An open file, shared by the editors showing it.
pub struct Document {
    pub buffer: Rc<EditBuffer>,
    pub file: RefCell<File>,
    /// What the file is written in, if known.
    pub language: Cell<Option<&'static Language>>,
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
        buffer.set_tab_width(4);
        let mut file = File {
            path,
            line_ending: Default::default(),
        };
        let mut notice = None;
        if let Some(loaded) = loaded {
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
        Ok((Rc::new(Document::new(buffer, file, theme)), notice))
    }

    /// A document of `buffer`'s text, saved to `file`.
    pub fn new(buffer: Rc<EditBuffer>, file: File, theme: Rc<Theme>) -> Document {
        buffer.set_syntax_style(Some(theme.syntax_style()));
        buffer.set_default_fg(Some(theme::TEXT));
        let language = detect_language(&buffer, file.path.as_deref());
        let syntax = language.and_then(|language| Highlighter::new(language, &theme));
        Document {
            buffer,
            file: RefCell::new(file),
            language: Cell::new(language),
            syntax: RefCell::new(syntax),
            history: RefCell::new(History::new()),
            theme,
            untitled: Cell::new(0),
            cursor_owner: Cell::new(None),
            parked: RefCell::default(),
            parked_text: RefCell::default(),
        }
    }

    /// Parks the buffer's cursor for editor `id`, which is giving it up.
    pub fn park(&self, id: u64) {
        self.follow_edits();
        let cursor = self.buffer.cursor();
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

    pub fn is_modified(&self) -> bool {
        self.history.borrow().is_modified()
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

    /// Saves to `path` from now on, highlighting for its language.
    pub fn rename(&self, path: PathBuf) {
        self.file.borrow_mut().path = Some(path);
        let language = detect_language(&self.buffer, self.path().as_deref());
        if language.map(|l| l.name) != self.language.get().map(|l| l.name) {
            self.language.set(language);
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
}

/// Reads `path`. A missing file loads as empty, to be created on save.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
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
    })
}

/// Writes `text` (with `\n` line breaks) to `path` atomically: a sibling
/// temporary file is written, synced, and renamed over the target, so a failed
/// save never leaves a truncated file. Existing permissions are kept, and a
/// symlink's target is written rather than the link replaced.
pub fn save(path: &Path, text: &str, line_ending: LineEnding) -> io::Result<()> {
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
        fs::rename(&tmp, &target)
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
