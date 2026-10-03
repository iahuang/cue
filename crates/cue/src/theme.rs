//! Colors. A theme names a few: its background, its text, its terminal
//! palette, and any of the rest it wants to set itself. Everything else is
//! worked out from those, so every theme colors everything. The default,
//! Terminal, is the terminal's own colors: text, background, and syntax in
//! them, and the rest mixed from what the terminal says they are.
//!
//! The colors in use are in [`colors`], on any thread. The styles that
//! highlights in the text refer to are in [`Theme`], one `SyntaxStyle`
//! shared by every editor. A buffer shows one style's highlights, so
//! everything that highlights text registers its styles there.

use std::rc::Rc;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::{LazyLock, RwLock};

use opentui::{Attributes, Rgba, SyntaxStyle};

/// Syntax styles by tree-sitter capture name: a slot of the terminal's
/// 16-color palette, or none to keep the text's color, and attributes. A
/// theme can give any of them its own color. A capture not listed takes
/// the style of what it refines (`keyword.return` is a `keyword`), or stays
/// uncolored, like punctuation. Comments and Markdown delimiters are the
/// text's color dimmed rather than slot 8, which some palettes make nearly
/// the background's.
const SYNTAX: &[(&str, Option<u8>, Attributes)] = &[
    ("attribute", Some(6), Attributes::NONE),
    ("boolean", Some(6), Attributes::NONE),
    ("character", Some(2), Attributes::NONE),
    ("comment", None, Attributes::DIM),
    ("constant", Some(6), Attributes::NONE),
    ("constructor", Some(3), Attributes::NONE),
    ("diff.minus", Some(1), Attributes::NONE),
    ("diff.plus", Some(2), Attributes::NONE),
    ("escape", Some(6), Attributes::NONE),
    ("function", Some(4), Attributes::NONE),
    ("function.macro", Some(6), Attributes::NONE),
    ("keyword", Some(5), Attributes::NONE),
    ("label", Some(6), Attributes::NONE),
    ("number", Some(6), Attributes::NONE),
    ("operator", None, Attributes::NONE),
    ("property", None, Attributes::NONE),
    ("string", Some(2), Attributes::NONE),
    ("string.escape", Some(6), Attributes::NONE),
    ("tag", Some(1), Attributes::NONE),
    // Markdown.
    ("text.delimiter", None, Attributes::DIM),
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
    ("variable.parameter", None, Attributes::NONE),
];

/// Captures named differently by different queries, as the [`SYNTAX`]
/// capture they mean: older Neovim names, and Helix's and newer Neovim's
/// `markup` for what cue's Markdown query calls `text`.
const ALIASES: &[(&str, &str)] = &[
    ("conditional", "keyword"),
    ("exception", "keyword"),
    ("field", "property"),
    ("float", "number"),
    ("include", "keyword"),
    ("markup.bold", "text.strong"),
    ("markup.heading", "text.title"),
    ("markup.italic", "text.emphasis"),
    ("markup.link", "text.reference"),
    ("markup.link.url", "text.uri"),
    ("markup.list", "text.list"),
    ("markup.math", "text.literal"),
    ("markup.raw", "text.literal"),
    ("markup.strikethrough", "text.strike"),
    ("markup.strong", "text.strong"),
    ("method", "function"),
    ("parameter", "variable.parameter"),
    ("preproc", "keyword"),
    ("repeat", "keyword"),
    ("storageclass", "keyword"),
    ("variable.member", "property"),
];

/// A syntax color: one of [`SYNTAX`]'s styles. Unlike a style id, it can be
/// worked out on any thread, and drawn without a `SyntaxStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxColor(u8);

impl SyntaxColor {
    /// The color for text a highlight query captured as `capture`: its own,
    /// or that of the nearest capture it refines (`function.method` is a
    /// `function`), or none. Aliases count as what they stand for.
    pub fn of(capture: &str) -> Option<SyntaxColor> {
        let mut name = capture;
        loop {
            if let Some(i) = SYNTAX.iter().position(|&(c, _, _)| c == name) {
                return Some(SyntaxColor(i as u8));
            }
            if let Some(&(_, meant)) = ALIASES.iter().find(|&&(alias, _)| alias == name) {
                name = meant;
                continue;
            }
            name = &name[..name.rfind('.')?];
        }
    }

    /// In the theme in use; `None` to keep the text's color.
    pub fn fg(self) -> Option<Rgba> {
        colors().syntax[self.0 as usize].0
    }

    pub fn attributes(self) -> Attributes {
        colors().syntax[self.0 as usize].1
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
        let colors = colors();
        let find_match = style.register(FIND_MATCH, None, Some(colors.match_bg), Attributes::NONE);
        let syntax = SYNTAX
            .iter()
            .zip(&colors.syntax)
            .map(|(&(capture, _, _), &(fg, attributes))| {
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

    /// Gives the styles the colors in use, keeping their ids.
    pub fn restyle(&self) {
        let colors = colors();
        self.style
            .register(FIND_MATCH, None, Some(colors.match_bg), Attributes::NONE);
        for (&(capture, _, _), &(fg, attributes)) in SYNTAX.iter().zip(&colors.syntax) {
            self.style.register(capture, fg, None, attributes);
        }
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

const FIND_MATCH: &str = "find.match";

// --- the colors in use ---------------------------------------------------------

/// Every color cue draws with, in one theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Colors {
    /// The theme's name.
    pub name: &'static str,
    /// Whether it's dark text on a light background.
    pub light: bool,
    /// Behind the text, and behind popups.
    pub bg: Rgba,
    pub text: Rgba,
    /// Text that matters less: hints, folders, shortcuts.
    pub muted: Rgba,
    /// Text that matters least: line numbers, ignored files.
    pub faint: Rgba,
    /// Popups' borders, and what can't be clicked.
    pub border: Rgba,
    /// Between panels.
    pub divider: Rgba,
    /// The status bar, the tab that's showing, and the header of the panel
    /// the keyboard is in.
    pub surface: Rgba,
    /// Other panels' headers, and the tree's sticky folders.
    pub surface_inactive: Rgba,
    /// A selected row in a list, or the selected text of a query.
    pub selected: Rgba,
    /// A selected row in a list without the keyboard.
    pub selected_unfocused: Rgba,
    /// Selected text in an editor.
    pub selection: Rgba,
    /// What a query matched, and the active file.
    pub accent: Rgba,
    /// Text on [`accent`](Colors::accent).
    pub on_accent: Rgba,
    /// What find matched, behind the text.
    pub match_bg: Rgba,
    /// The match find is on, and its text.
    pub current_match_bg: Rgba,
    pub current_match_fg: Rgba,
    pub error: Rgba,
    /// Behind an error in the status bar.
    pub error_bg: Rgba,
    /// The cursor's line's number.
    pub line_number_current: Rgba,
    /// The terminal's cursor. The terminal's own color in Terminal.
    pub cursor: Rgba,
    /// Over a panel's drop target, blended.
    pub drop_tint: Rgba,
    /// The colors of terminals in cue, if any are known.
    pub terminal: Option<TerminalPalette>,
    hues: [Rgba; HUES],
    syntax: Vec<(Option<Rgba>, Attributes)>,
}

/// The colors programs in cue's terminals start with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalPalette {
    pub fg: Rgba,
    pub bg: Rgba,
    /// Slots 0-15, or `None` for the terminal's own: then the colors cue's
    /// terminals show are the terminal's, which `fg` and `bg` are what it
    /// says they are.
    pub ansi: Option<[Rgba; 16]>,
}

/// Colors for things, such as files' icons, in the theme's palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Red,
    Peach,
    Yellow,
    Green,
    Teal,
    Sky,
    Sapphire,
    Blue,
    Lavender,
    Mauve,
    Pink,
    Gray,
}

const HUES: usize = 12;

impl Colors {
    pub fn hue(&self, hue: Hue) -> Rgba {
        self.hues[hue as usize]
    }
}

#[cfg(not(test))]
static CURRENT: LazyLock<RwLock<Arc<Colors>>> = LazyLock::new(|| {
    RwLock::new(Arc::new(
        ThemeId::TERMINAL.colors(&TerminalColors::default()),
    ))
});

/// The colors in use: Terminal's, until [`set`].
#[cfg(not(test))]
pub fn colors() -> Arc<Colors> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Puts `colors` in use.
#[cfg(not(test))]
pub fn set(colors: Colors) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(colors);
}

// In tests, each test's thread has its own, as with settings.
#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::RefCell<Arc<Colors>> = std::cell::RefCell::new(Arc::new(
        ThemeId::TERMINAL.colors(&TerminalColors::default()),
    ));
}

