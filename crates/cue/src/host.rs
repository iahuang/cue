//! The host: what cue does on the machine whose files it edits, apart
//! from the screen. The app (the client) asks it for things by message,
//! and never touches that machine itself, so the host can later run
//! elsewhere: in the session server, or over ssh (see
//! `docs/client-server.md`).
//!
//! For now the host runs on a thread of its own in the same process, but
//! the app talks to it over a socket all the same, as it will to a host in
//! another process: each message serialized ([`postcard`]), after its
//! length.
//!
//! - A [`Request`] is a call: the host answers it with [`Reply`]s, as many
//!   as it takes, and then ends it. Dropping the [`Call`] cancels it, and
//!   whatever it was doing stops; replies to it that were on their way are
//!   dropped, so a call that's gone never hears of results it no longer
//!   wants.
//! - A [`Tell`] asks for something with no answer.
//! - A [`Notice`] is something the host says unasked, such as files that
//!   changed.
//!
//! Each call is answered on a thread of its own, so a slow one never holds
//! up the others. Whatever comes from the host wakes the main loop (see
//! [`crate::wake`]), which takes it in with [`Call::try_recv`] and
//! [`Link::notices`].

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::document::{self, Checked, LineEnding, Loaded, Stamp, Unsaved, Written};
use crate::file_dialog;
use crate::file_index;
use crate::file_ops::{self, FileOp};
use crate::git::{self, GitAnswer, GitCall, Repo};
use crate::image;
use crate::pty;
use crate::quick_look::{self, Peek};
use crate::search::{self, Query};
use crate::session::{self, Session, SessionAnswer, SessionCall};
use crate::symbols::{self, FileSymbols};
use crate::tree::{self, Listed};
use crate::wake;
use crate::watch::{Changes, Watcher};
use crate::workspace::Workspace;

#[cfg(test)]
thread_local! {
    static AT_ONCE: Cell<bool> = const { Cell::new(true) };
}

/// Whether what's asked of the host is waited for as it's asked, in tests,
/// as most expect. A test can turn it off for its thread, to see what
/// shows meanwhile.
#[cfg(test)]
pub fn at_once() -> bool {
    AT_ONCE.get()
}

#[cfg(test)]
pub fn set_at_once(on: bool) {
    AT_ONCE.set(on);
}

/// Tells a call from the others on the same link.
pub type CallId = u64;

