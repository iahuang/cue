//! How cue goes on without a terminal, and comes back to one.
//!
//! The `cue` a shell runs is a client: it starts the cue that does the
//! work, a server, in a session of its own, handing it the terminal (its
//! stdin, stdout, and stderr), or hands the terminal to a session's server
//! that's running already. Then it waits, while the server shows itself
//! there, until the server says to go. Since the shell waits only on the
//! client, the server can go on in the background once it lets the
//! terminal go: it detaches.
//!
//! They talk over a Unix socket, a line per message. The server says `exit
//! CODE` when it's done; `detached TEXT` when it goes on without the
//! terminal, TEXT saying how to come back; `moved` when another client took
//! it; and `ended` when it was ended from elsewhere. The client says
//! `winch` when the terminal is resized.
//!
//! A server that's a session listens on the session's socket (see
//! [`crate::session::socket_path`]). A client connecting asks to attach,
//! sending the terminal's file descriptors (`SCM_RIGHTS`), its environment
//! (for what the terminal is), and which build of cue it is, or asks the
//! server to end the session. A client of another build has the server
//! start that build in its place, which takes everything over, its
//! terminals' shells still running (see [`exec`]).

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant, UNIX_EPOCH};

use toml::{Table, Value};

use crate::app::Adopted;

/// Names the cue process in its terminals' environment, so that a cue run
/// in one doesn't attach to it: it would show itself in itself.
pub const PID_VARIABLE: &str = "CUE_PID";
/// The control socket a client started a server with, by descriptor.
pub const CONTROL_VARIABLE: &str = "CUE_CONTROL_FD";
/// How long a client gets to say what it wants.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MANIFEST: &str = "handover.toml";
/// What the manifest starts with; a cue reading one of another version
/// can't take over.
const MANIFEST_VERSION: i64 = 1;
const HANDOVER_SCREENS: &str = "handover";

/// This build of cue, told from others of the same version by its binary,
/// as it was when this process started.
pub fn build() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let binary = std::env::current_exe().and_then(fs::metadata);
        match binary {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                    .unwrap_or_default();
                format!("{version}-{}-{}", modified.as_nanos(), meta.len())
            }
            Err(_) => version.to_string(),
        }
    })
}

/// One end of the socket between a client and its server.
pub struct Control {
    stream: UnixStream,
    /// What's been read of a message not yet whole.
    partial: Vec<u8>,
}

impl Control {
    pub fn new(stream: UnixStream) -> Control {
        Control {
            stream,
            partial: Vec::new(),
        }
    }

