//! The vendored tree-sitter grammar in `src`.

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_mermaid() -> *const ();
}

pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_mermaid) };

pub const HIGHLIGHTS_QUERY: &str = include_str!("queries/highlights.scm");
