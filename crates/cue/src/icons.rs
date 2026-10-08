//! Nerd Font icons for files, folders, and terminals.
//!
//! Off unless the `ui.nerd_font` setting or `CUE_NERD_FONT` turns them on,
//! since without a Nerd Font the glyphs show as boxes. `CUE_NERD_FONT` set
//! to `0` or nothing turns them off. Every icon is one column
//! wide and drawn with a space after it, which lets terminals that draw
//! icons wider, as Ghostty and kitty do, spill into the space.

use opentui::{Attributes, Buffer, Rgba};

use crate::theme::{self, Hue};

const BLUE: Hue = Hue::Blue;
const SAPPHIRE: Hue = Hue::Sapphire;
const SKY: Hue = Hue::Sky;
const TEAL: Hue = Hue::Teal;
const GREEN: Hue = Hue::Green;
const YELLOW: Hue = Hue::Yellow;
const PEACH: Hue = Hue::Peach;
const RED: Hue = Hue::Red;
const PINK: Hue = Hue::Pink;
const MAUVE: Hue = Hue::Mauve;
const LAVENDER: Hue = Hue::Lavender;
const GRAY: Hue = Hue::Gray;

/// Columns an icon takes, with the space after it.
pub const WIDTH: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Icon {
    pub glyph: char,
    /// Its color, in the theme in use.
    pub color: Hue,
}

impl Icon {
    const fn new(glyph: char, color: Hue) -> Icon {
        Icon { glyph, color }
    }

    /// Draws the icon at `x`, in `fg` if given, else its own color.
    /// Returns the column after it and its space.
    pub fn draw(self, frame: &Buffer, x: u32, y: u32, fg: Option<Rgba>) -> u32 {
        let glyph = self.glyph.encode_utf8(&mut [0; 4]).to_owned();
        frame.draw_text(
            &glyph,
            x,
            y,
            fg.unwrap_or_else(|| theme::colors().hue(self.color)),
            None,
            Attributes::NONE,
        );
        x + WIDTH
    }
}

/// Whether to show icons.
#[cfg(not(test))]
pub fn enabled() -> bool {
    static ENV: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    let env = *ENV.get_or_init(|| {
        std::env::var_os("CUE_NERD_FONT").map(|value| !value.is_empty() && value != "0")
    });
    env.unwrap_or_else(|| crate::config::get().nerd_font)
}

#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether to show icons: in tests, whether this test turned them on.
#[cfg(test)]
pub fn enabled() -> bool {
    ENABLED.with(std::cell::Cell::get)
}

/// Shows icons for the rest of this test.
#[cfg(test)]
pub fn enable() {
    ENABLED.with(|enabled| enabled.set(true));
}

/// Columns an icon takes, with its space, if icons are shown; else 0.
pub fn width() -> u32 {
    if enabled() {
        WIDTH
    } else {
        0
    }
}

const FILE: Icon = Icon::new('\u{e64e}', GRAY); // seti-default
const FOLDER: Icon = Icon::new('\u{e5ff}', BLUE); // custom-folder
const FOLDER_OPEN: Icon = Icon::new('\u{e5fe}', BLUE); // custom-folder_open
const TERMINAL: Icon = Icon::new('\u{ea85}', GREEN); // cod-terminal
const BRANCH: Icon = Icon::new('\u{e725}', PEACH); // dev-git_branch

/// A folder's icon, open if its contents are showing.
pub fn folder(open: bool) -> Icon {
    if open {
        FOLDER_OPEN
    } else {
        FOLDER
    }
}

pub fn terminal() -> Icon {
    TERMINAL
}

pub fn branch() -> Icon {
    BRANCH
}

