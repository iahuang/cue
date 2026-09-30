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
    SaveAs => "file:save-as", "Save As…";
    NewFile => "file:new", "New File";
    CreateFile => "file:create", "New File…";
    OpenFile => "file:open", "Open File…";
    CloseFile => "file:close", "Close File";
    GoToFile => "file:go-to", "Go to File";
    GoToLine => "editor:go-to-line", "Go to Line…";
    GoToSymbol => "editor:go-to-symbol", "Go to Symbol in File…";
    GoToWorkspaceSymbol => "search:symbols", "Go to Symbol in Workspace…";
    GoToTerminal => "terminal:go-to", "Go to Terminal…";
    RecoverUnsaved => "file:recover", "Recover Unsaved Changes…";
    SearchWorkspace => "search:workspace", "Search in Workspace";
    Find => "find:show", "Find in File";
    FindReplace => "find:show-replace", "Replace in File";
    FindNext => "find:next", "Find Next";
    FindPrevious => "find:previous", "Find Previous";
    FindSwitchField => "find:switch-field", "Find: Switch Between Find and Replace";
    FindClose => "find:close", "Close Find";
    Replace => "find:replace", "Replace";
    ReplaceAll => "find:replace-all", "Replace All";
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
    Indent => "editor:indent", "Indent Lines";
    Outdent => "editor:outdent", "Outdent Lines";
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
    SplitRight => "panel:split-right", "Split Panel Right";
    SplitDown => "panel:split-down", "Split Panel Down";
    ClosePanel => "panel:close", "Close Panel";
    GoBack => "panel:go-back", "Go Back";
    GoForward => "panel:go-forward", "Go Forward";
    FocusPanelLeft => "panel:focus-left", "Focus Panel Left";
    FocusPanelRight => "panel:focus-right", "Focus Panel Right";
    FocusPanelUp => "panel:focus-up", "Focus Panel Above";
    FocusPanelDown => "panel:focus-down", "Focus Panel Below";
    NewTab => "tab:new", "New Tab";
    CloseTab => "tab:close", "Close Tab";
    NextTab => "tab:next", "Next Tab";
    PreviousTab => "tab:previous", "Previous Tab";
    MoveTabLeft => "tab:move-left", "Move Tab Left";
    MoveTabRight => "tab:move-right", "Move Tab Right";
    RenameTab => "tab:rename", "Rename Tab";
    GoToTab1 => "tab:go-to-1", "Go to Tab 1";
    GoToTab2 => "tab:go-to-2", "Go to Tab 2";
    GoToTab3 => "tab:go-to-3", "Go to Tab 3";
    GoToTab4 => "tab:go-to-4", "Go to Tab 4";
    GoToTab5 => "tab:go-to-5", "Go to Tab 5";
    GoToTab6 => "tab:go-to-6", "Go to Tab 6";
    GoToTab7 => "tab:go-to-7", "Go to Tab 7";
    GoToTab8 => "tab:go-to-8", "Go to Tab 8";
    GoToTab9 => "tab:go-to-9", "Go to Tab 9";
    NewTerminal => "terminal:new", "New Terminal";
    ClearTerminal => "terminal:clear", "Clear Terminal";
    CloseTerminal => "terminal:close", "Close Terminal";
    RenameTerminal => "terminal:rename", "Rename Terminal";
    TerminalPrefix => "terminal:prefix", "Terminal: Send Next Shortcut to cue";
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
    TreeContextMenu => "tree:context-menu", "File Tree: Show Context Menu";
    TreeOpenToSide => "tree:open-to-side", "File Tree: Open to the Side";
    TreeNewFile => "tree:new-file", "File Tree: New File…";
    TreeNewFolder => "tree:new-folder", "File Tree: New Folder…";
    TreeRename => "tree:rename", "File Tree: Rename or Move…";
    TreeDuplicate => "tree:duplicate", "File Tree: Duplicate…";
    TreeTrash => "tree:trash", "File Tree: Move to Trash";
    TreeCopyPath => "tree:copy-path", "File Tree: Copy Path";
    TreeCopyRelativePath => "tree:copy-relative-path", "File Tree: Copy Relative Path";
    TreeReveal => "tree:reveal", "File Tree: Reveal in File Manager";
    TreeOpenInTerminal => "tree:open-in-terminal", "File Tree: Open in Terminal";
    AddFolder => "workspace:add-folder", "Add Folder to Workspace…";
    TreeRemoveFolder => "tree:remove-folder", "File Tree: Remove Folder from Workspace";
    PickerUp => "picker:up", "Picker: Select Previous";
    PickerDown => "picker:down", "Picker: Select Next";
    PickerPageUp => "picker:page-up", "Picker: Page Up";
    PickerPageDown => "picker:page-down", "Picker: Page Down";
    PickerAccept => "picker:accept", "Picker: Open Selected";
    PickerClose => "picker:close", "Picker: Close";
    PickerCloseItem => "picker:close-item", "Picker: Close Selected File or Terminal";
    SearchToggleCase => "search:toggle-case", "Search: Match Case";
    SearchToggleWord => "search:toggle-word", "Search: Match Whole Word";
    SearchToggleRegex => "search:toggle-regex", "Search: Use Regular Expression";
    DialogParent => "dialog:parent", "File Dialog: Parent Folder";
    DialogComplete => "dialog:complete", "File Dialog: Complete Name";
}

