//! Settings, from `$XDG_CONFIG_HOME/cue/config.toml`, or
//! `~/.config/cue/config.toml`, or the file `CUE_CONFIG` names.
//!
//! The file is optional, and so is everything in it: what isn't set keeps
//! its default. A setting that's misspelled or of the wrong kind is left at
//! its default with a warning, and the rest still apply, so a mistake never
//! keeps cue from starting.
//!
//! They're read at startup, and again when the file is saved from cue, or
//! on Reload Settings, into a [`Config`] that anything can get (see
//! [`get`]), on any thread.

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::{LazyLock, RwLock};

use toml::{Table, Value};

use crate::indent::Indent;
use crate::input::{Key, KeyCode};
use crate::keymap::{Command, Context};
use crate::theme::{ThemeId, ThemeSetting};

/// The settings, each named as in the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `editor.tab_width`: columns a tab takes.
    pub tab_width: u32,
    /// `editor.indent`: what Tab inserts in a file whose own indentation
    /// can't be told, and its language has no custom: a number of spaces,
    /// or `"tabs"`.
    pub indent: Indent,
    /// `editor.wrap`: whether long lines wrap, at first.
    pub wrap: bool,
    /// `editor.scroll_margin`: the part of the view, up to half, kept
    /// between the cursor and the view's edges.
    pub scroll_margin: f32,
    /// `ui.theme`: the colors, by a theme's name, or one for when the
    /// terminal is dark and one for when it's light.
    pub theme: ThemeSetting,
    /// `ui.terminal_background`: whether a theme other than Terminal sets
    /// the terminal's own background (OSC 11), so what's around the text,
    /// such as the window's padding, is the theme's too.
    pub terminal_background: bool,
    /// `ui.cursor_color`: whether a theme other than Terminal colors the
    /// cursor (OSC 12), rather than leaving it the terminal's color.
    pub cursor_color: bool,
    /// `ui.nerd_font`: whether to show file icons, which need a Nerd Font.
    /// `CUE_NERD_FONT` overrides it.
    pub nerd_font: bool,
    /// `ui.tree`: whether the file tree shows at first.
    pub tree: bool,
    /// `ui.tree_width`: the file tree's width, at first.
    pub tree_width: u32,
    /// `terminal.shell`: the program terminals run, or `$SHELL`.
    pub shell: Option<PathBuf>,
    /// `terminal.scrollback`: bytes of history a terminal keeps.
    pub scrollback: u32,
    /// `files.exclude`: names of files and folders never shown, besides
    /// `.git` and `.DS_Store`. `*` matches any run of characters.
    pub exclude: Vec<String>,
    /// `[keys]`: shortcuts, in the file's order, each binding a key to a
    /// command, or with none, unbinding it (see
    /// [`Keymap::new`](crate::keymap::Keymap::new)).
    pub keys: Vec<(Key, Option<Command>)>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            tab_width: 4,
            indent: Indent::Spaces(4),
            wrap: true,
            scroll_margin: 0.15,
            theme: ThemeSetting::default(),
            terminal_background: true,
            cursor_color: true,
            nerd_font: false,
            tree: true,
            tree_width: 30,
            shell: None,
            scrollback: 10 * 1024 * 1024,
            exclude: Vec::new(),
            keys: Vec::new(),
        }
    }
}

/// The narrowest the tree's width can be set: any narrower and it hides.
pub const MIN_TREE_WIDTH: u32 = 12;

/// What Open Settings creates when there's no file yet: every setting, at
/// its default, commented out.
pub const TEMPLATE: &str = r#"# cue settings. Uncomment a line to change it; the rest keep their defaults.
# Saving this file in cue applies it; after changing it elsewhere, run Reload
# Settings. Terminals already open keep their shell and scrollback.

[editor]
# Columns a tab takes.
# tab_width = 4
# What Tab inserts in files whose indentation can't be told: a number of
# spaces, or "tabs".
# indent = 4
# Whether long lines wrap. Toggle Word Wrap changes it for one editor.
# wrap = true
# The part of the view, from 0 to 0.5, kept between the cursor and its edges.
# scroll_margin = 0.15

