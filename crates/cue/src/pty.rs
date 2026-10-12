//! A pseudoterminal with a program running in it, as a terminal emulator
//! runs a shell.
//!
//! The program gets the pty's terminal end as its controlling terminal,
//! in a session of its own, so the kernel sends it and its jobs their
//! signals: SIGINT for Ctrl+C, SIGWINCH on resize, SIGHUP when the pty
//! closes. cue holds the other end, non-blocking, and reads it when
//! `poll` says it's ready.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Environment variables that describe the terminal cue runs in, which
/// would mislead programs in its terminals.
const HOST_VARIABLES: &[&str] = &[
    "ALACRITTY_WINDOW_ID",
    "COLUMNS",
    "GHOSTTY_RESOURCES_DIR",
    "ITERM_PROFILE",
    "ITERM_SESSION_ID",
    "KITTY_LISTEN_ON",
    "KITTY_PID",
    "KITTY_PUBLIC_KEY",
    "KITTY_WINDOW_ID",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "LINES",
    // Where the host's own terminfo entry is, for its `TERM`.
    "TERMINFO",
    "TERM_SESSION_ID",
    "VTE_VERSION",
    "WEZTERM_PANE",
    "WEZTERM_UNIX_SOCKET",
    "WT_SESSION",
];

pub struct Pty {
    master: OwnedFd,
    /// The program's process id. It's cue's child, to reap: when dropped,
    /// in the background.
    pid: libc::pid_t,
    /// How it ended, once reaped.
    exit: Option<ExitStatus>,
    /// Input the program hasn't taken yet.
    unsent: Vec<u8>,
}

impl Pty {
    /// Starts the user's shell (the `terminal.shell` setting, or `$SHELL`)
    /// as a login shell, in `cwd`, as terminals do.
    pub fn shell(cwd: &Path, cols: u16, rows: u16) -> io::Result<Pty> {
        let shell = crate::config::get()
            .shell
            .clone()
            .map(OsString::from)
            .or_else(|| std::env::var_os("SHELL").filter(|shell| !shell.is_empty()))
            .unwrap_or_else(|| "/bin/sh".into());
        let name = Path::new(&shell).file_name().unwrap_or(shell.as_ref());
        let mut command = Command::new(&shell);
        // A leading dash makes a login shell.
        let mut arg0 = OsString::from("-");
        arg0.push(name);
        command.arg0(arg0);
        Pty::spawn(command, cwd, cols, rows)
    }