/// Where a key binding applies: the focused part of the screen, or anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Global,
    Editor,
    Tree,
    /// The file picker and command palette, while open.
    Picker,
    /// Workspace search, while open. The picker's keys and the search
    /// options work there too.
    Search,
    /// The file dialog, while open. The picker's keys work there too.
    Dialog,
    /// Matching case, whole words, or regexes, in workspace search and in
    /// finding in the file.
    SearchOptions,
    /// The find bar's query, while it has focus. The search options work
    /// there too.
    Find,
    /// The find bar's replacement, while it has focus. The find bar's keys
    /// work there too.
    Replace,
    /// A terminal, which gets every key but a few (see
    /// [`Keymap::lookup_terminal`]).
    Terminal,
}

impl Context {
    /// Where to look for a key's binding after this context, in order,
    /// before the global bindings.
    fn parents(self) -> &'static [Context] {
        match self {
            Context::Search => &[Context::SearchOptions, Context::Picker],
            Context::Dialog => &[Context::Picker],
            Context::Find => &[Context::SearchOptions],
            Context::Replace => &[Context::Find, Context::SearchOptions],
            _ => &[],
        }
    }
}

impl Command {
    /// Where this command's key bindings apply.
    pub fn context(self) -> Context {
        use Command::*;
        match self {
            Quit | Palette | Save | SaveAs | NewFile | CreateFile | OpenFile | GoToFile
            | GoToLine | GoToSymbol | GoToWorkspaceSymbol | GoToTerminal | RecoverUnsaved
            | SearchWorkspace | Find | FindReplace | FindNext | FindPrevious | ToggleTree
            | FocusTree | FocusEditor | SplitRight | SplitDown | ClosePanel | GoBack
            | GoForward | FocusPanelLeft | FocusPanelRight | FocusPanelUp | FocusPanelDown
            | NewTerminal | NewTab | CloseTab | NextTab | PreviousTab | MoveTabLeft
            | MoveTabRight | RenameTab | GoToTab1 | GoToTab2 | GoToTab3 | GoToTab4 | GoToTab5
            | GoToTab6 | GoToTab7 | GoToTab8 | GoToTab9 | AddFolder => Context::Global,
            TreeUp | TreeDown | TreeExpand | TreeCollapse | TreeOpen | TreePreview | TreeFirst
            | TreeLast | TreePageUp | TreePageDown | TreeRefresh | TreeContextMenu
            | TreeOpenToSide | TreeNewFile | TreeNewFolder | TreeRename | TreeDuplicate
            | TreeTrash | TreeCopyPath | TreeCopyRelativePath | TreeReveal | TreeOpenInTerminal
            | TreeRemoveFolder => Context::Tree,
            PickerUp | PickerDown | PickerPageUp | PickerPageDown | PickerAccept | PickerClose
            | PickerCloseItem => Context::Picker,
            SearchToggleCase | SearchToggleWord | SearchToggleRegex => Context::SearchOptions,
            DialogParent | DialogComplete => Context::Dialog,
            FindSwitchField | FindClose => Context::Find,
            ClearTerminal | CloseTerminal | RenameTerminal | TerminalPrefix => Context::Terminal,
            Replace | ReplaceAll => Context::Replace,
            _ => Context::Editor,
        }
    }