[ui]
# The colors. "Terminal" is the terminal's own; Select Theme in the command
# palette shows the rest. A theme for a dark terminal and one for a light:
# theme = { dark = "Cue Dark", light = "GitHub Light" }
# theme = "Terminal"
# Whether a theme other than Terminal makes the terminal's own background
# its own, so the window's padding matches, and back when cue quits.
# terminal_background = true
# Whether a theme other than Terminal colors the cursor.
# cursor_color = true
# File icons. They need a Nerd Font; CUE_NERD_FONT=1 turns them on too.
# nerd_font = false
# Whether the file tree shows at first, and how wide.
# tree = true
# tree_width = 30

[terminal]
# The program terminals run, as a login shell. By default, $SHELL.
# shell = "/bin/zsh"
# History kept above the screen, as "512KB", "10MB", or a number of bytes.
# scrollback = "10MB"

[files]
# Names of files and folders never shown, besides .git and .DS_Store, in the
# tree, Go to File, and workspace search. * matches any run of characters.
# exclude = ["node_modules", "*.pyc"]

[keys]
# Shortcuts, as "key" = "command", with the commands' names that cue --help
# lists. A key bound here does only what it's bound to, and "none" unbinds
# one. Ctrl and Cmd are different keys: bind both to use either.
# "ctrl+shift+p" = "app:command-palette"
# "cmd+shift+p" = "app:command-palette"
# "ctrl+q" = "none"
# Keys without Ctrl, Alt, or Cmd type text, except in the file tree.
# "a" = "tree:new-file"
"#;

#[cfg(not(test))]
static CURRENT: LazyLock<RwLock<Arc<Config>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Config::default())));

/// The settings in use: the defaults, until [`set`].
#[cfg(not(test))]
pub fn get() -> Arc<Config> {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Puts `config` in use.
#[cfg(not(test))]
pub fn set(config: Config) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
}

// In tests, each test's thread has its own, so a test's settings can't
// reach those running alongside it. Other threads have the defaults.
#[cfg(test)]
thread_local! {
    static CURRENT: std::cell::RefCell<Arc<Config>> = Default::default();
}

/// The settings in use: in tests, those this test set.
#[cfg(test)]
pub fn get() -> Arc<Config> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Puts `config` in use for the rest of this test.
#[cfg(test)]
pub fn set(config: Config) {
    CURRENT.with(|current| *current.borrow_mut() = Arc::new(config));
}

/// Where the settings are, if there's anywhere for them.
pub fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CUE_CONFIG").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?;
    Some(config.join("cue/config.toml"))
}

/// The settings in the file at [`path`], or the defaults without one, and
/// what was wrong with it.
pub fn load() -> (Config, Vec<String>) {
    let Some(path) = path() else {
        return (Config::default(), Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(err) => (
            Config::default(),
            vec![format!("Can't read {}: {err}", path.display())],
        ),
    }
}

/// The settings `text` makes, and what was wrong with it.
pub fn parse(text: &str) -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    let table: Table = match text.parse() {
        Ok(table) => table,
        Err(err) => {
            warnings.push(syntax_error(text, &err));
            return (config, warnings);
        }
    };
    for (section, value) in &table {
        let Some(settings) = value.as_table() else {
            warnings.push(format!("{section} should be a section, [{section}]"));
            continue;
        };
        if section == "keys" {
            for (key, value) in settings {
                match binding(key, value) {
                    Ok(binding) => config.keys.push(binding),
                    Err(problem) => warnings.push(format!("keys.\"{key}\": {problem}")),
                }
            }
            continue;
        }
        for (name, value) in settings {
            let key = format!("{section}.{name}");
            if let Err(expected) = config.apply(&key, value) {
                warnings.push(match expected {
                    Some(expected) => format!("{key} should be {expected}"),
                    None => format!("There's no setting {key}"),
                });
            }
        }
    }
    (config, warnings)
}

