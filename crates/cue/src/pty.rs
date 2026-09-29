//! A pseudoterminal with a program running in it, as a terminal emulator
//! runs a shell.
//!
//! The program gets the pty's terminal end as its controlling terminal,
//! in a session of its own, so the kernel sends it and its jobs their
//! signals: SIGINT for Ctrl+C, SIGWINCH on resize, SIGHUP when the pty
//! closes. cue holds the other end, non-blocking, and reads it when
//! `poll` says it's ready.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

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
    /// Taken when dropped, to be reaped in the background.
    child: Option<Child>,
    /// Input the program hasn't taken yet.
    unsent: Vec<u8>,
}

impl Pty {
    /// Starts the user's shell (`$SHELL`) as a login shell, in `cwd`, as
    /// terminals do.
    pub fn shell(cwd: &Path, cols: u16, rows: u16) -> io::Result<Pty> {
        let shell = std::env::var_os("SHELL")
            .filter(|shell| !shell.is_empty())
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
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
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
        let child = command.spawn()?;
        // Closes cue's copies of the terminal end, so reading the master
        // ends once the program's copies are closed too.
        drop(command);
        set_nonblocking(master.as_raw_fd())?;
        Ok(Pty {
            master,
            child: Some(child),
            unsent: Vec::new(),
        })
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
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    fn pid(&self) -> Option<libc::pid_t> {
        Some(self.child.as_ref()?.id() as libc::pid_t)
    }

    /// Whether a program other than the one started, such as a command
    /// the shell ran, has the terminal.
    pub fn is_busy(&self) -> bool {
        let foreground = unsafe { libc::tcgetpgrp(self.fd()) };
        foreground > 0 && Some(foreground) != self.pid()
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
}

impl Drop for Pty {
    /// Hangs up, as closing a terminal window does: the program and its
    /// jobs get SIGHUP, and the program is reaped in the background.
    fn drop(&mut self) {
        if self.exit_status().is_some() {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        // The program leads its session and process group.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGHUP) };
        std::thread::spawn(move || child.wait());
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
