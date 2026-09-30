//! Which language a file is written in, from its name or, failing that, a
//! `#!` first line.

use std::path::Path;

use crate::indent::Indent;

pub struct Language {
    /// As shown in the status bar.
    pub name: &'static str,
    /// Without the dot, lowercase.
    extensions: &'static [&'static str],
    /// Whole file names, such as `Cargo.lock`.
    file_names: &'static [&'static str],
    /// Programs a `#!` line runs the file with, without version numbers.
    interpreters: &'static [&'static str],
    /// Other names for it, lowercase, as a Markdown code block's info string
    /// might give them.
    aliases: &'static [&'static str],
    /// How to highlight it, if cue can.
    pub syntax: Option<Syntax>,
    /// How files in it indent by custom, where it's not four spaces: for
    /// files that don't show how they do.
    pub indent: Option<Indent>,
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
    /// Applied as one query: what the text defines, for Go to Symbol (see
    /// [`crate::symbols`]).
    pub tags: &'static [&'static str],
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
                tags: &[],
            }),
            ..self
        }
    }

    /// Outlines it for Go to Symbol with `tags`.
    const fn tagged(self, tags: &'static [&'static str]) -> Language {
        let Some(syntax) = self.syntax else {
            panic!("tags without highlights");
        };
        Language {
            syntax: Some(Syntax { tags, ..syntax }),
            ..self
        }
    }

    const fn aka(self, aliases: &'static [&'static str]) -> Language {
        Language { aliases, ..self }
    }

    const fn indented(self, indent: Indent) -> Language {
        Language {
            indent: Some(indent),
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
        aliases: &[],
        syntax: None,
        indent: None,
    }
}

