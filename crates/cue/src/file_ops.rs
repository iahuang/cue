//! Creating, moving, copying, and trashing files and folders, as the file
//! tree's commands do. The host does them (see [`crate::host`]); the app
//! catches up once they're done.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What to do to the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// An empty file, and the folders it's in if need be. Fails if it's
    /// there already.
    CreateFile(PathBuf),
    /// A folder, and the folders it's in if need be.
    CreateFolder(PathBuf),
    /// Renames or moves a file or folder, creating the folder it goes in
    /// if need be.
    Move { from: PathBuf, to: PathBuf },
    /// Copies a file or folder, creating the folder it goes in if need be.
    Copy { from: PathBuf, to: PathBuf },
    /// Moves a file or folder to the Trash.
    Trash(PathBuf),
}

/// Does `op`, or says why it couldn't, in a sentence.
pub fn run(op: &FileOp) -> Result<(), String> {
    match op {
        FileOp::CreateFile(path) => {
            let created = (|| {
                if let Some(folder) = path.parent() {
                    fs::create_dir_all(folder)?;
                }
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(drop)
            })();
            created.map_err(|err| format!("Can't create {}: {err}", file_name(path)))
        }
        FileOp::CreateFolder(path) => fs::create_dir_all(path)
            .map_err(|err| format!("Can't create {}: {err}", file_name(path))),
        FileOp::Move { from, to } => {
            create_folder_of(to)?;
            fs::rename(from, to).map_err(|err| format!("Can't move {}: {err}", file_name(from)))
        }
        FileOp::Copy { from, to } => {
            create_folder_of(to)?;
            copy_all(from, to).map_err(|err| format!("Can't copy {}: {err}", file_name(from)))
        }
        FileOp::Trash(path) => move_to_trash(path)
            .map_err(|err| format!("Can't move {} to the Trash: {err}", file_name(path))),
    }
}

/// Creates the folder `path` goes in, if need be.
fn create_folder_of(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(folder) => fs::create_dir_all(folder)
            .map_err(|err| format!("Can't create the destination folder: {err}")),
        None => Ok(()),
    }
}

/// Copies the file, folder, or symlink `from` to `to`, which doesn't exist.
fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if meta.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(drop)
    }
}

/// Moves `path` to the Trash.
#[cfg(not(test))]
fn move_to_trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let trashed = {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut context = trash::TrashContext::default();
        // The default, the Finder's AppleScript, asks for permission to
        // control the Finder, and plays its sound.
        context.set_delete_method(DeleteMethod::NsFileManager);
        context.delete(path)
    };
    #[cfg(not(target_os = "macos"))]
    let trashed = trash::delete(path);
    trashed.map_err(|err| err.to_string())
}

/// Tests delete instead, to leave the Trash alone.
#[cfg(test)]
fn move_to_trash(path: &Path) -> Result<(), String> {
    let removed = match path.is_dir() {
        true => fs::remove_dir_all(path),
        false => fs::remove_file(path),
    };
    removed.map_err(|err| err.to_string())
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
