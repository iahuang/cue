//! Sessions: a workspace's tabs, panels, open files, unsaved changes, and
//! terminals, kept to come back to.
//!
//! Sessions are opt-in, so that the list of them stays short: a cue becomes
//! one with Keep Session, or by the answer to quitting with unsaved changes
//! or programs running, or by losing its terminal (an ssh connection
//! dropping) in that state. From then on it saves itself as it changes, and
//! quitting keeps it: while programs run in its terminals, cue goes on in
//! the background with them (it detaches; see [`crate::attach`]); otherwise
//! it exits, and the session waits on disk, dormant, for its shells to start
//! again in their folders, below what they showed. End Session lets it go.
//!
//! `cue` in a folder of exactly one session's workspace goes back to it;
//! `cue --resume` lists those to choose from, and `cue --list` all of them.
//!
//! Each session is a folder in `$XDG_STATE_HOME/cue/sessions`, or
//! `~/.local/state/cue/sessions`: `session.toml`, unsaved text in `docs/`,
//! terminals' screens in `terminals/`, and while a cue runs it, a lock it
//! holds and the socket it takes attaching on.

use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use toml::{Table, Value};

use crate::document::Document;
use crate::layout::{Axis, Layout, PanelId};

/// What `session.toml` starts with; files of other versions are passed over.
const VERSION: i64 = 1;
const STATE: &str = "session.toml";
const LOCK: &str = "lock";
const SOCKET: &str = "sock";
/// The longest socket path there's room for (`sun_path` is 104 bytes on
/// macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 100;

/// Something a panel shows, or showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    File(PathBuf),
    /// An untitled file, by number.
    Untitled(u32),
    /// A terminal, by id.
    Terminal(u32),
    Image(PathBuf),
}

/// An open file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Doc {
    /// `None` for an untitled one.
    pub path: Option<PathBuf>,
    /// Its number, if untitled.
    pub untitled: u32,
    /// Its unsaved text, in `docs/` (see [`Session::save_text`]).
    pub unsaved: Option<String>,
    /// Opened as a preview, which the next one replaces.
    pub preview: bool,
}

/// A panel's place in a file: its cursor and the first row in view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub doc: Shown,
    pub row: u32,
    pub col: u32,
    pub top: u32,
    /// In reader mode, the file line at the top of the view.
    pub reading: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PanelState {
    pub id: PanelId,
    pub shows: Option<Shown>,
    /// Where it was in each file it showed, most recently opened last.
    pub places: Vec<Place>,
    /// What it showed before, most recent last, and went back from.
    pub back: Vec<Shown>,
    pub forward: Vec<Shown>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabState {
    pub name: Option<String>,
    pub layout: Layout,
    pub active: PanelId,
    pub panels: Vec<PanelState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalState {
    pub id: u32,
    /// The name it was given, if renamed.
    pub name: Option<String>,
    /// The folder its shell is in, or started in.
    pub cwd: PathBuf,
    /// What's running in it, if anything but the shell, as last saved.
    pub program: Option<String>,
    /// Its screen and scrollback, in `terminals/`, as last saved.
    pub screen: Option<String>,
    /// The size it was, in columns and rows.
    pub size: (u16, u16),
}

/// Everything a session keeps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub roots: Vec<PathBuf>,
    /// When it was saved, in seconds since the epoch.
    pub saved: u64,
    pub tree_visible: bool,
    pub tree_width: u32,
    pub tree_focused: bool,
    /// The sidebar shows the changes rather than the tree.
    pub changes_shown: bool,
    /// The tab on screen.
    pub tab: usize,
    pub tabs: Vec<TabState>,
    pub documents: Vec<Doc>,
    pub terminals: Vec<TerminalState>,
    /// What the picker lists first, most recent first.
    pub recent: Vec<Shown>,
    /// A terminal shows its screen there.
    pub attached: bool,
}

impl State {
    /// The running programs, by name, as last saved.
    pub fn programs(&self) -> Vec<&str> {
        self.terminals
            .iter()
            .filter_map(|terminal| terminal.program.as_deref())
            .collect()
    }
}

/// Where sessions are kept.
pub fn default_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
    Some(state.join("cue/sessions"))
}

/// The session this cue is: its folder, which it holds the lock on.
pub struct Session {
    id: String,
    dir: PathBuf,
    _lock: fs::File,
    /// The state last written, to write only what changed.
    written: Option<String>,
    /// The documents whose unsaved text is in `docs/`.
    texts: Vec<Text>,
    /// Names the next document's text.
    next_text: u32,
}

