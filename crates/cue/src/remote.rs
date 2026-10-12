//! cue over ssh: `cue [user@]host:path` shows, here, the files, terminals,
//! and sessions of `host` (see `docs/client-server.md`, step 3).
//!
//! What's remote goes by scp's rule: a `:` before any `/`, as in
//! `host:src` or `me@host:` ([`destination`]), unless it names a file here,
//! as `main.rs:12` does; or a `user@host` alone. The path is from the home
//! folder there, unless it's absolute. With none, cue lists the host's
//! sessions and its folders to choose from (see [`crate::resume`]).
//!
//! It runs the system's `ssh` (or what `CUE_SSH` says), so that
//! `~/.ssh/config`, jump hosts, shared connections, and agents work as
//! they do for ssh. Through it, a script finds a cue of this very build on
//! the host, in `~/.cache/cue/`, or if there's none, has this one send it,
//! and runs it as `cue --bridge` (see [`crate::bridge`]): this cue's own
//! binary, for a host like this machine, and otherwise the build for the
//! host's platform kept beside it (see [`binary`]). Over that one
//! connection, the bridge answers what's asked before there's an app,
//! such as the host's sessions, then connects to a host process there,
//! started or running a session, and passes the host's messages along:
//! from then on, it's as on one machine.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bridge::{self, Answer, Ask, Greeting, Target};
use crate::client::Args;
use crate::file_dialog;
use crate::host::{Link, Look};
use crate::session::Listing;
use crate::wire::{read_frame, write_frame};

/// The ssh to run, with any arguments to give it first, split at spaces;
/// `ssh` without it.
const SSH_VARIABLE: &str = "CUE_SSH";
/// A folder of builds of cue to send hosts, named `cue-PLATFORM`, ahead of
/// those found elsewhere (see [`binary`]).
const BINARIES_VARIABLE: &str = "CUE_REMOTE_BINARIES";

/// A machine cue runs on, as `uname -sm` says, and the Rust target cue's
/// built for it with.
struct Platform {
    name: &'static str,
    uname: &'static [&'static str],
    target: &'static str,
}

const PLATFORMS: &[Platform] = &[
    Platform {
        name: "linux-x64",
        uname: &["Linux x86_64", "Linux amd64"],
        target: "x86_64-unknown-linux-musl",
    },
    Platform {
        name: "linux-arm64",
        uname: &["Linux aarch64", "Linux arm64"],
        target: "aarch64-unknown-linux-musl",
    },
    Platform {
        name: "darwin-arm64",
        uname: &["Darwin arm64"],
        target: "aarch64-apple-darwin",
    },
];

/// Where `cue` was asked to show files from another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The machine, as ssh is told: `host`, or `user@host`.
    pub host: String,
    /// The folder or file there, if one was named.
    pub path: Option<String>,
}

/// What `arg` names on another machine, if it does (see the module's
/// docs).
pub fn destination(arg: &OsStr) -> Option<Destination> {
    let text = arg.to_str()?;
    // A file here, though it looks like one there.
    if text.is_empty() || text.starts_with('-') || Path::new(text).exists() {
        return None;
    }
    // An IPv6 address, in brackets as scp has it.
    let (host, rest) = match text.strip_prefix('[') {
        Some(bracketed) => {
            let (address, rest) = bracketed.split_once(']')?;
            (address, rest)
        }
        None => match text.find(':') {
            Some(colon) if !text[..colon].contains('/') => text.split_at(colon),
            None if text.contains('@') && !text.contains('/') => (text, ""),
            _ => return None,
        },
    };
    let path = match rest.strip_prefix(':') {
        Some(path) => path,
        None if rest.is_empty() && !text.starts_with('[') => "",
        None => return None,
    };
    // A file here with a position after it, as `main.rs:12`.
    if host.is_empty() || Path::new(host).exists() {
        return None;
    }
    Some(Destination {
        host: host.to_string(),
        path: (!path.is_empty()).then(|| path.to_string()),
    })
}