    /// Runs `command` in a new pty `cols` by `rows`, in `cwd`.
    pub fn spawn(mut command: Command, cwd: &Path, cols: u16, rows: u16) -> io::Result<Pty> {
        let (master, terminal) = open(cols, rows)?;
        command
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "cue")
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            // So that a cue run in it doesn't show this one there, in
            // itself (see `crate::attach`).
            .env(crate::attach::PID_VARIABLE, std::process::id().to_string());
        for variable in HOST_VARIABLES {
            command.env_remove(variable);
        }
        command
            .stdin(Stdio::from(terminal.try_clone()?))
            .stdout(Stdio::from(terminal.try_clone()?))
            .stderr(Stdio::from(terminal));
        // Only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Reaped by its id (see `exit_status`), so it can be adopted.
        let pid = command.spawn()?.id() as libc::pid_t;
        // Closes cue's copies of the terminal end, so reading the master
        // ends once the program's copies are closed too.
        drop(command);
        set_nonblocking(master.as_raw_fd())?;
        Ok(Pty {
            master,
            pid,
            exit: None,
            unsent: Vec::new(),
        })
    }

    /// The pty whose master is `fd`, with program `pid` running in it,
    /// which this process started, or a process it replaced (see
    /// [`Pty::release`]).
    pub fn adopt(fd: RawFd, pid: libc::pid_t) -> io::Result<Pty> {
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        set_nonblocking(fd)?;
        Ok(Pty {
            master,
            pid,
            exit: None,
            unsent: Vec::new(),
        })
    }

    /// Lets go of the pty without hanging up, for a process that replaces
    /// this one to adopt: its master, kept open across `exec`, and the
    /// program's process id.
    pub fn release(self) -> (RawFd, libc::pid_t) {
        // Neither hung up on nor closed.
        let pty = std::mem::ManuallyDrop::new(self);
        let fd = pty.master.as_raw_fd();
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        (fd, pty.pid)
    }

    /// For polling: readable when the program wrote something, writable
    /// when it can take more input.
    pub fn fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Reads what the program wrote into `buf`: `Ok(0)` if there's nothing
    /// yet, `Err(UnexpectedEof)` once the program, and everything it
    /// started, closed the terminal.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Ok(n as usize);
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
            // What reading a pty with no terminal end open gives on Linux
            // and macOS.
            _ if err.raw_os_error() == Some(libc::EIO) => Err(io::ErrorKind::UnexpectedEof.into()),
            _ => Err(err),
        }
    }

    /// Sends `bytes` to the program, as typed. What it can't take now waits
    /// for [`Pty::flush`].
    pub fn write(&mut self, bytes: &[u8]) {
        self.unsent.extend_from_slice(bytes);
        self.flush();
    }

    /// Sends input that waited for the program to take it.
    pub fn flush(&mut self) {
        while !self.unsent.is_empty() {
            let n =
                unsafe { libc::write(self.fd(), self.unsent.as_ptr().cast(), self.unsent.len()) };
            if n <= 0 {
                let err = io::Error::last_os_error();
                if n < 0 && err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Full, or closed: it waits, or goes with the pty.
                return;
            }
            self.unsent.drain(..n as usize);
        }
    }

    /// Whether input is waiting for the program to take it.
    pub fn has_unsent(&self) -> bool {
        !self.unsent.is_empty()
    }

    /// Resizes the terminal. The kernel tells the program (SIGWINCH).
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = winsize(cols, rows);
        unsafe { libc::ioctl(self.fd(), libc::TIOCSWINSZ as _, &size) };
    }

    /// How the program ended, once it has.
    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        if self.exit.is_none() {
            let mut status = 0;
            match unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } {
                0 => {}
                // Reaped elsewhere: how it ended is lost.
                -1 if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => {
                    self.exit = Some(ExitStatus::from_raw(0));
                }
                -1 => {}
                _ => self.exit = Some(ExitStatus::from_raw(status)),
            }
        }
        self.exit
    }

    /// Whether a program other than the one started, such as a command
    /// the shell ran, has the terminal.
    pub fn is_busy(&self) -> bool {
        let foreground = unsafe { libc::tcgetpgrp(self.fd()) };
        foreground > 0 && foreground != self.pid
    }

    /// The name of the program that has the terminal, such as `vim`.
    #[cfg(target_os = "macos")]
    pub fn foreground_name(&self) -> Option<String> {
        let pid = unsafe { libc::tcgetpgrp(self.fd()) };
        if pid <= 0 {
            return None;
        }
        let mut name = [0u8; 256];
        let len = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        (len > 0).then(|| String::from_utf8_lossy(&name[..len as usize]).into_owned())
    }

    #[cfg(target_os = "linux")]
    pub fn foreground_name(&self) -> Option<String> {
        let pid = unsafe { libc::tcgetpgrp(self.fd()) };
        if pid <= 0 {
            return None;
        }
        let comm = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
        Some(String::from_utf8_lossy(comm.trim_ascii_end()).into_owned())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn foreground_name(&self) -> Option<String> {
        None
    }

    /// The folder the program started (the shell) is working in now, if
    /// it can be found.
    pub fn working_folder(&self) -> Option<PathBuf> {
        working_folder(self.pid)
    }

    /// Tells the program that has the terminal to draw its screen again,
    /// as it does when the terminal is resized (SIGWINCH).
    pub fn ask_to_redraw(&self) {
        let foreground = unsafe { libc::tcgetpgrp(self.fd()) };
        if foreground > 0 {
            unsafe { libc::kill(-foreground, libc::SIGWINCH) };
        }
    }

    /// The folders the program that has the terminal, and the program
    /// started (the shell), are working in now, in that order, as far as
    /// they can be found.
    pub fn working_folders(&self) -> Vec<PathBuf> {
        let foreground = unsafe { libc::tcgetpgrp(self.fd()) };
        let mut folders = Vec::new();
        for pid in [Some(foreground).filter(|&pid| pid > 0), Some(self.pid)]
            .into_iter()
            .flatten()
        {
            if let Some(folder) = working_folder(pid) {
                if !folders.contains(&folder) {
                    folders.push(folder);
                }
            }
        }
        folders
    }
}