/// The colors in use: in tests, those this test set.
#[cfg(test)]
pub fn colors() -> Arc<Colors> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Puts `colors` in use for the rest of this test.
#[cfg(test)]
pub fn set(colors: Colors) {
    CURRENT.with(|current| *current.borrow_mut() = Arc::new(colors));
}

// --- choosing a theme --------------------------------------------------------

/// A theme: Terminal, or one of [`BUILTIN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeId(u8);

impl ThemeId {
    pub const TERMINAL: ThemeId = ThemeId(0);

    /// Every theme, Terminal first.
    pub fn all() -> impl Iterator<Item = ThemeId> {
        (0..=BUILTIN.len() as u8).map(ThemeId)
    }

    /// The theme called `name`, ignoring case, spaces, punctuation, and
    /// accents: `rose-pine` is Rosé Pine.
    pub fn named(name: &str) -> Option<ThemeId> {
        let key = |name: &str| -> String {
            name.chars()
                .map(|c| if c == 'é' || c == 'É' { 'e' } else { c })
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect()
        };
        let wanted = key(name);
        ThemeId::all().find(|id| key(id.name()) == wanted)
    }

    /// Whether it's light, or `None` for Terminal, which is the terminal's.
    pub fn light(self) -> Option<bool> {
        self.def().map(|def| def.light)
    }

    pub fn name(self) -> &'static str {
        match self.def() {
            None => "Terminal",
            Some(def) => def.name,
        }
    }

    fn def(self) -> Option<&'static Def> {
        (self.0 as usize).checked_sub(1).map(|i| &BUILTIN[i])
    }

    /// This theme's colors, on a terminal whose colors are `terminal`.
    pub fn colors(self, terminal: &TerminalColors) -> Colors {
        match self.def() {
            None => {
                let base = Base::of_terminal(terminal);
                let known = terminal.fg.is_some() && terminal.bg.is_some();
                let mut colors = derive("Terminal", &base, &[], &[]);
                colors.terminal = known.then_some(TerminalPalette {
                    fg: base.fg,
                    bg: base.bg,
                    ansi: None,
                });
                colors
            }
            Some(def) => {
                let base = Base {
                    light: def.light,
                    bg: hex(def.bg),
                    fg: hex(def.fg),
                    ansi: def.ansi.map(hex),
                };
                let mut colors = derive(def.name, &base, def.ui, def.syntax);
                colors.terminal = Some(TerminalPalette {
                    fg: base.fg,
                    bg: base.bg,
                    ansi: Some(base.ansi),
                });
                colors
            }
        }
    }
}

/// The `ui.theme` setting: a theme for when the terminal is dark, and one
/// for when it's light. Usually the same one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeSetting {
    pub dark: ThemeId,
    pub light: ThemeId,
}

impl Default for ThemeSetting {
    fn default() -> ThemeSetting {
        ThemeSetting::one(ThemeId::TERMINAL)
    }
}

impl ThemeSetting {
    pub fn one(id: ThemeId) -> ThemeSetting {
        ThemeSetting {
            dark: id,
            light: id,
        }
    }

    /// The theme for a terminal whose colors are `terminal`: the dark one,
    /// unless the terminal says it's light.
    pub fn pick(self, terminal: &TerminalColors) -> ThemeId {
        match terminal.light() {
            Some(true) => self.light,
            _ => self.dark,
        }
    }
}

// --- the terminal's colors ---------------------------------------------------

/// What the terminal said its colors are, if it did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub ansi: [Option<[u8; 3]>; 16],
    /// Whether it last said it switched to light (mode 2031), which goes
    /// for its background, if it did.
    pub switched_light: Option<bool>,
    /// Its background is cue's (OSC 11), so what it says it is isn't its
    /// own: it's not taken in.
    pub background_set: bool,
}

/// Asks the terminal for its colors: text (OSC 10), background (OSC 11), and
/// palette slots 0-15 (OSC 4), then for its status (DSR), which every
/// terminal answers, after the rest: [`ASKED_LAST`].
pub fn color_queries() -> String {
    let mut queries = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for slot in 0..16 {
        queries += &format!("\x1b]4;{slot};?\x07");
    }
    queries + "\x1b[5n"
}

/// The terminal's answer to the last of [`color_queries`]: once it's here,
/// the colors it knows have come.
pub const ASKED_LAST: &[u8] = b"\x1b[0n";

/// Whether `reply` says the terminal switched between dark and light (mode
/// 2031): its colors are worth asking for again.
/// Whether it's light now, if so.
pub fn appearance_change(reply: &[u8]) -> Option<bool> {
    match reply {
        b"\x1b[?997;1n" => Some(false),
        b"\x1b[?997;2n" => Some(true),
        _ => None,
    }
}

/// Sets the terminal's own background to `rgb` (OSC 11), or back to its
/// own without (OSC 111).
pub fn set_background(rgb: Option<[u8; 3]>) -> String {
    match rgb {
        Some([r, g, b]) => format!("\x1b]11;rgb:{r:02x}/{g:02x}/{b:02x}\x07"),
        None => "\x1b]111\x07".to_string(),
    }
}

impl TerminalColors {
    /// Takes in the terminal's answer to one of [`color_queries`]. Returns
    /// false if `reply` isn't one.
    pub fn take_reply(&mut self, reply: &[u8]) -> bool {
        let Some(body) = reply.strip_prefix(b"\x1b]") else {
            return false;
        };
        let body = body
            .strip_suffix(b"\x07")
            .or_else(|| body.strip_suffix(b"\x1b\\"))
            .unwrap_or(body);
        let Ok(body) = std::str::from_utf8(body) else {
            return false;
        };
        let mut fields = body.splitn(3, ';');
        let mut ignored = None;
        let (slot, color) = match (fields.next(), fields.next(), fields.next()) {
            (Some("10"), Some(color), None) => (&mut self.fg, color),
            (Some("11"), Some(color), None) if self.background_set => (&mut ignored, color),
            (Some("11"), Some(color), None) => (&mut self.bg, color),
            (Some("4"), Some(index), Some(color)) => match index.parse::<usize>() {
                Ok(index) if index < 16 => (&mut self.ansi[index], color),
                _ => return false,
            },
            _ => return false,
        };
        match parse_color(color) {
            Some(rgb) => {
                *slot = Some(rgb);
                true
            }
            None => false,
        }
    }

