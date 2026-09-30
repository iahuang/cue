//! Which language a file is written in, from its name or, failing that, a
//! `#!` first line.

use std::path::Path;

pub struct Language {
    /// As shown in the status bar.
    pub name: &'static str,
    /// Without the dot, lowercase.
    extensions: &'static [&'static str],
    /// Whole file names, such as `Cargo.lock`.
    file_names: &'static [&'static str],
    /// Programs a `#!` line runs the file with, without version numbers.
    interpreters: &'static [&'static str],
    /// How to highlight it, if cue can.
    pub syntax: Option<Syntax>,
}

/// What syntax highlighting needs: a tree-sitter grammar, and queries whose
/// captures name what to color.
pub struct Syntax {
    pub grammar: fn() -> tree_sitter::Language,
    /// Applied as one query, in order: where patterns capture the same
    /// text, the last one wins, so later queries refine earlier ones.
    pub highlights: &'static [&'static str],
    /// Applied as one query: where to highlight parts of the text as
    /// another language, such as a Markdown code block's.
    pub injections: &'static [&'static str],
}

impl Language {
    const fn highlighted(
        self,
        grammar: fn() -> tree_sitter::Language,
        highlights: &'static [&'static str],
    ) -> Language {
        Language {
            syntax: Some(Syntax {
                grammar,
                highlights,
                injections: &[],
            }),
            ..self
        }
    }

    /// Highlights parts of it as other languages, with `injections`.
    const fn injecting(self, injections: &'static [&'static str]) -> Language {
        let Some(syntax) = self.syntax else {
            panic!("injections without highlights");
        };
        Language {
            syntax: Some(Syntax {
                injections,
                ..syntax
            }),
            ..self
        }
    }
}

const fn language(
    name: &'static str,
    extensions: &'static [&'static str],
    file_names: &'static [&'static str],
    interpreters: &'static [&'static str],
) -> Language {
    Language {
        name,
        extensions,
        file_names,
        interpreters,
        syntax: None,
    }
}

static LANGUAGES: &[Language] = &[
    language("Rust", &["rs"], &[], &[]).highlighted(
        || tree_sitter_rust::LANGUAGE.into(),
        &[include_str!("../queries/rust/highlights.scm")],
    ),
    language("TOML", &["toml"], &["Cargo.lock"], &[]).highlighted(
        || tree_sitter_toml_ng::LANGUAGE.into(),
        &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
    ),
    // Block structure: inline markup is `MARKDOWN_INLINE`, injected into
    // each paragraph, heading, and table cell.
    language("Markdown", &["md", "markdown"], &[], &[])
        .highlighted(
            || tree_sitter_md::LANGUAGE.into(),
            &[
                tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
                include_str!("../queries/markdown/highlights.scm"),
            ],
        )
        .injecting(&[
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            include_str!("../queries/markdown/injections.scm"),
        ]),
    language("JSON", &["json", "jsonc"], &[], &[]).highlighted(
        || tree_sitter_json::LANGUAGE.into(),
        &[tree_sitter_json::HIGHLIGHTS_QUERY],
    ),
    language("Python", &["py", "pyi", "pyw"], &[], &["python"]).highlighted(
        || tree_sitter_python::LANGUAGE.into(),
        &[tree_sitter_python::HIGHLIGHTS_QUERY],
    ),
    language("JavaScript", &["js", "mjs", "cjs", "jsx"], &[], &["node"]).highlighted(
        || tree_sitter_javascript::LANGUAGE.into(),
        &[
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
        ],
    ),
    // TypeScript's queries only add to JavaScript's.
    language("TypeScript", &["ts", "mts", "cts"], &[], &["deno", "bun"]).highlighted(
        || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        &[
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ],
    ),
    language("TSX", &["tsx"], &[], &[]).highlighted(
        || tree_sitter_typescript::LANGUAGE_TSX.into(),
        &[
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        ],
    ),
    language("Go", &["go"], &[], &[]).highlighted(
        || tree_sitter_go::LANGUAGE.into(),
        &[tree_sitter_go::HIGHLIGHTS_QUERY],
    ),
    language("C", &["c", "h"], &[], &[]).highlighted(
        || tree_sitter_c::LANGUAGE.into(),
        &[tree_sitter_c::HIGHLIGHT_QUERY],
    ),
    // C++'s queries only add to C's.
    language("C++", &["cc", "cpp", "cxx", "hh", "hpp", "hxx"], &[], &[]).highlighted(
        || tree_sitter_cpp::LANGUAGE.into(),
        &[
            tree_sitter_c::HIGHLIGHT_QUERY,
            tree_sitter_cpp::HIGHLIGHT_QUERY,
        ],
    ),
    language(
        "Shell",
        &["sh", "bash", "zsh"],
        &[
            ".bashrc",
            ".bash_profile",
            ".profile",
            ".zshrc",
            ".zprofile",
        ],
        &["sh", "bash", "zsh"],
    )
    .highlighted(
        || tree_sitter_bash::LANGUAGE.into(),
        &[tree_sitter_bash::HIGHLIGHT_QUERY],
    ),
    language("YAML", &["yaml", "yml"], &[], &[]).highlighted(
        || tree_sitter_yaml::LANGUAGE.into(),
        &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
    ),
    language("HTML", &["html", "htm"], &[], &[])
        .highlighted(
            || tree_sitter_html::LANGUAGE.into(),
            &[tree_sitter_html::HIGHLIGHTS_QUERY],
        )
        .injecting(&[tree_sitter_html::INJECTIONS_QUERY]),
    language("CSS", &["css"], &[], &[]).highlighted(
        || tree_sitter_css::LANGUAGE.into(),
        &[tree_sitter_css::HIGHLIGHTS_QUERY],
    ),
    language("Zig", &["zig", "zon"], &[], &[]).highlighted(
        || tree_sitter_zig::LANGUAGE.into(),
        &[tree_sitter_zig::HIGHLIGHTS_QUERY],
    ),
];

