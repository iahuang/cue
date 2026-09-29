//! The folders cue is working in. The file tree shows them, and the file
//! picker lists their files.
//!
//! There can be several roots, but the command line only opens one for now.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Workspace {
    /// Absolute, symlink-free paths, in the order they were added.
    roots: Vec<PathBuf>,
}

impl Workspace {
    /// A workspace of the directories in `roots`.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> io::Result<Workspace> {
        let mut workspace = Workspace { roots: Vec::new() };
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
        }
        Ok(())
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
        Path::new(&root_name(root))
            .join(relative)
            .display()
            .to_string()
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

/// The name shown for a root: its last component, or the whole path for `/`.
pub fn root_name(root: &Path) -> String {
    match root.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => root.display().to_string(),
    }
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
}
