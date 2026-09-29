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

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use opentui::{Attributes, Buffer, Rgba};

use crate::alert::{Alert, AlertAction, Button};
use crate::context_menu::{ContextMenu, MenuAction, MenuItem};
use crate::document::{self, Disk, DiskChange, Document};
use crate::editor::{Action, Editor};
use crate::file_dialog::{DialogAction, FileDialog, Purpose};
use crate::file_index::FileIndex;
use crate::find;
use crate::input::{Key, KeyCode, Mods, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Context, Keymap};
use crate::layout::{self, Axis, Direction, Layout, PanelId, Rect};
use crate::line_edit::Edit;
use crate::panel::{HeaderButton, Panel, Visit};
use crate::picker::{Choice, Item, Mode, Picker, PickerAction};
use crate::search_modal::{Memory, SearchAction, SearchModal};
use crate::status::{self, Prompt, PromptKey};
use crate::tab::{self, BarItem, Tab, TabId};
use crate::terminal::Terminal;
use crate::theme::Theme;
use crate::tree::{Entry, FileTree, TreeAction};
use crate::watch::{Changes, Watcher};
use crate::workspace::Workspace;

const DIVIDER: Rgba = Rgba::rgb(69, 71, 90);
/// Tints where a panel dragged by its header would land.
const DROP_TINT: Rgba = Rgba::rgba(137, 180, 250, 56);
/// How far sideways a header that resizes a split is dragged, before it's
/// dragged up or down, to move its panel instead.
const MOVE_THRESHOLD: u32 = 3;

const DEFAULT_TREE_WIDTH: u32 = 30;
const MIN_TREE_WIDTH: u32 = 12;
/// The tree hides rather than leave the panels narrower than this.
const MIN_EDITOR_WIDTH: u32 = 40;
/// How many recently shown files and terminals the picker lists first.
const RECENT_FILES: usize = 50;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    /// The active panel.
    Editor,
}

/// Where a mouse press landed; drags and the release go there too.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MouseTarget {
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
    /// Running terminals, and exited ones still shown, in the order they
    /// were started.
    terminals: Vec<Rc<RefCell<Terminal>>>,
    /// The id for the next new terminal.
    next_terminal: u32,
    /// The terminal prefix (Ctrl+`) was pressed: the next key is a cue
    /// shortcut.
    terminal_prefix: bool,
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
    menu: Option<(ContextMenu, Entry)>,
    /// What the last search left behind, for the next one.
    search_memory: Memory,
    /// The last find bar's query and replacement.
    find_memory: find::Memory,
}

/// A popup's query line, which typing, pasting, and the editor's cursor
/// keys edit.
trait QueryInput {
    fn edit(&mut self, edit: Edit);
}