impl Config {
    /// Sets `key` to `value`. The error says what it should be instead, or
    /// is `None` for a key that isn't a setting.
    fn apply(&mut self, key: &str, value: &Value) -> Result<(), Option<&'static str>> {
        match key {
            "editor.tab_width" => {
                const EXPECTED: &str = "a number from 1 to 16";
                self.tab_width = int(value, 1..=16).ok_or(EXPECTED)?;
            }
            "editor.indent" => {
                const EXPECTED: &str = "a number of spaces from 1 to 16, or \"tabs\"";
                self.indent = match value {
                    Value::String(s) if s == "tabs" => Indent::Tabs,
                    value => Indent::Spaces(int(value, 1..=16).ok_or(EXPECTED)?),
                };
            }
            "editor.wrap" => self.wrap = value.as_bool().ok_or("true or false")?,
            "editor.scroll_margin" => {
                const EXPECTED: &str = "a number from 0 to 0.5";
                let margin = match value {
                    Value::Float(f) => *f,
                    Value::Integer(i) => *i as f64,
                    _ => return Err(Some(EXPECTED)),
                };
                if !(0.0..=0.5).contains(&margin) {
                    return Err(Some(EXPECTED));
                }
                self.scroll_margin = margin as f32;
            }
            "ui.theme" => {
                const EXPECTED: &str = "a theme's name, as Select Theme lists them, or { dark = \"…\", light = \"…\" }";
                let named = |value: Option<&Value>| match value {
                    None => Some(ThemeId::TERMINAL),
                    Some(value) => ThemeId::named(value.as_str()?),
                };
                self.theme = match value {
                    Value::String(name) => ThemeSetting::one(ThemeId::named(name).ok_or(EXPECTED)?),
                    Value::Table(table) if table.keys().all(|k| k == "dark" || k == "light") => {
                        ThemeSetting {
                            dark: named(table.get("dark")).ok_or(EXPECTED)?,
                            light: named(table.get("light")).ok_or(EXPECTED)?,
                        }
                    }
                    _ => return Err(Some(EXPECTED)),
                };
            }
            "ui.terminal_background" => {
                self.terminal_background = value.as_bool().ok_or("true or false")?
            }
            "ui.cursor_color" => self.cursor_color = value.as_bool().ok_or("true or false")?,
            "ui.nerd_font" => self.nerd_font = value.as_bool().ok_or("true or false")?,
            "ui.tree" => self.tree = value.as_bool().ok_or("true or false")?,
            "ui.tree_width" => {
                const EXPECTED: &str = "a number from 12 to 1000";
                self.tree_width = int(value, MIN_TREE_WIDTH..=1000).ok_or(EXPECTED)?;
            }
            "terminal.shell" => {
                const EXPECTED: &str = "the path of a program";
                let shell = value.as_str().filter(|s| !s.is_empty()).ok_or(EXPECTED)?;
                self.shell = Some(PathBuf::from(shell));
            }
            "terminal.scrollback" => {
                const EXPECTED: &str = "a size such as \"10MB\", or a number of bytes";
                self.scrollback = size(value).ok_or(EXPECTED)?;
            }
            "files.exclude" => {
                const EXPECTED: &str = "a list of names, such as [\"node_modules\"]";
                let names = value.as_array().ok_or(EXPECTED)?;
                self.exclude = names
                    .iter()
                    .map(|name| name.as_str().filter(|s| !s.is_empty()).map(String::from))
                    .collect::<Option<_>>()
                    .ok_or(EXPECTED)?;
            }
            _ => return Err(None),
        }
        Ok(())
    }

    /// Whether `files.exclude` hides a file or folder named `name`.
    pub fn excludes(&self, name: &str) -> bool {
        self.exclude
            .iter()
            .any(|pattern| wildcard_match(pattern, name))
    }
}