#[cfg(target_os = "macos")]
fn working_folder(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast(),
            std::mem::size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let path = &raw[..raw.iter().position(|&b| b == 0)?];
    (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

#[cfg(target_os = "linux")]
fn working_folder(pid: libc::pid_t) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn working_folder(_pid: libc::pid_t) -> Option<PathBuf> {
    None
}

impl Drop for Pty {
    /// Hangs up, as closing a terminal window does: the program and its
    /// jobs get SIGHUP, and the program is reaped in the background.
    fn drop(&mut self) {
        if self.exit_status().is_some() {
            return;
        }
        let pid = self.pid;
        // The program leads its session and process group.
        unsafe { libc::kill(-pid, libc::SIGHUP) };
        std::thread::spawn(move || {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
        });
    }
}

/// A new pty's master and terminal ends, both closed on exec.
fn open(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut terminal = -1;
    let mut size = winsize(cols, rows);
    let status = unsafe {
        libc::openpty(
            &mut master,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    let (master, terminal) =
        unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(terminal)) };
    for fd in [master.as_raw_fd(), terminal.as_raw_fd()] {
        // Otherwise every program started from any terminal would hold
        // them open.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((master, terminal))
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

// --- on the host ------------------------------------------------------------

/// How a terminal starts on the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Start {
    /// The user's shell (see [`Pty::shell`]), in `cwd`, or in `or` if
    /// that isn't a folder (any more).
    Shell {
        cwd: PathBuf,
        or: Option<PathBuf>,
        cols: u16,
        rows: u16,
        /// What the terminal shows above what the shell writes, as a
        /// session's restored (see [`crate::terminal::Terminal::restore`]),
        /// for the host's screen of it (see [`crate::term_hub`]).
        above: Vec<u8>,
    },
    /// A pty a process this one replaced left running (see [`Pty::adopt`]).
    Adopt { fd: RawFd, pid: libc::pid_t },
    /// A terminal the host is running already, which another client
    /// started (see [`crate::term_hub`]).
    Attach(crate::term_hub::TermId),
}

/// What a terminal's program is doing, as the host last looked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// A program other than the shell has the terminal.
    pub busy: bool,
    /// The program that has the terminal, such as `vim`.
    pub program: Option<String>,
    /// The folders the program that has the terminal, and the shell, are
    /// working in, in that order (see [`Pty::working_folders`]).
    pub folders: Vec<PathBuf>,
    /// The folder the shell is working in.
    pub shell_folder: Option<PathBuf>,
}

/// What the host says of a terminal.
#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    /// Which terminal it is, on the host, for attaching to it again.
    Started(crate::term_hub::TermId),
    /// What it shows, for a client attaching to it, as
    /// [`crate::terminal::full_snapshot`] takes it, `cols` by `rows`.
    Screen {
        screen: Vec<u8>,
        cols: u16,
        rows: u16,
    },
    /// What the program wrote.
    Output(Vec<u8>),
    Status(Status),
    /// The shell ended, as `waitpid` said.
    Exited(i32),
    /// The shell couldn't start, for this reason.
    Failed(String),
    /// Let go of without hanging up, for a process that replaces this one
    /// (see [`Pty::release`]).
    Released(RawFd, libc::pid_t),
}

/// What the app has a terminal's host thread do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Control {
    /// Send the program this, as typed.
    Input(Vec<u8>),
    Resize(u16, u16),
    /// See [`Pty::ask_to_redraw`].
    Redraw,
    /// Let go of the pty without hanging up (see [`Event::Released`]).
    Release,
    /// Hang up, as the app's done with it.
    HangUp,
}

