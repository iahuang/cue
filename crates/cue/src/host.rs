//! The host: what cue does on the machine whose files it edits, apart
//! from the screen. The app (the client) asks it for things by message,
//! and never touches that machine itself, so the host can later run
//! elsewhere: in the session server, or over ssh (see
//! `docs/client-server.md`).
//!
//! For now the host runs on a thread of its own in the same process. The
//! messages are plain data all the same, as they'd be on a wire:
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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::file_index;
use crate::git::{self, Repo};
use crate::search::{self, Query};
use crate::wake;
use crate::watch::{Changes, Watcher};
use crate::workspace::Workspace;

/// Tells a call from the others on the same link.
type CallId = u64;

/// What the app asks the host to do, answered with [`Reply`]s.
#[derive(Debug, Clone)]
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
#[derive(Debug)]
pub enum Reply {
    /// Files found so far, during a [`Request::ListFiles`] with progress.
    FilesFound(Vec<PathBuf>),
    /// Every file, sorted as the picker lists them, and whether some went
    /// unlisted, there being too many.
    Files {
        files: Vec<PathBuf>,
        truncated: bool,
    },
    /// A file with matches, or the colors of one found before.
    Found(search::Found),
    Repos(Vec<Repo>),
    /// How a [`Request::GitSync`] went.
    Synced(Result<(), String>),
    /// The new commit's abbreviated hash, or what went wrong.
    Committed(Result<String, String>),
}

/// What the app asks of the host with no answer.
#[derive(Debug, Clone)]
pub enum Tell {
    /// Watch these folders for changes (see [`crate::watch`]), and no
    /// others. Those that don't exist are left out.
    Watch(BTreeSet<PathBuf>),
}

/// What the host says unasked.
#[derive(Debug)]
pub enum Notice {
    /// Files in the watched folders changed.
    DiskChanged(Changes),
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
    /// Answered once what came before is done with, for tests.
    #[cfg(test)]
    Settle(Sender<()>),
}

/// What the host sends the app.
enum Output {
    Reply(CallId, Reply),
    /// The call has no more replies.
    End(CallId),
    Notice(Notice),
}

/// The app's end of its connection to a host.
#[derive(Clone)]
pub struct Link {
    shared: Rc<Shared>,
}

struct Shared {
    to_host: Sender<Input>,
    next: Cell<CallId>,
    /// Where each call's replies go, until it ends.
    routes: Arc<Mutex<HashMap<CallId, Sender<Reply>>>>,
    notices: Receiver<Notice>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let _ = self.to_host.send(Input::Shutdown);
    }
}

impl Link {
    /// Starts a host in this process, on threads of its own, which stop
    /// once the link and its calls are dropped.
    pub fn local() -> Link {
        let (to_host, inbox) = mpsc::channel();
        let (output, from_host) = mpsc::channel();
        let host_inbox = to_host.clone();
        std::thread::spawn(move || serve(inbox, host_inbox, output));
        let routes: Arc<Mutex<HashMap<CallId, Sender<Reply>>>> = Arc::default();
        let (notify, notices) = mpsc::channel();
        let reader_routes = Arc::clone(&routes);
        std::thread::spawn(move || read(from_host, &reader_routes, notify));
        Link {
            shared: Rc::new(Shared {
                to_host,
                next: Cell::new(1),
                routes,
                notices,
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
        let _ = shared.to_host.send(Input::Call(id, request));
        Call {
            id,
            replies,
            shared: Rc::clone(shared),
        }
    }

    pub fn tell(&self, tell: Tell) {
        let _ = self.shared.to_host.send(Input::Tell(tell));
    }

    /// Waits for the host to take in what was asked of it so far, for
    /// tests: watching starts, for one.
    #[cfg(test)]
    pub fn settle(&self) {
        let (sender, settled) = mpsc::channel();
        let _ = self.shared.to_host.send(Input::Settle(sender));
        let _ = settled.recv();
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

    /// Waits for the next reply, for tests. `None` once the call ended.
    #[cfg(test)]
    pub fn recv(&self) -> Option<Reply> {
        self.replies.recv().ok()
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        // Harmless if it ended already.
        let _ = self.shared.to_host.send(Input::Cancel(self.id));
    }
}

/// Hands what the host sends to the calls it's for and to the notices,
/// waking the main loop, until the host and its calls are all gone.
fn read(
    from_host: Receiver<Output>,
    routes: &Mutex<HashMap<CallId, Sender<Reply>>>,
    notices: Sender<Notice>,
) {
    for output in from_host {
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
        }
        wake::wake();
    }
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

/// The host's thread: takes calls, tells, and the watcher's changes, until
/// the app's gone.
fn serve(inbox: Receiver<Input>, host_inbox: Sender<Input>, output: Sender<Output>) {
    let nudge = host_inbox.clone();
    let mut watcher = Watcher::new(move || {
        let _ = nudge.send(Input::Watched);
    });
    // Each call in progress, by whether it was cancelled.
    let mut calls: HashMap<CallId, Arc<AtomicBool>> = HashMap::new();
    loop {
        let input = match watcher.due() {
            Some(due) => inbox.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match input {
            Ok(Input::Call(id, request)) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                calls.insert(id, Arc::clone(&cancelled));
                let responder = Responder {
                    id,
                    output: output.clone(),
                    inbox: host_inbox.clone(),
                    cancelled,
                };
                std::thread::spawn(move || answer(request, responder));
            }
            Ok(Input::Cancel(id)) => {
                if let Some(cancelled) = calls.remove(&id) {
                    cancelled.store(true, Ordering::Relaxed);
                }
            }
            Ok(Input::Finished(id)) => {
                calls.remove(&id);
            }
            Ok(Input::Tell(Tell::Watch(folders))) => {
                watcher.watch(folders.into_iter().filter(|f| f.is_dir()).collect());
            }
            Ok(Input::Watched) | Err(RecvTimeoutError::Timeout) => {}
            #[cfg(test)]
            Ok(Input::Settle(settled)) => {
                let _ = settled.send(());
            }
            Ok(Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(changes) = watcher.poll() {
            let _ = output.send(Output::Notice(Notice::DiskChanged(changes)));
        }
    }
    for cancelled in calls.values() {
        cancelled.store(true, Ordering::Relaxed);
    }
}

/// Does what `request` asks, replying as it goes.
fn answer(request: Request, responder: Responder) {
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
        match call.recv() {
            Some(Reply::Files { files, truncated }) => {
                assert_eq!(files, [root.join("a.txt")]);
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }
        assert!(call.recv().is_none());
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
