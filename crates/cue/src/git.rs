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

/// A file changed since the last commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Where it is, or was, if it's gone.
    pub path: PathBuf,
    pub kind: Kind,
    /// Where it was, if it was renamed.
    pub from: Option<PathBuf>,
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
            let spec = format!("{commit}:{}", self.old_path.to_string_lossy());
            let output = git(&self.root).args(["cat-file", "blob", &spec]).output();
            match output {
                Ok(output) if output.status.success() => base_text(output.stdout),
                _ => Base::Missing,
            }
        })
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
    let (mut added, removed) = diff_lines(top, commit.is_none());
    let untracked = changes
        .iter()
        .filter(|(_, kind, _)| *kind == Kind::Untracked)
        .map(|(path, ..)| top.join(path));
    added += count_lines(untracked);
    let mut changes: Vec<Change> = changes
        .into_iter()
        .take(MAX_CHANGES)
        .map(|(path, kind, from)| Change {
            path: top.join(path),
            kind,
            from: from.map(|from| top.join(from)),
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

/// A changed file's path in the working tree, how it changed, and if it
/// was renamed, its path before.
type Entry = (PathBuf, Kind, Option<PathBuf>);

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
            "?" => Some((rest.to_string(), Kind::Untracked, None)),
            // XY sub mH mI mW hH hI path
            "1" => rest
                .splitn(8, ' ')
                .nth(7)
                .map(|path| (path.to_string(), ordinary(rest), None)),
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
                path.map(|path| (path, kind, from))
            }
            // XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => rest
                .splitn(10, ' ')
                .nth(9)
                .map(|path| (path.to_string(), Kind::Conflicted, None)),
            _ => None,
        };
        if let Some((path, kind, from)) = change {
            changes.push((PathBuf::from(path), kind, from.map(PathBuf::from)));
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
            ("src/a b.rs", Kind::Modified, None),
            ("new.rs", Kind::Added, None),
            ("gone.rs", Kind::Deleted, None),
            ("to.rs", Kind::Renamed, Some("from.rs")),
            ("copy.rs", Kind::Added, None),
            ("both.rs", Kind::Conflicted, None),
            ("notes.txt", Kind::Untracked, None),
        ];
        let expected: Vec<Entry> = expected
            .into_iter()
            .map(|(path, kind, from)| (PathBuf::from(path), kind, from.map(PathBuf::from)))
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
}
