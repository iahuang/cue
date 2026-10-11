//! Waking the main loop from another thread, as soon as that thread has
//! something to show, rather than when the loop next looks: a thread
//! calls [`wake`], and the loop, which polls [`fd`] along with its input,
//! redraws. The loop then takes the wake with [`take`].

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;

/// What a wake writes to, and what the loop polls. `None` if it couldn't
/// be made, in which case the loop sees a wake when it next looks anyway.
static PAIR: OnceLock<Option<(UnixStream, UnixStream)>> = OnceLock::new();

fn pair() -> Option<&'static (UnixStream, UnixStream)> {
    PAIR.get_or_init(|| {
        let (sender, receiver) = UnixStream::pair().ok()?;
        sender.set_nonblocking(true).ok()?;
        receiver.set_nonblocking(true).ok()?;
        Some((sender, receiver))
    })
    .as_ref()
}

/// Wakes the main loop. A wake that's waiting already will do.
pub fn wake() {
    if let Some((sender, _)) = pair() {
        let mut sender: &UnixStream = sender;
        // Full means a wake is waiting.
        let _ = sender.write(&[0]);
    }
}

/// What the main loop polls to see a wake.
pub fn fd() -> Option<RawFd> {
    pair().map(|(_, receiver)| receiver.as_raw_fd())
}

/// Whether anything woke the loop since the last call.
pub fn take() -> bool {
    let Some((_, receiver)) = pair() else {
        return false;
    };
    let mut receiver: &UnixStream = receiver;
    let mut woken = false;
    let mut buf = [0u8; 256];
    loop {
        match receiver.read(&mut buf) {
            Ok(0) => return woken,
            Ok(_) => woken = true,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => return woken,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakes_are_taken_once() {
        let _serial = crate::test_serial();
        take();
        assert!(!take());
        std::thread::spawn(wake).join().unwrap();
        wake();
        assert!(take());
        assert!(!take());
    }
}
