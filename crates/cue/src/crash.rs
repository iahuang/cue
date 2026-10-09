//! Crash reports, kept where they can be found after, as what cue says
//! when it crashes in the background goes to no terminal.
//!
//! A panic writes a report, with a backtrace, to
//! `$XDG_STATE_HOME/cue/crashes`, or `~/.local/state/cue/crashes`. In the
//! background, stderr goes to a log there, locked while cue runs, which
//! catches what's printed by a crash that isn't a panic, such as one in
//! the native core, before cue aborts. A log that a cue that's gone left
//! with something in it becomes a report. The next cue to start says
//! where the reports it hasn't said yet are.

use std::backtrace::Backtrace;
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

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
/// unwinding, and it isn't to be drawn into the alternate screen.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = Backtrace::force_capture();
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let report = format!(
            "cue {} on {} {}, process {}, thread '{thread}'\n\n{info}\n\n{backtrace}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::process::id(),
        );
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
