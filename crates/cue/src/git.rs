//! What git says about the workspace: the repositories its folders are in,
//! the branch each is on, and the files changed since the last commit.
//!
//! `git` itself is asked, in the background, so a large repository never
//! blocks the screen, and so whatever it's configured to do (fsmonitor,
//! the untracked cache) applies. It's asked again when the app hears of
//! changes to files or to a repository's `.git` folder, and every so often
//! besides, since the app doesn't watch every folder: an agent may change
//! files in folders the tree hasn't opened.
//!
//! Each run also counts the lines added and removed since the last commit,
//! staged or not, as `git diff --numstat` does, and the lines of new files
//! git doesn't know of yet, which it leaves out.
//!
//! git is run with `--no-optional-locks`, so it never writes the index:
//! otherwise each run would change the `.git` folder that's watched, and
//! run it again.

use std::cell::{Cell, OnceCell};
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::theme::Hue;

/// Changes past this many in a repository aren't listed.
const MAX_CHANGES: usize = 10_000;
/// New files git doesn't know of yet are counted as lines added only up to
/// this big; bigger ones count as none, as binary files do.
const MAX_COUNTED_FILE: u64 = 1 << 20;
/// And only up to this much of them in all, in a repository.
const MAX_COUNTED: u64 = 32 << 20;
/// The tree with nothing in it, to compare with before the first commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// How long after one run ends the next starts, with nothing heard of in
/// between, at least. A slow repository waits longer (see [`SLOWDOWN`]).
const INTERVAL: Duration = Duration::from_secs(3);
/// How many times as long as the last run took to wait before the next one
/// nothing asked for.
const SLOWDOWN: u32 = 10;
/// The least time between the starts of two runs, however often changes
/// come.
const MIN_GAP: Duration = Duration::from_millis(500);

/// How a file changed since the last commit, staged or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Modified,
    /// New to git, and staged.
    Added,
    Deleted,
    Renamed,
    /// New, and not staged: git doesn't know of it yet.
    Untracked,
    /// Both sides of a merge changed it.
    Conflicted,
}

impl Kind {
    /// The letter lists show it by, as VS Code's are.
    pub fn letter(self) -> char {
        match self {
            Kind::Modified => 'M',
            Kind::Added => 'A',
            Kind::Deleted => 'D',
            Kind::Renamed => 'R',
            Kind::Untracked => 'U',
            Kind::Conflicted => '!',
        }
    }

    pub fn hue(self) -> Hue {
        match self {
            Kind::Modified => Hue::Yellow,
            Kind::Added | Kind::Untracked => Hue::Green,
            Kind::Deleted | Kind::Conflicted => Hue::Red,
            Kind::Renamed => Hue::Sky,
        }
    }
}

/// How much of a file's change is staged, to go in the next commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Staged {
    #[default]
    No,
    /// Some of it: it changed again since it was staged.
    Partly,
    Yes,
}

/// A file changed since the last commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Where it is, or was, if it's gone.
    pub path: PathBuf,
    pub kind: Kind,
    /// Where it was, if it was renamed.
    pub from: Option<PathBuf>,
    /// How much of it is staged; [`Staged::No`] for a commit's.
    pub staged: Staged,
}

/// What a repository has checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// A branch, which may not have any commits yet.
    Branch(String),
    /// A commit no branch is on, by its abbreviated hash.
    Detached(String),
}

impl Head {
    /// How it's shown: the branch's name or the commit's hash.
    pub fn name(&self) -> &str {
        match self {
            Head::Branch(name) | Head::Detached(name) => name,
        }
    }
}

/// A repository a workspace folder is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Its working tree's top folder.
    pub root: PathBuf,
    /// Its `.git` folder, which may be elsewhere, as a worktree's is.
    pub git_dir: PathBuf,
    pub head: Head,
    /// The commit checked out, by its hash; `None` before the first.
    pub commit: Option<String>,
    /// Sorted by path; past [`MAX_CHANGES`], the rest aren't listed.
    pub changes: Vec<Change>,
    /// Lines added since the last commit, staged or not, and in new files.
    pub added: usize,
    /// Lines removed since the last commit, staged or not.
    pub removed: usize,
    /// The changes stashed, newest first.
    pub stashes: Vec<Stash>,
}

/// What git says of a file: the repository it's in, how it changed since
/// the last commit, if it did, and what it was in that commit, read the
/// first time it's asked for.
#[derive(Debug)]
pub struct Tracked {
    pub root: PathBuf,
    /// The commit checked out; `None` before the first.
    commit: Option<String>,
    /// Where it was in that commit, from the root: where it is now, or
    /// where it was renamed from.
    old_path: PathBuf,
    /// How it changed, if git says it did.
    pub kind: Cell<Option<Kind>>,
    base: OnceCell<Base>,
}

/// What a file was in the last commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// Its text, with `\n` line breaks.
    Text(String),
    /// It wasn't in it, or there's no commit yet.
    Missing,
    Binary,
}

impl Tracked {
    /// Whether `other` is of the same file in the same commit, so what it
    /// was then is the same.
    pub fn same_base(&self, other: &Tracked) -> bool {
        (&self.root, &self.commit, &self.old_path) == (&other.root, &other.commit, &other.old_path)
    }

    /// What the file was in the last commit, asking git the first time.
    pub fn base(&self) -> &Base {
        self.base.get_or_init(|| {
            let Some(commit) = &self.commit else {
                return Base::Missing;
            };
            file_at(&self.root, commit, &self.old_path)
        })
    }

    /// What the file was in the last commit, if git was asked already.
    pub fn base_if_read(&self) -> Option<&Base> {
        self.base.get()
    }
}

/// A commit, as the log lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    /// The hash as git abbreviates it.
    pub short: String,
    /// Its first parent, by hash; `None` for a repository's first commit.
    pub parent: Option<String>,
    pub author: String,
    /// When it was made, in seconds since the Unix epoch.
    pub time: i64,
    /// The first line of its message.
    pub subject: String,
}

/// The commits that led to what's checked out in the repository at
/// `root`, newest first: `count` of them, after the first `skip`. None
/// before the first commit.
pub fn log(root: &Path, skip: usize, count: usize) -> Vec<Commit> {
    let output = git(root)
        .args([
            "log",
            "--no-color",
            "--format=%H%x1f%h%x1f%P%x1f%an%x1f%at%x1f%s",
        ])
        .arg(format!("--skip={skip}"))
        .arg(format!("--max-count={count}"))
        .output();
    match output {
        Ok(output) if output.status.success() => parse_log(&output.stdout),
        _ => Vec::new(),
    }
}