/// The icon for a file named `name`: by its whole name, else its extension.
pub fn file(name: &str) -> Icon {
    let lower = name.to_ascii_lowercase();
    let by_name = match lower.as_str() {
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" => Some(('\u{e65d}', PEACH)), // seti-git
        "cargo.toml" | "cargo.lock" => Some(('\u{e7a8}', PEACH)), // dev-rust
        "package.json" | "package-lock.json" | ".npmrc" => Some(('\u{e616}', RED)), // seti-npm
        "dockerfile" | "containerfile" | ".dockerignore" => Some(('\u{e650}', SAPPHIRE)), // seti-docker
        "makefile" | "gnumakefile" | "justfile" => Some(('\u{e673}', PEACH)), // seti-makefile
        "license" | "license.md" | "license.txt" | "copying" => Some(('\u{e60a}', YELLOW)), // seti-license
        ".editorconfig" => Some(('\u{e652}', GRAY)), // seti-editorconfig
        _ if lower.starts_with("dockerfile.") => Some(('\u{e650}', SAPPHIRE)),
        _ if lower.starts_with(".env") => Some(('\u{e615}', YELLOW)), // seti-config
        _ => None,
    };
    if let Some((glyph, color)) = by_name {
        return Icon::new(glyph, color);
    }
    let Some((_, extension)) = lower.rsplit_once('.') else {
        return FILE;
    };
    let (glyph, color) = match extension {
        "rs" => ('\u{e7a8}', PEACH),                         // dev-rust
        "toml" => ('\u{e6b2}', GRAY),                        // custom-toml
        "md" | "markdown" | "mdx" => ('\u{e609}', LAVENDER), // seti-markdown
        "json" | "jsonc" | "json5" => ('\u{e60b}', YELLOW),  // seti-json
        "py" | "pyi" => ('\u{e606}', YELLOW),                // seti-python
        "js" | "mjs" | "cjs" => ('\u{e60c}', YELLOW),        // seti-javascript
        "ts" | "mts" | "cts" => ('\u{e628}', BLUE),          // seti-typescript
        "tsx" | "jsx" => ('\u{e7ba}', SKY),                  // dev-react
        "go" => ('\u{e627}', SAPPHIRE),                      // seti-go
        "c" | "h" => ('\u{e649}', BLUE),                     // seti-c
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => ('\u{e646}', BLUE), // seti-cpp
        "sh" | "bash" | "zsh" | "fish" => ('\u{e691}', GREEN), // seti-shell
        "yml" | "yaml" => ('\u{e6a8}', MAUVE),               // seti-yml
        "html" | "htm" => ('\u{e60e}', PEACH),               // seti-html
        "css" => ('\u{e614}', BLUE),                         // seti-css
        "scss" | "sass" => ('\u{e603}', PINK),               // seti-sass
        "zig" => ('\u{e6a9}', PEACH),                        // seti-zig
        "lock" => ('\u{e672}', GRAY),                        // seti-lock
        "java" => ('\u{e66d}', RED),                         // seti-java
        "rb" => ('\u{e605}', RED),                           // seti-ruby
        "lua" => ('\u{e620}', BLUE),                         // seti-lua
        "swift" => ('\u{e699}', PEACH),                      // seti-swift
        "php" => ('\u{e608}', MAUVE),                        // seti-php
        "hs" => ('\u{e61f}', MAUVE),                         // seti-haskell
        "kt" | "kts" => ('\u{e634}', MAUVE),                 // seti-kotlin
        "cs" => ('\u{e648}', GREEN),                         // seti-c_sharp
        "ex" | "exs" => ('\u{e62d}', MAUVE),                 // seti-elixir
        "scala" => ('\u{e68e}', RED),                        // seti-scala
        "dart" => ('\u{e64c}', SAPPHIRE),                    // seti-dart
        "vue" => ('\u{e6a0}', GREEN),                        // seti-vue
        "svelte" => ('\u{e697}', PEACH),                     // seti-svelte
        "ml" | "mli" => ('\u{e67a}', PEACH),                 // seti-ocaml
        "graphql" | "gql" => ('\u{e662}', PINK),             // seti-graphql
        "wasm" | "wat" => ('\u{e6a1}', MAUVE),               // seti-wasm
        "tex" => ('\u{e69b}', GREEN),                        // seti-tex
        "ps1" | "psm1" => ('\u{e683}', BLUE),                // seti-powershell
        "xml" => ('\u{e619}', PEACH),                        // seti-xml
        "csv" | "tsv" => ('\u{e64a}', GREEN),                // seti-csv
        "sql" | "db" | "sqlite" | "sqlite3" => ('\u{e64d}', YELLOW), // seti-db
        "ini" | "cfg" | "conf" | "env" => ('\u{e615}', GRAY), // seti-config
        "svg" => ('\u{e698}', PEACH),                        // seti-svg
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif" => ('\u{e60d}', MAUVE), // seti-image
        "pdf" => ('\u{e67d}', RED), // seti-pdf
        "mp4" | "mov" | "mkv" | "webm" | "avi" => ('\u{e69f}', PINK), // seti-video
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => ('\u{e638}', TEAL), // seti-audio
        "ttf" | "otf" | "woff" | "woff2" => ('\u{e659}', GRAY), // seti-font
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "zst" => ('\u{e6aa}', PEACH), // seti-zip
        _ => return FILE,
    };
    Icon::new(glyph, color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_by_name_then_extension() {
        assert_eq!(file("Cargo.toml"), file("main.rs"), "the name wins");
        assert_ne!(file("other.toml"), file("Cargo.toml"));
        assert_eq!(file("README.MD"), file("notes.md"), "ignoring case");
        assert_ne!(file(".env.local"), FILE, "by how the name starts");
        assert_eq!(file("Makefile"), file("makefile"));
        assert_eq!(file("notes"), FILE);
        assert_eq!(file("archive.unknown"), FILE);
    }

    #[test]
    fn icons_take_one_column_and_a_space() {
        let _serial = crate::test_serial();
        let screen =
            opentui::OwnedBuffer::new(8, 1, false, opentui::WidthMethod::Unicode, "test").unwrap();
        screen.clear(Rgba::BLACK);
        let x = file("a.rs").draw(&screen, 0, 0, None);
        let x = folder(true).draw(&screen, x, 0, None);
        screen.draw_text("x", x, 0, Rgba::WHITE, None, Attributes::NONE);
        assert_eq!(screen.to_text(true).trim_end(), "\u{e7a8} \u{e5fe} x");
    }
}
