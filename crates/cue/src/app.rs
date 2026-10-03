//! The whole screen: the file tree on the left, and right of it, panels.
//!
//! The space right of the tree splits into panels (see [`crate::layout`]),
//! side by side or stacked, each showing an open file or nothing yet. One
//! panel is active: keys go through the keymap to it, unless the tree has
//! focus, and files open in it.
//!
//! The app owns every open file. Each keeps its text and undo history while
//! no panel shows it, so switching files never loses work, and panels
//! showing the same file share them. Each panel keeps its own cursor and
//! scroll position in each file it has shown.
//!
//! As in VS Code, a file opened with a single click or Space is a preview:
//! the next preview replaces it, so stepping through files doesn't keep them
//! all open. Editing it, or opening it with Enter or a double click, keeps it.
//!
//! The picker (Ctrl+P for files, Ctrl+K for commands) and workspace search
//! (Ctrl+Shift+F) open over everything and have the keyboard until they
//! close. The workspace's files are listed in the background from the
//! start, so the picker can show them right away.
//!
//! Each editor has its own find bar (Ctrl+F); the app remembers the last
//! query, so finding in another file starts from it.
//!
//! Files are opened, saved under a new name, and created through the file
//! dialog (Ctrl+O, Ctrl+Shift+S, Ctrl+Alt+N), which opens over everything
//! like the picker. Ctrl+N opens an untitled file, `Untitled-1` and so on,
//! which the picker lists until it's saved; saving it asks where.
//!
//! Closing a panel (Ctrl+W) closes what it shows, unless another panel
//! shows it too: a file, which is no longer open, or a terminal, whose
//! shell is hung up on. Showing something else in a panel instead keeps it
//! open, for the picker to bring back, and Ctrl+W in the picker closes it
//! from there. Closing unsaved changes, or a running program, asks first.
//!
//! Right-clicking the file tree, or Shift+F10 there, opens a context menu
//! of file commands: renaming, moving, duplicating, and trashing files and
//! folders, and so on. They have keys and palette entries too, and act on
//! the tree's selection, or from elsewhere, on the file on screen.
//!
//! Terminals (Ctrl+Shift+N) are the app's too, like open files: a panel shows
//! one, and it keeps running when the panel moves on. While one has the
//! keyboard, keys go to its shell, but for a few (see
//! [`Keymap::lookup_terminal`]).
//!
//! Tabs (see [`crate::tab`]) are whole layouts of panels, one on screen at a
//! time; Ctrl+T opens one. Files and terminals aren't any tab's: a file
//! can be shown in panels in several, and a terminal, in one panel anywhere,
//! moves to the tab it's shown in. Closing a tab closes what its panels
//! show, as closing each of them would.
//!
//! A cue can be a session (see [`crate::session`]), kept to come back to:
//! Keep Session makes it one, as does quitting with unsaved changes or
//! programs running and answering Keep Session, or the terminal going away
//! then. A session saves its tabs, files, unsaved changes, and terminals as
//! they change; quitting it detaches while programs run in its terminals,
//! and otherwise leaves it saved. End Session lets it go.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use opentui::{Attributes, Buffer};

use crate::alert::{Alert, AlertAction, Button};
use crate::config::{self, Config, MIN_TREE_WIDTH};
use crate::context_menu::{ContextMenu, MenuAction, MenuItem};
use crate::document::{self, Disk, DiskChange, Document};
use crate::editor::{Action, Editor};
use crate::file_dialog::{DialogAction, FileDialog, Purpose};
use crate::file_index::FileIndex;
use crate::find;
use crate::image::{self, ImageView};
use crate::input::{Key, KeyCode, Mods, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Context, Keymap};
use crate::layout::{self, Axis, Direction, Layout, PanelId, Rect};
use crate::line_edit::Edit;
use crate::location::{self, Position, Target};
use crate::panel::{HeaderButton, Panel, Visit};
use crate::picker::{Choice, Item, Mode, Picker, PickerAction};
use crate::recovery::{self, Orphan, Recovery};
use crate::search::Toggle;
use crate::search_modal::{Memory, SearchAction, SearchModal};
use crate::session::{self, Session, Shown};
use crate::status::{self, Prompt, PromptKey};
use crate::symbols::{self, SymbolIndex};
use crate::tab::{self, BarItem, Tab, TabId};
use crate::terminal::Terminal;
use crate::theme::{self, TerminalColors, Theme, ThemeId, ThemeSetting};
use crate::tree::{Entry, FileTree, TreeAction};
use crate::watch::{Changes, Watcher};
use crate::workspace::Workspace;

/// How far sideways a header that resizes a split is dragged, before it's
/// dragged up or down, to move its panel instead.
const MOVE_THRESHOLD: u32 = 3;

/// The tree hides rather than leave the panels narrower than this.
const MIN_EDITOR_WIDTH: u32 = 40;
/// How many recently shown files and terminals the picker lists first.
const RECENT_FILES: usize = 50;
/// A session saves itself at most this often, as it changes.
const SESSION_INTERVAL: Duration = Duration::from_secs(1);
/// And its terminals' screens at most this often, but when it's left.
const SCREENS_INTERVAL: Duration = Duration::from_secs(60);

/// Something a panel showed, for the picker to list first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recent {
    File(PathBuf),
    /// A terminal, by id.
    Terminal(u32),
    /// An untitled file, by number.
    Untitled(u32),
}

/// Something that asked first with an alert, to do again once answered.
#[derive(Clone)]
enum Redo {
    /// A command that closes something, or quits.
    Run(Command),
    /// Closing this tab.
    CloseTab(TabId),
    /// Closing a file or terminal the picker lists.
    CloseItem(Choice),
    /// Moving this to the Trash.
    Trash(PathBuf),
}

/// An answer to an alert.
#[derive(Clone)]
enum Answer {
    /// Go ahead, losing what the alert said would be lost.
    Go(Redo),
    /// Save these files, then go ahead.
    Save(Vec<Rc<Document>>, Redo),
    /// Save this file over the one on disk, which changed since, then
    /// carry on saving, if it was.
    Overwrite(Rc<Document>, Option<Saving>),
    /// Take the file's text from disk, dropping unsaved changes, then
    /// carry on saving, if it was.
    Revert(Rc<Document>, Option<Saving>),
    /// Open these files with the unsaved changes a cue that's gone left.
    Recover(Vec<Orphan>),
    /// Let those changes go.
    DiscardRecovered(Vec<Orphan>),
    /// Keep everything as a session, then quit.
    KeepSession,
}

/// What's left of saving files and then going ahead, while one that
/// changed on disk is asked about.
#[derive(Clone)]
struct Saving {
    docs: Vec<Rc<Document>>,
    redo: Redo,
}

/// What the main loop should do after the app handled input.
pub enum AppAction {
    Continue,
    Quit,
    /// Put this text on the system clipboard.
    Copy(String),
    /// Go on in the background, a session, with the terminal let go.
    Detach,
    /// Start cue again in this process, from its binary as it is now,
    /// keeping everything (see [`App::hand_over`]).
    Restart,
}

/// A terminal a process this one replaced left running, to adopt.
pub struct Adopted {
    pub id: u32,
    /// The pty's master.
    pub fd: RawFd,
    /// The shell's process id.
    pub pid: libc::pid_t,
    /// Its screen, in full (see [`Terminal::screen`]), and the size then.
    pub screen: Vec<u8>,
    pub size: (u16, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    /// The active panel.
    Editor,
}

/// What a context menu's commands act on.
enum MenuFor {
    /// A file or folder, from the tree.
    File(Entry),
    /// The session, from the status bar's badge.
    Session,
}

/// Where a mouse press landed; drags and the release go there too.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseTarget {
    Language,
    /// The status bar's session badge.
    Session,
    Tree,
    /// The tree's divider.
    Divider,
    /// The divider between panels side by side.
    Handle(Vec<bool>),
    /// A panel's header, which drags the panel (see [`HeaderDrag`]).
    Header(PanelId),
    Panel(PanelId),
    /// A tab in the tab bar, which drags along it.
    Tab(TabId),
    /// The tab bar's button for a new tab.
    NewTab,
}

/// A panel's header being dragged. A header with a panel above it is also
/// the handle between them. Dragged up or down first, it resizes their
/// split, until it leaves the panel's columns. Dragged sideways first, past
/// [`MOVE_THRESHOLD`], or out of the panel's columns, it moves the panel,
/// to wherever it's released, and the split goes back to how it was.
/// Deciding by the first movement keeps a quick drag up or down, which
/// drifts sideways, resizing.
struct HeaderDrag {
    /// The split the header resizes, if any.
    handle: Option<Vec<bool>>,
    /// The cell it was grabbed at.
    x: u32,
    y: u32,
    /// The layout when it was grabbed.
    before: Layout,
    /// It went up or down first, so it resizes until it leaves the
    /// panel's columns.
    resizing: bool,
    moving: bool,
    /// Where the panel goes if released now.
    drop: Option<layout::Drop>,
}

pub struct App {
    workspace: Workspace,
    keymap: Keymap,
    tree: FileTree,
    /// Open files, in the order they were opened. They stay open when no
    /// panel shows them.
    documents: Vec<Rc<Document>>,
    /// The styles every editor's highlights use.
    theme: Rc<Theme>,
    /// What the terminal says its colors are, which Terminal is made of,
    /// and which pick between a dark theme and a light one.
    terminal_colors: TerminalColors,
    /// The theme last put in use, if any, and whether it needs working out
    /// again, as the terminal's colors changed.
    theme_shown: Option<ThemeId>,
    theme_stale: bool,
    /// Running terminals, and exited ones still shown, in the order they
    /// were started.
    terminals: Vec<Rc<RefCell<Terminal>>>,
    /// The id for the next new terminal.
    next_terminal: u32,
    /// The terminal prefix (Ctrl+`) was pressed: the next key is a cue
    /// shortcut.
    terminal_prefix: bool,
    /// The panel the last input went to, if it may not be the active one:
    /// the wheel scrolls panels without making them active.
    input_panel: Option<PanelId>,
    /// The terminal last told it has the keyboard.
    focused_terminal: Option<Rc<RefCell<Terminal>>>,
    /// The tabs, in the order the bar lists them. Never empty.
    tabs: Vec<Tab>,
    /// The index of the tab on screen.
    tab: usize,
    /// The id for the next new tab.
    next_tab: TabId,
    /// The id for the next new panel, in any tab.
    next_panel: PanelId,
    /// The prompt for the tab's new name, while it's open.
    tab_prompt: Option<Prompt>,
    /// Text from the last copy or cut, shared by all editors.
    clipboard: Option<String>,
    /// Text a terminal's program copied, for the system clipboard.
    copied: Option<String>,
    focus: Focus,
    tree_visible: bool,
    /// The tree's width when there is room for it.
    tree_width: u32,
    width: u32,
    height: u32,
    mouse_target: Option<MouseTarget>,
    header_drag: Option<HeaderDrag>,
    /// A question to answer before going on, above everything else.
    alert: Option<Alert<Answer>>,
    /// The alert was answered: what it asked about goes ahead without
    /// asking again.
    confirmed: bool,
    /// The file opened as a preview, which the next preview replaces.
    preview: Option<Rc<Document>>,
    /// The row and time of the last click in the tree, to spot double clicks.
    last_tree_click: Option<(u32, Instant)>,
    /// The tab and time of the last click on the tab bar.
    last_tab_click: Option<(TabId, Instant)>,
    picker: Option<Picker>,
    /// Every file in the workspace, for the picker.
    files: FileIndex,
    /// Every symbol the workspace's files define, for the picker.
    symbols: SymbolIndex,
    /// The picker asked for the workspace's symbols before its files were
    /// all listed: index them once they are.
    index_symbols_after_listing: bool,
    /// Hears of files other programs change, in the tree's open folders
    /// and those of open files.
    watcher: Watcher,
    /// The tree's listing and open files' folders the watcher last took,
    /// to tell when to update it.
    watching: Option<(u64, BTreeSet<PathBuf>)>,
    /// Files and terminals shown in the active panel, most recent first.
    recent: Vec<Recent>,
    /// Workspace search, while open. Never open with the picker.
    search: Option<SearchModal>,
    /// The file dialog, while open. Never open with the picker or search.
    dialog: Option<FileDialog>,
    /// A context menu, while open, and what its commands act on. Never
    /// open with another popup.
    menu: Option<(ContextMenu, MenuFor)>,
    /// What the last search left behind, for the next one.
    search_memory: Memory,
    /// The last find bar's query and replacement.
    find_memory: find::Memory,
    /// Copies of unsaved changes, in case cue exits without quitting.
    recovery: Recovery,
    /// The settings file Open Settings opens, if there's anywhere for one,
    /// resolved as documents' paths are.
    settings: Option<PathBuf>,
    /// The [`document::content_hash`] of the settings file as last read, or
    /// `None` if there was none: when an open document of it was saved
    /// with something else, the settings are read again.
    settings_hash: Option<u64>,
    /// Where sessions are kept, if anywhere.
    sessions: Option<PathBuf>,
    /// The session this cue is, if it is one.
    session: Option<Session>,
    /// When it was last saved, and its terminals' screens.
    session_saved: Option<Instant>,
    screens_saved: Option<Instant>,
    /// Each terminal's screen as last saved: its name in the session, and
    /// the terminal's output epoch then.
    screens: HashMap<u32, (String, u64)>,
    /// A terminal shows cue: it isn't detached.
    attached: bool,
}

/// A popup's query line, which typing, pasting, and the editor's cursor
/// keys edit.
trait QueryInput {
    fn edit(&mut self, edit: Edit);

    /// An edit with Shift held, which selects where the field can.
    fn edit_selecting(&mut self, edit: Edit) {
        self.edit(edit);
    }

    fn select_all(&mut self) {}

    fn selected_text(&self) -> Option<&str> {
        None
    }
}

impl QueryInput for Picker {
    fn edit(&mut self, edit: Edit) {
        Picker::edit(self, edit);
    }

    fn edit_selecting(&mut self, edit: Edit) {
        Picker::edit_selecting(self, edit);
    }

    fn select_all(&mut self) {
        Picker::select_all(self);
    }

    fn selected_text(&self) -> Option<&str> {
        Picker::selected_text(self)
    }
}

/// The find bar's focused field.
impl QueryInput for Editor {
    fn edit(&mut self, edit: Edit) {
        self.find_edit(edit);
    }
}

impl QueryInput for SearchModal {
    fn edit(&mut self, edit: Edit) {
        SearchModal::edit(self, edit);
    }
}

impl QueryInput for FileDialog {
    fn edit(&mut self, edit: Edit) {
        FileDialog::edit(self, edit);
    }

    fn edit_selecting(&mut self, edit: Edit) {
        FileDialog::edit_selecting(self, edit);
    }

    fn select_all(&mut self) {
        FileDialog::select_all(self);
    }

    fn selected_text(&self) -> Option<&str> {
        FileDialog::selected_text(self)
    }
}

impl App {
    /// Shows `workspace`, with `file` open, or a new, unnamed buffer. Focus
    /// starts in the editor when there is a file or no tree, in the tree
    /// otherwise.
    pub fn new(
        workspace: Workspace,
        file: Option<PathBuf>,
        width: u32,
        height: u32,
    ) -> Result<App, String> {
        let file = file.map(|file| document::resolve(&file));
        let mut tree = FileTree::new(workspace.roots());
        // Before revealing, so the file's folders stay in view above it.
        // The status bar is below it.
        tree.set_height(height.saturating_sub(1));
        if let Some(file) = &file {
            tree.set_active(Some(file), false);
        }
        let config = config::get();
        let focus = if file.is_some() || !config.tree {
            Focus::Editor
        } else {
            Focus::Tree
        };
        let theme = Rc::new(Theme::new().map_err(|e| e.to_string())?);
        // An image is shown over the unnamed buffer, which then goes.
        let (file, image) = match file {
            Some(file) if image::is_image(&file) => {
                let image = ImageView::open(&file)
                    .map_err(|reason| format!("{}: {reason}", file.display()))?;
                (None, Some(image))
            }
            file => (file, None),
        };
        let (doc, notice) =
            Document::open(file.clone(), theme.clone()).map_err(|reason| match &file {
                Some(file) => format!("{}: {reason}", file.display()),
                None => reason,
            })?;
        let mut app = App {
            files: FileIndex::new(&workspace),
            symbols: SymbolIndex::new(&workspace),
            index_symbols_after_listing: false,
            watcher: Watcher::new(),
            watching: None,
            workspace,
            keymap: Keymap::new(&config.keys),
            tree,
            documents: vec![doc.clone()],
            theme,
            terminal_colors: TerminalColors::default(),
            theme_shown: None,
            theme_stale: true,
            terminals: Vec::new(),
            next_terminal: 1,
            terminal_prefix: false,
            input_panel: None,
            focused_terminal: None,
            tabs: vec![Tab::new(0, 0)],
            tab: 0,
            next_tab: 1,
            next_panel: 1,
            tab_prompt: None,
            clipboard: None,
            copied: None,
            focus,
            tree_visible: config.tree,
            tree_width: config.tree_width,
            width,
            height,
            mouse_target: None,
            header_drag: None,
            alert: None,
            confirmed: false,
            preview: None,
            last_tree_click: None,
            last_tab_click: None,
            picker: None,
            recent: Vec::new(),
            search: None,
            dialog: None,
            menu: None,
            search_memory: Memory::default(),
            find_memory: find::Memory::default(),
            // Tests keep none; those of recovery set a folder of their own.
            recovery: Recovery::new(match cfg!(test) {
                true => None,
                false => Recovery::default_dir(),
            }),
            settings: match cfg!(test) {
                true => None,
                false => config::path().map(|path| document::resolve(&path)),
            },
            settings_hash: None,
            // Tests keep none; those of sessions set a folder of their own.
            sessions: match cfg!(test) {
                true => None,
                false => session::default_dir(),
            },
            session: None,
            session_saved: None,
            screens_saved: None,
            screens: HashMap::new(),
            attached: true,
        };
        app.settings_hash = app
            .settings
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .map(|bytes| document::content_hash(&bytes));
        if doc.path().is_none() {
            doc.untitled.set(1);
        }
        app.layout();
        app.active_panel_mut()
            .show(&doc)
            .map_err(|e| e.to_string())?;
        if let Some(notice) = notice {
            app.show_message(notice, false);
        }
        if let Some(image) = image {
            app.show_image(image);
        }
        app.note_recent();
        app.watch_folders();
        app.offer_recovery(false);
        Ok(app)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.layout();
    }

    pub fn handle_key(&mut self, key: Key) -> AppAction {
        let action = self.dispatch_key(key);
        self.after_input();
        action
    }

    fn dispatch_key(&mut self, key: Key) -> AppAction {
        self.active_panel_mut().clear_message();
        if let Some(alert) = &mut self.alert {
            let action = alert.handle_key(key);
            return self.alert_action(action);
        }
        if let Some(prompt) = &mut self.tab_prompt {
            match prompt.handle_key(key) {
                PromptKey::Continue => {}
                PromptKey::Cancel => self.tab_prompt = None,
                // An empty name goes back to naming it for what it shows.
                PromptKey::Submit(name) => {
                    self.tab_prompt = None;
                    self.tab_mut().name = (!name.is_empty()).then_some(name);
                }
            }
            return AppAction::Continue;
        }
        let prefixed = std::mem::take(&mut self.terminal_prefix);
        // While its find bar has the keyboard, keys are the bar's.
        if let Some(terminal) = self
            .keyboard_terminal()
            .filter(|terminal| !terminal.borrow().find_focused())
        {
            if terminal.borrow().prompt_open() {
                terminal.borrow_mut().handle_prompt_key(key);
                return AppAction::Continue;
            }
            if terminal.borrow().exit().is_none() {
                if prefixed {
                    return self.prefixed_key(&terminal, key);
                }
                return match self.keymap.lookup_terminal(key) {
                    Some(command) => self.run(command, false),
                    None => {
                        terminal.borrow_mut().send_key(key);
                        AppAction::Continue
                    }
                };
            }
            // With the shell gone, keys are cue's, but Enter starts a
            // new one.
            if key == Key::new(KeyCode::Enter, Mods::NONE) {
                if let Err(err) = terminal.borrow_mut().restart() {
                    self.show_message(format!("Can't start a shell: {err}"), true);
                }
                return AppAction::Continue;
            }
        }
        // The shortcut that opened a terminal's find bar closes it, though
        // Ctrl+Shift+F, there, is Find rather than Search Workspace.
        if self.finding_terminal().is_some()
            && self.keymap.lookup_terminal(key) == Some(Command::Find)
        {
            return self.run(Command::Find, false);
        }
        let context = match self.focus {
            // Its keys are the picker's.
            _ if self.menu.is_some() => Context::Picker,
            _ if self.search.is_some() => Context::Search,
            _ if self.dialog.is_some() => Context::Dialog,
            _ if self.picker.is_some() => Context::Picker,
            Focus::Tree => Context::Tree,
            Focus::Editor if self.finding_terminal().is_some() => Context::Find,
            Focus::Editor => self
                .editor()
                .and_then(Editor::find_field)
                .map_or(Context::Editor, find::Field::context),
        };
        match self.keymap.lookup(key, context) {
            Some((command, select)) => {
                // Other global commands (save, quit, ...) close the picker,
                // search, or file dialog and run as usual.
                if command.context() == Context::Global
                    && !matches!(
                        command,
                        Command::GoToFile
                            | Command::GoToLine
                            | Command::GoToSymbol
                            | Command::GoToWorkspaceSymbol
                            | Command::GoToTerminal
                            | Command::Palette
                            | Command::SearchWorkspace
                            | Command::OpenFile
                            | Command::CreateFile
                            | Command::SaveAs
                    )
                {
                    self.close_popups();
                }
                self.run(command, select)
            }
            None if self.menu.is_some() => AppAction::Continue,
            None if self.query_input().is_some() || self.finding_terminal().is_some() => {
                self.edit_query(key)
            }
            None => {
                if self.focus == Focus::Editor {
                    if let Some(editor) = self.editor_mut() {
                        let action = editor.type_key(key);
                        return self.editor_action(action);
                    }
                }
                AppAction::Continue
            }
        }
    }

    /// The query line of the open popup, or the focused find bar field.
    fn query_input(&mut self) -> Option<&mut dyn QueryInput> {
        if self.search.is_some() {
            return self
                .search
                .as_mut()
                .map(|search| search as &mut dyn QueryInput);
        }
        if self.picker.is_some() {
            return self
                .picker
                .as_mut()
                .map(|picker| picker as &mut dyn QueryInput);
        }
        if self.dialog.is_some() {
            return self
                .dialog
                .as_mut()
                .map(|dialog| dialog as &mut dyn QueryInput);
        }
        if self.focus != Focus::Editor || self.editor().and_then(Editor::find_field).is_none() {
            return None;
        }
        self.editor_mut()
            .map(|editor| editor as &mut dyn QueryInput)
    }

    /// A key for a popup's query: typing, or the editor's keys for moving
    /// the cursor, selecting, deleting, and the clipboard.
    fn edit_query(&mut self, key: Key) -> AppAction {
        let binding = self.keymap.lookup(key, Context::Editor);
        let (command, select) = binding.unzip();
        if self.finding_terminal().is_none() {
            if let Some(input) = self.query_input() {
                match command {
                    Some(Command::SelectAll) => {
                        input.select_all();
                        return AppAction::Continue;
                    }
                    Some(command @ (Command::Copy | Command::Cut)) => {
                        let Some(text) = input.selected_text().map(str::to_string) else {
                            return AppAction::Continue;
                        };
                        if command == Command::Cut {
                            input.edit(Edit::DeleteBackward);
                        }
                        self.clipboard = Some(text.clone());
                        return AppAction::Copy(text);
                    }
                    _ => {}
                }
            }
        }
        let clipboard = self.clipboard.clone();
        let mut buf = [0; 4];
        let edit = match command {
            Some(Command::DeleteBackward) => Edit::DeleteBackward,
            Some(Command::DeleteForward) => Edit::DeleteForward,
            Some(Command::DeleteWordBackward) => Edit::DeleteWordBackward,
            Some(Command::DeleteWordForward) => Edit::DeleteWordForward,
            Some(Command::CursorLeft) => Edit::Left,
            Some(Command::CursorRight) => Edit::Right,
            Some(Command::WordLeft) => Edit::WordLeft,
            Some(Command::WordRight) => Edit::WordRight,
            Some(Command::LineStart | Command::DocumentStart) => Edit::Start,
            Some(Command::LineEnd | Command::DocumentEnd) => Edit::End,
            Some(Command::Paste) => match &clipboard {
                Some(text) => Edit::Insert(text.lines().next().unwrap_or("")),
                None => return AppAction::Continue,
            },
            Some(_) => return AppAction::Continue,
            None => match key.code {
                KeyCode::Char(c) if key.mods.is_plain() => Edit::Insert(c.encode_utf8(&mut buf)),
                _ => return AppAction::Continue,
            },
        };
        if let Some(terminal) = self.finding_terminal() {
            terminal.borrow_mut().find_edit(edit);
        } else if let Some(input) = self.query_input() {
            match select {
                Some(true) => input.edit_selecting(edit),
                _ => input.edit(edit),
            }
        }
        AppAction::Continue
    }