    /// Whether the terminal is dark text on light, if it said what its
    /// background is.
    pub fn light(&self) -> Option<bool> {
        self.switched_light
            .or_else(|| self.bg.map(|bg| brightness(bg) > 0.5))
    }
}

/// An X11 color spec as terminals report them: `rgb:R/G/B` with 1 to 4 hex
/// digits each, or `rgba:R/G/B/A`, or `#RRGGBB`.
fn parse_color(spec: &str) -> Option<[u8; 3]> {
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    let channels = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))?;
    let mut rgb = [0u8; 3];
    let mut parts = channels.split('/');
    for channel in &mut rgb {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len())) - 1;
        *channel = ((value * 255 + max / 2) / max) as u8;
    }
    Some(rgb)
}

// --- working colors out --------------------------------------------------------

/// What a theme's colors are worked out from.
struct Base {
    light: bool,
    bg: Rgba,
    fg: Rgba,
    ansi: [Rgba; 16],
}

impl Base {
    /// The terminal's own colors, as the terminal's: text and background as
    /// its defaults, and its palette's slots. What it didn't say is guessed,
    /// to mix with.
    fn of_terminal(terminal: &TerminalColors) -> Base {
        let light = terminal.light().unwrap_or(false);
        let (bg, fg) = match light {
            true => ([255; 3], [0; 3]),
            false => ([0; 3], [255; 3]),
        };
        Base {
            light,
            bg: Rgba::terminal_default(terminal.bg.unwrap_or(bg)),
            fg: Rgba::terminal_default(terminal.fg.unwrap_or(fg)),
            ansi: std::array::from_fn(|i| match terminal.ansi[i] {
                Some(rgb) => Rgba::indexed_as(i as u8, rgb),
                None => Rgba::indexed(i as u8),
            }),
        }
    }
}

/// Colors a theme can set itself, besides its background, text, and palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Muted,
    Faint,
    Border,
    Divider,
    Surface,
    SurfaceInactive,
    Selected,
    SelectedUnfocused,
    Selection,
    Accent,
    MatchBg,
    CurrentMatchBg,
    CurrentMatchFg,
    Error,
    ErrorBg,
    LineNumberCurrent,
    Cursor,
}

/// The colors of a theme with `base`, and `ui` and `syntax` colors of its
/// own; the rest are mixed from `base`.
fn derive(name: &'static str, base: &Base, ui: &[(Role, u32)], syntax: &[(&str, u32)]) -> Colors {
    let set = |role| ui.iter().find(|&&(r, _)| r == role).map(|&(_, c)| hex(c));
    let Base { bg, fg, ansi, .. } = *base;
    let tint = |share| mix(bg, fg, share);
    let accent = set(Role::Accent).unwrap_or(ansi[4]);
    let surface = set(Role::Surface).unwrap_or(tint(0.1));
    let current_match_bg = set(Role::CurrentMatchBg).unwrap_or(ansi[3]);
    let muted = set(Role::Muted).unwrap_or(tint(0.66));
    let syntax = SYNTAX
        .iter()
        .map(|&(capture, slot, attributes)| {
            match syntax.iter().find(|&&(c, _)| c == capture) {
                // Its own color, rather than the text's dimmed.
                Some(&(_, color)) => (
                    Some(hex(color)),
                    Attributes(attributes.0 & !Attributes::DIM.0),
                ),
                None => (slot.map(|slot| ansi[slot as usize]), attributes),
            }
        })
        .collect();
    let mut hues = [muted; HUES];
    for (hue, color) in [
        (Hue::Red, ansi[1]),
        (Hue::Peach, mix(ansi[1], ansi[3], 0.5)),
        (Hue::Yellow, ansi[3]),
        (Hue::Green, ansi[2]),
        (Hue::Teal, ansi[6]),
        (Hue::Sky, mix(ansi[6], fg, 0.25)),
        (Hue::Sapphire, mix(ansi[4], ansi[6], 0.5)),
        (Hue::Blue, ansi[4]),
        (Hue::Lavender, mix(ansi[4], ansi[5], 0.4)),
        (Hue::Mauve, ansi[5]),
        (Hue::Pink, mix(ansi[5], fg, 0.3)),
    ] {
        hues[hue as usize] = color;
    }
    Colors {
        name,
        light: base.light,
        bg,
        text: fg,
        muted,
        faint: set(Role::Faint).unwrap_or(tint(0.4)),
        border: set(Role::Border).unwrap_or(tint(0.28)),
        divider: set(Role::Divider).unwrap_or(tint(0.18)),
        surface,
        surface_inactive: set(Role::SurfaceInactive).unwrap_or(tint(0.05)),
        selected: set(Role::Selected).unwrap_or(mix(bg, accent, 0.3)),
        selected_unfocused: set(Role::SelectedUnfocused).unwrap_or(surface),
        selection: set(Role::Selection).unwrap_or(mix(bg, accent, 0.3)),
        accent,
        on_accent: legible_on(accent, bg, fg),
        match_bg: set(Role::MatchBg).unwrap_or(mix(bg, ansi[3], 0.3)),
        current_match_bg,
        current_match_fg: set(Role::CurrentMatchFg).unwrap_or(legible_on(current_match_bg, bg, fg)),
        error: set(Role::Error).unwrap_or(ansi[1]),
        error_bg: set(Role::ErrorBg).unwrap_or(mix(bg, ansi[1], 0.55)),
        line_number_current: set(Role::LineNumberCurrent).unwrap_or(fg),
        cursor: set(Role::Cursor).unwrap_or(fg),
        drop_tint: with_alpha(accent, 56),
        terminal: None,
        hues,
        syntax,
    }
}

fn hex(rgb: u32) -> Rgba {
    Rgba::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// `a` with `share` of `b` mixed in, as RGB.
pub fn mix(a: Rgba, b: Rgba, share: f32) -> Rgba {
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * share).round() as u8;
    Rgba::rgb(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
    )
}

fn with_alpha(color: Rgba, alpha: u8) -> Rgba {
    Rgba::rgba(color.r(), color.g(), color.b(), alpha)
}

/// Whichever of `a` and `b` reads better on `bg`, as RGB.
fn legible_on(bg: Rgba, a: Rgba, b: Rgba) -> Rgba {
    let rgb = |c: Rgba| [c.r(), c.g(), c.b()];
    let level = brightness(rgb(bg));
    let pick = match (brightness(rgb(a)) - level).abs() >= (brightness(rgb(b)) - level).abs() {
        true => a,
        false => b,
    };
    Rgba::rgb(pick.r(), pick.g(), pick.b())
}

