//! Raw mode for the controlling terminal.
//!
//! The native core writes escape sequences but never touches termios; in the
//! TypeScript host that is Node's `stdin.setRawMode`. Without it the terminal's
//! replies to capability queries are echoed to the screen and left in the
//! input queue for the shell.

#[cfg(unix)]
pub(crate) use unix::RawMode;

#[cfg(not(unix))]
pub(crate) struct RawMode;

#[cfg(not(unix))]
impl RawMode {
    pub(crate) fn enable() -> Option<RawMode> {
        None
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::mem::MaybeUninit;

    /// Restores the saved terminal attributes on drop.
    pub(crate) struct RawMode {
        saved: libc::termios,
    }

    impl RawMode {
        /// Puts stdin in raw mode if it is a terminal. `None` if it is not.
        pub(crate) fn enable() -> Option<RawMode> {
            unsafe {
                if libc::isatty(libc::STDIN_FILENO) != 1 {
                    return None;
                }
                let mut saved = MaybeUninit::<libc::termios>::uninit();
                if libc::tcgetattr(libc::STDIN_FILENO, saved.as_mut_ptr()) != 0 {
                    return None;
                }
                let saved = saved.assume_init();

                // Same flags as libuv's UV_TTY_MODE_RAW, which Node's setRawMode uses.
                let mut raw = saved;
                raw.c_iflag &=
                    !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
                raw.c_oflag |= libc::ONLCR;
                raw.c_cflag |= libc::CS8;
                raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &raw) != 0 {
                    return None;
                }
                Some(RawMode { saved })
            }
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.saved) };
        }
    }

    /// Reads whatever stdin has buffered without blocking.
    pub(crate) fn read_available(buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0;
        loop {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, 0) };
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 || fd.revents & libc::POLLIN == 0 {
                return Ok(total);
            }
            let mut chunk = [0u8; 4096];
            let n =
                unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(total);
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            total += n as usize;
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::read_available;

#[cfg(not(unix))]
pub(crate) fn read_available(_buf: &mut Vec<u8>) -> std::io::Result<usize> {
    Ok(0)
}