    /// The control socket the client that started this server gave it, if
    /// one did.
    pub fn from_env() -> Option<Control> {
        let fd: RawFd = std::env::var(CONTROL_VARIABLE).ok()?.parse().ok()?;
        // Not for the shells it starts.
        std::env::remove_var(CONTROL_VARIABLE);
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0
            || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK
        {
            return None;
        }
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        stream.set_nonblocking(true).ok()?;
        Some(Control::new(stream))
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Sends `message`, a line. If the other end is gone, it's no matter.
    pub fn send(&mut self, message: &str) {
        let line = format!("{}\n", message.replace('\n', " "));
        let _ = self.stream.set_nonblocking(false);
        let _ = self.stream.write_all(line.as_bytes());
        let _ = self.stream.set_nonblocking(true);
    }

    /// The messages that came since the last call, without waiting.
    /// `UnexpectedEof` once the other end is gone and they've all been
    /// taken.
    pub fn receive(&mut self) -> io::Result<Vec<String>> {
        let mut buf = [0u8; 1024];
        let mut closed = false;
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => self.partial.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        let mut messages = Vec::new();
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            messages.push(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        if closed && messages.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(messages)
    }

    /// Waits for the next message: `None` once the other end is gone.
    #[cfg(test)]
    pub fn wait(&mut self) -> Option<String> {
        let _ = self.stream.set_nonblocking(false);
        let mut byte = [0u8; 1];
        let message = loop {
            if let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=end).collect();
                break Some(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            match self.stream.read(&mut byte) {
                Ok(0) => break None,
                Ok(_) => self.partial.push(byte[0]),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break None,
            }
        };
        let _ = self.stream.set_nonblocking(true);
        message
    }
}

/// What a client connecting to a session's socket asks for.
pub enum Request {
    /// To show the session in its terminal.
    Attach(Attach),
    /// To end the session.
    End,
}

pub struct Attach {
    /// The terminal: stdin, stdout, and stderr.
    pub stdio: [OwnedFd; 3],
    /// The client's build (see [`build`]), and its binary.
    pub build: String,
    pub exe: PathBuf,
    /// The client's environment, which says what the terminal is.
    pub environment: Vec<(String, String)>,
}

impl Attach {
    /// Whether the terminal is one of the process `pid`'s own: whether the
    /// client runs in it.
    pub fn is_inside(&self, pid: u32) -> bool {
        self.environment
            .iter()
            .any(|(key, value)| key == PID_VARIABLE && *value == pid.to_string())
    }
}

/// Listens on `path` for clients. Whatever is there is a socket left by
/// a server that's gone, since this one has the session's lock.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// A client that connected to `listener`, if one did, and what it asks.
/// Clients that don't make sense are hung up on.
pub fn accept(listener: &UnixListener) -> Option<(Control, Request)> {
    loop {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if let Some(request) = read_request(&stream) {
            let _ = stream.set_nonblocking(true);
            return Some((Control::new(stream), request));
        }
    }
}

/// Reads a client's request, up to the empty line that ends it, with the
/// descriptors that came with it.
fn read_request(stream: &UnixStream) -> Option<Request> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).ok()?;
    let started = Instant::now();
    let mut text = Vec::new();
    let mut fds = Vec::new();
    while !text.ends_with(b"\n\n") {
        if started.elapsed() > REQUEST_TIMEOUT || text.len() > 1 << 20 {
            return None;
        }
        let mut buf = [0u8; 4096];
        let (n, received) = receive_fds(stream.as_raw_fd(), &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        text.extend_from_slice(&buf[..n]);
        fds.extend(received);
    }
    let _ = stream.set_read_timeout(None);
    let text = String::from_utf8_lossy(&text).into_owned();
    let mut lines = text.lines();
    match lines.next()? {
        "end" => Some(Request::End),
        "attach" => {
            let mut build = String::new();
            let mut exe = PathBuf::new();
            let mut environment = Vec::new();
            for line in lines {
                match line.split_once(' ') {
                    Some(("build", value)) => build = value.to_string(),
                    Some(("exe", value)) => exe = PathBuf::from(value),
                    Some(("env", value)) => {
                        if let Some((key, value)) = value.split_once('=') {
                            environment.push((key.to_string(), value.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            let stdio: [OwnedFd; 3] = fds.try_into().ok()?;
            Some(Request::Attach(Attach {
                stdio,
                build,
                exe,
                environment,
            }))
        }
        _ => None,
    }
}

/// Connects to the server listening on `path`, asking it to show itself
/// in this process's terminal.
pub fn request_attach(path: &Path) -> io::Result<Control> {
    let stream = UnixStream::connect(path)?;
    let exe = std::env::current_exe()?;
    let mut text = format!("attach\nbuild {}\nexe {}\n", build(), exe.display());
    for (key, value) in std::env::vars_os() {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            continue;
        };
        if !key.contains(['=', '\n']) && !value.contains('\n') {
            text += &format!("env {key}={value}\n");
        }
    }
    text.push('\n');
    let bytes = text.as_bytes();
    let sent = send_fds(stream.as_raw_fd(), bytes, &[0, 1, 2])?;
    (&stream).write_all(&bytes[sent..])?;
    stream.set_nonblocking(true)?;
    Ok(Control::new(stream))
}

/// Connects to the server listening on `path`, asking it to end its
/// session, and waits for it to.
pub fn request_end(path: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(b"end\n\n")?;
    // It hangs up once it's done.
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    Ok(())
}

/// Puts `stdio` in place as this process's stdin, stdout, and stderr.
pub fn take_stdio(stdio: [OwnedFd; 3]) {
    for (target, fd) in stdio.iter().enumerate() {
        unsafe { libc::dup2(fd.as_raw_fd(), target as RawFd) };
    }
}

/// Lets go of the terminal: stdin and stdout become `/dev/null`, and
/// stderr `stderr`, or `/dev/null` without one.
pub fn let_go_of_stdio(stderr: Option<fs::File>) {
    let Ok(null) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    else {
        return;
    };
    for target in 0..2 {
        unsafe { libc::dup2(null.as_raw_fd(), target) };
    }
    let stderr = stderr.as_ref().unwrap_or(&null);
    unsafe { libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) };
}

// --- taking over ----------------------------------------------------------------

/// Writes what a process taking over from this one, from the session in
/// `dir`, needs besides the session: whether the session is only for the
/// while (`ephemeral`), and the terminals it's to adopt, their pty
/// masters kept open across `exec`.
pub fn write_manifest(dir: &Path, ephemeral: bool, terminals: &[Adopted]) -> io::Result<()> {
    let screens = dir.join(HANDOVER_SCREENS);
    fs::create_dir_all(&screens)?;
    let mut root = Table::new();
    root.insert("version".into(), Value::Integer(MANIFEST_VERSION));
    root.insert("ephemeral".into(), Value::Boolean(ephemeral));
    let mut list = Vec::new();
    for terminal in terminals {
        let name = format!("{}.vt", terminal.id);
        fs::write(screens.join(&name), &terminal.screen)?;
        let mut table = Table::new();
        let int = |n: i64| Value::Integer(n);
        table.insert("id".into(), int(terminal.id as i64));
        table.insert("fd".into(), int(terminal.fd as i64));
        table.insert("pid".into(), int(terminal.pid as i64));
        table.insert("cols".into(), int(terminal.size.0 as i64));
        table.insert("rows".into(), int(terminal.size.1 as i64));
        table.insert("screen".into(), Value::String(name));
        list.push(Value::Table(table));
    }
    root.insert("terminals".into(), Value::Array(list));
    fs::write(dir.join(MANIFEST), root.to_string())
}

/// Reads what [`write_manifest`] wrote in `dir`, removing it: whether the
/// session is only for the while, and the terminals to adopt.
pub fn read_manifest(dir: &Path) -> Option<(bool, Vec<Adopted>)> {
    let text = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let _ = fs::remove_file(dir.join(MANIFEST));
    let root: Table = text.parse().ok()?;
    if root.get("version")?.as_integer()? != MANIFEST_VERSION {
        return None;
    }
    let ephemeral = root.get("ephemeral")?.as_bool()?;
    let screens = dir.join(HANDOVER_SCREENS);
    let get = |table: &Table, key: &str| table.get(key).and_then(Value::as_integer);
    let terminals = root
        .get("terminals")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_table)
        .filter_map(|table| {
            let name = table.get("screen")?.as_str()?;
            Some(Adopted {
                id: get(table, "id")? as u32,
                fd: get(table, "fd")? as RawFd,
                pid: get(table, "pid")? as libc::pid_t,
                screen: fs::read(screens.join(name)).unwrap_or_default(),
                size: (get(table, "cols")? as u16, get(table, "rows")? as u16),
            })
        })
        .collect();
    let _ = fs::remove_dir_all(&screens);
    Some((ephemeral, terminals))
}

/// Replaces this process with the cue at `exe`, to take over from the
/// session in `dir`, with `control` to talk to its client over, if it has
/// one. Returns only if it can't.
pub fn exec(exe: &Path, dir: &Path, control: Option<&Control>) -> io::Error {
    let mut command = Command::new(exe);
    command.arg("--serve").arg("--adopt").arg(dir);
    match control {
        Some(control) => {
            let fd = control.fd();
            unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
            command.env(CONTROL_VARIABLE, fd.to_string());
        }
        None => {
            command.env_remove(CONTROL_VARIABLE);
        }
    }
    command.exec()
}

// --- descriptors ----------------------------------------------------------------

/// Sends `bytes`, or as many as go, with the descriptors `fds`. Returns
/// how many bytes went.
fn send_fds(socket: RawFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let data_len = std::mem::size_of_val(fds) as u32;
    let space = unsafe { libc::CMSG_SPACE(data_len) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(data_len) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
    }
    loop {
        let sent = unsafe { libc::sendmsg(socket, &msg, 0) };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Reads into `buf`, with any descriptors sent along, which are closed on
/// exec.
fn receive_fds(socket: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let space = unsafe { libc::CMSG_SPACE(8 * std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(socket, &mut msg, 0) };
        if n >= 0 {
            break n as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..len / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n, fds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_carry_descriptors_and_the_environment() {
        let dir = std::env::temp_dir().join(format!("cue-attach-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let listener = listen(&path).unwrap();
        assert!(accept(&listener).is_none(), "nobody yet");

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut control = request_attach(&path).unwrap();
                control.wait()
            }
        });
        let (mut control, request) = loop {
            if let Some(accepted) = accept(&listener) {
                break accepted;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let Request::Attach(attach) = request else {
            panic!("asked to attach");
        };
        assert_eq!(attach.build, build());
        assert_eq!(attach.exe, std::env::current_exe().unwrap());
        assert!(attach.environment.iter().any(|(key, _)| key == "PATH"));
        assert!(!attach.is_inside(std::process::id() + 1));
        // The descriptors are this process's own stdio, received anew.
        let mut a: libc::stat = unsafe { std::mem::zeroed() };
        let mut b: libc::stat = unsafe { std::mem::zeroed() };
        unsafe {
            libc::fstat(attach.stdio[1].as_raw_fd(), &mut a);
            libc::fstat(1, &mut b);
        }
        assert_eq!((a.st_dev, a.st_ino), (b.st_dev, b.st_ino));
        control.send("moved");
        assert_eq!(client.join().unwrap().as_deref(), Some("moved"));

        let ender = std::thread::spawn({
            let path = path.clone();
            move || request_end(&path).unwrap()
        });
        let request = loop {
            if let Some((_, request)) = accept(&listener) {
                break request;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(request, Request::End));
        ender.join().unwrap();
    }

    #[test]
    fn messages_sent_just_before_hanging_up_arrive() {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        let (mut a, mut b) = (Control::new(a), Control::new(b));
        assert_eq!(b.receive().unwrap(), Vec::<String>::new());
        a.send("detached later");
        a.send("exit 0");
        drop(a);
        assert_eq!(b.receive().unwrap(), ["detached later", "exit 0"]);
        assert!(b.receive().is_err());
    }

    #[test]
    fn the_manifest_reads_back_once() {
        let dir = std::env::temp_dir().join(format!("cue-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let terminals = [Adopted {
            id: 3,
            fd: 9,
            pid: 1234,
            screen: b"\x1b[1mhi".to_vec(),
            size: (80, 24),
        }];
        write_manifest(&dir, true, &terminals).unwrap();
        let (ephemeral, read) = read_manifest(&dir).unwrap();
        assert!(ephemeral);
        assert_eq!(read.len(), 1);
        let t = &read[0];
        assert_eq!((t.id, t.fd, t.pid, t.size), (3, 9, 1234, (80, 24)));
        assert_eq!(t.screen, b"\x1b[1mhi");
        assert!(read_manifest(&dir).is_none(), "taken");
    }
}