/// A document's unsaved text, in `docs/`.
struct Text {
    doc: Weak<Document>,
    name: String,
    /// The document's content epoch when it was written.
    epoch: u64,
}

impl Session {
    /// Makes a new session in `sessions`, the folder they're kept in.
    pub fn create(sessions: &Path) -> io::Result<Session> {
        fs::create_dir_all(sessions)?;
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ ((std::process::id() as u64) << 32);
        for attempt in 0..64u64 {
            // A small, well-mixed number, so ids are short but don't follow
            // one another.
            let id = format!("{:08x}", mix(seed.wrapping_add(attempt)) as u32);
            let dir = sessions.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => return Session::open(&dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::other("No session IDs available."))
    }

    /// Takes the session in `dir`, as one that was dormant, or this
    /// process's before it replaced itself. Fails if another cue has it.
    pub fn open(dir: &Path) -> io::Result<Session> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            return Err(match err.raw_os_error() {
                Some(libc::EWOULDBLOCK) => {
                    io::Error::other("This session is in use by another cue process.")
                }
                _ => err,
            });
        }
        // Not to programs cue starts: a shell holding it would keep the
        // session looking live.
        unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        let id = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Session {
            id,
            dir: dir.to_path_buf(),
            _lock: lock,
            written: None,
            texts: Vec::new(),
            next_text: 1,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where it takes attaching while it runs.
    pub fn socket(&self) -> PathBuf {
        socket_path(&self.dir)
    }

    /// Writes `state`, unless that's what was written last. Returns
    /// whether it wrote it.
    pub fn save(&mut self, state: &State) -> io::Result<bool> {
        let mut state = state.clone();
        let saved = std::mem::take(&mut state.saved);
        let text = encode(&state);
        if self.written.as_deref() == Some(text.as_str()) {
            return Ok(false);
        }
        state.saved = saved;
        write_atomic(&self.dir.join(STATE), encode(&state).as_bytes())?;
        self.written = Some(text);
        Ok(true)
    }

    /// Keeps `doc`'s unsaved text in `docs/`, unless it's there as it is
    /// now. Returns its name there, or `None` if it has no unsaved changes.
    pub fn save_text(&mut self, doc: &Rc<Document>) -> io::Result<Option<String>> {
        let epoch = doc.buffer.content_epoch();
        let index = self
            .texts
            .iter()
            .position(|text| text.doc.as_ptr() == Rc::as_ptr(doc));
        if let Some(text) = index.map(|index| &self.texts[index]) {
            if text.epoch == epoch && doc.is_modified() {
                return Ok(Some(text.name.clone()));
            }
        }
        let Some(unsaved) = doc.unsaved_text() else {
            return Ok(None);
        };
        let name = match index {
            Some(index) => self.texts[index].name.clone(),
            None => {
                self.next_text += 1;
                format!("{}.txt", self.next_text - 1)
            }
        };
        let docs = self.dir.join("docs");
        fs::create_dir_all(&docs)?;
        write_atomic(&docs.join(&name), unsaved.as_bytes())?;
        let text = Text {
            doc: Rc::downgrade(doc),
            name: name.clone(),
            epoch,
        };
        match index {
            Some(index) => self.texts[index] = text,
            None => self.texts.push(text),
        }
        Ok(Some(name))
    }

    /// Removes the texts in `docs/` but `kept`.
    pub fn prune_texts(&mut self, kept: &[String]) {
        self.texts.retain(|text| kept.contains(&text.name));
        prune(&self.dir.join("docs"), kept);
    }

    /// Notes that `doc`'s unsaved text is kept as `name`, as restored from
    /// it, so that it isn't written again until it changes.
    pub fn note_text(&mut self, doc: &Rc<Document>, name: &str) {
        if let Some(n) = name
            .strip_suffix(".txt")
            .and_then(|n| n.parse::<u32>().ok())
        {
            self.next_text = self.next_text.max(n + 1);
        }
        self.texts.push(Text {
            doc: Rc::downgrade(doc),
            name: name.to_string(),
            epoch: doc.buffer.content_epoch(),
        });
    }

    /// Keeps terminal `id`'s `screen`, returning its name in `terminals/`.
    pub fn save_screen(&self, id: u32, screen: &[u8]) -> io::Result<String> {
        let name = format!("{id}.vt");
        let terminals = self.dir.join("terminals");
        fs::create_dir_all(&terminals)?;
        write_atomic(&terminals.join(&name), screen)?;
        Ok(name)
    }

    /// Removes the screens in `terminals/` but `kept`.
    pub fn prune_screens(&self, kept: &[String]) {
        prune(&self.dir.join("terminals"), kept);
    }

    /// Unsaved text kept by [`Session::save_text`], by name.
    pub fn text(&self, name: &str) -> io::Result<String> {
        fs::read_to_string(self.dir.join("docs").join(name))
    }

    /// A screen kept by [`Session::save_screen`], by name.
    pub fn screen(&self, name: &str) -> Option<Vec<u8>> {
        fs::read(self.dir.join("terminals").join(name)).ok()
    }

    /// Lets the session go, removing everything it kept.
    pub fn end(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Where the session in `dir` takes attaching: in it, or if that path is
/// too long for a socket, in a folder of the user's in the temporary
/// folder.
pub fn socket_path(dir: &Path) -> PathBuf {
    let path = dir.join(SOCKET);
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return path;
    }
    let id = dir.file_name().unwrap_or_default().to_string_lossy();
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/cue-{uid}/{id}.sock"))
}

/// A session, as listed.
#[derive(Debug, Clone)]
pub struct Listing {
    pub id: String,
    pub dir: PathBuf,
    pub state: State,
    /// A cue runs it.
    pub live: bool,
}

impl Listing {
    /// Whether `folder` is in its workspace.
    pub fn has(&self, folder: &Path) -> bool {
        self.state.roots.iter().any(|root| folder.starts_with(root))
    }

    /// Its folders, shortest first, with `~` for the home folder, as
    /// `~/cue + ~/notes`.
    pub fn folders(&self) -> String {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut roots: Vec<String> = self
            .state
            .roots
            .iter()
            .map(|root| {
                match home
                    .as_deref()
                    .and_then(|home| root.strip_prefix(home).ok())
                {
                    Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
                    Some(rest) => format!("~/{}", rest.display()),
                    None => root.display().to_string(),
                }
            })
            .collect();
        roots.sort_by_key(|root| (root.chars().count(), root.clone()));
        roots.join(" + ")
    }

    /// How many of its files have unsaved changes.
    pub fn unsaved(&self) -> usize {
        self.state
            .documents
            .iter()
            .filter(|doc| doc.unsaved.is_some())
            .count()
    }

    /// How long ago it was saved, as `2h ago`.
    pub fn age(&self, now: SystemTime) -> String {
        let saved = UNIX_EPOCH + Duration::from_secs(self.state.saved);
        ago(now.duration_since(saved).unwrap_or_default())
    }

    /// Whether a terminal shows it (`attached`), it's in the background
    /// (`running`), or it waits on disk (`saved`).
    pub fn status(&self) -> &'static str {
        match (self.live, self.state.attached) {
            (true, true) => "attached",
            (true, false) => "running",
            (false, _) => "saved",
        }
    }

    /// A line about it, as `~/cue + ~/notes · 3 tabs · nvim · 2h ago · running`.
    pub fn describe(&self, now: SystemTime) -> String {
        let mut parts = vec![self.folders()];
        match self.state.tabs.len() {
            0 | 1 => {}
            tabs => parts.push(format!("{tabs} tabs")),
        }
        let programs = self.state.programs();
        if !programs.is_empty() {
            parts.push(programs.join(", "));
        }
        match self.unsaved() {
            0 => {}
            1 => parts.push("1 unsaved file".to_string()),
            n => parts.push(format!("{n} unsaved files")),
        }
        parts.push(self.age(now));
        parts.push(self.status().to_string());
        parts.join(" · ")
    }
}

/// How long ago, roughly: `just now`, `5m ago`, `2h ago`, `3d ago`.
fn ago(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// The sessions in `sessions`, most recently saved first.
pub fn list(sessions: &Path) -> Vec<Listing> {
    let Ok(entries) = fs::read_dir(sessions) else {
        return Vec::new();
    };
    let mut found: Vec<Listing> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let text = fs::read_to_string(dir.join(STATE)).ok()?;
            let state = decode(&text)?;
            Some(Listing {
                id: entry.file_name().to_string_lossy().into_owned(),
                live: is_live(&dir),
                dir,
                state,
            })
        })
        .collect();
    found.sort_by(|a, b| b.state.saved.cmp(&a.state.saved).then(a.id.cmp(&b.id)));
    found
}