    /// Runs `command`. With `select`, a cursor movement extends the selection.
    pub fn run(&mut self, command: Command, select: bool) -> AppAction {
        match command {
            Command::Quit => return self.quit(),
            Command::Restart => return AppAction::Restart,
            Command::KeepSession if self.session.is_some() => {
                self.show_message("This is already a session.", false)
            }
            Command::KeepSession => {
                self.keep_session(true);
            }
            Command::Detach => {
                if self.session.is_none() && !self.keep_session(false) {
                    return AppAction::Continue;
                }
                return if self.save_session(true) {
                    AppAction::Detach
                } else {
                    AppAction::Continue
                };
            }
            Command::EndSession => return self.end_session(),
            Command::ToggleTree => {
                self.tree_visible = !self.tree_visible;
                if self.tree_visible {
                    self.tree.refresh();
                }
                self.layout();
            }
            // Pressed again, it goes back to the editor.
            Command::FocusTree if self.focus == Focus::Tree => self.focus = Focus::Editor,
            Command::FocusTree => {
                self.tree_visible = true;
                self.layout();
                if self.visible_tree_width() > 0 {
                    // Pick up files created since it was last read.
                    self.tree.refresh();
                    self.focus = Focus::Tree;
                }
            }
            Command::FocusEditor => self.focus = Focus::Editor,
            Command::GoToFile => self.show_picker(Mode::Files),
            Command::GoToLine => self.show_picker(Mode::Line),
            Command::GoToSymbol => self.show_picker(Mode::Symbols),
            Command::GoToWorkspaceSymbol => self.show_picker(Mode::WorkspaceSymbols),
            Command::GoToTerminal => self.show_picker(Mode::Terminals),
            Command::Palette => self.show_picker(Mode::Commands),
            Command::SearchWorkspace => self.show_search(),
            Command::NewFile => self.new_untitled(),
            Command::OpenFile => self.show_dialog(Purpose::Open),
            Command::CreateFile => self.show_dialog(Purpose::Create),
            Command::AddFolder => self.show_dialog(Purpose::AddFolder),
            Command::SaveAs => self.show_dialog(Purpose::SaveAs),
            Command::SplitRight => self.split(Axis::Horizontal),
            Command::PreviewToSide => self.preview_to_side(),
            Command::SplitDown => self.split(Axis::Vertical),
            Command::ClosePanel => self.close_panel(),
            Command::Pop => self.pop(),
            Command::GoBack => self.go_history(true),
            Command::GoForward => self.go_history(false),
            Command::FocusPanelLeft => self.focus_panel(Direction::Left),
            Command::FocusPanelRight => self.focus_panel(Direction::Right),
            Command::FocusPanelUp => self.focus_panel(Direction::Up),
            Command::FocusPanelDown => self.focus_panel(Direction::Down),
            Command::NewTab => self.new_tab(),
            Command::CloseTab => self.close_tab(self.tab),
            Command::NextTab | Command::PreviousTab if self.tabs.len() == 1 => {
                let message = format!(
                    "No other tabs. Use {} to open a new tab.",
                    self.shortcut(Command::NewTab)
                );
                self.show_message(message, false);
            }
            Command::NextTab => self.switch_tab((self.tab + 1) % self.tabs.len()),
            Command::PreviousTab => {
                self.switch_tab((self.tab + self.tabs.len() - 1) % self.tabs.len())
            }
            Command::MoveTabLeft => self.move_tab(self.tab, self.tab.saturating_sub(1)),
            Command::MoveTabRight => self.move_tab(self.tab, self.tab + 1),
            command if command.tab_index().is_some() => {
                let index = command.tab_index().unwrap_or(0);
                if index < self.tabs.len() {
                    self.switch_tab(index);
                } else {
                    let message = format!("Tab {} doesn't exist.", index + 1);
                    self.show_message(message, false);
                }
            }
            Command::RecoverUnsaved => self.offer_recovery(true),
            Command::OpenSettings => self.open_settings(),
            Command::ReloadSettings => self.reload_settings(),
            Command::SelectTheme => {
                self.show_picker(Mode::Themes);
                let shown = self.theme_shown.unwrap_or(ThemeId::TERMINAL);
                if let Some(picker) = &mut self.picker {
                    picker.select_text(shown.name());
                }
            }
            Command::RenameTab => {
                let name = self.tab().name.clone().unwrap_or_default();
                self.tab_prompt = Some(Prompt::new("Rename tab", &name));
            }
            Command::NewTerminal => self.new_terminal(),
            Command::ClearTerminal => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.borrow_mut().clear();
                }
            }
            Command::CloseFile => {
                let file = self.editor().is_some() || self.active_panel().image().is_some();
                if file && self.close_shown(Redo::Run(Command::CloseFile)) {
                    self.show_active_in_tree();
                }
            }
            // Search and the file dialog have the picker's keys, but
            // nothing to close; Ctrl+W closes the panel, as without them.
            Command::PickerCloseItem if self.picker.is_none() => {
                self.close_popups();
                return self.run(Command::ClosePanel, false);
            }
            Command::RenameTerminal => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.borrow_mut().show_rename();
                    // The prompt is in the status bar, and takes the keys.
                    self.focus = Focus::Editor;
                }
            }
            Command::TerminalPrefix if self.keyboard_terminal().is_some() => {
                self.terminal_prefix = true;
                let message = format!(
                    "Next shortcut will go to cue. Press {0} again to send {0} to the shell.",
                    self.shortcut(Command::TerminalPrefix)
                );
                self.show_message(message, false);
            }
            Command::Copy | Command::Cut | Command::Paste if self.active_terminal().is_some() => {
                return self.terminal_clipboard(command);
            }
            Command::Find | Command::FindReplace => {
                self.focus = Focus::Editor;
                let memory = self.find_memory.clone();
                let replacing = command == Command::FindReplace;
                if let Some(terminal) = self.active_terminal() {
                    // Output can't be replaced; it can be found.
                    terminal.borrow_mut().show_find(&memory);
                } else if let Some(editor) = self.editor_mut() {
                    editor.show_find(&memory, replacing);
                }
            }
            // From the command palette, with no find bar open.
            Command::Replace | Command::ReplaceAll
                if !self.editor().is_some_and(Editor::find_open) =>
            {
                return self.run(Command::FindReplace, false);
            }
            Command::FindNext | Command::FindPrevious => {
                let memory = self.find_memory.clone();
                let forward = command == Command::FindNext;
                if let Some(terminal) = self.active_terminal() {
                    // As in other terminals, the next match is the one
                    // above, further back in the output.
                    terminal.borrow_mut().find_step(&memory, forward);
                } else if let Some(editor) = self.editor_mut() {
                    editor.find_step(&memory, forward);
                }
            }
            Command::FindClose if self.active_terminal().is_some() => {
                if let Some(terminal) = self.active_terminal() {
                    terminal.borrow_mut().close_find();
                }
            }
            command
                if Toggle::for_command(command).is_some()
                    && self.search.is_none()
                    && self
                        .active_terminal()
                        .is_some_and(|terminal| terminal.borrow().find_open()) =>
            {
                if let (Some(terminal), Some(toggle)) =
                    (self.active_terminal(), Toggle::for_command(command))
                {
                    terminal.borrow_mut().find_toggle(toggle);
                }
            }
            command
                if matches!(
                    command.context(),
                    Context::Picker | Context::Search | Context::Dialog
                ) || (command.context() == Context::SearchOptions && self.search.is_some()) =>
            {
                if let Some((menu, _)) = &mut self.menu {
                    let action = menu.run(command);
                    return self.menu_action(action);
                }
                if let Some(search) = &mut self.search {
                    let action = search.run(command);
                    return self.search_action(action);
                }
                if let Some(dialog) = &mut self.dialog {
                    let action = dialog.run(command);
                    return self.dialog_action(action);
                }
                let Some(picker) = &mut self.picker else {
                    return AppAction::Continue;
                };
                let action = picker.run(command);
                return self.picker_action(action);
            }
            Command::TreeContextMenu => self.show_tree_menu(),
            Command::TreeOpenToSide
            | Command::TreeNewFile
            | Command::TreeNewFolder
            | Command::TreeRename
            | Command::TreeDuplicate
            | Command::TreeTrash
            | Command::TreeCopyPath
            | Command::TreeCopyRelativePath
            | Command::TreeReveal
            | Command::TreeOpenInTerminal
            | Command::TreeRemoveFolder => {
                let target = self.file_target();
                return self.file_command(command, target, false);
            }
            command if command.context() == Context::Tree => {
                let action = self.tree.run(command);
                self.tree_action(action);
            }
            command => {
                let Some(editor) = self.tabs[self.tab].active_panel_mut().editor_mut() else {
                    return AppAction::Continue;
                };
                let action = editor.run(command, select, &mut self.clipboard);
                self.keep_if_edited();
                return self.editor_action(action);
            }
        }
        AppAction::Continue
    }

    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> AppAction {
        let action = self.dispatch_mouse(mouse, now);
        self.after_input();
        action
    }

    fn dispatch_mouse(&mut self, mouse: Mouse, now: Instant) -> AppAction {
        if let MouseKind::Press(button) | MouseKind::Drag(button) | MouseKind::Release(button) =
            mouse.kind
        {
            if matches!(button, MouseButton::Back | MouseButton::Forward) {
                self.history_button(mouse);
                return AppAction::Continue;
            }
        }
        if let Some(alert) = &mut self.alert {
            // Nor does the rest of a click on it go anywhere else.
            self.mouse_target = None;
            let action = alert.handle_mouse(mouse);
            return self.alert_action(action);
        }
        if let Some((menu, _)) = &mut self.menu {
            let action = menu.handle_mouse(mouse);
            if action != MenuAction::CloseAndPass {
                // Nor does the rest of a click on it go anywhere else.
                self.mouse_target = None;
                return self.menu_action(action);
            }
            self.menu = None;
        }
        if let Some(search) = &mut self.search {
            let action = search.handle_mouse(mouse);
            let action = self.search_action(action);
            self.keep_if_edited();
            return action;
        }
        if let Some(picker) = &mut self.picker {
            let action = picker.handle_mouse(mouse, now);
            let action = self.picker_action(action);
            self.keep_if_edited();
            return action;
        }
        if let Some(dialog) = &mut self.dialog {
            let action = dialog.handle_mouse(mouse, now);
            return self.dialog_action(action);
        }
        let target = match mouse.kind {
            MouseKind::Press(_) => {
                // In case the last drag's release never came.
                self.header_drag = None;
                let at = self.target_at(mouse.x, mouse.y);
                self.mouse_target = at.clone();
                at
            }
            MouseKind::Drag(_) | MouseKind::Release(_) => self.mouse_target.clone(),
            _ => self.target_at(mouse.x, mouse.y),
        };
        if let MouseKind::Release(_) = mouse.kind {
            self.mouse_target = None;
        }
        let Some(target) = target else {
            return AppAction::Continue;
        };
        if let MouseKind::Press(_) = mouse.kind {
            // Like a key press, a click dismisses messages, the terminal
            // prefix, and prompts.
            self.terminal_prefix = false;
            self.tab_prompt = None;
            self.active_panel_mut().clear_message();
            if let Some(terminal) = self.active_terminal() {
                terminal.borrow_mut().cancel_prompt();
            }
        }

        match target {
            MouseTarget::Language => {
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.show_picker(Mode::Languages);
                }
            }
            MouseTarget::Session => {
                if let MouseKind::Press(_) = mouse.kind {
                    self.show_session_menu();
                }
            }
            MouseTarget::Tree => match mouse.kind {
                // Ctrl+click, as on macOS.
                MouseKind::Press(button)
                    if button == MouseButton::Right
                        || (button == MouseButton::Left && mouse.mods.ctrl) =>
                {
                    self.focus = Focus::Tree;
                    // Below the entries, it's for the workspace's folder.
                    let target = match self.tree.select_at(mouse.y) {
                        true => self.tree.selected(),
                        false => self.tree.root(),
                    };
                    if let Some(target) = target {
                        self.open_menu(target, Some((mouse.x, mouse.y)));
                    }
                }
                MouseKind::Press(MouseButton::Left) => {
                    self.focus = Focus::Tree;
                    let double = self.last_tree_click.is_some_and(|(y, time)| {
                        y == mouse.y && now.duration_since(time) < MULTI_CLICK
                    });
                    // A third click starts over rather than counting as another double.
                    self.last_tree_click = (!double).then_some((mouse.y, now));
                    let action = self.tree.click(mouse.y, double);
                    self.tree_action(action);
                }
                MouseKind::ScrollUp => self.tree.scroll(-1),
                MouseKind::ScrollDown => self.tree.scroll(1),
                _ => {}
            },
            MouseTarget::Divider => {
                if let MouseKind::Drag(MouseButton::Left) = mouse.kind {
                    let max = self.width.saturating_sub(MIN_EDITOR_WIDTH + 1);
                    self.tree_width = mouse.x.clamp(MIN_TREE_WIDTH, max.max(MIN_TREE_WIDTH));
                    self.layout();
                }
            }
            MouseTarget::Handle(path) => {
                if let MouseKind::Drag(MouseButton::Left) = mouse.kind {
                    let area = self.main_area();
                    self.tab_mut().layout.drag(&path, area, mouse.x, mouse.y);
                    self.layout();
                }
            }
            MouseTarget::Header(id) => self.drag_header(id, mouse, now),
            MouseTarget::Panel(id) => {
                self.input_panel = Some(id);
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.activate(id);
                    // As Cmd+click in terminals on macOS, which terminals
                    // keep for themselves.
                    if mouse.mods.ctrl && self.open_from_terminal(mouse.x, mouse.y) {
                        self.mouse_target = None;
                        return AppAction::Continue;
                    }
                }
                // The wheel scrolls any panel; the rest goes to the active one.
                let link = self.tab_mut().panel_mut(id).and_then(|panel| {
                    panel.handle_mouse(mouse, now);
                    panel.take_link()
                });
                if let Some(link) = link {
                    self.follow_link(&link);
                }
                self.note_find_memory();
            }
            MouseTarget::Tab(id) => self.tab_mouse(id, mouse, now),
            MouseTarget::NewTab => {
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.new_tab();
                }
            }
        }
        AppAction::Continue
    }

    /// The mouse's back or forward button: goes back or forward in the
    /// panel under it, as in a browser, unless a popup is open.
    fn history_button(&mut self, mouse: Mouse) {
        let MouseKind::Press(button) = mouse.kind else {
            return;
        };
        let popup = self.alert.is_some()
            || self.menu.is_some()
            || self.search.is_some()
            || self.picker.is_some()
            || self.dialog.is_some();
        if popup {
            return;
        }
        if let Some(MouseTarget::Panel(id) | MouseTarget::Header(id)) =
            self.target_at(mouse.x, mouse.y)
        {
            self.activate(id);
        }
        self.go_history(button == MouseButton::Back);
    }

    /// What's at screen cell (`x`, `y`), if anything.
    fn target_at(&self, x: u32, y: u32) -> Option<MouseTarget> {
        let area = self.main_area();
        if y >= area.bottom() {
            if self.height > 1
                && y == self.height - 1
                && x < self.width
                && self.tab_prompt.is_none()
            {
                let status = self.active_panel().status();
                if let Some(badge) = status::session_badge(&status, self.width) {
                    if self.session.is_some() && badge.contains(&x) {
                        return Some(MouseTarget::Session);
                    }
                }
                if let crate::status::Status::EditorInfo { language, .. } = status {
                    if language.contains(&x) {
                        return Some(MouseTarget::Language);
                    }
                }
            }
            return None;
        }
        let tree_width = self.visible_tree_width();
        if tree_width > 0 && x < tree_width {
            return Some(MouseTarget::Tree);
        }
        if tree_width > 0 && x == tree_width {
            return Some(MouseTarget::Divider);
        }
        if y < area.y {
            let (item, ..) = tab::bar(&self.tabs, self.bar_area())
                .into_iter()
                .find(|(_, rect, _)| rect.contains(x, y))?;
            return Some(match item {
                BarItem::Tab(index) => MouseTarget::Tab(self.tabs[index].id),
                BarItem::New => MouseTarget::NewTab,
            });
        }
        let layout = &self.tab().layout;
        if let Some(handle) = layout
            .handles(area)
            .into_iter()
            .find(|handle| handle.axis == Axis::Horizontal && handle.rect.contains(x, y))
        {
            return Some(MouseTarget::Handle(handle.path));
        }
        let (id, rect) = layout
            .panels(area)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))?;
        Some(if y == rect.y {
            MouseTarget::Header(id)
        } else {
            MouseTarget::Panel(id)
        })
    }

    /// A mouse event on panel `id`'s header, or dragged from it: a click
    /// gives the panel the keyboard, and a drag resizes the split above it
    /// or moves the panel (see [`HeaderDrag`]).
    fn drag_header(&mut self, id: PanelId, mouse: Mouse, now: Instant) {
        let area = self.main_area();
        let tab = self.tab;
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                self.activate(id);
                let button = self
                    .tab_mut()
                    .panel_mut(id)
                    .and_then(|panel| panel.header_button(mouse.x));
                if let Some(button) = button {
                    match button {
                        HeaderButton::Back => self.go_history(true),
                        HeaderButton::Forward => self.go_history(false),
                        HeaderButton::Close => self.close_panel(),
                        HeaderButton::Reader => {
                            self.run(Command::ToggleReader, false);
                        }
                    }
                    return;
                }
                let handle = self
                    .tab()
                    .layout
                    .handles(area)
                    .into_iter()
                    .find(|handle| handle.rect.contains(mouse.x, mouse.y))
                    .map(|handle| handle.path);
                self.header_drag = Some(HeaderDrag {
                    handle,
                    x: mouse.x,
                    y: mouse.y,
                    before: self.tab().layout.clone(),
                    resizing: false,
                    moving: false,
                    drop: None,
                });
            }
            MouseKind::Drag(MouseButton::Left) => {
                let Some(drag) = &mut self.header_drag else {
                    return;
                };
                let layout = &mut self.tabs[tab].layout;
                if !drag.moving {
                    let columns = layout.panels(area).into_iter().find(|&(p, _)| p == id);
                    let within = columns
                        .is_some_and(|(_, rect)| (rect.x..rect.x + rect.width).contains(&mouse.x));
                    let (dx, dy) = (mouse.x.abs_diff(drag.x), mouse.y.abs_diff(drag.y));
                    // Cells are about twice as tall as they're wide, so
                    // this is well under 45 degrees from level.
                    let sideways = !drag.resizing && dx >= MOVE_THRESHOLD && dx > 3 * dy;
                    if drag.handle.is_none() || sideways || !within {
                        drag.moving = true;
                        *layout = drag.before.clone();
                    } else if dy > 0 {
                        drag.resizing = true;
                    }
                }
                match (&drag.handle, drag.moving) {
                    (_, true) => drag.drop = layout.drop_at(area, id, mouse.x, mouse.y),
                    (Some(path), false) if drag.resizing => {
                        layout.drag(path, area, mouse.x, mouse.y)
                    }
                    _ => {}
                }
                self.layout();
            }
            MouseKind::Release(MouseButton::Left) => {
                let Some(drag) = self.header_drag.take() else {
                    return;
                };
                if let Some(drop) = drag.drop.filter(|_| drag.moving) {
                    self.tab_mut().layout.drop_panel(id, drop);
                    self.layout();
                }
            }
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                if let Some(panel) = self.tab_mut().panel_mut(id) {
                    panel.handle_mouse(mouse, now);
                }
            }
            _ => {}
        }
    }

    /// A mouse event on tab `id` in the tab bar, or dragged from it: a
    /// click switches to it, a double click renames it, the middle button
    /// closes it, and dragging moves it along the bar.
    fn tab_mouse(&mut self, id: TabId, mouse: Mouse, now: Instant) {
        let Some(index) = self.tab_index(id) else {
            return;
        };
        match mouse.kind {
            MouseKind::Press(MouseButton::Left) => {
                let double = self
                    .last_tab_click
                    .is_some_and(|(tab, time)| tab == id && now.duration_since(time) < MULTI_CLICK);
                self.last_tab_click = (!double).then_some((id, now));
                self.switch_tab(index);
                if double {
                    self.run(Command::RenameTab, false);
                }
            }
            MouseKind::Press(MouseButton::Middle) => self.close_tab(index),
            MouseKind::Drag(MouseButton::Left) => {
                let over = |app: &App| {
                    tab::bar(&app.tabs, app.bar_area())
                        .into_iter()
                        .find(|(_, rect, _)| (rect.x..rect.x + rect.width).contains(&mouse.x))
                        .map(|(item, ..)| item)
                };
                let Some(BarItem::Tab(to)) = over(self) else {
                    return;
                };
                self.move_tab(index, to);
                // Tabs are as wide as their names: moved past a wider one,
                // it may not be under the mouse, and would move right back
                // on the next motion.
                if over(self) != Some(BarItem::Tab(to)) {
                    self.move_tab(to, index);
                }
            }
            _ => {}
        }
    }

    /// Text pasted through the terminal.
    pub fn paste(&mut self, text: &str) {
        if self.alert.is_some() {
            return;
        }
        if let Some(prompt) = &mut self.tab_prompt {
            prompt.paste(text);
            return;
        }
        let terminal = self.keyboard_terminal().filter(|terminal| {
            let terminal = terminal.borrow();
            terminal.prompt_open() || terminal.exit().is_none()
        });
        if let Some(terminal) = self.finding_terminal() {
            let line = text.split(['\r', '\n']).next().unwrap_or("");
            terminal.borrow_mut().find_edit(Edit::Insert(line));
            self.note_find_memory();
        } else if let Some(terminal) = terminal {
            terminal.borrow_mut().paste(text);
        } else if let Some(input) = self.query_input() {
            // Terminals send newlines in pastes as CR.
            let line = text.split(['\r', '\n']).next().unwrap_or("");
            input.edit(Edit::Insert(line));
            self.note_find_memory();
        } else if self.focus == Focus::Editor {
            if let Some(action) = self.editor_mut().map(|editor| editor.paste(text)) {
                self.editor_action(action);
            }
            self.keep_if_edited();
        }
        self.after_input();
    }

    /// Catches up after input: keeps an edited preview, remembers the find
    /// bar's query, puts the active editor's cursor back in the buffer it
    /// shares with other panels, which input to another may have taken,
    /// and watches the folders shown or with files open.
    fn after_input(&mut self) {
        self.keep_if_edited();
        self.note_find_memory();
        for doc in &self.documents {
            doc.follow_edits();
        }
        self.sync_scroll();
        self.editor_mut();
        self.note_terminal_focus();
        self.watch_folders();
        self.feed_picker();
        self.follow_settings();
    }

    /// Draws the frame and returns where the terminal cursor goes (0-based
    /// column, row), or `None` to hide it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        let colors = theme::colors();
        frame.clear(colors.bg);
        let tree_width = self.visible_tree_width();
        let main = self.main_area();
        if tree_width > 0 {
            self.tree
                .draw(frame, 0, tree_width, self.focus == Focus::Tree);
            for y in 0..self.height.saturating_sub(1) {
                frame.draw_text("│", tree_width, y, colors.divider, None, Attributes::NONE);
            }
        }
        if self.bar_height() > 0 {
            tab::draw_bar(frame, &self.tabs, self.tab, self.bar_area());
        }
        let mut cursor = None;
        let tab = self.tab();
        for panel in &tab.panels {
            let active = panel.id == tab.active;
            let preview = panel.document().is_some_and(|doc| self.is_preview(doc));
            let at = panel.draw(frame, &self.keymap, &self.workspace, active, preview);
            if active {
                cursor = at;
            }
        }
        if self.height > 1 {
            let status = match &self.tab_prompt {
                Some(prompt) => prompt.status(),
                None => self.active_panel().status(),
            };
            let y = self.height - 1;
            let session = self.session.is_some();
            if let Some(prompt) = status::draw(frame, &status, y, self.width, &self.keymap, session)
            {
                cursor = Some(prompt);
            }
        }
        for handle in tab.layout.handles(main) {
            if handle.axis == Axis::Horizontal {
                let rect = handle.rect;
                for y in rect.y..rect.y + rect.height {
                    frame.draw_text("│", rect.x, y, colors.divider, None, Attributes::NONE);
                }
            }
        }
        if let Some(drop) = self.header_drag.as_ref().and_then(|drag| drag.drop) {
            let rect = drop.rect;
            frame.fill_rect(rect.x, rect.y, rect.width, rect.height, colors.drop_tint);
        }
        let cursor = self.draw_popups(frame, cursor);
        match &self.alert {
            // Over any popup it asks for.
            Some(alert) => {
                alert.draw(frame);
                None
            }
            None => cursor,
        }
    }

    /// Draws the popup that's open, if any, and returns where the cursor
    /// goes: in it, or else at `cursor` if the editor has the keyboard.
    fn draw_popups(&self, frame: &Buffer, cursor: Option<(u32, u32)>) -> Option<(u32, u32)> {
        if let Some((menu, _)) = &self.menu {
            menu.draw(frame);
            return None;
        }
        if let Some(search) = &self.search {
            return search.draw(frame);
        }
        if let Some(picker) = &self.picker {
            if let (Some(area), Some(Choice::Terminal(id))) =
                (picker.preview_area(), picker.selected_choice())
            {
                if let Some(terminal) = self.terminals.iter().find(|t| t.borrow().id() == *id) {
                    terminal.borrow().draw_preview(frame, area);
                }
            }
            return picker.draw(frame);
        }
        if let Some(dialog) = &self.dialog {
            return dialog.draw(frame);
        }
        cursor.filter(|_| self.focus == Focus::Editor)
    }

    /// Catches up on work in the background: output from terminals,
    /// listing the workspace's files and searching them, and files other
    /// programs changed. Returns whether the screen needs redrawing.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for terminal in &self.terminals {
            let mut terminal = terminal.borrow_mut();
            changed |= terminal.poll();
            if let Some(text) = terminal.take_copied() {
                self.clipboard = Some(text.clone());
                self.copied = Some(text);
            }
        }
        if changed {
            self.prune_terminals();
        }
        changed |= self.search.as_mut().is_some_and(SearchModal::poll);
        if self.files.poll() {
            if self.index_symbols_after_listing && !self.files.listing() {
                self.index_symbols_after_listing = false;
                self.index_symbols();
            }
            if let Some(picker) = &mut self.picker {
                picker.set_files(&self.files);
                changed = true;
            }
        }
        if self.symbols.poll() {
            if let Some(picker) = &mut self.picker {
                picker.set_workspace_symbols(self.symbols.items(), self.symbols.indexing());
                changed = true;
            }
        }
        self.recovery
            .sync(&self.documents, self.workspace.roots(), false);
        let due = |at: Option<Instant>, every| at.is_none_or(|at| at.elapsed() >= every);
        if self.session.is_some() && due(self.session_saved, SESSION_INTERVAL) {
            let screens = due(self.screens_saved, SCREENS_INTERVAL);
            self.save_session(screens);
        }
        if let Some(changes) = self.watcher.poll() {
            changed |= self.disk_changed(changes);
            // Folders may have come back for open files, and the tree may
            // list others.
            self.watching = None;
            self.watch_folders();
            changed |= self.follow_settings();
        }
        changed
    }

    /// Text a terminal's program copied since the last call, for the
    /// system clipboard.
    pub fn take_copied(&mut self) -> Option<String> {
        self.copied.take()
    }

    /// Watches the folders open in the tree, and those of open files, if
    /// they changed since last time.
    fn watch_folders(&mut self) {
        let files: BTreeSet<PathBuf> = self
            .documents
            .iter()
            .filter_map(|doc| doc.path()?.parent().map(Path::to_path_buf))
            .collect();
        let watching = (self.tree.listing(), files);
        if self.watching.as_ref() == Some(&watching) {
            return;
        }
        let mut folders: BTreeSet<PathBuf> = self.tree.open_folders().cloned().collect();
        for folder in &watching.1 {
            // An open file's folder may be gone; the tree's were just listed.
            if !folders.contains(folder) && folder.is_dir() {
                folders.insert(folder.clone());
            }
        }
        self.watcher.watch(folders);
        self.watching = Some(watching);
    }

    /// Catches up with `changes` to files: the tree lists what's in its
    /// open folders now, and open files take changes to them, or, with
    /// unsaved changes, note them for saving to ask about. Returns whether
    /// anything did.
    fn disk_changed(&mut self, changes: Changes) -> bool {
        let listed = changes.rescan || self.tree.is_changed_by(&changes.paths);
        if listed {
            self.tree.refresh();
        }
        // Those changed, or in a folder that moved or went.
        let touched = |doc: &Rc<Document>| {
            doc.path().is_some_and(|path| {
                changes.rescan
                    || changes
                        .paths
                        .iter()
                        .any(|changed| path.starts_with(changed))
            })
        };
        let touched: Vec<Rc<Document>> = self
            .documents
            .iter()
            .filter(|doc| touched(doc))
            .cloned()
            .collect();
        let mut changed = listed;
        for doc in touched {
            let Some(change) = doc.check_disk() else {
                continue;
            };
            changed = true;
            if change == DiskChange::Conflict {
                let message = format!(
                    "{} changed on disk. Saving will prompt you to overwrite it.",
                    self.document_name(&doc)
                );
                self.show_message(message, false);
            }
        }
        if changed {
            // Back into the buffer, for the cursor a reload parked.
            self.editor_mut();
        }
        changed
    }

    /// The terminals' ptys, to wait on with the keyboard: for output, and
    /// with `true`, for taking input that's waiting.
    pub fn watched(&self) -> Vec<(RawFd, bool)> {
        self.terminals
            .iter()
            .filter_map(|terminal| terminal.borrow().watch())
            .collect()
    }

    // --- panels -----------------------------------------------------------------

    /// Splits the active panel, with a new, empty one right of it or below
    /// it, which becomes active.
    fn split(&mut self, axis: Axis) {
        let area = self.active_panel().area();
        let room = match axis {
            Axis::Horizontal => area.width > 2 * layout::MIN_WIDTH,
            Axis::Vertical => area.height >= 2 * layout::MIN_HEIGHT,
        };
        if !room {
            self.show_message("Not enough space to split this panel.", false);
            return;
        }
        let id = self.next_panel;
        self.next_panel += 1;
        let tab = self.tab_mut();
        tab.layout.split(tab.active, id, axis);
        tab.panels.push(Panel::new(id));
        self.layout();
        self.activate(id);
    }

    /// Shows the Markdown file on screen in reader mode in a new panel to
    /// the right, which scrolls along with it, and keeps the keyboard here,
    /// editing. A panel on screen reading it already will do instead.
    fn preview_to_side(&mut self) {
        let Some(editor) = self.editor() else {
            self.show_message("Open a Markdown file to preview it.", false);
            return;
        };
        if !editor.is_markdown() {
            self.show_message("Previews are for Markdown files.", false);
            return;
        }
        let doc = editor.document().clone();
        let here = self.tab().active;
        let Some(anchor) = self.editor_mut().map(|editor| {
            editor.set_reading(false);
            editor.scroll_anchor()
        }) else {
            return;
        };
        let shown = self.tab().panels.iter().any(|panel| {
            panel.id != here && panel.shows(&doc) && panel.editor().is_some_and(Editor::reading)
        });
        if !shown {
            self.split(Axis::Horizontal);
            if self.tab().active == here {
                // There wasn't room.
                return;
            }
            if let Err(err) = self.active_panel_mut().show(&doc) {
                self.show_message(format!("Can't show a preview: {err}"), true);
                self.activate(here);
                return;
            }
            if let Some(editor) = self.editor_mut() {
                editor.read_from(0);
                editor.follow(anchor);
            }
            self.activate(here);
            return;
        }
        for panel in &mut self.tab_mut().panels {
            if panel.id != here {
                panel.follow(&doc, false, anchor);
            }
        }
    }

    /// Scrolls views of a Markdown file in one mode along with the view of
    /// it in the other mode that the last input went to, so that an editor
    /// and a reader of it side by side keep to the same place. Views in
    /// the same mode are left alone, to show different places.
    fn sync_scroll(&mut self) {
        let leader = self.input_panel.take().unwrap_or(self.tab().active);
        let tab = &mut self.tabs[self.tab];
        // Moving the cursor around doesn't move the others; scrolling,
        // editing, and switching modes do.
        let Some(editor) = tab
            .panels
            .iter()
            .find(|panel| panel.id == leader)
            .and_then(Panel::editor)
            .filter(|editor| editor.is_markdown() && editor.moved())
        else {
            return;
        };
        let doc = editor.document().clone();
        let reading = editor.reading();
        let anchor = editor.scroll_anchor();
        for panel in tab.panels.iter_mut().filter(|panel| panel.id != leader) {
            panel.follow(&doc, reading, anchor);
        }
    }

    /// Closes the active panel and what it shows (see
    /// [`App::close_shown`]), giving its room to its neighbor, which becomes
    /// active. The tab's last panel is emptied instead, and once empty,
    /// closes the tab, unless it's the only one. Files it showed before
    /// stay open.
    fn close_panel(&mut self) {
        let tab = self.tab();
        if tab.panels.len() == 1 && tab.active_panel().is_empty() && self.tabs.len() > 1 {
            self.close_tab(self.tab);
            return;
        }
        if !self.close_shown(Redo::Run(Command::ClosePanel)) {
            return;
        }
        let tab = self.tab_mut();
        match tab.layout.remove(tab.active) {
            Some(next) => {
                let closed = tab.active;
                tab.panels.retain(|panel| panel.id != closed);
                self.layout();
                self.activate(next);
            }
            None => {
                self.active_panel_mut().clear();
                self.show_active_in_tree();
            }
        }
        self.prune_documents();
        self.prune_terminals();
    }

    /// Closes what the active panel shows (see [`App::close_shown`]) and
    /// shows what it showed before, as going back does, leaving the panel
    /// empty if there's nothing to go back to. What it closed is gone from
    /// the panel's history.
    fn pop(&mut self) {
        if self.active_panel().is_empty() {
            self.show_message("Nothing to close.", false);
            return;
        }
        let visit = self.active_panel().visit();
        if !self.close_shown(Redo::Run(Command::Pop)) {
            return;
        }
        if let Some(visit) = &visit {
            self.active_panel_mut().drop_visit(visit);
        }
        if !self.step_history(true) {
            self.show_active_in_tree();
        }
        self.prune_documents();
        self.prune_terminals();
    }

    /// Shows what the active panel showed before what it shows now, or
    /// with `back` false, what it went back from, or says there's nothing
    /// to (see [`App::step_history`]).
    fn go_history(&mut self, back: bool) {
        if !self.step_history(back) {
            let message = match back {
                true => "Nothing to go back to.",
                false => "Nothing to go forward to.",
            };
            self.show_message(message, false);
        }
    }

    /// Shows what the active panel showed before what it shows now, or
    /// with `back` false, what it went back from. A closed file opens
    /// again; closed terminals, and those another panel shows now, are
    /// skipped. Returns false if there was nowhere to go.
    fn step_history(&mut self, back: bool) -> bool {
        let active = self.tab().active;
        let elsewhere: Vec<u32> = tab::all_panels(&self.tabs)
            .filter(|panel| panel.id != active)
            .filter_map(Panel::terminal)
            .map(|terminal| terminal.borrow().id())
            .collect();
        let (documents, terminals) = (&self.documents, &self.terminals);
        let usable = |visit: &Visit| match visit {
            Visit::File(doc, path) => {
                let open = doc
                    .upgrade()
                    .is_some_and(|doc| documents.iter().any(|d| Rc::ptr_eq(d, &doc)));
                open || path.as_ref().is_some_and(|path| path.is_file())
            }
            Visit::Terminal(id) => {
                !elsewhere.contains(id) && terminals.iter().any(|t| t.borrow().id() == *id)
            }
            Visit::Image(path) => path.is_file(),
        };
        let Some(visit) = self.tabs[self.tab]
            .active_panel_mut()
            .step_history(back, usable)
        else {
            return false;
        };
        let history = self.active_panel_mut().take_history();
        match visit {
            Visit::File(doc, path) => {
                let doc = doc
                    .upgrade()
                    .filter(|doc| self.documents.iter().any(|d| Rc::ptr_eq(d, doc)));
                match (doc, path) {
                    (Some(doc), _) => self.show_document(&doc),
                    (None, Some(path)) => {
                        if self.open(&path, false) {
                            self.focus = Focus::Editor;
                        }
                    }
                    (None, None) => {}
                }
            }
            Visit::Terminal(id) => {
                let terminal = self.terminals.iter().find(|t| t.borrow().id() == id);
                if let Some(terminal) = terminal.cloned() {
                    self.show_terminal(terminal);
                }
            }
            Visit::Image(path) => {
                if self.open_image(&path) {
                    self.focus = Focus::Editor;
                }
            }
        }
        self.active_panel_mut().restore_history(history);
        true
    }

    /// Moves the keyboard to the panel next to the active one in
    /// `direction`. Left of the leftmost panels is the file tree.
    fn focus_panel(&mut self, direction: Direction) {
        if self.focus == Focus::Tree {
            if direction == Direction::Right {
                self.focus = Focus::Editor;
            }
            return;
        }
        let tab = self.tab();
        match tab.layout.neighbor(self.main_area(), tab.active, direction) {
            Some(id) => self.activate(id),
            None if direction == Direction::Left => {
                self.run(Command::FocusTree, false);
            }
            None => {}
        }
    }

    /// Gives the keyboard to panel `id`. The status bar is its now, so the
    /// last panel's message goes.
    fn activate(&mut self, id: PanelId) {
        self.focus = Focus::Editor;
        let tab = self.tab_mut();
        if tab.active != id {
            // It may have just closed.
            let active = tab.active;
            if let Some(panel) = tab.panel_mut(active) {
                panel.clear_message();
            }
            tab.previous = Some(active);
            tab.active = id;
            self.show_active_in_tree();
        }
    }

    // --- tabs -------------------------------------------------------------------

    /// Opens a new tab after the one on screen, with one empty panel, and
    /// switches to it.
    fn new_tab(&mut self) {
        let tab = Tab::new(self.next_tab, self.next_panel);
        self.next_tab += 1;
        self.next_panel += 1;
        self.tabs.insert(self.tab + 1, tab);
        self.switch_tab(self.tab + 1);
    }

    /// Puts the tab at `index` on screen, and gives the keyboard to its
    /// active panel. The status bar is that panel's now, so the last one's
    /// message goes.
    fn switch_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active_panel_mut().clear_message();
        self.tab_prompt = None;
        self.tab = index;
        self.focus = Focus::Editor;
        // The screen may have changed size while it was away.
        self.layout();
        self.show_active_in_tree();
    }

    /// Moves the tab at `from` to `to` in the bar.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let current = self.tab().id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.tab = self.tab_index(current).unwrap_or(0);
    }

    fn tab_index(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Closes the tab at `index`, and what its panels show, as closing each
    /// of them would (see [`App::close_shown`]). If that loses unsaved
    /// changes or stops a program, it asks first. The tab after it takes
    /// its place on screen, or the one before it. The only tab is emptied
    /// instead, down to one empty panel.
    fn close_tab(&mut self, index: usize) {
        let Some(closing) = self.tabs.get(index) else {
            return;
        };
        // What its panels show, except files panels in other tabs show too.
        let elsewhere = |doc: &Rc<Document>| {
            self.tabs
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != index)
                .flat_map(|(_, tab)| &tab.panels)
                .any(|panel| panel.shows(doc))
        };
        let mut documents: Vec<Rc<Document>> = Vec::new();
        for doc in closing.panels.iter().filter_map(Panel::document) {
            if !elsewhere(doc) && !documents.iter().any(|d| Rc::ptr_eq(d, doc)) {
                documents.push(doc.clone());
            }
        }
        let terminals: Vec<Rc<RefCell<Terminal>>> = closing
            .panels
            .iter()
            .filter_map(Panel::terminal)
            .cloned()
            .collect();
        let unsaved: Vec<Rc<Document>> = documents
            .iter()
            .filter(|doc| doc.is_modified())
            .cloned()
            .collect();
        let running = running_programs(&terminals);
        let title = format!("Close tab {}?", index + 1);
        let redo = Redo::CloseTab(closing.id);
        if !self.ask_first(title, unsaved, &running, "&Close Tab", redo) {
            return;
        }
        self.tab_prompt = None;
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new(self.next_tab, self.next_panel));
            self.next_tab += 1;
            self.next_panel += 1;
        }
        if index < self.tab || self.tab >= self.tabs.len() {
            self.tab -= 1;
        }
        for terminal in &terminals {
            self.destroy_terminal(terminal);
        }
        for doc in &documents {
            self.destroy_document(doc);
        }
        self.layout();
        self.prune_documents();
        self.show_active_in_tree();
    }

    // --- picker -----------------------------------------------------------------

    /// Opens the picker listing `mode`, or switches it to `mode`. Pressed
    /// again, the same shortcut closes it.
    fn show_picker(&mut self, mode: Mode) {
        self.menu = None;
        self.close_search();
        self.dialog = None;
        match &mut self.picker {
            Some(picker) if picker.mode() == mode => self.picker = None,
            Some(picker) => picker.set_mode(mode),
            None => {
                if matches!(mode, Mode::Files | Mode::WorkspaceSymbols) {
                    // Files may have come or gone since the last listing,
                    // which is shown until this one is done.
                    self.files.refresh();
                }
                let recent = self.recent_items();
                // Terminal commands, for the terminal on screen, and
                // session commands as it is one or not.
                let terminal = self.active_terminal().is_some();
                let session = self.session.is_some();
                let available = |command: Command| match command {
                    Command::KeepSession => !session && self.sessions.is_some(),
                    Command::EndSession => session,
                    Command::Detach => self.sessions.is_some(),
                    command => command.context() != Context::Terminal || terminal,
                };
                self.picker = Some(Picker::new(
                    mode,
                    &self.keymap,
                    available,
                    recent,
                    &self.files,
                    self.width,
                    self.height,
                ));
            }
        }
    }

    fn picker_action(&mut self, action: PickerAction) -> AppAction {
        match action {
            PickerAction::Continue => {}
            PickerAction::Close => self.picker = None,
            PickerAction::CloseItem(choice) => {
                self.close_item(choice);
                let recent = self.recent_items();
                if let Some(picker) = &mut self.picker {
                    picker.set_recent(recent);
                }
            }
            PickerAction::Accept(choice) => {
                self.picker = None;
                match choice {
                    Choice::File(path) => {
                        if self.open(&path, false) {
                            self.focus = Focus::Editor;
                        }
                    }
                    Choice::FileAt(path, position) => {
                        if self.open(&path, false) {
                            self.focus = Focus::Editor;
                            self.go_to(position);
                        }
                    }
                    Choice::Line(position) => {
                        self.focus = Focus::Editor;
                        self.go_to(position);
                    }
                    Choice::Symbol(path, line, bytes) => {
                        let shown = match &path {
                            Some(path) => self.open(path, false),
                            None => self.editor().is_some(),
                        };
                        if let Some(editor) = self.editor_mut().filter(|_| shown) {
                            editor.select_in_line(line, bytes);
                            self.focus = Focus::Editor;
                        }
                    }
                    Choice::Theme(id) => self.choose_theme(id),
                    Choice::Language(name) => {
                        if let Some(editor) = self.editor() {
                            let language = name.and_then(|name| {
                                crate::language::all().find(|language| language.name == name)
                            });
                            editor.document().set_language(language);
                        }
                    }
                    Choice::Command(command) => return self.run(command, false),
                    Choice::Terminal(id) => {
                        let terminal = self.terminals.iter().find(|t| t.borrow().id() == id);
                        if let Some(terminal) = terminal.cloned() {
                            self.show_terminal(terminal);
                        }
                    }
                    Choice::Untitled(number) => {
                        if let Some(doc) = self.find_untitled(number) {
                            self.show_document(&doc);
                        }
                    }
                }
            }
        }
        AppAction::Continue
    }

    /// Gives the picker what its mode lists, if it's waiting for it: the
    /// symbols of the file on screen, or the workspace's.
    fn feed_picker(&mut self) {
        if self.picker.as_ref().is_some_and(Picker::wants_outline) {
            let items = self.outline_items();
            if let Some(picker) = &mut self.picker {
                picker.set_outline(items);
            }
        }
        if self
            .picker
            .as_mut()
            .is_some_and(Picker::take_symbols_request)
        {
            self.index_symbols();
            let (items, indexing) = (self.symbols.items(), self.symbols.indexing());
            if let Some(picker) = &mut self.picker {
                picker.set_workspace_symbols(items, indexing);
            }
        }
    }

    /// The symbols the file on screen defines, in order.
    fn outline_items(&self) -> Vec<Item> {
        let Some(doc) = self.editor().map(Editor::document) else {
            return Vec::new();
        };
        let Some(language) = doc.language.get() else {
            return Vec::new();
        };
        symbols::outline(language, &doc.text())
            .into_iter()
            .map(Item::symbol)
            .collect()
    }

    /// Indexes the workspace's symbols again, in the background, once its
    /// files are listed.
    fn index_symbols(&mut self) {
        if self.files.listing() {
            self.index_symbols_after_listing = true;
            return;
        }
        let files = self
            .files
            .files()
            .iter()
            .filter_map(|item| item.path().map(Path::to_path_buf))
            .collect();
        self.symbols.refresh(files);
    }

    /// Puts the active editor's cursor at `position`.
    pub fn go_to(&mut self, position: Position) {
        if let Some(editor) = self.editor_mut() {
            editor.go_to(position);
        }
    }

    // --- search -----------------------------------------------------------------

    /// Opens workspace search, or closes it if it's open. A selection in
    /// the editor on a single line becomes the query; otherwise the last
    /// query is kept.
    fn show_search(&mut self) {
        if self.search.is_some() {
            self.close_search();
            return;
        }
        self.menu = None;
        self.picker = None;
        self.dialog = None;
        let selected = self
            .editor()
            .and_then(Editor::selected_text)
            .filter(|text| !text.contains('\n'));
        // Open files are searched as they are, saved or not.
        let unsaved: HashMap<PathBuf, String> = self
            .documents
            .iter()
            .filter(|doc| doc.is_modified())
            .filter_map(|doc| Some((doc.path()?, doc.text())))
            .collect();
        self.search = Some(SearchModal::new(
            &self.workspace,
            &self.keymap,
            self.search_memory.clone(),
            selected,
            unsaved,
            self.width,
            self.height,
        ));
    }

    /// Closes workspace search, keeping its query for next time.
    fn close_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.search_memory = search.memory();
        }
    }

    fn close_popups(&mut self) {
        self.menu = None;
        self.picker = None;
        self.dialog = None;
        self.close_search();
    }

    fn search_action(&mut self, action: SearchAction) -> AppAction {
        match action {
            SearchAction::Continue => {}
            SearchAction::Close => self.close_search(),
            SearchAction::Open { path, line, range } => {
                self.close_search();
                if self.open(&path, false) {
                    if let Some(editor) = self.editor_mut() {
                        editor.select_in_line(line, range);
                    }
                    self.focus = Focus::Editor;
                }
            }
        }
        AppAction::Continue
    }

    // --- files ------------------------------------------------------------------

    /// Shows the file at `path` in the active panel, opening it if it isn't
    /// open yet. A newly opened file takes the place of an unnamed buffer
    /// left behind that was never typed in, or as a `preview`, of the
    /// previous preview, unless another panel shows it. Opening a preview
    /// without `preview` keeps it. Returns false, with a message, if the
    /// file can't be opened.
    fn open(&mut self, path: &Path, preview: bool) -> bool {
        let path = document::resolve(path);
        self.active_panel_mut().clear_message();
        // Coming back to a file, the keyboard is for its text.
        if let Some(editor) = self.editor_mut() {
            editor.blur_find();
        }
        if image::is_image(&path) {
            return self.open_image(&path);
        }
        let existing = self.find_document(&path);
        let had_terminal = self.active_terminal().is_some();
        let (doc, notice) = match &existing {
            Some(doc) => (doc.clone(), None),
            None => match Document::open(Some(path.clone()), self.theme.clone()) {
                Ok(opened) => opened,
                Err(reason) => {
                    self.cant_open(&reason, &path);
                    return false;
                }
            },
        };
        let left = self.active_panel().document().cloned();
        if let Err(err) = self.active_panel_mut().show(&doc) {
            self.cant_open(&err.to_string(), &path);
            return false;
        }
        if existing.is_none() {
            let replaced = match left {
                Some(left) if left.is_blank() => Some(left),
                _ if preview => self.preview.clone().filter(|doc| !doc.is_modified()),
                _ => None,
            };
            let replaced = replaced
                .filter(|old| !tab::all_panels(&self.tabs).any(|panel| panel.shows(old)))
                .and_then(|old| {
                    let index = self.documents.iter().position(|d| Rc::ptr_eq(d, &old))?;
                    Some((index, old))
                });
            match replaced {
                Some((index, old)) => {
                    for panel in tab::all_panels_mut(&mut self.tabs) {
                        panel.forget(&old);
                    }
                    self.documents[index] = doc.clone();
                }
                None => self.documents.push(doc.clone()),
            }
            if preview {
                self.preview = Some(doc);
            }
            if let Some(notice) = notice {
                self.show_message(notice, false);
            }
        } else if !preview && self.preview.as_ref().is_some_and(|p| Rc::ptr_eq(p, &doc)) {
            self.preview = None;
        }
        self.prune_documents();
        if had_terminal {
            self.prune_terminals();
        }
        self.show_active_in_tree();
        true
    }

    /// Shows the image at `path` in the active panel, unless it's on
    /// screen there already. Returns false, with a message, if it can't
    /// be opened.
    fn open_image(&mut self, path: &Path) -> bool {
        let shown = self
            .active_panel()
            .image()
            .is_some_and(|image| image.path() == path);
        if !shown {
            match ImageView::open(path) {
                Ok(image) => self.show_image(image),
                Err(reason) => {
                    self.cant_open(&reason, path);
                    return false;
                }
            }
        }
        self.show_active_in_tree();
        true
    }

    fn show_image(&mut self, image: ImageView) {
        let had_terminal = self.active_terminal().is_some();
        self.active_panel_mut().show_image(image);
        self.prune_documents();
        if had_terminal {
            self.prune_terminals();
        }
    }

    /// Stops showing images of `path`, or of files in it if it's a folder,
    /// in every panel.
    fn close_images(&mut self, path: &Path) {
        for panel in tab::all_panels_mut(&mut self.tabs) {
            if panel
                .image()
                .is_some_and(|image| image.path().starts_with(path))
            {
                panel.hide_image();
            }
        }
    }

    fn cant_open(&mut self, reason: &str, path: &Path) {
        // Reason first: the status bar clips long paths on the right.
        let message = format!(
            "Can't open: {reason} ({})",
            self.workspace.display_path(path)
        );
        self.show_message(message, true);
    }

    /// Closes unnamed files that were never typed in and no panel has, in
    /// any tab.
    fn prune_documents(&mut self) {
        let tabs = &self.tabs;
        self.documents
            .retain(|doc| !doc.is_blank() || tab::all_panels(tabs).any(|panel| panel.has(doc)));
        let documents = &self.documents;
        self.recent.retain(|recent| match recent {
            Recent::Untitled(number) => documents
                .iter()
                .any(|doc| doc.path().is_none() && doc.untitled.get() == *number),
            Recent::File(_) | Recent::Terminal(_) => true,
        });
    }

    /// Keeps the find bar's query for finding in other files and
    /// terminals.
    fn note_find_memory(&mut self) {
        let memory = match self.active_terminal() {
            Some(terminal) => terminal.borrow().find_memory().cloned(),
            None => self.editor().and_then(Editor::find_memory).cloned(),
        };
        if let Some(memory) = memory {
            if memory != self.find_memory {
                self.find_memory = memory;
            }
        }
    }

    /// An edited preview is kept open.
    fn keep_if_edited(&mut self) {
        if self.preview.as_ref().is_some_and(|doc| doc.is_modified()) {
            self.preview = None;
            self.show_active_in_tree();
        }
    }

    /// Marks the active panel's file in the tree.
    fn show_active_in_tree(&mut self) {
        let path = self.active_panel().path();
        let doc = self.active_panel().document();
        let preview = doc.is_some_and(|doc| self.is_preview(doc));
        self.tree.set_active(path.as_deref(), preview);
        self.note_recent();
    }

    fn is_preview(&self, doc: &Rc<Document>) -> bool {
        self.preview.as_ref().is_some_and(|p| Rc::ptr_eq(p, doc))
    }

    /// What's on screen in the active panel, as the picker lists it.
    fn shown(&self) -> Option<Recent> {
        if let Some(terminal) = self.active_terminal() {
            return Some(Recent::Terminal(terminal.borrow().id()));
        }
        if let Some(image) = self.active_panel().image() {
            return Some(Recent::File(image.path().to_path_buf()));
        }
        self.active_panel()
            .document()
            .map(|doc| document_target(doc))
    }

    /// Moves the file or terminal on screen to the front of the recent
    /// ones.
    fn note_recent(&mut self) {
        let Some(shown) = self.shown() else {
            return;
        };
        self.recent.retain(|recent| *recent != shown);
        self.recent.insert(0, shown);
        self.recent.truncate(RECENT_FILES);
    }

    /// What the picker lists first: the files and terminals shown recently,
    /// most recent first, then any other terminals and untitled files.
    /// What's on screen is left out, so that Enter goes back to what was
    /// shown before it.
    fn recent_items(&self) -> Vec<Item> {
        let shown = self.shown();
        let terminal_item = |terminal: &Rc<RefCell<Terminal>>| {
            let terminal = terminal.borrow();
            let (name, about) = terminal.label();
            Item::terminal(terminal.id(), &name, &about)
        };
        let mut items: Vec<Item> = self
            .recent
            .iter()
            .filter(|recent| shown.as_ref() != Some(*recent))
            .filter_map(|recent| match recent {
                Recent::File(path) => path
                    .is_file()
                    .then(|| Item::file(path.clone(), &self.workspace, "recent")),
                Recent::Terminal(id) => self
                    .terminals
                    .iter()
                    .find(|terminal| terminal.borrow().id() == *id)
                    .map(terminal_item),
                Recent::Untitled(number) => {
                    self.find_untitled(*number).map(|_| Item::untitled(*number))
                }
            })
            .collect();
        let others = self.terminals.iter().map(|terminal| {
            (
                Recent::Terminal(terminal.borrow().id()),
                terminal_item(terminal),
            )
        });
        let untitled = self
            .documents
            .iter()
            .filter(|doc| doc.path().is_none())
            .map(|doc| doc.untitled.get())
            .map(|number| (Recent::Untitled(number), Item::untitled(number)));
        for (recent, item) in others.chain(untitled) {
            if !self.recent.contains(&recent) && shown.as_ref() != Some(&recent) {
                items.push(item);
            }
        }
        items
    }

    /// The open file at `path`, however it was named.
    fn find_document(&self, path: &Path) -> Option<Rc<Document>> {
        self.documents.iter().find(|doc| doc.is_file(path)).cloned()
    }

    // --- closing ----------------------------------------------------------------

    /// Closes what the active panel shows, as closing the panel does,
    /// leaving it empty: a terminal, or a file, unless another panel, in
    /// any tab, shows it too. Returns false if it asked first instead, to
    /// `redo` once answered (see [`App::ask_first`]).
    fn close_shown(&mut self, redo: Redo) -> bool {
        if let Some(terminal) = self.active_terminal() {
            let title = format!("Close {}?", terminal.borrow().name());
            let running = running_programs(std::slice::from_ref(&terminal));
            if !self.ask_first(title, Vec::new(), &running, "&Close", redo) {
                return false;
            }
            self.destroy_terminal(&terminal);
            return true;
        }
        if self.active_panel().image().is_some() {
            self.active_panel_mut().hide_image();
            return true;
        }
        let Some(doc) = self.active_panel().document().cloned() else {
            return true;
        };
        let active = self.tab().active;
        if tab::all_panels(&self.tabs).any(|panel| panel.id != active && panel.shows(&doc)) {
            self.active_panel_mut().forget(&doc);
            return true;
        }
        if !self.ask_to_close(&doc, redo) {
            return false;
        }
        self.destroy_document(&doc);
        true
    }

    /// Closes a recent file, terminal, or untitled file the picker lists,
    /// wherever it's shown, asking first as closing a panel does. A file
    /// that isn't open is only no longer listed as recent.
    fn close_item(&mut self, choice: Choice) {
        let redo = Redo::CloseItem(choice.clone());
        match choice {
            Choice::File(path) => {
                if let Some(doc) = self.find_document(&path) {
                    if !self.ask_to_close(&doc, redo) {
                        return;
                    }
                    self.destroy_document(&doc);
                }
                self.close_images(&path);
                self.recent
                    .retain(|recent| *recent != Recent::File(path.clone()));
            }
            Choice::Untitled(number) => {
                if let Some(doc) = self.find_untitled(number) {
                    if self.ask_to_close(&doc, redo) {
                        self.destroy_document(&doc);
                    }
                }
            }
            Choice::Terminal(id) => {
                let terminal = self.terminals.iter().find(|t| t.borrow().id() == id);
                if let Some(terminal) = terminal.cloned() {
                    let title = format!("Close {}?", terminal.borrow().name());
                    let running = running_programs(std::slice::from_ref(&terminal));
                    if self.ask_first(title, Vec::new(), &running, "&Close", redo) {
                        self.destroy_terminal(&terminal);
                    }
                }
            }
            Choice::FileAt(..)
            | Choice::Line(_)
            | Choice::Symbol(..)
            | Choice::Command(_)
            | Choice::Language(_)
            | Choice::Theme(_) => {}
        }
    }

    /// Whether to go ahead closing `doc` now, or ask first, to `redo` once
    /// answered, as it has unsaved changes.
    fn ask_to_close(&mut self, doc: &Rc<Document>, redo: Redo) -> bool {
        let title = format!("Close {}?", self.document_name(doc));
        let unsaved = match doc.is_modified() {
            true => vec![doc.clone()],
            false => Vec::new(),
        };
        self.ask_first(title, unsaved, &[], "&Close", redo)
    }

    /// Whether to go ahead now with what would lose `unsaved` files'
    /// changes or stop `running` programs: if it loses nothing, or an alert
    /// about it was just answered. Otherwise it asks, titled `title`, to
    /// `redo` once answered: saving first, where the files have names, or
    /// not, or, with only programs to stop, going ahead as `go` says.
    fn ask_first(
        &mut self,
        title: String,
        unsaved: Vec<Rc<Document>>,
        running: &[String],
        go: &str,
        redo: Redo,
    ) -> bool {
        if self.confirmed || (unsaved.is_empty() && running.is_empty()) {
            return true;
        }
        let names: Vec<String> = unsaved.iter().map(|doc| self.document_name(doc)).collect();
        let message = losing_reasons(&names, running);
        let mut buttons = Vec::new();
        if !unsaved.is_empty() && unsaved.iter().all(|doc| doc.path().is_some()) {
            let save = if unsaved.len() == 1 {
                "&Save"
            } else {
                "&Save All"
            };
            buttons.push(Button::new(
                save,
                Answer::Save(unsaved.clone(), redo.clone()),
            ));
        }
        let go = if unsaved.is_empty() {
            go
        } else {
            "Do&n't Save"
        };
        buttons.push(Button::new(go, Answer::Go(redo)).danger());
        self.alert = Some(Alert::new(title, message, buttons, self.width, self.height));
        false
    }

    /// Asks whether saving `doc` should overwrite the file, which changed
    /// on disk since, or take its text instead, then carry on `saving`.
    fn ask_overwrite(&mut self, doc: Rc<Document>, saving: Option<Saving>) {
        let name = self.document_name(&doc);
        let message = format!("{name} changed on disk since it was opened or last saved.");
        let buttons = vec![
            Button::new("&Overwrite", Answer::Overwrite(doc.clone(), saving.clone())).danger(),
            Button::new("&Revert", Answer::Revert(doc, saving)).danger(),
        ];
        let title = format!("Save {name}?");
        self.alert = Some(Alert::new(title, message, buttons, self.width, self.height));
    }

    /// Saves `doc` over its file, whatever is there.
    fn overwrite(&mut self, doc: &Rc<Document>) -> AppAction {
        let Some(path) = doc.path() else {
            return AppAction::Continue;
        };
        let action = match self.editor_mut() {
            Some(editor) if Rc::ptr_eq(editor.document(), doc) => editor.write(path),
            _ => match doc.save(&path) {
                Ok(()) => Action::Saved,
                Err(err) => {
                    self.show_message(format!("Can't save: {err} ({})", path.display()), true);
                    Action::Continue
                }
            },
        };
        self.editor_action(action)
    }

    fn alert_action(&mut self, action: AlertAction<Answer>) -> AppAction {
        let answer = match action {
            AlertAction::Continue => return AppAction::Continue,
            AlertAction::Cancel => {
                self.alert = None;
                return AppAction::Continue;
            }
            AlertAction::Answer(answer) => answer,
        };
        self.alert = None;
        let saving = match answer {
            Answer::Go(redo) => return self.go(redo),
            Answer::Recover(orphans) => {
                self.recover(&orphans);
                return AppAction::Continue;
            }
            Answer::DiscardRecovered(orphans) => {
                recovery::remove(&orphans);
                return AppAction::Continue;
            }
            Answer::KeepSession => {
                return match self.keep_session(false) {
                    true => self.leave_session(),
                    false => AppAction::Continue,
                };
            }
            Answer::Save(docs, redo) => Saving { docs, redo },
            Answer::Overwrite(doc, saving) => {
                let action = self.overwrite(&doc);
                match saving {
                    Some(saving) if !doc.is_modified() => saving,
                    _ => return action,
                }
            }
            Answer::Revert(doc, saving) => {
                let name = self.document_name(&doc);
                if let Err(err) = doc.revert() {
                    self.show_message(format!("Can't revert {name}: {err}"), true);
                    return AppAction::Continue;
                }
                self.show_message(format!("Reloaded {name} from disk."), false);
                match saving {
                    Some(saving) => saving,
                    None => return AppAction::Continue,
                }
            }
        };
        self.save_then_go(saving)
    }

    /// Saves `saving`'s files, then goes ahead. A file that changed on disk
    /// since stops it to ask whether to overwrite the file, then it carries
    /// on; one that can't be saved stops it.
    fn save_then_go(&mut self, saving: Saving) -> AppAction {
        let Saving { mut docs, redo } = saving;
        while !docs.is_empty() {
            let doc = docs.remove(0);
            let Some(path) = doc.path() else { continue };
            // It may have changed since the alert was put up.
            doc.check_disk();
            if doc.disk() == Disk::Changed {
                self.ask_overwrite(doc, Some(Saving { docs, redo }));
                return AppAction::Continue;
            }
            if let Err(err) = doc.save(&path) {
                let message = format!("Can't save: {err} ({})", path.display());
                self.show_message(message, true);
                return AppAction::Continue;
            }
        }
        self.go(redo)
    }

    /// Goes ahead with `redo`, which the user confirmed.
    fn go(&mut self, redo: Redo) -> AppAction {
        self.confirmed = true;
        let action = match redo {
            Redo::Run(command) => self.run(command, false),
            Redo::CloseTab(id) => {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    self.close_tab(index);
                }
                AppAction::Continue
            }
            Redo::CloseItem(choice) => self.picker_action(PickerAction::CloseItem(choice)),
            Redo::Trash(path) => {
                self.trash(&path, true);
                AppAction::Continue
            }
        };
        self.confirmed = false;
        action
    }

    /// Closes `doc`, wherever it's shown, dropping its unsaved changes.
    fn destroy_document(&mut self, doc: &Rc<Document>) {
        for panel in tab::all_panels_mut(&mut self.tabs) {
            panel.forget(doc);
        }
        self.documents.retain(|open| !Rc::ptr_eq(open, doc));
        if self.is_preview(doc) {
            self.preview = None;
        }
        self.prune_documents();
        self.show_active_in_tree();
    }

    /// How `doc` is named to the user.
    fn document_name(&self, doc: &Document) -> String {
        match doc.path() {
            Some(path) => self.workspace.display_path(&path),
            None => doc.untitled_name().unwrap_or_default(),
        }
    }

    /// The untitled file numbered `number`, if it's open.
    fn find_untitled(&self, number: u32) -> Option<Rc<Document>> {
        self.documents
            .iter()
            .find(|doc| doc.path().is_none() && doc.untitled.get() == number)
            .cloned()
    }

    /// Shows `doc`, which is open, in the active panel, and gives it the
    /// keyboard.
    fn show_document(&mut self, doc: &Rc<Document>) {
        let had_terminal = self.active_terminal().is_some();
        if let Some(editor) = self.editor_mut() {
            editor.blur_find();
        }
        if let Err(err) = self.active_panel_mut().show(doc) {
            self.show_message(format!("Can't show the file: {err}"), true);
            return;
        }
        self.focus = Focus::Editor;
        self.prune_documents();
        if had_terminal {
            self.prune_terminals();
        }
        self.show_active_in_tree();
    }

    /// Opens a new, untitled file in the active panel, numbered after the
    /// untitled files open. One on screen that was never typed in is kept.
    fn new_untitled(&mut self) {
        if self
            .active_panel()
            .document()
            .is_some_and(|doc| doc.is_blank())
        {
            self.focus = Focus::Editor;
            return;
        }
        if let Some(doc) = self.open_untitled() {
            self.show_document(&doc);
        }
    }

    /// Opens a new, untitled file, numbered after the untitled files open,
    /// without showing it.
    fn open_untitled(&mut self) -> Option<Rc<Document>> {
        let doc = match Document::open(None, self.theme.clone()) {
            Ok((doc, _)) => doc,
            Err(err) => {
                self.show_message(format!("Can't open a new file: {err}"), true);
                return None;
            }
        };
        let taken: Vec<u32> = self
            .documents
            .iter()
            .filter(|doc| doc.path().is_none())
            .map(|doc| doc.untitled.get())
            .collect();
        let number = (1..).find(|n| !taken.contains(n)).unwrap_or(1);
        doc.untitled.set(number);
        self.documents.push(doc.clone());
        Some(doc)
    }

    // --- settings ---------------------------------------------------------------

    /// Says what was wrong with the settings file, if anything: the first
    /// problem, and how many more.
    pub fn warn_about_config(&mut self, warnings: &[String]) {
        let Some(first) = warnings.first() else {
            return;
        };
        let more = match warnings.len() - 1 {
            0 => String::new(),
            1 => " (and 1 more problem)".to_string(),
            n => format!(" (and {n} more problems)"),
        };
        let message = format!("Settings: {first}{more}. Use Open Settings to fix this.");
        self.show_message(message, true);
    }

    /// Reads the settings again if the settings file is open, and was saved
    /// or reloaded with something other than what was read last. Returns
    /// whether it was.
    fn follow_settings(&mut self) -> bool {
        let Some(path) = &self.settings else {
            return false;
        };
        let saved = self
            .documents
            .iter()
            .find(|doc| doc.path().as_ref() == Some(path))
            .and_then(|doc| doc.disk_hash());
        if saved.is_none() || saved == self.settings_hash {
            return false;
        }
        self.reload_settings();
        true
    }

    /// Reads the settings file and puts what it says in use, saying so, or
    /// what was wrong with it.
    fn reload_settings(&mut self) {
        let Some(path) = self.settings.clone() else {
            self.show_message("Can't locate settings: HOME isn't set.", true);
            return;
        };
        let (config, warnings) = match fs::read_to_string(&path) {
            Ok(text) => {
                self.settings_hash = Some(document::content_hash(text.as_bytes()));
                config::parse(&text)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.settings_hash = None;
                (Config::default(), Vec::new())
            }
            Err(err) => {
                self.show_message(format!("Can't read {}: {err}", path.display()), true);
                return;
            }
        };
        self.apply_settings(config);
        if warnings.is_empty() {
            self.show_message("Settings applied.", false);
        } else {
            self.warn_about_config(&warnings);
        }
    }

    /// Puts `config` in use, applying what changed to what's open. The
    /// tree's visibility and width, and wrapping, change only if their
    /// settings did, since they may have been changed by hand since.
    /// Terminals already running keep their shell and scrollback.
    fn apply_settings(&mut self, config: Config) {
        let old = config::get();
        config::set(config);
        let new = config::get();
        self.keymap = Keymap::new(&new.keys);
        if new.tree != old.tree {
            self.tree_visible = new.tree;
            if !new.tree && self.focus == Focus::Tree {
                self.focus = Focus::Editor;
            }
        }
        if new.tree_width != old.tree_width {
            self.tree_width = new.tree_width;
        }
        if new.tab_width != old.tab_width {
            for doc in &self.documents {
                doc.buffer.set_tab_width(new.tab_width as u8);
            }
        }
        for panel in tab::all_panels_mut(&mut self.tabs) {
            for editor in panel.editors_mut() {
                editor.follow_settings(&old, &new);
            }
        }
        if new.exclude != old.exclude {
            self.tree.refresh();
            self.files = FileIndex::new(&self.workspace);
            self.symbols = SymbolIndex::new(&self.workspace);
            self.show_active_in_tree();
        }
        self.layout();
    }

    /// Takes in the terminal's answer to one of [`theme::color_queries`].
    /// Returns false if `reply` isn't one.
    pub fn take_terminal_reply(&mut self, reply: &[u8]) -> bool {
        if let Some(light) = theme::appearance_change(reply) {
            // Its colors are asked for again, but while its background is
            // cue's, this is what says which it is.
            self.terminal_colors.switched_light = Some(light);
            self.theme_stale = true;
            return false;
        }
        let taken = self.terminal_colors.take_reply(reply);
        self.theme_stale |= taken;
        taken
    }

    /// Says whether the terminal's background is cue's (see
    /// [`theme::set_background`]), so what it says it is isn't taken in.
    pub fn set_terminal_background(&mut self, set: bool) {
        self.terminal_colors.background_set = set;
    }

    /// Puts the theme in use, if it isn't already: the one Select Theme
    /// has selected, or the setting's, for the terminal's colors. Returns
    /// whether the colors changed.
    pub fn update_theme(&mut self) -> bool {
        let previewed = self
            .picker
            .as_ref()
            .filter(|picker| picker.mode() == Mode::Themes)
            .and_then(|picker| match picker.selected_choice() {
                Some(&Choice::Theme(id)) => Some(id),
                _ => None,
            });
        let id = previewed.unwrap_or_else(|| config::get().theme.pick(&self.terminal_colors));
        if !self.theme_stale && self.theme_shown == Some(id) {
            return false;
        }
        self.theme_stale = false;
        self.theme_shown = Some(id);
        let colors = id.colors(&self.terminal_colors);
        if *theme::colors() == colors {
            return false;
        }
        theme::set(colors);
        self.theme.restyle();
        let text = theme::colors().text;
        for doc in &self.documents {
            doc.buffer.set_default_fg(Some(text));
        }
        for terminal in &self.terminals {
            terminal.borrow_mut().restyle();
        }
        for panel in tab::all_panels_mut(&mut self.tabs) {
            for editor in panel.editors_mut() {
                editor.restyle();
            }
        }
        true
    }

    /// Puts the theme `id` in use, and in the settings file, for next time.
    /// With a theme for a dark terminal and one for a light, it's the one
    /// for the terminal now.
    fn choose_theme(&mut self, id: ThemeId) {
        let name = id.name();
        let light = self.terminal_colors.light() == Some(true);
        let mut setting = config::get().theme;
        match (setting.dark == setting.light, light) {
            (true, _) => setting = ThemeSetting::one(id),
            (false, true) => setting.light = id,
            (false, false) => setting.dark = id,
        }
        let unsaved = self.settings.as_ref().is_some_and(|path| {
            self.documents
                .iter()
                .any(|doc| doc.path().as_ref() == Some(path) && doc.is_modified())
        });
        let path = match &self.settings {
            Some(path) if !unsaved => path.clone(),
            _ => {
                self.use_theme_setting(setting);
                let why = match unsaved {
                    true => "the settings file has unsaved changes",
                    false => "can't locate the settings file",
                };
                self.show_message(
                    format!("Theme applied for this session only: {why}."),
                    false,
                );
                return;
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => config::TEMPLATE.to_string(),
            Err(err) => {
                self.use_theme_setting(setting);
                self.show_message(format!("Can't read {}: {err}", path.display()), true);
                return;
            }
        };
        let text = config::with_theme(&text, id, light);
        let written = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&path, &text));
        if let Err(err) = written {
            self.use_theme_setting(setting);
            self.show_message(format!("Can't save {}: {err}", path.display()), true);
            return;
        }
        self.settings_hash = Some(document::content_hash(text.as_bytes()));
        let (config, warnings) = config::parse(&text);
        self.apply_settings(config);
        if warnings.is_empty() {
            self.show_message(format!("Using {name}."), false);
        } else {
            self.warn_about_config(&warnings);
        }
    }

    /// Puts `setting` in use until cue quits, keeping the other settings.
    fn use_theme_setting(&mut self, setting: ThemeSetting) {
        let mut config = (*config::get()).clone();
        config.theme = setting;
        self.apply_settings(config);
    }

    /// Opens the settings file, made from [`config::TEMPLATE`] if there
    /// isn't one yet.
    fn open_settings(&mut self) {
        let Some(path) = self.settings.clone() else {
            self.show_message("Can't locate settings: HOME isn't set.", true);
            return;
        };
        let made = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                io::Write::write_all(&mut file, config::TEMPLATE.as_bytes())?;
                // It sets nothing: the defaults in use are what it says.
                self.settings_hash = Some(document::content_hash(config::TEMPLATE.as_bytes()));
                Ok(())
            });
        match made {
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
                self.show_message(format!("Can't make {}: {err}", path.display()), true);
            }
            _ => {
                // Its folder may be new.
                let path = document::resolve(&path);
                self.settings = Some(path.clone());
                if self.open(&path, false) {
                    self.focus = Focus::Editor;
                }
            }
        }
    }

    // --- recovery ---------------------------------------------------------------

    /// Offers back the unsaved changes cues that are gone left in this
    /// workspace, if any (see [`crate::recovery`]). `asked`, it says so if
    /// there are none.
    fn offer_recovery(&mut self, asked: bool) {
        let orphans = self.recovery.orphans(self.workspace.roots());
        if orphans.is_empty() {
            if asked {
                self.show_message("No unsaved changes to recover.", false);
            }
            return;
        }
        let names: Vec<String> = orphans
            .iter()
            .map(|orphan| match &orphan.path {
                Some(path) => self.workspace.display_path(path),
                None => "an untitled file".to_string(),
            })
            .collect();
        let message = format!("cue exited with unsaved changes to {}.", names.join(", "));
        let buttons = vec![
            Button::new("&Recover", Answer::Recover(orphans.clone())),
            Button::new("&Discard", Answer::DiscardRecovered(orphans)).danger(),
        ];
        let title = "Recover unsaved changes?";
        self.alert = Some(Alert::new(title, message, buttons, self.width, self.height));
    }

    /// Opens the files `orphans` are of with their text, as unsaved changes,
    /// showing the first, and removes the copies.
    fn recover(&mut self, orphans: &[Orphan]) {
        let mut first = None;
        let mut count = 0;
        for orphan in orphans {
            let doc = match &orphan.path {
                Some(path) => match self.find_document(path) {
                    Some(doc) => doc,
                    None => match Document::open(Some(path.clone()), self.theme.clone()) {
                        Ok((doc, _)) => {
                            self.documents.push(doc.clone());
                            doc
                        }
                        Err(reason) => {
                            self.cant_open(&reason, path);
                            continue;
                        }
                    },
                },
                None => match self.open_untitled() {
                    Some(doc) => doc,
                    None => continue,
                },
            };
            doc.restore_text(&orphan.text);
            count += 1;
            first.get_or_insert(doc);
        }
        recovery::remove(orphans);
        if let Some(doc) = first {
            self.show_document(&doc);
            let files = match count {
                1 => "1 file".to_string(),
                count => format!("{count} files"),
            };
            self.show_message(
                format!("Recovered changes to {files}. Save to keep them."),
                false,
            );
        }
    }

    /// Lets unsaved changes go, as quitting does: their copies are removed.
    pub fn discard_recovery(&mut self) {
        self.recovery.discard();
    }

    /// The workspace folder being worked in, where terminals start: the one
    /// with the tree's selection, if the tree has the keyboard, or else the
    /// one with what's on screen, or the first.
    fn current_root(&self) -> PathBuf {
        let selected = match self.focus {
            Focus::Tree => self.tree.selected().map(|entry| entry.path),
            _ => None,
        };
        let path = selected
            .or_else(|| self.active_panel().path())
            .or_else(|| Some(self.active_terminal()?.borrow().cwd().to_path_buf()));
        let root = path
            .as_deref()
            .and_then(|path| self.workspace.root_of(path));
        match root.or_else(|| self.workspace.roots().first().map(PathBuf::as_path)) {
            Some(root) => root.to_path_buf(),
            None => std::env::current_dir().unwrap_or_default(),
        }
    }

    // --- sessions -----------------------------------------------------------------

    /// The session this cue is, if any.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Stops being a session, removing what it kept, as when it was one
    /// only to restart (see [`App::hand_over`]). Unsaved changes have
    /// recovery copies again.
    pub fn forget_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.end();
        }
        self.recovery = Recovery::new(match cfg!(test) {
            true => None,
            false => Recovery::default_dir(),
        });
    }

    /// Lets the session go without asking, as `cue --end` does.
    pub fn end_session_now(&mut self) {
        if let Some(session) = self.session.take() {
            session.end();
        }
    }

    /// The programs running in terminals, by name.
    pub fn running_programs(&self) -> Vec<String> {
        running_programs(&self.terminals)
    }

    /// Shows `text` in the status bar, as from outside, after input.
    pub fn show_message_now(&mut self, text: String, error: bool) {
        self.show_message(text, error);
    }

    /// Notes whether a terminal shows cue, for the list of sessions.
    pub fn set_attached(&mut self, attached: bool) {
        self.attached = attached;
        self.save_session(false);
    }

    /// Makes this cue a session, saved from now on, which keeps its unsaved
    /// changes instead of recovery copies. Returns false, with a message,
    /// if it can't. With `announce`, says so.
    fn keep_session(&mut self, announce: bool) -> bool {
        let Some(sessions) = self.sessions.clone() else {
            self.show_message("Can't save sessions: HOME isn't set.", true);
            return false;
        };
        match Session::create(&sessions) {
            Ok(session) => {
                self.session = Some(session);
                if !self.save_session(false) {
                    self.session.take().unwrap().end();
                    return false;
                }
                self.recovery.discard();
                if announce {
                    self.show_message("Session saved. Run `cue` to resume after quitting.", false);
                }
                true
            }
            Err(err) => {
                self.show_message(format!("Can't create a session: {err}"), true);
                false
            }
        }
    }

    /// Leaves the session to come back to: in the background with its
    /// terminals, while programs run in them, or else saved, to exit.
    fn leave_session(&mut self) -> AppAction {
        if !self.save_session(true) {
            return AppAction::Continue;
        }
        match running_programs(&self.terminals).is_empty() {
            true => AppAction::Quit,
            false => AppAction::Detach,
        }
    }

    /// Lets the session go and quits, asking first, as quitting does
    /// otherwise, about unsaved changes and running programs.
    fn end_session(&mut self) -> AppAction {
        if self.session.is_none() {
            self.show_message("No active session. Use Keep Session to create one.", false);
            return AppAction::Continue;
        }
        let unsaved = self.unsaved();
        let running = running_programs(&self.terminals);
        let redo = Redo::Run(Command::EndSession);
        if !self.ask_first("End session?".into(), unsaved, &running, "&End", redo) {
            return AppAction::Continue;
        }
        if let Some(session) = self.session.take() {
            session.end();
        }
        AppAction::Quit
    }

    /// The terminal cue showed in is gone, as when an ssh connection
    /// drops. With unsaved changes or programs running, cue becomes a
    /// session, if it isn't one, as if asked when quitting. Returns whether
    /// to go on in the background: while programs run.
    pub fn hang_up(&mut self) -> bool {
        let running = !running_programs(&self.terminals).is_empty();
        if self.session.is_none() && (running || !self.unsaved().is_empty()) {
            self.keep_session(false);
        }
        if self.session.is_none() {
            return false;
        }
        self.attached = false;
        let saved = self.save_session(true);
        running || !saved
    }

    /// Saves the session, if this is one, as it is now; with `screens`,
    /// terminals' screens too, where they changed.
    /// Returns false, with a message, if unsaved text or metadata could
    /// not be written. Callers leaving the app must keep it open then.
    pub fn save_session(&mut self, screens: bool) -> bool {
        if self.session.is_none() {
            return true;
        }
        let result = self.session_state(screens).and_then(|state| {
            let session = self.session.as_mut().unwrap();
            session.save(&state)?;
            // Only the committed metadata decides which copies may go.
            let texts = state
                .documents
                .iter()
                .filter_map(|doc| doc.unsaved.clone())
                .collect::<Vec<_>>();
            session.prune_texts(&texts);
            Ok(())
        });
        self.session_saved = Some(Instant::now());
        if let Err(err) = result {
            self.show_message(format!("Can't save session: {err}"), true);
            return false;
        }
        if screens {
            self.screens_saved = self.session_saved;
        }
        true
    }

    /// Everything the session keeps, writing unsaved changes, and with
    /// `screens`, terminals' screens, where they changed.
    fn session_state(&mut self, screens: bool) -> io::Result<session::State> {
        let Some(session) = &mut self.session else {
            return Ok(session::State::default());
        };
        let documents = self
            .documents
            .iter()
            .filter(|doc| !doc.is_blank())
            .map(|doc| {
                let unsaved = session.save_text(doc)?;
                Ok(session::Doc {
                    path: doc.path(),
                    untitled: doc.untitled.get(),
                    unsaved,
                    preview: self.preview.as_ref().is_some_and(|p| Rc::ptr_eq(p, doc)),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        let mut terminals = Vec::new();
        for terminal in &self.terminals {
            let mut terminal = terminal.borrow_mut();
            let id = terminal.id();
            let epoch = terminal.epoch();
            let saved = self.screens.get(&id).map(|(_, at)| *at);
            if screens && saved != Some(epoch) {
                let name = terminal
                    .screen(false)
                    .and_then(|(screen, _)| session.save_screen(id, &screen).ok());
                if let Some(name) = name {
                    self.screens.insert(id, (name, epoch));
                }
            }
            let (cols, rows) = terminal.screen_size();
            terminals.push(session::TerminalState {
                id,
                name: terminal.given_name().map(str::to_string),
                cwd: terminal.current_folder(),
                program: terminal.is_busy().then(|| terminal.program()).flatten(),
                screen: self.screens.get(&id).map(|(name, _)| name.clone()),
                size: (cols, rows),
            });
        }
        let ids: Vec<u32> = terminals.iter().map(|terminal| terminal.id).collect();
        self.screens.retain(|id, _| ids.contains(id));
        if screens {
            let kept: Vec<String> = self
                .screens
                .values()
                .map(|(name, _)| name.clone())
                .collect();
            session.prune_screens(&kept);
        }

        let tabs = self
            .tabs
            .iter()
            .map(|tab| session::TabState {
                name: tab.name.clone(),
                layout: tab.layout.clone(),
                active: tab.active,
                panels: tab
                    .panels
                    .iter()
                    .map(|panel| {
                        let (back, forward) = panel.history();
                        session::PanelState {
                            id: panel.id,
                            shows: panel.visit().as_ref().and_then(visit_shown),
                            places: panel
                                .places()
                                .filter_map(|(doc, (row, col, top), reading)| {
                                    Some(session::Place {
                                        doc: doc_shown(doc)?,
                                        row,
                                        col,
                                        top,
                                        reading,
                                    })
                                })
                                .collect(),
                            back: back.iter().filter_map(visit_shown).collect(),
                            forward: forward.iter().filter_map(visit_shown).collect(),
                        }
                    })
                    .collect(),
            })
            .collect();
        let recent = self
            .recent
            .iter()
            .map(|recent| match recent {
                Recent::File(path) => Shown::File(path.clone()),
                Recent::Terminal(id) => Shown::Terminal(*id),
                Recent::Untitled(number) => Shown::Untitled(*number),
            })
            .collect();
        Ok(session::State {
            roots: self.workspace.roots().to_vec(),
            saved: session::now(),
            tree_visible: self.tree_visible,
            tree_width: self.tree_width,
            tree_focused: self.focus == Focus::Tree,
            tab: self.tab,
            tabs,
            documents,
            terminals,
            recent,
            attached: self.attached,
        })
    }

    /// Goes back to `session`, as `state` has it: its files, with their
    /// unsaved changes, its terminals, and its tabs. Terminals `adopted`
    /// from a process this one replaced go on running; the others start
    /// new shells, below what they showed.
    pub fn restore_session(
        &mut self,
        mut session: Session,
        state: session::State,
        adopted: Vec<Adopted>,
    ) -> Result<(), String> {
        self.close_popups();
        self.tree_visible = state.tree_visible;
        if state.tree_width > 0 {
            self.tree_width = state.tree_width.max(MIN_TREE_WIDTH);
        }

        // Files.
        for panel in tab::all_panels_mut(&mut self.tabs) {
            panel.clear();
        }
        self.documents.clear();
        self.preview = None;
        let mut docs: Vec<(Shown, Rc<Document>)> = Vec::new();
        for saved in &state.documents {
            let text = saved
                .unsaved
                .as_ref()
                .map(|name| {
                    session
                        .text(name)
                        .map_err(|err| format!("Can't read saved changes {name}: {err}"))
                })
                .transpose()?;
            let opened = match &saved.path {
                // A file that's gone comes back only with changes to it.
                Some(path) if !path.exists() && text.is_none() => None,
                Some(path) => match Document::open(Some(path.clone()), self.theme.clone()) {
                    Ok(doc) => Some(doc),
                    Err(err) if text.is_some() => {
                        self.show_message(
                            format!(
                                "Can't open {}: {err}. Saved changes restored.",
                                path.display()
                            ),
                            true,
                        );
                        Some((
                            Document::from_unsaved(
                                path.clone(),
                                text.as_deref().unwrap(),
                                self.theme.clone(),
                            )?,
                            None,
                        ))
                    }
                    Err(_) => None,
                },
                None => Some(Document::open(None, self.theme.clone())?),
            };
            let Some((doc, _)) = opened else {
                continue;
            };
            if saved.path.is_none() {
                doc.untitled.set(saved.untitled);
            }
            if let (Some(text), Some(name)) = (&text, &saved.unsaved) {
                doc.restore_text(text);
                session.note_text(&doc, name);
            }
            if saved.preview {
                self.preview = Some(doc.clone());
            }
            let key = match &saved.path {
                Some(path) => Shown::File(path.clone()),
                None => Shown::Untitled(saved.untitled),
            };
            self.documents.push(doc.clone());
            docs.push((key, doc));
        }
        let find_doc = |shown: &Shown| {
            docs.iter()
                .find(|(key, _)| key == shown)
                .map(|(_, doc)| doc.clone())
        };

        // Terminals.
        self.terminals.clear();
        self.focused_terminal = None;
        self.screens.clear();
        let mut adopted = adopted;
        for saved in &state.terminals {
            let cwd = match saved.cwd.is_dir() {
                true => saved.cwd.clone(),
                false => self.current_root(),
            };
            let (cols, rows) = saved.size;
            let area = Rect {
                width: cols as u32,
                height: rows as u32,
                ..Rect::default()
            };
            let terminal = match adopted.iter().position(|a| a.id == saved.id) {
                Some(index) => {
                    let a = adopted.remove(index);
                    Terminal::adopt(saved.id, &cwd, area, (a.fd, a.pid), &a.screen, a.size)
                }
                None => {
                    let screen = saved.screen.as_ref().and_then(|name| session.screen(name));
                    Terminal::restore(saved.id, &cwd, area, screen.as_deref(), cols, state.saved)
                }
            };
            match terminal {
                Ok(mut terminal) => {
                    terminal.set_name(saved.name.clone());
                    self.terminals.push(Rc::new(RefCell::new(terminal)));
                }
                Err(err) => self.show_message(format!("Can't start a shell: {err}"), true),
            }
        }
        // Any left over aren't the session's: they're hung up on.
        for left in adopted {
            if let Ok(pty) = crate::pty::Pty::adopt(left.fd, left.pid) {
                drop(pty);
            }
        }
        self.next_terminal = state
            .terminals
            .iter()
            .map(|terminal| terminal.id + 1)
            .max()
            .unwrap_or(1)
            .max(self.next_terminal);

        // Tabs.
        if !state.tabs.is_empty() {
            self.tabs = state
                .tabs
                .iter()
                .enumerate()
                .map(|(index, saved)| Tab {
                    id: index as TabId,
                    layout: saved.layout.clone(),
                    panels: saved
                        .panels
                        .iter()
                        .map(|panel| Panel::new(panel.id))
                        .collect(),
                    active: saved.active,
                    previous: None,
                    name: saved.name.clone(),
                })
                .collect();
        }
        self.tab = state.tab.min(self.tabs.len() - 1);
        self.next_tab = self.tabs.len() as TabId;
        self.next_panel = tab::all_panels(&self.tabs)
            .map(|panel| panel.id + 1)
            .max()
            .unwrap_or(0);
        let area = self.main_area();
        for tab in &mut self.tabs {
            tab.set_area(area);
        }
        let visit = |shown: &Shown| match shown {
            Shown::File(path) => Some(Visit::File(
                find_doc(shown).map_or_else(Weak::new, |doc| Rc::downgrade(&doc)),
                Some(path.clone()),
            )),
            Shown::Untitled(_) => Some(Visit::File(Rc::downgrade(&find_doc(shown)?), None)),
            Shown::Terminal(id) => Some(Visit::Terminal(*id)),
            Shown::Image(path) => Some(Visit::Image(path.clone())),
        };
        for (tab, saved) in self.tabs.iter_mut().zip(&state.tabs) {
            for saved in &saved.panels {
                let Some(panel) = tab.panel_mut(saved.id) else {
                    continue;
                };
                for place in &saved.places {
                    let Some(doc) = find_doc(&place.doc) else {
                        continue;
                    };
                    if panel.show(&doc).is_ok() {
                        if let Some(editor) = panel.editor_mut() {
                            editor.set_place(place.row, place.col, place.top);
                            if let Some(line) = place.reading {
                                editor.read_from(line);
                            }
                        }
                    }
                }
                let shown = match &saved.shows {
                    Some(Shown::Terminal(id)) => self
                        .terminals
                        .iter()
                        .find(|terminal| terminal.borrow().id() == *id)
                        .map(|terminal| panel.show_terminal(terminal.clone()))
                        .is_some(),
                    Some(Shown::Image(path)) => ImageView::open(path)
                        .map(|image| panel.show_image(image))
                        .is_ok(),
                    Some(doc) => find_doc(doc).is_some_and(|doc| panel.show(&doc).is_ok()),
                    None => false,
                };
                if !shown {
                    panel.show_nothing();
                }
                panel.set_history(
                    saved.back.iter().filter_map(visit).collect(),
                    saved.forward.iter().filter_map(visit).collect(),
                );
            }
        }

        self.recent = state
            .recent
            .iter()
            .map(|shown| match shown {
                Shown::File(path) | Shown::Image(path) => Recent::File(path.clone()),
                Shown::Terminal(id) => Recent::Terminal(*id),
                Shown::Untitled(number) => Recent::Untitled(*number),
            })
            .collect();
        self.prune_terminals();
        self.focus = match state.tree_focused && self.tree_visible {
            true => Focus::Tree,
            false => Focus::Editor,
        };
        self.session = Some(session);
        self.recovery.discard();
        self.layout();
        self.show_active_in_tree();
        self.watching = None;
        self.watch_folders();
        Ok(())
    }

    /// Lets go of everything, for a process that replaces this one to go
    /// on with (see [`App::restore_session`]): the session saved, made one
    /// for the while if this isn't one (`true` then), and the terminals,
    /// their shells still running, each with its screen in full. `None`,
    /// with a message, if there's nowhere to keep it.
    pub fn hand_over(&mut self) -> Option<(PathBuf, bool, Vec<Adopted>)> {
        let ephemeral = self.session.is_none();
        if ephemeral && !self.keep_session(false) {
            return None;
        }
        if !self.save_session(false) {
            return None;
        }
        let dir = self.session.take()?.dir().to_path_buf();
        // Nothing else holds the terminals then.
        self.close_popups();
        self.focused_terminal = None;
        for panel in tab::all_panels_mut(&mut self.tabs) {
            panel.hide_terminal();
        }
        let mut adopted = Vec::new();
        for terminal in std::mem::take(&mut self.terminals) {
            let Ok(terminal) = Rc::try_unwrap(terminal) else {
                continue;
            };
            let mut terminal = terminal.into_inner();
            let id = terminal.id();
            let Some((screen, size)) = terminal.screen(true) else {
                continue;
            };
            let (fd, pid) = terminal.release();
            adopted.push(Adopted {
                id,
                fd,
                pid,
                screen,
                size,
            });
        }
        Some((dir, ephemeral, adopted))
    }

    // --- file dialog ------------------------------------------------------------

    /// Opens the file dialog for `purpose`, or closes it if it's open for
    /// that. Saving, it suggests the file's name.
    fn show_dialog(&mut self, purpose: Purpose) {
        self.close_search();
        self.picker = None;
        if self
            .dialog
            .take()
            .is_some_and(|dialog| dialog.purpose() == purpose)
        {
            return;
        }
        let current = match purpose {
            Purpose::SaveAs => match self.active_panel().document() {
                Some(doc) => doc.path(),
                None => {
                    self.show_message("No file to save.", false);
                    return;
                }
            },
            _ => None,
        };
        let name = current
            .as_deref()
            .and_then(Path::file_name)
            .map_or(String::new(), |name| name.to_string_lossy().into_owned());
        let folder = match purpose {
            // Among the folders beside the one being worked in.
            Purpose::AddFolder => {
                let root = self.current_root();
                root.parent().map_or(root.clone(), Path::to_path_buf)
            }
            _ => self.dialog_folder(),
        };
        self.open_dialog(purpose, &folder, &name, current);
    }

    /// Opens the file dialog for `purpose` in `dir`, with `name` suggested,
    /// for `current` (see [`FileDialog::new`]).
    fn open_dialog(&mut self, purpose: Purpose, dir: &Path, name: &str, current: Option<PathBuf>) {
        self.close_popups();
        self.dialog = Some(FileDialog::new(
            purpose,
            dir,
            name,
            current,
            &self.keymap,
            self.width,
            self.height,
        ));
    }

    /// Where the file dialog starts: in the folder selected in the tree, if
    /// the tree has the keyboard, or else the active file's folder, or the
    /// workspace's.
    fn dialog_folder(&self) -> PathBuf {
        if self.focus == Focus::Tree {
            if let Some(folder) = self.tree.selected_folder() {
                return folder;
            }
        }
        let file = self.active_panel().path();
        match file.as_deref().and_then(Path::parent) {
            Some(folder) if folder.is_dir() => folder.to_path_buf(),
            _ => self.current_root(),
        }
    }

    fn dialog_action(&mut self, action: DialogAction) -> AppAction {
        match action {
            DialogAction::Continue => {}
            DialogAction::Close => self.dialog = None,
            DialogAction::Accept(path) => {
                let Some(mut dialog) = self.dialog.take() else {
                    return AppAction::Continue;
                };
                let result = match dialog.purpose() {
                    Purpose::Open => {
                        if self.open(&path, false) {
                            self.focus = Focus::Editor;
                        }
                        Ok(AppAction::Continue)
                    }
                    Purpose::SaveAs => self.save_as(&path),
                    Purpose::Create => self.create_file(&path),
                    Purpose::CreateFolder => self.create_folder(&path),
                    Purpose::AddFolder => self.add_folder(&path),
                    Purpose::Move | Purpose::Duplicate => match dialog.current() {
                        Some(from) => {
                            let from = from.to_path_buf();
                            match dialog.purpose() {
                                Purpose::Move => self.move_entry(&from, &path),
                                _ => self.duplicate(&from, &path),
                            }
                        }
                        None => Ok(AppAction::Continue),
                    },
                };
                match result {
                    Ok(action) => return action,
                    // The dialog stays, to choose another path.
                    Err(error) => {
                        dialog.show_error(error);
                        self.dialog = Some(dialog);
                    }
                }
            }
        }
        AppAction::Continue
    }

    /// Saves the active file to `path` from now on, creating its folder if
    /// need be, unless another file open is that one.
    fn save_as(&mut self, path: &Path) -> Result<AppAction, String> {
        let current = self.active_panel().document();
        let open = self.find_document(&document::resolve(path));
        if open.is_some_and(|open| !current.is_some_and(|current| Rc::ptr_eq(current, &open))) {
            return Err(format!("{} is open. Close it first.", file_name(path)));
        }
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)
                .map_err(|err| format!("Can't create the destination folder: {err}"))?;
        }
        let action = match self.editor_mut() {
            Some(editor) => editor.save_as(document::resolve(path)),
            None => Action::Continue,
        };
        self.focus = Focus::Editor;
        Ok(self.editor_action(action))
    }

    /// Creates an empty file at `path`, and its folder if need be, and
    /// opens it.
    fn create_file(&mut self, path: &Path) -> Result<AppAction, String> {
        let created = (|| {
            if let Some(folder) = path.parent() {
                fs::create_dir_all(folder)?;
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(drop)
        })();
        created.map_err(|err| format!("Can't create {}: {err}", file_name(path)))?;
        self.tree.refresh();
        self.files.refresh();
        let path = document::resolve(path);
        if self.open(&path, false) {
            self.tree.reveal(&path);
            self.focus = Focus::Editor;
        }
        Ok(AppAction::Continue)
    }

    /// Quits, but for a session, which it leaves to come back to (see
    /// [`App::leave_session`]). Unsaved changes or running programs ask
    /// first, offering to keep them as a session.
    fn quit(&mut self) -> AppAction {
        if self.session.is_some() {
            return self.leave_session();
        }
        let unsaved = self.unsaved();
        let running = running_programs(&self.terminals);
        let redo = Redo::Run(Command::Quit);
        if self.ask_first("Quit cue?".into(), unsaved, &running, "&Quit", redo) {
            return AppAction::Quit;
        }
        if let (Some(alert), Some(_)) = (&mut self.alert, &self.sessions) {
            alert.add_first(Button::new("&Keep Session", Answer::KeepSession));
        }
        AppAction::Continue
    }

    /// The open files with unsaved changes.
    fn unsaved(&self) -> Vec<Rc<Document>> {
        self.documents
            .iter()
            .filter(|doc| doc.is_modified())
            .cloned()
            .collect()
    }

    fn tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::None => {}
            TreeAction::Open {
                path,
                focus,
                preview,
            } => {
                if self.open(&path, preview) && focus {
                    self.focus = Focus::Editor;
                }
            }
        }
    }

    fn editor_action(&mut self, action: Action) -> AppAction {
        match action {
            Action::Continue => AppAction::Continue,
            Action::Copy(text) => AppAction::Copy(text),
            Action::Saved => {
                // The file may be new, or saved under a new name.
                self.tree.refresh();
                self.files.refresh();
                self.show_active_in_tree();
                AppAction::Continue
            }
            // It has no name yet.
            Action::SaveAs => {
                self.show_dialog(Purpose::SaveAs);
                AppAction::Continue
            }
            Action::Conflict => {
                if let Some(doc) = self.active_panel().document().cloned() {
                    self.ask_overwrite(doc, None);
                }
                AppAction::Continue
            }
            Action::ReadOnly => {
                let how = match self.keymap.shortcut(Command::ToggleReader) {
                    Some(key) => format!("double-click or press {key}"),
                    None => "double-click, or click Edit above,".to_string(),
                };
                self.show_message(format!("This is reader mode: {how} to edit."), false);
                AppAction::Continue
            }
        }
    }

    // --- file commands ----------------------------------------------------------

    /// What the file commands act on: the entry selected in the tree, if it
    /// has the keyboard, or else the file on screen, or the workspace's
    /// folder.
    fn file_target(&self) -> Entry {
        if self.focus == Focus::Tree {
            if let Some(entry) = self.tree.selected() {
                return entry;
            }
        }
        if let Some(path) = self.active_panel().path() {
            return Entry {
                path,
                is_dir: false,
                is_root: false,
            };
        }
        Entry {
            path: self.current_root(),
            is_dir: true,
            is_root: true,
        }
    }

    /// Shift+F10, or from the palette: the context menu of the tree's
    /// selection, next to it. From elsewhere, the tree shows the file on
    /// screen first.
    fn show_tree_menu(&mut self) {
        if self.focus != Focus::Tree {
            let file = self.active_panel().path();
            self.run(Command::FocusTree, false);
            if let Some(file) = file {
                self.tree.reveal(&file);
            }
        }
        if self.focus != Focus::Tree {
            return;
        }
        if let Some(target) = self.tree.selected() {
            self.open_menu(target, None);
        }
    }

    /// Opens the context menu of the file or folder `target`, at the cell
    /// right-clicked, or from the keyboard, next to the tree's selection.
    fn open_menu(&mut self, target: Entry, at: Option<(u32, u32)>) {
        use Command::*;
        let item = |command, label: &str| MenuItem::Command(command, label.to_string());
        let mut items = Vec::new();
        if !target.is_dir {
            items.extend([
                item(TreeOpen, "Open"),
                item(TreeOpenToSide, "Open to the Side"),
                MenuItem::Separator,
            ]);
        }
        items.extend([
            item(TreeNewFile, "New File…"),
            item(TreeNewFolder, "New Folder…"),
        ]);
        if target.is_dir {
            items.push(item(TreeOpenInTerminal, "Open in Terminal"));
        }
        if target.is_root {
            items.extend([
                MenuItem::Separator,
                item(AddFolder, "Add Folder to Workspace…"),
            ]);
            if self.workspace.roots().len() > 1 {
                items.push(item(TreeRemoveFolder, "Remove Folder from Workspace"));
            }
        } else {
            items.extend([
                MenuItem::Separator,
                item(TreeRename, "Rename…"),
                item(TreeDuplicate, "Duplicate…"),
                item(TreeTrash, "Move to Trash"),
            ]);
        }
        let reveal = match cfg!(target_os = "macos") {
            true => "Reveal in Finder",
            false => "Open Containing Folder",
        };
        items.extend([
            MenuItem::Separator,
            item(TreeCopyPath, "Copy Path"),
            item(TreeCopyRelativePath, "Copy Relative Path"),
            item(TreeReveal, reveal),
        ]);
        let (x, y) = at
            .or_else(|| self.tree.selected_position())
            .unwrap_or((0, 0));
        let menu = ContextMenu::new(
            items,
            &self.keymap,
            x,
            y,
            at.is_some(),
            self.width,
            self.height,
        );
        self.close_popups();
        self.menu = Some((menu, MenuFor::File(target)));
    }

    /// The session badge's menu, above it: to detach or end the session.
    fn show_session_menu(&mut self) {
        let status = self.active_panel().status();
        let Some(badge) = status::session_badge(&status, self.width) else {
            return;
        };
        let items = vec![
            MenuItem::Command(Command::Detach, "Detach Session".into()),
            MenuItem::Command(Command::EndSession, "End Session".into()),
        ];
        let y = self.height.saturating_sub(1);
        let menu = ContextMenu::new(
            items,
            &self.keymap,
            badge.start,
            y,
            true,
            self.width,
            self.height,
        );
        self.close_popups();
        self.menu = Some((menu, MenuFor::Session));
    }

    fn menu_action(&mut self, action: MenuAction) -> AppAction {
        match action {
            MenuAction::Continue => AppAction::Continue,
            MenuAction::Close | MenuAction::CloseAndPass => {
                self.menu = None;
                AppAction::Continue
            }
            MenuAction::Accept(command) => match self.menu.take() {
                Some((_, MenuFor::File(target))) => self.file_command(command, target, true),
                Some((_, MenuFor::Session)) => self.run(command, false),
                None => AppAction::Continue,
            },
        }
    }

    /// Runs the file command `command` on `target`. Trashing asks first,
    /// unless it was chosen from a context menu.
    fn file_command(&mut self, command: Command, target: Entry, from_menu: bool) -> AppAction {
        let parent = match target.path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => self.current_root(),
        };
        let folder = match target.is_dir {
            true => target.path.clone(),
            false => parent.clone(),
        };
        let name = file_name(&target.path);
        match command {
            Command::TreeOpen if !target.is_dir => {
                if self.open(&target.path, false) {
                    self.focus = Focus::Editor;
                }
            }
            Command::TreeOpenToSide if !target.is_dir => {
                let panels = self.tab().panels.len();
                self.split(Axis::Horizontal);
                if self.tab().panels.len() > panels && self.open(&target.path, false) {
                    self.focus = Focus::Editor;
                }
            }
            Command::TreeNewFile => self.open_dialog(Purpose::Create, &folder, "", None),
            Command::TreeNewFolder => self.open_dialog(Purpose::CreateFolder, &folder, "", None),
            Command::TreeOpenInTerminal => self.new_terminal_in(&folder),
            Command::TreeRename | Command::TreeDuplicate | Command::TreeTrash if target.is_root => {
                self.show_message(format!("{name} is a workspace folder."), false);
            }
            Command::TreeRename | Command::TreeTrash => {
                if let Some(root) = self.roots_in(&target.path).first() {
                    let root = self.workspace.name(root).unwrap_or_default();
                    let message = format!("{name} contains workspace folder {root}.");
                    self.show_message(message, false);
                    return AppAction::Continue;
                }
                match command {
                    Command::TreeRename => {
                        self.open_dialog(Purpose::Move, &parent, &name, Some(target.path));
                    }
                    _ => self.trash(&target.path, from_menu),
                }
            }
            Command::TreeDuplicate => {
                let copy = copy_name(&target.path);
                self.open_dialog(Purpose::Duplicate, &parent, &copy, Some(target.path));
            }
            Command::AddFolder => self.show_dialog(Purpose::AddFolder),
            Command::TreeRemoveFolder => self.remove_folder(&target),
            Command::TreeCopyPath => {
                return self.copy_path(target.path.display().to_string());
            }
            Command::TreeCopyRelativePath => {
                let relative = match self.workspace.root_of(&target.path) {
                    Some(root) => target.path.strip_prefix(root).unwrap_or(&target.path),
                    None => &target.path,
                };
                let relative = match relative.as_os_str().is_empty() {
                    true => ".".to_string(),
                    false => relative.display().to_string(),
                };
                return self.copy_path(relative);
            }
            Command::TreeReveal => self.reveal(&target.path),
            _ => {}
        }
        AppAction::Continue
    }

    /// Puts the path `text` on the clipboard.
    fn copy_path(&mut self, text: String) -> AppAction {
        self.show_message(format!("Copied {text}"), false);
        self.clipboard = Some(text.clone());
        AppAction::Copy(text)
    }

    /// Adds the folder `path` to the workspace, and selects it in the tree.
    fn add_folder(&mut self, path: &Path) -> Result<AppAction, String> {
        let root = path
            .canonicalize()
            .map_err(|err| format!("Can't add {}: {err}", file_name(path)))?;
        if self.workspace.roots().contains(&root) {
            return Err(format!("{} is already in the workspace.", file_name(path)));
        }
        self.workspace
            .add_root(&root)
            .map_err(|err| format!("Can't add {}: {err}", file_name(path)))?;
        self.roots_changed();
        self.tree.reveal(&root);
        self.focus = Focus::Tree;
        Ok(AppAction::Continue)
    }

    /// Takes `target`, a workspace folder, out of the workspace. Its files
    /// stay open.
    fn remove_folder(&mut self, target: &Entry) {
        let name = file_name(&target.path);
        if !self.workspace.roots().contains(&target.path) {
            self.show_message(format!("{name} isn't a workspace folder."), false);
        } else if self.workspace.roots().len() == 1 {
            self.show_message(format!("{name} is the only workspace folder."), false);
        } else {
            self.workspace.remove_root(&target.path);
            self.roots_changed();
            self.show_message(format!("Removed {name} from the workspace."), false);
        }
    }

    /// Shows the workspace's folders, after one came or went.
    fn roots_changed(&mut self) {
        self.tree.set_roots(self.workspace.roots());
        self.files = FileIndex::new(&self.workspace);
        self.symbols = SymbolIndex::new(&self.workspace);
        self.watching = None;
        self.watch_folders();
        self.show_active_in_tree();
    }

    /// The workspace folders in `path`, or that are it.
    fn roots_in(&self, path: &Path) -> Vec<PathBuf> {
        let path = document::resolve(path);
        let roots = self.workspace.roots().iter();
        roots
            .filter(|root| root.starts_with(&path))
            .cloned()
            .collect()
    }

    /// Creates the folder `path`, and those it's in if need be.
    fn create_folder(&mut self, path: &Path) -> Result<AppAction, String> {
        fs::create_dir_all(path)
            .map_err(|err| format!("Can't create {}: {err}", file_name(path)))?;
        self.tree.refresh();
        self.tree.reveal(path);
        Ok(AppAction::Continue)
    }

    /// Renames or moves the file or folder `from` to `to`, creating the
    /// folder it goes in if need be. Open files go with it.
    fn move_entry(&mut self, from: &Path, to: &Path) -> Result<AppAction, String> {
        if to.starts_with(from) && to != from {
            return Err("Can't move a folder into itself.".to_string());
        }
        if let Some(folder) = to.parent() {
            fs::create_dir_all(folder)
                .map_err(|err| format!("Can't create the destination folder: {err}"))?;
        }
        fs::rename(from, to).map_err(|err| format!("Can't move {}: {err}", file_name(from)))?;
        // Open files have their paths resolved.
        let (resolved_from, resolved_to) = (document::resolve(from), document::resolve(to));
        let moved = |path: &Path| {
            let rest = path.strip_prefix(&resolved_from).ok()?;
            Some(match rest.as_os_str().is_empty() {
                true => resolved_to.clone(),
                false => resolved_to.join(rest),
            })
        };
        for doc in &self.documents {
            if let Some(path) = doc.path().and_then(|path| moved(&path)) {
                doc.rename(path);
            }
        }
        for panel in tab::all_panels_mut(&mut self.tabs) {
            if let Some(image) = panel.image_mut() {
                if let Some(path) = moved(image.path()) {
                    image.rename(path);
                }
            }
        }
        for recent in &mut self.recent {
            if let Recent::File(path) = recent {
                if let Some(moved) = moved(path) {
                    *path = moved;
                }
            }
        }
        self.tree.moved(from, to);
        self.files.refresh();
        self.show_active_in_tree();
        let message = match from.parent() == to.parent() {
            true => format!("Renamed {} to {}.", file_name(from), file_name(to)),
            false => format!(
                "Moved {} to {}.",
                file_name(from),
                self.workspace.display_path(to)
            ),
        };
        self.show_message(message, false);
        Ok(AppAction::Continue)
    }

    /// Copies the file or folder `from` to `to`, creating the folder it goes
    /// in if need be.
    fn duplicate(&mut self, from: &Path, to: &Path) -> Result<AppAction, String> {
        if to.starts_with(from) {
            return Err("Can't copy a folder into itself.".to_string());
        }
        if let Some(folder) = to.parent() {
            fs::create_dir_all(folder)
                .map_err(|err| format!("Can't create the destination folder: {err}"))?;
        }
        copy_all(from, to).map_err(|err| format!("Can't copy {}: {err}", file_name(from)))?;
        self.tree.refresh();
        self.tree.reveal(to);
        self.files.refresh();
        Ok(AppAction::Continue)
    }

    /// Moves the file or folder at `path` to the Trash, after asking, unless
    /// `confirmed`. Open files that were in it close, unless they have
    /// unsaved changes, which saving puts back.
    fn trash(&mut self, path: &Path, confirmed: bool) {
        let name = file_name(path);
        if !confirmed {
            let message = match path.is_dir() {
                true => "You can restore this folder and its contents from the Trash.",
                false => "You can restore this file from the Trash.",
            };
            let redo = Redo::Trash(path.to_path_buf());
            let buttons = vec![Button::new("Move to &Trash", Answer::Go(redo)).danger()];
            let title = format!("Move {name} to the Trash?");
            self.alert = Some(Alert::new(title, message, buttons, self.width, self.height));
            return;
        }
        let resolved = document::resolve(path);
        if let Err(err) = move_to_trash(path) {
            self.show_message(format!("Can't move {name} to the Trash: {err}"), true);
            return;
        }
        let closing: Vec<Rc<Document>> = self
            .documents
            .iter()
            .filter(|doc| !doc.is_modified())
            .filter(|doc| doc.path().is_some_and(|open| open.starts_with(&resolved)))
            .cloned()
            .collect();
        for doc in closing {
            self.destroy_document(&doc);
        }
        self.close_images(&resolved);
        self.recent
            .retain(|recent| !matches!(recent, Recent::File(open) if open.starts_with(&resolved)));
        self.tree.refresh();
        self.files.refresh();
        self.show_message(format!("Moved {name} to the Trash."), false);
    }

    /// Shows `path` in the Finder, or elsewhere, opens its folder.
    fn reveal(&mut self, path: &Path) {
        let mut command = match cfg!(target_os = "macos") {
            true => {
                let mut command = std::process::Command::new("open");
                command.arg("-R").arg(path);
                command
            }
            false => {
                let mut command = std::process::Command::new("xdg-open");
                command.arg(path.parent().unwrap_or(path));
                command
            }
        };
        let spawned = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            // Waited for, so it doesn't linger as a zombie.
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(err) => self.show_message(format!("Can't show {}: {err}", file_name(path)), true),
        }
    }

    /// Follows a link clicked in reader mode: a web page opens in the
    /// browser, and a file in the panel, a Markdown one in reader mode at
    /// the heading the link names, if any.
    fn follow_link(&mut self, link: &str) {
        let scheme = link
            .split_once(':')
            .is_some_and(|(scheme, _)| scheme.len() > 1 && !scheme.contains('/'));
        if scheme && !link.starts_with("file:") {
            self.open_url(link);
            return;
        }
        let link = link.strip_prefix("file://").unwrap_or(link);
        let (path, anchor) = match link.split_once('#') {
            Some((path, anchor)) => (path, Some(anchor)),
            None => (link, None),
        };
        let path = location::percent_decode(path);
        let base = match self.active_panel().path() {
            Some(file) => file.parent().map(Path::to_path_buf),
            None => None,
        }
        .unwrap_or_else(|| self.current_root());
        // A path from `/` is from the workspace's folder, as on GitHub,
        // unless it's a file there.
        let target = match path.strip_prefix('/') {
            Some(rest) if !Path::new(&path).exists() => self.current_root().join(rest),
            _ => base.join(&path),
        };
        if !target.exists() {
            self.show_message(format!("Can't find {path}."), true);
            return;
        }
        if target.is_dir() {
            self.tree_visible = true;
            self.layout();
            self.tree.reveal(&target);
            return;
        }
        if !self.open(&target, false) {
            return;
        }
        self.focus = Focus::Editor;
        if let Some(editor) = self.editor_mut().filter(|editor| editor.is_markdown()) {
            if !editor.reading() {
                editor.read_from(0);
            }
            if let Some(anchor) = anchor {
                editor.go_to_anchor(anchor);
            }
        }
    }

    /// Opens `url` in the browser.
    fn open_url(&mut self, url: &str) {
        let opener = match cfg!(target_os = "macos") {
            true => "open",
            false => "xdg-open",
        };
        let spawned = std::process::Command::new(opener)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => drop(std::thread::spawn(move || child.wait())),
            Err(err) => self.show_message(format!("Can't open {url}: {err}"), true),
        }
    }

    // --- terminals --------------------------------------------------------------

    /// A Ctrl+click at (`x`, `y`) in the active panel's terminal: opens
    /// the file or URL there, if any (see [`Terminal::target_at`]).
    /// Returns whether it did.
    fn open_from_terminal(&mut self, x: u32, y: u32) -> bool {
        let Some(terminal) = self.active_terminal() else {
            return false;
        };
        let target = terminal.borrow().target_at(x, y, self.workspace.roots());
        match target {
            None => false,
            Some(Target::Url(url)) => {
                self.open_url(&url);
                true
            }
            Some(Target::File(path, position)) => {
                self.open_beside_terminal(&path, position);
                true
            }
        }
    }

    /// Opens `path` at `position` in a panel other than the active one,
    /// which shows a terminal, so it stays in view: the panel active
    /// before, unless it shows a terminal too, or else another that
    /// doesn't, or else a new one to its right.
    fn open_beside_terminal(&mut self, path: &Path, position: Option<Position>) {
        let tab = self.tab();
        let usable = |panel: &&Panel| panel.id != tab.active && panel.terminal().is_none();
        let beside = tab
            .previous
            .and_then(|id| tab.panels.iter().find(|panel| panel.id == id))
            .filter(usable)
            .or_else(|| tab.panels.iter().find(usable))
            .map(|panel| panel.id);
        match beside {
            Some(id) => self.activate(id),
            // Without room to split, the file takes the terminal's place;
            // the terminal keeps running.
            None => self.split(Axis::Horizontal),
        }
        if self.open(path, false) {
            self.focus = Focus::Editor;
            if let Some(position) = position {
                self.go_to(position);
            }
        }
    }

    /// Starts a shell in the workspace's first folder, in the active panel.
    fn new_terminal(&mut self) {
        self.new_terminal_in(&self.current_root());
    }

    /// Starts a shell in `cwd`, in the active panel.
    fn new_terminal_in(&mut self, cwd: &Path) {
        let body = self.active_panel().body();
        let id = self.next_terminal;
        match Terminal::new(id, cwd, body) {
            Ok(terminal) => {
                self.next_terminal += 1;
                let terminal = Rc::new(RefCell::new(terminal));
                self.terminals.push(terminal.clone());
                self.show_terminal(terminal);
            }
            Err(err) => self.show_message(format!("Can't start a shell: {err}"), true),
        }
    }

    /// Shows `terminal` in the active panel, and gives it the keyboard. It
    /// leaves any other panel showing it, in any tab, since its pty has one
    /// size.
    fn show_terminal(&mut self, terminal: Rc<RefCell<Terminal>>) {
        for panel in tab::all_panels_mut(&mut self.tabs) {
            if panel
                .terminal()
                .is_some_and(|shown| Rc::ptr_eq(shown, &terminal))
            {
                panel.hide_terminal();
            }
        }
        // Coming back to a terminal, the keyboard is for the program.
        terminal.borrow_mut().blur_find();
        self.active_panel_mut().show_terminal(terminal);
        self.focus = Focus::Editor;
        self.prune_documents();
        self.prune_terminals();
        self.show_active_in_tree();
    }

    /// Closes `terminal`, hanging up on its shell and whatever it's
    /// running. A panel showing it is left empty.
    fn destroy_terminal(&mut self, terminal: &Rc<RefCell<Terminal>>) {
        for panel in tab::all_panels_mut(&mut self.tabs) {
            if panel
                .terminal()
                .is_some_and(|shown| Rc::ptr_eq(shown, terminal))
            {
                panel.hide_terminal();
            }
        }
        self.terminals.retain(|t| !Rc::ptr_eq(t, terminal));
        self.prune_terminals();
    }

    /// The terminal in the active panel, if any.
    fn active_terminal(&self) -> Option<Rc<RefCell<Terminal>>> {
        self.active_panel().terminal().cloned()
    }

    /// The terminal whose find bar has the keyboard, if any.
    fn finding_terminal(&self) -> Option<Rc<RefCell<Terminal>>> {
        self.keyboard_terminal()
            .filter(|terminal| terminal.borrow().find_focused())
    }

    /// The terminal keys go to, if one has the keyboard.
    fn keyboard_terminal(&self) -> Option<Rc<RefCell<Terminal>>> {
        let popup = self.picker.is_some()
            || self.alert.is_some()
            || self.search.is_some()
            || self.dialog.is_some()
            || self.menu.is_some()
            || self.tab_prompt.is_some();
        if self.focus != Focus::Editor || popup {
            return None;
        }
        self.active_terminal()
    }

    /// A key after the terminal prefix: a cue shortcut, as if no terminal
    /// had the keyboard. The prefix again goes to the shell, and Esc
    /// cancels.
    fn prefixed_key(&mut self, terminal: &Rc<RefCell<Terminal>>, key: Key) -> AppAction {
        let command = self.keymap.lookup(key, Context::Editor);
        if self.keymap.lookup_terminal(key) == Some(Command::TerminalPrefix) {
            terminal.borrow_mut().send_key(key);
            return AppAction::Continue;
        }
        if key == Key::new(KeyCode::Esc, Mods::NONE) {
            return AppAction::Continue;
        }
        match command {
            Some((command, select)) => self.run(command, select),
            None => {
                self.show_message(format!("{key} isn't a cue shortcut."), false);
                AppAction::Continue
            }
        }
    }

    /// Copies the active terminal's selection, or pastes the last text
    /// copied in cue into it. (The terminal cue runs in pastes its own
    /// clipboard itself.)
    fn terminal_clipboard(&mut self, command: Command) -> AppAction {
        let Some(terminal) = self.active_terminal() else {
            return AppAction::Continue;
        };
        if command == Command::Paste {
            if let Some(text) = self.clipboard.clone() {
                if terminal.borrow().exit().is_none() {
                    terminal.borrow_mut().paste(&text);
                }
            }
            return AppAction::Continue;
        }
        let Some(text) = terminal.borrow().selected_text() else {
            return AppAction::Continue;
        };
        self.clipboard = Some(text.clone());
        AppAction::Copy(text)
    }

    /// Tells terminals when they get or lose the keyboard, for programs
    /// that asked to know.
    fn note_terminal_focus(&mut self) {
        let focused = self.keyboard_terminal();
        let same = match (&focused, &self.focused_terminal) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some(old) = self.focused_terminal.take() {
            old.borrow_mut().focus(false);
        }
        if let Some(new) = &focused {
            new.borrow_mut().focus(true);
        }
        self.focused_terminal = focused;
    }

    /// Closes terminals whose shell exited and that no panel shows, in any
    /// tab.
    fn prune_terminals(&mut self) {
        let tabs = &self.tabs;
        self.terminals.retain(|terminal| {
            terminal.borrow().exit().is_none()
                || tab::all_panels(tabs).any(|panel| {
                    panel
                        .terminal()
                        .is_some_and(|shown| Rc::ptr_eq(shown, terminal))
                })
        });
        let terminals = &self.terminals;
        self.recent.retain(|recent| match recent {
            Recent::File(_) | Recent::Untitled(_) => true,
            Recent::Terminal(id) => terminals.iter().any(|t| t.borrow().id() == *id),
        });
    }

    // --- layout and helpers ----------------------------------------------------

    /// The tree's width on screen, 0 when hidden or when there's no room.
    fn visible_tree_width(&self) -> u32 {
        if !self.tree_visible {
            return 0;
        }
        let room = self.width.saturating_sub(MIN_EDITOR_WIDTH + 1);
        let width = self.tree_width.min(room);
        if width < MIN_TREE_WIDTH {
            0
        } else {
            width
        }
    }

    /// The panels' left column: right of the tree and its divider.
    fn editor_x(&self) -> u32 {
        match self.visible_tree_width() {
            0 => 0,
            width => width + 1,
        }
    }

    /// Where the panels go: right of the tree, below the tab bar, above the
    /// status bar.
    fn main_area(&self) -> Rect {
        let x = self.editor_x();
        let bar = self.bar_height();
        Rect {
            x,
            y: bar,
            width: self.width.saturating_sub(x).max(1),
            height: self.height.saturating_sub(1 + bar),
        }
    }

    /// The tab bar's height: a row with more than one tab, if that leaves
    /// room for a panel below it.
    fn bar_height(&self) -> u32 {
        u32::from(self.tabs.len() > 1 && self.height >= layout::MIN_HEIGHT + 2)
    }

    /// The tab bar, across the top of the panels.
    fn bar_area(&self) -> Rect {
        Rect {
            height: self.bar_height(),
            y: 0,
            ..self.main_area()
        }
    }

    fn layout(&mut self) {
        if self.visible_tree_width() == 0 && self.focus == Focus::Tree {
            self.focus = Focus::Editor;
        }
        // The tree is as tall as the tab bar and panels together.
        self.tree.set_height(self.height.saturating_sub(1));
        if let Some(picker) = &mut self.picker {
            picker.set_size(self.width, self.height);
        }
        if let Some(search) = &mut self.search {
            search.set_size(self.width, self.height);
        }
        if let Some(dialog) = &mut self.dialog {
            dialog.set_size(self.width, self.height);
        }
        if let Some((menu, _)) = &mut self.menu {
            menu.set_size(self.width, self.height);
        }
        if let Some(alert) = &mut self.alert {
            alert.set_size(self.width, self.height);
        }
        // Tabs off screen are laid out when they come back.
        let area = self.main_area();
        self.tab_mut().set_area(area);
    }

    /// The tab on screen.
    fn tab(&self) -> &Tab {
        &self.tabs[self.tab]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.tab]
    }

    fn active_panel(&self) -> &Panel {
        self.tab().active_panel()
    }

    fn active_panel_mut(&mut self) -> &mut Panel {
        self.tab_mut().active_panel_mut()
    }

    /// The active panel's editor, if it isn't empty.
    fn editor(&self) -> Option<&Editor> {
        self.active_panel().editor()
    }

    /// The active panel's editor, if it isn't empty, with its cursor in the
    /// buffer.
    fn editor_mut(&mut self) -> Option<&mut Editor> {
        self.active_panel_mut().editor_mut()
    }

    /// Shows `text` in the active panel's status bar until the next key.
    fn show_message(&mut self, text: impl Into<String>, error: bool) {
        self.active_panel_mut().show_message(text.into(), error);
    }

    /// The label of `command`'s shortcut, or its id if it has none.
    fn shortcut(&self, command: Command) -> String {
        match self.keymap.shortcut(command) {
            Some(key) => key.to_string(),
            None => command.id().to_string(),
        }
    }
}

