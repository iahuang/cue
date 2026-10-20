//! Checkpoints: snapshots of the workspace's folders, files git doesn't
//! know of yet included, to see what changed since: before an agent's run,
//! say.
//!
//! Each workspace folder has a store of its own: a bare git repository in
//! `$XDG_STATE_HOME/cue/checkpoints`, or `~/.local/state/cue/checkpoints`,
//! named for the folder's path, with the folder's files as its working
//! tree. So the folder's own repository is never written to, a folder
//! needn't be in one, and a folder's checkpoints stay with it whatever
//! workspace it's opened in. The store keeps an index of its own, which
//! knows the files it read last time, so a snapshot only reads those
//! changed since. A folder in a repository is read from the repository's
//! top folder, so the repository's ignore files all apply, and its
//! `info/exclude` is copied over.
//!
//! A checkpoint of the workspace is a mark: snapshots of each folder, taken
//! together, under the same time and name. Comparing with a mark, each
//! folder compares with its own snapshot under that mark, or if it has none
//! (it wasn't open then, or the snapshot was dropped), the latest before
//! it; failing that, with the last commit, as if there were no mark. Each
//! snapshot is a tree, kept by a ref named for its mark:
//! `refs/marks/1760000000000-before%20refactor`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::git::{self, Base, Change};

/// A folder's snapshots past this many are dropped, oldest first.
pub const MAX_MARKS: usize = 20;
/// Where a store keeps its snapshots.
const MARKS: &str = "refs/marks/";

/// A checkpoint of the workspace, as its folders' snapshots are known by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// When it was taken, in milliseconds since the Unix epoch.
    pub id: i64,
    pub name: Option<String>,
}

/// What changed since a mark, in the folders that have a snapshot from
/// then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Since {
    pub mark: Mark,
    pub roots: Vec<RootSince>,
}

/// What changed in a folder since its snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSince {
    pub root: PathBuf,
    /// Its store.
    pub store: PathBuf,
    /// The folder the snapshot's paths are from: the top of the repository
    /// the folder's in, or the folder.
    pub top: PathBuf,
    /// The snapshot, by hash.
    pub tree: String,
    /// Sorted by path.
    pub changes: Vec<Change>,
}

impl Since {
    /// What changed in the folder `path` is in, if it has a snapshot: the
    /// deepest folder, should they nest.
    pub fn root_of(&self, path: &Path) -> Option<&RootSince> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.root))
            .max_by_key(|root| root.root.components().count())
    }
}

/// Stores are written to one snapshot at a time, from any thread: git
/// refuses to add to an index another git is adding to.
static WRITING: Mutex<()> = Mutex::new(());

/// A folder's store.
struct Store {
    root: PathBuf,
    dir: PathBuf,
    top: PathBuf,
}

impl Store {
    /// The store of the folder `root`, if it has one yet.
    fn find(root: &Path) -> Option<Store> {
        let dir = store_dir(root)?;
        if !dir.join("HEAD").is_file() {
            return None;
        }
        let top = git::locate(root).map_or_else(|| root.to_path_buf(), |(top, _)| top);
        Some(Store {
            root: root.to_path_buf(),
            dir,
            top,
        })
    }

    /// The store of the folder `root`, made if it has none.
    fn open(root: &Path) -> Result<Store, String> {
        if let Some(store) = Store::find(root) {
            store.copy_excludes();
            return Ok(store);
        }
        let dir = store_dir(root).ok_or("Failed to locate checkpoints folder.")?;
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let mut init = Command::new("git");
        init.args(["init", "--bare", "-q"]).arg(&dir);
        run(init)?;
        // For whoever wonders what it's of.
        let _ = std::fs::write(dir.join("cue-root"), root.as_os_str().as_encoded_bytes());
        let store = Store::find(root).ok_or("Failed to create checkpoints store.")?;
        store.copy_excludes();
        Ok(store)
    }

    /// Copies the ignore rules of the repository the folder's in that are
    /// kept in it rather than its files: its `info/exclude`.
    fn copy_excludes(&self) {
        let Some((_, git_dir)) = git::locate(&self.root) else {
            return;
        };
        let common = git_dir
            .parent()
            .filter(|parent| parent.ends_with("worktrees"))
            .and_then(Path::parent)
            .unwrap_or(&git_dir);
        let exclude = std::fs::read(common.join("info/exclude")).unwrap_or_default();
        let ours = self.dir.join("info/exclude");
        if std::fs::read(&ours).ok().as_ref() != Some(&exclude) {
            let _ = std::fs::create_dir_all(self.dir.join("info"));
            let _ = std::fs::write(ours, exclude);
        }
    }

