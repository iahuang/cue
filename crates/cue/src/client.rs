//! The `cue` a shell runs (see [`crate::attach`]): it works out what to
//! show, a session of the folder it's run in or a new cue, starts the
//! server for it or finds the one running it, and waits on it.
//!
//! `cue` in a folder of exactly one session's workspace goes back to it,
//! unless a file is named, or it runs in a cue terminal; `--fresh` starts
//! a new cue regardless. `--resume` lists the folder's sessions to choose
//! from, `--list` lists every session, and `--end` ends one.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::attach::{self, Control};
use crate::session::{self, Listing, Session};

/// What the command line asked for.
#[derive(Debug, Default)]
pub struct Args {
    /// The folders and file named, as given.
    pub paths: Vec<PathBuf>,
    /// Choose among the folder's sessions.
    pub resume: bool,
    /// Start a new cue even if the folder has a session.
    pub fresh: bool,
}

/// Opens what `args` say, in a server, and waits on it.
pub fn run(args: Args) -> ExitCode {
    let sessions = session::default_dir();
    let folder = match folder(&args.paths) {
        Ok(folder) => folder,
        Err(err) => return fail(&err.to_string()),
    };
    let names_file = args.paths.iter().any(|path| !path.is_dir());
    // A cue run in a cue terminal is its own, unless asked.
    let nested = std::env::var_os(attach::PID_VARIABLE).is_some();
    let found = match &sessions {
        Some(sessions) if !args.fresh && !names_file => session::matching(sessions, &folder),
        _ => Vec::new(),
    };
    let target = if args.resume {
        if found.is_empty() {
            return fail(&format!("there are no sessions in {}", tilde(&folder)));
        }
        match pick(found, &folder) {
            Some(listing) => Some(listing),
            None => return ExitCode::SUCCESS,
        }
    } else if found.len() == 1 && !nested {
        found.into_iter().next()
    } else {
        None
    };
    match target {
        Some(listing) if listing.live => {
            match attach::request_attach(&session::socket_path(&listing.dir)) {
                Ok(control) => wait(control, None),
                Err(err) => fail(&format!("can't reach session {}: {err}", listing.id)),
            }
        }
        Some(listing) => serve(vec!["--restore".into(), listing.dir.into_os_string()]),
        None => serve(
            args.paths
                .into_iter()
                .map(PathBuf::into_os_string)
                .collect(),
        ),
    }
}

/// The folder sessions are looked for in: the first named, or the
/// current one.
fn folder(paths: &[PathBuf]) -> io::Result<PathBuf> {
    match paths.iter().find(|path| path.is_dir()) {
        Some(folder) => folder.canonicalize(),
        None => std::env::current_dir()?.canonicalize(),
    }
}

/// Lists every session, most recent first.
pub fn list() -> ExitCode {
    let all = session::default_dir()
        .map(|dir| session::list(&dir))
        .unwrap_or_default();
    if all.is_empty() {
        println!("There are no sessions.");
    }
    let now = SystemTime::now();
    for listing in all {
        println!("{}  {}", listing.id, listing.describe(now));
    }
    ExitCode::SUCCESS
}

/// Ends the session `id` names, or else the current folder's only one:
/// its programs are hung up on, and what it kept is removed.
pub fn end(id: Option<String>) -> ExitCode {
    let Some(sessions) = session::default_dir() else {
        return fail("there's nowhere sessions are kept: HOME isn't set");
    };
    let listing = match &id {
        Some(id) => match session::find(&sessions, id) {
            Some(listing) => listing,
            None => return fail(&format!("there's no session {id}")),
        },
        None => {
            let folder = match folder(&[]) {
                Ok(folder) => folder,
                Err(err) => return fail(&err.to_string()),
            };
            let mut found = session::matching(&sessions, &folder);
            match found.len() {
                1 => found.remove(0),
                0 => return fail(&format!("there are no sessions in {}", tilde(&folder))),
                _ => {
                    return fail(&format!(
                        "{} has several sessions; name one (cue --list lists them)",
                        tilde(&folder)
                    ))
                }
            }
        }
    };
    let about = listing.describe(SystemTime::now());
    let ended = match listing.live {
        true => attach::request_end(&session::socket_path(&listing.dir)),
        false => Session::open(&listing.dir).map(Session::end),
    };
    match ended {
        Ok(()) => {
            println!("Ended session {}: {about}", listing.id);
            ExitCode::SUCCESS
        }
        Err(err) => fail(&format!("can't end session {}: {err}", listing.id)),
    }
}

/// Starts a server with `args`, handing it the terminal, and waits on it.
fn serve(args: Vec<OsString>) -> ExitCode {
    let (ours, theirs) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(err) => return fail(&format!("can't start: {err}")),
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => return fail(&format!("can't find cue's binary: {err}")),
    };
    let fd = theirs.as_raw_fd();
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    let mut command = Command::new(exe);
    command
        .arg("--serve")
        .args(args)
        .env(attach::CONTROL_VARIABLE, fd.to_string());
    // A session of its own, so that it outlives this one, and the
    // terminal's signals are this process's.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn();
    drop(theirs);
    match child {
        Ok(child) => {
            let _ = ours.set_nonblocking(true);
            wait(Control::new(ours), Some(child))
        }
        Err(err) => fail(&format!("can't start: {err}")),
    }
}

/// Set by SIGWINCH.
static RESIZED: AtomicBool = AtomicBool::new(false);

extern "C" fn resized(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}

