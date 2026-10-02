//! Terminal size and stdin reads. Raw mode itself is handled by
//! `opentui::Renderer::setup_terminal`.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

/// Columns and rows of the terminal on stdout, or 80x24 if it isn't one.
pub fn size() -> (u32, u32) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col as u32, ws.ws_row as u32)
    } else {
        (80, 24)
    }
}

/// The width and height of a cell in pixels, if the terminal on stdout
/// says (Ghostty, kitty, and iTerm2 do).
pub fn cell_pixels() -> Option<(u32, u32)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    let known = ok && ws.ws_col > 0 && ws.ws_row > 0 && ws.ws_xpixel > 0 && ws.ws_ypixel > 0;
    known.then(|| {
        (
            (ws.ws_xpixel / ws.ws_col) as u32,
            (ws.ws_ypixel / ws.ws_row) as u32,
        )
    })
}

/// Makes writing to stdout and stderr fail rather than wait, or wait
/// again: for a terminal that may have stopped reading.
pub fn set_nonblocking(nonblocking: bool) {
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            continue;
        }
        let flags = match nonblocking {
            true => flags | libc::O_NONBLOCK,
            false => flags & !libc::O_NONBLOCK,
        };
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    }
}

/// Waits up to `timeout` for one of the `watched` file descriptors (fd,
/// and whether to wait for it to take writes too) to be ready.
pub fn wait(watched: &[(RawFd, bool)], timeout: Duration) {
    let mut fds: Vec<libc::pollfd> = watched
        .iter()
        .map(|&(fd, write)| libc::pollfd {
            fd,
            events: if write {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        })
        .collect();
    // A signal interrupting it is as good as a timeout.
    unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            timeout.as_millis() as libc::c_int,
        )
    };
}

/// Waits up to `timeout` for input, or for one of the `watched` file
/// descriptors (fd, and whether to wait for it to take writes too) to be
/// ready, and returns the input that's available (empty if none).
pub fn read_input(timeout: Duration, watched: &[(RawFd, bool)]) -> io::Result<Vec<u8>> {
    let mut fds = vec![libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    }];
    fds.extend(watched.iter().map(|&(fd, write)| libc::pollfd {
        fd,
        events: if write {
            libc::POLLIN | libc::POLLOUT
        } else {
            libc::POLLIN
        },
        revents: 0,
    }));
    let ready = unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            timeout.as_millis() as libc::c_int,
        )
    };
    if ready < 0 {
        let err = io::Error::last_os_error();
        // A signal (e.g. SIGWINCH on resize) interrupted the wait.
        return if err.kind() == io::ErrorKind::Interrupted {
            Ok(Vec::new())
        } else {
            Err(err)
        };
    }
    if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed"));
    }
    buf.truncate(n as usize);
    Ok(buf)
}