/// What's running in `terminals`, by name, of those busy running something.
fn running_programs(terminals: &[Rc<RefCell<Terminal>>]) -> Vec<String> {
    terminals
        .iter()
        .map(|terminal| terminal.borrow())
        .filter(|terminal| terminal.is_busy())
        .map(|terminal| {
            terminal
                .program()
                .unwrap_or_else(|| "A program".to_string())
        })
        .collect()
}

/// What a session calls `visit`, if it's still about.
fn visit_shown(visit: &Visit) -> Option<Shown> {
    Some(match visit {
        Visit::File(_, Some(path)) => Shown::File(path.clone()),
        Visit::File(doc, None) => Shown::Untitled(doc.upgrade()?.untitled.get()),
        Visit::Terminal(id) => Shown::Terminal(*id),
        Visit::Image(path) => Shown::Image(path.clone()),
    })
}

/// What a session calls `doc`, unless it's an untitled one never typed in.
fn doc_shown(doc: &Document) -> Option<Shown> {
    match doc.path() {
        Some(path) => Some(Shown::File(path)),
        None if doc.is_blank() => None,
        None => Some(Shown::Untitled(doc.untitled.get())),
    }
}

/// Why going ahead would lose something: the files with `unsaved` changes,
/// and the programs `running` in terminals.
fn losing_reasons(unsaved: &[String], running: &[String]) -> String {
    let mut reasons = Vec::new();
    match unsaved {
        [] => {}
        [file] => reasons.push(format!("{file} has unsaved changes.")),
        files => reasons.push(format!("{} have unsaved changes.", files.join(", "))),
    }
    match running {
        [] => {}
        [program] => reasons.push(format!("{program} is running in a terminal.")),
        programs => reasons.push(format!("{} are running in terminals.", programs.join(", "))),
    }
    reasons.join(" ")
}

