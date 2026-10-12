//! Running from local storage when cue's binary is on a network
//! filesystem, such as NFS.
//!
//! A process reads its binary's pages as it needs them, for as long as it
//! runs. Replacing the binary, as installing an update does, leaves the old
//! one readable to the processes running it on a local filesystem, but on
//! NFS only to those on the host that replaced it: elsewhere, the next page
//! read fails, and the process gets SIGBUS, hours later perhaps.
//!
//! So a cue whose binary isn't on local storage copies it to a local
//! folder, runs the copy in its place (the same process, by `exec`), and
//! removes the copy once nothing needs its path: it stays readable to the
//! processes running it, and its space is freed when they're gone. A
//! server removes it at once; a client, once it started its server from it
//! (see [`release`]). The original binary, and its build (see
//! [`crate::attach::build`]), are what a copy goes by: they're passed to
//! it, and to the server a client starts.
//!
//! Run from a removed binary, a backtrace can't name functions, so crash
//! reports list the addresses (see [`crate::crash`]).

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The binary a copy was made of.
const ORIGIN_VARIABLE: &str = "CUE_ORIGIN";
/// Its build.
const BUILD_VARIABLE: &str = "CUE_BUILD";
/// The copy, which this process removes once it's done with its path.
const COPY_VARIABLE: &str = "CUE_COPY";
/// For testing: copies the binary even from local storage.
const FORCE_VARIABLE: &str = "CUE_FORCE_COPY";

struct Passed {
    origin: Option<PathBuf>,
    build: Option<String>,
    copy: Option<PathBuf>,
}

static PASSED: OnceLock<Passed> = OnceLock::new();
static ORIGIN: OnceLock<PathBuf> = OnceLock::new();

/// Takes what this process was passed, and runs it from a copy on local
/// storage if its binary isn't on local storage. Returns unless it does:
/// before anything else, as everything this process opened by then is
/// closed (except what's to be kept across `exec`, as when a server takes
/// over from another; see [`crate::attach::exec`]).
pub fn init() {
    let passed = PASSED.get_or_init(|| {
        let take = |name| {
            let value = std::env::var_os(name).filter(|value| !value.is_empty());
            // Not for the shells it starts.
            std::env::remove_var(name);
            value
        };
        Passed {
            origin: take(ORIGIN_VARIABLE).map(PathBuf::from),
            build: take(BUILD_VARIABLE).and_then(|build| build.into_string().ok()),
            copy: take(COPY_VARIABLE).map(PathBuf::from),
        }
    });
    if passed.copy.is_some() {
        return;
    }
    let exe = origin();
    let forced = std::env::var_os(FORCE_VARIABLE).is_some_and(|value| value == "1");
    if !forced && is_local(exe) {
        return;
    }
    for dir in folders() {
        let copy = match copy_to(exe, &dir) {
            Ok(copy) => copy,
            Err(_) => continue,
        };
        let mut args = std::env::args_os();
        let arg0 = args.next().unwrap_or_else(|| exe.as_os_str().to_owned());
        // Returns only if the copy can't run, as from a folder mounted
        // noexec.
        let _ = Command::new(&copy)
            .arg0(arg0)
            .args(args)
            .env(ORIGIN_VARIABLE, exe)
            .env(BUILD_VARIABLE, crate::attach::build())
            .env(COPY_VARIABLE, &copy)
            .exec();
        let _ = fs::remove_file(&copy);
    }
}

/// The binary this cue was run as: the original, if this runs from a copy.
pub fn origin() -> &'static Path {
    ORIGIN.get_or_init(|| {
        if let Some(origin) = PASSED.get().and_then(|passed| passed.origin.clone()) {
            return origin;
        }
        std::env::current_exe().unwrap_or_default()
    })
}

/// The build passed from the original binary, if this runs from a copy.
pub fn passed_build() -> Option<&'static str> {
    PASSED.get()?.build.as_deref()
}

/// Whether this runs from a copy (see [`init`]).
pub fn from_copy() -> bool {
    PASSED.get().is_some_and(|passed| passed.copy.is_some())
}

/// The binary to start a server with: the copy, while it's there, so that
/// it runs from local storage too, or else the original.
pub fn exe() -> PathBuf {
    match PASSED.get().and_then(|passed| passed.copy.as_ref()) {
        Some(copy) if copy.is_file() => copy.clone(),
        _ => origin().to_path_buf(),
    }
}

/// Passes on to `command`, a server started from [`exe`], what a copy goes
/// by, and that it's to remove the copy.
pub fn pass_on(command: &mut Command) {
    if let Some(copy) = PASSED.get().and_then(|passed| passed.copy.as_ref()) {
        command
            .env(ORIGIN_VARIABLE, origin())
            .env(BUILD_VARIABLE, crate::attach::build())
            .env(COPY_VARIABLE, copy);
    }
}

/// Removes the copy this runs from, if it does: its path isn't needed
/// any more.
pub fn release() {
    if let Some(copy) = PASSED.get().and_then(|passed| passed.copy.as_ref()) {
        let _ = fs::remove_file(copy);
    }
}

