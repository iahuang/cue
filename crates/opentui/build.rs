//! Lets this crate's examples and tests run without `cargo run`, which is what
//! normally puts `libopentui` on the loader path.

fn main() {
    let Ok(lib_dir) = std::env::var("DEP_OPENTUI_LIB_DIR") else {
        return;
    };
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "windows" {
        // Applies to this package's examples, tests, and benches only; binaries
        // in other crates need their own rpath or a bundled library.
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
}