/// Exiting other than by quitting, as on a panic, an error, or a hangup,
/// copies what's unsaved, however recently it was copied.
impl Drop for App {
    fn drop(&mut self) {
        self.recovery
            .sync(&self.documents, self.workspace.roots(), true);
        self.save_session(false);
    }
}

/// `doc`, as the picker lists it.
fn document_target(doc: &Document) -> Recent {
    match doc.path() {
        Some(path) => Recent::File(path),
        None => Recent::Untitled(doc.untitled.get()),
    }
}

/// A name for a copy of `path` next to it, as the Finder names them:
/// `a copy.txt`, then `a copy 2.txt`, and so on.
fn copy_name(path: &Path) -> String {
    let name = file_name(path);
    let (stem, extension) = match path.extension() {
        Some(extension) if !path.is_dir() => (
            name.strip_suffix(&format!(".{}", extension.to_string_lossy()))
                .unwrap_or(&name),
            format!(".{}", extension.to_string_lossy()),
        ),
        _ => (name.as_str(), String::new()),
    };
    (1..)
        .map(|n| match n {
            1 => format!("{stem} copy{extension}"),
            n => format!("{stem} copy {n}{extension}"),
        })
        .find(|copy| fs::symlink_metadata(path.with_file_name(copy)).is_err())
        .unwrap_or_default()
}

