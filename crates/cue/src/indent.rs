//! How a file indents: with tabs, or with some number of spaces.
//!
//! It's inferred from the file's text when it's opened, as most editors do:
//! whichever of tabs or spaces starts more lines wins, and for spaces, the
//! step between one line's indentation and the next's that comes up most
//! is the width. A file with no indented lines goes by its language's
//! custom, or four spaces.

use crate::language::Language;

/// Lines past this many aren't looked at.
const MAX_LINES: usize = 10_000;
/// Widths of space indentation that can be inferred, most likely first,
/// for breaking ties.
const WIDTHS: [u32; 4] = [4, 2, 8, 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indent {
    Tabs,
    Spaces(u32),
}

impl Default for Indent {
    fn default() -> Indent {
        Indent::Spaces(4)
    }
}

impl Indent {
    /// How `text` indents, or failing that, `language`'s custom.
    pub fn infer(text: &str, language: Option<&Language>) -> Indent {
        guess(text).unwrap_or_else(|| match language.map(|l| l.name) {
            // As gofmt writes it.
            Some("Go") => Indent::Tabs,
            Some("YAML" | "JSON" | "HTML" | "CSS" | "JavaScript" | "TypeScript" | "TSX") => {
                Indent::Spaces(2)
            }
            _ => Indent::default(),
        })
    }

    /// Columns one level of indentation takes. A tab counts as four, as
    /// the editor shows it.
    pub fn width(self) -> u32 {
        match self {
            Indent::Tabs => 4,
            Indent::Spaces(n) => n,
        }
    }

    /// One level of indentation.
    pub fn unit(self) -> String {
        match self {
            Indent::Tabs => "\t".to_string(),
            Indent::Spaces(n) => " ".repeat(n as usize),
        }
    }

    /// As the status bar shows it: `Spaces: 4`, or `Tabs`.
    pub fn label(self) -> String {
        match self {
            Indent::Tabs => "Tabs".to_string(),
            Indent::Spaces(n) => format!("Spaces: {n}"),
        }
    }

    /// `line` indented one level more. Empty lines stay empty.
    pub fn indent(self, line: &str) -> Option<String> {
        (!line.is_empty()).then(|| self.unit() + line)
    }

    /// How many bytes to take off the start of `line` to outdent it one
    /// level: a tab, or spaces back to the previous multiple of the width.
    /// Zero if it isn't indented.
    pub fn outdent(self, line: &str) -> usize {
        if line.starts_with('\t') {
            return 1;
        }
        let spaces = line.len() - line.trim_start_matches(' ').len();
        let width = self.width() as usize;
        match spaces % width {
            0 => spaces.min(width),
            over => over,
        }
    }
}

/// How `text` indents, if any line of it is indented.
fn guess(text: &str) -> Option<Indent> {
    let (mut tabs, mut spaces) = (0, 0);
    // How often each change in indentation, in spaces, comes up.
    let mut steps = [0u32; 9];
    let mut previous = 0;
    for line in text.lines().take(MAX_LINES) {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            previous = 0;
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 0 {
            spaces += 1;
        }
        let step = indent.abs_diff(previous);
        if step >= 2 && step < steps.len() {
            steps[step] += 1;
        }
        previous = indent;
    }
    if tabs == 0 && spaces == 0 {
        return None;
    }
    if tabs > spaces {
        return Some(Indent::Tabs);
    }
    let mut best = None;
    for width in WIDTHS {
        let count = steps[width as usize];
        if count > 0 && best.is_none_or(|(_, most)| count > most) {
            best = Some((width, count));
        }
    }
    Some(Indent::Spaces(best.map_or(4, |(width, _)| width)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_tabs_or_the_width_of_spaces() {
        assert_eq!(guess("fn a() {\n\tb();\n}\n"), Some(Indent::Tabs));
        assert_eq!(
            guess("a:\n  b:\n    c: 1\n  d: 2\n"),
            Some(Indent::Spaces(2))
        );
        assert_eq!(
            guess("fn a() {\n    if b {\n        c();\n    }\n}\n"),
            Some(Indent::Spaces(4))
        );
        // Block comments' one-space steps and alignment don't count.
        let doc = "/**\n * one\n */\nint f() {\n    return 1 +\n           2;\n}\n";
        assert_eq!(guess(doc), Some(Indent::Spaces(4)));
        // More lines with tabs than spaces.
        assert_eq!(guess("a\n\tb\n\tc\n  d\n"), Some(Indent::Tabs));
        assert_eq!(guess("nothing\nindented\n\n"), None);
        assert_eq!(guess(""), None);
    }

    #[test]
    fn falls_back_to_the_language_then_four_spaces() {
        let go = crate::language::detect(Some(std::path::Path::new("main.go")), String::new);
        assert_eq!(Indent::infer("package main\n", go), Indent::Tabs);
        assert_eq!(Indent::infer("x = 1\n", None), Indent::Spaces(4));
        assert_eq!(Indent::infer("x:\n  y\n", go), Indent::Spaces(2));
    }

    #[test]
    fn outdents_to_the_previous_stop() {
        let four = Indent::Spaces(4);
        assert_eq!(four.outdent("        x"), 4);
        assert_eq!(four.outdent("      x"), 2);
        assert_eq!(four.outdent("  x"), 2);
        assert_eq!(four.outdent("x"), 0);
        assert_eq!(four.outdent("\t\tx"), 1);
        assert_eq!(Indent::Tabs.outdent("    x"), 4);
        assert_eq!(four.indent(""), None);
        assert_eq!(Indent::Tabs.indent("x").as_deref(), Some("\tx"));
    }
}
