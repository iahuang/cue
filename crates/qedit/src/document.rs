//! Reading and writing files.
//!
//! The native edit buffer splits lines on `\n`, `\r\n`, and `\r`, and returns
//! text joined with `\n`. A file's line ending is detected on load and
//! restored on save, so CRLF files stay CRLF.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

#[derive(Debug)]
pub struct Loaded {
    /// The text with `\n` line breaks.
    pub text: String,
    pub line_ending: LineEnding,
    /// The file mixed line endings (or had lone `\r`s); saving will normalize them.
    pub mixed_endings: bool,
}

/// Reads `path`. A missing file loads as empty, to be created on save.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid UTF-8", path.display()),
        )
    })?;

    let crlf = text.matches("\r\n").count();
    let cr = text.matches('\r').count();
    let lf = text.matches('\n').count();
    let line_ending = if crlf > 0 && crlf * 2 >= lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    // Every \r is part of a \r\n and every \n is (CRLF) or none is (LF).
    let mixed_endings = cr != crlf || (crlf > 0 && crlf != lf);

    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    Ok(Loaded {
        text,
        line_ending,
        mixed_endings,
    })
}

/// Writes `text` (with `\n` line breaks) to `path` atomically: a sibling
/// temporary file is written, synced, and renamed over the target, so a failed
/// save never leaves a truncated file. Existing permissions are kept, and a
/// symlink's target is written rather than the link replaced.
pub fn save(path: &Path, text: &str, line_ending: LineEnding) -> io::Result<()> {
    let target = resolve_symlinks(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".qedit-{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);

    let contents = match line_ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    };

    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        if let Ok(meta) = fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => Ok(target),
            // A dangling link: write where it points.
            Err(_) => {
                let link = fs::read_link(path)?;
                Ok(path.parent().unwrap_or(Path::new("")).join(link))
            }
        },
        Ok(meta) if meta.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a directory", path.display()),
        )),
        _ => Ok(path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qedit-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn crlf_round_trips() {
        let dir = temp_dir("crlf");
        let path = dir.join("win.txt");
        fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "one\ntwo\n");
        assert_eq!(loaded.line_ending, LineEnding::CrLf);
        assert!(!loaded.mixed_endings);
        save(&path, "one\ntwo\nthree\n", loaded.line_ending).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "one\r\ntwo\r\nthree\r\n"
        );
    }

    #[test]
    fn detects_mixed_endings_and_missing_files() {
        let dir = temp_dir("mixed");
        let path = dir.join("mixed.txt");
        fs::write(&path, "a\r\nb\nc\rd").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "a\nb\nc\nd");
        assert!(loaded.mixed_endings);

        let missing = load(&dir.join("new.txt")).unwrap();
        assert_eq!(missing.text, "");
        assert_eq!(missing.line_ending, LineEnding::Lf);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = temp_dir("utf8");
        let path = dir.join("bin");
        fs::write(&path, [0xff, 0xfe, b'a']).unwrap();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_permissions_and_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = temp_dir("perms");
        let real = dir.join("script.sh");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o751)).unwrap();
        let link = dir.join("link.sh");
        symlink(&real, &link).unwrap();

        save(&link, "new\n", LineEnding::Lf).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o751
        );
        // No temp files left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn save_into_missing_directory_fails_cleanly() {
        let dir = temp_dir("missing-dir");
        let err = save(&dir.join("nope/file.txt"), "x", LineEnding::Lf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
}
