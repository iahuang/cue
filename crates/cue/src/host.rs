//! The host: what cue does on the machine whose files it edits, apart
//! from the screen. The app (the client) asks it for things by message,
//! and never touches that machine itself, so the host can later run
//! elsewhere: in the session server, or over ssh (see
//! `docs/client-server.md`).
//!
//! The host runs on a thread of its own in the same process, or in a
//! process of its own (`cue --host`, see [`serve_listener`]); either way
//! the app talks to it over a socket: each message serialized
//! ([`postcard`]), after its length. A host process serves one client at a
//! time, of its own build: one that connects takes over once the one
//! before, told [`Notice::Moved`], has saved and left (see
//! [`Link::leave`]). While it keeps a session, it takes clients on the
//! session's socket too, which is how `cue` finds it again. A client of
//! another build has it start that build in its place, which takes up the
//! session and its terminals (see [`Handover`]).
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

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::app::Adopted;
use crate::document::{self, Checked, LineEnding, Loaded, Stamp, Unsaved, Written};
use crate::file_dialog;
use crate::file_index::{self, Paths};
use crate::file_ops::{self, FileOp};
use crate::git::{self, GitAnswer, GitCall, Repo};
use crate::image;
use crate::pty;
use crate::quick_look::{self, Peek};
use crate::replica::{Copies, DocId, Edit, Texts};
use crate::search::{self, Query};
use crate::session::{self, Session, SessionAnswer, SessionCall};
use crate::symbols::{self, FileSymbols};
use crate::term_hub::{Colors, Hub, TermId, Token};
use crate::tree::{self, Listed};
use crate::wake;
use crate::watch::{Changes, Watcher};
use crate::wire::{self, read_frame, read_frames, write_frame, write_frames, Lane};
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

/// Hands a host process the socket it listens on, by descriptor.
const LISTENER_VARIABLE: &str = "CUE_HOST_FD";
/// Hands a host process that replaced another the client it's for, and
/// the session's lock, by descriptor (see [`Handover`]).
const CLIENT_VARIABLE: &str = "CUE_HOST_CLIENT_FD";
const LOCK_VARIABLE: &str = "CUE_HOST_LOCK_FD";
/// How long either end waits for the other's side of the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a host process waits for its first client.
const FIRST_CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a host process with no client looks whether it's to go on.
const DETACHED_POLL: Duration = Duration::from_secs(1);
/// How long a client taken over from gets to save and leave, before the
/// next is served regardless.
const MOVE_TIMEOUT: Duration = Duration::from_millis(if cfg!(test) { 300 } else { 5000 });

/// What the app asks the host to do, answered with [`Reply`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Every file in `workspace` (see [`crate::file_index`]): the files
    /// found so far as it goes, if `progress`, and then the whole list; or
    /// what changed since the list the app has, `since`, if the host still
    /// has it.
    ListFiles {
        workspace: Workspace,
        progress: bool,
        since: Option<u64>,
    },
    /// The files in `workspace` matching `query` (see [`crate::search`]),
    /// searching the copies of documents open with unsaved changes,
    /// `unsaved`, by path, instead of what's on disk.
    Search {
        workspace: Workspace,
        query: Query,
        unsaved: HashMap<PathBuf, DocId>,
    },
    /// What git says of the repositories `roots` are in, now and as it
    /// changes, until cancelled (see [`git::follow`]).
    GitStatus { roots: Vec<PathBuf> },
    /// Syncing the repository at `root` with its remote.
    GitSync { root: PathBuf, sync: git::Sync },
    /// The symbols `files` define (see [`crate::symbols`]): those found so
    /// far as it goes, if `progress`, and then all of them. Files that
    /// haven't changed since the last indexing aren't read again.
    IndexSymbols { files: Vec<PathBuf>, progress: bool },
    /// The text of the file at `path`, to open, and where it is,
    /// [`document::resolve`]d (see [`document::load`]); kept as the copy
    /// `doc`, if given (see [`crate::replica`]).
    Load { path: PathBuf, doc: Option<DocId> },
    /// Writing the copy `doc`, as of `version`, to `path`,
    /// [`document::resolve`]d (see [`document::write`]).
    Save {
        path: PathBuf,
        doc: DocId,
        version: u64,
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
    /// Which terminals are running, by their ids on the host, for a
    /// client attaching to them (see [`crate::term_hub`]).
    Terminals,
    /// What the app's to know of the host's machine (see [`About`]).
    About,
    /// The sessions kept on the host's machine, most recent first.
    Sessions,
    /// Nothing, until cancelled, for tests.
    #[cfg(test)]
    Hold,
}

/// The host's answers to [`Request`]s.
#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    /// Files found so far, during a [`Request::ListFiles`] with progress.
    FilesFound(Paths),
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
    /// unlisted, there being too many; the list's `version`, for the next
    /// [`Request::ListFiles`] to send only what changed since.
    Files {
        files: Paths,
        truncated: bool,
        version: u64,
    },
    /// What changed since the list a [`Request::ListFiles`] said the app
    /// has, which makes the list `version`.
    FilesChanged {
        added: Paths,
        removed: Paths,
        truncated: bool,
        version: u64,
    },
    /// A file with matches, or the colors of one found before.
    Found(search::Found),
    /// Symbols found so far, during a [`Request::IndexSymbols`] with
    /// progress.
    SymbolsFound(FileSymbols),
    /// Every file's symbols, sorted by path.
    Symbols(FileSymbols),
    /// What git says of a [`Request::GitStatus`]'s repositories, if it
    /// changed since the last reply, after the `asked`th run the app asked
    /// for.
    Repos {
        repos: Option<Vec<Repo>>,
        asked: u64,
    },
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
    Terminals(Vec<TermId>),
    About(About),
    Sessions(Vec<session::Listing>),
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

/// What the app's to know of the host's machine, which needn't be the
/// app's.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct About {
    /// Its user's home folder, if it has one.
    pub home: Option<PathBuf>,
    /// Where its sessions are kept (see [`session::default_dir`]).
    pub sessions: Option<PathBuf>,
    /// Its settings file's text, if it has one, of which the app takes all
    /// but `[ui]` and `[keys]` (see [`crate::config`]).
    pub settings: Option<String>,
}