/// What the app asks the host to do, answered with [`Reply`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Every file in `workspace` (see [`crate::file_index`]): the files
    /// found so far as it goes, if `progress`, and then the whole list.
    ListFiles {
        workspace: Workspace,
        progress: bool,
    },
    /// The files in `workspace` matching `query` (see [`crate::search`]),
    /// searching `unsaved`'s text, by path, instead of what's on disk.
    Search {
        workspace: Workspace,
        query: Query,
        unsaved: HashMap<PathBuf, String>,
    },
    /// What git says of the repositories `roots` are in.
    GitStatus { roots: Vec<PathBuf> },
    /// Syncing the repository at `root` with its remote.
    GitSync { root: PathBuf, sync: git::Sync },
    /// The symbols `files` define (see [`crate::symbols`]): those found so
    /// far as it goes, if `progress`, and then all of them. Files that
    /// haven't changed since the last indexing aren't read again.
    IndexSymbols { files: Vec<PathBuf>, progress: bool },
    /// The text of the file at `path`, to open, and where it is,
    /// [`document::resolve`]d (see [`document::load`]).
    Load { path: PathBuf },
    /// Writing `text` to `path`, [`document::resolve`]d (see
    /// [`document::write`]).
    Save {
        path: PathBuf,
        text: String,
        line_ending: LineEnding,
        /// The file as last loaded or saved, to write over only if it
        /// still is, if given.
        expected: Option<Option<Stamp>>,
        make_folder: bool,
    },
    /// How each of `files` compares with the file as a document last had
    /// it (see [`document::check`]), a reply each.
    CheckDisk {
        files: Vec<(PathBuf, Option<Stamp>)>,
    },
    /// The bytes of the file at `path`, for an image view (see
    /// [`crate::image`]), unless it's as `unless` says it was last read.
    ReadFile {
        path: PathBuf,
        unless: Option<image::Stamp>,
    },
    /// What each of `paths` is, a reply in all.
    Look { paths: Vec<PathBuf> },
    /// What's at `path`, for Quick Look (see [`quick_look::peek`]).
    Peek { path: PathBuf },
    /// What's in `dir`, for the file dialog (see [`crate::file_dialog`]).
    ReadDir { dir: PathBuf },
    /// Doing something to files (see [`crate::file_ops`]).
    FileOp(FileOp),
    /// What's in each of `folders` (see [`crate::tree`]), a reply each, in
    /// order.
    ListFolders { folders: Vec<PathBuf> },
    /// A terminal's program (see [`pty::serve`]): what it writes, and how
    /// it's doing, as it goes, until it ends. Cancelling it hangs up.
    Terminal(pty::Start),
    /// Doing something with the session's files (see [`session::run`]).
    Session(SessionCall),
    /// Asking git, or having it do something (see [`git::run`]).
    Git(GitCall),
    /// Committing what's staged in the repository at `root`.
    Commit {
        root: PathBuf,
        message: String,
        amend: bool,
    },
    /// Nothing, until cancelled, for tests.
    #[cfg(test)]
    Hold,
}

/// The host's answers to [`Request`]s.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// Files found so far, during a [`Request::ListFiles`] with progress.
    FilesFound(Vec<PathBuf>),
    /// A file's text, or why it couldn't be read.
    Loaded(Result<Loaded, String>),
    /// How a file was found, for a [`Request::CheckDisk`].
    Checked {
        path: PathBuf,
        checked: Checked,
    },
    /// The file as written, or why it wasn't.
    Saved(Result<Written, Unsaved>),
    /// A file's bytes, and how it was on disk then; `None` if it was as
    /// last read; or why it couldn't be read.
    Bytes(Result<Option<image::FileRead>, String>),
    Peeked(Peek),
    /// What each path of a [`Request::Look`] is, or `None` if it isn't
    /// there.
    Looked(Vec<Option<Look>>),
    /// What's in a folder, for the file dialog, or why it couldn't be read.
    Dir {
        dir: PathBuf,
        #[serde(with = "error_kind")]
        entries: Result<Vec<file_dialog::Entry>, io::ErrorKind>,
    },
    /// A folder's entries, folders first. Empty if it can't be read.
    Folder {
        dir: PathBuf,
        entries: Vec<Listed>,
    },
    /// Every file, sorted as the picker lists them, and whether some went
    /// unlisted, there being too many.
    Files {
        files: Vec<PathBuf>,
        truncated: bool,
    },
    /// A file with matches, or the colors of one found before.
    Found(search::Found),
    /// Symbols found so far, during a [`Request::IndexSymbols`] with
    /// progress.
    SymbolsFound(FileSymbols),
    /// Every file's symbols, sorted by path.
    Symbols(FileSymbols),
    Repos(Vec<Repo>),
    Git(GitAnswer),
    Term(pty::Event),
    Session(SessionAnswer),
    /// How a [`Request::GitSync`] went.
    Synced(Result<(), String>),
    /// How a [`Request::FileOp`] went: why it couldn't be done, if it
    /// couldn't.
    FileOpDone(Result<(), String>),
    /// The new commit's abbreviated hash, or what went wrong.
    Committed(Result<String, String>),
}

/// An [`io::ErrorKind`] in a result, as sent: those the app tells apart,
/// and the rest as [`io::ErrorKind::Other`].
mod error_kind {
    use super::*;