/// Where copies may go: folders of this user's own on local storage.
fn folders() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let name = format!("cue-{uid}");
    // On disk, ahead of the temporary folder, which may be in memory.
    let mut folders = vec![Path::new("/var/tmp").join(&name)];
    let temp = std::env::temp_dir().join(&name);
    if !folders.contains(&temp) {
        folders.push(temp);
    }
    folders
}

/// Copies `exe` into `dir`, which is made if it isn't there, as this
/// process's copy: `cue-PID`. Copies left by processes that are gone are
/// removed first.
fn copy_to(exe: &Path, dir: &Path) -> io::Result<PathBuf> {
    private_folder(dir)?;
    if !is_local(dir) {
        return Err(io::Error::other("not on local storage"));
    }
    sweep(dir);
    let copy = dir.join(format!("cue-{}", std::process::id()));
    // A copy of a process that had this id: it may still run, so it's
    // removed, never written over.
    let _ = fs::remove_file(&copy);
    let result = fs::copy(exe, &copy)
        .and_then(|_| fs::set_permissions(&copy, fs::Permissions::from_mode(0o700)));
    match result {
        Ok(()) => Ok(copy),
        Err(err) => {
            let _ = fs::remove_file(&copy);
            Err(err)
        }
    }
}

/// Makes `dir` if it isn't there, for this user only, and checks that it
/// is: a folder, not a link to one, of this user's, that no one else can
/// write to.
pub fn private_folder(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    let meta = fs::symlink_metadata(dir)?;
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(io::Error::other("not a private folder"));
    }
    Ok(())
}

/// Removes the copies in `dir` of processes that are gone. Those of
/// processes running may be needed still, to start a server from.
fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|name| name.strip_prefix("cue-"))
            .and_then(|pid| pid.parse::<libc::pid_t>().ok());
        let Some(pid) = pid.filter(|&pid| pid > 0) else {
            continue;
        };
        let gone = unsafe { libc::kill(pid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether `path` is on local storage. Unless the system says so, it's
/// taken not to be: copying needlessly costs little.
fn is_local(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    is_local_filesystem(&stat)
}

#[cfg(target_os = "macos")]
fn is_local_filesystem(stat: &libc::statfs) -> bool {
    stat.f_flags & libc::MNT_LOCAL as u32 != 0
}

/// Linux doesn't say which filesystems are local, so these are those
/// known to be.
#[cfg(target_os = "linux")]
fn is_local_filesystem(stat: &libc::statfs) -> bool {
    const LOCAL: &[u32] = &[
        0xEF53,     // ext2, ext3, ext4
        0x58465342, // xfs
        0x9123683E, // btrfs
        0x2FC12FC1, // zfs
        0xF2F52010, // f2fs
        0xCA451A4E, // bcachefs
        0x01021994, // tmpfs
        0x858458F6, // ramfs
        0x794C7630, // overlayfs, as in containers
        0x73717368, // squashfs, as in snaps and AppImages
        0x3153464A, // jfs
        0x52654973, // reiserfs
        0x00003434, // nilfs
        0x0000F15F, // ecryptfs
        0x2011BAB0, // exfat
        0x00004D44, // vfat
        0x7366746E, // ntfs3
    ];
    LOCAL.contains(&(stat.f_type as u32))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn is_local_filesystem(_: &libc::statfs) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cue-relocate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_temporary_folder_is_local() {
        assert!(is_local(&std::env::temp_dir()));
        assert!(!is_local(Path::new("/no/such/folder")));
    }

    #[test]
    fn copies_go_in_a_private_folder() {
        let dir = scratch("copy");
        let exe = dir.with_extension("sh");
        fs::write(&exe, "#!/bin/sh\necho \"$@\"\n").unwrap();
        let exe = exe.as_path();
        let copy = copy_to(exe, &dir).unwrap();
        assert_eq!(copy, dir.join(format!("cue-{}", std::process::id())));
        let mode = fs::metadata(&dir).unwrap().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Not run: spawning a process would hold other tests' files open a
        // moment.
        assert_eq!(fs::read(&copy).unwrap(), fs::read(exe).unwrap());
        assert_eq!(fs::metadata(&copy).unwrap().mode() & 0o777, 0o700);
        // Again, as by a process with this id that's gone.
        assert_eq!(copy_to(exe, &dir).unwrap(), copy);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(exe);
    }

    #[test]
    fn folders_others_can_write_to_are_passed_over() {
        let dir = scratch("shared");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(copy_to(Path::new("/bin/sh"), &dir).is_err());
        let link = dir.with_extension("link");
        let _ = fs::remove_file(&link);
        let target = scratch("target");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(private_folder(&link).is_err(), "a link");
        let _ = fs::remove_file(&link);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&target);
    }

    #[test]
    fn copies_of_processes_that_are_gone_are_swept() {
        let dir = scratch("sweep");
        fs::create_dir_all(&dir).unwrap();
        // This process runs, and no process has the largest id.
        let running = dir.join(format!("cue-{}", std::process::id()));
        let gone = dir.join(format!("cue-{}", libc::pid_t::MAX));
        let other = dir.join("notes");
        for path in [&running, &gone, &other] {
            fs::write(path, "").unwrap();
        }
        sweep(&dir);
        assert!(running.exists(), "its process runs");
        assert!(!gone.exists(), "its process is gone");
        assert!(other.exists(), "not a copy");
        let _ = fs::remove_dir_all(&dir);
    }
}