    /// The Go to Tab commands, in order.
    const GO_TO_TAB: [Command; 9] = [
        Command::GoToTab1,
        Command::GoToTab2,
        Command::GoToTab3,
        Command::GoToTab4,
        Command::GoToTab5,
        Command::GoToTab6,
        Command::GoToTab7,
        Command::GoToTab8,
        Command::GoToTab9,
    ];

    /// For Go to Tab N, the tab's index, N - 1.
    pub fn tab_index(self) -> Option<usize> {
        Command::GO_TO_TAB
            .iter()
            .position(|&command| command == self)
    }

    /// Cursor movements: with Shift held they extend the selection.
    pub fn extends_selection(self) -> bool {
        self.id().starts_with("cursor:")
    }

    /// Whether Shift plus this command's key still runs it when that shifted
    /// key has no binding of its own. Shift+Tab outdents.
    fn ignores_shift(self) -> bool {
        self != Command::InsertTab
    }
}

pub struct Keymap {
    /// In priority order: a command's first binding is the one shown for it.
    bindings: Vec<Binding>,
}

/// A key that runs a command where a context has focus: usually the
/// command's own context, but a command may have keys in others too.
#[derive(Debug, Clone, Copy)]
struct Binding {
    key: Key,
    command: Command,
    context: Context,
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
        const SHIFT: Mods = Mods {
            shift: true,
            ..Mods::NONE
        };
        const SUPER: Mods = Mods {
            sup: true,
            ..Mods::NONE
        };
        const SUPER_SHIFT: Mods = Mods {
            shift: true,
            ..SUPER
        };
        const SUPER_ALT: Mods = Mods { alt: true, ..SUPER };
        let key = Key::new;