fn parse_log(output: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, '\x1f');
            let mut next = || fields.next().map(str::to_string);
            let (hash, short, parents, author, time) =
                (next()?, next()?, next()?, next()?, next()?);
            Some(Commit {
                hash,
                short,
                parent: parents
                    .split(' ')
                    .next()
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                author,
                time: time.parse().unwrap_or(0),
                subject: next().unwrap_or_default(),
            })
        })
        .collect()
}

/// The files `commit`, in the repository at `root`, changed from its
/// first parent, sorted by path, with renames found as `git status` finds
/// them.
pub fn commit_changes(root: &Path, commit: &Commit) -> Vec<Change> {
    let parent = commit.parent.as_deref().unwrap_or(EMPTY_TREE);
    let output = git(root)
        .args([
            "diff-tree",
            "-r",
            "-M",
            "-z",
            "--name-status",
            "--no-commit-id",
        ])
        .args([parent, &commit.hash])
        .output();
    let mut changes = match output {
        Ok(output) if output.status.success() => parse_name_status(root, &output.stdout),
        _ => Vec::new(),
    };
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// `git diff-tree --name-status -z`'s output: a status, then a path, or
/// for a rename or copy, the old path and the new.
fn parse_name_status(root: &Path, output: &[u8]) -> Vec<Change> {
    let mut fields = output
        .split(|&byte| byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let (kind, from, path) = match status.chars().next() {
            Some('A') => (Kind::Added, None, path),
            Some('D') => (Kind::Deleted, None, path),
            Some('R') => match fields.next() {
                Some(to) => (Kind::Renamed, Some(root.join(path)), to),
                None => break,
            },
            // A copy is new, as far as what changed goes.
            Some('C') => match fields.next() {
                Some(to) => (Kind::Added, None, to),
                None => break,
            },
            Some('U') => (Kind::Conflicted, None, path),
            Some(_) => (Kind::Modified, None, path),
            None => continue,
        };
        changes.push(Change {
            path: root.join(path),
            kind,
            from,
            staged: Staged::No,
        });
    }
    changes
}

/// What the file at `path`, from `root`, was in `commit`.
pub fn file_at(root: &Path, commit: &str, path: &Path) -> Base {
    let spec = format!("{commit}:{}", path.to_string_lossy());
    let output = git(root).args(["cat-file", "blob", &spec]).output();
    match output {
        Ok(output) if output.status.success() => base_text(output.stdout),
        _ => Base::Missing,
    }
}

/// A file's bytes as [`Base`] has them: binary if there's a NUL early on,
/// as git tells, and otherwise its text, with `\r\n` as `\n`, as editors
/// have it.
fn base_text(bytes: Vec<u8>) -> Base {
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Base::Binary;
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    };
    match text.contains('\r') {
        true => Base::Text(text.replace("\r\n", "\n")),
        false => Base::Text(text),
    }
}

/// Stages the files at `paths`, in the repository at `root`, as they are
/// on disk: changed, new, or gone.
pub fn stage(root: &Path, paths: &[PathBuf]) -> Result<(), String> {
    change_index(root, &["add", "-A"], paths)
}

/// Unstages the files at `paths`, in the repository at `root`, leaving them
/// as they are on disk. A repository with no commits yet (`initial`) has
/// nothing to put back, so they're only taken out of the index.
pub fn unstage(root: &Path, paths: &[PathBuf], initial: bool) -> Result<(), String> {
    match initial {
        true => change_index(
            root,
            &["rm", "--cached", "-r", "-q", "--ignore-unmatch"],
            paths,
        ),
        false => change_index(root, &["restore", "--staged"], paths),
    }
}

/// Runs `git` with `args` on the files at `paths`, in the repository at
/// `root`, given on its input, however many there are, and taken as they
/// are, not as patterns. Returns what git said if it failed.
fn change_index(root: &Path, args: &[&str], paths: &[PathBuf]) -> Result<(), String> {
    let mut spec = Vec::new();
    for path in paths {
        let path = path.strip_prefix(root).unwrap_or(path);
        spec.extend_from_slice(path.as_os_str().as_encoded_bytes());
        spec.push(0);
    }
    let mut command = writing(root);
    // Only here: `git stash -u` with it leaves the new files it stashed
    // behind.
    command
        .env("GIT_LITERAL_PATHSPECS", "1")
        .args(args)
        .args(["--pathspec-from-file=-", "--pathspec-file-nul"]);
    run_with_input(command, &spec).map(drop)
}

/// The full message of the last commit in the repository at `root`, if
/// there is one.
pub fn last_message(root: &Path) -> Option<String> {
    let output = git(root).args(["log", "-1", "--format=%B"]).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    })
}

/// Commits what's staged in the repository at `root`, with `message`, or
/// replaces the last commit with it, if `amend`. Hooks run, and the commit
/// is signed if git's set up to sign. Returns the new commit's abbreviated
/// hash, or what git, or a hook, said went wrong.
pub fn commit(root: &Path, message: &str, amend: bool) -> Result<String, String> {
    let mut command = writing(root);
    command.args(["commit", "-F", "-"]);
    if amend {
        command.arg("--amend");
    }
    run_with_input(command, message.as_bytes())?;
    let output = git(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map_err(|err| err.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Changes put away, as `git stash` keeps them: a commit of the files as
/// they were, whose first parent is the commit checked out then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Its subject is what it was stashed as, without the branch it was
    /// stashed on.
    pub commit: Commit,
    /// The commit git keeps the new files it didn't know of in, by hash,
    /// if they were stashed too.
    pub untracked: Option<String>,
}

/// The changes stashed in the repository at `root`, newest first.
pub fn stashes(root: &Path) -> Vec<Stash> {
    let output = git(root)
        .args([
            "stash",
            "list",
            "--format=%H%x1f%h%x1f%P%x1f%an%x1f%at%x1f%gs",
        ])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output.stdout,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, '\x1f');
            let mut next = || fields.next().map(str::to_string);
            let (hash, short, parents, author, time) =
                (next()?, next()?, next()?, next()?, next()?);
            let subject = next().unwrap_or_default();
            let mut parents = parents.split(' ').map(str::to_string);
            let parent = parents.next().filter(|p| !p.is_empty());
            Some(Stash {
                commit: Commit {
                    hash,
                    short,
                    parent,
                    author,
                    time: time.parse().unwrap_or(0),
                    subject: stash_subject(&subject),
                },
                untracked: parents.nth(1),
            })
        })
        .collect()
}

/// What a stash was stashed as, from what git says of it: `On main: what`
/// with a message, and `WIP on main: 0123abc subject` without.
fn stash_subject(subject: &str) -> String {
    match subject
        .strip_prefix("On ")
        .and_then(|rest| rest.split_once(": "))
    {
        Some((_, message)) => message.to_string(),
        None => subject.to_string(),
    }
}

