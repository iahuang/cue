//! Embeds the directory holding `libopentui` as an rpath, so the binary runs
//! from `target/` without `cargo run` setting up the loader path.

fn main() {
    let Ok(lib_dir) = std::env::var("DEP_OPENTUI_LIB_DIR") else {
        return;
    };
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{lib_dir}");
    }
}