/// The sessions with `folder` in their workspace, most recent first.
pub fn matching(sessions: &Path, folder: &Path) -> Vec<Listing> {
    list(sessions)
        .into_iter()
        .filter(|listing| listing.has(folder))
        .collect()
}

/// The session `id` names, or the only one an id starting with it names.
pub fn find(sessions: &Path, id: &str) -> Option<Listing> {
    let all = list(sessions);
    if let Some(exact) = all.iter().find(|listing| listing.id == id) {
        return Some(exact.clone());
    }
    let mut prefixed = all.into_iter().filter(|listing| listing.id.starts_with(id));
    let first = prefixed.next()?;
    prefixed.next().is_none().then_some(first)
}

/// Whether a cue runs the session in `dir`: whether it holds the lock.
/// Locks are let go when a process exits however it does.
pub fn is_live(dir: &Path) -> bool {
    let Ok(file) = fs::File::open(dir.join(LOCK)) else {
        return false;
    };
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) != 0 }
}

/// Reads the state kept in `dir`.
pub fn read(dir: &Path) -> Option<State> {
    decode(&fs::read_to_string(dir.join(STATE)).ok()?)
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Removes the files in `dir` but those named in `kept`.
fn prune(dir: &Path, kept: &[String]) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !kept.contains(&name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Writes `bytes` to `path` whole or not at all.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// SplitMix64's finalizer.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

// --- the file -------------------------------------------------------------------

fn path_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

fn shown_value(shown: &Shown) -> Value {
    let mut table = Table::new();
    match shown {
        Shown::File(path) => table.insert("file".into(), path_value(path)),
        Shown::Untitled(n) => table.insert("untitled".into(), Value::Integer(*n as i64)),
        Shown::Terminal(id) => table.insert("terminal".into(), Value::Integer(*id as i64)),
        Shown::Image(path) => table.insert("image".into(), path_value(path)),
    };
    Value::Table(table)
}

fn layout_value(layout: &Layout) -> Value {
    let mut table = Table::new();
    match layout {
        Layout::Panel(id) => {
            table.insert("panel".into(), Value::Integer(*id as i64));
        }
        Layout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let axis = match axis {
                Axis::Horizontal => "horizontal",
                Axis::Vertical => "vertical",
            };
            table.insert("split".into(), Value::String(axis.into()));
            table.insert("ratio".into(), Value::Float(*ratio as f64));
            table.insert("first".into(), layout_value(first));
            table.insert("second".into(), layout_value(second));
        }
    }
    Value::Table(table)
}

fn shown_list(list: &[Shown]) -> Value {
    Value::Array(list.iter().map(shown_value).collect())
}

/// `state` as `session.toml` has it.
pub fn encode(state: &State) -> String {
    let int = |n: u64| Value::Integer(n.min(i64::MAX as u64) as i64);
    let mut root = Table::new();
    root.insert("version".into(), Value::Integer(VERSION));
    root.insert("saved".into(), int(state.saved));
    root.insert("attached".into(), Value::Boolean(state.attached));
    root.insert(
        "roots".into(),
        Value::Array(state.roots.iter().map(|root| path_value(root)).collect()),
    );
    let mut tree = Table::new();
    tree.insert("visible".into(), Value::Boolean(state.tree_visible));
    tree.insert("width".into(), int(state.tree_width as u64));
    tree.insert("focused".into(), Value::Boolean(state.tree_focused));
    tree.insert("changes".into(), Value::Boolean(state.changes_shown));
    root.insert("tree".into(), Value::Table(tree));
    root.insert("tab".into(), int(state.tab as u64));
    root.insert("recent".into(), shown_list(&state.recent));
    let documents = state.documents.iter().map(|doc| {
        let mut table = Table::new();
        match &doc.path {
            Some(path) => table.insert("path".into(), path_value(path)),
            None => table.insert("untitled".into(), int(doc.untitled as u64)),
        };
        if let Some(unsaved) = &doc.unsaved {
            table.insert("unsaved".into(), Value::String(unsaved.clone()));
        }
        if doc.preview {
            table.insert("preview".into(), Value::Boolean(true));
        }
        Value::Table(table)
    });
    root.insert("documents".into(), Value::Array(documents.collect()));
    let terminals = state.terminals.iter().map(|terminal| {
        let mut table = Table::new();
        table.insert("id".into(), int(terminal.id as u64));
        table.insert("cwd".into(), path_value(&terminal.cwd));
        if let Some(name) = &terminal.name {
            table.insert("name".into(), Value::String(name.clone()));
        }
        if let Some(program) = &terminal.program {
            table.insert("program".into(), Value::String(program.clone()));
        }
        if let Some(screen) = &terminal.screen {
            table.insert("screen".into(), Value::String(screen.clone()));
        }
        table.insert("cols".into(), int(terminal.size.0 as u64));
        table.insert("rows".into(), int(terminal.size.1 as u64));
        Value::Table(table)
    });
    root.insert("terminals".into(), Value::Array(terminals.collect()));
    let tabs = state.tabs.iter().map(|tab| {
        let mut table = Table::new();
        if let Some(name) = &tab.name {
            table.insert("name".into(), Value::String(name.clone()));
        }
        table.insert("active".into(), int(tab.active as u64));
        table.insert("layout".into(), layout_value(&tab.layout));
        let panels = tab.panels.iter().map(|panel| {
            let mut table = Table::new();
            table.insert("id".into(), int(panel.id as u64));
            if let Some(shows) = &panel.shows {
                table.insert("shows".into(), shown_value(shows));
            }
            let places = panel.places.iter().map(|place| {
                let Value::Table(mut table) = shown_value(&place.doc) else {
                    unreachable!()
                };
                table.insert("row".into(), int(place.row as u64));
                table.insert("col".into(), int(place.col as u64));
                table.insert("top".into(), int(place.top as u64));
                if let Some(line) = place.reading {
                    table.insert("reading".into(), int(line as u64));
                }
                Value::Table(table)
            });
            table.insert("places".into(), Value::Array(places.collect()));
            table.insert("back".into(), shown_list(&panel.back));
            table.insert("forward".into(), shown_list(&panel.forward));
            Value::Table(table)
        });
        table.insert("panels".into(), Value::Array(panels.collect()));
        Value::Table(table)
    });
    root.insert("tabs".into(), Value::Array(tabs.collect()));
    root.to_string()
}

fn get_u32(table: &Table, key: &str) -> Option<u32> {
    u32::try_from(table.get(key)?.as_integer()?).ok()
}

fn get_path(table: &Table, key: &str) -> Option<PathBuf> {
    table.get(key)?.as_str().map(PathBuf::from)
}

fn get_string(table: &Table, key: &str) -> Option<String> {
    table.get(key)?.as_str().map(str::to_string)
}

fn get_array<'a>(table: &'a Table, key: &str) -> impl Iterator<Item = &'a Table> {
    table
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_table)
}