    const KINDS: &[io::ErrorKind] = &[
        io::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::NotADirectory,
        io::ErrorKind::IsADirectory,
        io::ErrorKind::AlreadyExists,
    ];

    pub fn serialize<T: Serialize, S: serde::Serializer>(
        result: &Result<T, io::ErrorKind>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        let index = |kind| KINDS.iter().position(|&k| k == kind).unwrap_or(KINDS.len());
        result.as_ref().map_err(|&kind| index(kind)).serialize(ser)
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
        de: D,
    ) -> Result<Result<T, io::ErrorKind>, D::Error> {
        let result = Result::<T, usize>::deserialize(de)?;
        Ok(result.map_err(|i| KINDS.get(i).copied().unwrap_or(io::ErrorKind::Other)))
    }
}

/// What a path is, as the host found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Look {
    /// Where it really is: absolute, with symlinks followed.
    pub real: PathBuf,
    pub is_dir: bool,
}

/// What the app asks of the host with no answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Tell {
    /// Watch these folders for changes (see [`crate::watch`]), and no
    /// others. Those that don't exist are left out.
    Watch(BTreeSet<PathBuf>),
    /// Have the terminal of this [`Request::Terminal`] call do something.
    Term { call: CallId, control: pty::Control },
}

/// What the host says unasked.
#[derive(Debug, Serialize, Deserialize)]
pub enum Notice {
    /// Files in the watched folders changed, with what they are now.
    DiskChanged(Changes),
}

/// What the app sends the host.
#[derive(Serialize, Deserialize)]
enum ToHost {
    Call(CallId, Request),
    Cancel(CallId),
    Tell(Tell),
    /// Answered with [`Output::Settled`] once what came before is done
    /// with, for tests.
    #[cfg(test)]
    Settle,
}

/// What reaches the host's thread: from the app, and from the host's own
/// threads.
enum Input {
    Call(CallId, Request),
    Cancel(CallId),
    Tell(Tell),
    /// A call was answered in full.
    Finished(CallId),
    /// The watcher heard something.
    Watched,
    /// The app is gone.
    Shutdown,
    #[cfg(test)]
    Settle,
}

impl From<ToHost> for Input {
    fn from(message: ToHost) -> Input {
        match message {
            ToHost::Call(id, request) => Input::Call(id, request),
            ToHost::Cancel(id) => Input::Cancel(id),
            ToHost::Tell(tell) => Input::Tell(tell),
            #[cfg(test)]
            ToHost::Settle => Input::Settle,
        }
    }
}

/// What the host sends the app.
#[derive(Serialize, Deserialize)]
enum Output {
    Reply(CallId, Reply),
    /// The call has no more replies.
    End(CallId),
    Notice(Notice),
    #[cfg(test)]
    Settled,
}

/// The app's end of its connection to a host.
#[derive(Clone)]
pub struct Link {
    shared: Rc<Shared>,
}

struct Shared {
    /// To the thread that sends what's here to the host. Once it's
    /// dropped, the host hears the app's gone.
    to_host: Sender<ToHost>,
    next: Cell<CallId>,
    /// Where each call's replies go, until it ends.
    routes: Arc<Mutex<HashMap<CallId, Sender<Reply>>>>,
    notices: Receiver<Notice>,
    #[cfg(test)]
    settled: Receiver<()>,
    /// The host's, if it's in this process, for tests.
    #[cfg(test)]
    state: std::cell::OnceCell<Arc<State>>,
}

impl Link {
    /// Starts a host in this process, on threads of its own, which stop
    /// once the link and its calls are dropped.
    pub fn local() -> Link {
        let (ours, theirs) = UnixStream::pair().expect("can't make a socket pair");
        let state = Arc::new(State::default());
        let link = Link::over(ours);
        #[cfg(test)]
        let _ = link.shared.state.set(Arc::clone(&state));
        std::thread::spawn(move || serve_stream(theirs, state));
        link
    }

