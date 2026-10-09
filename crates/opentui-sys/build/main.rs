//! Builds `libopentui` from the vendored Zig sources, links it statically, and
//! generates the Rust declarations for its C ABI.
//!
//! The archive bundles Yoga, LittleCMS, libwebp, and Zig's runtimes. Before
//! linking, it is merged into one object whose only global symbols are the
//! exported API, so those copies cannot collide with others in the program.
//! On Linux that object also carries Zig's C++ runtime (libc++, libc++abi,
//! libunwind), so binaries need no C++ library on the machine they run on, and
//! musl targets produce fully static executables.
//!
//! Environment:
//! - `OPENTUI_NATIVE_DIR`: path to OpenTUI's `packages/native` (defaults to the
//!   vendored subtree in this repository).
//! - `OPENTUI_LIB_DIR`: directory containing a prebuilt `libopentui.a` built
//!   from the same sources with `-Dlinkage=static`; skips the Zig build. On
//!   Linux, Zig still builds the C++ runtime unless the directory also holds
//!   `libc++.a`, `libc++abi.a`, and `libunwind.a` for the target.
//! - `OPENTUI_ZIG`: the `zig` executable (default: `zig` on `PATH`; must be 0.16).
//! - `OPENTUI_ZIG_OPTIMIZE`: Zig optimize mode (default: `ReleaseFast`).

mod gen;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Zig's C++ runtime, which it leaves out of static archives.
const CXX_RUNTIME: [&str; 3] = ["libc++.a", "libc++abi.a", "libunwind.a"];

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

    let zig = env::var("OPENTUI_ZIG").unwrap_or_else(|_| "zig".into());
    let prebuilt = env::var_os("OPENTUI_LIB_DIR").is_some();
    let lib_dir = match env::var_os("OPENTUI_LIB_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => build_native(&zig, &native_dir, &out_dir),
    };

    let archive = lib_dir.join("libopentui.a");
    assert!(archive.is_file(), "{} not found", archive.display());
    if prebuilt {
        println!("cargo:rerun-if-changed={}", archive.display());
    }
    let mut runtime = Vec::new();
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux" {
        runtime = CXX_RUNTIME.iter().map(|name| lib_dir.join(name)).collect();
        if prebuilt {
            for path in &runtime {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        if !runtime.iter().all(|path| path.is_file()) {
            runtime = build_cxx_runtime(&zig, &out_dir);
        }
    }
    let prelinked_dir = prelink(&archive, &runtime, &generated.exports, &out_dir);

    println!("cargo:rustc-link-search=native={}", prelinked_dir.display());
    println!("cargo:rustc-link-lib=static=opentui");
    link_system_libraries();
}

/// Merges `archive`, plus whatever members of the `runtime` archives it needs,
/// into a single object that keeps only `exports` global, and returns the
/// directory holding the result as `libopentui.a`.
///
/// Hidden visibility alone would not do: a static link still resolves hidden
/// symbols across object files, so the vendored C and C++ libraries would
/// clash with any other copy of them. Symbols can only be made local once
/// everything that references them is in one object.
fn prelink(archive: &Path, runtime: &[PathBuf], exports: &[String], out_dir: &Path) -> PathBuf {
    let dir = out_dir.join("prelinked");
    fs::create_dir_all(&dir).unwrap();
    let object = dir.join("opentui.o");
    let symbols = dir.join("exports.txt");

    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => {
            // Mach-O symbols carry a leading underscore.
            let list: String = exports.iter().map(|name| format!("_{name}\n")).collect();
            fs::write(&symbols, list).unwrap();
            let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
                "aarch64" => "arm64",
                arch => arch,
            }
            .to_owned();
            // Apple's `ld -r` turns symbols left off the list into private
            // externs and then, by default, into locals.
            run(Command::new("ld")
                .args(["-r", "-arch", &arch, "-all_load"])
                .arg(archive)
                .arg("-exported_symbols_list")
                .arg(&symbols)
                .arg("-o")
                .arg(&object));
        }
        _ => {
            let list: String = exports.iter().map(|name| format!("{name}\n")).collect();
            fs::write(&symbols, list).unwrap();
            run(Command::new("ld")
                .args(["-r", "--whole-archive"])
                .arg(archive)
                .args(["--no-whole-archive", "--start-group"])
                .args(runtime)
                .arg("--end-group")
                .arg("-o")
                .arg(&object));
            run(Command::new("objcopy")
                .arg(format!("--keep-global-symbols={}", symbols.display()))
                .arg(&object));
        }
    }

    let lib = dir.join("libopentui.a");
    // `ar r` would add to an archive left over from a previous build.
    let _ = fs::remove_file(&lib);
    run(Command::new("ar").arg("rcs").arg(&lib).arg(&object));
    dir
}