/// The files `stash`, in the repository at `root`, changed, each with the
/// commit that has it as it was stashed: the stash's own, or for a new
/// file git didn't know of, the one it keeps those in. Sorted by path.
pub fn stash_changes(root: &Path, stash: &Stash) -> Vec<(Commit, Change)> {
    let mut changes: Vec<(Commit, Change)> = commit_changes(root, &stash.commit)
        .into_iter()
        .map(|change| (stash.commit.clone(), change))
        .collect();
    if let Some(untracked) = &stash.untracked {
        let commit = Commit {
            hash: untracked.clone(),
            short: untracked.chars().take(stash.commit.short.len()).collect(),
            parent: None,
            ..stash.commit.clone()
        };
        let new = commit_changes(root, &commit);
        changes.extend(new.into_iter().map(|change| (commit.clone(), change)));
    }
    changes.sort_by(|(_, a), (_, b)| a.path.cmp(&b.path));
    changes
}

/// Stashes the changes in the repository at `root`, as `message`, if
/// there is one: only what's staged, if `staged`, and otherwise all of
/// them, new files git doesn't know of included.
pub fn stash(root: &Path, message: &str, staged: bool) -> Result<(), String> {
    let mut command = writing(root);
    command.args(["stash", "push", "--quiet"]);
    command.arg(if staged {
        "--staged"
    } else {
        "--include-untracked"
    });
    let message = message.trim();
    if !message.is_empty() {
        command.args(["-m", message]);
    }
    run_with_input(command, b"").map(drop)
}

/// Puts the changes stashed as `hash`, in the repository at `root`, back,
/// and if `pop`, drops the stash, unless they don't go back cleanly.
/// Returns what git said went wrong: conflicts, or files in the way.
pub fn apply_stash(root: &Path, hash: &str, pop: bool) -> Result<(), String> {
    let name = stash_name(root, hash)?;
    let mut command = writing(root);
    // Not quiet: what conflicted is said along the way.
    command.args(["stash", if pop { "pop" } else { "apply" }, &name]);
    run_with_input(command, b"").map(drop)
}

/// Drops the changes stashed as `hash`, in the repository at `root`.
pub fn drop_stash(root: &Path, hash: &str) -> Result<(), String> {
    let name = stash_name(root, hash)?;
    let mut command = writing(root);
    command.args(["stash", "drop", "--quiet", &name]);
    run_with_input(command, b"").map(drop)
}

/// What git calls the stash `hash` now, `stash@{2}`: by where it is in
/// the list, which changes as stashes come and go.
fn stash_name(root: &Path, hash: &str) -> Result<String, String> {
    let output = git(root)
        .args(["stash", "list", "--format=%H %gd"])
        .output()
        .map_err(|err| err.to_string())?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix(hash)?
                .strip_prefix(' ')
                .map(str::to_string)
        })
        .ok_or_else(|| "The stash is gone.".to_string())
}

/// A branch to switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Its name: `main`, or a remote's, `origin/main`.
    pub name: String,
    /// It's a remote's, with no branch of its own here yet.
    pub remote: bool,
    /// It's checked out.
    pub current: bool,
    /// When its last commit was made, in seconds since the Unix epoch.
    pub time: i64,
}

/// The branches of the repository at `root`: its own, and the remotes' it
/// has none of its own for. The one checked out is first, then those with
/// the latest commits.
pub fn branches(root: &Path) -> Vec<Branch> {
    let output = git(root)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%1f%(refname:strip=2)%1f%(HEAD)%1f%(committerdate:unix)%1f%(symref)",
            "refs/heads",
            "refs/remotes",
        ])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output.stdout,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output);
    let mut branches: Vec<Branch> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\x1f');
            let (full, name, head, time, symref) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            // A remote's HEAD only says which of its branches is the main one.
            if !symref.is_empty() {
                return None;
            }
            Some(Branch {
                name: name.to_string(),
                remote: full.starts_with("refs/remotes/"),
                current: head == "*",
                time: time.parse().unwrap_or(0),
            })
        })
        .collect();
    let local: HashSet<String> = branches
        .iter()
        .filter(|branch| !branch.remote)
        .map(|branch| branch.name.clone())
        .collect();
    branches.retain(|branch| {
        !branch.remote
            || branch
                .name
                .split_once('/')
                .is_some_and(|(_, name)| !local.contains(name))
    });
    // The one checked out first, to say where it's switching from.
    branches.sort_by_key(|branch| !branch.current);
    branches
}

/// Where to switch a repository to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTo {
    Branch(String),
    /// A new branch, by this name, at the commit checked out.
    New(String),
    /// A branch of its own for this remote's branch, which tracks it.
    Track(String),
}

impl SwitchTo {
    /// The name of the branch it switches to.
    pub fn name(&self) -> &str {
        match self {
            SwitchTo::Branch(name) | SwitchTo::New(name) => name,
            SwitchTo::Track(remote) => remote.split_once('/').map_or(remote, |(_, name)| name),
        }
    }
}

/// How the changes not committed came along when switching branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carried {
    /// As they were, since the branch didn't change the files they did.
    Along,
    /// Stashed, then put back once switched.
    Stashed,
    /// Stashed, and not put back cleanly: what git said. The stash is kept.
    Conflicts(String),
}

/// Switches the repository at `root` to `to`, bringing the changes not
/// committed along. If git won't switch with them, as when the branch
/// changed the same files, they're stashed first, and put back after.
pub fn switch(root: &Path, to: &SwitchTo) -> Result<Carried, String> {
    let run = || {
        let mut command = writing(root);
        command.args(["switch", "--no-overwrite-ignore"]);
        match to {
            SwitchTo::Branch(name) => command.args(["--", name]),
            SwitchTo::New(name) => command.args(["-c", name]),
            SwitchTo::Track(remote) => command.args(["--track", &format!("refs/remotes/{remote}")]),
        };
        run_with_input(command, b"").map(drop)
    };
    let err = match run() {
        Ok(()) => return Ok(Carried::Along),
        Err(err) => err,
    };
    let in_the_way = ["would be overwritten", "commit your changes or stash them"];
    if !in_the_way.iter().any(|said| err.contains(said)) {
        return Err(err);
    }
    let message = format!("Switching to {}", to.name());
    let before = stashes(root).first().map(|stash| stash.commit.hash.clone());
    stash(root, &message, false)?;
    let created = stashes(root).into_iter().next();
    let Some(created) = created.filter(|stash| Some(&stash.commit.hash) != before.as_ref()) else {
        // Ignored files aren't stashed. If they're all that's in the way,
        // no stash was made, and an older one must not be popped.
        return Err(err);
    };
    let restore = || {
        let mut apply = writing(root);
        apply.args(["stash", "apply", "--index", &created.commit.hash]);
        run_with_input(apply, b"")?;
        drop_stash(root, &created.commit.hash)
    };
    if let Err(err) = run() {
        // Back as they were.
        let _ = restore();
        return Err(err);
    }
    match restore() {
        Ok(_) => Ok(Carried::Stashed),
        Err(err) => Ok(Carried::Conflicts(err)),
    }
}