    /// The app's end of a connection to a host on the other end of
    /// `stream`.
    fn over(stream: UnixStream) -> Link {
        let (to_host, outbox) = mpsc::channel();
        let routes: Arc<Mutex<HashMap<CallId, Sender<Reply>>>> = Arc::default();
        let (notify, notices) = mpsc::channel();
        #[cfg(test)]
        let (settle, settled) = mpsc::channel();
        let reader_routes = Arc::clone(&routes);
        match stream.try_clone() {
            Ok(reading) => {
                std::thread::spawn(move || {
                    read(
                        reading,
                        &reader_routes,
                        notify,
                        #[cfg(test)]
                        settle,
                    )
                });
                std::thread::spawn(move || write_frames(stream, outbox));
            }
            // As if the host were gone: calls end unanswered.
            Err(_) => drop(outbox),
        }
        Link {
            shared: Rc::new(Shared {
                to_host,
                next: Cell::new(1),
                routes,
                notices,
                #[cfg(test)]
                settled,
                #[cfg(test)]
                state: Default::default(),
            }),
        }
    }

    /// Asks the host for something. Its replies come to the call.
    pub fn call(&self, request: Request) -> Call {
        let shared = &self.shared;
        let id = shared.next.get();
        shared.next.set(id + 1);
        let (sender, replies) = mpsc::channel();
        lock(&shared.routes).insert(id, sender);
        let _ = shared.to_host.send(ToHost::Call(id, request));
        Call {
            id,
            replies,
            shared: Rc::clone(shared),
        }
    }

    pub fn tell(&self, tell: Tell) {
        let _ = self.shared.to_host.send(ToHost::Tell(tell));
    }

    /// Waits for the host to take in what was asked of it so far, for
    /// tests: watching starts, for one.
    #[cfg(test)]
    pub fn settle(&self) {
        let _ = self.shared.to_host.send(ToHost::Settle);
        let _ = self.shared.settled.recv();
    }

    /// Has the host's threads use `config`, for tests, where each thread
    /// has its own settings: they'd have the defaults.
    #[cfg(test)]
    pub fn set_config(&self, config: Arc<crate::config::Config>) {
        if let Some(state) = self.shared.state.get() {
            *lock(&state.config) = Some(config);
        }
    }

    /// What the host said since the last call, in order.
    pub fn notices(&self) -> Vec<Notice> {
        self.shared.notices.try_iter().collect()
    }
}

/// A [`Request`] in progress. Dropping it cancels it. It keeps the host
/// going, even once the link's gone.
pub struct Call {
    id: CallId,
    replies: Receiver<Reply>,
    shared: Rc<Shared>,
}

impl Call {
    /// The next reply, if one came. Disconnected once the call has ended
    /// and every reply was taken.
    pub fn try_recv(&self) -> Result<Reply, TryRecvError> {
        self.replies.try_recv()
    }

    pub fn id(&self) -> CallId {
        self.id
    }

    /// Waits for the next reply. `None` once the call ended. Only for
    /// what can't go on without it yet, and tests: it holds up the screen.
    pub fn wait(&self) -> Option<Reply> {
        self.replies.recv().ok()
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        // Harmless if it ended already.
        let _ = self.shared.to_host.send(ToHost::Cancel(self.id));
    }
}

/// Hands what the host sends to the calls it's for and to the notices,
/// waking the main loop, until the host's gone. Then the calls left end.
fn read(
    from_host: UnixStream,
    routes: &Mutex<HashMap<CallId, Sender<Reply>>>,
    notices: Sender<Notice>,
    #[cfg(test)] settled: Sender<()>,
) {
    read_frames(from_host, |output| {
        match output {
            Output::Reply(id, reply) => {
                if let Some(route) = lock(routes).get(&id) {
                    // The call may be gone, its replies unwanted.
                    let _ = route.send(reply);
                }
            }
            Output::End(id) => {
                lock(routes).remove(&id);
            }
            Output::Notice(notice) => {
                let _ = notices.send(notice);
            }
            #[cfg(test)]
            Output::Settled => {
                let _ = settled.send(());
            }
        }
        wake::wake();
        true
    });
    lock(routes).clear();
    wake::wake();
}