fn shown_from(table: &Table) -> Option<Shown> {
    if let Some(path) = get_path(table, "file") {
        return Some(Shown::File(path));
    }
    if let Some(n) = get_u32(table, "untitled") {
        return Some(Shown::Untitled(n));
    }
    if let Some(id) = get_u32(table, "terminal") {
        return Some(Shown::Terminal(id));
    }
    get_path(table, "image").map(Shown::Image)
}

fn layout_from(table: &Table) -> Option<Layout> {
    if let Some(id) = get_u32(table, "panel") {
        return Some(Layout::Panel(id));
    }
    let axis = match table.get("split")?.as_str()? {
        "horizontal" => Axis::Horizontal,
        "vertical" => Axis::Vertical,
        _ => return None,
    };
    let ratio = table.get("ratio")?.as_float()? as f32;
    Some(Layout::Split {
        axis,
        ratio: ratio.clamp(0.0, 1.0),
        first: Box::new(layout_from(table.get("first")?.as_table()?)?),
        second: Box::new(layout_from(table.get("second")?.as_table()?)?),
    })
}

/// The ids of the panels in `layout`.
fn layout_panels(layout: &Layout, ids: &mut Vec<PanelId>) {
    match layout {
        Layout::Panel(id) => ids.push(*id),
        Layout::Split { first, second, .. } => {
            layout_panels(first, ids);
            layout_panels(second, ids);
        }
    }
}