/// Markdown's inline markup, which only comes injected into Markdown.
static MARKDOWN_INLINE: Language = language("Markdown inline", &[], &[], &[])
    .highlighted(
        || tree_sitter_md::INLINE_LANGUAGE.into(),
        &[
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            include_str!("../queries/markdown_inline/highlights.scm"),
        ],
    )
    .injecting(&[tree_sitter_md::INJECTION_QUERY_INLINE]);

/// Every language cue knows.
#[cfg(test)]
pub fn all() -> impl Iterator<Item = &'static Language> {
    LANGUAGES.iter().chain([&MARKDOWN_INLINE])
}

/// The language an injection query names, as in a Markdown code block's
/// info string: `rust`, `rs`, `c++`, `bash`, or `rust,ignore`.
pub fn injected(name: &str) -> Option<&'static Language> {
    if name == "markdown_inline" {
        return Some(&MARKDOWN_INLINE);
    }
    let name = name
        .split([',', ' ', '{', '}'])
        .find(|word| !word.is_empty())?
        .trim_start_matches('.')
        .to_ascii_lowercase();
    LANGUAGES.iter().find(|l| {
        l.name.eq_ignore_ascii_case(&name)
            || l.extensions.contains(&name.as_str())
            || l.interpreters.contains(&name.as_str())
    })
}

/// The language of the file at `path`, going by its name, or else by the
/// program its `#!` line runs it with. `first_line` is only asked for when
/// the name doesn't tell.
pub fn detect(
    path: Option<&Path>,
    first_line: impl FnOnce() -> String,
) -> Option<&'static Language> {
    by_name(path?).or_else(|| by_interpreter(&first_line()))
}

fn by_name(path: &Path) -> Option<&'static Language> {
    let name = path.file_name()?.to_str()?;
    if let Some(language) = LANGUAGES.iter().find(|l| l.file_names.contains(&name)) {
        return Some(language);
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.extensions.contains(&extension.as_str()))
}

/// `#!/bin/sh`, `#!/usr/bin/env python3`, `#!/usr/bin/env -S deno run`.
fn by_interpreter(first_line: &str) -> Option<&'static Language> {
    let mut words = first_line.strip_prefix("#!")?.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == "env" {
        program = words.find(|word| !word.starts_with('-'))?;
    }
    let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    LANGUAGES.iter().find(|l| l.interpreters.contains(&program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(path: &str, first_line: &str) -> Option<&'static str> {
        detect(Some(Path::new(path)), || first_line.to_string()).map(|l| l.name)
    }

    #[test]
    fn detects_by_extension_then_file_name() {
        assert_eq!(name("src/main.rs", ""), Some("Rust"));
        assert_eq!(name("README.MD", ""), Some("Markdown"), "any case");
        assert_eq!(name("Cargo.lock", ""), Some("TOML"));
        assert_eq!(name("/home/me/.zshrc", ""), Some("Shell"));
        assert_eq!(name("notes.txt", ""), None);
        assert_eq!(name("Makefile", ""), None);
    }

    #[test]
    fn detects_by_interpreter() {
        assert_eq!(name("run", "#!/bin/sh"), Some("Shell"));
        assert_eq!(name("run", "#!/usr/bin/env python3"), Some("Python"));
        assert_eq!(name("run", "#!/usr/bin/python3.12 -u"), Some("Python"));
        assert_eq!(
            name("run", "#!/usr/bin/env -S deno run"),
            Some("TypeScript")
        );
        assert_eq!(name("run", "#!/usr/bin/env"), None);
        assert_eq!(name("run", "# not a shebang"), None);
        // The name wins over the first line.
        assert_eq!(name("run.rs", "#!/bin/sh"), Some("Rust"));
    }

    #[test]
    fn finds_injected_languages_by_any_name() {
        let name = |name: &str| injected(name).map(|l| l.name);
        assert_eq!(name("rust"), Some("Rust"));
        assert_eq!(name("rs"), Some("Rust"));
        assert_eq!(name("Python"), Some("Python"));
        assert_eq!(name("c++"), Some("C++"));
        assert_eq!(name("shell"), Some("Shell"));
        assert_eq!(name("zsh"), Some("Shell"));
        assert_eq!(name("jsx"), Some("JavaScript"));
        assert_eq!(name("rust,ignore"), Some("Rust"));
        assert_eq!(name("{.python}"), Some("Python"));
        assert_eq!(name("markdown_inline"), Some("Markdown inline"));
        assert_eq!(name("mermaid"), None);
        assert_eq!(name(""), None);
    }

    #[test]
    fn an_unnamed_buffer_has_no_language() {
        assert!(detect(None, || "#!/bin/sh".to_string()).is_none());
    }
}
