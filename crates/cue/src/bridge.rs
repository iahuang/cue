//! `cue --bridge`: what `cue host:path` runs on `host`, over ssh, with the
//! connection on its stdin and stdout (see [`crate::remote`]).
//!
//! It says it's there ([`MARKER`] and a [`Greeting`]), answers what the
//! client asks before there's an app ([`Ask`]), and once asked to connect,
//! connects to a host process, a new one or one keeping a session, as a
//! client of this build: a host of another build starts this one in its
//! place (see [`crate::host`]). Then it passes along whatever comes either
//! way, until either end goes.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::file_dialog;
use crate::host::{self, Look};
use crate::session::{self, Listing};
use crate::wire::{read_frame, write_frame};

/// Starts each line the bootstrap script and the bridge say, to tell them
/// from what the host's shell prints.
pub const MARKER: &str = "@cue@";

/// What the bridge says first, after the line that says it's there.
#[derive(Debug, Serialize, Deserialize)]
pub struct Greeting {
    /// The source it's built from (see `build.rs`), which the client's
    /// must be.
    pub source: String,
    pub version: String,
    /// The home folder there.
    pub home: Option<PathBuf>,
}

/// What the client asks the bridge, before there's an app.
#[derive(Debug, Serialize, Deserialize)]
pub enum Ask {
    /// The sessions kept there.
    Sessions,
    /// What each path is.
    Look(Vec<PathBuf>),
    /// What's in a folder.
    ReadDir(PathBuf),
    /// To connect to a host process there, and pass its messages along.
    Connect(Target),
}

/// The host process a bridge connects to.
#[derive(Debug, Serialize, Deserialize)]
pub enum Target {
    /// A new one.
    New,
    /// The one running the session in this folder.
    Attach(PathBuf),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Sessions(Vec<Listing>),
    Looked(Vec<Option<Look>>),
    Dir(Result<Vec<file_dialog::Entry>, String>),
    /// Connected, from now on passing messages along; or why not.
    Connected(Result<(), String>),
}

/// Runs the bridge on stdin and stdout, until the client or the host goes.
pub fn run() -> ExitCode {
    // Read and written as they are, unbuffered, so that nothing read
    // ahead is lost once they're passed along.
    let mut input = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    let mut output = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let greeting = Greeting {
        source: env!("CUE_SOURCE").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        home: std::env::var_os("HOME").map(PathBuf::from),
    };
    let greeted =
        write!(output, "\n{MARKER} bridge\n").and_then(|()| write_frame(&mut *output, &greeting));
    if greeted.is_err() {
        return ExitCode::FAILURE;
    }
    let host = loop {
        let Ok(ask) = read_frame::<Ask>(&mut *input) else {
            return ExitCode::SUCCESS;
        };
        let (answer, host) = answer(ask);
        if write_frame(&mut *output, &answer).is_err() {
            return ExitCode::FAILURE;
        }
        if let Some(host) = host {
            break host;
        }
    };
    relay(&input, &output, host);
    ExitCode::SUCCESS
}

/// The answer to `ask`, and once connected, the host process.
fn answer(ask: Ask) -> (Answer, Option<UnixStream>) {
    match ask {
        Ask::Sessions => {
            let sessions = session::default_dir()
                .map(|dir| session::list(&dir))
                .unwrap_or_default();
            (Answer::Sessions(sessions), None)
        }
        Ask::Look(paths) => (Answer::Looked(host::look(&paths)), None),
        Ask::ReadDir(dir) => {
            let entries = file_dialog::read(&dir).map_err(|kind| io::Error::from(kind).to_string());
            (Answer::Dir(entries), None)
        }
        Ask::Connect(target) => {
            let connected = match target {
                Target::New => host::spawn_host().and_then(|path| host::connect_stream(&path)),
                Target::Attach(dir) => host::connect_stream(&session::socket_path(&dir)),
            };
            match connected {
                Ok(stream) => (Answer::Connected(Ok(())), Some(stream)),
                Err(err) => (Answer::Connected(Err(err.to_string())), None),
            }
        }
    }
}

/// Passes what comes on `input` to `host`, and what comes from `host` to
/// `output`, until either end goes.
fn relay(input: &File, output: &File, host: UnixStream) {
    let Ok(to_host) = host.try_clone() else {
        return;
    };
    let input = input.try_clone().expect("stdin can be duplicated");
    std::thread::spawn(move || {
        pass(input, &to_host);
        // The client's gone: the host hears it.
        let _ = to_host.shutdown(std::net::Shutdown::Write);
    });
    pass(host, output);
}

/// Writes what's read from `from` to `to` as it comes, until either end
/// goes. Not [`io::copy`], which on Linux has the kernel splice a pipe and
/// a socket, holding on to what's read until there's more.
fn pass(mut from: impl Read, mut to: impl Write) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if to
            .write_all(&buffer[..read])
            .and_then(|()| to.flush())
            .is_err()
        {
            return;
        }
    }
}