        let mut bindings: Vec<(Key, Command)> = Vec::new();
        // Ctrl and Cmd (Super) are interchangeable for shortcuts; Ctrl comes
        // first because every terminal can send it.
        for (c, command) in [
            ('q', Quit),
            ('s', Save),
            // As in VS Code, where Ctrl+N is a new untitled file too.
            ('n', NewFile),
            ('o', OpenFile),
            ('z', Undo),
            ('y', Redo),
            ('c', Copy),
            ('x', Cut),
            ('v', Paste),
            ('a', SelectAll),
            // Ctrl+B and Ctrl+E as in VS Code (there Ctrl+Shift+E): "B" for
            // the sidebar, "E" for the explorer.
            ('b', ToggleTree),
            ('e', FocusTree),
            // Ctrl+P as in VS Code and Sublime. Ctrl+K rather than their
            // awkward Ctrl+Shift+P, as in Slack, Linear, and Raycast; typing
            // `>` in the file picker gets there too.
            ('p', GoToFile),
            ('k', Palette),
            // Ctrl+R as in Sublime; Ctrl+G, VS Code's, finds the next match.
            ('r', GoToSymbol),
            ('l', GoToLine),
            ('f', Find),
            // Ctrl+G, as in VS Code on macOS and in browsers.
            ('g', FindNext),
            // Ctrl+\ as in VS Code; Ctrl+W as closing a tab does.
            ('\\', SplitRight),
            ('w', ClosePanel),
            // As in browsers.
            ('t', NewTab),
        ] {
            bindings.push((key(Char(c), Mods::CTRL), command));
            bindings.push((key(Char(c), SUPER), command));
        }
        bindings.push((key(Char('z'), CTRL_SHIFT), Redo));
        bindings.push((key(Char('z'), SUPER_SHIFT), Redo));
        // Legacy terminals send it as Ctrl+S, which saves; there, the
        // command palette has it.
        bindings.push((key(Char('s'), CTRL_SHIFT), SaveAs));
        bindings.push((key(Char('s'), SUPER_SHIFT), SaveAs));
        // As in VS Code, Sublime, and Zed. Terminals that only speak the
        // legacy protocol send it as Ctrl+F, which finds in the file; there,
        // the command palette has it.
        bindings.push((key(Char('f'), CTRL_SHIFT), SearchWorkspace));
        bindings.push((key(Char('f'), SUPER_SHIFT), SearchWorkspace));
        // As in Sublime. Legacy terminals send it as Ctrl+R, the file's
        // symbols; there, `#` in the file picker has them.
        bindings.push((key(Char('r'), CTRL_SHIFT), GoToWorkspaceSymbol));
        bindings.push((key(Char('r'), SUPER_SHIFT), GoToWorkspaceSymbol));
        bindings.push((key(Char('g'), CTRL_SHIFT), FindPrevious));
        bindings.push((key(Char('g'), SUPER_SHIFT), FindPrevious));
        // As in VS Code, Sublime, and Zed; on macOS, VS Code's Cmd+Alt+F.
        // Legacy terminals send Ctrl+H as Backspace; there, the command
        // palette has it.
        bindings.push((key(Char('h'), Mods::CTRL), FindReplace));
        bindings.push((key(Char('f'), SUPER_ALT), FindReplace));
        // Legacy terminals send it as Ctrl+\, which splits right; there,
        // the command palette has it. Some report the shifted key, `|`.
        bindings.push((key(Char('\\'), CTRL_SHIFT), SplitDown));
        bindings.push((key(Char('\\'), SUPER_SHIFT), SplitDown));
        bindings.push((key(Char('|'), CTRL_SHIFT), SplitDown));
        bindings.push((key(Char('|'), SUPER_SHIFT), SplitDown));
        // Legacy terminals send it as Ctrl+N, a new file; there, the command
        // palette has it.
        bindings.push((key(Char('n'), CTRL_SHIFT), NewTerminal));
        bindings.push((key(Char('n'), SUPER_SHIFT), NewTerminal));
        // As in VS Code; Ctrl+` alone is the terminal's prefix. Some
        // terminals report the shifted key, `~`.
        bindings.push((key(Char('`'), CTRL_SHIFT), NewTerminal));
        bindings.push((key(Char('~'), CTRL_SHIFT), NewTerminal));
        // Ctrl+Alt rather than Ctrl alone, which macOS keeps for switching
        // desktops.
        const CTRL_ALT: Mods = Mods {
            alt: true,
            ..Mods::CTRL
        };
        for (code, command) in [
            (Left, FocusPanelLeft),
            (Right, FocusPanelRight),
            (Up, FocusPanelUp),
            (Down, FocusPanelDown),
            // VS Code's New File… is Ctrl+Alt+Super+N.
            (Char('n'), CreateFile),
            // Tabs are a level above panels: Ctrl+Alt as for moving between
            // panels, W as closing a panel, and brackets as macOS's
            // Cmd+Shift+[ and ].
            (Char('w'), CloseTab),
            (Char(']'), NextTab),
            (Char('['), PreviousTab),
        ] {
            bindings.push((key(code, CTRL_ALT), command));
            bindings.push((key(code, SUPER_ALT), command));
        }
        // As in VS Code on macOS. Legacy terminals send Ctrl+- as Ctrl+_;
        // some report Ctrl+Shift+- as Ctrl+Shift+_. In a terminal, Ctrl+-
        // is the shell's (undo, in readline).
        bindings.push((key(Char('-'), Mods::CTRL), GoBack));
        bindings.push((key(Char('_'), Mods::CTRL), GoBack));
        bindings.push((key(Char('-'), CTRL_SHIFT), GoForward));
        bindings.push((key(Char('_'), CTRL_SHIFT), GoForward));
        // As in VS Code and browsers. In a terminal, the shell has them.
        bindings.push((key(PageDown, Mods::CTRL), NextTab));
        bindings.push((key(PageUp, Mods::CTRL), PreviousTab));
        bindings.push((key(PageDown, CTRL_SHIFT), MoveTabRight));
        bindings.push((key(PageUp, CTRL_SHIFT), MoveTabLeft));
        // As in browsers, but tab 9 is the ninth, not the last. Legacy
        // terminals can't send these; the kitty protocol can.
        for (n, command) in ('1'..='9').zip(Command::GO_TO_TAB) {
            bindings.push((key(Char(n), Mods::CTRL), command));
            bindings.push((key(Char(n), SUPER), command));
        }