/// Takes each message from `stream`, until it ends, or `take` says to stop.
fn read_frames<T: DeserializeOwned>(stream: UnixStream, mut take: impl FnMut(T) -> bool) {
    let mut reader = BufReader::new(stream);
    let mut len = [0; 8];
    let mut frame = Vec::new();
    while reader.read_exact(&mut len).is_ok() {
        frame.resize(u64::from_le_bytes(len) as usize, 0);
        if reader.read_exact(&mut frame).is_err() {
            break;
        }
        let message = match postcard::from_bytes(&frame) {
            Ok(message) => message,
            Err(err) => {
                eprintln!("cue: a message from the other end can't be read: {err}");
                break;
            }
        };
        if !take(message) {
            break;
        }
    }
}

/// Sends what comes from `outbox` over `stream`, until it's all sent, or
/// the other end's gone. Then the other end hears it's done.
fn write_frames<T: Serialize>(stream: UnixStream, outbox: Receiver<T>) {
    fn write<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
        let frame = postcard::to_stdvec(message).map_err(io::Error::other)?;
        writer.write_all(&(frame.len() as u64).to_le_bytes())?;
        writer.write_all(&frame)
    }
    let mut writer = BufWriter::new(&stream);
    'sending: while let Ok(message) = outbox.recv() {
        if write(&mut writer, &message).is_err() {
            break;
        }
        // What's come meanwhile goes in the same write.
        while let Ok(message) = outbox.try_recv() {
            if write(&mut writer, &message).is_err() {
                break 'sending;
            }
        }
        if writer.flush().is_err() {
            break;
        }
    }
    drop(writer);
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Answers one call, from the thread working on it.
pub struct Responder {
    id: CallId,
    output: Sender<Output>,
    inbox: Sender<Input>,
    cancelled: Arc<AtomicBool>,
}

impl Responder {
    /// Sends `reply`. Returns false if the call was cancelled, and the work
    /// for it should stop.
    pub fn reply(&self, reply: Reply) -> bool {
        !self.is_cancelled() && self.output.send(Output::Reply(self.id, reply)).is_ok()
    }

    /// Set once the call is cancelled, for work that checks as it goes.
    pub fn cancelled(&self) -> &AtomicBool {
        &self.cancelled
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        let _ = self.output.send(Output::End(self.id));
        let _ = self.inbox.send(Input::Finished(self.id));
    }
}

/// Serves the app on the other end of `stream`, until it's gone.
fn serve_stream(stream: UnixStream, state: Arc<State>) {
    let (to_host, inbox) = mpsc::channel();
    let (output, outbox) = mpsc::channel();
    let Ok(reading) = stream.try_clone() else {
        return;
    };
    let from_app = to_host.clone();
    std::thread::spawn(move || {
        read_frames(reading, |message: ToHost| {
            from_app.send(message.into()).is_ok()
        });
        let _ = from_app.send(Input::Shutdown);
    });
    std::thread::spawn(move || write_frames(stream, outbox));
    serve(inbox, to_host, output, state);
}

