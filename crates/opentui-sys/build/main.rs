//! Builds `libopentui` from the vendored Zig sources and generates the Rust
//! declarations for its C ABI.
//!
//! Environment:
//! - `OPENTUI_NATIVE_DIR`: path to OpenTUI's `packages/native` (defaults to the
//!   vendored subtree in this repository).
//! - `OPENTUI_LIB_DIR`: directory containing a prebuilt `libopentui` built from
//!   the same sources; skips the Zig build.
//! - `OPENTUI_ZIG`: the `zig` executable (default: `zig` on `PATH`; must be 0.16).
//! - `OPENTUI_ZIG_OPTIMIZE`: Zig optimize mode (default: `ReleaseFast`).

mod gen;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    for var in [
        "OPENTUI_NATIVE_DIR",
        "OPENTUI_LIB_DIR",
        "OPENTUI_ZIG",
        "OPENTUI_ZIG_OPTIMIZE",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let native_dir = env::var_os("OPENTUI_NATIVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../vendor/opentui-native"));
    let native_dir = native_dir.canonicalize().unwrap_or_else(|e| {
        panic!(
            "OpenTUI native sources not found at {}: {e}",
            native_dir.display()
        )
    });

    let generated = gen::generate(&native_dir.join("src"))
        .unwrap_or_else(|e| panic!("binding generation failed: {e}"));
    fs::write(out_dir.join("bindings.rs"), generated.bindings).unwrap();
    fs::write(out_dir.join("link_test.rs"), generated.link_test).unwrap();
    for input in &generated.inputs {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    let lib_dir = match env::var_os("OPENTUI_LIB_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => build_native(&native_dir, &out_dir),
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=opentui");
    // Exposed to dependents' build scripts as DEP_OPENTUI_LIB_DIR, e.g. for
    // adding an rpath or bundling the library next to a binary.
    println!("cargo:lib_dir={}", lib_dir.display());
}

/// Runs `zig build` for the Cargo target and returns the directory holding the library.
fn build_native(native_dir: &Path, out_dir: &Path) -> PathBuf {
    // The generator already reads every Zig file it depends on, but the library
    // also compiles sources the bindings never mention.
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("build.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        native_dir.join("build.zig.zon").display()
    );

    let target = env::var("TARGET").unwrap();
    let (zig_target, output_name) = zig_target(&target)
        .unwrap_or_else(|| panic!("opentui-sys: unsupported target `{target}` (set OPENTUI_LIB_DIR to use a prebuilt library)"));

    // Unpacks the vendored Zig dependency archive into `zig-deps/` (gitignored).
    run(Command::new("sh")
        .arg("scripts/prepare-zig-deps.sh")
        .current_dir(native_dir));

    let zig = env::var("OPENTUI_ZIG").unwrap_or_else(|_| "zig".into());
    let optimize = env::var("OPENTUI_ZIG_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let prefix = out_dir.join("zig-out");
    run(Command::new(&zig)
        .arg("build")
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dlibrary-target={zig_target}"))
        .arg("--prefix")
        .arg(&prefix)
        .arg("--cache-dir")
        .arg(out_dir.join("zig-cache"))
        .current_dir(native_dir));

    // build.zig installs to `<prefix>/../lib/<output_name>/`.
    let lib_dir = out_dir.join("lib").join(output_name);
    assert!(
        lib_dir.is_dir(),
        "zig build did not produce {}",
        lib_dir.display()
    );
    lib_dir
}

/// Maps a Rust target triple to OpenTUI's `SUPPORTED_TARGETS` in build.zig.
fn zig_target(rust_target: &str) -> Option<(&'static str, &'static str)> {
    Some(match rust_target {
        "aarch64-apple-darwin" => ("aarch64-macos.13.0", "aarch64-macos"),
        "x86_64-apple-darwin" => ("x86_64-macos.13.0", "x86_64-macos"),
        "x86_64-unknown-linux-gnu" => ("x86_64-linux-gnu.2.17", "x86_64-linux"),
        "aarch64-unknown-linux-gnu" => ("aarch64-linux-gnu.2.17", "aarch64-linux"),
        "x86_64-unknown-linux-musl" => ("x86_64-linux-musl", "x86_64-linux-musl"),
        "aarch64-unknown-linux-musl" => ("aarch64-linux-musl", "aarch64-linux-musl"),
        "x86_64-pc-windows-gnu" => ("x86_64-windows-gnu", "x86_64-windows"),
        "aarch64-pc-windows-gnullvm" => ("aarch64-windows-gnu", "aarch64-windows"),
        _ => return None,
    })
}

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} failed with {status}");
}
