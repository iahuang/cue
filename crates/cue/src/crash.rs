//! Crash reports, kept where they can be found after, as what cue says
//! when it crashes in the background goes to no terminal.
//!
//! A panic writes a report, with a backtrace, to
//! `$XDG_STATE_HOME/cue/crashes`, or `~/.local/state/cue/crashes`. In the
//! background, stderr goes to a log there, locked while cue runs, which
//! catches what's printed by a crash that isn't a panic, such as one in
//! the native core, before cue aborts. A log that a cue that's gone left
//! with something in it becomes a report. A crash by a signal, such as
//! SIGBUS or SIGSEGV, which prints nothing, writes a report too. The next
//! cue to start says where the reports it hasn't said yet are.

use std::backtrace::Backtrace;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::relocate;

/// The names of the reports said, one a line.
const SEEN: &str = ".seen";

/// The panic there was, once the terminal is restored.
static PANIC: Mutex<Option<Panic>> = Mutex::new(None);
/// The log stderr goes to, while in the background.
static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where reports go.
pub fn default_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
    Some(state.join("cue/crashes"))
}

pub struct Panic {
    /// What to print of it.
    pub text: String,
    /// Its report, if it could be written.
    pub report: Option<PathBuf>,
}

/// Writes a report on a panic, and holds what to print of it until
/// [`take_panic`]: the renderer restores the terminal when dropped during
/// unwinding, and it isn't to be drawn into the alternate screen. Writes a
/// report on a crash by a signal too.
pub fn install() {
    install_signal_handler();
    std::panic::set_hook(Box::new(|info| {
        let backtrace = Backtrace::force_capture();
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let mut report = format!("{}, thread '{thread}'\n\n{info}\n\n{backtrace}", about());
        // The backtrace can't name functions without the binary.
        if relocate::from_copy() {
            report += &addresses();
        }
        let report = default_dir().and_then(|dir| write_report(&dir, &report).ok());
        let text = match &report {
            Some(path) => format!(
                "{info}\nThe report, with a backtrace, is in {}.",
                path.display()
            ),
            None => format!("{info}\n{backtrace}"),
        };
        *PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(Panic { text, report });
    }));
}

/// The panic there was, if any.
pub fn take_panic() -> Option<Panic> {
    PANIC.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Adds `report` to this process's report of the second in `dir`.
fn write_report(dir: &Path, report: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.txt", now(), std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(report.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(path)
}

/// A log for stderr while in the background, locked while this process
/// runs, if one can be made.
pub fn open_log() -> Option<fs::File> {
    let (file, path) = open_log_in(&default_dir()?).ok()?;
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    Some(file)
}

fn open_log_in(dir: &Path) -> io::Result<(fs::File, PathBuf)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.log", now(), std::process::id()));
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // The lock goes with the file once it's stderr, as it's the same open
    // file, until this process is gone.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((file, path))
}

/// Removes the log, once stderr is a terminal again or cue exits: what's
/// in it didn't come before a crash.
pub fn close_log() {
    if let Some(path) = LOG.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = fs::remove_file(path);
    }
}

/// Notes `report` as said, as it was printed to a terminal.
pub fn mark_seen(report: &Path) {
    let (Some(dir), Some(name)) = (report.parent(), report.file_name()) else {
        return;
    };
    let mut seen = read_seen(dir);
    seen.insert(name.to_string_lossy().into_owned());
    write_seen(dir, &seen);
}

/// Turns the logs that cues that are gone left with something in them into
/// reports, removing the rest, and returns the reports in `dir` not said
/// yet, oldest first, noting them as said.
pub fn collect(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("txt") => reports.push(path),
            Some("log") => {
                let Ok(file) = fs::File::open(&path) else {
                    continue;
                };
                // Still running.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    continue;
                }
                if file.metadata().is_ok_and(|meta| meta.len() > 0) {
                    let report = path.with_extension("txt");
                    if !report.exists() && fs::rename(&path, &report).is_ok() {
                        reports.push(report);
                        continue;
                    }
                }
                let _ = fs::remove_file(&path);
            }
            _ => {}
        }
    }
    reports.sort();
    let seen = read_seen(dir);
    let names: HashSet<String> = reports
        .iter()
        .filter_map(|report| Some(report.file_name()?.to_string_lossy().into_owned()))
        .collect();
    // Only the reports still there are kept noted.
    write_seen(dir, &names);
    reports.retain(|report| {
        report
            .file_name()
            .is_some_and(|name| !seen.contains(&*name.to_string_lossy()))
    });
    reports
}