/// How bright `rgb` looks, from 0 to 1.
fn brightness([r, g, b]: [u8; 3]) -> f32 {
    (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
}

// --- the themes ------------------------------------------------------------------

/// A theme cue comes with.
struct Def {
    name: &'static str,
    light: bool,
    bg: u32,
    fg: u32,
    /// Its terminal palette, which colors what it doesn't set itself.
    ansi: [u32; 16],
    ui: &'static [(Role, u32)],
    /// Colors by [`SYNTAX`] capture.
    syntax: &'static [(&'static str, u32)],
}

use Role::*;

/// The themes besides Terminal, as they're listed.
static BUILTIN: &[Def] = &[
    // Zed's Fleet Dark, as cue's author has it, with find's matches easier
    // to tell from the one it's on.
    Def {
        name: "Cue Dark",
        light: false,
        bg: 0x17171a,
        fg: 0xdde3f0,
        ansi: [
            0x4a4b4d, 0xeb4056, 0x82d2ce, 0xfad075, 0x4b8dec, 0xeb83e2, 0x2ccce6, 0xe0e1e4,
            0x4a4b4d, 0xc7001b, 0x009e6f, 0xe09119, 0x0067e6, 0xeb4dde, 0x00bad6, 0x9d9d9f,
        ],
        ui: &[
            (Muted, 0xa6a9ba),
            (Faint, 0x6e747b),
            (Border, 0x3e4147),
            (Divider, 0x2b2c31),
            (Surface, 0x212126),
            (SurfaceInactive, 0x141417),
            (Selected, 0x1d3a66),
            (SelectedUnfocused, 0x2a2b30),
            (Selection, 0x225090),
            (Accent, 0x8f9eff),
            (MatchBg, 0x4a3a1a),
            (CurrentMatchBg, 0xf5c451),
            (CurrentMatchFg, 0x17171a),
            (Error, 0xe1465e),
            (ErrorBg, 0x682b36),
            (LineNumberCurrent, 0xe0e1e4),
            (Cursor, 0xe0e1e4),
        ],
        syntax: &[
            ("attribute", 0xaf9cff),
            ("boolean", 0x82d2ce),
            ("character", 0xb0de92),
            ("comment", 0x68696e),
            ("constant", 0xaf9cff),
            ("constructor", 0x87c3ff),
            ("diff.minus", 0xf87c88),
            ("diff.plus", 0x69b090),
            ("escape", 0x82d2ce),
            ("function", 0xffc882),
            ("function.macro", 0xa8cc7c),
            ("keyword", 0xed705c),
            ("label", 0x87c3ff),
            ("number", 0xf288db),
            ("operator", 0xa6b0ba),
            ("property", 0x87c3ff),
            ("string", 0xb0de92),
            ("string.escape", 0x82d2ce),
            ("tag", 0xffe194),
            ("text.list", 0xed705c),
            ("text.literal", 0xe394dc),
            ("text.reference", 0x4b8dec),
            ("text.title", 0x82d2ce),
            ("text.uri", 0x4b8dec),
            ("type", 0xaf9cff),
            ("variable.builtin", 0xcad1db),
        ],
    },
    Def {
        name: "Ayu Dark",
        light: false,
        bg: 0x0d1017,
        fg: 0xbfbdb6,
        ansi: [
            0x11151c, 0xea6c73, 0x7fd962, 0xf9af4f, 0x53bdfa, 0xcda1fa, 0x90e1c6, 0xc7c7c7,
            0x686868, 0xf07178, 0xaad94c, 0xffb454, 0x59c2ff, 0xd2a6ff, 0x95e6cb, 0xffffff,
        ],
        ui: &[
            (Faint, 0x545a65),
            (Selection, 0x1c3b5d),
            (Accent, 0xe6b450),
            (MatchBg, 0x332d41),
            (CurrentMatchBg, 0xe6b450),
            (Error, 0xd95757),
            (Cursor, 0xe6b450),
        ],
        syntax: &[
            ("attribute", 0xe6c08a),
            ("boolean", 0xd2a6ff),
            ("character", 0x95e6cb),
            ("comment", 0x646b73),
            ("constant", 0xd2a6ff),
            ("constructor", 0x59c2ff),
            ("escape", 0x95e6cb),
            ("function", 0xffb454),
            ("function.macro", 0xe6c08a),
            ("keyword", 0xff8f40),
            ("label", 0x39bae6),
            ("number", 0xd2a6ff),
            ("operator", 0xf29668),
            ("property", 0xf07178),
            ("string", 0xaad94c),
            ("string.escape", 0x95e6cb),
            ("tag", 0x39bae6),
            ("text.literal", 0xaad94c),
            ("text.reference", 0x39bae6),
            ("text.title", 0xf07178),
            ("text.uri", 0x39bae6),
            ("type", 0x59c2ff),
            ("variable.builtin", 0x39bae6),
            ("variable.parameter", 0xd2a6ff),
        ],
    },
    Def {
        name: "Ayu Mirage",
        light: false,
        bg: 0x1f2430,
        fg: 0xcccac2,
        ansi: [
            0x191e2a, 0xed8274, 0xa6cc70, 0xfad07b, 0x6dcbfa, 0xcfbafa, 0x90e1c6, 0xc7c7c7,
            0x686868, 0xf28779, 0xbae67e, 0xffd580, 0x73d0ff, 0xd4bfff, 0x95e6cb, 0xffffff,
        ],
        ui: &[
            (Faint, 0x535a66),
            (Selection, 0x274364),
            (Accent, 0xffcc66),
            (MatchBg, 0x3d3750),
            (CurrentMatchBg, 0xffcc66),
            (Error, 0xff6666),
            (Cursor, 0xffcc66),
        ],
        syntax: &[
            ("attribute", 0xffdfb3),
            ("boolean", 0xdfbfff),
            ("character", 0x95e6cb),
            ("comment", 0x6b798b),
            ("constant", 0xdfbfff),
            ("constructor", 0x73d0ff),
            ("escape", 0x95e6cb),
            ("function", 0xffd173),
            ("function.macro", 0xffdfb3),
            ("keyword", 0xffad66),
            ("label", 0x5ccfe6),
            ("number", 0xdfbfff),
            ("operator", 0xf29e74),
            ("property", 0xf28779),
            ("string", 0xd5ff80),
            ("string.escape", 0x95e6cb),
            ("tag", 0x5ccfe6),
            ("text.literal", 0xd5ff80),
            ("text.reference", 0x5ccfe6),
            ("text.title", 0xf28779),
            ("text.uri", 0x5ccfe6),
            ("type", 0x73d0ff),
            ("variable.builtin", 0x5ccfe6),
            ("variable.parameter", 0xdfbfff),
        ],
    },
    Def {
        name: "Ayu Light",
        light: true,
        bg: 0xfcfcfc,
        fg: 0x5c6166,
        ansi: [
            0x000000, 0xea6c6d, 0x6cbf43, 0xeca944, 0x3199e1, 0x9e75c7, 0x46ba94, 0xbababa,
            0x686868, 0xf07171, 0x86b300, 0xf2ae49, 0x399ee6, 0xa37acc, 0x4cbf99, 0xd1d1d1,
        ],
        ui: &[
            (Faint, 0xa9acb0),
            (Selection, 0xd7e4f6),
            (Accent, 0xfa8d3e),
            (MatchBg, 0xecdcfc),
            (CurrentMatchBg, 0xffaa33),
            (Error, 0xe65050),
            (Cursor, 0xffaa33),
        ],
        syntax: &[
            ("attribute", 0xe6ba7e),
            ("boolean", 0xa37acc),
            ("character", 0x4cbf99),
            ("comment", 0xadaeb1),
            ("constant", 0xa37acc),
            ("constructor", 0x399ee6),
            ("escape", 0x4cbf99),
            ("function", 0xf2ae49),
            ("function.macro", 0xe6ba7e),
            ("keyword", 0xfa8d3e),
            ("label", 0x55b4d4),
            ("number", 0xa37acc),
            ("operator", 0xed9366),
            ("property", 0xf07171),
            ("string", 0x86b300),
            ("string.escape", 0x4cbf99),
            ("tag", 0x55b4d4),
            ("text.literal", 0x86b300),
            ("text.reference", 0x55b4d4),
            ("text.title", 0xf07171),
            ("text.uri", 0x55b4d4),
            ("type", 0x399ee6),
            ("variable.builtin", 0x55b4d4),
            ("variable.parameter", 0xa37acc),
        ],
    },
    Def {
        name: "Catppuccin Latte",
        light: true,
        bg: 0xeff1f5,
        fg: 0x4c4f69,
        ansi: [
            0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
            0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
        ],
        ui: &[
            (Muted, 0x7c7f93),
            (Faint, 0x8c8fa1),
            (Border, 0xacb0be),
            (Divider, 0xbcc0cc),
            (Surface, 0xccd0da),
            (SurfaceInactive, 0xe6e9ef),
            (SelectedUnfocused, 0xccd0da),
            (Accent, 0x1e66f5),
            (Error, 0xd20f39),
            (LineNumberCurrent, 0x7287fd),
            (Cursor, 0xdc8a78),
        ],
        syntax: &[
            ("attribute", 0xdf8e1d),
            ("boolean", 0xfe640b),
            ("character", 0x179299),
            ("comment", 0x7c7f93),
            ("constant", 0xfe640b),
            ("constructor", 0x209fb5),
            ("escape", 0xea76cb),
            ("function", 0x1e66f5),
            ("function.macro", 0x179299),
            ("keyword", 0x8839ef),
            ("label", 0x209fb5),
            ("number", 0xfe640b),
            ("operator", 0x04a5e5),
            ("property", 0x7287fd),
            ("string", 0x40a02b),
            ("string.escape", 0xea76cb),
            ("tag", 0x8839ef),
            ("text.reference", 0x7287fd),
            ("text.uri", 0xdc8a78),
            ("type", 0xdf8e1d),
            ("variable.builtin", 0xd20f39),
            ("variable.parameter", 0xe64553),
        ],
    },
    // The colors cue had before it had themes.
    Def {
        name: "Catppuccin Mocha",
        light: false,
        bg: 0x1e1e2e,
        fg: 0xcdd6f4,
        ansi: [
            0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
            0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
        ],
        ui: &[
            (Muted, 0x9399b2),
            (Faint, 0x6c7086),
            (Border, 0x585b70),
            (Divider, 0x45475a),
            (Surface, 0x313244),
            (SurfaceInactive, 0x242534),
            (Selected, 0x45476e),
            (SelectedUnfocused, 0x313244),
            (Selection, 0x45476e),
            (Accent, 0x89b4fa),
            (MatchBg, 0x5f5537),
            (CurrentMatchBg, 0xf9e2af),
            (Error, 0xf38ba8),
            (ErrorBg, 0xb43c50),
            (LineNumberCurrent, 0xb4befe),
            (Cursor, 0xf5e0dc),
        ],
        syntax: &[
            ("attribute", 0xf9e2af),
            ("boolean", 0xfab387),
            ("character", 0x94e2d5),
            ("comment", 0x9399b2),
            ("constant", 0xfab387),
            ("constructor", 0x74c7ec),
            ("escape", 0xf5c2e7),
            ("function", 0x89b4fa),
            ("function.macro", 0x94e2d5),
            ("keyword", 0xcba6f7),
            ("label", 0x74c7ec),
            ("number", 0xfab387),
            ("operator", 0x89dceb),
            ("property", 0xb4befe),
            ("string", 0xa6e3a1),
            ("string.escape", 0xf5c2e7),
            ("tag", 0xcba6f7),
            ("text.reference", 0xb4befe),
            ("text.uri", 0xf5e0dc),
            ("type", 0xf9e2af),
            ("variable.builtin", 0xf38ba8),
            ("variable.parameter", 0xeba0ac),
        ],
    },
    Def {
        name: "Dracula",
        light: false,
        bg: 0x282a36,
        fg: 0xf8f8f2,
        ansi: [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ],
        ui: &[
            (Faint, 0x6272a4),
            (Border, 0x6272a4),
            (Divider, 0x191a21),
            (Surface, 0x44475a),
            (SurfaceInactive, 0x343746),
            (Selected, 0x44475a),
            (SelectedUnfocused, 0x343746),
            (Selection, 0x44475a),
            (Accent, 0xbd93f9),
            (MatchBg, 0x695546),
            (CurrentMatchBg, 0xffb86c),
            (Error, 0xff5555),
        ],
        syntax: &[
            ("attribute", 0x50fa7b),
            ("boolean", 0xbd93f9),
            ("character", 0xf1fa8c),
            ("comment", 0x6272a4),
            ("constant", 0xbd93f9),
            ("constructor", 0x8be9fd),
            ("escape", 0xff79c6),
            ("function", 0x50fa7b),
            ("function.macro", 0x8be9fd),
            ("keyword", 0xff79c6),
            ("label", 0x8be9fd),
            ("number", 0xbd93f9),
            ("operator", 0xff79c6),
            ("string", 0xf1fa8c),
            ("string.escape", 0xff79c6),
            ("tag", 0xff79c6),
            ("text.list", 0x8be9fd),
            ("text.literal", 0x50fa7b),
            ("text.reference", 0x8be9fd),
            ("text.title", 0xbd93f9),
            ("text.uri", 0x8be9fd),
            ("type", 0x8be9fd),
            ("variable.builtin", 0xbd93f9),
            ("variable.parameter", 0xffb86c),
        ],
    },
    Def {
        name: "GitHub Dark Default",
        light: false,
        bg: 0x0d1117,
        fg: 0xe6edf3,
        ansi: [
            0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
            0x6e7681, 0xffa198, 0x56d364, 0xe3b341, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xffffff,
        ],
        ui: &[
            (Muted, 0x7d8590),
            (Faint, 0x6e7681),
            (Border, 0x30363d),
            (Divider, 0x21262d),
            (Surface, 0x21262d),
            (SurfaceInactive, 0x161b22),
            (SelectedUnfocused, 0x21262d),
            (Accent, 0x58a6ff),
            (CurrentMatchBg, 0xe3b341),
            (Error, 0xf85149),
            (Cursor, 0x2f81f7),
        ],
        syntax: &[
            ("attribute", 0xd2a8ff),
            ("boolean", 0x79c0ff),
            ("character", 0xa5d6ff),
            ("comment", 0x8b949e),
            ("constant", 0x79c0ff),
            ("constructor", 0xffa657),
            ("escape", 0x79c0ff),
            ("function", 0xd2a8ff),
            ("function.macro", 0xd2a8ff),
            ("keyword", 0xff7b72),
            ("label", 0x79c0ff),
            ("number", 0x79c0ff),
            ("operator", 0xff7b72),
            ("property", 0x79c0ff),
            ("string", 0xa5d6ff),
            ("string.escape", 0x79c0ff),
            ("tag", 0x7ee787),
            ("text.literal", 0x79c0ff),
            ("text.reference", 0xa5d6ff),
            ("text.title", 0x79c0ff),
            ("text.uri", 0xa5d6ff),
            ("type", 0xffa657),
            ("variable.builtin", 0x79c0ff),
        ],
    },
    Def {
        name: "GitHub Dark Dimmed",
        light: false,
        bg: 0x22272e,
        fg: 0xadbac7,
        ansi: [
            0x545d68, 0xf47067, 0x57ab5a, 0xc69026, 0x539bf5, 0xb083f0, 0x39c5cf, 0x909dab,
            0x636e7b, 0xff938a, 0x6bc46d, 0xdaaa3f, 0x6cb6ff, 0xdcbdfb, 0x56d4dd, 0xcdd9e5,
        ],
        ui: &[
            (Muted, 0x768390),
            (Faint, 0x636e7b),
            (Border, 0x444c56),
            (Divider, 0x373e47),
            (Surface, 0x373e47),
            (SurfaceInactive, 0x2d333b),
            (SelectedUnfocused, 0x2d333b),
            (Accent, 0x539bf5),
            (CurrentMatchBg, 0xdaaa3f),
            (Error, 0xe5534b),
            (Cursor, 0x539bf5),
        ],
        syntax: &[
            ("attribute", 0xdcbdfb),
            ("boolean", 0x6cb6ff),
            ("character", 0x96d0ff),
            ("comment", 0x768390),
            ("constant", 0x6cb6ff),
            ("constructor", 0xf69d50),
            ("escape", 0x8ddb8c),
            ("function", 0xdcbdfb),
            ("function.macro", 0xdcbdfb),
            ("keyword", 0xf47067),
            ("label", 0x6cb6ff),
            ("number", 0x6cb6ff),
            ("operator", 0xf47067),
            ("property", 0x6cb6ff),
            ("string", 0x96d0ff),
            ("string.escape", 0x8ddb8c),
            ("tag", 0x8ddb8c),
            ("text.literal", 0x6cb6ff),
            ("text.reference", 0x96d0ff),
            ("text.title", 0x6cb6ff),
            ("text.uri", 0x96d0ff),
            ("type", 0xf69d50),
            ("variable.builtin", 0x6cb6ff),
        ],
    },
    Def {
        name: "GitHub Light",
        light: true,
        bg: 0xffffff,
        fg: 0x1f2328,
        ansi: [
            0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
            0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
        ],
        ui: &[
            (Muted, 0x656d76),
            (Faint, 0x8c959f),
            (Border, 0xd0d7de),
            (Divider, 0xd8dee4),
            (Surface, 0xeaeef2),
            (SurfaceInactive, 0xf6f8fa),
            (Selected, 0xddf4ff),
            (SelectedUnfocused, 0xeaeef2),
            (Selection, 0xcee1f8),
            (Accent, 0x0969da),
            (MatchBg, 0xfff8c5),
            (CurrentMatchBg, 0xd4a72c),
            (Error, 0xcf222e),
            (ErrorBg, 0xffcecb),
            (Cursor, 0x0969da),
        ],
        syntax: &[
            ("attribute", 0x8250df),
            ("boolean", 0x0550ae),
            ("character", 0x0a3069),
            ("comment", 0x6e7781),
            ("constant", 0x0550ae),
            ("constructor", 0x953800),
            ("escape", 0x116329),
            ("function", 0x8250df),
            ("function.macro", 0x8250df),
            ("keyword", 0xcf222e),
            ("label", 0x0550ae),
            ("number", 0x0550ae),
            ("operator", 0xcf222e),
            ("property", 0x0550ae),
            ("string", 0x0a3069),
            ("string.escape", 0x116329),
            ("tag", 0x116329),
            ("text.literal", 0x0550ae),
            ("text.reference", 0x0a3069),
            ("text.title", 0x0550ae),
            ("text.uri", 0x0a3069),
            ("type", 0x953800),
            ("variable.builtin", 0x0550ae),
        ],
    },
    Def {
        name: "Gruvbox Dark",
        light: false,
        bg: 0x282828,
        fg: 0xebdbb2,
        ansi: [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ],
        ui: &[
            (Muted, 0xa89984),
            (Faint, 0x7c6f64),
            (Border, 0x665c54),
            (Divider, 0x3c3836),
            (Surface, 0x504945),
            (SurfaceInactive, 0x32302f),
            (Selected, 0x504945),
            (SelectedUnfocused, 0x3c3836),
            (Selection, 0x665c54),
            (Accent, 0x83a598),
            (CurrentMatchBg, 0xfe8019),
            (Error, 0xfb4934),
            (ErrorBg, 0x9d0006),
            (LineNumberCurrent, 0xfabd2f),
        ],
        syntax: &[
            ("attribute", 0x8ec07c),
            ("boolean", 0xd3869b),
            ("character", 0xd3869b),
            ("comment", 0x928374),
            ("constant", 0xd3869b),
            ("constructor", 0x8ec07c),
            ("escape", 0xfe8019),
            ("function", 0xb8bb26),
            ("function.macro", 0x8ec07c),
            ("keyword", 0xfb4934),
            ("label", 0xfb4934),
            ("number", 0xd3869b),
            ("operator", 0xfe8019),
            ("property", 0x83a598),
            ("string", 0xb8bb26),
            ("string.escape", 0xfe8019),
            ("tag", 0x8ec07c),
            ("text.literal", 0x8ec07c),
            ("text.reference", 0x83a598),
            ("text.title", 0xb8bb26),
            ("text.uri", 0x83a598),
            ("type", 0xfabd2f),
            ("variable.builtin", 0xfe8019),
            ("variable.parameter", 0x83a598),
        ],
    },
    // Kanagawa Wave.
    Def {
        name: "Kanagawa",
        light: false,
        bg: 0x1f1f28,
        fg: 0xdcd7ba,
        ansi: [
            0x16161d, 0xc34043, 0x76946a, 0xc0a36e, 0x7e9cd8, 0x957fb8, 0x6a9589, 0xc8c093,
            0x727169, 0xe82424, 0x98bb6c, 0xe6c384, 0x7fb4ca, 0x938aa9, 0x7aa89f, 0xdcd7ba,
        ],
        ui: &[
            (Faint, 0x54546d),
            (Border, 0x54546d),
            (Divider, 0x16161d),
            (Surface, 0x2a2a37),
            (SurfaceInactive, 0x1a1a22),
            (Selected, 0x2d4f67),
            (SelectedUnfocused, 0x2a2a37),
            (Selection, 0x223249),
            (Accent, 0x7e9cd8),
            (MatchBg, 0x2d4f67),
            (CurrentMatchBg, 0xff9e3b),
            (Error, 0xe82424),
            (ErrorBg, 0x43242b),
            (LineNumberCurrent, 0xff9e3b),
            (Cursor, 0xc8c093),
        ],
        syntax: &[
            ("attribute", 0xe6c384),
            ("boolean", 0xffa066),
            ("character", 0x98bb6c),
            ("comment", 0x727169),
            ("constant", 0xffa066),
            ("constructor", 0x7fb4ca),
            ("escape", 0xc0a36e),
            ("function", 0x7e9cd8),
            ("function.macro", 0xe46876),
            ("keyword", 0x957fb8),
            ("label", 0x7fb4ca),
            ("number", 0xd27e99),
            ("operator", 0xc0a36e),
            ("property", 0xe6c384),
            ("string", 0x98bb6c),
            ("string.escape", 0xc0a36e),
            ("tag", 0x7fb4ca),
            ("text.literal", 0x98bb6c),
            ("text.reference", 0x7fb4ca),
            ("text.title", 0x7e9cd8),
            ("text.uri", 0x7fb4ca),
            ("type", 0x7aa89f),
            ("variable.builtin", 0xe46876),
            ("variable.parameter", 0xb8b4d0),
        ],
    },
    Def {
        name: "Nord",
        light: false,
        bg: 0x2e3440,
        fg: 0xd8dee9,
        ansi: [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ],
        ui: &[
            (Faint, 0x4c566a),
            (Border, 0x4c566a),
            (Divider, 0x3b4252),
            (Surface, 0x3b4252),
            (SurfaceInactive, 0x353b49),
            (Selected, 0x434c5e),
            (SelectedUnfocused, 0x3b4252),
            (Selection, 0x434c5e),
            (Accent, 0x88c0d0),
            (Error, 0xbf616a),
        ],
        syntax: &[
            ("attribute", 0xd08770),
            ("boolean", 0x81a1c1),
            ("character", 0xebcb8b),
            ("comment", 0x616e88),
            ("constant", 0xb48ead),
            ("constructor", 0x8fbcbb),
            ("escape", 0xebcb8b),
            ("function", 0x88c0d0),
            ("function.macro", 0x5e81ac),
            ("keyword", 0x81a1c1),
            ("label", 0x81a1c1),
            ("number", 0xb48ead),
            ("operator", 0x81a1c1),
            ("string", 0xa3be8c),
            ("string.escape", 0xebcb8b),
            ("tag", 0x81a1c1),
            ("text.literal", 0xa3be8c),
            ("text.reference", 0x88c0d0),
            ("text.title", 0x88c0d0),
            ("text.uri", 0x88c0d0),
            ("type", 0x8fbcbb),
            ("variable.builtin", 0x81a1c1),
        ],
    },
    Def {
        name: "One Dark",
        light: false,
        bg: 0x282c34,
        fg: 0xabb2bf,
        ansi: [
            0x3f4451, 0xe05561, 0x8cc265, 0xd18f52, 0x4aa5f0, 0xc162de, 0x42b3c2, 0xd7dae0,
            0x4f5666, 0xff616e, 0xa5e075, 0xf0a45d, 0x4dc4ff, 0xde73ff, 0x4cd1e0, 0xe6e6e6,
        ],
        ui: &[
            (Muted, 0x7f848e),
            (Faint, 0x495162),
            (Border, 0x3e4452),
            (Divider, 0x181a1f),
            (Surface, 0x2c313c),
            (SurfaceInactive, 0x21252b),
            (Selected, 0x3e4452),
            (SelectedUnfocused, 0x2c313a),
            (Selection, 0x3e4451),
            (Accent, 0x61afef),
            (MatchBg, 0x314365),
            (CurrentMatchBg, 0xe5c07b),
            (Error, 0xe06c75),
            (ErrorBg, 0xbe5046),
            (Cursor, 0x528bff),
        ],
        syntax: &[
            ("attribute", 0xd19a66),
            ("boolean", 0xd19a66),
            ("character", 0x98c379),
            ("comment", 0x5c6370),
            ("constant", 0xd19a66),
            ("constructor", 0xe5c07b),
            ("escape", 0x56b6c2),
            ("function", 0x61afef),
            ("function.macro", 0x56b6c2),
            ("keyword", 0xc678dd),
            ("label", 0xe06c75),
            ("number", 0xd19a66),
            ("operator", 0x56b6c2),
            ("property", 0xe06c75),
            ("string", 0x98c379),
            ("string.escape", 0x56b6c2),
            ("tag", 0xe06c75),
            ("text.list", 0xe06c75),
            ("text.literal", 0x98c379),
            ("text.reference", 0x61afef),
            ("text.title", 0xe06c75),
            ("text.uri", 0x56b6c2),
            ("type", 0xe5c07b),
            ("variable.builtin", 0xe5c07b),
        ],
    },
    Def {
        name: "Rosé Pine",
        light: false,
        bg: 0x191724,
        fg: 0xe0def4,
        ansi: [
            0x26233a, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
            0x6e6a86, 0xeb6f92, 0x31748f, 0xf6c177, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xe0def4,
        ],
        ui: &[
            (Muted, 0x908caa),
            (Faint, 0x6e6a86),
            (Border, 0x524f67),
            (Divider, 0x26233a),
            (Surface, 0x26233a),
            (SurfaceInactive, 0x1f1d2e),
            (Selected, 0x403d52),
            (SelectedUnfocused, 0x21202e),
            (Selection, 0x403d52),
            (Accent, 0xebbcba),
            (CurrentMatchBg, 0xf6c177),
            (Error, 0xeb6f92),
        ],
        syntax: &[
            ("attribute", 0xc4a7e7),
            ("boolean", 0xebbcba),
            ("character", 0xf6c177),
            ("comment", 0x6e6a86),
            ("constant", 0xf6c177),
            ("constructor", 0x9ccfd8),
            ("escape", 0x31748f),
            ("function", 0xebbcba),
            ("function.macro", 0xc4a7e7),
            ("keyword", 0x31748f),
            ("label", 0x9ccfd8),
            ("number", 0xf6c177),
            ("operator", 0x908caa),
            ("property", 0x9ccfd8),
            ("string", 0xf6c177),
            ("string.escape", 0x31748f),
            ("tag", 0x9ccfd8),
            ("text.literal", 0xf6c177),
            ("text.reference", 0x9ccfd8),
            ("text.title", 0xc4a7e7),
            ("text.uri", 0xc4a7e7),
            ("type", 0x9ccfd8),
            ("variable.builtin", 0xeb6f92),
            ("variable.parameter", 0xc4a7e7),
        ],
    },
    // Tokyo Night's Night.
    Def {
        name: "Tokyo Night",
        light: false,
        bg: 0x1a1b26,
        fg: 0xc0caf5,
        ansi: [
            0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
        ],
        ui: &[
            (Muted, 0x737aa2),
            (Faint, 0x3b4261),
            (Border, 0x545c7e),
            (Divider, 0x15161e),
            (Surface, 0x292e42),
            (SurfaceInactive, 0x1f2231),
            (Selected, 0x2e3c64),
            (SelectedUnfocused, 0x292e42),
            (Selection, 0x283457),
            (Accent, 0x7aa2f7),
            (MatchBg, 0x3d59a1),
            (CurrentMatchBg, 0xff9e64),
            (Error, 0xf7768e),
            (LineNumberCurrent, 0x737aa2),
        ],
        syntax: &[
            ("attribute", 0xbb9af7),
            ("boolean", 0xff9e64),
            ("character", 0x9ece6a),
            ("comment", 0x565f89),
            ("constant", 0xff9e64),
            ("constructor", 0x2ac3de),
            ("escape", 0xbb9af7),
            ("function", 0x7aa2f7),
            ("function.macro", 0x7dcfff),
            ("keyword", 0xbb9af7),
            ("label", 0x7aa2f7),
            ("number", 0xff9e64),
            ("operator", 0x89ddff),
            ("property", 0x73daca),
            ("string", 0x9ece6a),
            ("string.escape", 0xbb9af7),
            ("tag", 0xf7768e),
            ("text.literal", 0x9ece6a),
            ("text.reference", 0x7aa2f7),
            ("text.title", 0x7aa2f7),
            ("text.uri", 0x73daca),
            ("type", 0x2ac3de),
            ("variable.builtin", 0xf7768e),
            ("variable.parameter", 0xe0af68),
        ],
    },
];

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
        // Aliases, and what refines them.
        assert_eq!(
            theme.capture_style("repeat"),
            theme.capture_style("keyword")
        );
        assert_eq!(
            theme.capture_style("markup.heading.2"),
            theme.capture_style("text.title")
        );
        assert_eq!(
            theme.capture_style("markup.link.url"),
            theme.capture_style("text.uri")
        );
        assert_eq!(
            theme.capture_style("variable.member"),
            theme.capture_style("property")
        );
        assert_eq!(theme.capture_style("functional"), None);
    }

    #[test]
    fn every_capture_a_theme_colors_is_one() {
        for def in BUILTIN {
            for &(capture, _) in def.syntax {
                assert!(
                    SYNTAX.iter().any(|&(c, _, _)| c == capture),
                    "{}: {capture}",
                    def.name
                );
            }
            for (i, &(role, _)) in def.ui.iter().enumerate() {
                assert!(
                    !def.ui[..i].iter().any(|&(r, _)| r == role),
                    "{} sets {role:?} twice",
                    def.name
                );
            }
        }
    }

    #[test]
    fn themes_by_name() {
        assert_eq!(ThemeId::named("Terminal"), Some(ThemeId::TERMINAL));
        let rose = ThemeId::named("rose-pine").unwrap();
        assert_eq!(rose.name(), "Rosé Pine");
        assert_eq!(ThemeId::named("Rosé Pine"), Some(rose));
        assert_eq!(
            ThemeId::named("github_dark_dimmed").map(ThemeId::name),
            Some("GitHub Dark Dimmed")
        );
        assert_eq!(ThemeId::named("Solarized"), None);
        let names: Vec<&str> = ThemeId::all().map(ThemeId::name).collect();
        assert_eq!(names.len(), BUILTIN.len() + 1);
        for name in &names {
            assert_eq!(ThemeId::named(name).map(ThemeId::name), Some(*name));
        }
    }

    #[test]
    fn terminal_replies() {
        let mut colors = TerminalColors::default();
        assert!(colors.take_reply(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07"));
        assert!(colors.take_reply(b"\x1b]10;rgb:cd/d6/f4\x1b\\"));
        assert!(colors.take_reply(b"\x1b]4;1;rgb:f3f3/8b8b/a8a8\x07"));
        assert!(colors.take_reply(b"\x1b]4;12;#89b4fa\x07"));
        assert!(!colors.take_reply(b"\x1b]4;200;rgb:0/0/0\x07"));
        assert!(!colors.take_reply(b"\x1b]52;c;aGk=\x07"));
        assert!(!colors.take_reply(b"\x1b[0n"));
        assert_eq!(colors.bg, Some([0x1e, 0x1e, 0x2e]));
        assert_eq!(colors.fg, Some([0xcd, 0xd6, 0xf4]));
        assert_eq!(colors.ansi[1], Some([0xf3, 0x8b, 0xa8]));
        assert_eq!(colors.ansi[12], Some([0x89, 0xb4, 0xfa]));
        assert_eq!(colors.light(), Some(false));
        assert_eq!(parse_color("rgb:f/0/8"), Some([255, 0, 136]));
        assert_eq!(parse_color("rgba:ffff/ffff/ffff/ffff"), Some([255; 3]));
        assert_eq!(parse_color("rgb:12345/0/0"), None);
        assert_eq!(appearance_change(b"\x1b[?997;2n"), Some(true));
        assert_eq!(appearance_change(b"\x1b[0n"), None);
        // While cue sets the background, the terminal's answer is cue's.
        colors.background_set = true;
        assert!(colors.take_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x07"));
        assert_eq!(colors.bg, Some([0x1e, 0x1e, 0x2e]));
        // Said to be light, it is, whatever its background was.
        assert_eq!(colors.light(), Some(false));
        colors.switched_light = Some(true);
        assert_eq!(colors.light(), Some(true));
        assert_eq!(
            set_background(Some([1, 2, 255])),
            "\x1b]11;rgb:01/02/ff\x07"
        );
    }

    /// Terminal colors everything with the terminal's colors, or colors
    /// mixed from them.
    #[test]
    fn terminal_is_the_terminals_colors() {
        let mut reported = TerminalColors {
            bg: Some([250, 250, 250]),
            fg: Some([20, 20, 20]),
            ..TerminalColors::default()
        };
        reported.ansi[3] = Some([200, 150, 0]);
        let colors = ThemeId::TERMINAL.colors(&reported);
        assert!(colors.light);
        assert!(colors.bg.is_terminal_default());
        assert!(colors.text.is_terminal_default());
        assert_eq!(colors.current_match_bg, Rgba::indexed_as(3, [200, 150, 0]));
        assert_eq!(colors.accent.palette_index(), Some(4));
        // Mixed: between the text and the background.
        assert!(colors.muted.r() > 20 && colors.muted.r() < 250);
        assert!(colors.surface.r() < 250, "a little darker on light");
        // Dark text reads on the yellow.
        assert_eq!(colors.current_match_fg, Rgba::rgb(20, 20, 20));
        let keyword = SyntaxColor::of("keyword").unwrap();
        assert_eq!(
            colors.syntax[keyword.0 as usize]
                .0
                .and_then(Rgba::palette_index),
            Some(5)
        );
        assert_eq!(colors.terminal.map(|t| t.ansi), Some(None));
        // Nothing reported: nothing to tell programs.
        assert_eq!(
            ThemeId::TERMINAL
                .colors(&TerminalColors::default())
                .terminal,
            None
        );
    }

    #[test]
    fn a_dark_and_a_light_theme() {
        let setting = ThemeSetting {
            dark: ThemeId::named("Cue Dark").unwrap(),
            light: ThemeId::named("GitHub Light").unwrap(),
        };
        let mut terminal = TerminalColors::default();
        assert_eq!(setting.pick(&terminal), setting.dark);
        terminal.bg = Some([255; 3]);
        assert_eq!(setting.pick(&terminal), setting.light);
        let light = setting.pick(&terminal).colors(&terminal);
        assert!(light.light);
        assert_eq!(light.bg, Rgba::rgb(255, 255, 255));
        assert!(light.terminal.unwrap().ansi.is_some());
    }
}
