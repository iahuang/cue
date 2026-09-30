//! The folders cue is working in. The file tree shows them, and the file
//! picker lists their files.
//!
//! Roots may nest, as when a package of a monorepo is added beside the
//! repository: a file belongs to the deepest root it's in.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Workspace {
    /// Absolute, symlink-free paths, in the order they were added.
    roots: Vec<PathBuf>,
    /// What each root is called, by [`root_names`].
    names: Vec<String>,
}

impl Workspace {
    /// A workspace of the directories in `roots`.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> io::Result<Workspace> {
        let mut workspace = Workspace {
            roots: Vec::new(),
            names: Vec::new(),
        };
        for root in roots {
            workspace.add_root(&root)?;
        }
        Ok(workspace)
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Adds the directory `path`, unless it is already a root.
    pub fn add_root(&mut self, path: &Path) -> io::Result<()> {
        let root = path.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", path.display()),
            ));
        }
        if !self.roots.contains(&root) {
            self.roots.push(root);
            self.names = root_names(&self.roots);
        }
        Ok(())
    }

    /// Removes the root `root`. Returns false if it isn't one.
    pub fn remove_root(&mut self, root: &Path) -> bool {
        let count = self.roots.len();
        self.roots.retain(|other| other != root);
        self.names = root_names(&self.roots);
        self.roots.len() < count
    }

    /// The deepest root containing `path`, if any. Roots may nest.
    pub fn root_of(&self, path: &Path) -> Option<&Path> {
        deepest_root(&self.roots, path)
    }

    /// How to show `path` to the user: relative to its root (prefixed with
    /// the root's name when there are several roots), or in full if it is
    /// outside the workspace.
    pub fn display_path(&self, path: &Path) -> String {
        let Some(root) = self.root_of(path) else {
            return path.display().to_string();
        };
        let relative = path.strip_prefix(root).unwrap_or(path);
        if self.roots.len() == 1 {
            return relative.display().to_string();
        }
        let name = self.name(root).unwrap_or_default();
        Path::new(name).join(relative).display().to_string()
    }

    /// What the root `root` is called, if it is one.
    pub fn name(&self, root: &Path) -> Option<&str> {
        let index = self.roots.iter().position(|other| other == root)?;
        Some(&self.names[index])
    }
}

/// The deepest of `roots` containing `path`, if any.
pub fn deepest_root<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .map(PathBuf::as_path)
}

/// The names shown for `roots`, in order: each its last component (the
/// whole path for `/`), with as many of the folders above it as it takes to
/// tell it from the others, as `api/src` and `web/src`.
pub fn root_names(roots: &[PathBuf]) -> Vec<String> {
    // Each root's last `n` components, or its whole path if it has fewer.
    let tail = |root: &Path, n: usize| {
        let names: Vec<_> = root.iter().skip(root.has_root() as usize).collect();
        if n > names.len() {
            return root.display().to_string();
        }
        let tail: PathBuf = names[names.len() - n..].iter().collect();
        tail.display().to_string()
    };
    roots
        .iter()
        .map(|root| {
            let mut n = 1;
            loop {
                let name = tail(root, n);
                let taken = roots
                    .iter()
                    .any(|other| other != root && tail(other, n) == name);
                // A whole path is told apart from the others already.
                if !taken || name == root.display().to_string() {
                    return name;
                }
                n += 1;
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-workspace-{}", std::process::id()))
            .join(name);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn roots_are_canonical_and_unique() {
        let dir = temp_dir("unique");
        let dotted = dir.join("..").join("unique");
        let workspace = Workspace::new([dir.clone(), dotted]).unwrap();
        assert_eq!(workspace.roots(), std::slice::from_ref(&dir));
        assert!(Workspace::new([dir.join("missing")]).is_err());
    }

    #[test]
    fn display_paths_are_relative_to_the_deepest_root() {
        let outer = temp_dir("outer");
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        let file = inner.join("a.rs");

        let single = Workspace::new([outer.clone()]).unwrap();
        assert_eq!(single.display_path(&file), "inner/a.rs");
        assert_eq!(
            single.display_path(Path::new("/elsewhere/b.rs")),
            "/elsewhere/b.rs"
        );

        let nested = Workspace::new([outer.clone(), inner.clone()]).unwrap();
        assert_eq!(nested.root_of(&file), Some(inner.as_path()));
        assert_eq!(nested.display_path(&file), "inner/a.rs");
        assert_eq!(nested.display_path(&outer.join("c.rs")), "outer/c.rs");
    }

    #[test]
    fn roots_with_the_same_name_are_told_apart() {
        let roots = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            root_names(&roots(&["/w/api/src", "/w/web/src", "/w/docs"])),
            ["api/src", "web/src", "docs"]
        );
        assert_eq!(root_names(&roots(&["/src", "/w/src"])), ["/src", "w/src"]);
        assert_eq!(root_names(&roots(&["/"])), ["/"]);

        let dir = temp_dir("names");
        let (a, b) = (dir.join("a/lib"), dir.join("b/lib"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let mut workspace = Workspace::new([a.clone(), b.clone()]).unwrap();
        assert_eq!(workspace.display_path(&a.join("x.rs")), "a/lib/x.rs");
        assert!(workspace.remove_root(&b));
        assert!(!workspace.remove_root(&b));
        assert_eq!(workspace.roots(), std::slice::from_ref(&a));
        assert_eq!(workspace.display_path(&a.join("x.rs")), "x.rs");
    }
}