fn read_seen(dir: &Path) -> HashSet<String> {
    fs::read_to_string(dir.join(SEEN))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn write_seen(dir: &Path, seen: &HashSet<String>) {
    let mut names: Vec<&str> = seen.iter().map(String::as_str).collect();
    names.sort();
    let mut text = names.join("\n");
    text.push('\n');
    let _ = fs::write(dir.join(SEEN), text);
}

/// The first line of a report: which cue, where, and which process.
fn about() -> String {
    let copy = match relocate::from_copy() {
        true => ", run from a copy",
        false => "",
    };
    format!(
        "cue {} ({}) on {} {}, process {}, binary {}{copy}",
        env!("CARGO_PKG_VERSION"),
        crate::attach::build(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        relocate::origin().display(),
    )
}

// --- addresses ------------------------------------------------------------------

/// The return addresses on the stack, to look up in the binary of this
/// build, as a backtrace does.
fn addresses() -> String {
    let mut addresses: Vec<usize> = Vec::new();
    unsafe { _Unwind_Backtrace(push_address, (&mut addresses as *mut Vec<usize>).cast()) };
    let bias = load_bias();
    let mut text = format!(
        "\nThe binary was removed, so here are the addresses instead. Less the load bias, 0x{bias:x}, \
         they're the binary's: look them up in a binary of this build with `addr2line -fe cue`, \
         or on macOS, `atos -o cue`.\n"
    );
    for address in addresses {
        text += &format!("  0x{:x}\n", address.wrapping_sub(bias));
    }
    text
}

extern "C" fn push_address(
    context: *mut libc::c_void,
    addresses: *mut libc::c_void,
) -> libc::c_int {
    let addresses = unsafe { &mut *addresses.cast::<Vec<usize>>() };
    let ip = unsafe { _Unwind_GetIP(context) };
    if ip != 0 && addresses.len() < 256 {
        addresses.push(ip);
    }
    0
}

extern "C" {
    fn _Unwind_Backtrace(
        trace: extern "C" fn(*mut libc::c_void, *mut libc::c_void) -> libc::c_int,
        data: *mut libc::c_void,
    ) -> libc::c_int;
    fn _Unwind_GetIP(context: *mut libc::c_void) -> usize;
}

/// How far from where it was linked to be cue's binary was loaded.
#[cfg(target_os = "macos")]
fn load_bias() -> usize {
    extern "C" {
        fn _dyld_get_image_vmaddr_slide(image_index: u32) -> libc::intptr_t;
    }
    // The binary is the first image.
    unsafe { _dyld_get_image_vmaddr_slide(0) as usize }
}

#[cfg(target_os = "linux")]
fn load_bias() -> usize {
    extern "C" fn first(
        info: *mut libc::dl_phdr_info,
        _: libc::size_t,
        bias: *mut libc::c_void,
    ) -> libc::c_int {
        unsafe { *bias.cast::<usize>() = (*info).dlpi_addr as usize };
        // The binary comes first.
        1
    }
    let mut bias = 0usize;
    unsafe { libc::dl_iterate_phdr(Some(first), (&mut bias as *mut usize).cast()) };
    bias
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn load_bias() -> usize {
    0
}

// --- signals --------------------------------------------------------------------

/// What a report on a crash by a signal needs, made beforehand, as the
/// handler can't allocate.
struct SignalReport {
    dir: CString,
    /// What it says before which signal it was.
    about: Vec<u8>,
}

static SIGNAL_REPORT: OnceLock<SignalReport> = OnceLock::new();
/// The signals handled, and the actions the handler took the place of.
static PREVIOUS: OnceLock<Vec<(libc::c_int, libc::sigaction)>> = OnceLock::new();
const SIGNALS: [libc::c_int; 2] = [libc::SIGBUS, libc::SIGSEGV];

fn install_signal_handler() {
    let Some(dir) = default_dir() else {
        return;
    };
    // The handler can only make the last of it.
    let _ = fs::create_dir_all(&dir);
    let Ok(dir) = CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    let about = format!("{}\n\n", about()).into_bytes();
    if SIGNAL_REPORT.set(SignalReport { dir, about }).is_err() {
        return;
    }
    let mut previous = Vec::new();
    for signal in SIGNALS {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_fault as extern "C" fn(_, _, _) as libc::sighandler_t;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, &action, &mut old) == 0 {
                previous.push((signal, old));
            }
        }
    }
    let _ = PREVIOUS.set(previous);
}

/// Writes a report, then puts back the action there was before, std's,
/// which tells a stack overflow, or the default: on return, the fault
/// happens again, for it.
extern "C" fn on_fault(signal: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    if let Some(report) = SIGNAL_REPORT.get() {
        unsafe { write_signal_report(report, signal, info) };
    }
    let previous = PREVIOUS.get().and_then(|previous| {
        previous
            .iter()
            .find(|(handled, _)| *handled == signal)
            .map(|(_, action)| action)
    });
    unsafe {
        match previous {
            Some(action) => libc::sigaction(signal, action, std::ptr::null_mut()),
            None => {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(signal, &default, std::ptr::null_mut())
            }
        };
    }
}

/// Writes a report on `signal`, in a signal handler: only with calls safe
/// there, into buffers on the stack.
unsafe fn write_signal_report(
    report: &SignalReport,
    signal: libc::c_int,
    info: *const libc::siginfo_t,
) {
    libc::mkdir(report.dir.as_ptr(), 0o700);
    let mut now: libc::timespec = std::mem::zeroed();
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut now);
    let mut path = StackText::<1024>::new();
    path.push(report.dir.as_bytes());
    path.push(b"/");
    path.push_number(now.tv_sec as u64, 10);
    path.push(b"-");
    path.push_number(libc::getpid() as u64, 10);
    path.push(b".txt\0");
    if path.full {
        return;
    }
    let fd = libc::open(
        path.bytes().as_ptr().cast(),
        libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
        0o600 as libc::c_uint,
    );
    if fd < 0 {
        return;
    }
    let mut line = StackText::<256>::new();
    line.push(match signal {
        libc::SIGBUS => b"SIGBUS".as_slice(),
        libc::SIGSEGV => b"SIGSEGV",
        _ => b"signal",
    });
    line.push(b" (code ");
    let code = (*info).si_code;
    if code < 0 {
        line.push(b"-");
    }
    line.push_number(code.unsigned_abs() as u64, 10);
    line.push(b") at address 0x");
    line.push_number((*info).si_addr() as u64, 16);
    line.push(b"\n");
    for bytes in [report.about.as_slice(), line.bytes()] {
        libc::write(fd, bytes.as_ptr().cast(), bytes.len());
    }
    libc::close(fd);
}

