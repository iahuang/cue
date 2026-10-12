//! Names this build's source (`CUE_SOURCE`), a hash of everything in
//! `src/`, so that a cue on another machine, built for it, is known to
//! speak the same protocol (see `src/remote.rs`): builds for different
//! platforms can't be told apart by their binaries.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src");
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    files.sort();
    // FNV-1a, which is the same everywhere.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        let name = file.to_string_lossy().replace('\\', "/");
        let text = std::fs::read(&file).expect("a source file can be read");
        for byte in name.bytes().chain([0]).chain(text) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=CUE_SOURCE={hash:016x}");
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("src/ can be read");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