    /// `git`, with the store as its repository and the folder's files as
    /// its working tree.
    fn git(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.dir)
            .arg("--work-tree")
            .arg(&self.top)
            .arg("-C")
            .arg(&self.top)
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// Saves the folder's files as they are, but for those in `others`,
    /// workspace folders inside it, which have snapshots of their own.
    /// Returns the tree's hash.
    fn snapshot(&self, others: &[PathBuf]) -> Result<String, String> {
        let mut spec = vec![match self.root.strip_prefix(&self.top) {
            Ok(rest) if !rest.as_os_str().is_empty() => literal("", rest),
            _ => ".".to_string(),
        }];
        for other in others {
            if other != &self.root && other.starts_with(&self.root) {
                if let Ok(rest) = other.strip_prefix(&self.top) {
                    spec.push(literal("exclude,", rest));
                }
            }
        }
        let _writing = WRITING.lock().unwrap_or_else(|err| err.into_inner());
        let mut add = self.git();
        add.args(["add", "-A", "--"]).args(&spec);
        run(add)?;
        let mut write = self.git();
        write.arg("write-tree");
        run(write)
    }

    /// Its snapshots, by the marks they're under, and their trees, newest
    /// first.
    fn marks(&self) -> Vec<(Mark, String)> {
        let mut command = self.git();
        command.args(["for-each-ref", "--format=%(refname)%00%(objectname)", MARKS]);
        let Ok(output) = run(command) else {
            return Vec::new();
        };
        let mut marks: Vec<(Mark, String)> = output
            .lines()
            .filter_map(|line| {
                let (refname, tree) = line.split_once('\0')?;
                let mark = parse_mark(refname.strip_prefix(MARKS)?)?;
                Some((mark, tree.to_string()))
            })
            .collect();
        marks.sort_by(|a, b| b.0.id.cmp(&a.0.id));
        marks
    }

    /// Takes a snapshot under `mark`, and drops the oldest past
    /// [`MAX_MARKS`].
    fn mark(&self, mark: &Mark, others: &[PathBuf]) -> Result<(), String> {
        let tree = self.snapshot(others)?;
        let mut update = self.git();
        update.args(["update-ref", &mark_ref(mark), &tree]);
        run(update)?;
        let old: Vec<String> = self
            .marks()
            .iter()
            .skip(MAX_MARKS)
            .map(|(mark, _)| format!("delete {}\n", mark_ref(mark)))
            .collect();
        if !old.is_empty() {
            let mut command = self.git();
            command.args(["update-ref", "--stdin"]);
            // The snapshot's taken even if they stay.
            let _ = run_with_input(command, old.concat().as_bytes());
        }
        // Snapshots of a folder an agent's changing pile up; git packs
        // them when there are enough.
        let mut gc = self.git();
        gc.args(["gc", "--auto", "--quiet"]);
        let _ = run(gc);
        Ok(())
    }

    /// The files changed since the snapshot `tree`, sorted by path, with
    /// renames found as `git status` finds them.
    fn changes_since(&self, tree: &str, others: &[PathBuf]) -> Result<Vec<Change>, String> {
        let now = self.snapshot(others)?;
        let mut diff = self.git();
        diff.args(["diff-tree", "-r", "-M", "-z", "--name-status", tree, &now]);
        let output = diff.output().map_err(|err| err.to_string())?;
        if !output.status.success() {
            return Err("git diff-tree failed.".to_string());
        }
        let mut changes = git::parse_name_status(&self.top, &output.stdout);
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes.truncate(git::MAX_CHANGES);
        Ok(changes)
    }
}

/// Takes `mark` of each of `roots`.
pub fn take(roots: &[PathBuf], mark: &Mark) -> Result<(), String> {
    for root in roots {
        Store::open(root)?.mark(mark, roots)?;
    }
    Ok(())
}

/// The marks `roots` have snapshots under, each once, newest first.
pub fn marks(roots: &[PathBuf]) -> Vec<Mark> {
    let mut marks: Vec<Mark> = Vec::new();
    for store in roots.iter().filter_map(|root| Store::find(root)) {
        for (mark, _) in store.marks() {
            match marks.iter_mut().find(|known| known.id == mark.id) {
                // A folder may have lost the name to a failed rename; any
                // folder's will do.
                Some(known) if known.name.is_none() => known.name = mark.name,
                Some(_) => {}
                None => marks.push(mark),
            }
        }
    }
    marks.sort_by(|a, b| b.id.cmp(&a.id));
    marks
}