/// The name of the platform this build is for, if it's one cue's built for.
fn own_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("macos", "aarch64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// This build of cue for `platform`, to send a host, if there's one here:
/// in [`BINARIES_VARIABLE`]'s folder; built beside this one, as cargo puts
/// a build for another target (`target/x86_64-unknown-linux-musl/debug/cue`
/// beside `target/debug/cue`); in `lib/cue/` beside the `bin/` this is in,
/// as installed; or this one, for its own platform.
fn binary(platform: &Platform) -> Option<PathBuf> {
    let file = format!("cue-{}", platform.name);
    if let Some(dir) = std::env::var_os(BINARIES_VARIABLE) {
        let path = PathBuf::from(dir).join(&file);
        if path.is_file() {
            return Some(path);
        }
    }
    let own = crate::relocate::origin();
    let dir = own.parent()?;
    let mut found = Vec::new();
    if let (Some(profile), Some(target)) = (dir.file_name(), dir.parent()) {
        found.push(target.join(platform.target).join(profile).join("cue"));
    }
    if let Some(prefix) = dir.parent() {
        found.push(prefix.join("lib/cue").join(&file));
    }
    if own_platform() == Some(platform.name) {
        found.push(own.to_path_buf());
    }
    found.into_iter().find(|path| path.is_file())
}

/// What a build of cue for another machine is kept as there: its folder
/// in `~/.cache/cue/`, named for this version and the binary, so that one
/// that changed is sent again.
fn id(binary: &Path) -> Option<String> {
    let meta = std::fs::metadata(binary).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let modified = meta.mtime() as u64 ^ (meta.mtime_nsec() as u64) << 32;
    for byte in modified
        .to_le_bytes()
        .into_iter()
        .chain(meta.len().to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    Some(format!("{}-{hash:016x}", env!("CARGO_PKG_VERSION")))
}

/// What's said first on the connection, by the script and then the
/// bridge, after what the host's shell may print: lines starting with
/// this.
const MARKER: &str = bridge::MARKER;

/// The script ssh runs on the host: it finds the cue to run there, by
/// platform, or asks for it, and runs it as a bridge. One line of plain
/// `sh`, in single quotes, with none inside, which the user's shell there,
/// whatever it is, passes to `sh` as it is.
fn script(builds: &[(&Platform, Option<String>)]) -> String {
    let mut cases = String::new();
    for (platform, id) in builds {
        let patterns: Vec<String> = platform.uname.iter().map(|u| format!("\"{u}\"")).collect();
        let id = id.as_deref().unwrap_or("");
        cases += &format!("{}) p={} b={id};; ", patterns.join("|"), platform.name);
    }
    let script = [
        "d=\"${XDG_CACHE_HOME:-$HOME/.cache}/cue\"".to_string(),
        format!("case \"$(uname -sm)\" in {cases}*) echo \"{MARKER} unsupported $(uname -sm)\"; exit 1;; esac"),
        format!("if [ -z \"$b\" ]; then echo \"{MARKER} missing $p\"; exit 1; fi"),
        "f=\"$d/$b/cue\"".to_string(),
        "if [ ! -x \"$f\" ]; then z=; if command -v gzip >/dev/null 2>&1; then z=gzip; fi".to_string(),
        format!("echo \"{MARKER} send $p $z\""),
        "read n || exit 1".to_string(),
        format!("mkdir -p \"$d/$b\" || {{ echo \"{MARKER} failed cannot make $d/$b\"; exit 1; }}"),
        "if [ -n \"$z\" ]; then head -c \"$n\" | gzip -dc > \"$f.part\"; else head -c \"$n\" > \"$f.part\"; fi".to_string(),
        format!("[ -s \"$f.part\" ] && chmod +x \"$f.part\" && mv -f \"$f.part\" \"$f\" || {{ echo \"{MARKER} failed cannot write $f\"; exit 1; }}"),
        // Builds sent before go: one that's running keeps its file open.
        "for o in \"$d\"/[0-9]*-*; do [ \"$o\" = \"$d/$b\" ] || rm -rf \"$o\"; done".to_string(),
        "fi".to_string(),
        "exec \"$f\" --bridge".to_string(),
    ];
    format!("sh -c '{}'", script.join("; "))
}

/// Set once the files are on another machine: what looks at this one's
/// for them, such as finding a path a terminal printed, finds nothing.
static ELSEWHERE: AtomicBool = AtomicBool::new(false);

/// Whether the files are on another machine.
pub fn files_elsewhere() -> bool {
    ELSEWHERE.load(Ordering::Relaxed)
}

/// The ssh that reaches a host, and what it says on stderr.
pub struct Remote {
    /// The host, as ssh is told.
    name: String,
    ssh: Child,
    /// ssh's stderr, as it comes: shown until the app is, and kept.
    stderr: Arc<Stderr>,
}

#[derive(Default)]
struct Stderr {
    /// What it said last, a line or so.
    last: Mutex<String>,
    /// Kept, not shown, once the app's on screen.
    quiet: AtomicBool,
}

impl Remote {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Remote {
    /// Gives ssh a moment to pass on what was last sent, which it's done
    /// with once it hears the app's end of the connection close.
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !matches!(self.ssh.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.ssh.kill();
        let _ = self.ssh.wait();
    }
}

/// A connection to a bridge on another machine, before there's an app:
/// what it says of the machine, and what it answers (see [`Ask`]).
pub struct Bridge {
    stream: UnixStream,
    remote: Remote,
    /// The home folder there, which paths are from.
    pub home: PathBuf,
}

impl Bridge {
    /// Reaches `host` with ssh, sending it this build of cue if it hasn't
    /// one, and has it run it as a bridge. Says on stderr what's sent.
    pub fn open(host: &str) -> io::Result<Bridge> {
        let builds: Vec<(&Platform, Option<String>)> = PLATFORMS
            .iter()
            .map(|platform| (platform, binary(platform).and_then(|path| id(&path))))
            .collect();
        let (ours, theirs) = UnixStream::pair()?;
        let mut ssh = ssh_command();
        ssh.arg(host)
            .arg(script(&builds))
            .stdin(OwnedFd::from(theirs.try_clone()?))
            .stdout(OwnedFd::from(theirs))
            .stderr(Stdio::piped());
        let spawned = ssh.spawn();
        // It has ssh's end of the connection, which ssh's going must close.
        drop(ssh);
        let mut child = spawned.map_err(|err| io::Error::other(format!("can't run ssh: {err}")))?;
        let stderr = Arc::new(Stderr::default());
        if let Some(pipe) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            std::thread::spawn(move || pass_on(pipe, &stderr));
        }
        let remote = Remote {
            name: host.to_string(),
            ssh: child,
            stderr,
        };
        let mut stream = ours;
        let greeting = match bootstrap(&mut stream, host) {
            Ok(greeting) => greeting,
            Err(err) => return Err(remote.failed(err)),
        };
        if greeting.source != env!("CUE_SOURCE") {
            let err = io::Error::other(format!(
                "the cue sent to {host} is another build ({} {}, not {} {}): build cue for it again from the same source",
                greeting.version,
                greeting.source,
                env!("CARGO_PKG_VERSION"),
                env!("CUE_SOURCE"),
            ));
            return Err(err);
        }
        Ok(Bridge {
            stream,
            remote,
            home: greeting.home.unwrap_or_else(|| PathBuf::from("/")),
        })
    }

    /// The host, as ssh is told.
    pub fn name(&self) -> &str {
        &self.remote.name
    }

    fn ask(&mut self, ask: Ask) -> io::Result<Answer> {
        write_frame(&mut self.stream, &ask)?;
        read_frame(&mut self.stream).map_err(|err| self.remote.failed(err))
    }

    /// The sessions kept on the host, most recent first.
    pub fn sessions(&mut self) -> io::Result<Vec<Listing>> {
        match self.ask(Ask::Sessions)? {
            Answer::Sessions(sessions) => Ok(sessions),
            _ => Err(other_answer()),
        }
    }

    /// What's at each of `paths` on the host.
    pub fn look(&mut self, paths: Vec<PathBuf>) -> io::Result<Vec<Option<Look>>> {
        match self.ask(Ask::Look(paths))? {
            Answer::Looked(looked) => Ok(looked),
            _ => Err(other_answer()),
        }
    }

    /// What's in the folder `dir` on the host.
    pub fn read_dir(&mut self, dir: &Path) -> io::Result<Vec<file_dialog::Entry>> {
        match self.ask(Ask::ReadDir(dir.to_path_buf()))? {
            Answer::Dir(entries) => entries.map_err(io::Error::other),
            _ => Err(other_answer()),
        }
    }

    /// Has the bridge connect to a host process there, as `target` says,
    /// and the app's link to it, through the bridge.
    pub fn connect(mut self, target: Target) -> io::Result<Link> {
        match self.ask(Ask::Connect(target))? {
            Answer::Connected(Ok(())) => {}
            Answer::Connected(Err(err)) => return Err(io::Error::other(err)),
            _ => return Err(other_answer()),
        }
        // What ssh says from now on would garble the screen.
        self.remote.stderr.quiet.store(true, Ordering::Relaxed);
        ELSEWHERE.store(true, Ordering::Relaxed);
        Ok(Link::over_ssh(self.stream, self.remote))
    }
}

impl Remote {
    /// `err`, or that ssh stopped, if it did, and why, unless it's said so
    /// on stderr already.
    fn failed(&self, err: io::Error) -> io::Error {
        if err.kind() != io::ErrorKind::UnexpectedEof {
            return err;
        }
        if !self.stderr.quiet.load(Ordering::Relaxed) {
            return io::Error::other("ssh stopped");
        }
        let last = self.stderr.last.lock().unwrap_or_else(|e| e.into_inner());
        match last.trim().lines().last() {
            Some(said) => io::Error::other(format!("ssh stopped: {said}")),
            None => io::Error::other("ssh stopped"),
        }
    }
}

/// `ssh`, or what [`SSH_VARIABLE`] says, with a connection that isn't a
/// terminal's. It ignores the terminal hanging up, as ssh lets it, so that
/// cue can still save the session through it then; it ends once cue's end
/// of the connection closes.
fn ssh_command() -> Command {
    use std::os::unix::process::CommandExt;
    let given = std::env::var(SSH_VARIABLE).unwrap_or_default();
    let mut words = given.split_whitespace();
    let mut command = Command::new(words.next().unwrap_or("ssh"));
    command.args(words).arg("-T");
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    command
}

/// Passes on what ssh says on stderr, until the app's on screen, keeping
/// the last of it.
fn pass_on(mut pipe: impl Read, stderr: &Stderr) {
    let mut buffer = [0; 4096];
    loop {
        let n = match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let text = String::from_utf8_lossy(&buffer[..n]);
        if !stderr.quiet.load(Ordering::Relaxed) {
            let _ = io::stderr().write_all(text.as_bytes());
        }
        let mut last = stderr.last.lock().unwrap_or_else(|e| e.into_inner());
        last.push_str(&text);
        // A few lines' worth is enough to say why it stopped.
        if last.len() > 1024 {
            let cut = last.len() - 512;
            let cut = (cut..last.len())
                .find(|&i| last.is_char_boundary(i))
                .unwrap_or(0);
            last.drain(..cut);
        }
    }
}

/// Follows the script on `stream` until the bridge greets, sending this
/// build for the host's platform if it's asked for.
fn bootstrap(stream: &mut UnixStream, host: &str) -> io::Result<Greeting> {
    loop {
        let line = read_line(stream)?;
        let Some(said) = line.strip_prefix(MARKER) else {
            // What the host's shell printed, as its rc files may.
            continue;
        };
        let mut words = said.split_whitespace();
        match words.next() {
            Some("bridge") => return read_frame(stream),
            Some("send") => {
                let platform = words.next().unwrap_or_default();
                let gzip = words.next() == Some("gzip");
                send(stream, host, platform, gzip)?;
            }
            Some("missing") => {
                let platform = words.next().unwrap_or_default();
                return Err(io::Error::other(missing(host, platform)));
            }
            Some("unsupported") => {
                let rest: Vec<&str> = words.collect();
                return Err(io::Error::other(format!(
                    "{host} is a {}, which cue isn't built for",
                    rest.join(" ")
                )));
            }
            Some("failed") => {
                let rest: Vec<&str> = words.collect();
                return Err(io::Error::other(format!(
                    "can't install cue on {host}: {}",
                    rest.join(" ")
                )));
            }
            _ => {}
        }
    }
}

/// Why there's no build of cue for `platform` to send `host`.
fn missing(host: &str, platform: &str) -> String {
    let target = PLATFORMS
        .iter()
        .find(|known| known.name == platform)
        .map_or("", |known| known.target);
    format!(
        "{host} needs cue for {platform}, and there's none of this build here: build it (`cargo zigbuild --target {target}`), or put it at $CUE_REMOTE_BINARIES/cue-{platform}"
    )
}

/// Sends the build of cue for `platform`, gzipped if `gzip` and this
/// machine has gzip, after its length, saying so on stderr.
fn send(stream: &mut UnixStream, host: &str, platform: &str, gzip: bool) -> io::Result<()> {
    let found = PLATFORMS
        .iter()
        .find(|known| known.name == platform)
        .and_then(binary);
    let Some(path) = found else {
        return Err(io::Error::other(missing(host, platform)));
    };
    let bytes = match gzip {
        true => gzipped(&path)?,
        false => std::fs::read(&path)?,
    };
    eprintln!(
        "cue: sending cue {} ({platform}, {:.1} MB) to {host}…",
        env!("CARGO_PKG_VERSION"),
        bytes.len() as f64 / 1e6
    );
    stream.write_all(format!("{}\n", bytes.len()).as_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

/// The file at `path`, gzipped.
fn gzipped(path: &Path) -> io::Result<Vec<u8>> {
    let output = Command::new("gzip")
        .arg("-c")
        .stdin(std::fs::File::open(path)?)
        .stderr(Stdio::null())
        .output()
        .map_err(|err| io::Error::other(format!("can't run gzip: {err}")))?;
    if !output.status.success() {
        return Err(io::Error::other("gzip failed"));
    }
    Ok(output.stdout)
}

/// A line from `stream`, read a byte at a time, so that what comes after
/// it is left to read.
fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        stream.read_exact(&mut byte)?;
        match byte[0] {
            b'\n' => return Ok(String::from_utf8_lossy(&line).trim_end().to_string()),
            byte => line.push(byte),
        }
        // Not a line of the script's: it'd have said so by now.
        if line.len() > 64 * 1024 {
            line.clear();
        }
    }
}

fn other_answer() -> io::Error {
    io::Error::other("The bridge answered something else.")
}

/// What `cue host:path` opens on the host.
pub enum Opening {
    Session(Box<Listing>),
    /// The folders and file named, from the home folder there.
    Paths(Vec<PathBuf>),
}

/// What `args` say to open on the host `bridge` reached, as for a folder
/// here (see [`crate::client::target`]): the session of the folder named,
/// if it has just one, or one chosen with `--resume`, or a new cue. With
/// no folder named, it's chosen from the host's sessions and folders, full
/// screen. Otherwise, the code to exit with: there's nothing to show.
pub fn target(
    bridge: &mut Bridge,
    destination: &Destination,
    args: &Args,
) -> Result<Opening, std::process::ExitCode> {
    let host = bridge.name().to_string();
    let fail = |message: String| {
        eprintln!("cue: {message}");
        std::process::ExitCode::FAILURE
    };
    let sessions = bridge
        .sessions()
        .map_err(|err| fail(format!("can't list the sessions on {host}: {err}")))?;
    let interactive = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    let Some(path) = &destination.path else {
        if !interactive {
            crate::client::print_sessions(&sessions, &bridge.home, true);
            return Err(std::process::ExitCode::SUCCESS);
        }
        let home = bridge.home.clone();
        let chosen =
            crate::resume::choose_or_browse(sessions, &home, &host, |dir| bridge.read_dir(dir));
        return match chosen {
            Ok(Some(crate::resume::Chosen::Session(listing))) => Ok(Opening::Session(listing)),
            Ok(Some(crate::resume::Chosen::Path(path))) => Ok(Opening::Paths(vec![path])),
            Ok(None) => Err(std::process::ExitCode::SUCCESS),
            Err(err) => Err(fail(format!("can't list the sessions on {host}: {err}"))),
        };
    };
    // The folder named, or the file's: there's no folder the user's in
    // there, as there is here, but the home folder, which is too much.
    let parent = Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let mut asked = vec![bridge.home.join(path)];
    asked.extend(parent.map(|parent| bridge.home.join(parent)));
    let looked = bridge
        .look(asked)
        .map_err(|err| fail(format!("can't reach {host}: {err}")))?;
    let mut looked = looked
        .into_iter()
        .map(|look| look.filter(|look| look.is_dir));
    let (folder, names_file) = match (looked.next().flatten(), looked.next().flatten()) {
        (Some(folder), _) => (folder.real, false),
        (None, Some(parent)) => (parent.real, true),
        (None, None) => (bridge.home.clone(), true),
    };
    let named = match (names_file, parent) {
        (true, Some(_)) => vec![folder.clone(), PathBuf::from(path)],
        _ => vec![PathBuf::from(path)],
    };
    if args.resume {
        if sessions.is_empty() {
            return Err(fail(format!("there are no sessions on {host}")));
        }
        // With none here, the rest are worth seeing.
        let everywhere = args.all || !sessions.iter().any(|listing| listing.has(&folder));
        if !interactive {
            crate::client::print_sessions(&sessions, &folder, everywhere);
            return Err(std::process::ExitCode::SUCCESS);
        }
        return match crate::resume::choose(sessions, &folder, everywhere) {
            Ok(Some(listing)) => Ok(Opening::Session(Box::new(listing))),
            Ok(None) => Err(std::process::ExitCode::SUCCESS),
            Err(err) => Err(fail(format!("can't list sessions: {err}"))),
        };
    }
    let mut found: Vec<Listing> = sessions
        .into_iter()
        .filter(|listing| listing.has(&folder))
        .collect();
    if found.len() == 1 && !args.fresh && !names_file {
        return Ok(Opening::Session(Box::new(found.remove(0))));
    }
    Ok(Opening::Paths(named))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(arg: &str) -> Option<Destination> {
        destination(OsStr::new(arg))
    }

    fn at(host: &str, path: Option<&str>) -> Option<Destination> {
        Some(Destination {
            host: host.to_string(),
            path: path.map(str::to_string),
        })
    }

    #[test]
    fn a_colon_before_any_slash_names_a_host() {
        assert_eq!(named("devbox:src/cue"), at("devbox", Some("src/cue")));
        assert_eq!(named("me@devbox:/srv"), at("me@devbox", Some("/srv")));
        assert_eq!(named("devbox:"), at("devbox", None));
        assert_eq!(named("me@10.0.0.2"), at("me@10.0.0.2", None));
        assert_eq!(named("[::1]:src"), at("::1", Some("src")));
        assert_eq!(named("src/main.rs:12"), None);
        assert_eq!(named("./devbox:src"), None);
        assert_eq!(named("devbox"), None, "a folder here");
        assert_eq!(named("-o:x"), None);
    }

    #[test]
    fn a_file_here_with_a_position_isnt_a_host() {
        let dir = std::env::temp_dir().join(format!("cue-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let arg = format!("{}:12", file.display());
        assert_eq!(named(&arg), None, "a slash comes first");
        // `main.rs:12` is a host unless main.rs is here, which this can't
        // test without changing the folder every test runs in.
    }

    #[test]
    fn the_script_has_no_single_quotes_inside() {
        let platforms: Vec<(&Platform, Option<String>)> = PLATFORMS
            .iter()
            .map(|p| (p, Some("0.1-ab".into())))
            .collect();
        let script = script(&platforms);
        let inside = &script["sh -c '".len()..script.len() - 1];
        assert!(!inside.contains('\''), "{script}");
        assert!(!inside.contains('\n'), "{script}");
        assert!(inside.contains("\"Linux x86_64\"|\"Linux amd64\") p=linux-x64 b=0.1-ab;;"));
    }
}