/// Copies the file, folder, or symlink `from` to `to`, which doesn't exist.
fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if meta.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(drop)
    }
}

/// Moves `path` to the Trash.
#[cfg(not(test))]
fn move_to_trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let trashed = {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut context = trash::TrashContext::default();
        // The default, the Finder's AppleScript, asks for permission to
        // control the Finder, and plays its sound.
        context.set_delete_method(DeleteMethod::NsFileManager);
        context.delete(path)
    };
    #[cfg(not(target_os = "macos"))]
    let trashed = trash::delete(path);
    trashed.map_err(|err| err.to_string())
}

/// Tests delete instead, to leave the Trash alone.
#[cfg(test)]
fn move_to_trash(path: &Path) -> Result<(), String> {
    let removed = match path.is_dir() {
        true => fs::remove_dir_all(path),
        false => fs::remove_file(path),
    };
    removed.map_err(|err| err.to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyCode, Mods};
    use opentui::{OwnedBuffer, WidthMethod};
    use std::fs;
    use std::ops::Range;
    use std::time::Duration;

    /// A fresh workspace folder with `files` (name, contents).
    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-app-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, contents) in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    impl App {
        /// The active panel's editor, which the test expects.
        fn ed(&self) -> &Editor {
            self.editor().expect("a file is shown")
        }

        fn ed_mut(&mut self) -> &mut Editor {
            self.editor_mut().expect("a file is shown")
        }

        fn ed_is_shown(&self) -> bool {
            self.editor().is_some()
        }

        /// Whether the command palette lists `command`.
        fn palette_has(&mut self, command: Command) -> bool {
            self.show_picker(Mode::Commands);
            let listed = self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.lists_command(command));
            self.picker = None;
            listed
        }
    }

    fn app(root: &Path, file: Option<&str>) -> App {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        App::new(workspace, file.map(|f| root.join(f)), 80, 10).unwrap()
    }

    fn key(app: &mut App, code: KeyCode) -> AppAction {
        app.handle_key(Key::new(code, Mods::NONE))
    }

    fn ctrl(app: &mut App, c: char) -> AppAction {
        app.handle_key(Key::new(KeyCode::Char(c), Mods::CTRL))
    }

    /// Ctrl+`c` from a terminal, after the prefix that makes it cue's.
    fn prefixed_ctrl(app: &mut App, c: char) -> AppAction {
        app.handle_key(Key::new(KeyCode::Char('`'), Mods::CTRL));
        ctrl(app, c)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn screen(app: &App) -> String {
        let frame = OwnedBuffer::new(80, 10, false, WidthMethod::Unicode, "test").unwrap();
        app.draw(&frame);
        frame.to_text(true)
    }

    fn left_click(app: &mut App, x: u32, y: u32) {
        let now = Instant::now();
        for kind in [
            MouseKind::Press(MouseButton::Left),
            MouseKind::Release(MouseButton::Left),
        ] {
            app.handle_mouse(
                Mouse {
                    kind,
                    x,
                    y,
                    mods: Mods::NONE,
                },
                now,
            );
        }
    }

    #[test]
    fn clicking_the_language_changes_highlighting_and_escape_cancels() {
        let _serial = crate::test_serial();
        let root = fixture("language-picker", &[("a.txt", "fn main() {}")]);
        let mut app = app(&root, Some("a.txt"));
        let language_x = |app: &App| match app.ed().status() {
            crate::status::Status::EditorInfo { language, .. } => language.start,
            other => panic!("{other:?}"),
        };
        let x = language_x(&app);
        left_click(&mut app, x - 1, 9);
        assert!(app.picker.is_none());
        left_click(&mut app, x, 9);
        assert_eq!(app.picker.as_ref().unwrap().mode(), Mode::Languages);
        type_text(&mut app, "Rust");
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_none());
        let doc = app.ed().document().clone();
        assert_eq!(doc.language.get().unwrap().name, "Rust");
        assert!(doc.syntax.borrow().is_some());
        assert!(!doc.is_modified());
        doc.rename(root.join("renamed.py"));
        assert_eq!(doc.language.get().unwrap().name, "Rust");

        let x = language_x(&app);
        left_click(&mut app, x, 9);
        type_text(&mut app, "Python");
        key(&mut app, KeyCode::Esc);
        assert_eq!(doc.language.get().unwrap().name, "Rust");
        left_click(&mut app, x, 9);
        type_text(&mut app, "Plain Text");
        // Selecting a result with the mouse uses the same picker path.
        let text = screen(&app);
        let (y, line) = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("Plain Text"))
            .last()
            .unwrap();
        let x = line[..line.find("Plain Text").unwrap()].chars().count() as u32;
        left_click(&mut app, x, y as u32);
        assert!(app.picker.is_none());
        assert!(doc.language.get().is_none());
        assert!(doc.syntax.borrow().is_none());
    }

    #[test]
    fn opening_a_folder_focuses_the_tree_and_enter_opens_a_file() {
        let _serial = crate::test_serial();
        let root = fixture("enter", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, None);
        assert_eq!(app.focus, Focus::Tree);
        type_text(&mut app, "x");
        assert!(app.ed().is_blank(), "typing in the tree doesn't edit");

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("b.txt").as_path())
        );
        assert_eq!(app.documents.len(), 1, "the blank buffer was replaced");
        let text = screen(&app);
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("b.txt"), "the status bar names the file");
    }

    #[test]
    fn switching_files_keeps_unsaved_edits_and_quit_lists_them() {
        let _serial = crate::test_serial();
        let root = fixture("switch", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        assert_eq!(app.focus, Focus::Editor);
        type_text(&mut app, "1");

        // Ctrl+E, then Space previews b.txt without leaving the tree.
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("b.txt").as_path())
        );
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.documents.len(), 2);
        assert!(screen(&app).contains("1alpha"), "a.txt kept its edit");

        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        assert!(screen(&app).contains("a.txt has unsaved changes."));
        assert!(matches!(key(&mut app, KeyCode::Char('n')), AppAction::Quit));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha");
    }

    #[test]
    fn quitting_asks_about_unsaved_files_and_can_save_them() {
        let _serial = crate::test_serial();
        let root = fixture("quit", &[("a.txt", "alpha")]);
        let mut app = app(&root, None);
        assert!(
            matches!(ctrl(&mut app, 'q'), AppAction::Quit),
            "nothing unsaved"
        );
        app.focus = Focus::Editor;
        type_text(&mut app, "x");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        // An untitled file can't be saved from the alert.
        let text = screen(&app);
        assert!(text.contains(" Don't Save    Cancel "), "{text}");
        assert!(!text.contains(" Save    Don't Save "), "{text}");
        // The alert takes the keys, and Esc cancels it.
        type_text(&mut app, "y");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        key(&mut app, KeyCode::Esc);
        assert!(app.alert.is_none());
        assert_eq!(app.ed().text(), "x");

        assert!(app.open(&root.join("a.txt"), false));
        type_text(&mut app, "1");
        ctrl(&mut app, 'q');
        let text = screen(&app);
        assert!(text.contains("Quit cue?"), "{text}");
        assert!(
            text.contains("Untitled-1, a.txt have unsaved changes."),
            "{text}"
        );
        assert!(!text.contains("Save All"), "Untitled-1 has no name");
        key(&mut app, KeyCode::Esc);
        app.destroy_document(&app.find_untitled(1).unwrap());

        // Saved from the alert, then gone ahead with.
        ctrl(&mut app, 'q');
        assert!(
            screen(&app).contains(" Save    Don't Save "),
            "{}",
            screen(&app)
        );
        assert!(matches!(key(&mut app, KeyCode::Enter), AppAction::Quit));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "1alpha");
    }

    #[test]
    fn clipboard_is_shared_between_files() {
        let _serial = crate::test_serial();
        let root = fixture("clipboard", &[("a.txt", "copy me"), ("b.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'a');
        assert!(matches!(ctrl(&mut app, 'c'), AppAction::Copy(text) if text == "copy me"));
        app.open(&root.join("b.txt"), false);
        ctrl(&mut app, 'v');
        assert!(screen(&app).contains("copy me"));
        assert!(app.ed().is_modified());
    }

    #[test]
    fn popup_queries_select_copy_and_cut() {
        let _serial = crate::test_serial();
        let root = fixture("popup-select", &[("main.rs", "")]);
        let mut app = app(&root, None);
        let copied = |action| match action {
            AppAction::Copy(text) => Some(text),
            _ => None,
        };
        let shift_left = Key::new(KeyCode::Left, Mods::SHIFT);

        ctrl(&mut app, 'p');
        type_text(&mut app, "main");
        app.handle_key(shift_left);
        app.handle_key(shift_left);
        assert_eq!(copied(ctrl(&mut app, 'c')).as_deref(), Some("in"));
        assert_eq!(copied(ctrl(&mut app, 'x')).as_deref(), Some("in"));
        assert_eq!(app.clipboard.as_deref(), Some("in"));
        assert_eq!(copied(ctrl(&mut app, 'c')), None, "nothing selected");
        ctrl(&mut app, 'a');
        assert_eq!(copied(ctrl(&mut app, 'c')).as_deref(), Some("ma"));
        type_text(&mut app, "x");
        ctrl(&mut app, 'a');
        assert_eq!(copied(ctrl(&mut app, 'c')).as_deref(), Some("x"));
        key(&mut app, KeyCode::Esc);

        // Backspace deletes the path's `/` like any other character.
        ctrl(&mut app, 'o');
        key(&mut app, KeyCode::Backspace);
        ctrl(&mut app, 'a');
        let path = copied(ctrl(&mut app, 'c')).unwrap();
        assert!(!path.ends_with('/'), "{path}");
        key(&mut app, KeyCode::Backspace);
        assert!(app.dialog.as_ref().unwrap().selected_text().is_none());
    }

    #[test]
    fn programs_in_terminals_copy_to_the_clipboard() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-copy", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('n'), ctrl_shift));
        // OSC 52 with "copied" in base64, as programs send over ssh.
        type_text(&mut app, r"printf '\033]52;c;Y29waWVk\a'");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the copy", |app| app.copied.is_some());
        assert_eq!(app.take_copied().as_deref(), Some("copied"));
        assert_eq!(app.take_copied(), None);
        // Pasting in cue gets it too.
        assert_eq!(app.clipboard.as_deref(), Some("copied"));
    }

    /// Polls `app` until `done`, or panics after a few seconds.
    fn wait_until(app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{}",
                screen(app)
            );
            app.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn files_changed_on_disk_reload_or_ask_before_overwriting() {
        let _serial = crate::test_serial();
        let root = fixture("disk", &[("a.txt", "alpha\n"), ("sub/b.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        let file = root.join("a.txt");
        // Let the watcher start.
        std::thread::sleep(Duration::from_millis(200));

        // Another program changes it, and it's taken, without listing the
        // folder again.
        let listing = app.tree.listing();
        fs::write(&file, "alpha\nbeta\n").unwrap();
        wait_until(&mut app, "the change", |app| {
            app.ed().text() == "alpha\nbeta\n"
        });
        assert!(!app.ed().is_modified());
        assert_eq!(app.tree.listing(), listing);
        fs::write(root.join("new.txt"), "").unwrap();
        wait_until(&mut app, "the new file", |app| {
            screen(app).contains("new.txt")
        });
        // A folder expanded is watched from then on.
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Right);
        assert!(screen(&app).contains("b.txt"), "{}", screen(&app));
        fs::write(root.join("sub/c.txt"), "").unwrap();
        wait_until(&mut app, "the file in the folder", |app| {
            screen(app).contains("c.txt")
        });
        key(&mut app, KeyCode::Esc);

        // With unsaved changes, it keeps them, and saving asks first.
        type_text(&mut app, "x");
        fs::write(&file, "gamma\n").unwrap();
        wait_until(&mut app, "the conflict", |app| {
            app.ed().document().disk() == Disk::Changed
        });
        assert!(
            screen(&app).contains("a.txt [+] [changed on disk]"),
            "{}",
            screen(&app)
        );
        assert_eq!(app.ed().text(), "xalpha\nbeta\n");
        ctrl(&mut app, 's');
        assert!(screen(&app).contains("Save a.txt?"), "{}", screen(&app));
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(fs::read_to_string(&file).unwrap(), "xalpha\nbeta\n");
        assert!(!app.ed().is_modified());
        assert_eq!(app.ed().document().disk(), Disk::Same);

        // Or it takes the file's text, which undo takes back.
        type_text(&mut app, "y");
        fs::write(&file, "delta\n").unwrap();
        wait_until(&mut app, "the conflict", |app| {
            app.ed().document().disk() == Disk::Changed
        });
        ctrl(&mut app, 's');
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.ed().text(), "delta\n");
        assert!(!app.ed().is_modified());
        ctrl(&mut app, 'z');
        assert_eq!(app.ed().text(), "xyalpha\nbeta\n");

        // Deleted, it stays open, and saving writes it again.
        fs::remove_file(&file).unwrap();
        wait_until(&mut app, "the deletion", |app| {
            app.ed().document().disk() == Disk::Deleted
        });
        assert!(
            screen(&app).contains("a.txt [+] [deleted]"),
            "{}",
            screen(&app)
        );
        ctrl(&mut app, 's');
        assert!(app.alert.is_none());
        assert_eq!(fs::read_to_string(&file).unwrap(), "xyalpha\nbeta\n");
    }

    #[test]
    fn saving_all_asks_about_files_changed_on_disk_then_carries_on() {
        let _serial = crate::test_serial();
        let root = fixture("save-all-disk", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let mut app = app(&root, Some("a.txt"));
        // Let the watcher start.
        std::thread::sleep(Duration::from_millis(200));
        type_text(&mut app, "x");
        assert!(app.open(&root.join("b.txt"), false));
        type_text(&mut app, "y");

        // a.txt changes before quitting, and it's heard of; b.txt changes
        // while the alert is up.
        fs::write(root.join("a.txt"), "A\n").unwrap();
        wait_until(&mut app, "the conflict", |app| {
            app.documents.iter().any(|doc| doc.disk() == Disk::Changed)
        });
        ctrl(&mut app, 'q');
        assert!(screen(&app).contains(" Save All "), "{}", screen(&app));
        fs::write(root.join("b.txt"), "B\n").unwrap();

        // Each is asked about in turn, then it quits.
        key(&mut app, KeyCode::Enter);
        assert!(screen(&app).contains("Save a.txt?"), "{}", screen(&app));
        assert!(matches!(
            key(&mut app, KeyCode::Char('o')),
            AppAction::Continue
        ));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "xa\n");
        assert!(screen(&app).contains("Save b.txt?"), "{}", screen(&app));
        assert!(matches!(key(&mut app, KeyCode::Char('r')), AppAction::Quit));
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
    }

    /// The file the active panel shows, by name.
    fn shown_name(app: &App) -> Option<String> {
        app.editor()
            .and_then(Editor::path)
            .map(|path| file_name(&path))
    }

    #[test]
    fn popping_closes_what_a_panel_shows_and_goes_back() {
        let _serial = crate::test_serial();
        let root = fixture("pop", &[("a.txt", "a"), ("b.txt", "b")]);
        let mut app = app(&root, Some("a.txt"));
        app.open(&root.join("b.txt"), false);
        app.run(Command::NewTerminal, false);
        assert!(app.active_terminal().is_some());

        ctrl(&mut app, '0');
        // The login shell may be running its own startup.
        if app.alert.is_some() {
            key(&mut app, KeyCode::Char('c'));
        }
        assert!(app.terminals.is_empty());
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));
        ctrl(&mut app, '0');
        assert!(app.find_document(&root.join("b.txt")).is_none());
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        // What was popped isn't gone back to.
        ctrl(&mut app, '-');
        assert!(screen(&app).contains("Nothing to go back to."));
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        // The last leaves the panel empty.
        ctrl(&mut app, '0');
        assert!(app.active_panel().is_empty());
        assert!(app.documents.is_empty());
        ctrl(&mut app, '0');
        assert!(screen(&app).contains("Nothing to close."));

        // A file another panel shows stays open there.
        app.open(&root.join("b.txt"), false);
        ctrl(&mut app, '\\');
        app.open(&root.join("a.txt"), false);
        app.open(&root.join("b.txt"), false);
        ctrl(&mut app, '0');
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        assert!(app.find_document(&root.join("b.txt")).is_some());
    }

    #[test]
    fn panels_go_back_and_forward_through_what_they_showed() {
        let _serial = crate::test_serial();
        let root = fixture("history", &[("a.txt", "a"), ("b.txt", "b"), ("c.txt", "c")]);
        let mut app = app(&root, Some("a.txt"));
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        let forward = |app: &mut App| ctrl(app, '=');
        app.open(&root.join("b.txt"), false);
        app.open(&root.join("c.txt"), false);
        ctrl(&mut app, '-');
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));
        // Legacy terminals send Ctrl+- as Ctrl+_.
        ctrl(&mut app, '_');
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        ctrl(&mut app, '-');
        assert!(screen(&app).contains("Nothing to go back to."));
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        forward(&mut app);
        forward(&mut app);
        assert_eq!(shown_name(&app).as_deref(), Some("c.txt"));
        forward(&mut app);
        assert!(screen(&app).contains("Nothing to go forward to."));

        // Going somewhere new, there's no going forward.
        ctrl(&mut app, '-');
        app.open(&root.join("a.txt"), false);
        forward(&mut app);
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        ctrl(&mut app, '-');
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));

        // A closed file opens again.
        app.run(Command::CloseFile, false);
        assert!(!app.ed_is_shown());
        assert!(app.find_document(&root.join("b.txt")).is_none());
        ctrl(&mut app, '-');
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));

        // Terminals are in it too; the mouse's back button goes back.
        let terminal = Key::new(KeyCode::Char('n'), ctrl_shift);
        app.handle_key(terminal);
        assert!(app.active_terminal().is_some());
        let back = Mouse {
            kind: MouseKind::Press(MouseButton::Back),
            x: 50,
            y: 3,
            mods: Mods::NONE,
        };
        app.handle_mouse(back, Instant::now());
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));
        forward(&mut app);
        assert!(app.active_terminal().is_some());
        // Ctrl+- is the shell's; the prefix makes it cue's.
        prefixed_ctrl(&mut app, '-');
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));

        // Each panel has its own; a terminal another panel shows now is
        // skipped.
        ctrl(&mut app, '\\');
        assert!(!app.ed_is_shown());
        ctrl(&mut app, '-');
        assert!(screen(&app).contains("Nothing to go back to."));
        app.open(&root.join("c.txt"), false);
        let id = app.terminals[0].borrow().id();
        app.picker_action(PickerAction::Accept(Choice::Terminal(id)));
        assert!(app.active_terminal().is_some());
        app.run(Command::FocusPanelLeft, false);
        forward(&mut app);
        assert!(
            app.active_terminal().is_none(),
            "the terminal is the other panel's"
        );
        assert!(screen(&app).contains("Nothing to go forward to."));
    }

    #[test]
    fn header_buttons_go_back_forward_and_close_the_panel() {
        let _serial = crate::test_serial();
        let root = fixture("header-buttons", &[("a.txt", "a"), ("b.txt", "b")]);
        let mut app = app(&root, Some("a.txt"));
        app.open(&root.join("b.txt"), false);
        let area = app.active_panel().area();
        let header = screen(&app)
            .lines()
            .nth(area.y as usize)
            .unwrap()
            .to_string();
        assert!(header.ends_with(" <  >  × "), "{header:?}");
        let button = |i: u32| area.x + area.width - 9 + 3 * i + 1;
        left_click(&mut app, button(0), area.y);
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        left_click(&mut app, button(1), area.y);
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));
        // Clicking the header elsewhere doesn't.
        left_click(&mut app, area.x + 20, area.y);
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));

        // Closing the other of two panels gives its room back.
        app.run(Command::SplitRight, false);
        assert_eq!(app.tab().panels.len(), 2);
        let area = app.active_panel().area();
        left_click(&mut app, area.x + area.width - 2, area.y);
        assert_eq!(app.tab().panels.len(), 1);
        assert_eq!(shown_name(&app).as_deref(), Some("b.txt"));
        // The last one is emptied, as Ctrl+W does.
        let area = app.active_panel().area();
        left_click(&mut app, area.x + area.width - 2, area.y);
        assert!(!app.ed_is_shown());
    }

    #[test]
    fn terminal_picker_previews_without_switching_until_accepted() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-preview", &[("a.txt", "original editor")]);
        let mut app = app(&root, Some("a.txt"));
        app.resize(120, 30);
        app.run(Command::NewTerminal, false);
        let id = app.active_terminal().unwrap().borrow().id();
        app.open(&root.join("a.txt"), false);
        app.show_picker(Mode::Terminals);
        assert_eq!(
            app.picker.as_ref().unwrap().selected_choice(),
            Some(&Choice::Terminal(id))
        );
        let frame = OwnedBuffer::new(120, 30, false, WidthMethod::Unicode, "preview").unwrap();
        app.draw(&frame);
        // The preview is drawn under the list, untitled until it's named.
        let preview = app.picker.as_ref().unwrap().preview_area().unwrap();
        let text = frame.to_text(true);
        let top: String = text
            .lines()
            .nth(preview.y as usize)
            .unwrap()
            .chars()
            .skip(preview.x as usize)
            .take(preview.width as usize)
            .collect();
        let rule = "─".repeat(preview.width as usize - 2);
        assert_eq!(top, format!("╭{rule}╮"), "{text}");
        assert!(app.active_terminal().is_none());
        assert!(app.ed().path().is_some_and(|p| p.ends_with("a.txt")));
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());
        assert!(app.active_terminal().is_none());
        app.show_picker(Mode::Terminals);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.active_terminal().unwrap().borrow().id(), id);
    }

    #[test]
    fn terminals_take_the_keyboard_and_outlive_their_panel() {
        let _serial = crate::test_serial();
        let root = fixture("terminal", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('n'), ctrl_shift));
        assert!(app.active_terminal().is_some());
        assert!(app.editor().is_none());
        assert_eq!(app.focus, Focus::Editor);

        type_text(&mut app, "echo $((6*7))");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the shell's output", |app| {
            screen(app)
                .lines()
                .any(|line| line.trim_end().ends_with("│42"))
        });
        assert!(!app.ed_is_shown(), "typing went to the shell");

        // Ctrl+Shift reaches cue's Ctrl shortcut directly while the
        // terminal has focus, including uppercase terminal reports.
        app.handle_key(Key::new(KeyCode::Char('P'), ctrl_shift));
        assert!(app.picker.is_some());
        key(&mut app, KeyCode::Esc);
        assert!(app.keyboard_terminal().is_some());

        // Ctrl+P is the shell's, and cue's after the prefix, Ctrl+`, which
        // the status bar hints.
        assert!(screen(&app).contains("^` cue keys"), "{}", screen(&app));
        let prefix = Key::new(KeyCode::Char('`'), Mods::CTRL);
        app.handle_key(prefix);
        assert!(screen(&app).contains("Next shortcut will go to cue"));
        ctrl(&mut app, 'p');
        assert!(app.picker.is_some());
        key(&mut app, KeyCode::Esc);
        // Esc after it cancels; a key that's no shortcut says so.
        app.handle_key(prefix);
        key(&mut app, KeyCode::Esc);
        assert!(!app.terminal_prefix);
        app.handle_key(prefix);
        ctrl(&mut app, 'j');
        assert!(screen(&app).contains("Ctrl+J isn't a cue shortcut."));
        assert!(app.picker.is_none());

        // A program running keeps quitting from being immediate.
        type_text(&mut app, "sleep 30");
        key(&mut app, KeyCode::Enter);
        // Not only busy: the login shell may be running its own startup.
        wait_until(&mut app, "sleep to run", |app| {
            app.terminals[0].borrow().program().as_deref() == Some("sleep")
        });
        assert!(matches!(prefixed_ctrl(&mut app, 'q'), AppAction::Continue));
        assert!(
            screen(&app).contains("sleep is running in a terminal."),
            "{}",
            screen(&app)
        );
        key(&mut app, KeyCode::Esc);

        // Opening a file replaces the terminal, which keeps running.
        app.open(&root.join("a.txt"), false);
        assert!(app.active_terminal().is_none());
        assert!(screen(&app).contains("alpha"));
        assert_eq!(app.terminals.len(), 1);
        assert!(app.terminals[0].borrow().exit().is_none());

        // The picker lists it first, as the last thing shown: Enter goes back.
        ctrl(&mut app, 'p');
        assert!(screen(&app).contains("│ sleep"), "{}", screen(&app));
        key(&mut app, KeyCode::Enter);
        assert!(app.active_terminal().is_some());
        // And from there, back to the file.
        prefixed_ctrl(&mut app, 'p');
        key(&mut app, KeyCode::Enter);
        assert!(app.ed().path().is_some_and(|path| path.ends_with("a.txt")));
    }

    #[test]
    fn a_terminals_find_bar_has_the_keys_while_focused() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-find", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('n'), ctrl_shift));
        type_text(&mut app, "echo Fo''und; echo fo''und");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the shell's output", |app| {
            screen(app).to_lowercase().matches("found").count() == 2
        });
        let terminal = app.active_terminal().unwrap();
        let shell_line = |app: &App| {
            screen(app)
                .lines()
                .filter(|line| line.contains("echo"))
                .count()
        };
        let before = shell_line(&app);

        // Ctrl+Shift+F, Find from a terminal, opens the bar, which takes
        // what's typed; the shell gets none of it.
        app.handle_key(Key::new(KeyCode::Char('f'), ctrl_shift));
        type_text(&mut app, "found");
        assert_eq!(terminal.borrow().find_memory().unwrap().query.text, "found");
        assert!(screen(&app).contains("2 of 2"), "{}", screen(&app));
        // Enter goes up, Shift+Enter back down.
        key(&mut app, KeyCode::Enter);
        assert!(screen(&app).contains("1 of 2"), "{}", screen(&app));
        app.handle_key(Key::new(KeyCode::Enter, Mods::SHIFT));
        assert!(screen(&app).contains("2 of 2"));
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        app.handle_key(Key::new(KeyCode::Char('c'), alt));
        assert!(screen(&app).contains("1 of 1"), "{}", screen(&app));
        assert_eq!(app.find_memory.query.text, "found", "remembered");

        // Clicking the output gives the shell the keyboard, leaving the bar.
        let area = app.active_panel().body();
        left_click(&mut app, area.x + 1, area.y + area.height - 1);
        assert!(app.keyboard_terminal().is_some());
        assert!(terminal.borrow().find_open());
        assert!(!terminal.borrow().find_focused());
        // The shortcut focuses it again, then closes it; so does Esc.
        app.handle_key(Key::new(KeyCode::Char('f'), ctrl_shift));
        assert!(terminal.borrow().find_focused());
        app.handle_key(Key::new(KeyCode::Char('f'), ctrl_shift));
        assert!(!terminal.borrow().find_open());
        app.handle_key(Key::new(KeyCode::Char('f'), ctrl_shift));
        key(&mut app, KeyCode::Esc);
        assert!(!terminal.borrow().find_open());
        assert_eq!(shell_line(&app), before, "nothing went to the shell");
    }

    #[test]
    fn the_palette_has_terminal_commands_for_the_terminal_on_screen() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-commands", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let palette = |app: &mut App, query: &str| {
            app.run(Command::Palette, false);
            type_text(app, query);
            let shown = screen(app);
            key(app, KeyCode::Esc);
            shown
        };
        let shown = palette(&mut app, "terminal");
        assert!(shown.contains("New Terminal"), "{shown}");
        assert!(!shown.contains("Clear Terminal"), "{shown}");

        app.run(Command::NewTerminal, false);
        let shown = palette(&mut app, "terminal");
        assert!(shown.contains("Clear Terminal"), "{shown}");
        // More than fit on screen match "terminal".
        let shown = palette(&mut app, "rename terminal");
        assert!(shown.contains("Rename Terminal"), "{shown}");

        // Renaming it asks for a name in the status bar, which takes the
        // keys, even from the tree, and the name is shown there.
        let status_bar = |app: &App| screen(app).lines().last().unwrap().to_string();
        app.run(Command::FocusTree, false);
        app.run(Command::RenameTerminal, false);
        assert_eq!(status_bar(&app).trim_end(), " Rename terminal:");
        type_text(&mut app, "bu");
        app.paste("ild\nrest");
        key(&mut app, KeyCode::Enter);
        assert!(
            status_bar(&app).starts_with(" build"),
            "{}",
            status_bar(&app)
        );
        // The picker lists it by name.
        app.run(Command::GoToFile, false);
        type_text(&mut app, "build");
        let shown = screen(&app);
        let above_status = shown.rsplit_once('\n').unwrap().0;
        assert!(above_status.contains("build"), "{shown}");
        key(&mut app, KeyCode::Esc);
        // Renaming starts from the name; Esc keeps it, and an empty name
        // takes it away.
        app.run(Command::RenameTerminal, false);
        assert_eq!(status_bar(&app).trim_end(), " Rename terminal: build");
        key(&mut app, KeyCode::Esc);
        assert!(
            status_bar(&app).starts_with(" build"),
            "{}",
            status_bar(&app)
        );
        app.run(Command::RenameTerminal, false);
        for _ in 0..5 {
            key(&mut app, KeyCode::Backspace);
        }
        key(&mut app, KeyCode::Enter);
        assert!(
            !status_bar(&app).starts_with(" build"),
            "{}",
            status_bar(&app)
        );

        // Popping it, with Ctrl+0 even from the terminal, asks first, then
        // hangs up on what it's running, and goes back to the file.
        type_text(&mut app, "sleep 30");
        key(&mut app, KeyCode::Enter);
        // Not only busy: the login shell may be running its own startup.
        wait_until(&mut app, "sleep to run", |app| {
            app.terminals[0].borrow().program().as_deref() == Some("sleep")
        });
        ctrl(&mut app, '0');
        assert!(app.active_terminal().is_some());
        assert!(screen(&app).contains("sleep is running in a terminal."));
        key(&mut app, KeyCode::Char('c'));
        assert!(app.active_terminal().is_none());
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        assert!(app.terminals.is_empty());
        assert!(!app.recent.contains(&Recent::Terminal(1)));
        // Nothing to quit over.
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Quit));
    }

    #[test]
    fn mouse_opens_files_and_drags_the_divider() {
        let _serial = crate::test_serial();
        let root = fixture("mouse", &[("dir/inner.txt", "inside"), ("top.txt", "")]);
        let mut app = app(&root, None);
        // Rows: root, dir/, top.txt. Clicking dir/ expands it.
        left_click(&mut app, 5, 1);
        left_click(&mut app, 5, 2);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("dir/inner.txt").as_path())
        );
        assert_eq!(
            app.focus,
            Focus::Tree,
            "a single click keeps focus in the tree"
        );

        // Clicking the editor focuses it.
        left_click(&mut app, 50, 0);
        assert_eq!(app.focus, Focus::Editor);

        let now = Instant::now();
        let at = |kind, x| Mouse {
            kind,
            x,
            y: 3,
            mods: Mods::NONE,
        };
        app.handle_mouse(at(MouseKind::Press(MouseButton::Left), 30), now);
        app.handle_mouse(at(MouseKind::Drag(MouseButton::Left), 20), now);
        app.handle_mouse(at(MouseKind::Release(MouseButton::Left), 20), now);
        assert_eq!(app.visible_tree_width(), 20);
        assert_eq!(app.editor_x(), 21);
        // The editor draws right of the divider, below its header.
        let line = screen(&app).lines().nth(1).unwrap().to_string();
        assert_eq!(line.chars().nth(20), Some('│'));
        assert!(line.contains("inside"), "{line}");
    }

    #[test]
    fn toggling_the_tree_and_narrow_screens() {
        let _serial = crate::test_serial();
        let root = fixture("toggle", &[("a.txt", "")]);
        let mut app = app(&root, None);
        ctrl(&mut app, 'b');
        assert_eq!(app.visible_tree_width(), 0);
        assert_eq!(app.focus, Focus::Editor, "focus leaves a hidden tree");
        ctrl(&mut app, 'e');
        assert_eq!(app.focus, Focus::Tree, "focusing the tree shows it");
        assert_eq!(app.visible_tree_width(), config::get().tree_width);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);

        app.resize(45, 10);
        assert_eq!(app.visible_tree_width(), 0, "no room next to the editor");
        ctrl(&mut app, 'e');
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn save_as_shows_the_new_file_in_the_tree() {
        let _serial = crate::test_serial();
        let root = fixture("save-as", &[]);
        let mut app = app(&root, None);
        app.focus = Focus::Editor;
        type_text(&mut app, "hi");
        ctrl(&mut app, 's');
        assert_eq!(
            app.dialog.as_ref().map(FileDialog::purpose),
            Some(Purpose::SaveAs)
        );
        let path = root.join("new.txt");
        type_text(&mut app, "new.txt");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        assert!(screen(&app).contains("new.txt"));
        app.tree.run(Command::TreeLast);
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.documents.len(),
            1,
            "reopening switches to the open file"
        );
    }

    fn press(app: &mut App, x: u32, y: u32) {
        app.handle_mouse(
            Mouse {
                kind: MouseKind::Press(MouseButton::Left),
                x,
                y,
                mods: Mods::NONE,
            },
            Instant::now(),
        );
    }

    #[test]
    fn saving_an_unnamed_file_from_the_tree_asks_where_in_its_folder() {
        let _serial = crate::test_serial();
        let root = fixture("save-from-tree", &[("dir/a.txt", "")]);
        let mut app = app(&root, None);
        assert_eq!(app.focus, Focus::Tree);
        key(&mut app, KeyCode::Down);
        ctrl(&mut app, 's');
        assert!(app.dialog.is_some());
        type_text(&mut app, "named.txt");
        key(&mut app, KeyCode::Enter);
        assert!(
            root.join("dir/named.txt").exists(),
            "in the folder selected"
        );
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.documents.len(), 1);
    }

    #[test]
    fn save_as_asks_before_replacing_and_refuses_open_files() {
        let _serial = crate::test_serial();
        let root = fixture("save-as-other", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        app.open(&root.join("b.txt"), false);
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        with_mods(&mut app, KeyCode::Char('s'), ctrl_shift);
        assert!(screen(&app).contains("Save As"));
        // The name is suggested, and typing replaces it.
        type_text(&mut app, "a.txt");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_some());
        assert!(screen(&app).contains("a.txt is open. Close it first."));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha");

        for _ in 0..5 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "c.txt");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), "beta");
        assert_eq!(open_paths(&app), ["a.txt", "c.txt"]);
    }

    #[test]
    fn the_file_dialog_opens_and_creates_files() {
        let _serial = crate::test_serial();
        let root = fixture("dialog", &[("a.txt", "alpha"), ("dir/b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'o');
        assert!(screen(&app).contains("Open File"));
        // Pressed again, it closes.
        ctrl(&mut app, 'o');
        assert!(app.dialog.is_none());
        ctrl(&mut app, 'o');
        type_text(&mut app, "dir/b");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert_eq!(app.ed().text(), "beta");

        // It starts in the active file's folder; new folders are created.
        with_mods(&mut app, KeyCode::Char('n'), CTRL_ALT);
        assert!(screen(&app).contains("New File"));
        type_text(&mut app, "b.txt");
        key(&mut app, KeyCode::Enter);
        assert!(screen(&app).contains("b.txt already exists."));
        for _ in 0..5 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "sub/new.rs");
        key(&mut app, KeyCode::Enter);
        let new = root.join("dir/sub/new.rs");
        assert_eq!(fs::read_to_string(&new).unwrap(), "");
        assert_eq!(app.ed().path(), Some(new));
        assert_eq!(app.focus, Focus::Editor);
        assert!(screen(&app).contains("new.rs"), "revealed in the tree");
    }

    #[test]
    fn ctrl_n_opens_untitled_files_the_picker_lists() {
        let _serial = crate::test_serial();
        let root = fixture("untitled", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'n');
        assert!(screen(&app).contains("Untitled-1"));
        type_text(&mut app, "one");
        ctrl(&mut app, 'n');
        assert!(screen(&app).contains("Untitled-2"));
        // A blank one on screen is kept rather than stacked.
        ctrl(&mut app, 'n');
        assert_eq!(app.documents.len(), 3);
        type_text(&mut app, "two");

        go_to_file(&mut app);
        let text = screen(&app);
        assert!(text.contains("Untitled-1"), "{text}");
        key(&mut app, KeyCode::Esc);

        // Something else shown in its place, it stays open.
        assert!(app.open(&root.join("a.txt"), false));
        go_to_file(&mut app);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.ed().text(), "two");

        // Ctrl+W in the picker closes one, asking first.
        go_to_file(&mut app);
        key(&mut app, KeyCode::Down);
        ctrl(&mut app, 'w');
        assert!(screen(&app).contains("Untitled-1 has unsaved changes."));
        assert_eq!(app.documents.len(), 3);
        key(&mut app, KeyCode::Char('n'));
        assert!(app.picker.is_some());
        assert_eq!(app.documents.len(), 2);
        assert!(!screen(&app).contains("Untitled-1"));
        key(&mut app, KeyCode::Esc);

        // Saved, it's a file.
        ctrl(&mut app, 's');
        type_text(&mut app, "two.txt");
        key(&mut app, KeyCode::Enter);
        assert_eq!(fs::read_to_string(root.join("two.txt")).unwrap(), "two");
        ctrl(&mut app, 'n');
        assert!(
            screen(&app).contains("Untitled-1"),
            "the number is free again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_is_open_once_however_it_is_named() {
        let _serial = crate::test_serial();
        let root = fixture("same-file", &[("dir/a.txt", "")]);
        std::os::unix::fs::symlink(root.join("dir"), root.join("linked")).unwrap();
        std::os::unix::fs::symlink(root.join("dir/a.txt"), root.join("link.txt")).unwrap();
        let workspace = Workspace::new([root.clone()]).unwrap();
        let dotted = root.join("dir/../dir/a.txt");
        let mut app = App::new(workspace, Some(dotted), 80, 10).unwrap();
        let a = root.join("dir/a.txt");
        assert_eq!(app.ed().path().as_deref(), Some(a.as_path()));
        assert!(
            screen(&app).contains("▾ dir"),
            "revealed without `..` folders"
        );

        for other in ["linked/a.txt", "link.txt", "dir/a.txt"] {
            assert!(app.open(&root.join(other), false));
            assert_eq!(app.documents.len(), 1, "{other}");
        }
    }

    #[test]
    fn save_as_refuses_a_file_open_in_another_editor() {
        let _serial = crate::test_serial();
        let root = fixture("save-as-open", &[("a.txt", "original")]);
        let mut app = app(&root, Some("a.txt"));
        let (doc, _) = Document::open(None, app.theme.clone()).unwrap();
        app.documents.push(doc.clone());
        app.active_panel_mut().show(&doc).unwrap();
        type_text(&mut app, "other");
        ctrl(&mut app, 's');
        type_text(&mut app, "a.txt");
        // Once to confirm replacing it.
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "original");
        assert!(screen(&app).contains("a.txt is open. Close it first."));
    }

    #[test]
    fn the_file_opened_at_startup_is_shown_below_its_folders() {
        let _serial = crate::test_serial();
        let mut files: Vec<(String, &str)> = (0..12).map(|i| (format!("d{i:02}/x"), "")).collect();
        files.push(("zz/file.txt".to_string(), ""));
        // Files after it, so the scroll isn't clamped at the end of the list.
        files.extend((0..12).map(|i| (format!("f{i:02}"), "")));
        let files: Vec<(&str, &str)> = files.iter().map(|(f, c)| (f.as_str(), *c)).collect();
        let root = fixture("reveal-scroll", &files);
        let app = app(&root, Some("zz/file.txt"));
        let text = screen(&app);
        assert!(text.contains("▾ zz"), "{text}");
        assert!(text.contains("file.txt"), "{text}");
    }

    #[test]
    fn a_click_outside_an_alert_cancels_it() {
        let _serial = crate::test_serial();
        let root = fixture("click-quit", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        type_text(&mut app, "x");
        ctrl(&mut app, 'q');
        assert!(screen(&app).contains("unsaved changes"));
        press(&mut app, 0, 0);
        assert!(!screen(&app).contains("unsaved changes"));
        assert!(app.alert.is_none());
        assert_eq!(app.focus, Focus::Editor, "nor did the click go to the tree");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
    }

    #[test]
    fn failing_to_open_keeps_focus_and_names_the_reason_first() {
        let _serial = crate::test_serial();
        let root = fixture("bad-open", &[("a.txt", ""), ("bin.dat", "")]);
        fs::write(root.join("bin.dat"), [0xff, 0xfe]).unwrap();
        let mut app = app(&root, None);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.focus, Focus::Tree);
        assert!(app.ed().is_blank());
        let text = screen(&app);
        assert!(
            text.contains("Can't open: not valid UTF-8 (bin.dat)"),
            "{text}"
        );
    }

    #[test]
    fn images_open_in_panels_and_go_in_the_history() {
        let _serial = crate::test_serial();
        let root = fixture("images", &[("a.txt", "a"), ("broken.png", "not a png")]);
        fs::write(root.join("red.png"), image::TEST_PNG).unwrap();
        let mut app = app(&root, Some("a.txt"));
        assert!(app.open(&root.join("red.png"), false));
        assert!(!app.ed_is_shown());
        let text = screen(&app);
        assert!(text.contains("red.png"), "{text}");
        assert!(text.contains("4 × 2  PNG  75 B"), "{text}");
        assert_eq!(app.tab().title(), "red.png");
        assert!(matches!(app.shown(), Some(Recent::File(path)) if path == root.join("red.png")));

        // The wheel zooms it, and back out to its own size.
        let body = app.active_panel().body();
        let (x, y) = (body.x + body.width / 2, body.y + body.height / 2);
        mouse_at(&mut app, MouseKind::ScrollUp, x, y);
        assert!(screen(&app).contains("75 B  110%"), "{}", screen(&app));
        mouse_at(&mut app, MouseKind::ScrollDown, x, y);
        assert!(!screen(&app).contains('%'), "{}", screen(&app));

        // Back to the file, and forward to the image again.
        ctrl(&mut app, '-');
        assert_eq!(shown_name(&app).as_deref(), Some("a.txt"));
        ctrl(&mut app, '=');
        assert!(app.active_panel().image().is_some());

        // It follows the file when it's renamed.
        app.move_entry(&root.join("red.png"), &root.join("blue.png"))
            .unwrap();
        assert_eq!(app.tab().title(), "blue.png");

        // Closing it empties the panel.
        app.run(Command::CloseFile, false);
        assert!(app.active_panel().is_empty());

        // An image that doesn't decode says why.
        assert!(!app.open(&root.join("broken.png"), false));
        let text = screen(&app);
        assert!(text.contains("Can't open: "), "{text}");
        assert!(text.contains("(broken.png)"), "{text}");
    }

    #[test]
    fn an_image_opens_from_the_command_line() {
        let _serial = crate::test_serial();
        let root = fixture("image-arg", &[]);
        fs::write(root.join("red.png"), image::TEST_PNG).unwrap();
        let app = app(&root, Some("red.png"));
        assert!(app.active_panel().image().is_some());
        assert!(app.documents.is_empty());
        assert_eq!(app.focus, Focus::Editor);
        let workspace = Workspace::new([root.clone()]).unwrap();
        let bad = App::new(workspace, Some(root.join("missing.png")), 80, 10);
        assert!(bad.is_err());
    }

    #[test]
    fn switching_files_clears_stale_messages() {
        let _serial = crate::test_serial();
        let root = fixture("stale", &[("a.txt", "a"), ("b.txt", "b")]);
        let mut app = app(&root, Some("a.txt"));
        type_text(&mut app, "!");
        ctrl(&mut app, 's');
        assert!(screen(&app).contains("Wrote a.txt"));
        assert!(app.open(&root.join("b.txt"), false));
        assert!(app.open(&root.join("a.txt"), false));
        assert!(!screen(&app).contains("Wrote"));
    }

    /// The name of the file previewed.
    fn preview(app: &App) -> Option<String> {
        let path = app.preview.as_ref()?.path()?;
        Some(path.file_name()?.to_string_lossy().into_owned())
    }

    fn open_paths(app: &App) -> Vec<String> {
        app.documents
            .iter()
            .map(|doc| {
                let path = doc.path().unwrap();
                path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect()
    }

    #[test]
    fn previews_replace_each_other_until_kept() {
        let _serial = crate::test_serial();
        let root = fixture("preview", &[("a.txt", ""), ("b.txt", ""), ("c.txt", "")]);
        let mut app = app(&root, None);
        // Space steps through the files, one preview at a time.
        for _ in 0..3 {
            key(&mut app, KeyCode::Down);
            key(&mut app, KeyCode::Char(' '));
        }
        assert_eq!(open_paths(&app), ["c.txt"]);
        assert_eq!(preview(&app), Some("c.txt".into()));
        assert!(app.tree.active_is_preview());

        // Enter keeps the preview open.
        key(&mut app, KeyCode::Enter);
        assert_eq!(preview(&app), None);
        assert!(!app.tree.active_is_preview());
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Char(' '));
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(open_paths(&app), ["c.txt", "a.txt"]);

        // Editing keeps it too.
        key(&mut app, KeyCode::Esc);
        type_text(&mut app, "edit");
        assert_eq!(preview(&app), None);
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(open_paths(&app), ["c.txt", "a.txt", "b.txt"]);
    }

    #[test]
    fn the_file_opened_at_startup_is_kept() {
        let _serial = crate::test_serial();
        let root = fixture("startup-kept", &[("a.txt", ""), ("b.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        assert_eq!(preview(&app), None);
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
    }

    #[test]
    fn a_click_previews_and_a_double_click_keeps() {
        let _serial = crate::test_serial();
        let root = fixture("click-preview", &[("a.txt", ""), ("b.txt", "")]);
        let mut app = app(&root, None);
        let t0 = Instant::now();
        let click = |app: &mut App, y, at| {
            app.handle_mouse(
                Mouse {
                    kind: MouseKind::Press(MouseButton::Left),
                    x: 5,
                    y,
                    mods: Mods::NONE,
                },
                at,
            );
        };
        click(&mut app, 1, t0);
        assert_eq!(
            preview(&app),
            Some("a.txt".into()),
            "a single click previews"
        );
        assert_eq!(app.focus, Focus::Tree);
        click(&mut app, 1, t0 + Duration::from_millis(100));
        assert_eq!(preview(&app), None, "a double click keeps it");
        assert_eq!(app.focus, Focus::Editor, "and moves focus to it");
        // Slow clicks are two single clicks.
        click(&mut app, 2, t0 + Duration::from_secs(2));
        click(&mut app, 2, t0 + Duration::from_secs(3));
        assert_eq!(preview(&app), Some("b.txt".into()));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
    }

    /// Opens the file picker once the files are listed.
    fn go_to_file(app: &mut App) {
        wait_for_files(app);
        ctrl(app, 'p');
    }

    /// Waits for every listing of the files in progress, and shows the
    /// result in the picker if it's open.
    fn wait_for_files(app: &mut App) {
        app.files.wait();
        if let Some(picker) = &mut app.picker {
            picker.set_files(&app.files);
        }
    }

    #[test]
    fn ctrl_p_opens_a_file_by_name_and_goes_back_to_the_previous_one() {
        let _serial = crate::test_serial();
        let root = fixture(
            "go-to",
            &[("a.txt", "alpha"), ("src/abracadabra.rs", "magic")],
        );
        let mut app = app(&root, Some("a.txt"));
        go_to_file(&mut app);
        type_text(&mut app, "abcr");
        let text = screen(&app);
        assert!(text.contains("Go to File"), "{text}");
        assert!(text.contains("src/abracadabra.rs"), "{text}");
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path()),
            "typing doesn't edit"
        );
        assert!(!app.ed().is_modified());

        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert!(screen(&app).contains("magic"));

        // The previous file is listed first, so Ctrl+P, Enter switches back.
        go_to_file(&mut app);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
        assert_eq!(app.documents.len(), 2);
    }

    #[test]
    fn ctrl_k_runs_a_command_by_name() {
        let _serial = crate::test_serial();
        let root = fixture("palette", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'k');
        type_text(&mut app, "hide tree");
        let text = screen(&app);
        assert!(text.contains("Show or Hide File Tree"), "{text}");
        assert!(text.contains("Ctrl+B"), "shortcuts are shown: {text}");
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_none());
        assert_eq!(app.visible_tree_width(), 0);
    }

    #[test]
    fn picker_shortcuts_switch_toggle_and_close_it() {
        let _serial = crate::test_serial();
        let root = fixture("picker-keys", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'p');
        type_text(&mut app, "wrap");
        ctrl(&mut app, 'k');
        assert!(screen(&app).contains(">wrap"), "switching keeps the query");
        // Backspace past the `>` goes back to files.
        for _ in 0..5 {
            key(&mut app, KeyCode::Backspace);
        }
        assert!(screen(&app).contains("Go to File"));
        ctrl(&mut app, 'p');
        assert!(
            app.picker.is_none(),
            "pressed again, the shortcut closes it"
        );

        ctrl(&mut app, 'k');
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());

        // Other global shortcuts close it and run.
        type_text(&mut app, "x");
        ctrl(&mut app, 'k');
        ctrl(&mut app, 's');
        assert!(app.picker.is_none());
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "x");

        // Pastes go to the query.
        ctrl(&mut app, 'p');
        app.paste("a.t\rignored");
        assert!(screen(&app).contains("a.t "), "{}", screen(&app));
    }

    #[test]
    fn the_picker_shows_the_last_listing_while_refreshing_it() {
        let _serial = crate::test_serial();
        let root = fixture("stale-list", &[("a.txt", "")]);
        let mut app = app(&root, None);
        wait_for_files(&mut app);
        fs::write(root.join("b.txt"), "").unwrap();
        ctrl(&mut app, 'p');
        let text = screen(&app);
        assert!(
            text.contains("a.txt") && !text.contains("listing"),
            "{text}"
        );
        assert!(!text.contains("b.txt"), "not listed yet: {text}");

        wait_for_files(&mut app);
        assert!(screen(&app).contains("b.txt"));
    }

    #[test]
    fn clicking_outside_the_picker_closes_it() {
        let _serial = crate::test_serial();
        let root = fixture("picker-mouse", &[("a.txt", ""), ("b.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        go_to_file(&mut app);
        press(&mut app, 0, 9);
        assert!(app.picker.is_none());
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
    }

    fn search_workspace(app: &mut App) {
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('f'), ctrl_shift));
    }

    /// Waits for the search in progress to finish.
    fn wait_for_search(app: &mut App) {
        let started = Instant::now();
        while app.screen_contains_searching() {
            assert!(started.elapsed() < Duration::from_secs(10));
            app.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    impl App {
        fn screen_contains_searching(&self) -> bool {
            let text = screen(self);
            text.contains("Searching") || text.contains("searching")
        }
    }

    #[test]
    fn workspace_search_opens_the_match_selected() {
        let _serial = crate::test_serial();
        let root = fixture(
            "search",
            &[
                ("a.txt", "alpha"),
                ("src/b.rs", "fn main() {\n    needle();\n}\n"),
            ],
        );
        let mut app = app(&root, Some("a.txt"));
        search_workspace(&mut app);
        type_text(&mut app, "needle");
        wait_for_search(&mut app);
        let text = screen(&app);
        assert!(text.contains("Search"), "{text}");
        assert!(text.contains("src/b.rs"), "{text}");
        assert!(text.contains("2  ") && text.contains("needle();"), "{text}");
        assert!(!app.ed().is_modified(), "typing doesn't edit");

        key(&mut app, KeyCode::Enter);
        assert!(app.search.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("src/b.rs").as_path())
        );
        assert_eq!(app.ed().selected_text().as_deref(), Some("needle"));

        // Reopened, it keeps the query; the shortcut again closes it.
        search_workspace(&mut app);
        assert!(screen(&app).contains("needle"));
        search_workspace(&mut app);
        assert!(app.search.is_none());

        // Ctrl+P switches to the file picker; other shortcuts close it.
        search_workspace(&mut app);
        ctrl(&mut app, 'p');
        assert!(app.search.is_none() && app.picker.is_some());
        search_workspace(&mut app);
        assert!(app.search.is_some() && app.picker.is_none());
        ctrl(&mut app, 'b');
        assert!(app.search.is_none());
    }

    #[test]
    fn workspace_search_starts_from_the_selection_and_sees_unsaved_edits() {
        let _serial = crate::test_serial();
        let root = fixture("search-unsaved", &[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let mut app = app(&root, Some("a.txt"));
        // Unsaved: "two one".
        type_text(&mut app, "two ");
        ctrl(&mut app, 'a');
        app.ed_mut().select_in_line(0, 0..3);
        search_workspace(&mut app);
        wait_for_search(&mut app);
        let text = screen(&app);
        assert!(text.contains("│ two"), "the selection is the query: {text}");
        assert!(text.contains("a.txt") && text.contains("b.txt"), "{text}");

        // Typing replaces the query it started with.
        type_text(&mut app, "one");
        wait_for_search(&mut app);
        let text = screen(&app);
        assert!(text.contains("│ one "), "{text}");
        assert!(text.contains("1 of 1 in 1 file"), "{text}");
        key(&mut app, KeyCode::Esc);
        assert!(app.search.is_none());
        assert!(app.ed().is_modified(), "the edit is still there");
    }

    fn with_mods(app: &mut App, code: KeyCode, mods: Mods) -> AppAction {
        app.handle_key(Key::new(code, mods))
    }

    #[test]
    fn find_and_replace_from_the_keyboard() {
        let _serial = crate::test_serial();
        let root = fixture(
            "find-keys",
            &[("a.txt", "cat dog cat\nCat\n"), ("b.txt", "a cat\n")],
        );
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'f');
        type_text(&mut app, "cat");
        assert_eq!(
            app.ed().text(),
            "cat dog cat\nCat\n",
            "typing goes to the query"
        );
        assert_eq!(app.ed().selected_text().as_deref(), Some("cat"));
        let text = screen(&app);
        assert!(text.contains("▸  cat") && text.contains("1 of 3"), "{text}");

        // Enter steps through the matches; Alt+C matches case.
        key(&mut app, KeyCode::Enter);
        assert!(screen(&app).contains("2 of 3"));
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        with_mods(&mut app, KeyCode::Char('c'), alt);
        assert!(screen(&app).contains("of 2"), "{}", screen(&app));

        // Ctrl+H shows the replacement; Tab switches fields.
        ctrl(&mut app, 'h');
        type_text(&mut app, "cow");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.ed().find_field(), Some(find::Field::Find));
        key(&mut app, KeyCode::Tab);
        with_mods(&mut app, KeyCode::Enter, alt);
        assert_eq!(app.ed().text(), "cow dog cow\nCat\n");
        assert!(screen(&app).contains("Replaced 2 matches."));

        // Esc closes the bar, and the keys are the text's again.
        key(&mut app, KeyCode::Esc);
        assert!(!app.ed().find_open());
        type_text(&mut app, "!");
        assert!(app.ed().text().contains('!'));

        // Another file starts from the same query; Ctrl+G finds it without
        // opening the bar for typing.
        assert!(app.open(&root.join("b.txt"), false));
        ctrl(&mut app, 'g');
        assert_eq!(app.ed().selected_text().as_deref(), Some("cat"));
        assert_eq!(app.ed().find_field(), None);
        assert!(screen(&app).contains("1 of 1"));
        type_text(&mut app, "x");
        assert_eq!(app.ed().text(), "a x\n", "typing replaced the match");
    }

    #[test]
    fn find_bar_takes_pastes_and_palette_commands() {
        let _serial = crate::test_serial();
        let root = fixture("find-palette", &[("a.txt", "one two one\n")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'f');
        app.paste("one\rignored");
        assert_eq!(app.ed().find_memory().unwrap().query.text, "one");
        assert_eq!(app.ed().text(), "one two one\n");
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.ed().find_memory().unwrap().query.text, "on");

        // Replace All from the palette, with the bar closed, opens it for a
        // replacement.
        key(&mut app, KeyCode::Esc);
        ctrl(&mut app, 'k');
        type_text(&mut app, "replace all");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.ed().find_field(), Some(find::Field::Replace));
        type_text(&mut app, "1");
        with_mods(
            &mut app,
            KeyCode::Enter,
            Mods {
                alt: true,
                ..Mods::NONE
            },
        );
        assert_eq!(app.ed().text(), "1e two 1e\n");
    }

    const CTRL_ALT: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: false,
        sup: false,
    };

    /// Screen row `y`, columns `columns`.
    fn cells(app: &App, y: usize, columns: Range<usize>) -> String {
        let text = screen(app);
        let line = text.lines().nth(y).unwrap_or_default();
        line.chars()
            .skip(columns.start)
            .take(columns.len())
            .collect()
    }

    /// What the status bar would show with panel `id` active.
    fn status_of(app: &App, id: PanelId) -> String {
        let panel = app
            .tab()
            .panels
            .iter()
            .find(|panel| panel.id == id)
            .unwrap();
        match panel.status() {
            crate::status::Status::Info(info)
            | crate::status::Status::EditorInfo { text: info, .. } => info,
            other => panic!("{other:?}"),
        }
    }

    fn numbered(lines: u32) -> String {
        (1..=lines).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn splitting_opens_an_empty_panel_to_open_a_file_in() {
        let _serial = crate::test_serial();
        let root = fixture("split", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert_eq!(app.tab().panels.len(), 2);
        assert!(app.editor().is_none(), "the new panel is active, and empty");
        let text = screen(&app);
        assert!(
            text.contains("Go to File") && text.contains("Ctrl+P"),
            "{text}"
        );
        assert!(cells(&app, 0, 41..80).contains("No file"), "{text}");
        assert!(
            cells(&app, 0, 0..40).contains("a.txt"),
            "the header: {text}"
        );
        assert!(cells(&app, 1, 0..40).contains("alpha"), "{text}");
        assert_eq!(cells(&app, 1, 40..41), "│");
        type_text(&mut app, "x");
        assert!(!app.documents[0].is_modified(), "typing in it does nothing");

        go_to_file(&mut app);
        type_text(&mut app, "b.txt");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("b.txt").as_path())
        );
        assert!(cells(&app, 1, 0..40).contains("alpha"));
        assert!(cells(&app, 1, 41..80).contains("beta"));
        let headers = cells(&app, 0, 0..80);
        assert!(
            headers.contains("a.txt") && headers.contains("b.txt"),
            "{headers}"
        );
        // One status bar, the active panel's.
        assert!(cells(&app, 9, 0..80).starts_with(" Ln 1, Col 1  Spaces: 4  Plain Text"));

        // Split down, from a panel showing a file.
        with_mods(
            &mut app,
            KeyCode::Char('\\'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        assert_eq!(app.tab().panels.len(), 3);
        assert_eq!(
            app.active_panel().area(),
            Rect {
                x: 41,
                y: 5,
                width: 39,
                height: 4
            }
        );
        assert!(cells(&app, 5, 41..80).contains("No file"), "its header");
    }

    #[test]
    fn a_file_in_two_panels_shares_its_text_but_not_its_cursor() {
        let _serial = crate::test_serial();
        let root = fixture("shared", &[("a.txt", &numbered(40))]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("a.txt"), false));
        assert_eq!(app.documents.len(), 1, "opened once");

        // Down in the right panel, and type there.
        for _ in 0..30 {
            key(&mut app, KeyCode::Down);
        }
        type_text(&mut app, "X");
        assert!(
            cells(&app, 1, 0..40).contains("line 1 "),
            "the left one stays put"
        );
        assert!(screen(&app).contains("Xline 31"));
        assert!(status_of(&app, 0).contains("Ln 1, Col 1"));
        assert!(status_of(&app, 1).contains("Ln 31, Col 2"));
        assert!(
            cells(&app, 9, 0..80).starts_with(" Ln 31, Col 2"),
            "the active one's"
        );

        // Back left, typing goes where its cursor was.
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert_eq!(app.tab().active, 0);
        type_text(&mut app, "Y");
        assert!(app.ed().text().starts_with("Yline 1\n"));
        assert!(
            cells(&app, 1, 0..40).contains("Yline 1"),
            "{}",
            screen(&app)
        );
        assert!(screen(&app).contains("Xline 31"), "the right one stays put");
        assert!(status_of(&app, 1).contains("Ln 31, Col 2"));
        // Its cursor moves along with lines added above it.
        key(&mut app, KeyCode::Enter);
        assert!(status_of(&app, 1).contains("Ln 32, Col 2"));
        ctrl(&mut app, 'z');
        assert!(status_of(&app, 1).contains("Ln 31, Col 2"));

        // They share the undo history: the right panel's edit is undone
        // second, and the cursor goes to it.
        ctrl(&mut app, 'z');
        assert!(app.ed().text().starts_with("line 1\n"));
        ctrl(&mut app, 'z');
        assert!(!app.ed().text().contains('X'));
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        type_text(&mut app, "Z");
        assert!(app.ed().text().contains("Zline 31"), "{}", app.ed().text());
    }

    #[test]
    fn closing_a_panel_gives_its_room_to_its_neighbor() {
        let _serial = crate::test_serial();
        let root = fixture("close", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("b.txt"), false));
        type_text(&mut app, "edited ");
        // Its file goes with it, after asking about the edits.
        ctrl(&mut app, 'w');
        assert_eq!(app.tab().panels.len(), 2);
        assert!(screen(&app).contains("b.txt has unsaved changes."));
        // Selecting Don't Save.
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.tab().panels.len(), 1);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
        assert_eq!(app.active_panel().area().width, 80);
        assert_eq!(open_paths(&app), ["a.txt"]);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "beta");

        // A file another panel shows stays open.
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("a.txt"), false));
        ctrl(&mut app, 'w');
        assert_eq!(app.tab().panels.len(), 1);
        assert_eq!(open_paths(&app), ["a.txt"]);

        // Files shown there before stay open; the last panel empties.
        assert!(app.open(&root.join("b.txt"), false));
        type_text(&mut app, "edited ");
        assert!(app.open(&root.join("a.txt"), false));
        ctrl(&mut app, 'w');
        assert_eq!(app.tab().panels.len(), 1);
        assert!(app.editor().is_none());
        assert!(screen(&app).contains("No file"));
        assert_eq!(open_paths(&app), ["b.txt"]);
        assert!(
            matches!(ctrl(&mut app, 'q'), AppAction::Continue),
            "b.txt is unsaved"
        );
    }

    #[test]
    fn closing_a_busy_terminal_asks_first() {
        let _serial = crate::test_serial();
        let root = fixture("close-terminal", &[]);
        let mut app = app(&root, None);
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('`'), ctrl_shift));
        type_text(&mut app, "sleep 30");
        key(&mut app, KeyCode::Enter);
        // Not only busy: the login shell may be running its own startup.
        wait_until(&mut app, "sleep to run", |app| {
            app.terminals[0].borrow().program().as_deref() == Some("sleep")
        });
        prefixed_ctrl(&mut app, 'w');
        assert_eq!(app.terminals.len(), 1);
        let text = screen(&app);
        assert!(text.contains("Close Terminal?"), "{text}");
        assert!(text.contains("sleep is running in a terminal."), "{text}");
        // The alert has the keyboard, not the terminal.
        key(&mut app, KeyCode::Char('c'));
        assert!(app.terminals.is_empty());
        assert!(app.active_terminal().is_none());
    }

    #[test]
    fn a_blank_untitled_file_goes_away_with_its_panel() {
        let _serial = crate::test_serial();
        let root = fixture("close-new", &[]);
        let mut app = self::app(&root, None);
        ctrl(&mut app, '\\');
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert!(app.ed().is_blank());
        ctrl(&mut app, 'w');
        assert_eq!(app.tab().panels.len(), 1);
        assert!(app.documents.is_empty(), "{}", app.documents.len());
    }

    #[test]
    fn the_mouse_focuses_scrolls_and_resizes_panels() {
        let _serial = crate::test_serial();
        let root = fixture(
            "panel-mouse",
            &[("a.txt", "alpha"), ("b.txt", &numbered(40))],
        );
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("b.txt"), false));

        // On the line numbers: the start of the line.
        left_click(&mut app, 1, 1);
        assert_eq!(app.tab().active, 0);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
        // A header gives its panel the keyboard, and leaves the cursor.
        left_click(&mut app, 60, 0);
        assert_eq!(app.tab().active, 1);
        left_click(&mut app, 30, 0);
        assert_eq!(app.tab().active, 0);

        // The wheel scrolls the other panel without taking the keyboard.
        let wheel = |app: &mut App| {
            app.handle_mouse(
                Mouse {
                    kind: MouseKind::ScrollDown,
                    x: 60,
                    y: 3,
                    mods: Mods::NONE,
                },
                Instant::now(),
            )
        };
        wheel(&mut app);
        wheel(&mut app);
        assert_eq!(app.tab().active, 0);
        assert!(
            !cells(&app, 1, 41..80).contains("line 1 "),
            "{}",
            screen(&app)
        );
        type_text(&mut app, "!");
        assert_eq!(app.ed().text(), "!alpha");

        // Dragging the divider resizes the panels.
        let drag = |app: &mut App, kind, x, y| {
            app.handle_mouse(
                Mouse {
                    kind,
                    x,
                    y,
                    mods: Mods::NONE,
                },
                Instant::now(),
            )
        };
        drag(&mut app, MouseKind::Press(MouseButton::Left), 40, 3);
        drag(&mut app, MouseKind::Drag(MouseButton::Left), 30, 3);
        drag(&mut app, MouseKind::Release(MouseButton::Left), 30, 3);
        assert_eq!(app.tab().panels[0].area().width, 30);
        assert_eq!(cells(&app, 0, 30..31), "│");
        assert_eq!(app.tab().active, 0, "dragging doesn't move the keyboard");

        // Stacked panels: the lower one's header drags.
        ctrl(&mut app, '|');
        with_mods(
            &mut app,
            KeyCode::Char('\\'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        assert_eq!(
            app.active_panel().area(),
            Rect {
                x: 0,
                y: 5,
                width: 30,
                height: 4
            }
        );
        drag(&mut app, MouseKind::Press(MouseButton::Left), 10, 5);
        drag(&mut app, MouseKind::Drag(MouseButton::Left), 10, 3);
        drag(&mut app, MouseKind::Release(MouseButton::Left), 10, 3);
        assert_eq!(
            app.active_panel().area(),
            Rect {
                x: 0,
                y: 3,
                width: 30,
                height: 6
            }
        );
    }

    #[test]
    fn panels_move_by_their_headers() {
        let _serial = crate::test_serial();
        let root = fixture("panel-move", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("b.txt"), false));
        let mouse = |app: &mut App, kind, x, y| {
            app.handle_mouse(
                Mouse {
                    kind,
                    x,
                    y,
                    mods: Mods::NONE,
                },
                Instant::now(),
            );
        };
        let area = |app: &App, id| app.tab().panels.iter().find(|p| p.id == id).unwrap().area();
        let tinted = |app: &App, x, y| {
            let frame = OwnedBuffer::new(80, 10, false, WidthMethod::Unicode, "t").unwrap();
            app.draw(&frame);
            frame.bg_at(x, y) != Some(theme::colors().bg)
        };

        // Dropped in the middle of the other panel, they swap. Over itself,
        // it would stay.
        mouse(&mut app, MouseKind::Press(MouseButton::Left), 10, 0);
        assert_eq!(
            app.tab().active,
            0,
            "grabbing a header gives it the keyboard"
        );
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 20, 0);
        assert!(app.header_drag.as_ref().unwrap().drop.is_none());
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 60, 4);
        assert!(tinted(&app, 60, 4), "the landing spot is tinted");
        mouse(&mut app, MouseKind::Release(MouseButton::Left), 60, 4);
        assert_eq!(area(&app, 0).x, 41);
        assert!(cells(&app, 0, 41..80).contains("a.txt"), "{}", screen(&app));
        assert!(cells(&app, 0, 0..40).contains("b.txt"));

        // Dropped near the bottom of the other panel, it goes below it.
        mouse(&mut app, MouseKind::Press(MouseButton::Left), 60, 0);
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 10, 8);
        mouse(&mut app, MouseKind::Release(MouseButton::Left), 10, 8);
        assert_eq!(
            area(&app, 1),
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 5
            }
        );
        assert_eq!(
            area(&app, 0),
            Rect {
                x: 0,
                y: 5,
                width: 80,
                height: 4
            }
        );

        // Its header now resizes the split above it when dragged up or
        // down, even drifting sideways, as quick drags do.
        mouse(&mut app, MouseKind::Press(MouseButton::Left), 40, 5);
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 44, 3);
        assert_eq!(area(&app, 0).y, 3);
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 50, 4);
        assert_eq!(area(&app, 0).y, 4);
        assert!(app.header_drag.as_ref().is_some_and(|d| !d.moving));
        mouse(&mut app, MouseKind::Release(MouseButton::Left), 50, 4);

        // Dragged sideways first, the panel moves.
        mouse(&mut app, MouseKind::Press(MouseButton::Left), 40, 4);
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 41, 4);
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 44, 3);
        assert!(app.header_drag.as_ref().is_some_and(|d| d.moving));
        assert_eq!(area(&app, 0).y, 4, "the split is back where it was");
        mouse(&mut app, MouseKind::Drag(MouseButton::Left), 4, 2);
        mouse(&mut app, MouseKind::Release(MouseButton::Left), 4, 2);
        assert_eq!(
            area(&app, 0),
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 9
            }
        );
        assert_eq!(area(&app, 1).x, 41);
        assert!(app.header_drag.is_none());
        assert!(!tinted(&app, 4, 2));
    }

    #[test]
    fn the_keyboard_moves_between_panels_and_the_tree() {
        let _serial = crate::test_serial();
        let root = fixture("panel-keys", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, '\\');
        let right = app.tab().active;
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert_eq!((app.tab().active, app.focus), (0, Focus::Editor));
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert_eq!(app.focus, Focus::Tree, "left of the panels is the tree");
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!((app.tab().active, app.focus), (0, Focus::Editor));
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!(app.tab().active, right);
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!(app.tab().active, right, "nothing further right");

        // Too small to split again.
        ctrl(&mut app, '\\');
        assert_eq!(app.tab().panels.len(), 2);
        assert!(
            screen(&app).contains("Not enough space to split"),
            "{}",
            screen(&app)
        );
    }

    #[test]
    fn a_preview_another_panel_shows_stays_open() {
        let _serial = crate::test_serial();
        let root = fixture("preview-panels", &[("a.txt", ""), ("b.txt", "")]);
        let mut app = app(&root, None);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(preview(&app), Some("a.txt".into()));
        ctrl(&mut app, '\\');
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
        assert_eq!(preview(&app), Some("b.txt".into()));
        assert!(cells(&app, 0, 0..80).contains("a.txt"), "{}", screen(&app));
    }

    #[test]
    fn panels_survive_tiny_screens() {
        let _serial = crate::test_serial();
        let root = fixture("tiny", &[("a.txt", &numbered(50))]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("a.txt"), false));
        with_mods(
            &mut app,
            KeyCode::Char('\\'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        // A second tab, for the tab bar.
        ctrl(&mut app, 't');
        assert!(app.open(&root.join("a.txt"), false));
        for (width, height) in [(30, 5), (3, 2), (1, 1), (0, 0), (200, 60), (80, 10)] {
            app.resize(width, height);
            let frame = OwnedBuffer::new(
                width.max(1),
                height.max(1),
                false,
                WidthMethod::Unicode,
                "t",
            )
            .unwrap();
            app.draw(&frame);
            for (x, y) in [
                (0, 0),
                (width / 2, height / 2),
                (width.saturating_sub(1), 0),
            ] {
                left_click(&mut app, x, y);
                type_text(&mut app, "x");
            }
        }
        assert!(screen(&app).contains("│"));
    }

    // --- tabs -------------------------------------------------------------------

    /// The tab bar's text, row 0, with the tree hidden.
    fn tab_bar(app: &App) -> String {
        cells(app, 0, 0..80).trim_end().to_string()
    }

    #[test]
    fn tabs_switch_the_whole_layout() {
        let _serial = crate::test_serial();
        let root = fixture("tabs", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("b.txt"), false));
        assert!(
            cells(&app, 0, 0..80).contains("a.txt"),
            "no bar with one tab"
        );

        // A new tab has one empty panel, below a bar naming the tabs for
        // their active panels.
        ctrl(&mut app, 't');
        assert_eq!((app.tabs.len(), app.tab), (2, 1));
        assert_eq!(app.tab().panels.len(), 1);
        assert!(app.editor().is_none());
        assert_eq!(tab_bar(&app), " 1 b.txt  2 Empty  +");
        assert!(
            cells(&app, 1, 0..80).contains("No file"),
            "{}",
            screen(&app)
        );
        assert_eq!(app.active_panel().area().y, 1);

        // A file can be in both.
        assert!(app.open(&root.join("a.txt"), false));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
        assert_eq!(tab_bar(&app), " 1 b.txt  2 a.txt  +");

        // Back to the first, as it was.
        with_mods(&mut app, KeyCode::Char('['), CTRL_ALT);
        assert_eq!(app.tab, 0);
        assert_eq!(app.tab().panels.len(), 2);
        assert!(cells(&app, 2, 0..40).contains("alpha"), "{}", screen(&app));
        assert!(cells(&app, 2, 41..80).contains("beta"));
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("b.txt").as_path())
        );
        app.handle_key(Key::new(KeyCode::PageDown, Mods::CTRL));
        assert_eq!(app.tab, 1);
        with_mods(&mut app, KeyCode::Char(']'), CTRL_ALT);
        assert_eq!(app.tab, 0, "wrapping around");
        ctrl(&mut app, '2');
        assert_eq!(app.tab, 1);
        ctrl(&mut app, '3');
        assert_eq!(app.tab, 1);
        assert!(screen(&app).contains("Tab 3 doesn't exist."));
        ctrl(&mut app, '1');
        assert_eq!(app.tab, 0);

        // Moving it right, then closing the other: a.txt stays open, since
        // the tab left shows it too, and that tab fills the screen again.
        with_mods(
            &mut app,
            KeyCode::PageDown,
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        assert_eq!(tab_bar(&app), " 1 a.txt  2 b.txt  +");
        assert_eq!(app.tab, 1);
        with_mods(&mut app, KeyCode::Char('['), CTRL_ALT);
        with_mods(&mut app, KeyCode::Char('w'), CTRL_ALT);
        assert_eq!((app.tabs.len(), app.tab), (1, 0));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
        assert_eq!(app.tab().panels.len(), 2);
        assert_eq!(app.active_panel().area().y, 0);
        assert!(cells(&app, 0, 0..40).contains("a.txt"), "{}", screen(&app));
        assert!(cells(&app, 0, 41..80).contains("b.txt"));
    }

    #[test]
    fn closing_a_tab_asks_before_losing_edits() {
        let _serial = crate::test_serial();
        let root = fixture("close-tab", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        with_mods(&mut app, KeyCode::Char(']'), CTRL_ALT);
        assert!(screen(&app).contains("No other tabs. Use Ctrl+T to open a new tab."));

        ctrl(&mut app, 't');
        assert!(app.open(&root.join("b.txt"), false));
        type_text(&mut app, "edited ");
        with_mods(&mut app, KeyCode::Char('w'), CTRL_ALT);
        assert_eq!(app.tabs.len(), 2);
        let text = screen(&app);
        assert!(text.contains("Close tab 2?"), "{text}");
        assert!(text.contains("b.txt has unsaved changes."), "{text}");
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(open_paths(&app), ["a.txt"]);
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "beta");

        // Closing a tab's last panel once it's empty closes the tab, but
        // not the only one.
        ctrl(&mut app, 't');
        assert_eq!(app.tabs.len(), 2);
        ctrl(&mut app, 'w');
        assert_eq!(app.tabs.len(), 1);
        assert!(app.ed_is_shown(), "back to a.txt");
        ctrl(&mut app, 'w');
        ctrl(&mut app, 'w');
        assert_eq!(app.tabs.len(), 1);
        assert!(app.active_panel().is_empty());

        // Closing the only tab empties it.
        assert!(app.open(&root.join("a.txt"), false));
        ctrl(&mut app, '\\');
        with_mods(&mut app, KeyCode::Char('w'), CTRL_ALT);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tab().panels.len(), 1);
        assert!(app.active_panel().is_empty());
        assert!(app.documents.is_empty());
    }

    #[test]
    fn a_terminal_moves_to_the_tab_it_is_shown_in() {
        let _serial = crate::test_serial();
        let root = fixture("tab-terminal", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        app.handle_key(Key::new(KeyCode::Char('n'), ctrl_shift));
        let terminal = app.active_terminal().expect("a terminal");
        // Ctrl+T is the shell's; after the prefix, it's cue's.
        ctrl(&mut app, 't');
        assert_eq!(app.tabs.len(), 1);
        prefixed_ctrl(&mut app, 't');
        assert_eq!(app.tabs.len(), 2);
        assert!(app.active_terminal().is_none());

        app.show_terminal(terminal.clone());
        assert!(app.active_terminal().is_some());
        assert!(
            app.tabs[0].active_panel().is_empty(),
            "a terminal is in one panel at a time"
        );
        with_mods(&mut app, KeyCode::Char('['), CTRL_ALT);
        assert!(app.active_terminal().is_none());

        // Closing its tab hangs up on it. Ctrl+2 gets there from a
        // terminal too.
        ctrl(&mut app, '2');
        assert_eq!(app.tab, 1);
        with_mods(&mut app, KeyCode::Char('w'), CTRL_ALT);
        if app.tabs.len() == 2 {
            // The login shell was still starting up: it asked first.
            with_mods(&mut app, KeyCode::Char('w'), CTRL_ALT);
        }
        assert_eq!(app.tabs.len(), 1);
        assert!(app.terminals.is_empty());
    }

    #[test]
    fn the_mouse_switches_opens_closes_moves_and_renames_tabs() {
        let _serial = crate::test_serial();
        let root = fixture("tab-mouse", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, 'b');
        ctrl(&mut app, 't');
        assert!(app.open(&root.join("b.txt"), false));
        assert_eq!(tab_bar(&app), " 1 a.txt  2 b.txt  +");

        left_click(&mut app, 2, 0);
        assert_eq!(app.tab, 0);
        // The + opens one after the tab on screen.
        left_click(&mut app, 19, 0);
        assert_eq!(tab_bar(&app), " 1 a.txt  2 Empty  3 b.txt  +");
        assert_eq!(app.tab, 1);
        mouse_at(&mut app, MouseKind::Press(MouseButton::Middle), 12, 0);
        assert_eq!(tab_bar(&app), " 1 a.txt  2 b.txt  +");
        assert_eq!(app.tab, 1, "b.txt, after the closed tab");

        // Dragged along the bar, a tab moves.
        mouse_at(&mut app, MouseKind::Press(MouseButton::Left), 2, 0);
        mouse_at(&mut app, MouseKind::Drag(MouseButton::Left), 12, 0);
        mouse_at(&mut app, MouseKind::Drag(MouseButton::Left), 14, 0);
        mouse_at(&mut app, MouseKind::Release(MouseButton::Left), 14, 0);
        assert_eq!(tab_bar(&app), " 1 b.txt  2 a.txt  +");
        assert_eq!(app.tab, 1, "a.txt stays on screen");

        // A double click renames it; no name names it for its file again.
        left_click(&mut app, 12, 0);
        left_click(&mut app, 12, 0);
        type_text(&mut app, "notes");
        assert!(cells(&app, 9, 0..80).starts_with(" Rename tab: notes"));
        key(&mut app, KeyCode::Enter);
        assert_eq!(tab_bar(&app), " 1 b.txt  2 notes  +");
        assert!(!app.ed().is_modified(), "the prompt took the keys");
        app.run(Command::RenameTab, false);
        for _ in 0.."notes".len() {
            key(&mut app, KeyCode::Backspace);
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(tab_bar(&app), " 1 b.txt  2 a.txt  +");
    }

    // --- context menu and file commands ----------------------------------------

    /// An app over a screen tall enough for the tree's context menu.
    fn tall_app(root: &Path, file: Option<&str>) -> App {
        let workspace = Workspace::new([root.to_path_buf()]).unwrap();
        App::new(workspace, file.map(|f| root.join(f)), 80, 24).unwrap()
    }

    fn tall_screen(app: &App) -> String {
        let frame = OwnedBuffer::new(80, 24, false, WidthMethod::Unicode, "test").unwrap();
        app.draw(&frame);
        frame.to_text(true)
    }

    fn mouse_at(app: &mut App, kind: MouseKind, x: u32, y: u32) {
        let mouse = Mouse {
            kind,
            x,
            y,
            mods: Mods::NONE,
        };
        app.handle_mouse(mouse, Instant::now());
    }

    fn right_click(app: &mut App, x: u32, y: u32) {
        mouse_at(app, MouseKind::Press(MouseButton::Right), x, y);
        mouse_at(app, MouseKind::Release(MouseButton::Right), x, y);
    }

    /// Clicks the menu item labeled `label`.
    fn click_item(app: &mut App, label: &str) {
        let text = tall_screen(app);
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(&format!("│ {label}")))
            .unwrap_or_else(|| panic!("no {label} in\n{text}"));
        let x = line.chars().position(|c| c == '│').unwrap() as u32;
        // Past the tree's divider, if the menu is right of it.
        let x = line
            .chars()
            .skip(x as usize + 1)
            .position(|c| c == '│')
            .map_or(x, |_| x);
        left_click(app, x + 2, y as u32);
    }

    #[test]
    fn right_clicking_a_file_in_the_tree_renames_it_from_the_menu() {
        let _serial = crate::test_serial();
        let root = fixture("menu-rename", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = tall_app(&root, Some("a.txt"));
        type_text(&mut app, "1");

        // The root is on row 0, a.txt on row 1.
        right_click(&mut app, 4, 1);
        assert!(app.menu.is_some(), "the release left it open");
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(app.tree.selected().unwrap().path, root.join("a.txt"));
        let text = tall_screen(&app);
        assert!(text.contains("Rename…"), "{text}");
        assert!(!text.contains("Open in Terminal"), "that's for folders");

        click_item(&mut app, "Rename…");
        assert!(app.menu.is_none());
        assert_eq!(
            app.dialog.as_ref().map(|d| d.purpose()),
            Some(Purpose::Move)
        );
        type_text(&mut app, "c.txt");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "{}", tall_screen(&app));
        assert!(!root.join("a.txt").exists());
        assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), "alpha");
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("c.txt").as_path()),
            "the open file went with it"
        );
        assert!(tall_screen(&app).contains("1alpha"), "keeping its edit");
        assert_eq!(app.tree.selected().unwrap().path, root.join("c.txt"));
    }

    #[test]
    fn renaming_to_an_existing_name_is_refused() {
        let _serial = crate::test_serial();
        let root = fixture("menu-exists", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = tall_app(&root, None);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::F(2));
        type_text(&mut app, "b.txt");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_some());
        assert!(tall_screen(&app).contains("b.txt already exists."));
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "beta");

        // Enter on the name it has leaves it be.
        for _ in 0..5 {
            key(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "a.txt");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none());
        assert!(root.join("a.txt").exists());
    }

    #[test]
    fn moving_a_folder_takes_its_open_files_and_expanded_folders() {
        let _serial = crate::test_serial();
        let root = fixture(
            "menu-move",
            &[("src/deep/x.rs", "fn x() {}"), ("dest/keep.txt", "")],
        );
        let mut app = tall_app(&root, Some("src/deep/x.rs"));
        app.focus = Focus::Tree;
        app.tree.reveal(&root.join("src"));
        app.run(Command::TreeRename, false);
        // Replace the whole path.
        app.paste(&format!("{}/dest/src", root.display()));
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "{}", tall_screen(&app));
        let moved = root.join("dest/src/deep/x.rs");
        assert!(moved.is_file());
        assert_eq!(app.ed().path().as_deref(), Some(moved.as_path()));
        assert!(tall_screen(&app).contains("x.rs"), "still expanded to it");
    }

    #[test]
    fn a_folder_cant_move_into_itself() {
        let _serial = crate::test_serial();
        let root = fixture("menu-into", &[("src/a.rs", "")]);
        let mut app = tall_app(&root, None);
        key(&mut app, KeyCode::Down);
        app.run(Command::TreeRename, false);
        app.paste(&format!("{}/src/inner/src", root.display()));
        key(&mut app, KeyCode::Enter);
        assert!(tall_screen(&app).contains("Can't move a folder into itself."));
        assert!(root.join("src/a.rs").is_file());
    }

    #[test]
    fn duplicating_names_copies_as_the_finder_does() {
        let _serial = crate::test_serial();
        let root = fixture(
            "menu-duplicate",
            &[
                ("a.txt", "alpha"),
                ("a copy.txt", ""),
                ("dir/b.txt", "beta"),
            ],
        );
        let mut app = tall_app(&root, None);
        // dir, then a copy.txt, then a.txt.
        for _ in 0..3 {
            key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.tree.selected().unwrap().path, root.join("a.txt"));
        app.run(Command::TreeDuplicate, false);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            fs::read_to_string(root.join("a copy 2.txt")).unwrap(),
            "alpha"
        );

        app.tree.reveal(&root.join("dir"));
        app.run(Command::TreeDuplicate, false);
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            fs::read_to_string(root.join("dir copy/b.txt")).unwrap(),
            "beta"
        );
        assert_eq!(app.tree.selected().unwrap().path, root.join("dir copy"));
    }

    #[test]
    fn delete_asks_before_trashing_but_the_menu_doesnt() {
        let _serial = crate::test_serial();
        let root = fixture("menu-trash", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let mut app = tall_app(&root, Some("a.txt"));
        app.focus = Focus::Tree;
        app.tree.reveal(&root.join("a.txt"));
        key(&mut app, KeyCode::Delete);
        assert!(root.join("a.txt").exists());
        assert!(tall_screen(&app).contains("Move a.txt to the Trash?"));
        key(&mut app, KeyCode::Enter);
        assert!(!root.join("a.txt").exists());
        assert!(!app.ed_is_shown(), "its unedited file closed");

        // b.txt is on row 1 now.
        right_click(&mut app, 4, 1);
        click_item(&mut app, "Move to Trash");
        assert!(!root.join("b.txt").exists());
    }

    #[test]
    fn roots_arent_renamed_or_trashed() {
        let _serial = crate::test_serial();
        let root = fixture("menu-root", &[("a.txt", "")]);
        let mut app = tall_app(&root, None);
        right_click(&mut app, 2, 0);
        let text = tall_screen(&app);
        assert!(text.contains("Open in Terminal"), "{text}");
        assert!(!text.contains("Rename"), "{text}");
        key(&mut app, KeyCode::Esc);
        assert!(app.menu.is_none());
        key(&mut app, KeyCode::Delete);
        key(&mut app, KeyCode::Delete);
        assert!(root.exists());
    }

    #[test]
    fn new_folder_from_the_menu_goes_in_the_folder_clicked() {
        let _serial = crate::test_serial();
        let root = fixture("menu-folder", &[("dir/a.txt", "")]);
        let mut app = tall_app(&root, None);
        // Below the entries, the menu is the workspace folder's.
        right_click(&mut app, 4, 15);
        click_item(&mut app, "New Folder…");
        assert_eq!(
            app.dialog.as_ref().map(|d| d.purpose()),
            Some(Purpose::CreateFolder)
        );
        type_text(&mut app, "made");
        key(&mut app, KeyCode::Enter);
        assert!(root.join("made").is_dir());
        assert_eq!(app.tree.selected().unwrap().path, root.join("made"));
    }

    #[test]
    fn shift_f10_opens_the_menu_from_the_keyboard() {
        let _serial = crate::test_serial();
        let root = fixture("menu-keys", &[("a.txt", "alpha")]);
        let mut app = tall_app(&root, None);
        key(&mut app, KeyCode::Down);
        let shift = Mods {
            shift: true,
            ..Mods::NONE
        };
        app.handle_key(Key::new(KeyCode::F(10), shift));
        assert!(app.menu.is_some());
        type_text(&mut app, "x");
        assert!(app.menu.is_some(), "typing does nothing");
        // Open is first.
        key(&mut app, KeyCode::Enter);
        assert!(app.menu.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert!(tall_screen(&app).contains("alpha"));
    }

    #[test]
    fn a_left_click_outside_closes_the_menu_and_a_right_click_moves_it() {
        let _serial = crate::test_serial();
        let root = fixture("menu-outside", &[("a.txt", ""), ("b.txt", "")]);
        let mut app = tall_app(&root, None);
        right_click(&mut app, 4, 1);
        // Left of the menu, which is over the row.
        right_click(&mut app, 1, 2);
        assert!(app.menu.is_some());
        assert_eq!(app.tree.selected().unwrap().path, root.join("b.txt"));
        left_click(&mut app, 60, 20);
        assert!(app.menu.is_none());
        assert_eq!(app.focus, Focus::Tree, "that click only closed it");
    }

    #[test]
    fn copy_relative_path_copies_it() {
        let _serial = crate::test_serial();
        let root = fixture("menu-copy", &[("dir/a.txt", "")]);
        let mut app = tall_app(&root, Some("dir/a.txt"));
        // From the editor, it's the file on screen's.
        match app.run(Command::TreeCopyRelativePath, false) {
            AppAction::Copy(text) => assert_eq!(text, "dir/a.txt"),
            _ => panic!("nothing copied"),
        }
        assert_eq!(app.clipboard.as_deref(), Some("dir/a.txt"));
    }

    #[test]
    fn the_menu_survives_tiny_screens() {
        let _serial = crate::test_serial();
        let root = fixture("menu-tiny", &[("a.txt", "")]);
        let mut app = tall_app(&root, None);
        right_click(&mut app, 4, 1);
        for (width, height) in [(30, 5), (3, 2), (1, 1), (0, 0), (80, 24)] {
            app.resize(width, height);
            let frame = OwnedBuffer::new(
                width.max(1),
                height.max(1),
                false,
                WidthMethod::Unicode,
                "t",
            )
            .unwrap();
            app.draw(&frame);
            mouse_at(
                &mut app,
                MouseKind::Drag(MouseButton::Right),
                width / 2,
                height / 2,
            );
        }
        assert!(app.menu.is_some());
    }

    #[test]
    fn folders_are_added_to_the_workspace_and_removed() {
        let _serial = crate::test_serial();
        let dir = fixture("add-folder", &[("one/a.txt", "a"), ("two/b.txt", "b")]);
        let (one, two) = (dir.join("one"), dir.join("two"));
        let mut app = tall_app(&one, Some("a.txt"));

        // It lists the folders beside the one being worked in.
        app.run(Command::AddFolder, false);
        assert_eq!(
            app.dialog.as_ref().map(|d| d.purpose()),
            Some(Purpose::AddFolder)
        );
        type_text(&mut app, "tw");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_some(), "Enter went into two");
        key(&mut app, KeyCode::Enter);
        assert!(app.dialog.is_none(), "{}", tall_screen(&app));
        assert_eq!(app.workspace.roots(), [one.clone(), two.clone()]);
        assert_eq!(app.tree.selected().unwrap().path, two);
        app.files.wait();
        let files: Vec<String> = app.files.files().iter().map(|f| f.text.clone()).collect();
        assert_eq!(files, ["one/a.txt", "two/b.txt"]);

        // Terminals start in the folder being worked in.
        assert_eq!(app.current_root(), two, "the tree's selection");
        app.focus = Focus::Editor;
        assert_eq!(app.current_root(), one, "the file on screen");

        app.run(Command::AddFolder, false);
        type_text(&mut app, "two/");
        key(&mut app, KeyCode::Enter);
        assert!(tall_screen(&app).contains("two is already in the workspace."));
        key(&mut app, KeyCode::Esc);

        // The menu of a root has it, when there's another.
        app.tree.reveal(&two);
        right_click(&mut app, 4, 2);
        assert_eq!(app.tree.selected().unwrap().path, two);
        click_item(&mut app, "Remove Folder from Workspace");
        assert_eq!(app.workspace.roots(), std::slice::from_ref(&one));
        assert_eq!(
            app.ed().path().as_deref(),
            Some(one.join("a.txt").as_path())
        );
        right_click(&mut app, 4, 0);
        let text = tall_screen(&app);
        assert!(text.contains("Add Folder to Workspace…"), "{text}");
        assert!(!text.contains("Remove Folder"), "{text}");
    }

    #[test]
    fn folders_with_workspace_folders_in_them_stay_put() {
        let _serial = crate::test_serial();
        let root = fixture("nested-roots", &[("packages/pkg/x.txt", "x")]);
        let pkg = root.join("packages/pkg");
        let workspace = Workspace::new([root.clone(), pkg.clone()]).unwrap();
        let mut app = App::new(workspace, None, 80, 24).unwrap();
        app.tree.reveal(&root.join("packages"));
        app.focus = Focus::Tree;
        for command in [Command::TreeRename, Command::TreeTrash] {
            app.run(command, false);
            assert!(app.dialog.is_none() && app.alert.is_none());
            let text = tall_screen(&app);
            assert!(text.contains("contains workspace folder pkg"), "{text}");
        }
        assert!(pkg.exists());

        // Its file belongs to it, and shows there.
        assert!(app.open(&pkg.join("x.txt"), false));
        assert_eq!(app.workspace.display_path(&pkg.join("x.txt")), "pkg/x.txt");
        assert_eq!(app.current_root(), pkg);
    }

    /// Where the active editor's cursor is: `Ln 3, Col 5`.
    fn cursor_at(app: &App) -> String {
        let info = status_of(app, app.tab().active);
        info.split("  ").next().unwrap().to_string()
    }

    const CTRL_SHIFT: Mods = Mods {
        shift: true,
        ..Mods::CTRL
    };

    #[test]
    fn the_picker_goes_to_lines_and_symbols() {
        let _serial = crate::test_serial();
        let root = fixture(
            "go-to",
            &[
                (
                    "a.rs",
                    "fn alpha() {}\n\nstruct Beta;\n\nimpl Beta {\n    fn gamma(&self) {}\n}\n",
                ),
                ("src/b.py", "x = 1\n\ndef delta():\n    pass\n"),
            ],
        );
        let mut app = app(&root, Some("a.rs"));

        // A line, and a column, after `:`.
        ctrl(&mut app, 'l');
        assert_eq!(app.picker.as_ref().unwrap().mode(), Mode::Line);
        type_text(&mut app, "5:3");
        assert!(
            screen(&app).contains("Go to line 5, column 3"),
            "{}",
            screen(&app)
        );
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_none());
        assert_eq!(cursor_at(&app), "Ln 5, Col 3");
        ctrl(&mut app, 'l');
        type_text(&mut app, "x");
        assert!(screen(&app).contains("Not a line number"));
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_some());
        key(&mut app, KeyCode::Esc);

        // The file's symbols after `@`, in order, with what they're in.
        ctrl(&mut app, 'r');
        assert_eq!(app.picker.as_ref().unwrap().mode(), Mode::Symbols);
        let text = screen(&app);
        let order: Vec<usize> = ["alpha", "Beta ", "gamma Beta"]
            .iter()
            .map(|name| text.find(name).unwrap_or_else(|| panic!("{name}:\n{text}")))
            .collect();
        assert!(order.is_sorted(), "{text}");
        type_text(&mut app, "gam");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.ed().selected_text().as_deref(), Some("gamma"));
        assert_eq!(cursor_at(&app), "Ln 6, Col 13 (5 sel)");

        // A file's name can end with a line to go to.
        go_to_file(&mut app);
        type_text(&mut app, "b.py:3");
        assert!(screen(&app).contains("src/b.py"), "{}", screen(&app));
        key(&mut app, KeyCode::Enter);
        assert!(app.ed().path().unwrap().ends_with("src/b.py"));
        assert_eq!(cursor_at(&app), "Ln 3, Col 1");

        // The workspace's symbols after `#`, once they're indexed.
        app.handle_key(Key::new(KeyCode::Char('r'), CTRL_SHIFT));
        assert_eq!(app.picker.as_ref().unwrap().mode(), Mode::WorkspaceSymbols);
        wait_until(&mut app, "the symbols", |app| !app.symbols.indexing());
        type_text(&mut app, "alpha");
        assert!(screen(&app).contains("alpha a.rs:1"), "{}", screen(&app));
        key(&mut app, KeyCode::Enter);
        assert!(app.ed().path().unwrap().ends_with("a.rs"));
        assert_eq!(app.ed().selected_text().as_deref(), Some("alpha"));

        // Plain text has none.
        app.editor_mut().unwrap().document().set_language(None);
        ctrl(&mut app, 'r');
        assert!(screen(&app).contains("No symbols in this file"));
    }

    #[test]
    fn the_command_line_opens_a_file_at_a_line() {
        let _serial = crate::test_serial();
        let root = fixture("command-line-line", &[("a.txt", &numbered(20))]);
        let mut app = app(&root, Some("a.txt"));
        app.go_to(Position::printed(12, Some(3)));
        assert_eq!(cursor_at(&app), "Ln 12, Col 3");
        // Past the end, the last line; past a line's end, its end.
        app.go_to(Position::printed(99, Some(99)));
        assert_eq!(cursor_at(&app), "Ln 21, Col 1");
        app.go_to(Position::printed(2, Some(99)));
        assert_eq!(cursor_at(&app), "Ln 2, Col 7");
    }

    #[test]
    fn ctrl_click_in_a_terminal_opens_the_file_beside_it() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-links", &[("src/main.rs", &numbered(20))]);
        let mut app = app(&root, None);
        app.handle_key(Key::new(KeyCode::Char('n'), CTRL_SHIFT));
        assert!(app.active_terminal().is_some());
        // Printed so the command typed doesn't read the same.
        type_text(&mut app, "printf '%s:%s\\n' src/main.rs 12:3");
        key(&mut app, KeyCode::Enter);
        let printed = "│src/main.rs:12:3";
        wait_until(&mut app, "the path printed", |app| {
            screen(app).contains(printed)
        });
        // Where the path is on screen, a few characters in.
        let find = |app: &App| {
            let text = screen(app);
            let (y, line) = text
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(printed))
                .unwrap();
            let x = line[..line.find(printed).unwrap()].chars().count() as u32 + 5;
            (x, y as u32)
        };
        let ctrl_click = |app: &mut App, (x, y): (u32, u32)| {
            for kind in [
                MouseKind::Press(MouseButton::Left),
                MouseKind::Release(MouseButton::Left),
            ] {
                let mods = Mods::CTRL;
                app.handle_mouse(Mouse { kind, x, y, mods }, Instant::now());
            }
        };
        let terminal_panel = app.tab().active;

        // A plain click selects, as before.
        let (x, y) = find(&app);
        left_click(&mut app, x, y);
        assert_eq!(app.tab().panels.len(), 1);

        // It opens to the right, leaving the terminal on screen.
        ctrl_click(&mut app, (x, y));
        assert_eq!(app.tab().panels.len(), 2);
        assert_ne!(app.tab().active, terminal_panel);
        assert!(app.ed().path().unwrap().ends_with("src/main.rs"));
        assert_eq!(cursor_at(&app), "Ln 12, Col 3");
        let terminal_shown = app
            .tab()
            .panels
            .iter()
            .any(|panel| panel.id == terminal_panel && panel.terminal().is_some());
        assert!(terminal_shown);

        // Again, into the same panel rather than another.
        app.activate(terminal_panel);
        let at = find(&app);
        ctrl_click(&mut app, at);
        assert_eq!(app.tab().panels.len(), 2);
        assert_ne!(app.tab().active, terminal_panel);
        assert!(app.ed().path().unwrap().ends_with("src/main.rs"));

        // Where there's nothing to open, nothing happens.
        app.activate(terminal_panel);
        let (x, _) = find(&app);
        ctrl_click(&mut app, (x, 0));
        assert_eq!(app.tab().active, terminal_panel);
    }

    /// A recovery folder of its own for a test, empty.
    fn recovery_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-recovery-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unsaved_changes_are_copied_until_saved_or_quit() {
        let _serial = crate::test_serial();
        let root = fixture("recovery-copies", &[("a.txt", "alpha")]);
        let dir = recovery_dir("copies");
        let copies = |dir: &Path| -> Vec<String> {
            let mut texts: Vec<String> = fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|e| e == "txt"))
                .map(|entry| fs::read_to_string(entry.path()).unwrap())
                .collect();
            texts.sort();
            texts
        };
        let mut app = app(&root, Some("a.txt"));
        app.recovery = Recovery::new(Some(dir.clone()));
        app.poll();
        assert!(copies(&dir).is_empty());

        type_text(&mut app, "x");
        app.poll();
        let copied = copies(&dir);
        assert_eq!(copied.len(), 1);
        assert!(copied[0].contains(&format!("path {}", root.join("a.txt").display())));
        assert!(copied[0].ends_with("\n\nxalpha"), "{}", copied[0]);

        // Saving removes it.
        ctrl(&mut app, 's');
        app.poll();
        assert!(copies(&dir).is_empty());

        // Dropped without quitting, as on a crash, the latest text is
        // copied, however recently it was.
        type_text(&mut app, "y");
        app.poll();
        type_text(&mut app, "z");
        drop(app);
        let copied = copies(&dir);
        assert_eq!(copied.len(), 1);
        assert!(copied[0].ends_with("\n\nxyzalpha"), "{}", copied[0]);

        // Quitting lets them go.
        let dir = recovery_dir("quit");
        let mut app = self::app(&root, Some("a.txt"));
        app.recovery = Recovery::new(Some(dir.clone()));
        type_text(&mut app, "q");
        app.poll();
        assert_eq!(copies(&dir).len(), 1);
        app.discard_recovery();
        drop(app);
        assert!(copies(&dir).is_empty());
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "the lock is gone too"
        );
    }

    #[test]
    fn copies_a_cue_that_is_gone_left_are_offered_back() {
        let _serial = crate::test_serial();
        let root = fixture("recovery-offer", &[("a.txt", "alpha"), ("b.txt", "beta")]);
        let dir = recovery_dir("offer");
        fs::create_dir_all(&dir).unwrap();
        // From a process that's gone: no one holds its lock.
        let copy = |id: u32, header: String, text: &str| {
            let file = dir.join(format!("999999-{id}.txt"));
            fs::write(file, format!("cue recovery 1\n{header}\n{text}")).unwrap();
        };
        copy(
            1,
            format!("path {}\n", root.join("a.txt").display()),
            "alpha, edited",
        );
        copy(2, format!("untitled\nroot {}\n", root.display()), "notes");
        // The same as the file now: nothing to recover.
        copy(
            3,
            format!("path {}\n", root.join("b.txt").display()),
            "beta",
        );
        // Elsewhere: kept for when cue opens there.
        copy(4, "path /elsewhere/c.txt\n".to_string(), "gamma");
        copy(5, "untitled\nroot /elsewhere\n".to_string(), "delta");
        fs::write(dir.join("999999.lock"), "").unwrap();

        let mut app = app(&root, None);
        app.recovery = Recovery::new(Some(dir.clone()));
        app.offer_recovery(false);
        let text = screen(&app);
        assert!(text.contains("Recover unsaved changes?"), "{text}");
        assert!(!dir.join("999999-3.txt").exists());

        key(&mut app, KeyCode::Char('r'));
        assert!(app.alert.is_none());
        let a = app.find_document(&root.join("a.txt")).unwrap();
        assert!(a.is_modified());
        assert_eq!(a.text(), "alpha, edited");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha");
        let untitled = app
            .documents
            .iter()
            .find(|doc| doc.path().is_none() && doc.text() == "notes");
        assert!(untitled.is_some_and(|doc| doc.is_modified()));
        assert!(app.ed().path().is_some_and(|path| path.ends_with("a.txt")));
        assert!(screen(&app).contains("Recovered changes to 2 files"));
        // Undoing goes back to the file as it is.
        ctrl(&mut app, 'z');
        assert_eq!(a.text(), "alpha");

        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["999999-4.txt", "999999-5.txt", "999999.lock"]);

        // Asked for again, there's nothing more here.
        app.run(Command::RecoverUnsaved, false);
        assert!(app.alert.is_none());
        assert!(screen(&app).contains("No unsaved changes to recover"));
    }

    #[test]
    fn discarding_recovered_changes_removes_them() {
        let _serial = crate::test_serial();
        let root = fixture("recovery-discard", &[("a.txt", "alpha")]);
        let dir = recovery_dir("discard");
        fs::create_dir_all(&dir).unwrap();
        let header = format!("cue recovery 1\npath {}\n\n", root.join("a.txt").display());
        fs::write(dir.join("999998-1.txt"), header + "edited").unwrap();
        let mut app = app(&root, Some("a.txt"));
        app.recovery = Recovery::new(Some(dir.clone()));
        app.offer_recovery(false);
        key(&mut app, KeyCode::Char('d'));
        assert!(app.alert.is_none());
        assert!(!app.ed().is_modified());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn open_settings_makes_the_file_from_the_template() {
        let _serial = crate::test_serial();
        let root = fixture("settings", &[("a.txt", "")]);
        let path = root.join("config/cue/config.toml");
        let mut first = app(&root, Some("a.txt"));
        first.settings = Some(path.clone());
        first.run(Command::OpenSettings, false);
        assert_eq!(fs::read_to_string(&path).unwrap(), config::TEMPLATE);
        assert_eq!(first.ed().path(), Some(path.clone()));
        assert_eq!(first.focus, Focus::Editor);

        // One already there is opened as it is.
        fs::write(&path, "[editor]\nwrap = false\n").unwrap();
        let mut app = app(&root, Some("a.txt"));
        app.settings = Some(path.clone());
        app.run(Command::OpenSettings, false);
        assert_eq!(app.ed().text(), "[editor]\nwrap = false\n");
    }

    #[test]
    fn select_theme_shows_each_theme_and_saves_the_one_picked() {
        let _serial = crate::test_serial();
        let root = fixture("select-theme", &[("a.txt", "")]);
        let path = root.join("config/cue/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[editor]\nwrap = false\n").unwrap();
        let mut app = app(&root, Some("a.txt"));
        app.settings = Some(path.clone());
        app.update_theme();
        assert_eq!(theme::colors().name, "Terminal");

        // Moving through the list shows each theme, on the theme in use first.
        app.run(Command::SelectTheme, false);
        assert!(!app.update_theme());
        key(&mut app, KeyCode::Down);
        assert!(app.update_theme());
        assert_eq!(theme::colors().name, "Cue Dark");
        assert!(!theme::colors().bg.is_terminal_default());
        // Escape goes back.
        key(&mut app, KeyCode::Esc);
        assert!(app.update_theme());
        assert_eq!(theme::colors().name, "Terminal");

        // Picked, it's saved with the rest of the settings.
        app.run(Command::SelectTheme, false);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        app.update_theme();
        assert_eq!(theme::colors().name, "Cue Dark");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[editor]\nwrap = false\n\n[ui]\ntheme = \"Cue Dark\"\n"
        );
        assert!(!config::get().wrap);
        assert!(screen(&app).contains("Using Cue Dark."));
    }

    #[test]
    fn settings_problems_show_in_the_status_bar() {
        let _serial = crate::test_serial();
        let root = fixture("settings-warnings", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        app.warn_about_config(&[]);
        assert!(!screen(&app).contains("Settings:"));
        let warnings = [
            "Unknown setting: editor.tabwidth".to_string(),
            "x".to_string(),
        ];
        app.warn_about_config(&warnings);
        let text = screen(&app);
        assert!(
            text.contains("Settings: Unknown setting: editor.tabwidth (and 1 more"),
            "{text}"
        );
    }

    #[test]
    fn saving_settings_applies_them() {
        let _serial = crate::test_serial();
        let root = fixture("settings-save", &[("a.txt", "")]);
        let path = root.join("config/cue/config.toml");
        let mut app = app(&root, Some("a.txt"));
        app.settings = Some(path.clone());
        app.run(Command::OpenSettings, false);
        assert!(!screen(&app).contains("Settings applied."));

        ctrl(&mut app, 'a');
        app.paste(
            "[editor]\ntab_width = 2\nwrap = false\n[ui]\ntree_width = 20\n\
             [keys]\n\"ctrl+shift+p\" = \"app:command-palette\"\n",
        );
        app.after_input();
        // Not until it's saved.
        assert_eq!(config::get().tab_width, 4);
        ctrl(&mut app, 's');
        assert_eq!(config::get().tab_width, 2);
        let text = screen(&app);
        assert!(text.contains("Settings applied."), "{text}");
        app.active_panel_mut().clear_message();
        let text = screen(&app);
        assert!(text.contains("nowrap"), "{text}");
        assert_eq!(app.visible_tree_width(), 20);
        let palette = Key::new(
            KeyCode::Char('p'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        assert_eq!(
            app.keymap.lookup(palette, Context::Editor),
            Some((Command::Palette, false))
        );

        // Saved again unchanged, nothing's read.
        ctrl(&mut app, 's');
        assert!(!screen(&app).contains("Settings applied."));

        // Mistakes are told of, and what's right still applies.
        ctrl(&mut app, 'a');
        app.paste("[editor]\ntab_width = 3\nwrapping = false\n");
        ctrl(&mut app, 's');
        assert_eq!(config::get().tab_width, 3);
        assert_eq!(config::get().tree_width, 30);
        let text = screen(&app);
        assert!(text.contains("Unknown setting: editor.wrapping"), "{text}");
    }

    #[test]
    fn reload_settings_reads_the_file() {
        let _serial = crate::test_serial();
        let root = fixture("settings-reload", &[("a.txt", ""), ("b.txt", "")]);
        let path = root.join("config.toml");
        let mut app = app(&root, Some("a.txt"));
        app.settings = Some(path.clone());
        assert!(screen(&app).contains("b.txt"));

        fs::write(&path, "[files]\nexclude = [\"b.*\"]\n[ui]\ntree = false\n").unwrap();
        app.run(Command::ReloadSettings, false);
        assert_eq!(app.visible_tree_width(), 0);
        ctrl(&mut app, 'b');
        let text = screen(&app);
        assert!(text.contains("a.txt") && !text.contains("b.txt"), "{text}");

        // Gone, it's the defaults again; the tree stays as it was put.
        fs::remove_file(&path).unwrap();
        app.run(Command::ReloadSettings, false);
        assert_eq!(*config::get(), Config::default());
        assert!(app.visible_tree_width() > 0);
        assert!(screen(&app).contains("b.txt"));
    }

    // --- sessions -------------------------------------------------------------------

    fn sessions_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-sessions-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A new app going back to the session `app` was, as a cue started
    /// later does, with terminals `adopted`.
    fn reopen(app: App, adopted: Vec<Adopted>) -> App {
        let dir = app.session().expect("a session").dir().to_path_buf();
        let sessions = app.sessions.clone();
        drop(app);
        reopen_dir(&dir, sessions, adopted)
    }

    fn reopen_dir(dir: &Path, sessions: Option<PathBuf>, adopted: Vec<Adopted>) -> App {
        let session = Session::open(dir).unwrap();
        let state = session::read(dir).unwrap();
        let workspace = Workspace::new(state.roots.clone()).unwrap();
        let mut app = App::new(workspace, None, 80, 10).unwrap();
        app.sessions = sessions;
        app.restore_session(session, state, adopted).unwrap();
        app
    }

    fn snapshot_text(terminal: &Rc<RefCell<Terminal>>) -> String {
        let (screen, _) = terminal.borrow_mut().screen(false).unwrap();
        String::from_utf8_lossy(&screen).into_owned()
    }

    #[test]
    fn sessions_keep_tabs_files_places_and_unsaved_changes() {
        let _serial = crate::test_serial();
        let root = fixture("session", &[("a.txt", &numbered(40)), ("b.txt", "beta")]);
        let mut app = app(&root, Some("a.txt"));
        app.sessions = Some(sessions_dir("keep"));
        app.go_to(Position {
            line: 19,
            column: Some(2),
        });
        type_text(&mut app, "X");
        ctrl(&mut app, '\\');
        assert!(app.open(&root.join("b.txt"), false));
        let right = app.tab().active;
        ctrl(&mut app, 't');
        ctrl(&mut app, 'n');
        type_text(&mut app, "draft");
        app.tab_mut().name = Some("notes".into());

        assert!(app.palette_has(Command::KeepSession));
        app.run(Command::KeepSession, false);
        assert!(screen(&app).contains("Session saved."), "{}", screen(&app));
        assert!(!app.palette_has(Command::KeepSession));
        assert!(app.palette_has(Command::EndSession));
        let dir = app.session().unwrap().dir().to_path_buf();
        assert!(dir.join("session.toml").is_file());
        app.save_session(true);

        let mut app = reopen(app, Vec::new());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tab, 1);
        assert_eq!(app.tab().name.as_deref(), Some("notes"));
        assert_eq!(app.ed().text(), "draft");
        assert!(app.ed().is_modified());
        assert!(screen(&app).contains("Untitled-1"), "{}", screen(&app));

        app.switch_tab(0);
        assert_eq!(app.tab().panels.len(), 2);
        assert_eq!(app.tab().active, right);
        assert!(app.ed().path().is_some_and(|path| path.ends_with("b.txt")));
        app.focus_panel(Direction::Left);
        let editor = app.ed();
        assert!(editor.path().is_some_and(|path| path.ends_with("a.txt")));
        assert!(editor.is_modified());
        assert!(editor.text().contains("liXne 20"), "{}", editor.text());
        assert_eq!(editor.place().0, 19, "the cursor's row");
        // Going back still works from where it was.
        assert_eq!(app.session().map(Session::dir), Some(dir.as_path()));

        // Nothing running: quitting saves it and exits, keeping it.
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Quit));
        assert!(dir.join("session.toml").is_file());
        let state = session::read(&dir).unwrap();
        assert_eq!(
            state
                .documents
                .iter()
                .filter(|d| d.unsaved.is_some())
                .count(),
            2
        );
        drop(app);
        assert!(!session::is_live(&dir));
    }

    #[test]
    fn the_status_bar_shows_a_session_and_its_badge_detaches_or_ends_it() {
        let _serial = crate::test_serial();
        let root = fixture("session-badge", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        app.sessions = Some(sessions_dir("badge"));
        let status_line = |app: &App| screen(app).lines().last().unwrap().trim_end().to_string();
        assert!(
            !status_line(&app).ends_with(" session"),
            "{}",
            status_line(&app)
        );
        left_click(&mut app, 78, 9);
        assert!(app.menu.is_none());

        app.run(Command::KeepSession, false);
        key(&mut app, KeyCode::Right);
        let line = status_line(&app);
        assert!(line.ends_with(" session"), "{line}");
        // Where a menu item is on screen, by its label.
        let find = |app: &App, label: &str| {
            screen(app).lines().enumerate().find_map(|(y, line)| {
                let x = line.find(label)?;
                Some((line[..x].chars().count() as u32, y as u32))
            })
        };
        let click = |app: &mut App, (x, y): (u32, u32)| {
            let mut action = AppAction::Continue;
            for kind in [
                MouseKind::Press(MouseButton::Left),
                MouseKind::Release(MouseButton::Left),
            ] {
                let mouse = Mouse {
                    kind,
                    x,
                    y,
                    mods: Mods::NONE,
                };
                // The press picks; the release does nothing more.
                match app.handle_mouse(mouse, Instant::now()) {
                    AppAction::Continue => {}
                    picked => action = picked,
                }
            }
            action
        };
        click(&mut app, (78, 9));
        assert!(app.menu.is_some());
        let detach = find(&app, "Detach Session").expect("the menu");
        assert!(find(&app, "End Session").is_some(), "{}", screen(&app));
        assert!(matches!(click(&mut app, detach), AppAction::Detach));
        assert!(app.menu.is_none());

        click(&mut app, (78, 9));
        let end = find(&app, "End Session").expect("the menu");
        let dir = app.session().unwrap().dir().to_path_buf();
        assert!(matches!(click(&mut app, end), AppAction::Quit));
        assert!(app.session().is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn quitting_with_unsaved_changes_offers_to_keep_a_session() {
        let _serial = crate::test_serial();
        let root = fixture("session-quit", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let sessions = sessions_dir("quit");
        app.sessions = Some(sessions.clone());
        app.focus = Focus::Editor;
        type_text(&mut app, "x");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        let text = screen(&app);
        assert!(text.contains("Keep Session"), "{text}");
        // Selected first: Enter keeps everything.
        assert!(matches!(key(&mut app, KeyCode::Enter), AppAction::Quit));
        let listed = session::list(&sessions);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].state.documents[0].unsaved.as_deref(),
            Some("1.txt")
        );
        drop(app);

        // Ending it asks about the changes it keeps, then lets it go.
        let mut app = reopen_dir(&listed[0].dir, Some(sessions.clone()), Vec::new());
        assert_eq!(app.ed().text(), "xalpha");
        assert!(matches!(
            app.run(Command::EndSession, false),
            AppAction::Continue
        ));
        assert!(screen(&app).contains("End session?"), "{}", screen(&app));
        assert!(matches!(key(&mut app, KeyCode::Char('n')), AppAction::Quit));
        assert!(session::list(&sessions).is_empty());
        assert!(!listed[0].dir.exists());
    }

    #[test]
    fn session_terminals_are_adopted_running_or_start_below_their_screen() {
        let _serial = crate::test_serial();
        let root = fixture("session-terminals", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let sessions = sessions_dir("terminals");
        app.sessions = Some(sessions.clone());
        app.run(Command::NewTerminal, false);
        type_text(&mut app, "echo mark-$((6*7))");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the shell's output", |app| {
            snapshot_text(&app.terminals[0]).contains("mark-42")
        });
        app.run(Command::RenameTerminal, false);
        type_text(&mut app, "server");
        key(&mut app, KeyCode::Enter);

        // Taken over by another process: the shell runs on.
        let (dir, ephemeral, adopted) = app.hand_over().unwrap();
        assert!(ephemeral, "made a session for the while");
        assert_eq!(adopted.len(), 1);
        let pid = adopted[0].pid;
        drop(app);
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "not hung up on");
        let mut app = reopen_dir(&dir, Some(sessions), adopted);
        app.forget_session();
        assert!(!dir.exists());
        assert!(app.session().is_none());
        assert_eq!(app.terminals.len(), 1);
        assert!(app.active_terminal().is_some(), "still on screen");
        assert_eq!(app.terminals[0].borrow().given_name(), Some("server"));
        type_text(&mut app, "echo again-$((1+1))");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the same shell's output", |app| {
            let text = snapshot_text(&app.terminals[0]);
            text.contains("mark-42") && text.contains("again-2")
        });

        // Kept and left: a new shell starts below what it showed.
        app.run(Command::KeepSession, false);
        app.save_session(true);
        let mut app = reopen(app, Vec::new());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "hung up on");
        assert_eq!(app.terminals.len(), 1);
        let text = snapshot_text(&app.terminals[0]);
        assert!(
            text.contains("again-2") && text.contains("restored session from"),
            "{text}"
        );
        type_text(&mut app, "echo new-$((2+3))");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "the new shell's output", |app| {
            snapshot_text(&app.terminals[0]).contains("new-5")
        });
        // With a program running, quitting goes on in the background.
        type_text(&mut app, "sleep 30");
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, "sleep to run", |app| {
            app.terminals[0].borrow().program().as_deref() == Some("sleep")
        });
        assert!(matches!(prefixed_ctrl(&mut app, 'q'), AppAction::Detach));
        app.end_session_now();
    }

    #[test]
    fn session_snapshot_failure_preserves_copy_and_blocks_leaving() {
        let _serial = crate::test_serial();
        let root = fixture("session-save-failure", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        app.sessions = Some(sessions_dir("save-failure"));
        type_text(&mut app, "x");
        assert!(app.keep_session(false));
        let dir = app.session().unwrap().dir().to_path_buf();
        let metadata = fs::read(dir.join("session.toml")).unwrap();
        // A deterministic write failure, even when tests run as root.
        fs::create_dir(dir.join("docs/1.tmp")).unwrap();
        type_text(&mut app, "y");
        assert!(!app.save_session(false));
        assert!(screen(&app).contains("Can't save session"));
        assert!(matches!(app.run(Command::Quit, false), AppAction::Continue));
        assert!(matches!(
            app.run(Command::Detach, false),
            AppAction::Continue
        ));
        assert!(app.hand_over().is_none());
        assert_eq!(
            fs::read_to_string(dir.join("docs/1.txt")).unwrap(),
            "xalpha"
        );
        assert_eq!(fs::read(dir.join("session.toml")).unwrap(), metadata);
        assert_eq!(app.ed().text(), "xyalpha");
        fs::remove_dir(dir.join("docs/1.tmp")).unwrap();
        assert!(matches!(app.run(Command::Quit, false), AppAction::Quit));
        let app = reopen(app, Vec::new());
        assert_eq!(app.ed().text(), "xyalpha");
    }

    #[test]
    fn session_metadata_failure_does_not_prune_committed_texts() {
        let _serial = crate::test_serial();
        let root = fixture("session-metadata-failure", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        app.sessions = Some(sessions_dir("metadata-failure"));
        type_text(&mut app, "x");
        assert!(app.keep_session(false));
        let dir = app.session().unwrap().dir().to_path_buf();
        let metadata = fs::read(dir.join("session.toml")).unwrap();
        fs::create_dir(dir.join("session.tmp")).unwrap();
        app.run(Command::Save, false);
        assert!(!app.save_session(false));
        assert_eq!(
            fs::read_to_string(dir.join("docs/1.txt")).unwrap(),
            "xalpha"
        );
        assert_eq!(fs::read(dir.join("session.toml")).unwrap(), metadata);
        assert!(app.hand_over().is_none());
        fs::remove_dir(dir.join("session.tmp")).unwrap();
        assert!(app.save_session(false));
        assert!(!dir.join("docs/1.txt").exists());
        assert!(session::read(&dir).unwrap().documents[0].unsaved.is_none());
    }

    #[test]
    fn session_restores_edits_when_original_path_cannot_be_read() {
        let _serial = crate::test_serial();
        for (name, text) in [("unreadable", "draft"), ("unreadable-empty", "")] {
            let root = fixture(name, &[("a.txt", "alpha")]);
            let mut app = app(&root, Some("a.txt"));
            app.sessions = Some(sessions_dir(name));
            let doc = app.documents[0].clone();
            doc.restore_text(text);
            assert!(app.keep_session(false));
            let dir = app.session().unwrap().dir().to_path_buf();
            drop(app);
            fs::remove_file(root.join("a.txt")).unwrap();
            fs::create_dir(root.join("a.txt")).unwrap();
            let mut restored = reopen_dir(&dir, None, Vec::new());
            assert_eq!(restored.ed().text(), text);
            assert!(restored.ed().is_modified());
            assert_eq!(restored.ed().path(), Some(root.join("a.txt")));
            assert!(restored.save_session(false));
            assert_eq!(fs::read_to_string(dir.join("docs/1.txt")).unwrap(), text);
            let restored = reopen(restored, Vec::new());
            assert_eq!(restored.ed().text(), text);
            assert!(restored.ed().is_modified());
        }
    }

    #[test]
    fn session_unreadable_snapshot_aborts_restore_without_pruning() {
        let _serial = crate::test_serial();
        let root = fixture("session-unreadable-copy", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        app.sessions = Some(sessions_dir("unreadable-copy"));
        type_text(&mut app, "x");
        assert!(app.keep_session(false));
        let dir = app.session().unwrap().dir().to_path_buf();
        drop(app);
        let metadata = fs::read(dir.join("session.toml")).unwrap();
        fs::rename(dir.join("docs/1.txt"), dir.join("docs/backup.txt")).unwrap();
        fs::create_dir(dir.join("docs/1.txt")).unwrap();
        let workspace = Workspace::new([root]).unwrap();
        let mut restored = App::new(workspace, None, 80, 10).unwrap();
        assert!(restored
            .restore_session(
                Session::open(&dir).unwrap(),
                session::read(&dir).unwrap(),
                Vec::new()
            )
            .is_err());
        drop(restored);
        assert_eq!(fs::read(dir.join("session.toml")).unwrap(), metadata);
        assert_eq!(
            fs::read_to_string(dir.join("docs/backup.txt")).unwrap(),
            "xalpha"
        );
    }

    /// Where `text` is on screen, as (column, row), if it's there.
    fn find_on_screen(app: &App, text: &str) -> Option<(u32, u32)> {
        screen(app).lines().enumerate().find_map(|(y, line)| {
            let byte = line.find(text)?;
            Some((line[..byte].chars().count() as u32, y as u32))
        })
    }

    fn status_message(app: &App) -> Option<String> {
        match app.active_panel().status() {
            status::Status::Message { text, .. } => Some(text),
            _ => None,
        }
    }

    #[test]
    fn reader_mode_shows_markdown_as_it_reads_and_cant_be_typed_in() {
        let _serial = crate::test_serial();
        let text = "# Title\n\nSome **bold** text.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";
        let root = fixture("reader-mode", &[("doc.md", text), ("a.txt", "a")]);
        let mut app = app(&root, Some("doc.md"));
        assert!(screen(&app).contains(" Read "), "{}", screen(&app));
        app.run(Command::ToggleReader, false);
        assert!(app.ed().reading());
        let shown = screen(&app);
        assert!(shown.contains("Some bold text."), "{shown}");
        assert!(shown.contains("┌───┬───┐"), "{shown}");
        assert!(
            !shown.contains("**") && !shown.contains("# Title"),
            "{shown}"
        );
        assert!(shown.contains(" Edit "), "{shown}");

        // Typing, pasting, and deleting say how to edit, and don't.
        key(&mut app, KeyCode::Char('x'));
        let message = status_message(&app).unwrap_or_default();
        assert!(
            message.contains("reader mode") && message.contains("Ctrl+U"),
            "{message:?}"
        );
        app.paste("pasted");
        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.ed().document().text(), text);

        // Back to editing, the markup shows.
        ctrl(&mut app, 'u');
        assert!(!app.ed().reading());
        assert!(screen(&app).contains("# Title"));
        // Find and Find Next work on the text as written.
        ctrl(&mut app, 'u');
        ctrl(&mut app, 'f');
        assert!(!app.ed().reading());
        type_text(&mut app, "bold");
        key(&mut app, KeyCode::Esc);
        ctrl(&mut app, 'u');
        ctrl(&mut app, 'g');
        assert!(!app.ed().reading());
        assert_eq!(app.ed().selected_text().as_deref(), Some("bold"));
        // Other files have no reader mode.
        app.open(&root.join("a.txt"), false);
        assert!(!screen(&app).contains(" Read "));
        app.run(Command::ToggleReader, false);
        assert!(!app.ed().reading());
    }

    #[test]
    fn reader_mode_keeps_the_line_at_the_top_both_ways() {
        let _serial = crate::test_serial();
        let text: String = (0..60).map(|i| format!("para {i}\n\n")).collect();
        let root = fixture("reader-top", &[("doc.md", &text)]);
        let mut app = app(&root, Some("doc.md"));
        // Paragraph 10 is on line 20, and the cursor clear of the edges.
        app.ed_mut().set_place(23, 0, 20);
        app.run(Command::ToggleReader, false);
        assert_eq!(app.ed().reading_line(), Some(20));
        let body = app.active_panel().body();
        assert_eq!(
            find_on_screen(&app, "para 10").map(|(_, y)| y),
            Some(body.y)
        );
        // Two rows down is the next paragraph.
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.ed().reading_line(), Some(22));
        app.run(Command::ToggleReader, false);
        let (row, _, top) = app.ed().place();
        assert_eq!(top, 22, "the line at the top");
        // The cursor, still in view, stays put.
        assert_eq!(row, 23, "the cursor's row");
        assert_eq!(
            find_on_screen(&app, "para 11").map(|(_, y)| y),
            Some(body.y)
        );
    }

    #[test]
    fn double_clicking_in_reader_mode_edits_the_line_there() {
        let _serial = crate::test_serial();
        let text: String = (0..30).map(|i| format!("para {i}\n\n")).collect();
        let root = fixture("reader-edit", &[("doc.md", &text)]);
        let mut app = app(&root, Some("doc.md"));
        app.run(Command::ToggleReader, false);
        app.run(Command::CursorPageDown, false);
        app.run(Command::CursorPageDown, false);
        let (x, y) = find_on_screen(&app, "para 8").unwrap();
        left_click(&mut app, x, y);
        left_click(&mut app, x, y);
        assert!(!app.ed().reading());
        assert_eq!(app.ed().place().0, 16, "the cursor's row");
        // The line stays where it was on screen.
        assert_eq!(find_on_screen(&app, "para 8").map(|(_, row)| row), Some(y));
    }

    #[test]
    fn links_in_reader_mode_open_files_at_their_headings() {
        let _serial = crate::test_serial();
        let other: String = std::iter::once("# One\n\n".to_string())
            .chain((0..20).map(|i| format!("filler {i}\n\n")))
            .chain(std::iter::once("## Part Two\n\n".to_string()))
            .chain((0..20).map(|i| format!("more {i}\n\n")))
            .collect();
        let root = fixture(
            "reader-links",
            &[
                (
                    "doc.md",
                    "see [the other](sub/other%20file.md#part-two) one\n",
                ),
                ("sub/other file.md", &other),
            ],
        );
        let mut app = app(&root, Some("doc.md"));
        app.run(Command::ToggleReader, false);
        let (x, y) = find_on_screen(&app, "the other").unwrap();
        // Clicking elsewhere doesn't.
        left_click(&mut app, x - 2, y);
        assert_eq!(shown_name(&app).as_deref(), Some("doc.md"));
        left_click(&mut app, x + 1, y);
        assert_eq!(shown_name(&app).as_deref(), Some("other file.md"));
        assert!(app.ed().reading());
        let body = app.active_panel().body();
        assert_eq!(
            find_on_screen(&app, "Part Two").map(|(_, y)| y),
            Some(body.y)
        );
        // Back goes to the file with the link, still in reader mode.
        app.run(Command::GoBack, false);
        assert_eq!(shown_name(&app).as_deref(), Some("doc.md"));
        assert!(app.ed().reading());
        // A link to nowhere says so.
        fs::write(root.join("doc.md"), "[gone](missing.md)\n").unwrap();
        app.ed().document().revert().unwrap();
        let (x, y) = find_on_screen(&app, "gone").unwrap();
        left_click(&mut app, x, y);
        assert_eq!(shown_name(&app).as_deref(), Some("doc.md"));
        let message = status_message(&app).unwrap_or_default();
        assert!(message.contains("Can't find missing.md"), "{message:?}");
    }

    #[test]
    fn reader_mode_follows_edits_from_other_panels() {
        let _serial = crate::test_serial();
        let root = fixture("reader-live", &[("doc.md", "# Title\n\nbody\n")]);
        let mut app = app(&root, Some("doc.md"));
        app.run(Command::SplitRight, false);
        app.open(&root.join("doc.md"), false);
        app.run(Command::ToggleReader, false);
        app.run(Command::FocusPanelLeft, false);
        assert!(!app.ed().reading(), "each panel has its own mode");
        app.run(Command::DocumentEnd, false);
        type_text(&mut app, "- new item");
        assert!(screen(&app).contains("• new item"), "{}", screen(&app));
    }

    #[test]
    fn header_button_switches_reader_mode() {
        let _serial = crate::test_serial();
        let root = fixture("reader-button", &[("doc.md", "# Title\n")]);
        let mut app = app(&root, Some("doc.md"));
        let (x, y) = find_on_screen(&app, " Read ").unwrap();
        left_click(&mut app, x + 1, y);
        assert!(app.ed().reading());
        let (x, y) = find_on_screen(&app, " Edit ").unwrap();
        left_click(&mut app, x + 1, y);
        assert!(!app.ed().reading());
    }

    fn screen_of(app: &App, width: u32, height: u32) -> String {
        let frame = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
        app.draw(&frame);
        frame.to_text(true)
    }

    /// The rows `text` is on in the active panel and the other one, of two
    /// side by side, 120 x 20.
    fn level(app: &App, text: &str) -> (Option<u32>, Option<u32>) {
        let split = other_panel(app).area().x as usize;
        let screen = screen_of(app, 120, 20);
        let find = |right: bool| {
            screen.lines().enumerate().find_map(|(y, line)| {
                let cut: String = match right {
                    false => line.chars().take(split).collect(),
                    true => line.chars().skip(split).collect(),
                };
                cut.contains(text).then_some(y as u32)
            })
        };
        (find(false), find(true))
    }

    /// The panel other than the active one, of two.
    fn other_panel(app: &App) -> &Panel {
        let tab = app.tab();
        tab.panels
            .iter()
            .find(|panel| panel.id != tab.active)
            .unwrap()
    }

    #[test]
    fn preview_to_the_side_reads_beside_the_editor_and_scrolls_with_it() {
        let _serial = crate::test_serial();
        let text: String = (0..80).map(|i| format!("para {i}\n\n")).collect();
        let root = fixture("preview-side", &[("doc.md", &text), ("a.txt", "a")]);
        let mut app = app(&root, Some("doc.md"));
        app.resize(120, 20);
        app.ed_mut().set_place(23, 0, 20);
        app.run(Command::PreviewToSide, false);
        assert_eq!(app.tab().panels.len(), 2);
        // The keyboard stays with the editor, on the left.
        assert!(!app.ed().reading());
        let preview = other_panel(&app).editor().unwrap();
        assert!(preview.reading());
        assert!(other_panel(&app).area().x > app.active_panel().area().x);
        // The cursor's line is level in both.
        let (left, right) = level(&app, "para 11 ");
        assert!(
            left.is_some() && left == right,
            "{}",
            screen_of(&app, 120, 20)
        );

        // Paging the editor down pages the preview.
        key(&mut app, KeyCode::PageDown);
        let line = app.ed().place().0;
        let shown = format!("para {} ", line / 2);
        let (left, right) = level(&app, &shown);
        assert!(
            left.is_some() && left == right,
            "{shown:?} {}",
            screen_of(&app, 120, 20)
        );

        // Moving the cursor without scrolling leaves the preview be.
        let before = other_panel(&app).editor().unwrap().reading_line();
        key(&mut app, KeyCode::Up);
        assert_eq!(other_panel(&app).editor().unwrap().reading_line(), before);

        // The wheel over the preview scrolls the editor along.
        let area = other_panel(&app).body();
        for _ in 0..4 {
            app.handle_mouse(
                Mouse {
                    kind: MouseKind::ScrollDown,
                    x: area.x + 5,
                    y: area.y + 2,
                    mods: Mods::NONE,
                },
                Instant::now(),
            );
        }
        let top = other_panel(&app).editor().unwrap().reading_line().unwrap();
        assert!(top > before.unwrap());
        let editor_top = app.ed().scroll_anchor();
        assert!(
            app.ed().place().2 >= top.saturating_sub(1),
            "{editor_top:?} {top}"
        );

        // Asked again, the preview on screen will do.
        app.run(Command::PreviewToSide, false);
        assert_eq!(app.tab().panels.len(), 2);
        // Other files have no preview.
        app.open(&root.join("a.txt"), false);
        app.run(Command::PreviewToSide, false);
        assert_eq!(app.tab().panels.len(), 2);
    }

    #[test]
    fn two_editors_of_a_file_scroll_apart() {
        let _serial = crate::test_serial();
        let text: String = (0..80).map(|i| format!("para {i}\n\n")).collect();
        let root = fixture("preview-apart", &[("doc.md", &text)]);
        let mut app = app(&root, Some("doc.md"));
        app.resize(120, 20);
        app.run(Command::SplitRight, false);
        app.open(&root.join("doc.md"), false);
        let before = other_panel(&app).editor().unwrap().place().2;
        key(&mut app, KeyCode::PageDown);
        key(&mut app, KeyCode::PageDown);
        assert_eq!(other_panel(&app).editor().unwrap().place().2, before);
    }
}