/// `git` for something that changes the repository at `dir`: as [`git`],
/// but in a session of its own, without the terminal, so a hook or a
/// signing program that asks for something can't read from it or draw
/// over the screen.
fn writing(dir: &Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GPG_TTY");
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
}

/// Runs `command` with `input`, and returns what it printed, or if it
/// failed, what it said: its errors, or else what it printed.
fn run_with_input(mut command: Command, input: &[u8]) -> Result<String, String> {
    use std::io::Write;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Can't run git: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // git reads all of it before it writes much, if anything.
        let _ = stdin.write_all(input);
    }
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(match stderr.is_empty() {
        true => stdout,
        false => stderr,
    })
}

pub struct Git {
    roots: Vec<PathBuf>,
    /// As of the last run, in the order of the roots they hold.
    repos: Vec<Repo>,
    /// The run in progress, if any, and when it started.
    running: Option<(Receiver<Vec<Repo>>, Instant)>,
    /// Something asked for a run while one was in progress, or too soon
    /// after the last started.
    again: bool,
    /// When the last run started and how long it took, once it ended.
    last: Option<(Instant, Duration)>,
}

impl Git {
    /// Starts finding out about the repositories `roots` are in.
    pub fn new(roots: &[PathBuf]) -> Git {
        let mut git = Git {
            roots: roots.to_vec(),
            repos: Vec::new(),
            running: None,
            again: false,
            last: None,
        };
        git.start();
        git
    }

    /// Follows the workspace's folders changing.
    pub fn set_roots(&mut self, roots: &[PathBuf]) {
        if self.roots != roots {
            self.roots = roots.to_vec();
            self.refresh();
        }
    }

    /// The repositories as last heard, in the order of the roots they hold.
    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    /// The repository `path` is in, if any: the deepest, should they nest.
    pub fn repo_of(&self, path: &Path) -> Option<&Repo> {
        self.repos
            .iter()
            .filter(|repo| path.starts_with(&repo.root))
            .max_by_key(|repo| repo.root.components().count())
    }

    /// What git says of the file at `path`, if it's in a repository.
    pub fn tracked(&self, path: &Path) -> Option<Tracked> {
        let repo = self.repo_of(path)?;
        let change = repo
            .changes
            .binary_search_by(|change| change.path.as_path().cmp(path))
            .ok()
            .map(|index| &repo.changes[index]);
        let old = change
            .and_then(|change| change.from.as_deref())
            .unwrap_or(path);
        Some(Tracked {
            root: repo.root.clone(),
            commit: repo.commit.clone(),
            old_path: old.strip_prefix(&repo.root).ok()?.to_path_buf(),
            kind: Cell::new(change.map(|change| change.kind)),
            base: OnceCell::new(),
        })
    }

    /// Asks git again: now, unless a run is in progress or one started
    /// moments ago, and otherwise once [`Git::poll`] finds it can.
    pub fn refresh(&mut self) {
        self.again = true;
        let soon = self
            .last
            .is_some_and(|(started, _)| started.elapsed() < MIN_GAP);
        if self.running.is_none() && !soon {
            self.start();
        }
    }

    /// Asks git again now, dropping a run in progress, which may have
    /// read the repositories before this app changed them.
    pub fn restart(&mut self) {
        self.running = None;
        self.start();
    }

    /// Takes the result of a run that ended, and starts another if one was
    /// asked for or it's time. Returns whether the repositories changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some((receiver, started)) = &self.running {
            match receiver.try_recv() {
                Ok(repos) => {
                    self.last = Some((*started, started.elapsed()));
                    self.running = None;
                    changed = repos != self.repos;
                    self.repos = repos;
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.last = Some((*started, started.elapsed()));
                    self.running = None;
                }
            }
        }
        let due = match self.last {
            None => true,
            Some((started, took)) => {
                let since = started.elapsed();
                let ended = since.saturating_sub(took);
                (self.again && since >= MIN_GAP) || ended >= INTERVAL.max(took * SLOWDOWN)
            }
        };
        if due {
            self.start();
        }
        changed
    }

    /// Waits for the run in progress, for tests.
    #[cfg(test)]
    pub fn wait(&mut self) {
        while let Some((receiver, started)) = self.running.take() {
            if let Ok(repos) = receiver.recv() {
                self.repos = repos;
            }
            self.last = Some((started, started.elapsed()));
        }
    }

    fn start(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let roots = self.roots.clone();
        std::thread::spawn(move || {
            let _ = sender.send(read_repos(&roots));
        });
        self.running = Some((receiver, Instant::now()));
        self.again = false;
    }
}

/// The repositories `roots` are in, each once, in the order of the roots.
fn read_repos(roots: &[PathBuf]) -> Vec<Repo> {
    let mut repos: Vec<Repo> = Vec::new();
    for root in roots {
        let Some((top, git_dir)) = locate(root) else {
            continue;
        };
        if repos.iter().any(|repo| repo.root == top) {
            continue;
        }
        if let Some(repo) = status(&top, git_dir) {
            repos.push(repo);
        }
    }
    repos
}