impl About {
    /// What this machine is like.
    pub fn here() -> About {
        About {
            home: std::env::var_os("HOME").map(PathBuf::from),
            sessions: session::default_dir(),
            settings: crate::config::path().and_then(|path| std::fs::read_to_string(path).ok()),
        }
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
    /// Have this [`Request::GitStatus`] call ask git again, as the
    /// `asked`th run asked for, dropping one in progress if `restart`.
    GitAgain {
        call: CallId,
        asked: u64,
        restart: bool,
    },
    /// Keep `text` as the copy `doc` (see [`crate::replica`]).
    Open { doc: DocId, text: String },
    /// Make `edit` to the copy `doc`, as `version`.
    Edit {
        doc: DocId,
        version: u64,
        edit: Edit,
    },
    /// Let the copy `doc` go.
    Close(DocId),
    /// The colors programs in the client's terminals start with, which a
    /// host process's emulators answer with (see [`crate::term_hub`]).
    Colors(Colors),
    /// Read the settings again: the client did. A host process reads its
    /// own file, and takes its part of it (see [`crate::config`]); one in
    /// the app's process shares the app's.
    ReloadConfig,
}

/// What the host says unasked.
#[derive(Debug, Serialize, Deserialize)]
pub enum Notice {
    /// Files in the watched folders changed, with what they are now.
    DiskChanged(Changes),
    /// The copy `doc` took the edits up to `version`.
    Acked { doc: DocId, version: u64 },
    /// Another client is taking over: this one's to save what it has,
    /// and leave (see [`Link::leave`]), within [`MOVE_TIMEOUT`].
    Moved,
    /// The session was ended from elsewhere (see [`end_session`]): its
    /// terminals are hung up on, and this client's served no more.
    Ended,
    /// The host's gone. Not sent: the link says so itself.
    Gone,
}

/// What a client says first, to a host process: the one message, with
/// the answer to it, that builds share. Fields are only ever added, at the
/// end, which a host of an earlier build doesn't read.
#[derive(Serialize, Deserialize)]
struct Hello {
    /// Which build of cue it is (see [`crate::attach::build`]).
    build: String,
    /// The cue whose terminal it runs in, if it does (see
    /// [`crate::attach::PID_VARIABLE`]): a host doesn't take a client in
    /// its own terminal, which would show it in itself.
    inside: Option<u32>,
    /// To end the host's session, not to be served.
    end: bool,
    /// Its binary, for a host of another build to start in its place.
    exe: PathBuf,
}

impl Hello {
    fn new(build: &str, end: bool) -> Hello {
        let inside = std::env::var(crate::attach::PID_VARIABLE).ok();
        Hello {
            build: build.to_string(),
            inside: inside.and_then(|pid| pid.parse().ok()),
            end,
            exe: crate::relocate::origin().to_path_buf(),
        }
    }
}

/// What the app sends the host.
#[derive(Serialize, Deserialize)]
enum ToHost {
    Call(CallId, Request),
    Cancel(CallId),
    Tell(Tell),
    /// The app's leaving its terminals running, for another client: what
    /// comes after, such as cancelling them, is ignored.
    Leave,
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
    /// The app left, for another client to come.
    Left,
    /// The app is gone.
    Shutdown,
    /// Another client is taking over: the app's told so, and served on
    /// until it leaves.
    Moving,
    /// Another client took over, this one having taken too long to leave.
    Evicted,
    /// The session was ended: the app's told so.
    Ended,
    #[cfg(test)]
    Settle,
}

impl From<ToHost> for Input {
    fn from(message: ToHost) -> Input {
        match message {
            ToHost::Call(id, request) => Input::Call(id, request),
            ToHost::Cancel(id) => Input::Cancel(id),
            ToHost::Tell(tell) => Input::Tell(tell),
            ToHost::Leave => Input::Left,
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

/// The lane the session's calls share, so that they arrive in order even
/// once a save with screens goes in chunks (see [`crate::wire`]).
const SESSION_LANE: Lane = Lane::MAX;

impl wire::Message for ToHost {
    fn bulk(&self) -> bool {
        match self {
            ToHost::Call(_, Request::Session(SessionCall::Save(snapshot))) => {
                !snapshot.screens.is_empty()
            }
            ToHost::Call(_, Request::Terminal(pty::Start::Shell { above, .. })) => {
                !above.is_empty()
            }
            _ => false,
        }
    }

    fn lane(&self) -> Option<Lane> {
        match self {
            ToHost::Call(_, Request::Session(_)) => Some(SESSION_LANE),
            ToHost::Call(id, _) | ToHost::Cancel(id) => Some(*id),
            ToHost::Tell(Tell::Term { call, .. } | Tell::GitAgain { call, .. }) => Some(*call),
            _ => None,
        }
    }
}

impl wire::Message for Output {
    fn bulk(&self) -> bool {
        match self {
            Output::Reply(_, reply) => reply.bulk(),
            _ => false,
        }
    }

    fn lane(&self) -> Option<Lane> {
        match self {
            Output::Reply(id, _) | Output::End(id) => Some(*id),
            _ => None,
        }
    }

    /// A notice comes after the replies before it: a file's text, then
    /// that it changed on disk since.
    fn barrier(&self) -> bool {
        matches!(self, Output::Notice(_))
    }
}

impl Reply {
    /// Whether it may be big, to go in chunks, behind what's typed.
    fn bulk(&self) -> bool {
        match self {
            Reply::Term(event) => matches!(event, pty::Event::Screen { .. }),
            Reply::FilesFound(_)
            | Reply::Files { .. }
            | Reply::FilesChanged { .. }
            | Reply::Loaded(_)
            | Reply::Bytes(_)
            | Reply::Peeked(_)
            | Reply::Dir { .. }
            | Reply::Folder { .. }
            | Reply::Found(_)
            | Reply::SymbolsFound(_)
            | Reply::Symbols(_)
            | Reply::Git(_)
            | Reply::Session(_)
            | Reply::Sessions(_) => true,
            _ => false,
        }
    }
}

/// Where each call's replies go, until it ends; `None` once the host's
/// gone, or left.
type Routes = Option<HashMap<CallId, Sender<Reply>>>;

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
    next_doc: Cell<DocId>,
    /// Where each call's replies go, until it ends.
    routes: Arc<Mutex<Routes>>,
    notices: Receiver<Notice>,
    /// Disconnected once everything asked of the host is sent (see
    /// [`Link::closed`]).
    sent: RefCell<Option<Receiver<()>>>,
    /// The host is on a thread of this process.
    local: bool,
    /// How the host's reached, if it's on another machine.
    remote: Option<crate::remote::Remote>,
    #[cfg(test)]
    settled: Receiver<()>,
    /// The host's, if it's in this process, for tests.
    #[cfg(test)]
    state: std::cell::OnceCell<Arc<State>>,
}

impl Link {
    /// The app's host: in a process of its own, with `CUE_HOST_PROCESS`
    /// set (experimental, until the app moves into the client), or else
    /// in this one.
    pub fn start() -> Link {
        if cfg!(test) || !crate::env_flag("CUE_HOST_PROCESS") {
            return Link::local();
        }
        Link::spawn().unwrap_or_else(|err| {
            eprintln!("cue: can't start a host process: {err}");
            Link::local()
        })
    }

    /// Starts a host in this process, on threads of its own, which stop
    /// once the link and its calls are dropped.
    pub fn local() -> Link {
        let (ours, theirs) = UnixStream::pair().expect("can't make a socket pair");
        // The process's native core is the app's.
        let state = Arc::new(State::new(false, None));
        let link = Link::over(ours, true);
        #[cfg(test)]
        let _ = link.shared.state.set(Arc::clone(&state));
        std::thread::spawn(move || {
            let _ = serve_stream(theirs, mpsc::channel(), &state);
            // No other client can come for its terminals.
            state.hub.hang_up_all();
        });
        link
    }

    /// Starts a host process (`cue --host`), listening on a socket of its
    /// own, and connects to it. It ends once no client is connected.
    pub fn spawn() -> io::Result<Link> {
        Link::connect(&spawn_host()?)
    }

    /// Connects to the host process listening on `path`. If it serves
    /// another client, that one leaves first (see [`Notice::Moved`]), and
    /// what's asked meanwhile waits.
    pub fn connect(path: &Path) -> io::Result<Link> {
        Ok(Link::over(connect_stream(path)?, false))
    }

    /// The app's end of a host on another machine, on the other end of
    /// `stream`, as `remote` reaches it (see [`crate::remote`]).
    pub fn over_ssh(stream: UnixStream, remote: crate::remote::Remote) -> Link {
        let mut link = Link::over(stream, false);
        Rc::get_mut(&mut link.shared)
            .expect("a new link's its own")
            .remote = Some(remote);
        link
    }

    /// Connects to the host process on `path` as a client of `build`,
    /// whose binary is `exe`.
    #[cfg(test)]
    fn connect_as(path: &Path, build: &str, exe: &Path) -> io::Result<Link> {
        let mut hello = Hello::new(build, false);
        hello.exe = exe.to_path_buf();
        let stream = greeted(path, &hello)?;
        Ok(Link::over(stream, false))
    }

    /// Whether the host is on a thread of this process.
    pub fn is_local(&self) -> bool {
        self.shared.local
    }

    /// The machine the host's on, as ssh is told, if it's another.
    pub fn remote(&self) -> Option<&str> {
        self.shared.remote.as_ref().map(crate::remote::Remote::name)
    }

    /// The app's end of a connection to a host on the other end of
    /// `stream`, in this process if `local`.
    fn over(stream: UnixStream, local: bool) -> Link {
        let (to_host, outbox) = mpsc::channel();
        let (all_sent, sent) = mpsc::channel::<()>();
        let routes = Arc::new(Mutex::new(Some(HashMap::new())));
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
                std::thread::spawn(move || {
                    write_frames(stream, outbox);
                    drop(all_sent);
                });
            }
            // As if the host were gone: calls end unanswered.
            Err(_) => drop(outbox),
        }
        Link {
            shared: Rc::new(Shared {
                to_host,
                next: Cell::new(1),
                next_doc: Cell::new(1),
                routes,
                notices,
                sent: RefCell::new(Some(sent)),
                local,
                remote: None,
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
        // Once the host's gone, or left, it ends unanswered.
        if let Some(routes) = lock(&shared.routes).as_mut() {
            routes.insert(id, sender);
        }
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

    /// An id for a document's copy on the host (see [`crate::replica`]).
    pub fn next_doc(&self) -> DocId {
        let id = self.shared.next_doc.get();
        self.shared.next_doc.set(id + 1);
        id
    }

    /// A link that doesn't keep the host going, as this one does.
    pub fn weak(&self) -> WeakLink {
        WeakLink(Rc::downgrade(&self.shared))
    }

    /// Leaves the host, for another client to come to: its terminals run
    /// on, and dropping their calls from now on doesn't hang up. Calls end,
    /// unanswered, as do those made later, such as saving as the app's
    /// dropped.
    pub fn leave(&self) {
        let _ = self.shared.to_host.send(ToHost::Leave);
        *lock(&self.shared.routes) = None;
    }

    /// What says when everything asked of the host is sent, which is once
    /// the link and its calls are dropped: as when quitting, cancelling
    /// terminals' calls hangs up, which only the host can do.
    pub fn closed(&self) -> Closed {
        Closed(self.shared.sent.borrow_mut().take())
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

/// A [`Link`] that doesn't keep the host going: what it's told once the
/// link and its calls are gone goes nowhere.
pub struct WeakLink(std::rc::Weak<Shared>);

impl WeakLink {
    pub fn tell(&self, tell: Tell) {
        if let Some(shared) = self.0.upgrade() {
            let _ = shared.to_host.send(ToHost::Tell(tell));
        }
    }
}

/// Says when a link's done sending (see [`Link::closed`]).
pub struct Closed(Option<Receiver<()>>);

impl Closed {
    /// Waits until it's done, or `timeout` passes.
    pub fn wait(self, timeout: Duration) {
        if let Some(sent) = self.0 {
            let _ = sent.recv_timeout(timeout);
        }
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
    routes: &Mutex<Routes>,
    notices: Sender<Notice>,
    #[cfg(test)] settled: Sender<()>,
) {
    read_frames(from_host, |output| {
        match output {
            Output::Reply(id, reply) => {
                if let Some(route) = lock(routes).as_ref().and_then(|r| r.get(&id)) {
                    // The call may be gone, its replies unwanted.
                    let _ = route.send(reply);
                }
            }
            Output::End(id) => {
                if let Some(routes) = lock(routes).as_mut() {
                    routes.remove(&id);
                }
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
    *lock(routes) = None;
    let _ = notices.send(Notice::Gone);
    wake::wake();
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

/// The listening socket this host process was handed, if it was.
pub fn listener_from_env() -> Option<UnixListener> {
    let fd = socket_from_env(LISTENER_VARIABLE)?;
    Some(unsafe { UnixListener::from_raw_fd(fd) })
}

/// The socket handed to this process by descriptor, as the environment
/// variable `name` says, if it was. It's closed on `exec` from then on.
fn socket_from_env(name: &str) -> Option<RawFd> {
    fd_from_env(name, libc::S_IFSOCK)
}

/// The descriptor of a file of type `kind` handed to this process, as the
/// environment variable `name` says, if it was. It's closed on `exec`
/// from then on.
fn fd_from_env(name: &str, kind: libc::mode_t) -> Option<RawFd> {
    let fd: RawFd = std::env::var(name).ok()?.parse().ok()?;
    // Not for the shells it starts.
    std::env::remove_var(name);
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != kind {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Some(fd)
}

/// What a host process that replaced another, of another build, goes on
/// with (see [`upgrade`]): the session the other kept, its terminals, their
/// programs running, and the client that asked for it, greeted already.
pub struct Handover {
    session: Session,
    terminals: Vec<Adopted>,
    client: UnixStream,
}

impl Handover {
    /// What the host process this one replaced handed it, for the session
    /// in `dir`.
    pub fn from_env(dir: &Path) -> io::Result<Handover> {
        let client = socket_from_env(CLIENT_VARIABLE)
            .ok_or_else(|| io::Error::other("No client was handed over."))?;
        let client = unsafe { UnixStream::from_raw_fd(client) };
        let (_, terminals) = crate::attach::read_manifest(dir)
            .ok_or_else(|| io::Error::other("No terminals were handed over."))?;
        let Some(lock) = fd_from_env(LOCK_VARIABLE, libc::S_IFREG) else {
            // Not this one's, after all: they're hung up on.
            for left in &terminals {
                unsafe { libc::close(left.fd) };
            }
            return Err(io::Error::other("The session's lock wasn't handed over."));
        };
        let session = Session::adopt(dir, unsafe { std::fs::File::from_raw_fd(lock) });
        Ok(Handover {
            session,
            terminals,
            client,
        })
    }
}

/// What happens to a host process.
enum Happened {
    /// A client said hello, and is of this build.
    Greeted(UnixStream),
    /// A client of another build said hello, for its binary to take over
    /// from this one, once the client before has left.
    Upgrade(UnixStream, PathBuf),
    /// A client asked to end the session (see [`end_session`]).
    End(UnixStream),
    /// The client of this connection is gone, or left (`true`).
    Ended(u64, bool),
}

/// Runs a host process: serves the clients that connect to `listener`,
/// and to its session's socket while it keeps one, one at a time,
/// keeping terminals' screens if `screens` (see [`crate::term_hub`]). A
/// newer client takes over once the one before has left, having heard
/// [`Notice::Moved`]; one of another build, by having its binary take this
/// process's place (see [`upgrade`]). With `handover`, it goes on from the
/// host process it replaced, `listener` being the session's socket. Ends
/// once the one it serves is gone, unless it keeps a session with
/// terminals running, for another to attach to; or if no client comes
/// within [`FIRST_CLIENT_TIMEOUT`] of starting, or of the last one
/// leaving; or once its session's ended. Then hangs up on its terminals,
/// and removes its sockets.
pub fn serve_listener(listener: UnixListener, screens: bool, handover: Option<Handover>) {
    let path = listener
        .local_addr()
        .ok()
        .and_then(|addr| addr.as_pathname().map(Path::to_path_buf));
    let (happen, happened) = mpsc::channel();
    let state = Arc::new(State::new(screens, Some(happen.clone())));
    // The client to serve next, once the one served leaves, until when it
    // waits for that, and the binary to start for it, if it's of another
    // build.
    let mut waiting: Option<(UnixStream, Instant, Option<PathBuf>)> = None;
    if let Some(handover) = handover {
        // Listened on already.
        *lock(&state.socket) = path.clone().map(|path| (path, listener.as_raw_fd()));
        *lock(&state.session) = Some(handover.session);
        state.hub.adopt(handover.terminals);
        waiting = Some((handover.client, Instant::now(), None));
    }
    take_clients(listener, happen.clone());
    // The connection served, by its number, and its host thread's inbox.
    let mut serving: Option<(u64, Sender<Input>)> = None;
    // The client that asked to end the session, told once it has.
    let mut ender = None;
    let mut connections = 0;
    // Until when it waits for a client with no other reason to go on.
    let mut alone_until = Instant::now() + FIRST_CLIENT_TIMEOUT;
    loop {
        if serving.is_none() {
            if let Some((stream, _, exe)) = waiting.take() {
                if let Some(exe) = exe {
                    match upgrade(&state, &exe, stream, path.as_deref()) {
                        #[cfg(test)]
                        Ok((listener, handover)) => {
                            return serve_listener(listener, screens, Some(handover))
                        }
                        Err(err) => eprintln!("cue: can't start {}: {err}", exe.display()),
                    }
                    continue;
                }
                connections += 1;
                let id = connections;
                // Not waiting for one any more.
                alone_until = Instant::now();
                let (to_host, inbox) = mpsc::channel();
                serving = Some((id, to_host.clone()));
                let state = Arc::clone(&state);
                let ended = happen.clone();
                std::thread::spawn(move || {
                    let left = serve_stream(stream, (to_host, inbox), &state);
                    let _ = ended.send(Happened::Ended(id, left));
                });
            }
        }
        let next = match (&serving, &waiting) {
            (_, Some((_, until, _))) => {
                happened.recv_timeout(until.saturating_duration_since(Instant::now()))
            }
            (Some(_), None) => happened.recv().map_err(|_| RecvTimeoutError::Disconnected),
            // Looking now and then whether there's still a reason to wait.
            (None, None) => happened.recv_timeout(DETACHED_POLL),
        };
        match next {
            Ok(happened @ (Happened::Greeted(_) | Happened::Upgrade(..))) => {
                let (stream, exe) = match happened {
                    Happened::Upgrade(stream, exe) => (stream, Some(exe)),
                    Happened::Greeted(stream) => (stream, None),
                    _ => unreachable!(),
                };
                // Told once: a third client coming meanwhile replaces the
                // second, which is hung up on.
                if let (Some((_, served)), None) = (&serving, &waiting) {
                    let _ = served.send(Input::Moving);
                }
                waiting = Some((stream, Instant::now() + MOVE_TIMEOUT, exe));
            }
            Ok(Happened::End(stream)) => {
                if let Some((_, served)) = serving.take() {
                    let _ = served.send(Input::Ended);
                }
                if let Some(session) = lock(&state.session).take() {
                    session.end();
                }
                ender = Some(stream);
                break;
            }
            Ok(Happened::Ended(id, left)) => {
                if serving.as_ref().is_some_and(|(serving, _)| *serving == id) {
                    serving = None;
                    if left {
                        // As for a restart: one may be on its way.
                        alone_until = Instant::now() + FIRST_CLIENT_TIMEOUT;
                    } else if waiting.is_none() && !state.keeps_going() {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) if waiting.is_some() => {
                // Taking too long to leave: the next is served regardless.
                if let Some((_, served)) = serving.take() {
                    let _ = served.send(Input::Evicted);
                }
            }
            Err(RecvTimeoutError::Timeout)
                if state.keeps_going() || Instant::now() < alone_until => {}
            Err(_) => break,
        }
    }
    state.hub.hang_up_all();
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    if let Some((socket, _)) = lock(&state.socket).take() {
        let _ = std::fs::remove_file(socket);
    }
    drop(ender);
}

/// Starts the cue at `exe` in this host process's place, for `client`, of
/// its build: it takes up the session, its terminals, their programs
/// running, and the session's socket, and serves `client` first (see
/// [`Handover`]). The socket the process was started on, `own`, goes.
/// Only for a host that keeps a session, whose folder the terminals are
/// handed over in. Returns only if it can't; in tests, with what the new
/// host's to go on with instead, in this same process.
fn upgrade(
    state: &State,
    exe: &Path,
    client: UnixStream,
    own: Option<&Path>,
) -> io::Result<Replaced> {
    let Some((socket, listener)) = lock(&state.socket).clone() else {
        return Err(io::Error::other("Not a session."));
    };
    let Some(session) = lock(&state.session).take() else {
        return Err(io::Error::other("Not a session."));
    };
    // Held on to across `exec`, so that no other cue takes it meanwhile.
    let (dir, session_lock) = session.into_lock();
    let terminals = state.hub.release_all();
    let replaced = crate::attach::write_manifest(&dir, false, &terminals).and_then(|()| {
        if own.is_some_and(|own| own != socket) {
            // No client's to come to it from now on.
            let _ = std::fs::remove_file(own.unwrap());
        }
        replace(exe, &dir, listener, &session_lock, client)
    });
    // Going on as before: the terminals are taken back.
    let err = match replaced {
        Ok(replaced) => return Ok(replaced),
        Err(err) => err,
    };
    let (_, terminals) = crate::attach::read_manifest(&dir).unwrap_or((false, terminals));
    state.hub.adopt(terminals);
    *lock(&state.session) = Some(Session::adopt(&dir, session_lock));
    Err(err)
}

/// What a host goes on with once another build's taken its place: nothing,
/// `exec` having replaced it; in tests, the new host's socket and what it
/// was handed, for this same process to go on with.
#[cfg(not(test))]
type Replaced = std::convert::Infallible;
#[cfg(test)]
type Replaced = (UnixListener, Handover);

/// Replaces this process with the host at `exe`, for the session in
/// `dir`, handing it the session's socket, `listener`, its lock, and
/// `client`. Returns only if it can't.
#[cfg(not(test))]
fn replace(
    exe: &Path,
    dir: &Path,
    listener: RawFd,
    session_lock: &std::fs::File,
    client: UnixStream,
) -> io::Result<Replaced> {
    let kept = [listener, session_lock.as_raw_fd(), client.as_raw_fd()];
    for fd in kept {
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    }
    let err = Command::new(exe)
        .arg("--host")
        .arg("--adopt")
        .arg(dir)
        .env(LISTENER_VARIABLE, listener.to_string())
        .env(LOCK_VARIABLE, session_lock.as_raw_fd().to_string())
        .env(CLIENT_VARIABLE, client.as_raw_fd().to_string())
        .exec();
    for fd in kept {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Err(err)
}

/// In tests, the new host is the same process's: what it's handed is
/// what `exec` would keep.
#[cfg(test)]
fn replace(
    _exe: &Path,
    dir: &Path,
    listener: RawFd,
    session_lock: &std::fs::File,
    client: UnixStream,
) -> io::Result<Replaced> {
    let listener = unsafe { libc::fcntl(listener, libc::F_DUPFD_CLOEXEC, 0) };
    let listener = unsafe { UnixListener::from_raw_fd(listener) };
    let (_, terminals) = crate::attach::read_manifest(dir).unwrap_or_default();
    let session = Session::adopt(dir, session_lock.try_clone()?);
    let handover = Handover {
        session,
        terminals,
        client,
    };
    Ok((listener, handover))
}

/// Takes the clients that connect to `listener`, on a thread of its own,
/// for the host process's loop.
fn take_clients(listener: UnixListener, happen: Sender<Happened>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                // Out of descriptors, say: a moment may free some.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            let happen = happen.clone();
            // Apart, so a client that says nothing holds up no other.
            std::thread::spawn(move || {
                if let Some(happened) = greet(stream) {
                    let _ = happen.send(happened);
                }
            });
        }
    });
}

/// What the client on `stream` is there for, once it's said hello: to be
/// served, unless it's in one of the host's terminals, by this process, or
/// if it's of another build, by its binary in this one's place; or to end
/// the session. Told why not, if it's none of those.
fn greet(stream: UnixStream) -> Option<Happened> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    let hello: Hello = read_frame(&mut &stream).ok()?;
    let build = crate::attach::build();
    let welcome = if hello.end {
        Ok(())
    } else if hello.inside == Some(std::process::id()) {
        Err("A session can't be shown in one of its own terminals.".to_string())
    } else if hello.build != build && !hello.exe.is_file() {
        Err(format!(
            "The host is another build of cue ({build}, not {}), and {} isn't there to start.",
            hello.build,
            hello.exe.display()
        ))
    } else {
        Ok(())
    };
    write_frame(&mut &stream, &welcome).ok()?;
    stream.set_read_timeout(None).ok()?;
    welcome.ok()?;
    Some(match hello.end {
        true => Happened::End(stream),
        false if hello.build != build => Happened::Upgrade(stream, hello.exe),
        false => Happened::Greeted(stream),
    })
}

/// Starts a host process (`cue --host`), listening on a socket of its own,
/// whose path it returns. It ends if no client comes soon.
pub fn spawn_host() -> io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!("/tmp/cue-{uid}/host-{}.sock", std::process::id()));
    let listener = crate::attach::listen(&path)?;
    listener.set_nonblocking(false)?;
    let fd = listener.as_raw_fd();
    let mut command = Command::new(crate::relocate::exe());
    command
        .arg("--host")
        .env(LISTENER_VARIABLE, fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::relocate::pass_on(&mut command);
    // A session of its own, so that the terminal's signals aren't its;
    // and the listener, kept across `exec`.
    unsafe {
        command.pre_exec(move || {
            libc::setsid();
            libc::fcntl(fd, libc::F_SETFD, 0);
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(listener);
    std::thread::spawn(move || child.wait());
    Ok(path)
}

/// A connection to the host process listening on `path`, as a client of
/// this build, once it's said hello.
pub fn connect_stream(path: &Path) -> io::Result<UnixStream> {
    greeted(path, &Hello::new(crate::attach::build(), false))
}

/// What each of `paths` is, or `None` where nothing is.
pub fn look(paths: &[PathBuf]) -> Vec<Option<Look>> {
    paths
        .iter()
        .map(|path| {
            let real = std::fs::canonicalize(path).ok()?;
            let is_dir = real.is_dir();
            Some(Look { real, is_dir })
        })
        .collect()
}

/// A connection to the host process on `path`, once it's taken `hello`.
fn greeted(path: &Path, hello: &Hello) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    write_frame(&mut &stream, hello)?;
    let welcome: Result<(), String> = read_frame(&mut &stream)?;
    welcome.map_err(io::Error::other)?;
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// Ends the session of the host process on `path`, as `cue --end` does:
/// the client it serves hears so, its terminals are hung up on, what it
/// kept is removed, and the host ends. Waits until it's done.
pub fn end_session(path: &Path) -> io::Result<()> {
    let stream = greeted(path, &Hello::new(crate::attach::build(), true))?;
    // It hangs up once it's done.
    let mut rest = Vec::new();
    let _ = (&stream).read_to_end(&mut rest);
    Ok(())
}

/// Serves the app on the other end of `stream`, until it's gone, or
/// another takes over: the host's thread takes what comes to `inbox`.
/// Returns whether it left, rather than went.
fn serve_stream(
    stream: UnixStream,
    (to_host, inbox): (Sender<Input>, Receiver<Input>),
    state: &Arc<State>,
) -> bool {
    let (output, outbox) = mpsc::channel();
    let Ok(reading) = stream.try_clone() else {
        return false;
    };
    let from_app = to_host.clone();
    std::thread::spawn(move || {
        read_frames(reading, |message: ToHost| {
            // What comes after leaving isn't for the host.
            let more = !matches!(message, ToHost::Leave);
            from_app.send(message.into()).is_ok() && more
        });
        let _ = from_app.send(Input::Shutdown);
    });
    std::thread::spawn(move || write_frames(stream, outbox));
    serve(inbox, to_host, output, Arc::clone(state))
}

/// The host's thread: takes calls, tells, and the watcher's changes, until
/// the app's gone. Returns whether it left, rather than went.
fn serve(
    inbox: Receiver<Input>,
    host_inbox: Sender<Input>,
    output: Sender<Output>,
    state: Arc<State>,
) -> bool {
    let nudge = host_inbox.clone();
    let mut watcher = Watcher::new(move || {
        let _ = nudge.send(Input::Watched);
    });
    // Each call in progress, by whether it was cancelled.
    let mut calls: HashMap<CallId, Arc<AtomicBool>> = HashMap::new();
    // The terminal each terminal call hears of, and its token for it.
    let mut terminals: HashMap<CallId, (TermId, Token)> = HashMap::new();
    // Where each git status call hears what it's to know.
    let mut statuses: HashMap<CallId, Sender<git::Nudge>> = HashMap::new();
    // What the app has: its documents' copies, which its calls read.
    let conn = Arc::new(Connection::default());
    let mut left = false;
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
                #[cfg(test)]
                let opened = state
                    .hub
                    .open(start, responder, lock(&state.config).clone());
                #[cfg(not(test))]
                let opened = state.hub.open(start, responder);
                terminals.insert(id, opened);
            }
            Ok(Input::Call(id, Request::GitStatus { roots })) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                calls.insert(id, Arc::clone(&cancelled));
                let responder = Responder {
                    id,
                    output: output.clone(),
                    inbox: host_inbox.clone(),
                    cancelled,
                };
                let (nudge, nudges) = mpsc::channel();
                statuses.insert(id, nudge);
                std::thread::spawn(move || git::follow(&roots, nudges, &responder));
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
                // As they are now, before the edits that come after.
                let texts = texts_for(&request, &lock(&conn.copies));
                let conn = Arc::clone(&conn);
                std::thread::spawn(move || answer(request, &state, &conn, texts, responder));
            }
            Ok(Input::Cancel(id)) => {
                if let Some(cancelled) = calls.remove(&id) {
                    cancelled.store(true, Ordering::Relaxed);
                }
                // The app's done with it.
                if let Some((terminal, _)) = terminals.remove(&id) {
                    state.hub.control(terminal, pty::Control::HangUp);
                }
                statuses.remove(&id);
            }
            Ok(Input::Finished(id)) => {
                calls.remove(&id);
                terminals.remove(&id);
                statuses.remove(&id);
            }
            Ok(Input::Tell(Tell::GitAgain {
                call,
                asked,
                restart,
            })) => {
                if let Some(status) = statuses.get(&call) {
                    let _ = status.send(git::Nudge::Asked { asked, restart });
                }
            }
            Ok(Input::Tell(Tell::Term { call, control })) => {
                if let Some(&(terminal, _)) = terminals.get(&call) {
                    state.hub.control(terminal, control);
                }
            }
            // Here, before the calls that come after are answered.
            Ok(Input::Tell(Tell::ReloadConfig)) => state.reload_config(),
            // Here too, before the terminals asked for after.
            Ok(Input::Tell(Tell::Colors(colors))) => state.hub.set_colors(colors),
            Ok(Input::Tell(Tell::Open { doc, text })) => lock(&conn.copies).open(doc, text),
            Ok(Input::Tell(Tell::Edit { doc, version, edit })) => {
                if lock(&conn.copies).edit(doc, version, &edit) {
                    let acked = Notice::Acked { doc, version };
                    let _ = output.send(Output::Notice(acked));
                }
            }
            Ok(Input::Tell(Tell::Close(doc))) => lock(&conn.copies).close(doc),
            Ok(Input::Tell(Tell::Watch(folders))) => {
                watcher.watch(folders.into_iter().filter(|f| f.is_dir()).collect());
            }
            Ok(Input::Watched) | Err(RecvTimeoutError::Timeout) => {}
            #[cfg(test)]
            Ok(Input::Settle) => {
                let _ = output.send(Output::Settled);
            }
            Ok(Input::Moving) => {
                let _ = output.send(Output::Notice(Notice::Moved));
            }
            Ok(Input::Ended) => {
                let _ = output.send(Output::Notice(Notice::Ended));
                break;
            }
            Ok(Input::Left) => {
                left = true;
                break;
            }
            Ok(Input::Evicted | Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(mut changes) = watcher.poll() {
            for status in statuses.values() {
                let _ = status.send(git::Nudge::Changed);
            }
            changes.look();
            let _ = output.send(Output::Notice(Notice::DiskChanged(changes)));
        }
    }
    for cancelled in calls.values() {
        cancelled.store(true, Ordering::Relaxed);
    }
    // Running on, for another client, or until the host's done.
    for &(terminal, token) in terminals.values() {
        state.hub.unwatch(terminal, token);
    }
    left
}

/// What the host keeps from one call to the next, and one client to the
/// next.
struct State {
    symbols: symbols::Cache,
    /// The session this host keeps, if it's one.
    session: Mutex<Option<Session>>,
    hub: Hub,
    /// Where a host process's loop takes clients, connecting to the
    /// session's socket too while it keeps one.
    clients: Option<Sender<Happened>>,
    /// The session's socket, while it's listened on, and its descriptor.
    socket: Mutex<Option<(PathBuf, RawFd)>>,
    /// The settings the app said to use, for tests (see
    /// [`Link::set_config`]).
    #[cfg(test)]
    config: Mutex<Option<Arc<crate::config::Config>>>,
}

impl State {
    /// Keeping terminals' screens if `screens` (see [`crate::term_hub`]),
    /// and in a host process, taking clients to `clients`.
    fn new(screens: bool, clients: Option<Sender<Happened>>) -> State {
        State {
            symbols: symbols::Cache::default(),
            session: Mutex::default(),
            hub: Hub::new(screens),
            clients,
            socket: Mutex::default(),
            #[cfg(test)]
            config: Mutex::default(),
        }
    }

    /// Whether a host process goes on with no client: while it keeps a
    /// session with terminals running, for one to attach to.
    fn keeps_going(&self) -> bool {
        lock(&self.session).is_some() && self.hub.count() > 0
    }

    /// Listens on the socket of `session`, the host's, for clients coming
    /// back to it, if this is a host process; or stops, with none.
    fn listen_for(&self, session: Option<&Session>) {
        let Some(clients) = &self.clients else {
            return;
        };
        let path = session.map(|session| session::socket_path(session.dir()));
        let mut socket = lock(&self.socket);
        if socket.as_ref().map(|(socket, _)| socket) == path.as_ref() {
            return;
        }
        if let Some((old, _)) = socket.take() {
            let _ = std::fs::remove_file(old);
        }
        let Some(path) = path else {
            return;
        };
        let listener = crate::attach::listen(&path).and_then(|listener| {
            listener.set_nonblocking(false)?;
            Ok(listener)
        });
        match listener {
            Ok(listener) => {
                *socket = Some((path, listener.as_raw_fd()));
                take_clients(listener, clients.clone());
            }
            Err(err) => eprintln!("cue: can't listen on the session's socket: {err}"),
        }
    }

    /// Reads the settings file again, if this is a host process. What's
    /// wrong with it goes to the log: the client says what's wrong with
    /// its own.
    fn reload_config(&self) {
        if self.clients.is_none() {
            return;
        }
        let (config, warnings) = crate::config::load();
        for warning in warnings {
            eprintln!("cue: {warning}");
        }
        crate::config::set(config);
    }

    /// Puts the settings the app told of in use on this thread, for tests.
    fn use_config(&self) {
        #[cfg(test)]
        if let Some(config) = lock(&self.config).clone() {
            crate::config::set((*config).clone());
        }
    }
}

/// What the host keeps of a connection's app, for its calls.
#[derive(Default)]
struct Connection {
    /// The app's documents' copies.
    copies: Mutex<Copies>,
    /// The list of files the app was last sent.
    listed: Mutex<file_index::Listed>,
}

/// The texts of the app's `copies` of documents that `request` reads.
fn texts_for(request: &Request, copies: &Copies) -> Texts {
    match request {
        Request::Save { doc, version, .. } => copies.take([(*doc, Some(*version))]),
        Request::Search { unsaved, .. } => copies.take(unsaved.values().map(|&doc| (doc, None))),
        Request::Session(SessionCall::Save(snapshot)) => {
            copies.take(snapshot.texts.iter().map(|&(_, doc)| (doc, None)))
        }
        _ => Texts::default(),
    }
}

/// Does what `request` asks, replying as it goes: with what the host keeps
/// of the app's connection, `conn`, such as its documents' copies, which
/// loading adds to, and the `texts` of them it reads, taken as it came.
fn answer(
    request: Request,
    state: &State,
    conn: &Connection,
    mut texts: Texts,
    responder: Responder,
) {
    state.use_config();
    match request {
        Request::ListFiles {
            workspace,
            progress,
            since,
        } => file_index::list(&workspace, progress, since, &conn.listed, &responder),
        Request::Search {
            workspace,
            query,
            unsaved,
        } => {
            let unsaved = unsaved
                .into_iter()
                .filter_map(|(path, doc)| Some((path, texts.remove(doc).ok()?)))
                .collect();
            search::run(&workspace, &query, unsaved, &responder)
        }
        Request::IndexSymbols { files, progress } => {
            symbols::index(&files, &state.symbols, progress, &responder)
        }
        Request::Load { path, doc } => {
            let loaded = document::load(&path).map_err(|err| err.to_string());
            if let Some(doc) = doc {
                let text = loaded.as_ref().ok().map(|loaded| loaded.text.as_str());
                lock(&conn.copies).loaded(doc, text);
            }
            responder.reply(Reply::Loaded(loaded));
        }
        Request::Save {
            path,
            doc,
            version: _,
            line_ending,
            expected,
            make_folder,
        } => {
            let path = document::resolve(&path);
            // Taken as of it already.
            let written = texts
                .get(doc)
                .map_err(Unsaved::Failed)
                .and_then(|text| document::write(&path, text, line_ending, expected, make_folder));
            let written = written.map(|stamp| Written {
                real: std::fs::canonicalize(&path).ok(),
                path,
                stamp,
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
            responder.reply(Reply::Looked(look(&paths)));
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
        // Answered apart, in `serve`, which knows the link's terminals.
        Request::Terminal(_) => {}
        Request::Session(mut call) => {
            if let SessionCall::Save(snapshot) = &mut call {
                // Those the client didn't send, being the host's.
                for (id, on_host) in std::mem::take(&mut snapshot.host_screens) {
                    if let Some(screen) = state.hub.text(on_host) {
                        snapshot.screens.push((id, screen));
                    }
                }
            }
            let mut session = lock(&state.session);
            let answer = session::run(call, &mut session, &texts);
            state.listen_for(session.as_ref());
            drop(session);
            responder.reply(Reply::Session(answer));
        }
        Request::Terminals => {
            responder.reply(Reply::Terminals(state.hub.list()));
        }
        Request::About => {
            responder.reply(Reply::About(About::here()));
        }
        Request::Sessions => {
            let sessions = session::default_dir()
                .map(|dir| session::list(&dir))
                .unwrap_or_default();
            responder.reply(Reply::Sessions(sessions));
        }
        Request::Git(call) => {
            responder.reply(Reply::Git(git::run(&call)));
        }
        // Answered apart, in `serve`, which tells it what changes.
        Request::GitStatus { .. } => {}
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
            since: None,
        });
        match call.wait() {
            Some(Reply::Files {
                files, truncated, ..
            }) => {
                assert_eq!(files.unpack(), [root.join("a.txt")]);
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
        while lock(&link.shared.routes)
            .as_ref()
            .is_some_and(|routes| routes.contains_key(&id))
        {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A host process's loop, on a thread, listening on a socket in
    /// `name`'s fixture folder, keeping terminals' screens if `screens`.
    fn host_process(name: &str, screens: bool) -> (PathBuf, std::thread::JoinHandle<()>) {
        let path = fixture(name).join("host.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (
            path,
            std::thread::spawn(move || serve_listener(listener, screens, None)),
        )
    }

    /// Waits for `host` to end, which it does once its client's gone.
    fn ends(host: std::thread::JoinHandle<()>) {
        let start = Instant::now();
        while !host.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(5), "the host goes on");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_host_process_serves_its_client_until_it_goes() {
        let (path, host) = host_process("process", false);
        let root = path.parent().unwrap().to_path_buf();
        let link = Link::connect(&path).unwrap();
        assert!(!link.is_local());
        let call = link.call(Request::Look {
            paths: vec![root.clone()],
        });
        match call.wait() {
            Some(Reply::Looked(looked)) => {
                assert_eq!(
                    looked,
                    [Some(Look {
                        real: root,
                        is_dir: true
                    })]
                );
            }
            other => panic!("{other:?}"),
        }
        drop(call);
        drop(link);
        ends(host);
        assert!(!path.exists(), "the socket's removed");
    }

    /// Waits for `link` to hear something `wanted`.
    fn hears(link: &Link, wanted: impl Fn(&Notice) -> bool) {
        let start = Instant::now();
        while !link.notices().iter().any(&wanted) {
            assert!(start.elapsed() < Duration::from_secs(5), "no notice");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_client_that_connects_takes_over_once_the_one_before_leaves() {
        let (path, host) = host_process("taken", false);
        let first = Link::connect(&path).unwrap();
        let held = first.call(Request::Hold);
        let second = Link::connect(&path).unwrap();
        let waiting = second.call(Request::Look { paths: Vec::new() });
        // The first hears it's moving, and is served until it leaves.
        hears(&first, |n| matches!(n, Notice::Moved));
        let call = first.call(Request::Look { paths: Vec::new() });
        assert!(matches!(call.wait(), Some(Reply::Looked(_))));
        assert!(matches!(waiting.try_recv(), Err(TryRecvError::Empty)));
        first.leave();
        assert!(held.wait().is_none());
        // Then the second's served, and the host goes on without the first.
        assert!(matches!(waiting.wait(), Some(Reply::Looked(_))));
        drop((call, held, first));
        assert!(!host.is_finished());
        drop((waiting, second));
        ends(host);
    }

    #[test]
    fn a_client_that_doesnt_leave_is_let_go_of() {
        let (path, host) = host_process("evicted", false);
        let first = Link::connect(&path).unwrap();
        let held = first.call(Request::Hold);
        let second = Link::connect(&path).unwrap();
        let call = second.call(Request::Look { paths: Vec::new() });
        // Served once the first's had its time.
        assert!(matches!(call.wait(), Some(Reply::Looked(_))));
        assert!(held.wait().is_none());
        drop((call, second, held, first));
        ends(host);
    }

    #[test]
    fn a_client_in_one_of_the_hosts_terminals_is_refused() {
        let (path, host) = host_process("inside", false);
        let mut hello = Hello::new(crate::attach::build(), false);
        hello.inside = Some(std::process::id());
        let err = greeted(&path, &hello).err().unwrap();
        assert!(err.to_string().contains("its own terminals"), "{err}");
        drop(Link::connect(&path).unwrap());
        ends(host);
    }

    #[test]
    fn a_client_of_another_build_without_its_binary_is_refused() {
        let (path, host) = host_process("refused", false);
        let gone = path.parent().unwrap().join("gone");
        let err = Link::connect_as(&path, "another", &gone).err().unwrap();
        assert!(err.to_string().contains("another build"), "{err}");
        // It waits on for a client of its own build.
        let link = Link::connect(&path).unwrap();
        drop(link);
        ends(host);
    }

    /// Takes what `call`'s terminal writes until it's written `text`.
    fn wait_for_output(call: &Call, text: &str) {
        let start = Instant::now();
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(text) {
            assert!(start.elapsed() < Duration::from_secs(20), "no {text}");
            match call.try_recv() {
                Ok(Reply::Term(pty::Event::Output(bytes))) => output.extend(bytes),
                Ok(_) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => panic!("the terminal ended"),
            }
        }
    }

    #[test]
    fn a_terminal_runs_on_for_the_next_client_with_its_screen() {
        // The host's emulators take the native core, one thread's at a time.
        let _serial = crate::test_serial();
        let (path, host) = host_process("attach", true);
        let root = path.parent().unwrap().to_path_buf();
        let first = Link::connect(&path).unwrap();
        // A session, so the host goes on without a client.
        let sessions = root.join("sessions");
        let opened = first.call(Request::Session(SessionCall::Create(sessions.clone())));
        let session_id = match opened.wait() {
            Some(Reply::Session(SessionAnswer::Opened(Ok((id, _))))) => id,
            other => panic!("{other:?}"),
        };
        let term = first.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        let id = match term.wait() {
            Some(Reply::Term(pty::Event::Started(id))) => id,
            other => panic!("{other:?}"),
        };
        let typed = |link: &Link, call: &Call, text: &str| {
            let input = pty::Control::Input(text.as_bytes().to_vec());
            link.tell(Tell::Term {
                call: call.id(),
                control: input,
            });
        };
        typed(&first, &term, "echo hi-$((40+2))\r");
        wait_for_output(&term, "hi-42");

        // Another client takes over, at the session's socket: once the
        // first leaves, its call ends, but not the terminal.
        let socket = session::socket_path(&sessions.join(&session_id));
        let second = Link::connect(&socket).unwrap();
        hears(&first, |n| matches!(n, Notice::Moved));
        first.leave();
        while term.wait().is_some() {}
        drop((term, first));
        let reopened = second.call(Request::Session(SessionCall::Open(
            sessions.join(&session_id),
        )));
        assert!(matches!(
            reopened.wait(),
            Some(Reply::Session(SessionAnswer::Opened(Ok(_))))
        ));
        let running = second.call(Request::Terminals);
        assert!(matches!(running.wait(), Some(Reply::Terminals(ids)) if ids == [id]));
        let attached = second.call(Request::Terminal(pty::Start::Attach(id)));
        match attached.wait() {
            Some(Reply::Term(pty::Event::Screen { screen, cols, rows })) => {
                assert!(String::from_utf8_lossy(&screen).contains("hi-42"));
                assert_eq!((cols, rows), (80, 24));
            }
            other => panic!("{other:?}"),
        }
        typed(&second, &attached, "echo again-$((1+1))\r");
        wait_for_output(&attached, "again-2");

        // Closing it hangs up; with no terminals left, the host ends.
        drop((attached, reopened, running, second));
        ends(host);
        assert!(!socket.exists(), "the session's socket is removed");
    }

    #[test]
    fn a_session_save_has_the_host_write_screens_it_keeps() {
        let _serial = crate::test_serial();
        let (path, host) = host_process("host-screens", true);
        let root = path.parent().unwrap().to_path_buf();
        let link = Link::connect(&path).unwrap();
        let sessions = root.join("sessions");
        let opened = link.call(Request::Session(SessionCall::Create(sessions.clone())));
        let dir = match opened.wait() {
            Some(Reply::Session(SessionAnswer::Opened(Ok((_, dir))))) => dir,
            other => panic!("{other:?}"),
        };
        let term = link.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        let id = match term.wait() {
            Some(Reply::Term(pty::Event::Started(id))) => id,
            other => panic!("{other:?}"),
        };
        link.tell(Tell::Term {
            call: term.id(),
            control: pty::Control::Input(b"echo kept-$((6*7))\r".to_vec()),
        });
        wait_for_output(&term, "kept-42");
        let snapshot = session::Snapshot {
            state: session::State::default(),
            texts: Vec::new(),
            screens: Vec::new(),
            host_screens: vec![(3, id)],
            prune_screens: false,
        };
        let saved = link.call(Request::Session(SessionCall::Save(Box::new(snapshot))));
        assert!(matches!(
            saved.wait(),
            Some(Reply::Session(SessionAnswer::Saved(Ok(_))))
        ));
        let screen = fs::read_to_string(dir.join("terminals").join(session::screen_name(3)));
        assert!(
            screen.unwrap().contains("kept-42"),
            "the host's screen is saved"
        );
        drop((term, saved, opened, link));
        ends(host);
    }

    #[test]
    fn a_client_of_another_build_has_its_binary_take_over_the_terminals() {
        let _serial = crate::test_serial();
        let (path, host) = host_process("upgrade", true);
        let root = path.parent().unwrap().to_path_buf();
        let first = Link::connect(&path).unwrap();
        let sessions = root.join("sessions");
        let opened = first.call(Request::Session(SessionCall::Create(sessions.clone())));
        let dir = match opened.wait() {
            Some(Reply::Session(SessionAnswer::Opened(Ok((_, dir))))) => dir,
            other => panic!("{other:?}"),
        };
        let term = first.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        let id = match term.wait() {
            Some(Reply::Term(pty::Event::Started(id))) => id,
            other => panic!("{other:?}"),
        };
        let typed = |link: &Link, call: &Call, text: &str| {
            let input = pty::Control::Input(text.as_bytes().to_vec());
            link.tell(Tell::Term {
                call: call.id(),
                control: input,
            });
        };
        typed(&first, &term, "echo hi-$((40+2))\r");
        wait_for_output(&term, "hi-42");

        // Once the first leaves, the host's replaced: in tests, by a host
        // in the same thread, with what `exec` would keep.
        let socket = session::socket_path(&dir);
        let exe = std::env::current_exe().unwrap();
        let second = Link::connect_as(&socket, "another", &exe).unwrap();
        hears(&first, |n| matches!(n, Notice::Moved));
        first.leave();
        while term.wait().is_some() {}
        drop((term, first));
        let reopened = second.call(Request::Session(SessionCall::Open(dir.clone())));
        assert!(matches!(
            reopened.wait(),
            Some(Reply::Session(SessionAnswer::Opened(Ok(_))))
        ));
        assert!(!path.exists(), "the socket it was started on goes");
        let running = second.call(Request::Terminals);
        assert!(matches!(running.wait(), Some(Reply::Terminals(ids)) if ids == [id]));
        let attached = second.call(Request::Terminal(pty::Start::Attach(id)));
        match attached.wait() {
            Some(Reply::Term(pty::Event::Screen { screen, cols, rows })) => {
                assert!(String::from_utf8_lossy(&screen).contains("hi-42"));
                assert_eq!((cols, rows), (80, 24));
            }
            other => panic!("{other:?}"),
        }
        // Its shell's the same one, still running.
        typed(&second, &attached, "echo again-$((1+1))\r");
        wait_for_output(&attached, "again-2");
        let started = second.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        match started.wait() {
            Some(Reply::Term(pty::Event::Started(new))) => assert!(new > id, "ids go on"),
            other => panic!("{other:?}"),
        }

        drop((started, attached, reopened, running, second));
        ends(host);
        assert!(!socket.exists(), "the session's socket is removed");
    }

    #[test]
    fn a_host_waits_a_while_for_a_client_after_one_leaves() {
        let (path, host) = host_process("linger", false);
        let first = Link::connect(&path).unwrap();
        first.leave();
        drop(first);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!host.is_finished(), "it waits, as for a restart");
        let second = Link::connect(&path).unwrap();
        let call = second.call(Request::Look { paths: Vec::new() });
        assert!(matches!(call.wait(), Some(Reply::Looked(_))));
        drop((call, second));
        ends(host);
    }

    #[test]
    fn ending_the_session_hangs_up_and_tells_the_client() {
        let (path, host) = host_process("end", false);
        let root = path.parent().unwrap().to_path_buf();
        let link = Link::connect(&path).unwrap();
        let sessions = root.join("sessions");
        let opened = link.call(Request::Session(SessionCall::Create(sessions)));
        let dir = match opened.wait() {
            Some(Reply::Session(SessionAnswer::Opened(Ok((_, dir))))) => dir,
            other => panic!("{other:?}"),
        };
        let term = link.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        assert!(matches!(
            term.wait(),
            Some(Reply::Term(pty::Event::Started(_)))
        ));
        end_session(&session::socket_path(&dir)).unwrap();
        hears(&link, |n| matches!(n, Notice::Ended));
        assert!(!dir.exists(), "what it kept is removed");
        ends(host);
    }

    #[test]
    fn a_host_process_answers_its_terminals_queries_in_the_clients_colors() {
        let _serial = crate::test_serial();
        let (path, host) = host_process("answers", true);
        let root = path.parent().unwrap().to_path_buf();
        let link = Link::connect(&path).unwrap();
        let palette = crate::theme::TerminalPalette {
            fg: opentui::Rgba::rgb(0xee, 0xee, 0xee),
            bg: opentui::Rgba::rgb(0x12, 0x34, 0x56),
            ansi: None,
        };
        link.tell(Tell::Colors(Colors::from(&palette)));
        let term = link.call(Request::Terminal(pty::Start::Shell {
            cwd: root.clone(),
            or: None,
            cols: 80,
            rows: 24,
            above: Vec::new(),
        }));
        assert!(matches!(
            term.wait(),
            Some(Reply::Term(pty::Event::Started(_)))
        ));
        assert!(matches!(
            term.wait(),
            Some(Reply::Term(pty::Event::Answering))
        ));
        // Asked for its background, while a client hears of it, which
        // doesn't answer: the host does.
        let asked = "stty -echo; printf '\\033]11;?\\007'; read -rs -t 5 -d $'\\a' reply; \
                     stty echo; echo \"bg=${reply#*rgb:};end-$((1+1))\"\r";
        link.tell(Tell::Term {
            call: term.id(),
            control: pty::Control::Input(asked.as_bytes().to_vec()),
        });
        let start = Instant::now();
        let mut output = Vec::new();
        let said = loop {
            assert!(start.elapsed() < Duration::from_secs(20), "no answer");
            match term.try_recv() {
                Ok(Reply::Term(pty::Event::Output(bytes))) => output.extend(bytes),
                Ok(_) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => panic!("the terminal ended"),
            }
            let text = String::from_utf8_lossy(&output);
            if let Some(end) = text.find(";end-2") {
                let from = text[..end].rfind("bg=").unwrap();
                break text[from + 3..end].to_string();
            }
        };
        assert!(said == "1212/3434/5656" || said == "12/34/56", "{said:?}");

        drop((term, link));
        ends(host);
    }

    #[test]
    fn attaching_to_a_terminal_that_isnt_there_fails() {
        let link = Link::local();
        let call = link.call(Request::Terminal(pty::Start::Attach(7)));
        assert!(matches!(
            call.wait(),
            Some(Reply::Term(pty::Event::Failed(_)))
        ));
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
                _ => false,
            });
            if heard {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