/// `text`, a settings file, with `ui.theme` set to `id` in place of what it
/// was, or added. If it was a theme for a dark terminal and one for a
/// light, only the one for a `light` terminal or a dark one is.
pub fn with_theme(text: &str, id: ThemeId, light: bool) -> String {
    let quoted = |id: ThemeId| format!("\"{}\"", id.name());
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut section = String::new();
    let mut ui = None;
    let mut found = None;
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).trim().to_string();
            if section == "ui" && ui.is_none() {
                ui = Some(i);
            }
            continue;
        }
        let key = match section.as_str() {
            "" => "ui.theme",
            "ui" => "theme",
            _ => continue,
        };
        let value = line
            .strip_prefix(key)
            .and_then(|rest| rest.trim_start().strip_prefix('='));
        if let Some(value) = value {
            found = Some((i, key, value.trim().to_string()));
            break;
        }
    }
    match found {
        Some((i, key, value)) => {
            let value = match format!("theme = {value}").parse::<Table>() {
                Ok(table) if table["theme"].is_table() => {
                    let mut config = Config::default();
                    let setting = match config.apply("ui.theme", &table["theme"]) {
                        Ok(()) => config.theme,
                        Err(_) => ThemeSetting::default(),
                    };
                    let (dark, light) = match light {
                        true => (setting.dark, id),
                        false => (id, setting.light),
                    };
                    format!("{{ dark = {}, light = {} }}", quoted(dark), quoted(light))
                }
                _ => quoted(id),
            };
            lines[i] = format!("{key} = {value}");
        }
        None => match ui {
            Some(header) => lines.insert(header + 1, format!("theme = {}", quoted(id))),
            None => {
                if lines.last().is_some_and(|line| !line.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push("[ui]".to_string());
                lines.push(format!("theme = {}", quoted(id)));
            }
        },
    }
    lines.join("\n") + "\n"
}

/// The shortcut `key` = `value` in `[keys]`: a key, and the id of the
/// command it runs, or `"none"`.
fn binding(key: &str, value: &Value) -> Result<(Key, Option<Command>), String> {
    let key: Key = key.parse()?;
    let id = value
        .as_str()
        .ok_or("should be a command, such as \"app:quit\", or \"none\"")?;
    if id == "none" {
        return Ok((key, None));
    }
    let command = Command::from_id(id).ok_or_else(|| format!("there's no command \"{id}\""))?;
    // Only the tree has no text to type into.
    let types =
        matches!(key.code, KeyCode::Char(_)) && !(key.mods.ctrl || key.mods.alt || key.mods.sup);
    if types && command.context() != Context::Tree {
        return Err(
            "a key without Ctrl, Alt, or Cmd types text, so only tree: commands can have it"
                .to_string(),
        );
    }
    Ok((key, Some(command)))
}

/// `value` as a whole number in `range`.
fn int(value: &Value, range: std::ops::RangeInclusive<u32>) -> Option<u32> {
    let n = u32::try_from(value.as_integer()?).ok()?;
    range.contains(&n).then_some(n)
}

/// `value` as bytes: a number of them, or a size such as `"10MB"` or
/// `"512 KB"`, in units of 1024. At most 4GB, less a byte.
fn size(value: &Value) -> Option<u32> {
    let bytes = match value {
        Value::Integer(n) => u64::try_from(*n).ok()?,
        Value::String(s) => {
            let s = s.trim();
            let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            let n: u64 = s[..digits].parse().ok()?;
            let unit = match s[digits..].trim().to_ascii_uppercase().as_str() {
                "" | "B" => 1,
                "K" | "KB" => 1 << 10,
                "M" | "MB" => 1 << 20,
                "G" | "GB" => 1 << 30,
                _ => return None,
            };
            n.checked_mul(unit)?
        }
        _ => return None,
    };
    u32::try_from(bytes).ok()
}

/// Whether `name` matches `pattern`, where `*` matches any run of
/// characters, and anything else itself.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let Some((first, rest)) = pattern.split_once('*') else {
        return pattern == name;
    };
    let Some(mut name) = name.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = rest.split('*').collect();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match name.find(part) {
            Some(i) => name = &name[i + part.len()..],
            None => return false,
        }
    }
    name.len() >= last.len() && name.ends_with(last)
}