/// `git` with what every run here needs: in `dir`, never asking for
/// anything, and never writing the index.
fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The top folder of the working tree `dir` is in, and its `.git` folder,
/// if it's in one.
fn locate(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = git(dir)
        .args(["rev-parse", "--show-toplevel", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let top = PathBuf::from(lines.next().filter(|line| !line.is_empty())?);
    let git_dir = PathBuf::from(lines.next()?);
    Some((top, git_dir))
}

/// The branch and changes of the repository whose working tree is `top`.
fn status(top: &Path, git_dir: PathBuf) -> Option<Repo> {
    let output = git(top)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let (head, commit, changes) = parse_status(&output.stdout);
    let stashes = stashes(top);
    let (mut added, removed) = diff_lines(top, commit.is_none());
    let untracked = changes
        .iter()
        .filter(|(_, kind, ..)| *kind == Kind::Untracked)
        .map(|(path, ..)| top.join(path));
    added += count_lines(untracked);
    let mut changes: Vec<Change> = changes
        .into_iter()
        .take(MAX_CHANGES)
        .map(|(path, kind, from, staged)| Change {
            path: top.join(path),
            kind,
            from: from.map(|from| top.join(from)),
            staged,
        })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Some(Repo {
        root: top.to_path_buf(),
        git_dir,
        head,
        commit,
        changes,
        added,
        removed,
        stashes,
    })
}

/// The lines added and removed in the working tree `top`, staged or not,
/// since the last commit, or if there's none yet, since the start.
fn diff_lines(top: &Path, initial: bool) -> (usize, usize) {
    let base = if initial { EMPTY_TREE } else { "HEAD" };
    let output = git(top)
        .args([
            "diff",
            "--numstat",
            "--no-ext-diff",
            "--no-color",
            base,
            "--",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => parse_numstat(&output.stdout),
        _ => (0, 0),
    }
}

/// The lines added and removed in all, from `git diff --numstat`: a line
/// per file, starting with the counts, which are `-` for a binary file.
fn parse_numstat(output: &[u8]) -> (usize, usize) {
    let text = String::from_utf8_lossy(output);
    text.lines().fold((0, 0), |(added, removed), line| {
        let mut counts = line.split('\t').map(|n| n.parse::<usize>().unwrap_or(0));
        let (a, r) = (counts.next().unwrap_or(0), counts.next().unwrap_or(0));
        (added + a, removed + r)
    })
}

/// The lines of the text files `paths`, as git would count them, but for
/// those past [`MAX_COUNTED_FILE`], or [`MAX_COUNTED`] in all.
fn count_lines(paths: impl Iterator<Item = PathBuf>) -> usize {
    let mut budget = MAX_COUNTED;
    let mut lines = 0;
    let mut bytes = Vec::new();
    for path in paths {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let size = file.metadata().map_or(u64::MAX, |meta| meta.len());
        if size > MAX_COUNTED_FILE || size > budget {
            continue;
        }
        budget -= size;
        bytes.clear();
        if file.take(size).read_to_end(&mut bytes).is_err() {
            continue;
        }
        // Binary, as git tells: a NUL early on.
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue;
        }
        lines += bytes.iter().filter(|&&byte| byte == b'\n').count();
        // A last line without a newline counts too.
        lines += usize::from(bytes.last().is_some_and(|&byte| byte != b'\n'));
    }
    lines
}

/// A changed file's path in the working tree, how it changed, if it was
/// renamed, its path before, and how much of it is staged.
type Entry = (PathBuf, Kind, Option<PathBuf>, Staged);

/// What `git status --porcelain=v2 --branch -z` says: the head, the commit
/// checked out, unless there's none yet, and the files changed.
fn parse_status(output: &[u8]) -> (Head, Option<String>, Vec<Entry>) {
    let mut oid = None;
    let mut branch = None;
    let mut changes = Vec::new();
    let mut fields = output
        .split(|&byte| byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    while let Some(field) = fields.next() {
        if let Some(header) = field.strip_prefix("# ") {
            if let Some(value) = header.strip_prefix("branch.oid ") {
                oid = Some(value.to_string());
            } else if let Some(value) = header.strip_prefix("branch.head ") {
                branch = Some(value.to_string());
            }
            continue;
        }
        let mut parts = field.splitn(2, ' ');
        let (Some(kind), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        let change = match kind {
            "?" => Some((rest.to_string(), Kind::Untracked, None, Staged::No)),
            // XY sub mH mI mW hH hI path
            "1" => rest
                .splitn(8, ' ')
                .nth(7)
                .map(|path| (path.to_string(), ordinary(rest), None, staged(rest))),
            // XY sub mH mI mW hH hI Xscore path, then the old path.
            "2" => {
                let mut parts = rest.splitn(9, ' ').skip(7);
                let score = parts.next().unwrap_or("");
                let path = parts.next().map(str::to_string);
                let from = fields.next();
                // A copy is new, as far as what changed goes.
                let (kind, from) = match score.starts_with('C') {
                    true => (Kind::Added, None),
                    false => (Kind::Renamed, from),
                };
                path.map(|path| (path, kind, from, staged(rest)))
            }
            // XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => rest
                .splitn(10, ' ')
                .nth(9)
                .map(|path| (path.to_string(), Kind::Conflicted, None, Staged::No)),
            _ => None,
        };
        if let Some((path, kind, from, staged)) = change {
            let from = from.map(PathBuf::from);
            changes.push((PathBuf::from(path), kind, from, staged));
        }
    }
    let commit = oid.clone().filter(|oid| oid != "(initial)");
    let head = match branch {
        Some(branch) if branch != "(detached)" => Head::Branch(branch),
        _ => {
            let oid = oid.unwrap_or_default();
            Head::Detached(oid.chars().take(7).collect())
        }
    };
    (head, commit, changes)
}

/// How much of a changed entry is staged, from the `XY` its line starts
/// with: what changed in the index (X), and in the working tree since (Y).
fn staged(rest: &str) -> Staged {
    let mut xy = rest.chars();
    let (x, y) = (xy.next().unwrap_or('.'), xy.next().unwrap_or('.'));
    match (x != '.', y != '.') {
        (false, _) => Staged::No,
        (true, true) => Staged::Partly,
        (true, false) => Staged::Yes,
    }
}

/// How an ordinary changed entry changed, from the `XY` its line starts
/// with: staged (X) and not (Y).
fn ordinary(rest: &str) -> Kind {
    let mut xy = rest.chars();
    let (x, y) = (xy.next().unwrap_or('.'), xy.next().unwrap_or('.'));
    if x == 'D' || y == 'D' {
        Kind::Deleted
    } else if x == 'A' {
        Kind::Added
    } else {
        Kind::Modified
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-git-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=cue", "-c", "user.email=cue@example.com"])
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Commits everything in the repository at `dir`, as `message`.
    pub(crate) fn commit_all(dir: &Path, message: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", message]);
    }

    /// A repository with three commits: `first`, of `a.txt` and `b.txt`;
    /// `second`, which changes `a.txt`, renames `b.txt` to `c.txt`, and
    /// adds `d.txt`; and `third`, which deletes `d.txt`.
    pub(crate) fn history_repo(name: &str) -> PathBuf {
        let dir = repo(name);
        fs::write(dir.join("a.txt"), "a\nmore\n").unwrap();
        run(&dir, &["mv", "b.txt", "c.txt"]);
        fs::write(dir.join("d.txt"), "d\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "second"]);
        run(&dir, &["rm", "-q", "d.txt"]);
        run(&dir, &["commit", "-q", "-m", "third"]);
        dir
    }

    /// A repository on `main` with one commit, of `a.txt` and `b.txt`.
    pub(crate) fn repo(name: &str) -> PathBuf {
        let dir = fixture(name);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        fs::write(dir.join("b.txt"), "b\n").unwrap();
        run(&dir, &["init", "-q"]);
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "first"]);
        dir
    }

    #[test]
    fn parses_each_kind_of_entry() {
        let output = b"# branch.oid 0123456789abcdef\0# branch.head main\0\
            1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
            1 MM N... 100644 100644 100644 aaa bbb half.rs\0\
            1 A. N... 000000 100644 100644 000 bbb new.rs\0\
            1 .D N... 100644 100644 000000 aaa aaa gone.rs\0\
            2 R. N... 100644 100644 100644 aaa aaa R100 to.rs\0from.rs\0\
            2 C. N... 100644 100644 100644 aaa aaa C75 copy.rs\0orig.rs\0\
            u UU N... 100644 100644 100644 100644 a b c both.rs\0\
            ? notes.txt\0";
        let (head, commit, changes) = parse_status(output);
        assert_eq!(head, Head::Branch("main".into()));
        assert_eq!(commit.as_deref(), Some("0123456789abcdef"));
        let expected = [
            ("src/a b.rs", Kind::Modified, None, Staged::No),
            ("half.rs", Kind::Modified, None, Staged::Partly),
            ("new.rs", Kind::Added, None, Staged::Yes),
            ("gone.rs", Kind::Deleted, None, Staged::No),
            ("to.rs", Kind::Renamed, Some("from.rs"), Staged::Yes),
            ("copy.rs", Kind::Added, None, Staged::Yes),
            ("both.rs", Kind::Conflicted, None, Staged::No),
            ("notes.txt", Kind::Untracked, None, Staged::No),
        ];
        let expected: Vec<Entry> = expected
            .into_iter()
            .map(|(path, kind, from, staged)| {
                (PathBuf::from(path), kind, from.map(PathBuf::from), staged)
            })
            .collect();
        assert_eq!(changes, expected);
    }

    #[test]
    fn a_detached_head_is_its_short_hash() {
        let output = b"# branch.oid 0123456789abcdef\0# branch.head (detached)\0";
        assert_eq!(parse_status(output).0, Head::Detached("0123456".into()));
    }

    #[test]
    fn counts_lines_added_and_removed_but_not_in_binary_files() {
        let output = b"3\t1\tsrc/a.rs\n-\t-\timage.png\n0\t7\told.rs => new.rs\n";
        assert_eq!(parse_numstat(output), (3, 8));
    }

    #[test]
    fn a_repository_with_no_commits_counts_from_nothing() {
        let dir = fixture("initial");
        run(&dir, &["init", "-q"]);
        fs::write(dir.join("staged.txt"), "1\n2\n").unwrap();
        run(&dir, &["add", "."]);
        fs::write(dir.join("new.txt"), "1\n2\n3").unwrap();
        fs::write(dir.join("binary"), b"\0\n\n").unwrap();
        let mut git = Git::new(std::slice::from_ref(&dir));
        git.wait();
        let repo = &git.repos()[0];
        assert_eq!(repo.head, Head::Branch("main".into()));
        assert_eq!((repo.added, repo.removed), (5, 0));
    }

    #[test]
    fn reads_the_branch_and_changes_of_a_repository() {
        let dir = repo("status");
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::remove_file(dir.join("b.txt")).unwrap();
        fs::create_dir(dir.join("new")).unwrap();
        fs::write(dir.join("new/c.txt"), "c\n").unwrap();
        let mut git = Git::new(std::slice::from_ref(&dir));
        git.wait();
        let [repo] = git.repos() else {
            panic!("one repository: {:?}", git.repos());
        };
        assert_eq!(repo.root, dir);
        assert_eq!(repo.head, Head::Branch("main".into()));
        let changes: Vec<(PathBuf, Kind)> = repo
            .changes
            .iter()
            .map(|change| (change.path.clone(), change.kind))
            .collect();
        assert_eq!(
            changes,
            [
                (dir.join("a.txt"), Kind::Modified),
                (dir.join("b.txt"), Kind::Deleted),
                (dir.join("new/c.txt"), Kind::Untracked),
            ]
        );
        assert_eq!(
            git.repo_of(&dir.join("new/c.txt")).map(|r| &r.root),
            Some(&dir)
        );
        assert_eq!((repo.added, repo.removed), (2, 2));
    }

    #[test]
    fn reads_what_a_file_was_in_the_last_commit() {
        let dir = repo("base");
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        run(&dir, &["mv", "b.txt", "c.txt"]);
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        let mut git = Git::new(std::slice::from_ref(&dir));
        git.wait();
        let base = |name: &str| git.tracked(&dir.join(name)).map(|t| t.base().clone());
        assert_eq!(base("a.txt"), Some(Base::Text("a\n".into())));
        assert_eq!(base("c.txt"), Some(Base::Text("b\n".into())), "renamed");
        assert_eq!(base("new.txt"), Some(Base::Missing));
        let tracked = git.tracked(&dir.join("a.txt")).unwrap();
        assert_eq!(tracked.kind.get(), Some(Kind::Modified));
        assert!(git.tracked(Path::new("/elsewhere/a.txt")).is_none());
    }

    #[test]
    fn folders_in_one_repository_list_it_once() {
        let dir = repo("nested");
        fs::create_dir(dir.join("sub")).unwrap();
        let mut git = Git::new(&[dir.join("sub"), dir.clone()]);
        git.wait();
        assert_eq!(git.repos().len(), 1);
        assert_eq!(git.repos()[0].root, dir);
    }

    #[test]
    fn a_folder_outside_any_repository_has_none() {
        let dir = fixture("none");
        let mut git = Git::new(&[dir]);
        git.wait();
        assert!(git.repos().is_empty());
    }

    #[test]
    fn reads_the_log_what_each_commit_changed_and_files_in_it() {
        let dir = history_repo("history");
        let log = log(&dir, 0, 10);
        let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["third", "second", "first"]);
        assert_eq!(log[0].parent.as_ref(), Some(&log[1].hash));
        assert_eq!(log[2].parent, None);
        assert_eq!(log[0].author, "cue");
        assert!(log[0].hash.starts_with(&log[0].short));
        let page: Vec<String> = super::log(&dir, 1, 1)
            .into_iter()
            .map(|c| c.subject)
            .collect();
        assert_eq!(page, ["second"]);

        let changes = |commit: &Commit| -> Vec<(String, Kind, Option<String>)> {
            let name = |path: &Path| path.strip_prefix(&dir).unwrap().display().to_string();
            commit_changes(&dir, commit)
                .iter()
                .map(|c| (name(&c.path), c.kind, c.from.as_deref().map(name)))
                .collect()
        };
        assert_eq!(
            changes(&log[1]),
            [
                ("a.txt".to_string(), Kind::Modified, None),
                (
                    "c.txt".to_string(),
                    Kind::Renamed,
                    Some("b.txt".to_string())
                ),
                ("d.txt".to_string(), Kind::Added, None),
            ]
        );
        assert_eq!(
            changes(&log[0]),
            [("d.txt".to_string(), Kind::Deleted, None)]
        );
        assert_eq!(changes(&log[2]).len(), 2, "the first commit, from nothing");

        let text = |commit: &Commit, path: &str| file_at(&dir, &commit.hash, Path::new(path));
        assert_eq!(text(&log[1], "a.txt"), Base::Text("a\nmore\n".into()));
        assert_eq!(text(&log[2], "a.txt"), Base::Text("a\n".into()));
        assert_eq!(text(&log[0], "d.txt"), Base::Missing);
    }

    /// Sets the repository at `dir` up to commit as cue, unsigned and
    /// without hooks, whatever the user's git does.
    pub(crate) fn commit_as_cue(dir: &Path) {
        run(dir, &["config", "user.name", "cue"]);
        run(dir, &["config", "user.email", "cue@example.com"]);
        run(dir, &["config", "commit.gpgsign", "false"]);
        run(dir, &["config", "core.hooksPath", ".no-hooks"]);
    }

    /// What git says of each file changed in the repository at `dir`, by
    /// name: how much of it is staged.
    fn staged(dir: &Path) -> Vec<(String, Staged)> {
        let mut git = Git::new(&[dir.to_path_buf()]);
        git.wait();
        git.repos()[0]
            .changes
            .iter()
            .map(|change| {
                let name = change.path.strip_prefix(dir).unwrap();
                (name.display().to_string(), change.staged)
            })
            .collect()
    }

    #[test]
    fn stages_unstages_and_commits() {
        let dir = repo("stage");
        commit_as_cue(&dir);
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        fs::remove_file(dir.join("b.txt")).unwrap();
        fs::write(dir.join("[new].txt"), "new\n").unwrap();
        fs::write(dir.join("n.txt"), "not this one\n").unwrap();
        let paths = ["a.txt", "b.txt", "[new].txt"].map(|name| dir.join(name));
        stage(&dir, &paths).unwrap();
        fs::write(dir.join("a.txt"), "changed again\n").unwrap();
        let s = |name: &str, staged| (name.to_string(), staged);
        assert_eq!(
            staged(&dir),
            [
                s("[new].txt", Staged::Yes),
                s("a.txt", Staged::Partly),
                s("b.txt", Staged::Yes),
                s("n.txt", Staged::No),
            ],
            "taken as they're named, not as patterns"
        );
        unstage(&dir, &paths[..1], false).unwrap();
        assert_eq!(staged(&dir)[1], s("a.txt", Staged::No));

        let hash = commit(&dir, "Remove b\n\nAnd add [new].", false).unwrap();
        let last = super::log(&dir, 0, 1).remove(0);
        assert_eq!((last.short, last.subject.as_str()), (hash, "Remove b"));
        assert_eq!(
            last_message(&dir).as_deref(),
            Some("Remove b\n\nAnd add [new].")
        );
        assert_eq!(
            staged(&dir),
            [s("a.txt", Staged::No), s("n.txt", Staged::No)]
        );

        stage(&dir, &[dir.join("a.txt")]).unwrap();
        commit(&dir, "Remove b, change a", true).unwrap();
        let log = super::log(&dir, 0, 10);
        let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["Remove b, change a", "first"], "amended");

        let err = commit(&dir, "Nothing", false).unwrap_err();
        assert!(err.contains("nothing added to commit"), "{err}");
    }

    #[test]
    fn unstages_before_the_first_commit() {
        let dir = fixture("stage-initial");
        run(&dir, &["init", "-q"]);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        stage(&dir, &[dir.join("a.txt")]).unwrap();
        assert_eq!(staged(&dir), [("a.txt".to_string(), Staged::Yes)]);
        unstage(&dir, &[dir.join("a.txt")], true).unwrap();
        assert_eq!(staged(&dir), [("a.txt".to_string(), Staged::No)]);
        assert!(last_message(&dir).is_none());
    }

    #[test]
    fn stashes_applies_and_drops() {
        let dir = repo("stash");
        commit_as_cue(&dir);
        fs::write(dir.join("a.txt"), "staged\n").unwrap();
        fs::write(dir.join("b.txt"), "not staged\n").unwrap();
        stage(&dir, &[dir.join("a.txt")]).unwrap();
        stash(&dir, "  just a  ", true).unwrap();
        assert_eq!(
            staged(&dir),
            [("b.txt".to_string(), Staged::No)],
            "only what was staged"
        );
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        stash(&dir, "", false).unwrap();
        assert!(staged(&dir).is_empty(), "everything, new files too");

        let list = stashes(&dir);
        let subjects: Vec<&str> = list.iter().map(|s| s.commit.subject.as_str()).collect();
        assert!(subjects[0].starts_with("WIP on main: "), "{subjects:?}");
        assert_eq!(subjects[1], "just a");
        assert!(list[0].untracked.is_some() && list[1].untracked.is_none());
        let files = |stash: &Stash| -> Vec<(String, Kind)> {
            stash_changes(&dir, stash)
                .into_iter()
                .map(|(commit, change)| {
                    let name = change.path.strip_prefix(&dir).unwrap().display();
                    let text = match file_at(&dir, &commit.hash, Path::new(&name.to_string())) {
                        Base::Text(text) => text,
                        _ => String::new(),
                    };
                    (format!("{name}: {}", text.trim()), change.kind)
                })
                .collect()
        };
        assert_eq!(
            files(&list[0]),
            [
                ("b.txt: not staged".to_string(), Kind::Modified),
                ("new.txt: new".to_string(), Kind::Added),
            ]
        );

        apply_stash(&dir, &list[0].commit.hash, true).unwrap();
        assert_eq!(stashes(&dir).len(), 1, "popped");
        assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "new\n");
        // a.txt changed again, so the other doesn't go back cleanly.
        fs::write(dir.join("a.txt"), "in the way\n").unwrap();
        stage(&dir, &[dir.join("a.txt")]).unwrap();
        commit(&dir, "In the way", false).unwrap();
        let err = apply_stash(&dir, &list[1].commit.hash, false).unwrap_err();
        assert!(err.contains("CONFLICT"), "{err}");
        drop_stash(&dir, &list[1].commit.hash).unwrap();
        assert!(stashes(&dir).is_empty());
        assert_eq!(
            drop_stash(&dir, &list[1].commit.hash).unwrap_err(),
            "The stash is gone."
        );

        run(&dir, &["reset", "-q", "--hard"]);
        let mut git = Git::new(std::slice::from_ref(&dir));
        git.wait();
        assert!(git.repos()[0].stashes.is_empty());
        stash(&dir, "again", false).unwrap();
        git.restart();
        git.wait();
        assert_eq!(git.repos()[0].stashes, stashes(&dir));
    }

    #[test]
    fn switching_preserves_ignored_files_and_existing_stashes() {
        let dir = repo("switch-ignored");
        commit_as_cue(&dir);
        fs::write(dir.join(".gitignore"), "config.local\n").unwrap();
        commit_all(&dir, "ignore local config");
        run(&dir, &["switch", "-q", "-c", "other"]);
        fs::write(dir.join("config.local"), "other branch\n").unwrap();
        run(&dir, &["add", "-f", "config.local"]);
        commit_all(&dir, "track config");
        run(&dir, &["switch", "-q", "main"]);

        fs::write(dir.join("a.txt"), "older stash\n").unwrap();
        stash(&dir, "keep this stash", false).unwrap();
        let before = stashes(&dir);
        fs::write(dir.join("config.local"), "local edits\n").unwrap();
        // With only ignored changes, stash push succeeds without creating
        // a stash. With other changes, those must be put back on failure.
        for dirty in [false, true] {
            if dirty {
                fs::write(dir.join("b.txt"), "staged\n").unwrap();
                stage(&dir, &[dir.join("b.txt")]).unwrap();
                fs::write(dir.join("b.txt"), "unstaged\n").unwrap();
                fs::write(dir.join("new.txt"), "new\n").unwrap();
            }
            let changes = staged(&dir);
            let err = switch(&dir, &SwitchTo::Branch("other".into())).unwrap_err();
            assert!(err.contains("config.local"), "{err}");
            assert_eq!(
                fs::read_to_string(dir.join("config.local")).unwrap(),
                "local edits\n"
            );
            assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
            assert_eq!(stashes(&dir), before, "an older stash is never popped");
            assert_eq!(staged(&dir), changes);
            let head = git(&dir).args(["branch", "--show-current"]).output().unwrap();
            assert_eq!(head.stdout, b"main\n");
            if dirty {
                assert_eq!(
                    fs::read_to_string(dir.join("b.txt")).unwrap(),
                    "unstaged\n"
                );
                assert_eq!(
                    file_at(&dir, "", Path::new("b.txt")),
                    Base::Text("staged\n".into())
                );
                assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "new\n");
            }
        }
    }

    #[test]
    fn switches_to_branches_that_share_names_with_tags() {
        let dir = repo("switch-tags");
        run(&dir, &["tag", "main"]);
        run(&dir, &["remote", "add", "origin", "."]);
        run(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run(&dir, &["update-ref", "refs/remotes/origin/topic", "HEAD"]);
        run(&dir, &["tag", "origin/topic"]);
        run(&dir, &["switch", "-q", "-c", "other"]);
        let listed = branches(&dir);
        assert!(!listed.iter().any(|branch| branch.name == "origin/main"));
        for (name, remote) in [("main", false), ("origin/topic", true)] {
            let branch = listed.iter().find(|branch| branch.name == name).unwrap();
            assert_eq!(branch.remote, remote);
            let to = if remote {
                SwitchTo::Track(branch.name.clone())
            } else {
                SwitchTo::Branch(branch.name.clone())
            };
            assert_eq!(switch(&dir, &to), Ok(Carried::Along));
        }
        let upstream = git(&dir)
            .args(["rev-parse", "--symbolic-full-name", "@{upstream}"])
            .output()
            .unwrap();
        assert_eq!(upstream.stdout, b"refs/remotes/origin/topic\n");
    }

    #[test]
    fn lists_branches_and_switches_bringing_changes() {
        let dir = repo("switch");
        commit_as_cue(&dir);
        fs::write(dir.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        commit_all(&dir, "lines");
        run(&dir, &["switch", "-q", "-c", "other"]);
        fs::write(dir.join("a.txt"), "one\n2\n3\n4\n5\n").unwrap();
        commit_all(&dir, "other");
        run(&dir, &["switch", "-q", "main"]);
        run(&dir, &["remote", "add", "origin", "."]);
        run(&dir, &["update-ref", "refs/remotes/origin/main", "main"]);
        run(&dir, &["update-ref", "refs/remotes/origin/feature", "main"]);
        let head = [
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ];
        run(&dir, &head);

        let mut listed: Vec<(String, bool, bool)> = branches(&dir)
            .into_iter()
            .map(|branch| (branch.name, branch.remote, branch.current))
            .collect();
        listed.sort();
        let branch = |name: &str, remote, current| (name.to_string(), remote, current);
        assert_eq!(
            listed,
            [
                branch("main", false, true),
                branch("origin/feature", true, false),
                branch("other", false, false),
            ],
            "a remote's branch only if there's none here by its name"
        );

        let head = |dir: &Path| {
            let output = git(dir)
                .args(["branch", "--show-current"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let a = |dir: &Path| fs::read_to_string(dir.join("a.txt")).unwrap();
        // A change git switches with.
        fs::write(dir.join("b.txt"), "changed\n").unwrap();
        let to = |name: &str| SwitchTo::Branch(name.to_string());
        assert_eq!(switch(&dir, &to("other")), Ok(Carried::Along));
        assert_eq!(head(&dir), "other");
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "changed\n");
        assert_eq!(switch(&dir, &to("main")), Ok(Carried::Along));

        // One git won't switch with, but that goes back cleanly.
        stage(&dir, &[dir.join("b.txt")]).unwrap();
        fs::write(dir.join("b.txt"), "changed again\n").unwrap();
        fs::write(dir.join("a.txt"), "1\n2\n3\n4\nfive\n").unwrap();
        assert_eq!(switch(&dir, &to("other")), Ok(Carried::Stashed));
        assert_eq!(a(&dir), "one\n2\n3\n4\nfive\n");
        assert_eq!(
            fs::read_to_string(dir.join("b.txt")).unwrap(),
            "changed again\n"
        );
        assert_eq!(
            staged(&dir),
            [
                ("a.txt".to_string(), Staged::No),
                ("b.txt".to_string(), Staged::Partly),
            ],
            "the staged and unstaged changes stay separate"
        );
        assert!(stashes(&dir).is_empty());
        run(&dir, &["reset", "-q", "--hard"]);
        run(&dir, &["switch", "-q", "main"]);

        // And one that doesn't: it's kept in a stash.
        fs::write(dir.join("a.txt"), "uno\n2\n3\n4\n5\n").unwrap();
        let Ok(Carried::Conflicts(err)) = switch(&dir, &to("other")) else {
            panic!("conflicts");
        };
        assert!(!err.is_empty());
        assert!(a(&dir).contains("<<<<<<<"), "{err}");
        assert_eq!(head(&dir), "other");
        assert_eq!(stashes(&dir)[0].commit.subject, "Switching to other");
        run(&dir, &["reset", "-q", "--hard"]);

        let new = SwitchTo::New("fresh".to_string());
        assert_eq!(switch(&dir, &new), Ok(Carried::Along));
        assert_eq!(head(&dir), "fresh");
        let track = SwitchTo::Track("origin/feature".to_string());
        assert_eq!(track.name(), "feature");
        assert_eq!(switch(&dir, &track), Ok(Carried::Along));
        assert_eq!(head(&dir), "feature");
        let err = switch(&dir, &to("nowhere")).unwrap_err();
        assert!(err.contains("nowhere"), "{err}");
    }

    #[test]
    fn a_repository_without_commits_has_no_log() {
        let dir = fixture("no-commits");
        run(&dir, &["init", "-q"]);
        assert!(log(&dir, 0, 10).is_empty());
    }
}