        bindings.extend([
            (key(Esc, Mods::NONE), ClearSelection),
            (key(Enter, Mods::NONE), NewLine),
            (key(Tab, Mods::NONE), InsertTab),
            (key(Tab, SHIFT), Outdent),
            // As in VS Code. Legacy terminals send Ctrl+[ as Esc.
            (key(Char(']'), Mods::CTRL), Indent),
            (key(Char(']'), SUPER), Indent),
            (key(Char('['), Mods::CTRL), Outdent),
            (key(Char('['), SUPER), Outdent),
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
            // As in VS Code.
            (key(Enter, Mods::CTRL), TreeOpenToSide),
            (key(Enter, SUPER), TreeOpenToSide),
            // As in Windows and GTK; F2 renames there, and in VS Code.
            (key(F(10), SHIFT), TreeContextMenu),
            (key(F(2), Mods::NONE), TreeRename),
            // Cmd+Backspace as in the Finder.
            (key(Delete, Mods::NONE), TreeTrash),
            (key(Backspace, SUPER), TreeTrash),
            (key(Up, Mods::NONE), PickerUp),
            (key(Down, Mods::NONE), PickerDown),
            (key(PageUp, Mods::NONE), PickerPageUp),
            (key(PageDown, Mods::NONE), PickerPageDown),
            (key(Enter, Mods::NONE), PickerAccept),
            (key(Esc, Mods::NONE), PickerClose),
            // Closing a panel closes what it shows; in the picker, what's
            // selected.
            (key(Char('w'), Mods::CTRL), PickerCloseItem),
            (key(Char('w'), SUPER), PickerCloseItem),
            // As in VS Code's search.
            (key(Char('c'), ALT), SearchToggleCase),
            (key(Char('w'), ALT), SearchToggleWord),
            (key(Char('r'), ALT), SearchToggleRegex),
            (key(Esc, Mods::NONE), FindClose),
            (key(Tab, Mods::NONE), FindSwitchField),
            (key(Enter, Mods::NONE), Replace),
            (key(Enter, ALT), ReplaceAll),
            // As in the Finder and macOS's file dialogs.
            (key(Up, ALT), DialogParent),
            (key(Up, SUPER), DialogParent),
            (key(Tab, Mods::NONE), DialogComplete),
        ]);
        let mut bindings: Vec<Binding> = bindings
            .into_iter()
            .map(|(key, command)| Binding {
                key,
                command,
                context: command.context(),
            })
            .collect();
        // In the find bar, Enter steps through the matches.
        for (key, command) in [
            (key(Enter, Mods::NONE), FindNext),
            (key(Enter, SHIFT), FindPrevious),
        ] {
            bindings.push(Binding {
                key,
                command,
                context: Context::Find,
            });
        }
        // In a terminal, Ctrl+C and Ctrl+V are the shell's; as in Linux
        // terminals, Ctrl+Shift copies and pastes. Ctrl+` makes the next
        // shortcut cue's, as in tmux (VS Code's terminal toggle; shells
        // don't use it). Terminals without the kitty keyboard protocol send
        // it as Ctrl+Space. Ctrl+1 to 9 go to tabs, from terminals too:
        // shells don't use them either.
        let go_to_tab = ('1'..='9')
            .zip(Command::GO_TO_TAB)
            .map(|(n, command)| (key(Char(n), Mods::CTRL), command));
        for (key, command) in go_to_tab.chain([
            (key(Char('`'), Mods::CTRL), TerminalPrefix),
            (key(Char('c'), CTRL_SHIFT), Copy),
            (key(Char('c'), SUPER), Copy),
            (key(Char('v'), CTRL_SHIFT), Paste),
            (key(Char('v'), SUPER), Paste),
        ]) {
            bindings.push(Binding {
                key,
                command,
                context: Context::Terminal,
            });
        }
        Keymap { bindings }
    }
}

impl Keymap {
    /// The command bound to `key` where `context` has focus, and whether
    /// Shift should extend the selection. Bindings for the focused context
    /// win over its parent's, which win over global ones. A key with Shift held falls back to its
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

