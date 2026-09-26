//! Named commands and the key bindings that trigger them.
//!
//! Every shortcut maps to a [`Command`] with a stable id (`editor:copy`) and
//! a title, so anything reachable from the keyboard can also be listed, run
//! by name, and shown with its shortcut.

use std::fmt;

use crate::input::{Key, KeyCode, Mods};

macro_rules! commands {
    ($($variant:ident => $id:literal, $title:literal;)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Command {
            $($variant),*
        }

        impl Command {
            pub const ALL: &[Command] = &[$(Command::$variant),*];

            /// The stable name, `namespace:action`.
            pub fn id(self) -> &'static str {
                match self {
                    $(Command::$variant => $id),*
                }
            }

            pub fn title(self) -> &'static str {
                match self {
                    $(Command::$variant => $title),*
                }
            }
        }
    };
}

commands! {
    Quit => "app:quit", "Quit";
    Palette => "app:command-palette", "Show Command Palette";
    Save => "file:save", "Save";
    GoToFile => "file:go-to", "Go to File";
    Undo => "editor:undo", "Undo";
    Redo => "editor:redo", "Redo";
    Copy => "editor:copy", "Copy";
    Cut => "editor:cut", "Cut";
    Paste => "editor:paste", "Paste";
    SelectAll => "editor:select-all", "Select All";
    ClearSelection => "editor:clear-selection", "Clear Selection";
    ToggleWrap => "editor:toggle-wrap", "Toggle Word Wrap";
    NewLine => "editor:newline", "Insert Line Break";
    InsertTab => "editor:insert-tab", "Insert Tab";
    DeleteBackward => "editor:delete-backward", "Delete Backward";
    DeleteForward => "editor:delete-forward", "Delete Forward";
    DeleteWordBackward => "editor:delete-word-backward", "Delete Word Backward";
    DeleteWordForward => "editor:delete-word-forward", "Delete Word Forward";
    MoveLinesUp => "editor:move-lines-up", "Move Lines Up";
    MoveLinesDown => "editor:move-lines-down", "Move Lines Down";
    CursorLeft => "cursor:left", "Cursor Left";
    CursorRight => "cursor:right", "Cursor Right";
    CursorUp => "cursor:up", "Cursor Up";
    CursorDown => "cursor:down", "Cursor Down";
    WordLeft => "cursor:word-left", "Cursor Word Left";
    WordRight => "cursor:word-right", "Cursor Word Right";
    LineStart => "cursor:line-start", "Cursor to Line Start";
    LineEnd => "cursor:line-end", "Cursor to Line End";
    DocumentStart => "cursor:document-start", "Cursor to Document Start";
    DocumentEnd => "cursor:document-end", "Cursor to Document End";
    CursorPageUp => "cursor:page-up", "Page Up";
    CursorPageDown => "cursor:page-down", "Page Down";
    ToggleTree => "tree:toggle", "Show or Hide File Tree";
    FocusTree => "tree:focus", "Focus File Tree";
    FocusEditor => "editor:focus", "Focus Editor";
    TreeUp => "tree:up", "File Tree: Select Previous";
    TreeDown => "tree:down", "File Tree: Select Next";
    TreeExpand => "tree:expand", "File Tree: Expand";
    TreeCollapse => "tree:collapse", "File Tree: Collapse";
    TreeOpen => "tree:open", "File Tree: Open";
    TreePreview => "tree:preview", "File Tree: Preview";
    TreeFirst => "tree:first", "File Tree: Select First";
    TreeLast => "tree:last", "File Tree: Select Last";
    TreePageUp => "tree:page-up", "File Tree: Page Up";
    TreePageDown => "tree:page-down", "File Tree: Page Down";
    TreeRefresh => "tree:refresh", "File Tree: Refresh";
    PickerUp => "picker:up", "Picker: Select Previous";
    PickerDown => "picker:down", "Picker: Select Next";
    PickerPageUp => "picker:page-up", "Picker: Page Up";
    PickerPageDown => "picker:page-down", "Picker: Page Down";
    PickerAccept => "picker:accept", "Picker: Open Selected";
    PickerClose => "picker:close", "Picker: Close";
}

/// Where a key binding applies: the focused part of the screen, or anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Global,
    Editor,
    Tree,
    /// The file picker and command palette, while open.
    Picker,
}