/// The state `text` has, if it's a `session.toml` of this version. What
/// doesn't make sense in it is left out.
pub fn decode(text: &str) -> Option<State> {
    let root: Table = text.parse().ok()?;
    if root.get("version")?.as_integer()? != VERSION {
        return None;
    }
    let shown_list = |table: &Table, key: &str| -> Vec<Shown> {
        get_array(table, key).filter_map(shown_from).collect()
    };
    let tree = root.get("tree").and_then(Value::as_table);
    let tree_flag = |key: &str| tree.and_then(|tree| tree.get(key)).and_then(Value::as_bool);
    let mut state = State {
        roots: root
            .get("roots")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect(),
        saved: root
            .get("saved")
            .and_then(Value::as_integer)
            .unwrap_or(0)
            .max(0) as u64,
        attached: root
            .get("attached")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tree_visible: tree_flag("visible").unwrap_or(true),
        tree_width: tree.and_then(|tree| get_u32(tree, "width")).unwrap_or(0),
        tree_focused: tree_flag("focused").unwrap_or(false),
        changes_shown: tree_flag("changes").unwrap_or(false),
        tab: get_u32(&root, "tab").unwrap_or(0) as usize,
        recent: shown_list(&root, "recent"),
        ..State::default()
    };
    state.documents = get_array(&root, "documents")
        .filter_map(|table| {
            let path = get_path(table, "path");
            let untitled = get_u32(table, "untitled").unwrap_or(0);
            (path.is_some() || untitled > 0).then(|| Doc {
                path,
                untitled,
                unsaved: get_string(table, "unsaved"),
                preview: table
                    .get("preview")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect();
    state.terminals = get_array(&root, "terminals")
        .filter_map(|table| {
            Some(TerminalState {
                id: get_u32(table, "id")?,
                cwd: get_path(table, "cwd")?,
                name: get_string(table, "name"),
                program: get_string(table, "program"),
                screen: get_string(table, "screen"),
                size: (
                    get_u32(table, "cols")
                        .unwrap_or(80)
                        .clamp(1, u16::MAX as u32) as u16,
                    get_u32(table, "rows")
                        .unwrap_or(24)
                        .clamp(1, u16::MAX as u32) as u16,
                ),
            })
        })
        .collect();
    state.tabs = get_array(&root, "tabs")
        .filter_map(|table| {
            let layout = layout_from(table.get("layout")?.as_table()?)?;
            let mut ids = Vec::new();
            layout_panels(&layout, &mut ids);
            let mut panels: Vec<PanelState> = get_array(table, "panels")
                .filter_map(|panel| {
                    let id = get_u32(panel, "id")?;
                    let places = get_array(panel, "places")
                        .filter_map(|place| {
                            Some(Place {
                                doc: shown_from(place)?,
                                row: get_u32(place, "row").unwrap_or(0),
                                col: get_u32(place, "col").unwrap_or(0),
                                top: get_u32(place, "top").unwrap_or(0),
                                reading: get_u32(place, "reading"),
                            })
                        })
                        .collect();
                    Some(PanelState {
                        id,
                        shows: panel
                            .get("shows")
                            .and_then(Value::as_table)
                            .and_then(shown_from),
                        places,
                        back: shown_list(panel, "back"),
                        forward: shown_list(panel, "forward"),
                    })
                })
                .filter(|panel| ids.contains(&panel.id))
                .collect();
            // Every panel in the layout, once.
            panels.dedup_by_key(|panel| panel.id);
            for &id in &ids {
                if !panels.iter().any(|panel| panel.id == id) {
                    panels.push(PanelState {
                        id,
                        ..PanelState::default()
                    });
                }
            }
            if panels.len() != ids.len() {
                return None;
            }
            let active = get_u32(table, "active")
                .filter(|active| ids.contains(active))
                .unwrap_or(ids[0]);
            Some(TabState {
                name: get_string(table, "name"),
                layout,
                active,
                panels,
            })
        })
        .collect();
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-session-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> State {
        State {
            roots: vec![PathBuf::from("/w/cue"), PathBuf::from("/w/notes")],
            saved: 1_700_000_000,
            tree_visible: false,
            tree_width: 32,
            tree_focused: true,
            changes_shown: true,
            tab: 1,
            tabs: vec![
                TabState {
                    name: None,
                    layout: Layout::Panel(0),
                    active: 0,
                    panels: vec![PanelState {
                        id: 0,
                        shows: Some(Shown::File("/w/cue/a \"quoted\".rs".into())),
                        places: vec![Place {
                            doc: Shown::File("/w/cue/a \"quoted\".rs".into()),
                            row: 12,
                            col: 4,
                            top: 3,
                            reading: Some(7),
                        }],
                        back: vec![Shown::Terminal(2), Shown::Image("/w/i.png".into())],
                        forward: vec![Shown::Untitled(1)],
                    }],
                },
                TabState {
                    name: Some("build".into()),
                    layout: Layout::Split {
                        axis: Axis::Vertical,
                        ratio: 0.25,
                        first: Box::new(Layout::Panel(3)),
                        second: Box::new(Layout::Panel(5)),
                    },
                    active: 5,
                    panels: vec![
                        PanelState {
                            id: 3,
                            shows: Some(Shown::Terminal(2)),
                            ..PanelState::default()
                        },
                        PanelState {
                            id: 5,
                            ..PanelState::default()
                        },
                    ],
                },
            ],
            documents: vec![
                Doc {
                    path: Some("/w/cue/a \"quoted\".rs".into()),
                    unsaved: Some("0.txt".into()),
                    ..Doc::default()
                },
                Doc {
                    untitled: 1,
                    preview: true,
                    ..Doc::default()
                },
            ],
            terminals: vec![TerminalState {
                id: 2,
                name: Some("server".into()),
                cwd: "/w/cue".into(),
                program: Some("cargo".into()),
                screen: Some("2.vt".into()),
                size: (100, 30),
            }],
            recent: vec![Shown::Terminal(2), Shown::File("/w/cue/b.rs".into())],
            attached: true,
        }
    }

    #[test]
    fn state_reads_back_as_written() {
        let state = sample();
        assert_eq!(decode(&encode(&state)), Some(state));
    }

    #[test]
    fn broken_parts_are_left_out() {
        let mut text = encode(&sample());
        assert!(decode(&text.replace("version = 1", "version = 2")).is_none());
        // A tab whose layout names a panel it doesn't have gets an empty
        // one; one with no layout is dropped.
        text = text.replace("panel = 5", "panel = 9");
        let state = decode(&text).unwrap();
        let panels: Vec<PanelId> = state.tabs[1].panels.iter().map(|p| p.id).collect();
        assert_eq!(panels, [3, 9]);
        assert_eq!(state.tabs[1].active, 3, "the active panel is gone");
    }

    #[test]
    fn sessions_are_listed_by_folder_and_liveness() {
        let sessions = temp_dir("list");
        let mut session = Session::create(&sessions).unwrap();
        assert_eq!(session.id().len(), 8);
        let mut state = sample();
        state.attached = false;
        assert!(session.save(&state).unwrap());
        assert!(!session.save(&state).unwrap(), "unchanged");
        state.saved += 10;
        assert!(!session.save(&state).unwrap(), "only the time changed");

        let listed = list(&sessions);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].live, "this process holds it");
        assert!(Session::open(&listed[0].dir).is_err(), "taken");
        assert_eq!(matching(&sessions, Path::new("/w/cue/src")).len(), 1);
        assert_eq!(matching(&sessions, Path::new("/w/notes")).len(), 1);
        assert!(matching(&sessions, Path::new("/w")).is_empty());
        assert!(matching(&sessions, Path::new("/w/cuex")).is_empty());
        let id = session.id().to_string();
        assert_eq!(find(&sessions, &id[..3]).map(|l| l.id), Some(id.clone()));

        let line = listed[0].describe(UNIX_EPOCH + Duration::from_secs(1_700_007_200));
        assert_eq!(
            line,
            "/w/cue + /w/notes · 2 tabs · cargo · 1 unsaved file · 2h ago · running"
        );

        let screen = session.save_screen(2, b"\x1b[1mhi").unwrap();
        assert_eq!(session.screen(&screen).as_deref(), Some(&b"\x1b[1mhi"[..]));

        drop(session);
        let listed = list(&sessions);
        assert!(!listed[0].live, "let go");
        let session = Session::open(&listed[0].dir).unwrap();
        session.end();
        assert!(list(&sessions).is_empty());
    }

    #[test]
    fn long_socket_paths_go_in_the_temporary_folder() {
        let short = Path::new("/s/abcd1234");
        assert_eq!(socket_path(short), short.join("sock"));
        let long = PathBuf::from(format!("/{}/abcd1234", "x".repeat(100)));
        let uid = unsafe { libc::getuid() };
        assert_eq!(
            socket_path(&long),
            PathBuf::from(format!("/tmp/cue-{uid}/abcd1234.sock"))
        );
    }
}