/// The host's thread: takes calls, tells, and the watcher's changes, until
/// the app's gone.
fn serve(
    inbox: Receiver<Input>,
    host_inbox: Sender<Input>,
    output: Sender<Output>,
    state: Arc<State>,
) {
    let nudge = host_inbox.clone();
    let mut watcher = Watcher::new(move || {
        let _ = nudge.send(Input::Watched);
    });
    // Each call in progress, by whether it was cancelled.
    let mut calls: HashMap<CallId, Arc<AtomicBool>> = HashMap::new();
    // Each terminal's controls, by the call it is.
    let mut terminals: HashMap<CallId, pty::Controller> = HashMap::new();
    loop {
        let input = match watcher.due() {
            Some(due) => inbox.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match input {
            Ok(Input::Call(id, Request::Terminal(start))) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                calls.insert(id, Arc::clone(&cancelled));
                let responder = Responder {
                    id,
                    output: output.clone(),
                    inbox: host_inbox.clone(),
                    cancelled,
                };
                let (controller, controls) = match pty::controls() {
                    Ok(controls) => controls,
                    Err(err) => {
                        responder.reply(Reply::Term(pty::Event::Failed(err.to_string())));
                        continue;
                    }
                };
                terminals.insert(id, controller);
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    state.use_config();
                    pty::serve(start, controls, |event| responder.reply(Reply::Term(event)));
                });
            }
            Ok(Input::Call(id, request)) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                calls.insert(id, Arc::clone(&cancelled));
                let responder = Responder {
                    id,
                    output: output.clone(),
                    inbox: host_inbox.clone(),
                    cancelled,
                };
                let state = Arc::clone(&state);
                std::thread::spawn(move || answer(request, &state, responder));
            }
            Ok(Input::Cancel(id)) => {
                if let Some(cancelled) = calls.remove(&id) {
                    cancelled.store(true, Ordering::Relaxed);
                }
                if let Some(terminal) = terminals.remove(&id) {
                    terminal.send(pty::Control::HangUp);
                }
            }
            Ok(Input::Finished(id)) => {
                calls.remove(&id);
                terminals.remove(&id);
            }
            Ok(Input::Tell(Tell::Term { call, control })) => {
                if let Some(terminal) = terminals.get(&call) {
                    terminal.send(control);
                }
            }
            Ok(Input::Tell(Tell::Watch(folders))) => {
                watcher.watch(folders.into_iter().filter(|f| f.is_dir()).collect());
            }
            Ok(Input::Watched) | Err(RecvTimeoutError::Timeout) => {}
            #[cfg(test)]
            Ok(Input::Settle) => {
                let _ = output.send(Output::Settled);
            }
            Ok(Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(mut changes) = watcher.poll() {
            changes.look();
            let _ = output.send(Output::Notice(Notice::DiskChanged(changes)));
        }
    }
    for cancelled in calls.values() {
        cancelled.store(true, Ordering::Relaxed);
    }
    for terminal in terminals.values() {
        terminal.send(pty::Control::HangUp);
    }
}

/// What the host keeps from one call to the next.
#[derive(Default)]
struct State {
    symbols: symbols::Cache,
    /// The session this host keeps, if it's one.
    session: Mutex<Option<Session>>,
    /// The settings the app said to use, for tests (see
    /// [`Link::set_config`]).
    #[cfg(test)]
    config: Mutex<Option<Arc<crate::config::Config>>>,
}

impl State {
    /// Puts the settings the app told of in use on this thread, for tests.
    fn use_config(&self) {
        #[cfg(test)]
        if let Some(config) = lock(&self.config).clone() {
            crate::config::set((*config).clone());
        }
    }
}