/// What changed in `roots` since `mark`: in each, since its snapshot
/// under it or the latest before.
pub fn since(roots: &[PathBuf], mark: &Mark) -> Since {
    let roots_since = roots
        .iter()
        .filter_map(|root| {
            let store = Store::find(root)?;
            let (_, tree) = store
                .marks()
                .into_iter()
                .find(|(taken, _)| taken.id <= mark.id)?;
            let changes = store.changes_since(&tree, roots).ok()?;
            Some(RootSince {
                root: root.clone(),
                store: store.dir,
                top: store.top,
                tree,
                changes,
            })
        })
        .collect();
    Since {
        mark: mark.clone(),
        roots: roots_since,
    }
}

/// What the file at `path`, from `top`, was in the snapshot `tree` in
/// `store`.
pub fn file_at(store: &Path, tree: &str, path: &Path) -> Base {
    let spec = format!("{tree}:{}", path.to_string_lossy());
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(store)
        .args(["cat-file", "blob", &spec])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => git::base_text(output.stdout),
        _ => Base::Missing,
    }
}

/// How `mark` is named in lists: by its name, in quotes, or without one,
/// by when it was taken, as `Oct 2 14:03`.
pub fn label(mark: &Mark) -> String {
    match &mark.name {
        Some(name) => format!("“{name}”"),
        None => crate::terminal::local_time((mark.id / 1000) as u64),
    }
}

/// Where the store of the folder `root` is.
fn store_dir(root: &Path) -> Option<PathBuf> {
    let name = format!("{:016x}.git", fnv1a(root.as_os_str().as_encoded_bytes()));
    Some(stores()?.join(name))
}

/// The folder the stores are in.
#[cfg(not(test))]
fn stores() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
    Some(state.join("cue/checkpoints"))
}

/// In tests, a folder of their own, out of the way of the user's.
#[cfg(test)]
fn stores() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("cue-checkpoints-{}", std::process::id())))
}

/// The 64-bit FNV-1a hash of `bytes`: the same in every build, as std's
/// hashers aren't promised to be.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    })
}

/// A pathspec of `path` as it's written, with `magic` (`exclude,`, say).
fn literal(magic: &str, path: &Path) -> String {
    format!(":({magic}literal){}", path.to_string_lossy())
}

/// The ref a snapshot under `mark` is kept by.
fn mark_ref(mark: &Mark) -> String {
    match &mark.name {
        Some(name) => format!("{MARKS}{}-{}", mark.id, escape_ref(name)),
        None => format!("{MARKS}{}", mark.id),
    }
}

/// A mark from the last part of its ref: `1760000000000-before%20refactor`,
/// or without a name, `1760000000000`.
fn parse_mark(last: &str) -> Option<Mark> {
    let (id, name) = match last.split_once('-') {
        Some((id, name)) => (id, Some(unescape_ref(name))),
        None => (last, None),
    };
    Some(Mark {
        id: id.parse().ok()?,
        name,
    })
}

/// `name` as it can go in a ref: but for letters, digits, `-` and `_`,
/// each byte as `%` and its hex, as in a URL.
fn escape_ref(name: &str) -> String {
    let mut escaped = String::new();
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => escaped.push(byte as char),
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

/// What [`escape_ref`] escaped.
fn unescape_ref(escaped: &str) -> String {
    let bytes = escaped.as_bytes();
    let mut name = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                name.push(byte);
                index += 3;
            }
            (byte, _) => {
                name.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&name).into_owned()
}

/// Runs `command`, and returns what it printed, or if it failed, what it
/// said.
fn run(command: Command) -> Result<String, String> {
    run_with_input(command, b"")
}