static LANGUAGES: &[Language] = &[
    language("Rust", &["rs"], &[], &[])
        .highlighted(
            || tree_sitter_rust::LANGUAGE.into(),
            &[include_str!("../queries/rust/highlights.scm")],
        )
        .tagged(&[include_str!("../queries/rust/tags.scm")]),
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
        ])
        .tagged(&[include_str!("../queries/markdown/tags.scm")]),
    language("JSON", &["json", "jsonc"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_json::LANGUAGE.into(),
            &[tree_sitter_json::HIGHLIGHTS_QUERY],
        ),
    language("Python", &["py", "pyi", "pyw"], &[], &["python"])
        .highlighted(
            || tree_sitter_python::LANGUAGE.into(),
            &[tree_sitter_python::HIGHLIGHTS_QUERY],
        )
        .tagged(&[tree_sitter_python::TAGS_QUERY]),
    language("JavaScript", &["js", "mjs", "cjs", "jsx"], &[], &["node"])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_javascript::LANGUAGE.into(),
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ],
        )
        .tagged(&[tree_sitter_javascript::TAGS_QUERY]),
    // TypeScript's queries only add to JavaScript's.
    language("TypeScript", &["ts", "mts", "cts"], &[], &["deno", "bun"])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
        )
        .tagged(&[
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY,
        ]),
    language("TSX", &["tsx"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_typescript::LANGUAGE_TSX.into(),
            &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
        )
        .tagged(&[
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY,
        ]),
    // Indented as gofmt writes it.
    language("Go", &["go"], &[], &[])
        .indented(Indent::Tabs)
        .highlighted(
            || tree_sitter_go::LANGUAGE.into(),
            &[tree_sitter_go::HIGHLIGHTS_QUERY],
        )
        .tagged(&[tree_sitter_go::TAGS_QUERY]),
    language("C", &["c", "h"], &[], &[])
        .highlighted(
            || tree_sitter_c::LANGUAGE.into(),
            &[tree_sitter_c::HIGHLIGHT_QUERY],
        )
        .tagged(&[tree_sitter_c::TAGS_QUERY]),
    // C++'s queries only add to C's. CUDA is C++ with a few extensions,
    // which C++'s grammar gets mostly right: CUDA's own grammar is ~7 MB.
    language(
        "C++",
        &["cc", "cpp", "cxx", "hh", "hpp", "hxx", "cu", "cuh"],
        &[],
        &[],
    )
    .aka(&["cuda"])
    .highlighted(
        || tree_sitter_cpp::LANGUAGE.into(),
        &[
            tree_sitter_c::HIGHLIGHT_QUERY,
            tree_sitter_cpp::HIGHLIGHT_QUERY,
        ],
    )
    .tagged(&[tree_sitter_cpp::TAGS_QUERY]),
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
    )
    .tagged(&[include_str!("../queries/bash/tags.scm")]),
    language("YAML", &["yaml", "yml"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_yaml::LANGUAGE.into(),
            &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
        ),
    language("HTML", &["html", "htm"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_html::LANGUAGE.into(),
            &[tree_sitter_html::HIGHLIGHTS_QUERY],
        )
        .injecting(&[tree_sitter_html::INJECTIONS_QUERY]),
    language("CSS", &["css"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_css::LANGUAGE.into(),
            &[tree_sitter_css::HIGHLIGHTS_QUERY],
        ),
    language("Zig", &["zig", "zon"], &[], &[])
        .highlighted(
            || tree_sitter_zig::LANGUAGE.into(),
            &[include_str!("../queries/zig/highlights.scm")],
        )
        .tagged(&[include_str!("../queries/zig/tags.scm")]),
    language("Java", &["java"], &[], &[])
        .highlighted(
            || tree_sitter_java::LANGUAGE.into(),
            &[tree_sitter_java::HIGHLIGHTS_QUERY],
        )
        .tagged(&[tree_sitter_java::TAGS_QUERY]),
    language("Kotlin", &["kt", "kts"], &[], &[]).highlighted(
        || arborium_kotlin::language().into(),
        &[arborium_kotlin::HIGHLIGHTS_QUERY],
    ),
    language("Swift", &["swift"], &[], &["swift"])
        .highlighted(
            || tree_sitter_swift::LANGUAGE.into(),
            &[tree_sitter_swift::HIGHLIGHTS_QUERY],
        )
        .tagged(&[tree_sitter_swift::TAGS_QUERY]),
    language("Dart", &["dart"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || arborium_dart::language().into(),
            &[arborium_dart::HIGHLIGHTS_QUERY],
        ),
    language(
        "Ruby",
        &["rb", "rake", "gemspec", "ru"],
        &["Gemfile", "Rakefile"],
        &["ruby"],
    )
    .indented(Indent::Spaces(2))
    .highlighted(
        || tree_sitter_ruby::LANGUAGE.into(),
        &[tree_sitter_ruby::HIGHLIGHTS_QUERY],
    )
    .tagged(&[tree_sitter_ruby::TAGS_QUERY]),
    // Text outside `<?php ?>` is HTML.
    language("PHP", &["php", "phtml"], &[], &["php"])
        .highlighted(
            || tree_sitter_php::LANGUAGE_PHP.into(),
            &[tree_sitter_php::HIGHLIGHTS_QUERY],
        )
        .injecting(&[
            tree_sitter_php::INJECTIONS_QUERY,
            include_str!("../queries/php/injections.scm"),
        ])
        .tagged(&[tree_sitter_php::TAGS_QUERY]),
    language("Perl", &["pl", "pm"], &[], &["perl"])
        .highlighted(
            || arborium_perl::language().into(),
            &[include_str!("../queries/perl/highlights.scm")],
        )
        .injecting(&[arborium_perl::INJECTIONS_QUERY]),
    language("Elixir", &["ex", "exs"], &[], &["elixir"])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_elixir::LANGUAGE.into(),
            &[tree_sitter_elixir::HIGHLIGHTS_QUERY],
        )
        .injecting(&[tree_sitter_elixir::INJECTIONS_QUERY])
        .tagged(&[tree_sitter_elixir::TAGS_QUERY]),
    language("Haskell", &["hs"], &[], &["runhaskell", "runghc"])
        .highlighted(
            || tree_sitter_haskell::LANGUAGE.into(),
            &[include_str!("../queries/haskell/highlights.scm")],
        )
        .injecting(&[tree_sitter_haskell::INJECTIONS_QUERY]),
    language("R", &["r"], &[".Rprofile"], &["Rscript"])
        .highlighted(
            || tree_sitter_r::LANGUAGE.into(),
            &[tree_sitter_r::HIGHLIGHTS_QUERY],
        )
        .tagged(&[tree_sitter_r::TAGS_QUERY]),
    language("SQL", &["sql"], &[], &[]).highlighted(
        || tree_sitter_sequel::LANGUAGE.into(),
        &[tree_sitter_sequel::HIGHLIGHTS_QUERY],
    ),
    language("GraphQL", &["graphql", "gql"], &[], &[]).highlighted(
        || arborium_graphql::language().into(),
        &[include_str!("../queries/graphql/highlights.scm")],
    ),
    language("PowerShell", &["ps1", "psm1", "psd1"], &[], &["pwsh"]).highlighted(
        || tree_sitter_powershell::LANGUAGE.into(),
        &[tree_sitter_powershell::HIGHLIGHTS_QUERY],
    ),
    language(
        "Clojure",
        &["clj", "cljs", "cljc", "edn", "bb"],
        &[],
        &["bb", "clojure"],
    )
    .indented(Indent::Spaces(2))
    .highlighted(
        || arborium_clojure::language().into(),
        &[include_str!("../queries/clojure/highlights.scm")],
    ),
    language("Scheme", &["scm", "ss", "sld"], &[], &["guile"])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_scheme::LANGUAGE.into(),
            &[tree_sitter_scheme::HIGHLIGHTS_QUERY],
        ),
    language("Common Lisp", &["lisp", "lsp", "cl", "asd"], &[], &["sbcl"])
        .aka(&["commonlisp"])
        .indented(Indent::Spaces(2))
        .highlighted(
            || arborium_commonlisp::language().into(),
            &[arborium_commonlisp::HIGHLIGHTS_QUERY],
        ),
    language("GDScript", &["gd"], &[], &[]).highlighted(
        || tree_sitter_gdscript::LANGUAGE.into(),
        &[include_str!("../queries/gdscript/highlights.scm")],
    ),
    // GLSL's queries only add to C's.
    language(
        "GLSL",
        &["glsl", "vert", "frag", "geom", "comp", "tesc", "tese"],
        &[],
        &[],
    )
    .highlighted(
        || tree_sitter_glsl::LANGUAGE_GLSL.into(),
        &[
            tree_sitter_c::HIGHLIGHT_QUERY,
            include_str!("../queries/glsl/highlights.scm"),
        ],
    ),
    language(
        "Vim script",
        &["vim"],
        &[".vimrc", "_vimrc", ".gvimrc"],
        &[],
    )
    .aka(&["vimscript", "viml"])
    .highlighted(
        || arborium_vim::language().into(),
        &[arborium_vim::HIGHLIGHTS_QUERY],
    )
    .injecting(&[arborium_vim::INJECTIONS_QUERY]),
    // Svelte's queries only add to HTML's.
    language("Svelte", &["svelte"], &[], &[])
        .indented(Indent::Spaces(2))
        .highlighted(
            || tree_sitter_svelte_ng::LANGUAGE.into(),
            &[
                tree_sitter_html::HIGHLIGHTS_QUERY,
                tree_sitter_svelte_ng::HIGHLIGHTS_QUERY,
            ],
        )
        .injecting(&[include_str!("../queries/svelte/injections.scm")]),
    language("Assembly", &["asm", "s"], &[], &[]).highlighted(
        || tree_sitter_asm::LANGUAGE.into(),
        &[tree_sitter_asm::HIGHLIGHTS_QUERY],
    ),
    language(
        "Dockerfile",
        &["dockerfile", "containerfile"],
        &["Dockerfile", "Containerfile"],
        &[],
    )
    .aka(&["docker"])
    .highlighted(
        || arborium_dockerfile::language().into(),
        &[arborium_dockerfile::HIGHLIGHTS_QUERY],
    ),
    // make needs tabs.
    language(
        "Makefile",
        &["mk", "mak"],
        &["Makefile", "makefile", "GNUmakefile"],
        &["make"],
    )
    .indented(Indent::Tabs)
    .highlighted(
        || tree_sitter_make::LANGUAGE.into(),
        &[
            include_str!("../queries/make/highlights.scm"),
            tree_sitter_make::HIGHLIGHTS_QUERY,
        ],
    ),
    language("CMake", &["cmake"], &["CMakeLists.txt"], &[]).highlighted(
        || tree_sitter_cmake::LANGUAGE.into(),
        &[include_str!("../queries/cmake/highlights.scm")],
    ),
    language("Nginx", &["nginx"], &["nginx.conf"], &[]).highlighted(
        || tree_sitter_nginx::LANGUAGE.into(),
        &[include_str!("../queries/nginx/highlights.scm")],
    ),
    language("Diff", &["diff", "patch"], &[], &[]).highlighted(
        || arborium_diff::language().into(),
        &[include_str!("../queries/diff/highlights.scm")],
    ),
    language(
        "Requirements",
        &[],
        &[
            "requirements.txt",
            "requirements-dev.txt",
            "constraints.txt",
        ],
        &[],
    )
    .aka(&["requirements"])
    .highlighted(
        || tree_sitter_requirements::LANGUAGE.into(),
        &[tree_sitter_requirements::HIGHLIGHTS_QUERY],
    ),
    // Raw blocks are highlighted as the language they name.
    language("Typst", &["typ"], &[], &[])
        .highlighted(
            || arborium_typst::language().into(),
            &[arborium_typst::HIGHLIGHTS_QUERY],
        )
        .injecting(&[include_str!("../queries/typst/injections.scm")]),
    language("LaTeX", &["tex", "sty", "cls", "ltx"], &[], &[]).highlighted(
        || tree_sitter_latex::LANGUAGE.into(),
        &[include_str!("../queries/latex/highlights.scm")],
    ),
    language("BibTeX", &["bib"], &[], &[]).highlighted(
        || tree_sitter_bibtex::LANGUAGE.into(),
        &[include_str!("../queries/bibtex/highlights.scm")],
    ),
    language("Mermaid", &["mmd", "mermaid"], &[], &[]).highlighted(
        || tree_sitter_mermaid::LANGUAGE.into(),
        &[tree_sitter_mermaid::HIGHLIGHTS_QUERY],
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
            || l.aliases.contains(&name.as_str())
            || l.file_names.iter().any(|f| f.eq_ignore_ascii_case(&name))
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
        assert_eq!(name("Makefile", ""), Some("Makefile"));
        assert_eq!(name("docker/Dockerfile", ""), Some("Dockerfile"));
        assert_eq!(name("CMakeLists.txt", ""), Some("CMake"));
        assert_eq!(name("requirements.txt", ""), Some("Requirements"));
        assert_eq!(name("kernel.cu", ""), Some("C++"), "CUDA");
        assert_eq!(name("shader.frag", ""), Some("GLSL"));
    }

    #[test]
    fn detects_by_interpreter() {
        assert_eq!(name("run", "#!/bin/sh"), Some("Shell"));
        assert_eq!(name("run", "#!/usr/bin/env python3"), Some("Python"));
        assert_eq!(name("run", "#!/usr/bin/env ruby"), Some("Ruby"));
        assert_eq!(name("run", "#!/usr/bin/env Rscript"), Some("R"));
        assert_eq!(name("run", "#!/usr/bin/perl -w"), Some("Perl"));
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
        assert_eq!(name("cuda"), Some("C++"));
        assert_eq!(name("dockerfile"), Some("Dockerfile"));
        assert_eq!(name("makefile"), Some("Makefile"));
        assert_eq!(name("vimscript"), Some("Vim script"));
        assert_eq!(name("tex"), Some("LaTeX"));
        assert_eq!(name("kt"), Some("Kotlin"));
        assert_eq!(name("{.python}"), Some("Python"));
        assert_eq!(name("markdown_inline"), Some("Markdown inline"));
        assert_eq!(name("mermaid"), Some("Mermaid"));
        assert_eq!(name("plantuml"), None);
        assert_eq!(name(""), None);
    }

    #[test]
    fn an_unnamed_buffer_has_no_language() {
        assert!(detect(None, || "#!/bin/sh".to_string()).is_none());
    }
}