/// Does what `request` asks, replying as it goes.
fn answer(request: Request, state: &State, responder: Responder) {
    state.use_config();
    match request {
        Request::ListFiles {
            workspace,
            progress,
        } => file_index::list(&workspace, progress, &responder),
        Request::Search {
            workspace,
            query,
            unsaved,
        } => search::run(&workspace, &query, unsaved, &responder),
        Request::IndexSymbols { files, progress } => {
            symbols::index(&files, &state.symbols, progress, &responder)
        }
        Request::Load { path } => {
            let loaded = document::load(&path).map_err(|err| err.to_string());
            responder.reply(Reply::Loaded(loaded));
        }
        Request::Save {
            path,
            text,
            line_ending,
            expected,
            make_folder,
        } => {
            let path = document::resolve(&path);
            let written =
                document::write(&path, &text, line_ending, expected, make_folder).map(|stamp| {
                    Written {
                        real: std::fs::canonicalize(&path).ok(),
                        path,
                        stamp,
                    }
                });
            responder.reply(Reply::Saved(written));
        }
        Request::CheckDisk { files } => {
            for (path, stamp) in files {
                let checked = document::check(&path, stamp);
                if !responder.reply(Reply::Checked { path, checked }) {
                    return;
                }
            }
        }
        Request::ReadFile { path, unless } => {
            responder.reply(Reply::Bytes(image::read_if_changed(&path, unless)));
        }
        Request::Look { paths } => {
            let looked = paths
                .iter()
                .map(|path| {
                    let real = std::fs::canonicalize(path).ok()?;
                    let is_dir = real.is_dir();
                    Some(Look { real, is_dir })
                })
                .collect();
            responder.reply(Reply::Looked(looked));
        }
        Request::Peek { path } => {
            responder.reply(Reply::Peeked(quick_look::peek(&path)));
        }
        Request::ReadDir { dir } => {
            let entries = file_dialog::read(&dir);
            responder.reply(Reply::Dir { dir, entries });
        }
        Request::FileOp(op) => {
            responder.reply(Reply::FileOpDone(file_ops::run(&op)));
        }
        Request::ListFolders { folders } => {
            for dir in folders {
                let entries = tree::read_dir(&dir);
                if !responder.reply(Reply::Folder { dir, entries }) {
                    return;
                }
            }
        }
        // Started apart, in `serve`, with its controls.
        Request::Terminal(_) => {}
        Request::Session(call) => {
            let answer = session::run(call, &mut lock(&state.session));
            responder.reply(Reply::Session(answer));
        }
        Request::Git(call) => {
            responder.reply(Reply::Git(git::run(&call)));
        }
        Request::GitStatus { roots } => {
            responder.reply(Reply::Repos(git::read_repos(&roots)));
        }
        Request::GitSync { root, sync } => {
            responder.reply(Reply::Synced(git::sync(&root, &sync)));
        }
        Request::Commit {
            root,
            message,
            amend,
        } => {
            responder.reply(Reply::Committed(git::commit(&root, &message, amend)));
        }
        #[cfg(test)]
        Request::Hold => {
            while !responder.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-host-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn calls_are_answered_and_then_end() {
        let root = fixture("answered");
        fs::write(root.join("a.txt"), "").unwrap();
        let link = Link::local();
        let workspace = Workspace::new([root.clone()]).unwrap();
        let call = link.call(Request::ListFiles {
            workspace,
            progress: false,
        });
        match call.wait() {
            Some(Reply::Files { files, truncated }) => {
                assert_eq!(files, [root.join("a.txt")]);
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }
        assert!(call.wait().is_none());
    }

    #[test]
    fn dropping_a_call_cancels_it() {
        let link = Link::local();
        let call = link.call(Request::Hold);
        let id = call.id;
        drop(call);
        // The call ends once it hears it was cancelled.
        let start = Instant::now();
        while lock(&link.shared.routes).contains_key(&id) {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn watched_folders_notice_changes() {
        let root = fixture("watched");
        let link = Link::local();
        link.tell(Tell::Watch(BTreeSet::from([
            root.clone(),
            root.join("gone"),
        ])));
        link.settle();
        // Let the watch start before changing anything.
        std::thread::sleep(Duration::from_millis(100));
        let file = root.join("a.txt");
        fs::write(&file, "a").unwrap();
        let start = Instant::now();
        loop {
            assert!(start.elapsed() < Duration::from_secs(5), "no notice");
            let heard = link.notices().into_iter().any(|notice| match notice {
                Notice::DiskChanged(changes) => changes.paths.contains(&file),
            });
            if heard {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