impl QueryInput for Picker {
    fn edit(&mut self, edit: Edit) {
        Picker::edit(self, edit);
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
}

impl App {
    /// Shows `workspace`, with `file` open, or a new, unnamed buffer. Focus
    /// starts in the editor when there is a file, in the tree otherwise.
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
        let focus = if file.is_some() {
            Focus::Editor
        } else {
            Focus::Tree
        };
        let theme = Rc::new(Theme::new().map_err(|e| e.to_string())?);
        let (doc, notice) =
            Document::open(file.clone(), theme.clone()).map_err(|reason| match &file {
                Some(file) => format!("{}: {reason}", file.display()),
                None => reason,
            })?;
        let mut app = App {
            files: FileIndex::new(&workspace),
            watcher: Watcher::new(),
            watching: None,
            workspace,
            keymap: Keymap::default(),
            tree,
            documents: vec![doc.clone()],
            theme,
            terminals: Vec::new(),
            next_terminal: 1,
            terminal_prefix: false,
            focused_terminal: None,
            tabs: vec![Tab::new(0, 0)],
            tab: 0,
            next_tab: 1,
            next_panel: 1,
            tab_prompt: None,
            clipboard: None,
            focus,
            tree_visible: true,
            tree_width: DEFAULT_TREE_WIDTH,
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
        };
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
        app.note_recent();
        app.watch_folders();
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
        if let Some(terminal) = self.keyboard_terminal() {
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
        let context = match self.focus {
            // Its keys are the picker's.
            _ if self.menu.is_some() => Context::Picker,
            _ if self.search.is_some() => Context::Search,
            _ if self.dialog.is_some() => Context::Dialog,
            _ if self.picker.is_some() => Context::Picker,
            Focus::Tree => Context::Tree,
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
            None if self.query_input().is_some() => {
                self.edit_query(key);
                AppAction::Continue
            }
            None => {
                if self.focus == Focus::Editor {
                    if let Some(editor) = self.editor_mut() {
                        editor.type_key(key);
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
    /// the cursor, deleting, and pasting.
    fn edit_query(&mut self, key: Key) {
        let binding = self.keymap.lookup(key, Context::Editor);
        let clipboard = self.clipboard.clone();
        let mut buf = [0; 4];
        let edit = match binding.map(|(command, _)| command) {
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
                None => return,
            },
            Some(_) => return,
            None => match key.code {
                KeyCode::Char(c) if key.mods.is_plain() => Edit::Insert(c.encode_utf8(&mut buf)),
                _ => return,
            },
        };
        if let Some(input) = self.query_input() {
            input.edit(edit);
        }
    }

    /// Runs `command`. With `select`, a cursor movement extends the selection.
    pub fn run(&mut self, command: Command, select: bool) -> AppAction {
        match command {
            Command::Quit => return self.quit(),
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
            Command::Palette => self.show_picker(Mode::Commands),
            Command::SearchWorkspace => self.show_search(),
            Command::NewFile => self.new_untitled(),
            Command::OpenFile => self.show_dialog(Purpose::Open),
            Command::CreateFile => self.show_dialog(Purpose::Create),
            Command::SaveAs => self.show_dialog(Purpose::SaveAs),
            Command::SplitRight => self.split(Axis::Horizontal),
            Command::SplitDown => self.split(Axis::Vertical),
            Command::ClosePanel => self.close_panel(),
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
                    "There's only this tab; {} opens another.",
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
                    let message = format!("There's no tab {}.", index + 1);
                    self.show_message(message, false);
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
            Command::CloseTerminal => {
                if self.active_terminal().is_some() {
                    self.close_shown(Redo::Run(Command::CloseTerminal));
                }
            }
            Command::CloseFile => {
                if self.editor().is_some() && self.close_shown(Redo::Run(Command::CloseFile)) {
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
                    "Next shortcut goes to cue; {} again sends it to the shell.",
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
                if let Some(editor) = self.editor_mut() {
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
                if let Some(editor) = self.editor_mut() {
                    editor.find_step(&memory, forward);
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
            | Command::TreeOpenInTerminal => {
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
            let action = picker.handle_mouse(mouse);
            let action = self.picker_action(action);
            self.keep_if_edited();
            return action;
        }
        if let Some(dialog) = &mut self.dialog {
            let action = dialog.handle_mouse(mouse);
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
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.activate(id);
                }
                // The wheel scrolls any panel; the rest goes to the active one.
                if let Some(panel) = self.tab_mut().panel_mut(id) {
                    panel.handle_mouse(mouse, now);
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
            // The status bar.
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
        if let Some(terminal) = terminal {
            terminal.borrow_mut().paste(text);
        } else if let Some(input) = self.query_input() {
            // Terminals send newlines in pastes as CR.
            let line = text.split(['\r', '\n']).next().unwrap_or("");
            input.edit(Edit::Insert(line));
            self.note_find_memory();
        } else if self.focus == Focus::Editor {
            if let Some(editor) = self.editor_mut() {
                editor.paste(text);
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
        self.editor_mut();
        self.note_terminal_focus();
        self.watch_folders();
    }

    /// Draws the frame and returns where the terminal cursor goes (0-based
    /// column, row), or `None` to hide it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        frame.clear(Rgba::terminal_default([0, 0, 0]));
        let tree_width = self.visible_tree_width();
        let main = self.main_area();
        if tree_width > 0 {
            self.tree
                .draw(frame, 0, tree_width, self.focus == Focus::Tree);
            for y in 0..self.height.saturating_sub(1) {
                frame.draw_text("│", tree_width, y, DIVIDER, None, Attributes::NONE);
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
            if let Some(prompt) = status::draw(frame, &status, y, self.width, &self.keymap) {
                cursor = Some(prompt);
            }
        }
        for handle in tab.layout.handles(main) {
            if handle.axis == Axis::Horizontal {
                let rect = handle.rect;
                for y in rect.y..rect.y + rect.height {
                    frame.draw_text("│", rect.x, y, DIVIDER, None, Attributes::NONE);
                }
            }
        }
        if let Some(drop) = self.header_drag.as_ref().and_then(|drag| drag.drop) {
            let rect = drop.rect;
            frame.fill_rect(rect.x, rect.y, rect.width, rect.height, DROP_TINT);
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
            changed |= terminal.borrow_mut().poll();
        }
        if changed {
            self.prune_terminals();
        }
        changed |= self.search.as_mut().is_some_and(SearchModal::poll);
        if self.files.poll() {
            if let Some(picker) = &mut self.picker {
                picker.set_files(&self.files);
                changed = true;
            }
        }
        if let Some(changes) = self.watcher.poll() {
            changed |= self.disk_changed(changes);
            // Folders may have come back for open files, and the tree may
            // list others.
            self.watching = None;
            self.watch_folders();
        }
        changed
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
                    "{} changed on disk; saving will ask whether to overwrite it.",
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
            self.show_message("No room to split this panel.", false);
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

    /// Shows what the active panel showed before what it shows now, or
    /// with `back` false, what it went back from. A closed file opens
    /// again; closed terminals, and those another panel shows now, are
    /// skipped.
    fn go_history(&mut self, back: bool) {
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
        };
        let Some(visit) = self.tabs[self.tab]
            .active_panel_mut()
            .step_history(back, usable)
        else {
            let message = match back {
                true => "Nothing to go back to.",
                false => "Nothing to go forward to.",
            };
            self.show_message(message, false);
            return;
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
        }
        self.active_panel_mut().restore_history(history);
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
                if mode == Mode::Files {
                    // Files may have come or gone since the last listing,
                    // which is shown until this one is done.
                    self.files.refresh();
                }
                let recent = self.recent_items();
                // Terminal commands, for the terminal on screen.
                let terminal = self.active_terminal().is_some();
                let available =
                    |command: Command| command.context() != Context::Terminal || terminal;
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

    /// Keeps the find bar's query for finding in other files.
    fn note_find_memory(&mut self) {
        if let Some(memory) = self.editor().and_then(Editor::find_memory) {
            if *memory != self.find_memory {
                self.find_memory = memory.clone();
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
        let doc = self.active_panel().document();
        let path = doc.and_then(|doc| doc.path());
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
            let running = match terminal.exit() {
                Some(_) => "exited".to_string(),
                None => terminal.program().unwrap_or_default(),
            };
            Item::terminal(terminal.id(), &terminal.name(), &running)
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
            Choice::Command(_) => {}
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
        let message = format!(
            "{name} changed on disk since it was opened or last saved. Overwrite it \
             with your changes, or revert to the file on disk and lose them?"
        );
        let buttons = vec![
            Button::new(
                "&Overwrite",
                Answer::Overwrite(doc.clone(), saving.clone()),
            )
            .danger(),
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
                self.show_message(format!("Reverted {name} to the file on disk."), false);
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
        let doc = match Document::open(None, self.theme.clone()) {
            Ok((doc, _)) => doc,
            Err(err) => {
                self.show_message(format!("Can't open a new file: {err}"), true);
                return;
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
        self.show_document(&doc);
    }

    /// The workspace's first folder, where terminals start.
    fn workspace_folder(&self) -> PathBuf {
        match self.workspace.roots().first() {
            Some(root) => root.clone(),
            None => std::env::current_dir().unwrap_or_default(),
        }
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
                    self.show_message("There's no file here to save.", false);
                    return;
                }
            },
            _ => None,
        };
        let name = current
            .as_deref()
            .and_then(Path::file_name)
            .map_or(String::new(), |name| name.to_string_lossy().into_owned());
        self.open_dialog(purpose, &self.dialog_folder(), &name, current);
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
        let file = self.active_panel().document().and_then(|doc| doc.path());
        match file.as_deref().and_then(Path::parent) {
            Some(folder) if folder.is_dir() => folder.to_path_buf(),
            _ => self.workspace_folder(),
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
            return Err(format!("{} is open; close it first.", file_name(path)));
        }
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder).map_err(|err| format!("Can't create its folder: {err}"))?;
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

    fn quit(&mut self) -> AppAction {
        let unsaved: Vec<Rc<Document>> = self
            .documents
            .iter()
            .filter(|doc| doc.is_modified())
            .cloned()
            .collect();
        let running = running_programs(&self.terminals);
        let redo = Redo::Run(Command::Quit);
        match self.ask_first("Quit cue?".into(), unsaved, &running, "&Quit", redo) {
            true => AppAction::Quit,
            false => AppAction::Continue,
        }
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
        if let Some(path) = self.active_panel().document().and_then(|doc| doc.path()) {
            return Entry {
                path,
                is_dir: false,
                is_root: false,
            };
        }
        Entry {
            path: self.workspace_folder(),
            is_dir: true,
            is_root: true,
        }
    }

    /// Shift+F10, or from the palette: the context menu of the tree's
    /// selection, next to it. From elsewhere, the tree shows the file on
    /// screen first.
    fn show_tree_menu(&mut self) {
        if self.focus != Focus::Tree {
            let file = self.active_panel().document().and_then(|doc| doc.path());
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
        if !target.is_root {
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
        self.menu = Some((menu, target));
    }

    fn menu_action(&mut self, action: MenuAction) -> AppAction {
        match action {
            MenuAction::Continue => AppAction::Continue,
            MenuAction::Close | MenuAction::CloseAndPass => {
                self.menu = None;
                AppAction::Continue
            }
            MenuAction::Accept(command) => match self.menu.take() {
                Some((_, target)) => self.file_command(command, target, true),
                None => AppAction::Continue,
            },
        }
    }

    /// Runs the file command `command` on `target`. Trashing asks first,
    /// unless it was chosen from a context menu.
    fn file_command(&mut self, command: Command, target: Entry, from_menu: bool) -> AppAction {
        let parent = match target.path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => self.workspace_folder(),
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
            Command::TreeRename => {
                self.open_dialog(Purpose::Move, &parent, &name, Some(target.path));
            }
            Command::TreeDuplicate => {
                let copy = copy_name(&target.path);
                self.open_dialog(Purpose::Duplicate, &parent, &copy, Some(target.path));
            }
            Command::TreeTrash => self.trash(&target.path, from_menu),
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
            fs::create_dir_all(folder).map_err(|err| format!("Can't create its folder: {err}"))?;
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
            fs::create_dir_all(folder).map_err(|err| format!("Can't create its folder: {err}"))?;
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
                true => "The folder and everything in it can be restored from the Trash.",
                false => "It can be restored from the Trash.",
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

    // --- terminals --------------------------------------------------------------

    /// Starts a shell in the workspace's first folder, in the active panel.
    fn new_terminal(&mut self) {
        self.new_terminal_in(&self.workspace_folder());
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
        assert!(matches!(
            key(&mut app, KeyCode::Char('r')),
            AppAction::Quit
        ));
        assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
    }

    /// The file the active panel shows, by name.
    fn shown_name(app: &App) -> Option<String> {
        app.editor()
            .and_then(Editor::path)
            .map(|path| file_name(&path))
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
        let forward = |app: &mut App| app.handle_key(Key::new(KeyCode::Char('-'), ctrl_shift));
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
        let header = screen(&app).lines().nth(area.y as usize).unwrap().to_string();
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

        // Ctrl+P is the shell's, and cue's after the prefix, Ctrl+`, which
        // the status bar hints.
        assert!(screen(&app).contains("^` cue keys"), "{}", screen(&app));
        let prefix = Key::new(KeyCode::Char('`'), Mods::CTRL);
        app.handle_key(prefix);
        assert!(screen(&app).contains("Next shortcut goes to cue"));
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
        assert!(
            screen(&app).contains("Terminal 1 sleep"),
            "{}",
            screen(&app)
        );
        key(&mut app, KeyCode::Enter);
        assert!(app.active_terminal().is_some());
        // And from there, back to the file.
        prefixed_ctrl(&mut app, 'p');
        key(&mut app, KeyCode::Enter);
        assert!(app.ed().path().is_some_and(|path| path.ends_with("a.txt")));
    }

    #[test]
    fn the_palette_has_terminal_commands_for_the_terminal_on_screen() {
        let _serial = crate::test_serial();
        let root = fixture("terminal-commands", &[("a.txt", "alpha")]);
        let mut app = app(&root, Some("a.txt"));
        let palette = |app: &mut App| {
            app.run(Command::Palette, false);
            type_text(app, "terminal");
            let shown = screen(app);
            key(app, KeyCode::Esc);
            shown
        };
        let shown = palette(&mut app);
        assert!(shown.contains("New Terminal"), "{shown}");
        assert!(!shown.contains("Close Terminal"), "{shown}");
        assert!(!shown.contains("Clear Terminal"), "{shown}");

        app.run(Command::NewTerminal, false);
        let shown = palette(&mut app);
        assert!(shown.contains("Close Terminal"), "{shown}");
        assert!(shown.contains("Clear Terminal"), "{shown}");
        assert!(shown.contains("Rename Terminal"), "{shown}");

        // Its name is in the status bar. Renaming it asks for another
        // there, which takes the keys, even from the tree.
        let status_bar = |app: &App| screen(app).lines().last().unwrap().to_string();
        assert!(
            status_bar(&app).starts_with(" Terminal 1"),
            "{}",
            status_bar(&app)
        );
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
        // goes back to the number.
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
            status_bar(&app).starts_with(" Terminal 1"),
            "{}",
            status_bar(&app)
        );

        // Closing it asks first, then hangs up on what it's running, and
        // leaves the panel empty.
        type_text(&mut app, "sleep 30");
        key(&mut app, KeyCode::Enter);
        // Not only busy: the login shell may be running its own startup.
        wait_until(&mut app, "sleep to run", |app| {
            app.terminals[0].borrow().program().as_deref() == Some("sleep")
        });
        for _ in 0..2 {
            assert!(app.active_terminal().is_some());
            prefixed_ctrl(&mut app, 'k');
            type_text(&mut app, "close terminal");
            key(&mut app, KeyCode::Enter);
        }
        assert!(app.active_terminal().is_none());
        assert!(app.editor().is_none());
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
        assert_eq!(app.visible_tree_width(), DEFAULT_TREE_WIDTH);
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
        assert!(screen(&app).contains("a.txt is open; close it first."));
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
        assert!(screen(&app).contains("a.txt is open; close it first."));
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
            crate::status::Status::Info(info) => info,
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
        assert!(text.contains("Close Terminal 1?"), "{text}");
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
            frame.bg_at(x, y) != Some(Rgba::terminal_default([0, 0, 0]))
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
            screen(&app).contains("No room to split"),
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
        assert!(screen(&app).contains("There's no tab 3."));
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
        assert!(screen(&app).contains("There's only this tab; Ctrl+T opens another."));

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
}