/// Text put together without allocating, for a signal handler.
struct StackText<const N: usize> {
    bytes: [u8; N],
    len: usize,
    /// Whether something didn't fit.
    full: bool,
}

impl<const N: usize> StackText<N> {
    fn new() -> Self {
        StackText {
            bytes: [0; N],
            len: 0,
            full: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(N - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        self.full |= n < bytes.len();
    }

    fn push_number(&mut self, mut n: u64, base: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b"0123456789abcdef"[(n % base) as usize];
            n /= base;
            if n == 0 {
                break;
            }
        }
        self.push(&digits[i..]);
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cue-crash-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn reports_are_said_once() {
        let dir = dir("said");
        let first = write_report(&dir, "first").unwrap();
        assert_eq!(collect(&dir), vec![first.clone()]);
        assert_eq!(collect(&dir), Vec::<PathBuf>::new());
        fs::write(dir.join("1-2.txt"), "second").unwrap();
        assert_eq!(collect(&dir), vec![dir.join("1-2.txt")]);
        // A report printed to a terminal was said.
        fs::write(dir.join("1-3.txt"), "third").unwrap();
        mark_seen(&dir.join("1-3.txt"));
        assert_eq!(collect(&dir), Vec::<PathBuf>::new());
        // A report removed isn't kept noted.
        fs::remove_file(&first).unwrap();
        collect(&dir);
        assert!(!read_seen(&dir).contains(&*first.file_name().unwrap().to_string_lossy()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn signals_write_reports() {
        let dir = dir("signal");
        let report = SignalReport {
            dir: CString::new(dir.as_os_str().as_bytes()).unwrap(),
            about: b"cue 1.2.3\n\n".to_vec(),
        };
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        info.si_code = 2;
        unsafe { write_signal_report(&report, libc::SIGBUS, &info) };
        let reports = collect(&dir);
        assert_eq!(reports.len(), 1);
        assert_eq!(
            fs::read_to_string(&reports[0]).unwrap(),
            "cue 1.2.3\n\nSIGBUS (code 2) at address 0x0\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stack_text_says_when_it_overflows() {
        let mut text = StackText::<8>::new();
        text.push_number(0xbeef, 16);
        text.push(b"-");
        text.push_number(42, 10);
        assert_eq!((text.bytes(), text.full), (b"beef-42".as_slice(), false));
        text.push(b"xy");
        assert_eq!((text.bytes(), text.full), (b"beef-42x".as_slice(), true));
    }

    #[test]
    fn logs_left_with_something_in_them_become_reports() {
        let dir = dir("logs");
        let (file, path) = open_log_in(&dir).unwrap();
        (&file).write_all(b"panic: index out of bounds\n").unwrap();
        // Locked while its cue runs.
        assert_eq!(collect(&dir), Vec::<PathBuf>::new());
        assert!(path.exists());
        drop(file);
        let report = path.with_extension("txt");
        assert_eq!(collect(&dir), vec![report.clone()]);
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(&report).unwrap(),
            "panic: index out of bounds\n"
        );
        // An empty one is removed.
        fs::write(dir.join("5-6.log"), "").unwrap();
        assert_eq!(collect(&dir), Vec::<PathBuf>::new());
        assert!(!dir.join("5-6.log").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
