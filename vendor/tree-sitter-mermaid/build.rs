fn main() {
    cc::Build::new()
        .include("src")
        .warnings(false)
        .file("src/parser.c")
        .compile("tree_sitter_mermaid");
    println!("cargo:rerun-if-changed=src");
}