/// How often a terminal's program is looked at while it writes nothing.
const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// The host's end of a terminal's controls, which wakes its thread.
pub struct Controller {
    controls: std::sync::mpsc::Sender<Control>,
    wake: std::os::unix::net::UnixStream,
}

impl Controller {
    pub fn send(&self, control: Control) {
        use std::io::Write;
        if self.controls.send(control).is_ok() {
            // Full means a wake is waiting.
            let _ = (&self.wake).write(&[0]);
        }
    }
}

/// The thread's end of a terminal's controls.
pub struct Controls {
    controls: std::sync::mpsc::Receiver<Control>,
    wake: std::os::unix::net::UnixStream,
}

/// A terminal's controls, for the host and its thread.
pub fn controls() -> io::Result<(Controller, Controls)> {
    let (sender, receiver) = std::os::unix::net::UnixStream::pair()?;
    sender.set_nonblocking(true)?;
    receiver.set_nonblocking(true)?;
    let (tx, rx) = std::sync::mpsc::channel();
    Ok((
        Controller {
            controls: tx,
            wake: sender,
        },
        Controls {
            controls: rx,
            wake: receiver,
        },
    ))
}

/// Runs a terminal on the host: starts its program as `start` says, sends
/// what it writes and how it's doing through `send`, which returns false
/// once no one listens, and does what `controls` say, until the program
/// and everything it started close the pty, or it's let go of.
pub fn serve(start: Start, controls: Controls, mut send: impl FnMut(Event) -> bool) {
    let pty = match start {
        Start::Shell {
            cwd,
            or,
            cols,
            rows,
            ..
        } => match or.filter(|_| !cwd.is_dir()) {
            Some(or) => Pty::shell(&or, cols, rows),
            None => Pty::shell(&cwd, cols, rows),
        },
        Start::Adopt { fd, pid } => Pty::adopt(fd, pid),
        // The hub's to do (see `crate::term_hub`).
        Start::Attach(_) => Err(io::Error::other("there's no terminal to attach to")),
    };
    let mut pty = match pty {
        Ok(pty) => pty,
        Err(err) => {
            send(Event::Failed(err.to_string()));
            return;
        }
    };
    let mut status = Status::default();
    let mut exited = false;
    let mut closed = false;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: if closed { -1 } else { pty.fd() },
                events: libc::POLLIN | if pty.has_unsent() { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: controls.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = STATUS_INTERVAL.as_millis() as libc::c_int;
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        let mut drain = [0u8; 64];
        while matches!((&controls.wake).read(&mut drain), Ok(n) if n > 0) {}
        while let Ok(control) = controls.controls.try_recv() {
            match control {
                Control::Input(bytes) => pty.write(&bytes),
                Control::Resize(cols, rows) => pty.resize(cols, rows),
                Control::Redraw => pty.ask_to_redraw(),
                Control::Release => {
                    let (fd, pid) = pty.release();
                    send(Event::Released(fd, pid));
                    return;
                }
                Control::HangUp => return,
            }
        }
        pty.flush();
        while !closed {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !send(Event::Output(buf[..n].to_vec())) {
                        return;
                    }
                }
                Err(_) => closed = true,
            }
        }
        if !exited {
            // A shell can exit leaving jobs that hold the pty open.
            if let Some(exit) = pty.exit_status() {
                exited = true;
                if !send(Event::Exited(exit.into_raw())) {
                    return;
                }
            }
        }
        if exited && closed {
            return;
        }
        let now = Status {
            busy: !exited && pty.is_busy(),
            program: (!exited).then(|| pty.foreground_name()).flatten(),
            folders: if exited {
                Vec::new()
            } else {
                pty.working_folders()
            },
            shell_folder: (!exited).then(|| pty.working_folder()).flatten(),
        };
        if now != status {
            status = now;
            if !send(Event::Status(status.clone())) {
                return;
            }
        }
    }
}