impl Command {
    /// Where this command's key bindings apply.
    pub fn context(self) -> Context {
        use Command::*;
        match self {
            Quit | Palette | Save | GoToFile | ToggleTree | FocusTree | FocusEditor => {
                Context::Global
            }
            TreeUp | TreeDown | TreeExpand | TreeCollapse | TreeOpen | TreePreview | TreeFirst
            | TreeLast | TreePageUp | TreePageDown | TreeRefresh => Context::Tree,
            PickerUp | PickerDown | PickerPageUp | PickerPageDown | PickerAccept | PickerClose => {
                Context::Picker
            }
            _ => Context::Editor,
        }
    }

    /// Cursor movements: with Shift held they extend the selection.
    pub fn extends_selection(self) -> bool {
        self.id().starts_with("cursor:")
    }

    /// Whether Shift plus this command's key still runs it when that shifted
    /// key has no binding of its own. Shift+Tab is kept free for outdenting.
    fn ignores_shift(self) -> bool {
        self != Command::InsertTab
    }
}

pub struct Keymap {
    /// In priority order: a command's first binding is the one shown for it.
    bindings: Vec<(Key, Command)>,
}

impl Default for Keymap {
    fn default() -> Keymap {
        use Command::*;
        use KeyCode::*;

        const ALT: Mods = Mods {
            alt: true,
            ..Mods::NONE
        };
        const CTRL_SHIFT: Mods = Mods {
            shift: true,
            ..Mods::CTRL
        };
        const SUPER: Mods = Mods {
            sup: true,
            ..Mods::NONE
        };
        const SUPER_SHIFT: Mods = Mods {
            shift: true,
            ..SUPER
        };
        let key = Key::new;

        let mut bindings = Vec::new();
        // Ctrl and Cmd (Super) are interchangeable for shortcuts; Ctrl comes
        // first because every terminal can send it.
        for (c, command) in [
            ('q', Quit),
            ('s', Save),
            ('z', Undo),
            ('y', Redo),
            ('c', Copy),
            ('x', Cut),
            ('v', Paste),
            ('a', SelectAll),
            ('w', ToggleWrap),
            // Ctrl+B and Ctrl+E as in VS Code (there Ctrl+Shift+E): "B" for
            // the sidebar, "E" for the explorer.
            ('b', ToggleTree),
            ('e', FocusTree),
            // Ctrl+P as in VS Code and Sublime. Ctrl+K rather than their
            // awkward Ctrl+Shift+P, as in Slack, Linear, and Raycast; typing
            // `>` in the file picker gets there too.
            ('p', GoToFile),
            ('k', Palette),
        ] {
            bindings.push((key(Char(c), Mods::CTRL), command));
            bindings.push((key(Char(c), SUPER), command));
        }
        bindings.push((key(Char('z'), CTRL_SHIFT), Redo));
        bindings.push((key(Char('z'), SUPER_SHIFT), Redo));

        bindings.extend([
            (key(Esc, Mods::NONE), ClearSelection),
            (key(Enter, Mods::NONE), NewLine),
            (key(Tab, Mods::NONE), InsertTab),
            (key(Backspace, Mods::NONE), DeleteBackward),
            (key(Delete, Mods::NONE), DeleteForward),
            // Word and line editing: Alt (Option) is the macOS modifier, Ctrl
            // the Linux/Windows one. macOS terminals often send Option+Left/
            // Right as the emacs keys Alt+B/Alt+F, and Option+Delete as Alt+D.
            (key(Backspace, ALT), DeleteWordBackward),
            (key(Backspace, Mods::CTRL), DeleteWordBackward),
            (key(Delete, ALT), DeleteWordForward),
            (key(Delete, Mods::CTRL), DeleteWordForward),
            (key(Char('d'), ALT), DeleteWordForward),
            (key(Up, ALT), MoveLinesUp),
            (key(Down, ALT), MoveLinesDown),
            (key(Left, Mods::NONE), CursorLeft),
            (key(Right, Mods::NONE), CursorRight),
            (key(Up, Mods::NONE), CursorUp),
            (key(Down, Mods::NONE), CursorDown),
            (key(Left, ALT), WordLeft),
            (key(Left, Mods::CTRL), WordLeft),
            (key(Char('b'), ALT), WordLeft),
            (key(Right, ALT), WordRight),
            (key(Right, Mods::CTRL), WordRight),
            (key(Char('f'), ALT), WordRight),
            // macOS conventions: Cmd+Left/Right go to the line's start/end and
            // Cmd+Up/Down to the document's; Ctrl+Home/End do the latter too.
            (key(Home, Mods::NONE), LineStart),
            (key(Left, SUPER), LineStart),
            (key(Home, SUPER), LineStart),
            (key(End, Mods::NONE), LineEnd),
            (key(Right, SUPER), LineEnd),
            (key(End, SUPER), LineEnd),
            (key(Home, Mods::CTRL), DocumentStart),
            (key(Up, SUPER), DocumentStart),
            (key(End, Mods::CTRL), DocumentEnd),
            (key(Down, SUPER), DocumentEnd),
            (key(PageUp, Mods::NONE), CursorPageUp),
            (key(PageDown, Mods::NONE), CursorPageDown),
            // Esc in the editor clears the selection; elsewhere it returns
            // to the editor.
            (key(Esc, Mods::NONE), FocusEditor),
            (key(Up, Mods::NONE), TreeUp),
            (key(Down, Mods::NONE), TreeDown),
            (key(Right, Mods::NONE), TreeExpand),
            (key(Left, Mods::NONE), TreeCollapse),
            (key(Enter, Mods::NONE), TreeOpen),
            (key(Char(' '), Mods::NONE), TreePreview),
            (key(Home, Mods::NONE), TreeFirst),
            (key(End, Mods::NONE), TreeLast),
            (key(PageUp, Mods::NONE), TreePageUp),
            (key(PageDown, Mods::NONE), TreePageDown),
            (key(Char('r'), Mods::CTRL), TreeRefresh),
            (key(Char('r'), SUPER), TreeRefresh),
            (key(Up, Mods::NONE), PickerUp),
            (key(Down, Mods::NONE), PickerDown),
            (key(PageUp, Mods::NONE), PickerPageUp),
            (key(PageDown, Mods::NONE), PickerPageDown),
            (key(Enter, Mods::NONE), PickerAccept),
            (key(Esc, Mods::NONE), PickerClose),
        ]);
        Keymap { bindings }
    }
}