    /// The command bound to `key` in a terminal, which gets the key when
    /// there is none. The shell has the keys a terminal would send it:
    /// cue keeps only its terminal bindings, and global shortcuts with
    /// Cmd (Super), Ctrl+Alt, or Ctrl+Shift, which shells don't use.
    /// Ctrl+Shift+key runs cue's Ctrl+key command, taking precedence over
    /// its usual shifted binding. The prefix, Ctrl+`, reaches the rest.
    pub fn lookup_terminal(&self, key: Key) -> Option<Command> {
        let key = normalize(key);
        let bound = |key, context| {
            self.bindings
                .iter()
                .find(|b| b.key == key && b.context == context)
                .map(|b| b.command)
        };
        if key.mods.ctrl && key.mods.shift {
            // Terminals may report the shifted character instead of the
            // base key (for example, `|` instead of `\\`).
            let code = match key.code {
                KeyCode::Char(c) => {
                    let base = "~!@#$%^&*()_+{}|:\"<>?"
                        .chars()
                        .zip("`1234567890-=[]\\;',./".chars())
                        .find_map(|(shifted, base)| (c == shifted).then_some(base))
                        .unwrap_or(c);
                    KeyCode::Char(base)
                }
                code => code,
            };
            let unshifted = Key::new(
                code,
                Mods {
                    shift: false,
                    ..key.mods
                },
            );
            return bound(unshifted, Context::Terminal)
                .or_else(|| self.find(unshifted, Context::Editor));
        }
        if let Some(command) = bound(key, Context::Terminal) {
            return Some(command);
        }
        let Mods {
            shift,
            alt,
            ctrl,
            sup,
        } = key.mods;
        if !(sup || (ctrl && (alt || shift))) {
            return None;
        }
        bound(key, Context::Global)
    }

    /// The key shown for `command`, if it has one.
    pub fn shortcut(&self, command: Command) -> Option<Key> {
        self.bindings
            .iter()
            .find(|b| b.command == command)
            .map(|b| b.key)
    }

    /// The key for `command` where `context` has focus, if it has one there.
    pub fn shortcut_in(&self, command: Command, context: Context) -> Option<Key> {
        self.bindings
            .iter()
            .find(|b| b.command == command && b.context == context)
            .map(|b| b.key)
    }

    fn find(&self, key: Key, context: Context) -> Option<Command> {
        let bound = |context| {
            self.bindings
                .iter()
                .find(|b| b.key == key && b.context == context)
                .map(|b| b.command)
        };
        std::iter::once(context)
            .chain(context.parents().iter().copied())
            .chain([Context::Global])
            .find_map(bound)
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
            KeyCode::F(n) => write!(f, "F{n}"),
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
        // Reached only from the command palette, or the tree's context menu.
        const UNBOUND: &[Command] = &[
            // `$` in the file picker gets there too.
            Command::GoToTerminal,
            Command::ToggleWrap,
            Command::CloseFile,
            Command::ClearTerminal,
            Command::CloseTerminal,
            Command::RenameTerminal,
            Command::RenameTab,
            Command::TreeNewFile,
            Command::TreeNewFolder,
            Command::TreeDuplicate,
            Command::TreeCopyPath,
            Command::TreeCopyRelativePath,
            Command::TreeReveal,
            Command::TreeOpenInTerminal,
            Command::AddFolder,
            Command::RecoverUnsaved,
            Command::TreeRemoveFolder,
        ];
        let keymap = Keymap::default();
        for &command in Command::ALL {
            let bound = keymap.shortcut(command).is_some();
            assert_eq!(bound, !UNBOUND.contains(&command), "{}", command.id());
        }
    }

