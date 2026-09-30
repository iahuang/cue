//! The styles that highlights in the text refer to, in one `SyntaxStyle`
//! shared by every editor. A buffer shows one style's highlights, so
//! everything that highlights text registers its styles here.

use std::rc::Rc;

use opentui::{Attributes, Rgba, SyntaxStyle};

/// Text nothing colors: the terminal's own text color, so it suits the
/// terminal's theme, light or dark.
pub const TEXT: Rgba = Rgba::terminal_default([255, 255, 255]);

/// The find bar's matches.
pub const MATCH_BG: Rgba = Rgba::rgb(95, 85, 55);

/// Syntax styles by tree-sitter capture name: a slot of the terminal's
/// 16-color palette, so they follow its theme, or none to keep the text's
/// color, and attributes. A capture not listed takes the style of what it
/// refines (`keyword.return` is a `keyword`), or stays uncolored, like
/// punctuation.
const SYNTAX: &[(&str, Option<u8>, Attributes)] = &[
    ("attribute", Some(6), Attributes::NONE),
    ("boolean", Some(6), Attributes::NONE),
    ("character", Some(2), Attributes::NONE),
    ("comment", Some(8), Attributes::NONE),
    ("constant", Some(6), Attributes::NONE),
    ("constructor", Some(3), Attributes::NONE),
    ("escape", Some(6), Attributes::NONE),
    ("function", Some(4), Attributes::NONE),
    ("function.macro", Some(6), Attributes::NONE),
    ("keyword", Some(5), Attributes::NONE),
    ("label", Some(6), Attributes::NONE),
    ("number", Some(6), Attributes::NONE),
    ("string", Some(2), Attributes::NONE),
    ("string.escape", Some(6), Attributes::NONE),
    ("tag", Some(1), Attributes::NONE),
    // Markdown.
    ("text.delimiter", Some(8), Attributes::NONE),
    ("text.emphasis", None, Attributes::ITALIC),
    ("text.list", Some(3), Attributes::NONE),
    ("text.literal", Some(2), Attributes::NONE),
    ("text.reference", Some(6), Attributes::NONE),
    ("text.strike", None, Attributes::STRIKETHROUGH),
    ("text.strong", None, Attributes::BOLD),
    ("text.title", Some(4), Attributes::BOLD),
    ("text.uri", Some(6), Attributes::UNDERLINE),
    ("type", Some(3), Attributes::NONE),
    ("variable.builtin", Some(5), Attributes::NONE),
];

/// A syntax color: one of [`SYNTAX`]'s styles. Unlike a style id, it can be
/// worked out on any thread, and drawn without a `SyntaxStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxColor(u8);

impl SyntaxColor {
    /// The color for text a highlight query captured as `capture`: its own,
    /// or that of the nearest capture it refines (`function.method` is a
    /// `function`), or none.
    pub fn of(capture: &str) -> Option<SyntaxColor> {
        let mut name = capture;
        loop {
            if let Some(i) = SYNTAX.iter().position(|&(c, _, _)| c == name) {
                return Some(SyntaxColor(i as u8));
            }
            name = &name[..name.rfind('.')?];
        }
    }

    /// `None` to keep the text's color.
    pub fn fg(self) -> Option<Rgba> {
        SYNTAX[self.0 as usize].1.map(Rgba::indexed)
    }

    pub fn attributes(self) -> Attributes {
        SYNTAX[self.0 as usize].2
    }
}

/// Style ids for [`SYNTAX`]'s captures, in order.
#[derive(Clone)]
pub struct SyntaxStyles(Vec<u32>);

impl SyntaxStyles {
    pub fn of(&self, color: SyntaxColor) -> u32 {
        self.0[color.0 as usize]
    }
}

pub struct Theme {
    style: Rc<SyntaxStyle>,
    /// The find bar's matches: a background alone, so text keeps its color.
    pub find_match: u32,
    syntax: SyntaxStyles,
}

impl Theme {
    pub fn new() -> opentui::Result<Theme> {
        let style = SyntaxStyle::new()?;
        let find_match = style.register("find.match", None, Some(MATCH_BG), Attributes::NONE);
        let syntax = SYNTAX
            .iter()
            .map(|&(capture, slot, attributes)| {
                let fg = slot.map(Rgba::indexed);
                style.register(capture, fg, None, attributes)
            })
            .collect();
        let syntax = SyntaxStyles(syntax);
        Ok(Theme {
            style: Rc::new(style),
            find_match,
            syntax,
        })
    }

    /// The style for text a highlight query captured as `capture`, as
    /// [`SyntaxColor::of`] picks it.
    #[cfg(test)]
    pub fn capture_style(&self, capture: &str) -> Option<u32> {
        SyntaxColor::of(capture).map(|color| self.syntax.of(color))
    }

    /// The style ids of syntax colors.
    pub fn syntax_styles(&self) -> SyntaxStyles {
        self.syntax.clone()
    }

    /// The style to give buffers.
    pub fn syntax_style(&self) -> Rc<SyntaxStyle> {
        self.style.clone()
    }

    /// Defines a style for tests to highlight with.
    #[cfg(test)]
    pub fn register(&self, name: &str, fg: Option<Rgba>, bg: Option<Rgba>) -> u32 {
        self.style.register(name, fg, bg, Attributes::NONE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_fall_back_to_what_they_refine() {
        let _serial = crate::test_serial();
        let theme = Theme::new().unwrap();
        let comment = theme.capture_style("comment");
        assert!(comment.is_some());
        assert_eq!(theme.capture_style("comment.documentation"), comment);
        assert_ne!(
            theme.capture_style("function.macro"),
            theme.capture_style("function")
        );
        assert_eq!(
            theme.capture_style("function.method.call"),
            theme.capture_style("function")
        );
        assert_eq!(theme.capture_style("punctuation.bracket"), None);
        assert_eq!(theme.capture_style("functional"), None);
    }
}