impl Keymap {
    /// The command bound to `key` where `context` has focus, and whether
    /// Shift should extend the selection. Bindings for the focused context
    /// win over global ones. A key with Shift held falls back to its
    /// unshifted binding when it has none of its own: Shift+Left selects,
    /// Shift+Enter still breaks the line.
    pub fn lookup(&self, key: Key, context: Context) -> Option<(Command, bool)> {
        let key = normalize(key);
        if let Some(command) = self.find(key, context) {
            return Some((command, false));
        }
        if !key.mods.shift {
            return None;
        }
        let unshifted = Key::new(
            key.code,
            Mods {
                shift: false,
                ..key.mods
            },
        );
        self.find(unshifted, context)
            .filter(|command| command.ignores_shift())
            .map(|command| (command, command.extends_selection()))
    }

    /// The key shown for `command`, if it has one.
    pub fn shortcut(&self, command: Command) -> Option<Key> {
        self.bindings
            .iter()
            .find(|&&(_, c)| c == command)
            .map(|&(key, _)| key)
    }

    fn find(&self, key: Key, context: Context) -> Option<Command> {
        let bound = |context| {
            self.bindings
                .iter()
                .find(|&&(k, command)| k == key && command.context() == context)
                .map(|&(_, command)| command)
        };
        bound(context).or_else(|| bound(Context::Global))
    }
}

/// Compares shortcut letters lowercase, with Shift: modifyOtherKeys reports
/// Ctrl+Shift+Z as 'Z', and legacy Alt+Shift+B arrives as Alt+'B'.
fn normalize(key: Key) -> Key {
    match key.code {
        KeyCode::Char(c) if !key.mods.is_plain() && c.is_uppercase() => Key::new(
            KeyCode::Char(c.to_lowercase().next().unwrap_or(c)),
            Mods {
                shift: true,
                ..key.mods
            },
        ),
        _ => key,
    }
}