fn run_with_input(mut command: Command, input: &[u8]) -> Result<String, String> {
    use std::io::Write;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Can't run git: {err}"))?;
    if let Some(mut stdin) = child.stdin.take() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Kind;
    use std::fs;

    fn mark(id: i64, name: Option<&str>) -> Mark {
        Mark {
            id,
            name: name.map(str::to_string),
        }
    }

    /// What changed in each of `roots` since `mark`, by path from it.
    fn changed(roots: &[PathBuf], mark: &Mark) -> Vec<(String, Kind)> {
        since(roots, mark)
            .roots
            .iter()
            .flat_map(|root| {
                root.changes.iter().map(|change| {
                    let path = change.path.strip_prefix(&root.root).unwrap();
                    (path.display().to_string(), change.kind)
                })
            })
            .collect()
    }

    /// What `git` prints in `dir`.
    fn output(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn lists_what_changed_since_without_touching_the_repository() {
        let dir = crate::git::tests::repo("checkpoint");
        fs::write(dir.join("a.txt"), "staged\n").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["add", "a.txt"])
            .status()
            .unwrap()
            .success());
        fs::write(dir.join("notes.txt"), "notes\n").unwrap();
        fs::write(dir.join(".git/info/exclude"), "secret.txt\n").unwrap();
        fs::write(dir.join("secret.txt"), "shh\n").unwrap();
        let before = (
            output(&dir, &["status", "--porcelain"]),
            output(&dir, &["for-each-ref"]),
        );
        let roots = [dir.clone()];

        take(&roots, &mark(1000, Some("before"))).unwrap();
        let after = (
            output(&dir, &["status", "--porcelain"]),
            output(&dir, &["for-each-ref"]),
        );
        assert_eq!(
            after, before,
            "the repository's index and refs are as they were"
        );
        assert_eq!(marks(&roots), [mark(1000, Some("before"))]);
        assert!(changed(&roots, &mark(1000, None)).is_empty());

        fs::write(dir.join("b.txt"), "changed\n").unwrap();
        fs::remove_file(dir.join("notes.txt")).unwrap();
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        fs::write(dir.join("secret.txt"), "still excluded\n").unwrap();
        assert_eq!(
            changed(&roots, &mark(1000, None)),
            [
                ("b.txt".to_string(), Kind::Modified),
                ("new.txt".to_string(), Kind::Added),
                ("notes.txt".to_string(), Kind::Deleted),
            ],
            "a.txt changed before, and the repository excludes secret.txt"
        );
        let root = &since(&roots, &mark(1000, None)).roots[0];
        assert_eq!(
            file_at(&root.store, &root.tree, Path::new("notes.txt")),
            Base::Text("notes\n".into())
        );
    }

    #[test]
    fn folders_in_no_repository_and_inside_others_have_their_own() {
        let dir = std::env::temp_dir().join(format!("cue-checkpoint-plain-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("inner")).unwrap();
        let dir = dir.canonicalize().unwrap();
        fs::write(dir.join("a.md"), "a\n").unwrap();
        fs::write(dir.join("inner/b.md"), "b\n").unwrap();
        let inner = dir.join("inner");
        let roots = [dir.clone(), inner.clone()];
        take(&roots, &mark(1, None)).unwrap();

        fs::write(dir.join("a.md"), "changed\n").unwrap();
        fs::write(inner.join("b.md"), "changed\n").unwrap();
        let since = since(&roots, &mark(1, None));
        let outer = &since.roots[0].changes;
        assert_eq!(outer.len(), 1, "the folder inside is left to its own");
        assert_eq!(outer[0].path, dir.join("a.md"));
        assert_eq!(since.root_of(&inner.join("b.md")).unwrap().root, inner);
        assert_eq!(since.roots[1].changes[0].path, inner.join("b.md"));
    }

    #[test]
    fn folders_compare_with_their_latest_snapshot_from_then() {
        let one = crate::git::tests::repo("checkpoint-one");
        let two = crate::git::tests::repo("checkpoint-two");
        take(std::slice::from_ref(&one), &mark(1, Some("first"))).unwrap();
        fs::write(one.join("a.txt"), "between\n").unwrap();
        // The second folder wasn't open for the first.
        take(&[one.clone(), two.clone()], &mark(2, None)).unwrap();
        let both = [one.clone(), two.clone()];
        assert_eq!(marks(&both), [mark(2, None), mark(1, Some("first"))]);

        let first = since(&both, &mark(1, Some("first")));
        assert_eq!(first.roots.len(), 1, "the second has none from then");
        assert_eq!(first.roots[0].changes.len(), 1);
        assert!(since(&both, &mark(2, None))
            .roots
            .iter()
            .all(|root| root.changes.is_empty()));
    }

    #[test]
    fn keeps_the_newest_snapshots() {
        let dir = crate::git::tests::repo("checkpoint-many");
        let roots = [dir];
        for id in 1..=MAX_MARKS as i64 + 2 {
            take(&roots, &mark(id, None)).unwrap();
        }
        let ids: Vec<i64> = marks(&roots).iter().map(|mark| mark.id).collect();
        assert_eq!(ids, (3..=MAX_MARKS as i64 + 2).rev().collect::<Vec<_>>());
    }

    #[test]
    fn names_marks_in_their_refs() {
        let name = "before the refactor: ünïcode 100% / done.";
        assert_eq!(unescape_ref(&escape_ref(name)), name);
        assert!(escape_ref(name)
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_%".contains(c)));
        assert_eq!(parse_mark("12-a-b"), Some(mark(12, Some("a-b"))));
        assert_eq!(parse_mark("12"), Some(mark(12, None)));
        assert_eq!(parse_mark("x"), None);
        assert_eq!(mark_ref(&mark(12, Some("a b"))), "refs/marks/12-a%20b");
    }
}