/// A TOML syntax error, on one line: where, and what's wrong.
fn syntax_error(text: &str, err: &toml::de::Error) -> String {
    let message = err.message().trim_end_matches('\n').replace('\n', "; ");
    match err.span() {
        Some(span) => {
            let line = text[..span.start.min(text.len())].matches('\n').count() + 1;
            format!("Line {line}: {message}")
        }
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn nothing_set_is_the_defaults() {
        assert_eq!(parse(""), (Config::default(), Vec::new()));
        assert_eq!(parse(TEMPLATE), (Config::default(), Vec::new()));
    }

    /// The template, uncommented, sets what the defaults are already, and
    /// so tells them right. The example lines aren't defaults.
    #[test]
    fn the_template_tells_the_defaults() {
        let uncommented: String = TEMPLATE
            .lines()
            .filter_map(|line| match line.strip_prefix("# ") {
                Some(setting)
                    if setting
                        .split_once(" = ")
                        .is_some_and(|(key, _)| !key.contains(' ')) =>
                {
                    Some(setting)
                }
                Some(_) => None,
                None => Some(line),
            })
            .filter(|line| {
                !line.starts_with("shell")
                    && !line.starts_with("exclude")
                    && !line.starts_with("theme = {")
                    && !line.starts_with('"')
            })
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(parse(&uncommented), (Config::default(), Vec::new()));
        // And the examples are good.
        let (config, warnings) = parse(
            &TEMPLATE
                .replace("# shell", "shell")
                .replace("# exclude", "exclude")
                .replace("# theme = {", "theme = {")
                .replace("# \"", "\""),
        );
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(config.shell, Some(PathBuf::from("/bin/zsh")));
        assert_eq!(config.exclude, ["node_modules", "*.pyc"]);
        assert_eq!(config.theme.dark.name(), "Cue Dark");
        assert_eq!(config.theme.light.name(), "GitHub Light");
        assert_eq!(config.keys.len(), 4);
    }

    #[test]
    fn keys_bind_in_order() {
        let (config, warnings) = parse(
            r#"
            [keys]
            "ctrl+shift+p" = "app:command-palette"
            "Cmd+Alt+Left" = "panel:focus-left"
            "ctrl+q" = "none"
            "a" = "tree:new-file"
            "#,
        );
        assert_eq!(warnings, Vec::<String>::new());
        let shift = |mods| Mods {
            shift: true,
            ..mods
        };
        let cmd_alt = Mods {
            sup: true,
            alt: true,
            ..Mods::NONE
        };
        assert_eq!(
            config.keys,
            [
                (
                    Key::new(KeyCode::Char('p'), shift(Mods::CTRL)),
                    Some(Command::Palette)
                ),
                (
                    Key::new(KeyCode::Left, cmd_alt),
                    Some(Command::FocusPanelLeft)
                ),
                (Key::new(KeyCode::Char('q'), Mods::CTRL), None),
                (
                    Key::new(KeyCode::Char('a'), Mods::NONE),
                    Some(Command::TreeNewFile)
                ),
            ]
        );
    }

    #[test]
    fn bad_keys_warn() {
        let (config, warnings) = parse(
            r#"
            [keys]
            "ctrl+k" = "app:quit"
            "hyper+k" = "app:quit"
            "ctrl+kk" = "app:quit"
            "ctrl+j" = "app:leave"
            "ctrl+l" = 1
            "shift+x" = "app:quit"
            "#,
        );
        assert_eq!(
            warnings,
            [
                r#"keys."hyper+k": there's no modifier "hyper""#,
                r#"keys."ctrl+kk": there's no key "kk""#,
                r#"keys."ctrl+j": there's no command "app:leave""#,
                r#"keys."ctrl+l": should be a command, such as "app:quit", or "none""#,
                r#"keys."shift+x": a key without Ctrl, Alt, or Cmd types text, so only tree: commands can have it"#,
            ]
        );
        assert_eq!(config.keys.len(), 1);
    }

    #[test]
    fn settings_apply() {
        let (config, warnings) = parse(
            r#"
            [editor]
            tab_width = 8
            indent = "tabs"
            wrap = false
            scroll_margin = 0
            [ui]
            theme = "catppuccin mocha"
            terminal_background = false
            cursor_color = false
            nerd_font = true
            tree = false
            tree_width = 40
            [terminal]
            shell = "/bin/bash"
            scrollback = "512 KB"
            [files]
            exclude = ["target"]
            "#,
        );
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(
            config,
            Config {
                tab_width: 8,
                indent: Indent::Tabs,
                wrap: false,
                scroll_margin: 0.0,
                theme: ThemeSetting::one(ThemeId::named("Catppuccin Mocha").unwrap()),
                terminal_background: false,
                cursor_color: false,
                nerd_font: true,
                tree: false,
                tree_width: 40,
                shell: Some(PathBuf::from("/bin/bash")),
                scrollback: 512 * 1024,
                exclude: vec!["target".to_string()],
                keys: Vec::new(),
            }
        );
        assert_eq!(parse("editor.indent = 2").0.indent, Indent::Spaces(2));
    }

    #[test]
    fn mistakes_warn_and_keep_the_defaults() {
        let (config, warnings) = parse(
            r##"
            theme = "dark"
            [editor]
            tab_width = 0
            wrap = "yes"
            tabwidth = 2
            indent = 2
            [terminal]
            scrollback = "10 parsecs"
            [colors]
            red = "#f00"
            [ui]
            theme = "Solarized"
            "##,
        );
        assert_eq!(
            warnings,
            [
                "theme should be a section, [theme]",
                "editor.tab_width should be a number from 1 to 16",
                "editor.wrap should be true or false",
                "There's no setting editor.tabwidth",
                "terminal.scrollback should be a size such as \"10MB\", or a number of bytes",
                "There's no setting colors.red",
                "ui.theme should be a theme's name, as Select Theme lists them, or { dark = \"…\", light = \"…\" }",
            ]
        );
        assert_eq!(
            config,
            Config {
                indent: Indent::Spaces(2),
                ..Config::default()
            }
        );
    }

    #[test]
    fn syntax_errors_say_where() {
        let (config, warnings) = parse("[editor]\ntab_width = 4\nwrap = \n");
        assert_eq!(config, Config::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("Line 3: "), "{warnings:?}");
        assert!(!warnings[0].contains('\n'));
    }

    #[test]
    fn choosing_a_theme_sets_it() {
        let cue_dark = ThemeId::named("Cue Dark").unwrap();
        let nord = ThemeId::named("Nord").unwrap();
        let theme = |text: &str| {
            let (config, warnings) = parse(text);
            assert_eq!(warnings, Vec::<String>::new(), "{text}");
            config.theme
        };
        // Added under [ui], where the template has it commented out.
        let text = with_theme(TEMPLATE, cue_dark, false);
        assert_eq!(theme(&text), ThemeSetting::one(cue_dark));
        assert!(text.contains("[ui]\ntheme = \"Cue Dark\"\n"));
        // Replaced.
        let text = with_theme(&text, nord, true);
        assert_eq!(theme(&text), ThemeSetting::one(nord));
        assert_eq!(text.matches("\ntheme =").count(), 1);
        // Only the one for the terminal's appearance.
        let both = "[ui]\ntheme = { dark = \"Nord\", light = \"GitHub Light\" }\n";
        let text = with_theme(both, cue_dark, false);
        assert_eq!(
            theme(&text),
            ThemeSetting {
                dark: cue_dark,
                light: ThemeId::named("GitHub Light").unwrap(),
            }
        );
        // No [ui] at all, or set as a dotted key.
        let text = with_theme("[editor]\nwrap = false", nord, false);
        assert_eq!(text, "[editor]\nwrap = false\n\n[ui]\ntheme = \"Nord\"\n");
        assert_eq!(theme(&text), ThemeSetting::one(nord));
        let text = with_theme("ui.theme = \"Dracula\"\n[editor]\n", nord, false);
        assert_eq!(theme(&text), ThemeSetting::one(nord));
    }

    #[test]
    fn sizes() {
        let size = |s: &str| size(&Value::String(s.to_string()));
        assert_eq!(size("10MB"), Some(10 << 20));
        assert_eq!(size("10m"), Some(10 << 20));
        assert_eq!(size("3 GB"), Some(3 << 30));
        assert_eq!(size("4GB"), None);
        assert_eq!(size("100"), Some(100));
        assert_eq!(size("MB"), None);
        assert_eq!(super::size(&Value::Integer(-1)), None);
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("node_modules", "node_modules"));
        assert!(!wildcard_match("node_modules", "node_modules2"));
        assert!(wildcard_match("*.pyc", "a.pyc"));
        assert!(wildcard_match("*.pyc", ".pyc"));
        assert!(!wildcard_match("*.pyc", "a.py"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "abc"));
        assert!(wildcard_match("a*b*c", "a-b-b-c"));
        assert!(!wildcard_match("a*b*c", "a-c"));
        assert!(!wildcard_match("ab*ba", "aba"));
    }
}