/// Shortcut labels such as `Ctrl+Shift+Z` or `Alt+Left`. The alternate
/// form (`{:#}`) is compact for the status bar, writing Ctrl as `^`: `^S`.
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Mods {
            shift,
            alt,
            ctrl,
            sup,
        } = self.mods;
        let super_name = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Super"
        };
        if ctrl && f.alternate() {
            f.write_str("^")?;
        }
        for (held, name) in [
            (ctrl && !f.alternate(), "Ctrl"),
            (alt, "Alt"),
            (shift, "Shift"),
            (sup, super_name),
        ] {
            if held {
                write!(f, "{name}+")?;
            }
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => write!(f, "{}", c.to_uppercase()),
            code => write!(f, "{code:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(shift: bool, alt: bool, ctrl: bool, sup: bool) -> Mods {
        Mods {
            shift,
            alt,
            ctrl,
            sup,
        }
    }

    fn in_editor(keymap: &Keymap, key: Key) -> Option<(Command, bool)> {
        keymap.lookup(key, Context::Editor)
    }

    #[test]
    fn ids_are_unique_and_namespaced() {
        for (i, a) in Command::ALL.iter().enumerate() {
            let (namespace, name) = a.id().split_once(':').expect("namespace:name");
            assert!(!namespace.is_empty() && !name.is_empty(), "{}", a.id());
            for b in &Command::ALL[i + 1..] {
                assert_ne!(a.id(), b.id());
            }
        }
    }

    #[test]
    fn every_command_has_a_shortcut() {
        let keymap = Keymap::default();
        for &command in Command::ALL {
            assert!(keymap.shortcut(command).is_some(), "{}", command.id());
        }
    }

    #[test]
    fn keys_are_bound_at_most_once_per_context() {
        let keymap = Keymap::default();
        for (i, (a, x)) in keymap.bindings.iter().enumerate() {
            for (b, y) in &keymap.bindings[i + 1..] {
                assert!(
                    a != b || x.context() != y.context(),
                    "{a} is bound twice in {:?}",
                    x.context()
                );
            }
        }
    }

    #[test]
    fn focused_context_wins_over_global() {
        let keymap = Keymap::default();
        let esc = Key::new(KeyCode::Esc, Mods::NONE);
        assert_eq!(
            keymap.lookup(esc, Context::Editor),
            Some((Command::ClearSelection, false))
        );
        assert_eq!(
            keymap.lookup(esc, Context::Tree),
            Some((Command::FocusEditor, false))
        );
        let up = Key::new(KeyCode::Up, Mods::NONE);
        assert_eq!(
            keymap.lookup(up, Context::Tree),
            Some((Command::TreeUp, false))
        );
        let ctrl_s = Key::new(KeyCode::Char('s'), Mods::CTRL);
        assert_eq!(
            keymap.lookup(ctrl_s, Context::Tree),
            Some((Command::Save, false))
        );
        // Editor commands aren't reachable from the tree.
        let ctrl_z = Key::new(KeyCode::Char('z'), Mods::CTRL);
        assert_eq!(keymap.lookup(ctrl_z, Context::Tree), None);
    }

    #[test]
    fn shift_extends_movements_and_is_otherwise_ignored() {
        let keymap = Keymap::default();
        let shift = mods(true, false, false, false);
        assert_eq!(
            in_editor(&keymap, Key::new(KeyCode::Left, shift)),
            Some((Command::CursorLeft, true))
        );
        assert_eq!(
            in_editor(
                &keymap,
                Key::new(KeyCode::Left, mods(true, false, false, true))
            ),
            Some((Command::LineStart, true))
        );
        assert_eq!(
            in_editor(&keymap, Key::new(KeyCode::Enter, shift)),
            Some((Command::NewLine, false))
        );
        assert_eq!(in_editor(&keymap, Key::new(KeyCode::Tab, shift)), None);
        // A shifted binding of its own wins over the fallback.
        assert_eq!(
            in_editor(
                &keymap,
                Key::new(KeyCode::Char('z'), mods(true, false, true, false))
            ),
            Some((Command::Redo, false))
        );
    }

    #[test]
    fn uppercase_letters_count_as_shifted() {
        let keymap = Keymap::default();
        assert_eq!(
            in_editor(&keymap, Key::new(KeyCode::Char('Z'), Mods::CTRL)),
            Some((Command::Redo, false))
        );
        assert_eq!(
            in_editor(
                &keymap,
                Key::new(KeyCode::Char('B'), mods(false, true, false, false))
            ),
            Some((Command::WordLeft, true))
        );
        // Plain letters are text, not shortcuts.
        assert_eq!(
            in_editor(&keymap, Key::new(KeyCode::Char('A'), Mods::NONE)),
            None
        );
    }

    #[test]
    fn labels() {
        let keymap = Keymap::default();
        let label = |command| keymap.shortcut(command).unwrap().to_string();
        assert_eq!(label(Command::Save), "Ctrl+S");
        assert_eq!(label(Command::WordLeft), "Alt+Left");
        assert_eq!(label(Command::DocumentStart), "Ctrl+Home");
        assert_eq!(
            Key::new(KeyCode::Char('z'), mods(true, false, true, false)).to_string(),
            "Ctrl+Shift+Z"
        );
        assert_eq!(
            format!("{:#}", keymap.shortcut(Command::Save).unwrap()),
            "^S"
        );
        assert_eq!(
            format!(
                "{:#}",
                Key::new(KeyCode::Char('z'), mods(true, false, true, false))
            ),
            "^Shift+Z"
        );
    }
}