    #[test]
    fn keys_are_bound_at_most_once_per_context() {
        let keymap = Keymap::default();
        for (i, a) in keymap.bindings.iter().enumerate() {
            for b in &keymap.bindings[i + 1..] {
                assert!(
                    a.key != b.key || a.context != b.context,
                    "{} is bound twice in {:?}",
                    a.key,
                    a.context
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
        // Search has the picker's keys, and its own.
        assert_eq!(
            keymap.lookup(up, Context::Search),
            Some((Command::PickerUp, false))
        );
        let alt_c = Key::new(KeyCode::Char('c'), mods(false, true, false, false));
        assert_eq!(
            keymap.lookup(alt_c, Context::Search),
            Some((Command::SearchToggleCase, false))
        );
        assert_eq!(keymap.lookup(alt_c, Context::Picker), None);

        // In the find bar, Enter finds the next match; in the replacement,
        // it replaces, and the find bar's other keys still work.
        let enter = Key::new(KeyCode::Enter, Mods::NONE);
        let shift_enter = Key::new(KeyCode::Enter, mods(true, false, false, false));
        assert_eq!(
            keymap.lookup(enter, Context::Find),
            Some((Command::FindNext, false))
        );
        assert_eq!(
            keymap.lookup(shift_enter, Context::Find),
            Some((Command::FindPrevious, false))
        );
        assert_eq!(
            keymap.lookup(enter, Context::Replace),
            Some((Command::Replace, false))
        );
        assert_eq!(
            keymap.lookup(esc, Context::Replace),
            Some((Command::FindClose, false))
        );
        assert_eq!(
            keymap.lookup(alt_c, Context::Replace),
            Some((Command::SearchToggleCase, false))
        );
        assert_eq!(
            keymap.lookup(enter, Context::Editor),
            Some((Command::NewLine, false))
        );
        // The shortcut shown for Find Next is the one that works anywhere.
        assert_eq!(
            keymap.shortcut(Command::FindNext).unwrap().to_string(),
            "Ctrl+G"
        );
        assert_eq!(
            keymap.shortcut_in(Command::FindNext, Context::Find),
            Some(enter)
        );
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
        assert_eq!(
            in_editor(&keymap, Key::new(KeyCode::Tab, shift)),
            Some((Command::Outdent, false))
        );
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

    #[test]
    fn terminals_get_keys_but_cues_chords() {
        let keymap = Keymap::default();
        let key =
            |c, shift, alt, ctrl, sup| Key::new(KeyCode::Char(c), mods(shift, alt, ctrl, sup));
        let lookup = |key| keymap.lookup_terminal(key);
        // The shell's: Ctrl+key, Alt+key, Esc, arrows.
        for key in [
            key('p', false, false, true, false),
            key('w', false, false, true, false),
            key('c', false, false, true, false),
            key('q', false, false, true, false),
            key('\\', false, false, true, false),
            key('b', false, true, false, false),
            Key::new(KeyCode::Esc, Mods::NONE),
            Key::new(KeyCode::Left, Mods::CTRL),
            Key::new(KeyCode::PageDown, Mods::CTRL),
            key('t', false, false, true, false),
        ] {
            assert_eq!(lookup(key), None, "{key}");
        }
        // cue's: Ctrl+Shift aliases Ctrl; Cmd and Ctrl+Alt keep their bindings.
        for (key, command) in [
            (key('p', true, false, true, false), Command::GoToFile),
            (key('P', false, false, true, false), Command::GoToFile),
            (key('w', true, false, true, false), Command::ClosePanel),
            (key('k', true, false, true, false), Command::Palette),
            (key('z', true, false, true, false), Command::Undo),
            (key('|', true, false, true, false), Command::SplitRight),
            (key('!', true, false, true, false), Command::GoToTab1),
            (key('_', true, false, true, false), Command::GoBack),
            (key('f', true, false, true, false), Command::Find),
            (key('\\', true, false, true, false), Command::SplitRight),
            (key('p', false, false, false, true), Command::GoToFile),
            (key('n', true, false, true, false), Command::NewFile),
            (key('N', false, false, true, false), Command::NewFile),
            (key('`', true, false, true, false), Command::TerminalPrefix),
            (key('~', true, false, true, false), Command::TerminalPrefix),
            (key('n', false, true, true, false), Command::CreateFile),
            (key('s', true, false, true, false), Command::Save),
            (
                Key::new(KeyCode::Left, mods(false, true, true, false)),
                Command::FocusPanelLeft,
            ),
            (key('t', false, false, false, true), Command::NewTab),
            (key('1', false, false, true, false), Command::GoToTab1),
            (key('9', false, false, true, false), Command::GoToTab9),
            (key('9', false, false, false, true), Command::GoToTab9),
            (key('w', false, true, true, false), Command::CloseTab),
            (key(']', false, true, true, false), Command::NextTab),
            (key('[', false, true, true, false), Command::PreviousTab),
            (
                Key::new(KeyCode::PageDown, mods(true, false, true, false)),
                Command::NextTab,
            ),
            (key('c', true, false, true, false), Command::Copy),
            (key('c', false, false, false, true), Command::Copy),
            (key('v', true, false, true, false), Command::Paste),
            (key('`', false, false, true, false), Command::TerminalPrefix),
        ] {
            assert_eq!(lookup(key), Some(command), "{key}");
        }
        // Cmd with an editing key is the editor's, not the shell's or cue's.
        assert_eq!(
            lookup(Key::new(KeyCode::Left, mods(false, false, false, true))),
            None
        );
    }
}