/// Waits while the server shows itself in the terminal, passing on
/// resizes, until it says to go, or goes. `child` is the server, if this
/// process started it.
fn wait(mut control: Control, child: Option<Child>) -> ExitCode {
    unsafe {
        libc::signal(
            libc::SIGWINCH,
            resized as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let mut code = None;
    loop {
        if RESIZED.swap(false, Ordering::Relaxed) {
            control.send("winch");
        }
        let mut fds = [libc::pollfd {
            fd: control.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // A signal interrupts the wait.
        unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
        let messages = match control.receive() {
            Ok(messages) => messages,
            Err(_) => break,
        };
        for message in messages {
            let (kind, text) = message.split_once(' ').unwrap_or((&message, ""));
            match kind {
                "exit" => code = text.parse::<u8>().ok(),
                "detached" => {
                    eprintln!("cue: {text}");
                    return ExitCode::SUCCESS;
                }
                "moved" => {
                    eprintln!("cue: the session moved to another terminal.");
                    return ExitCode::SUCCESS;
                }
                "ended" => {
                    eprintln!("cue: the session was ended.");
                    return ExitCode::SUCCESS;
                }
                "error" => {
                    eprintln!("cue: {text}");
                    return ExitCode::FAILURE;
                }
                _ => {}
            }
        }
    }
    // Gone: it said how it exited, or its exit status does.
    let status = child.and_then(|mut child| child.wait().ok());
    match (code, status) {
        (Some(code), _) => ExitCode::from(code),
        (None, Some(status)) if status.success() => ExitCode::SUCCESS,
        (None, Some(status)) => match status.code() {
            Some(code) => ExitCode::from(code.clamp(1, 255) as u8),
            None => {
                let signal = status.signal().unwrap_or(0);
                eprintln!("cue: the server was killed (signal {signal})");
                ExitCode::FAILURE
            }
        },
        (None, None) => ExitCode::FAILURE,
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("cue: {message}");
    ExitCode::FAILURE
}

/// `path` with `~` for the home folder.
pub fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

// --- choosing a session ---------------------------------------------------------

/// Asks which of `listings`, the sessions in `folder`, to open, with the
/// arrow keys, or a number, and Enter. `None` if none was chosen.
fn pick(listings: Vec<Listing>, folder: &Path) -> Option<Listing> {
    let now = SystemTime::now();
    let lines: Vec<String> = listings.iter().map(|l| l.describe(now)).collect();
    let interactive = unsafe { libc::isatty(0) == 1 && libc::isatty(2) == 1 };
    if !interactive {
        eprintln!("Sessions in {}:", tilde(folder));
        for (listing, line) in listings.iter().zip(&lines) {
            eprintln!("  {}  {line}", listing.id);
        }
        return None;
    }
    let raw = Raw::enable(0)?;
    let width = terminal_width();
    let mut out = io::stderr();
    let room = width.saturating_sub(1);
    let mut title = clip(&format!("Sessions in {}", tilde(folder)), room);
    let hint = "  ↑↓ to choose, Enter to open, Esc to cancel";
    if title.chars().count() + hint.chars().count() <= room {
        title += &format!("\x1b[2m{hint}\x1b[0m");
    }
    let mut selected = 0;
    let mut drawn = 0;
    let chosen = loop {
        // Back to the top of what was drawn, and draw it again.
        let mut frame = String::new();
        if drawn > 0 {
            frame += &format!("\x1b[{drawn}A");
        }
        frame += &format!("\r\x1b[J{title}\r\n");
        for (i, line) in lines.iter().enumerate() {
            // Short of the edge, so that no line wraps.
            let line = clip(&format!("{} {line}", i + 1), width.saturating_sub(5));
            frame += &match i == selected {
                true => format!("\x1b[1m›\x1b[0m \x1b[7m {line} \x1b[0m\r\n"),
                false => format!("   {line}\r\n"),
            };
        }
        drawn = lines.len() + 1;
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
        match read_key() {
            Some(Key::Up) => selected = (selected + lines.len() - 1) % lines.len(),
            Some(Key::Down) => selected = (selected + 1) % lines.len(),
            Some(Key::Digit(n)) if n >= 1 && n <= lines.len() => break Some(n - 1),
            Some(Key::Enter) => break Some(selected),
            Some(Key::Cancel) | None => break None,
            Some(Key::Digit(_)) => {}
        }
    };
    let _ = write!(out, "\x1b[{drawn}A\r\x1b[J");
    let _ = out.flush();
    drop(raw);
    chosen.map(|index| listings.into_iter().nth(index).expect("chosen from them"))
}

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Digit(usize),
}

/// The next key, as a terminal without the kitty keyboard protocol sends
/// it; `None` if stdin closed.
fn read_key() -> Option<Key> {
    loop {
        let mut buf = [0u8; 16];
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        let key = match &buf[..n as usize] {
            b"\x1b[A" | b"\x1bOA" | b"k" => Key::Up,
            b"\x1b[B" | b"\x1bOB" | b"j" => Key::Down,
            b"\r" | b"\n" => Key::Enter,
            b"\x1b" | b"\x03" | b"q" => Key::Cancel,
            [digit @ b'1'..=b'9'] => Key::Digit((digit - b'0') as usize),
            _ => continue,
        };
        return Some(key);
    }
}

/// The terminal's width, or 80.
fn terminal_width() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) } == 0;
    match ok && ws.ws_col > 0 {
        true => ws.ws_col as usize,
        false => 80,
    }
}

/// The first `max` characters of `s`.
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Raw input on a terminal, until dropped: keys come one at a time,
/// without echo.
struct Raw {
    fd: RawFd,
    saved: libc::termios,
}

impl Raw {
    fn enable(fd: RawFd) -> Option<Raw> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        let _ = io::stderr().write_all(b"\x1b[?25l");
        Some(Raw { fd, saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = io::stderr().write_all(b"\x1b[?25h");
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}
