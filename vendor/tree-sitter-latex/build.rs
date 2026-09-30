fn main() {
    cc::Build::new()
        .include("src")
        .warnings(false)
        .file("src/parser.c")
        .file("src/scanner.c")
        .compile("tree_sitter_latex");
    println!("cargo:rerun-if-changed=src");
}