/// Links what `build.zig` would have linked into the shared library.
fn link_system_libraries() {
    match env::var("CARGO_CFG_TARGET_OS").unwrap().as_str() {
        "macos" => {
            for framework in [
                "AppKit",
                "AudioToolbox",
                "CoreAudio",
                "CoreFoundation",
                "CoreGraphics",
                "Foundation",
                "ImageIO",
            ] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
            println!("cargo:rustc-link-lib=dylib=objc");
            // Yoga is C++. Every Mac ships libc++, with the ABI Zig builds against.
            println!("cargo:rustc-link-lib=dylib=c++");
        }
        // musl's libc.a already contains these.
        _ if env::var("CARGO_CFG_TARGET_ENV").unwrap() == "musl" => {}
        _ => {
            for lib in ["dl", "pthread", "m"] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
    }
}

/// Has Zig build its C++ runtime for the target and returns the archives.
///
/// Zig only builds the runtime when linking an executable or shared library, so
/// this links an empty shared library with a private global cache, where the
/// archives are then the only ones by their names.
fn build_cxx_runtime(zig: &str, out_dir: &Path) -> Vec<PathBuf> {
    let (zig_target, _) = target_zig_names();
    let version = Command::new(zig)
        .arg("version")
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{zig} version`: {e}"));
    let version = String::from_utf8(version.stdout).unwrap();
    // Keyed by Zig version so a different Zig rebuilds instead of reusing.
    let dir = out_dir.join("cxx-runtime").join(version.trim());
    let cache = dir.join("zig-global-cache");
    let find = || -> Option<Vec<PathBuf>> {
        CXX_RUNTIME
            .iter()
            .map(|name| find_file(&cache, name))
            .collect()
    };
    if let Some(archives) = find() {
        return archives;
    }

    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let probe = dir.join("probe.cpp");
    fs::write(&probe, "int opentui_cxx_runtime_probe() { return 0; }\n").unwrap();
    run(Command::new(zig)
        .args(["c++", "-shared", "-O2", "-target", zig_target])
        .arg(&probe)
        .arg("-o")
        .arg(dir.join("probe.so"))
        .env("ZIG_GLOBAL_CACHE_DIR", &cache));
    find().unwrap_or_else(|| panic!("zig did not build the C++ runtime in {}", cache.display()))
}

/// Returns the only file named `name` under `dir`, if there is exactly one.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, name, found);
            } else if entry.file_name() == name {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, name, &mut found);
    match <[PathBuf; 1]>::try_from(found) {
        Ok([path]) => Some(path),
        Err(_) => None,
    }
}

/// Runs `zig build` for the Cargo target and returns the directory holding the library.
fn build_native(zig: &str, native_dir: &Path, out_dir: &Path) -> PathBuf {
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

    let (zig_target, output_name) = target_zig_names();

    // Unpacks the vendored Zig dependency archive into `zig-deps/` (gitignored).
    run(Command::new("sh")
        .arg("scripts/prepare-zig-deps.sh")
        .current_dir(native_dir));

    let optimize = env::var("OPENTUI_ZIG_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let prefix = out_dir.join("zig-out");
    run(Command::new(zig)
        .arg("build")
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dlibrary-target={zig_target}"))
        .arg("-Dlinkage=static")
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

/// The Zig target and `build.zig` output name for the Cargo target.
fn target_zig_names() -> (&'static str, &'static str) {
    let target = env::var("TARGET").unwrap();
    zig_target(&target).unwrap_or_else(|| {
        panic!("opentui-sys: unsupported target `{target}` (set OPENTUI_LIB_DIR to use a prebuilt library)")
    })
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
        _ => return None,
    })
}

fn run(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} failed with {status}");
}
