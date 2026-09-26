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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use opentui::{Attributes, Buffer, Rgba};

use crate::document::{self, Document};
use crate::editor::{Action, Editor};
use crate::file_index::FileIndex;
use crate::find;
use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Context, Keymap};
use crate::layout::{self, Axis, Direction, Layout, PanelId, Rect};
use crate::line_edit::Edit;
use crate::panel::Panel;
use crate::picker::{Choice, Mode, Picker, PickerAction};
use crate::search_modal::{Memory, SearchAction, SearchModal};
use crate::status;
use crate::theme::Theme;
use crate::tree::{FileTree, TreeAction};
use crate::workspace::Workspace;

const DIVIDER: Rgba = Rgba::rgb(69, 71, 90);

const DEFAULT_TREE_WIDTH: u32 = 30;
const MIN_TREE_WIDTH: u32 = 12;
/// The tree hides rather than leave the panels narrower than this.
const MIN_EDITOR_WIDTH: u32 = 40;
/// How many recently shown files the picker lists first.
const RECENT_FILES: usize = 50;

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
    /// Where two panels meet.
    Handle(Vec<bool>),
    Panel(PanelId),
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
    layout: Layout,
    /// The panels in the layout, in no particular order. Never empty.
    panels: Vec<Panel>,
    /// The panel keys go to, unless the tree has focus.
    active: PanelId,
    /// The id for the next new panel.
    next_panel: PanelId,
    /// Text from the last copy or cut, shared by all editors.
    clipboard: Option<String>,
    focus: Focus,
    tree_visible: bool,
    /// The tree's width when there is room for it.
    tree_width: u32,
    width: u32,
    height: u32,
    mouse_target: Option<MouseTarget>,
    /// Quit was asked for with unsaved changes; asking again quits.
    quit_armed: bool,
    /// The file opened as a preview, which the next preview replaces.
    preview: Option<Rc<Document>>,
    /// The row and time of the last click in the tree, to spot double clicks.
    last_tree_click: Option<(u32, Instant)>,
    picker: Option<Picker>,
    /// Every file in the workspace, for the picker.
    files: FileIndex,
    /// Files shown in the editor, most recent first.
    recent: Vec<PathBuf>,
    /// Workspace search, while open. Never open with the picker.
    search: Option<SearchModal>,
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
            tree.reveal(file);
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
            workspace,
            keymap: Keymap::default(),
            tree,
            documents: vec![doc.clone()],
            theme,
            layout: Layout::Panel(0),
            panels: vec![Panel::new(0)],
            active: 0,
            next_panel: 1,
            clipboard: None,
            focus,
            tree_visible: true,
            tree_width: DEFAULT_TREE_WIDTH,
            width,
            height,
            mouse_target: None,
            quit_armed: false,
            preview: None,
            last_tree_click: None,
            picker: None,
            recent: Vec::new(),
            search: None,
            search_memory: Memory::default(),
            find_memory: find::Memory::default(),
        };
        app.layout();
        app.panels[0].show(&doc).map_err(|e| e.to_string())?;
        if let Some(notice) = notice {
            app.panels[0].show_message(notice, false);
        }
        app.note_recent();
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
        if self.editor().is_some_and(Editor::prompt_open) {
            let action = match self.editor_mut() {
                Some(editor) => editor.handle_prompt_key(key),
                None => Action::Continue,
            };
            return self.editor_action(action);
        }
        let context = match self.focus {
            _ if self.search.is_some() => Context::Search,
            _ if self.picker.is_some() => Context::Picker,
            Focus::Tree => Context::Tree,
            Focus::Editor => self
                .editor()
                .and_then(Editor::find_field)
                .map_or(Context::Editor, find::Field::context),
        };
        let binding = self.keymap.lookup(key, context);
        if !matches!(binding, Some((Command::Quit, _))) {
            self.quit_armed = false;
        }
        match binding {
            Some((command, select)) => {
                // Other global commands (save, quit, ...) close the picker
                // or search and run as usual.
                if command.context() == Context::Global
                    && !matches!(
                        command,
                        Command::GoToFile | Command::Palette | Command::SearchWorkspace
                    )
                {
                    self.close_popups();
                }
                self.run(command, select)
            }
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
            Command::SplitRight => self.split(Axis::Horizontal),
            Command::SplitDown => self.split(Axis::Vertical),
            Command::ClosePanel => self.close_panel(),
            Command::FocusPanelLeft => self.focus_panel(Direction::Left),
            Command::FocusPanelRight => self.focus_panel(Direction::Right),
            Command::FocusPanelUp => self.focus_panel(Direction::Up),
            Command::FocusPanelDown => self.focus_panel(Direction::Down),
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
                if matches!(command.context(), Context::Picker | Context::Search)
                    || (command.context() == Context::SearchOptions && self.search.is_some()) =>
            {
                if let Some(search) = &mut self.search {
                    let action = search.run(command);
                    return self.search_action(action);
                }
                let Some(picker) = &mut self.picker else {
                    return AppAction::Continue;
                };
                let action = picker.run(command);
                return self.picker_action(action);
            }
            command if command.context() == Context::Tree => {
                let action = self.tree.run(command);
                self.tree_action(action);
            }
            command => {
                let active = self.active_index();
                let Some(editor) = self.panels[active].editor_mut() else {
                    return AppAction::Continue;
                };
                let action = editor.run(command, select, &mut self.clipboard);
                // Saving an unnamed file from the tree asks for a name in
                // the editor's status bar.
                if editor.prompt_open() {
                    self.focus = Focus::Editor;
                }
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
        let target = match mouse.kind {
            MouseKind::Press(_) => {
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
            // Like a key press, a click dismisses messages, the quit
            // confirmation, and the "Save as" prompt.
            self.quit_armed = false;
            self.active_panel_mut().clear_message();
            if let Some(editor) = self.editor_mut() {
                editor.cancel_prompt();
            }
        }

        match target {
            MouseTarget::Tree => match mouse.kind {
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
            MouseTarget::Handle(path) => match mouse.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    let area = self.main_area();
                    self.layout.drag(&path, area, mouse.x, mouse.y);
                    self.layout();
                }
                // A panel's header is a handle when there's one above it;
                // clicking it still gives it the keyboard.
                MouseKind::Press(MouseButton::Left) => {
                    if let Some(id) = self.panel_at(mouse.x, mouse.y) {
                        self.activate(id);
                    }
                }
                _ => {}
            },
            MouseTarget::Panel(id) => {
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.activate(id);
                }
                // The wheel scrolls any panel; the rest goes to the active one.
                if let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) {
                    panel.handle_mouse(mouse, now);
                }
                self.note_find_memory();
            }
        }
        AppAction::Continue
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
        if let Some(handle) = self
            .layout
            .handles(area)
            .into_iter()
            .find(|handle| handle.rect.contains(x, y))
        {
            return Some(MouseTarget::Handle(handle.path));
        }
        self.panel_at(x, y).map(MouseTarget::Panel)
    }

    /// The panel at screen cell (`x`, `y`), if any.
    fn panel_at(&self, x: u32, y: u32) -> Option<PanelId> {
        self.layout
            .panels(self.main_area())
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(id, _)| id)
    }

    /// Text pasted through the terminal.
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = self.query_input() {
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
    /// bar's query, and puts the active editor's cursor back in the buffer
    /// it shares with other panels, which input to another may have taken.
    fn after_input(&mut self) {
        self.keep_if_edited();
        self.note_find_memory();
        for doc in &self.documents {
            doc.follow_edits();
        }
        self.editor_mut();
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
            for y in 0..main.height {
                frame.draw_text("│", tree_width, y, DIVIDER, None, Attributes::NONE);
            }
        }
        let mut cursor = None;
        for panel in &self.panels {
            let active = panel.id == self.active;
            let preview = panel.document().is_some_and(|doc| self.is_preview(doc));
            let at = panel.draw(frame, &self.keymap, &self.workspace, active, preview);
            if active {
                cursor = at;
            }
        }
        if self.height > 1 {
            let status = self.active_panel().status();
            let y = self.height - 1;
            if let Some(prompt) = status::draw(frame, &status, y, self.width, &self.keymap) {
                cursor = Some(prompt);
            }
        }
        for handle in self.layout.handles(main) {
            if handle.axis == Axis::Horizontal {
                let rect = handle.rect;
                for y in rect.y..rect.y + rect.height {
                    frame.draw_text("│", rect.x, y, DIVIDER, None, Attributes::NONE);
                }
            }
        }
        if let Some(search) = &self.search {
            return search.draw(frame);
        }
        if let Some(picker) = &self.picker {
            return picker.draw(frame);
        }
        cursor.filter(|_| self.focus == Focus::Editor)
    }

    /// Catches up on work in the background: listing the workspace's files
    /// and searching them. Returns whether the screen needs redrawing.
    pub fn poll(&mut self) -> bool {
        let searched = self.search.as_mut().is_some_and(SearchModal::poll);
        if !self.files.poll() {
            return searched;
        }
        match &mut self.picker {
            Some(picker) => {
                picker.set_files(&self.files);
                true
            }
            None => searched,
        }
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
        self.layout.split(self.active, id, axis);
        self.panels.push(Panel::new(id));
        self.layout();
        self.activate(id);
    }

    /// Closes the active panel, giving its room to its neighbor, which
    /// becomes active. The last panel is emptied instead. Its files stay
    /// open.
    fn close_panel(&mut self) {
        // With no name, the file couldn't be opened again.
        if let Some(doc) = self.active_panel().document() {
            let elsewhere = self
                .panels
                .iter()
                .any(|panel| panel.id != self.active && panel.has(doc));
            if doc.path().is_none() && doc.is_modified() && !elsewhere {
                let save = self.shortcut(Command::Save);
                self.show_message(format!("Save the new file first ({save})."), true);
                return;
            }
        }
        match self.layout.remove(self.active) {
            Some(next) => {
                let closed = self.active;
                self.panels.retain(|panel| panel.id != closed);
                self.layout();
                self.activate(next);
            }
            None => {
                self.active_panel_mut().clear();
                self.show_active_in_tree();
            }
        }
        self.prune_documents();
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
        match self
            .layout
            .neighbor(self.main_area(), self.active, direction)
        {
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
        if self.active != id {
            // It may have just closed.
            if let Some(panel) = self.panels.iter_mut().find(|p| p.id == self.active) {
                panel.clear_message();
            }
            self.active = id;
            self.show_active_in_tree();
        }
    }

    // --- picker -----------------------------------------------------------------

    /// Opens the picker listing `mode`, or switches it to `mode`. Pressed
    /// again, the same shortcut closes it.
    fn show_picker(&mut self, mode: Mode) {
        self.close_search();
        match &mut self.picker {
            Some(picker) if picker.mode() == mode => self.picker = None,
            Some(picker) => picker.set_mode(mode),
            None => {
                if mode == Mode::Files {
                    // Files may have come or gone since the last listing,
                    // which is shown until this one is done.
                    self.files.refresh();
                }
                // Listed first, most recent first. The file on screen is left
                // out so that Enter goes back to the previous one.
                let shown = self.editor().and_then(Editor::path);
                let recent = self
                    .recent
                    .iter()
                    .filter(|path| shown.as_ref() != Some(*path) && path.is_file())
                    .cloned()
                    .collect();
                self.picker = Some(Picker::new(
                    mode,
                    &self.workspace,
                    &self.keymap,
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
            PickerAction::Accept(choice) => {
                self.picker = None;
                match choice {
                    Choice::File(path) => {
                        if self.open(&path, false) {
                            self.focus = Focus::Editor;
                        }
                    }
                    Choice::Command(command) => return self.run(command, false),
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
        self.picker = None;
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
        self.picker = None;
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
        let active = self.active_index();
        self.panels[active].clear_message();
        // Coming back to a file, the keyboard is for its text.
        if let Some(editor) = self.panels[active].editor_mut() {
            editor.blur_find();
        }
        let existing = self.find_document(&path);
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
        let left = self.panels[active].document().cloned();
        if let Err(err) = self.panels[active].show(&doc) {
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
                .filter(|old| !self.panels.iter().any(|panel| panel.shows(old)))
                .and_then(|old| {
                    let index = self.documents.iter().position(|d| Rc::ptr_eq(d, &old))?;
                    Some((index, old))
                });
            match replaced {
                Some((index, old)) => {
                    for panel in &mut self.panels {
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

    /// Closes unnamed files that were never typed in and no panel has.
    fn prune_documents(&mut self) {
        let panels = &self.panels;
        self.documents
            .retain(|doc| !doc.is_blank() || panels.iter().any(|panel| panel.has(doc)));
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

    /// Moves the file on screen to the front of the recent files.
    fn note_recent(&mut self) {
        let Some(path) = self.editor().and_then(Editor::path) else {
            return;
        };
        self.recent.retain(|recent| *recent != path);
        self.recent.insert(0, path);
        self.recent.truncate(RECENT_FILES);
    }

    /// The open file at `path`, however it was named.
    fn find_document(&self, path: &Path) -> Option<Rc<Document>> {
        self.documents.iter().find(|doc| doc.is_file(path)).cloned()
    }

    /// Saves the current file to `input` from the "Save as" prompt, relative
    /// to the workspace's first folder, unless another file open is that one.
    fn save_as(&mut self, input: &Path) -> AppAction {
        let base = match self.workspace.roots().first() {
            Some(root) => root.clone(),
            None => std::env::current_dir().unwrap_or_default(),
        };
        let path = document::resolve(&base.join(input));
        let current = self.active_panel().document();
        if self
            .find_document(&path)
            .is_some_and(|open| !current.is_some_and(|current| Rc::ptr_eq(current, &open)))
        {
            let message = format!(
                "Can't save: {} is already open.",
                self.workspace.display_path(&path)
            );
            self.show_message(message, true);
            return AppAction::Continue;
        }
        let action = match self.editor_mut() {
            Some(editor) => editor.save_as(path),
            None => Action::Continue,
        };
        self.editor_action(action)
    }

    fn quit(&mut self) -> AppAction {
        let unsaved: Vec<String> = self
            .documents
            .iter()
            .filter(|doc| doc.is_modified())
            .map(|doc| match doc.path() {
                Some(path) => self.workspace.display_path(&path),
                None => "[new file]".to_string(),
            })
            .collect();
        if unsaved.is_empty() || self.quit_armed {
            return AppAction::Quit;
        }
        self.quit_armed = true;
        let message = format!(
            "Unsaved changes in {}. {} again to quit, {} to save.",
            unsaved.join(", "),
            self.shortcut(Command::Quit),
            self.shortcut(Command::Save),
        );
        self.show_message(message, true);
        AppAction::Continue
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
            Action::SaveAs(input) => self.save_as(&input),
        }
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

    /// Where the panels go: right of the tree, above the status bar.
    fn main_area(&self) -> Rect {
        let x = self.editor_x();
        Rect {
            x,
            y: 0,
            width: self.width.saturating_sub(x).max(1),
            height: self.height.saturating_sub(1),
        }
    }

    fn layout(&mut self) {
        if self.visible_tree_width() == 0 && self.focus == Focus::Tree {
            self.focus = Focus::Editor;
        }
        self.tree.set_height(self.main_area().height);
        if let Some(picker) = &mut self.picker {
            picker.set_size(self.width, self.height);
        }
        if let Some(search) = &mut self.search {
            search.set_size(self.width, self.height);
        }
        for (id, area) in self.layout.panels(self.main_area()) {
            if let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) {
                panel.set_area(area);
            }
        }
    }

    fn active_index(&self) -> usize {
        self.panels
            .iter()
            .position(|panel| panel.id == self.active)
            .expect("the active panel is open")
    }

    fn active_panel(&self) -> &Panel {
        &self.panels[self.active_index()]
    }

    fn active_panel_mut(&mut self) -> &mut Panel {
        let index = self.active_index();
        &mut self.panels[index]
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
            .join(format!("qedit-app-{}", std::process::id()))
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
        assert!(screen(&app).contains("Unsaved changes in a.txt."));
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Quit));
    }

    #[test]
    fn quit_confirmation_is_disarmed_by_other_keys() {
        let _serial = crate::test_serial();
        let root = fixture("quit", &[]);
        let mut app = app(&root, None);
        assert!(
            matches!(ctrl(&mut app, 'q'), AppAction::Quit),
            "nothing unsaved"
        );
        app.focus = Focus::Editor;
        type_text(&mut app, "x");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        type_text(&mut app, "y");
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Continue));
        assert!(matches!(ctrl(&mut app, 'q'), AppAction::Quit));
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
        assert!(app.ed().prompt_open());
        let path = root.join("new.txt");
        type_text(&mut app, path.to_str().unwrap());
        key(&mut app, KeyCode::Enter);
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
    fn saving_an_unnamed_file_from_the_tree_focuses_the_prompt() {
        let _serial = crate::test_serial();
        let root = fixture("prompt-focus", &[("a.txt", "")]);
        let mut app = app(&root, None);
        assert_eq!(app.focus, Focus::Tree);
        ctrl(&mut app, 's');
        assert_eq!(app.focus, Focus::Editor);
        type_text(&mut app, "named.txt");
        key(&mut app, KeyCode::Enter);
        assert!(root.join("named.txt").exists(), "relative to the workspace");
        assert_eq!(app.documents.len(), 1);
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
        key(&mut app, KeyCode::Enter);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "original");
        assert!(screen(&app).contains("Can't save: a.txt is already open."));
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
    fn a_click_disarms_quit_and_clears_messages() {
        let _serial = crate::test_serial();
        let root = fixture("click-quit", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        type_text(&mut app, "x");
        ctrl(&mut app, 'q');
        assert!(screen(&app).contains("Unsaved changes"));
        press(&mut app, 50, 0);
        assert!(!screen(&app).contains("Unsaved changes"));
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
        let panel = app.panels.iter().find(|panel| panel.id == id).unwrap();
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
        assert_eq!(app.panels.len(), 2);
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
        assert!(cells(&app, 9, 0..80).starts_with(" Ln 1, Col 1  Plain Text"));

        // Split down, from a panel showing a file.
        with_mods(
            &mut app,
            KeyCode::Char('\\'),
            Mods {
                shift: true,
                ..Mods::CTRL
            },
        );
        assert_eq!(app.panels.len(), 3);
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
        assert_eq!(app.active, 0);
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
        ctrl(&mut app, 'w');
        assert_eq!(app.panels.len(), 1);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
        assert_eq!(app.active_panel().area().width, 80);
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"], "files stay open");

        // Ctrl+P goes back to b.txt, edits and all.
        go_to_file(&mut app);
        key(&mut app, KeyCode::Enter);
        assert!(app.ed().text().starts_with("edited beta"));

        // The last panel empties instead.
        ctrl(&mut app, 'w');
        assert_eq!(app.panels.len(), 1);
        assert!(app.editor().is_none());
        assert!(screen(&app).contains("No file"));
        assert_eq!(open_paths(&app), ["a.txt", "b.txt"]);
        assert!(
            matches!(ctrl(&mut app, 'q'), AppAction::Continue),
            "b.txt is unsaved"
        );
    }

    #[test]
    fn a_panel_with_an_unsaved_new_file_stays_open() {
        let _serial = crate::test_serial();
        let root = fixture("close-new", &[]);
        let mut app = app(&root, None);
        key(&mut app, KeyCode::Esc);
        type_text(&mut app, "draft");
        ctrl(&mut app, 'w');
        assert!(app.editor().is_some());
        assert!(screen(&app).contains("Save the new file first (Ctrl+S)."));

        // A blank one goes away with its panel.
        let mut app = self::app(&root, None);
        ctrl(&mut app, '\\');
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert!(app.ed().is_blank());
        ctrl(&mut app, 'w');
        assert_eq!(app.panels.len(), 1);
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
        assert_eq!(app.active, 0);
        assert_eq!(
            app.ed().path().as_deref(),
            Some(root.join("a.txt").as_path())
        );
        // A header gives its panel the keyboard, and leaves the cursor.
        left_click(&mut app, 60, 0);
        assert_eq!(app.active, 1);
        left_click(&mut app, 30, 0);
        assert_eq!(app.active, 0);

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
        assert_eq!(app.active, 0);
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
        assert_eq!(app.panels[0].area().width, 30);
        assert_eq!(cells(&app, 0, 30..31), "│");
        assert_eq!(app.active, 0, "dragging doesn't move the keyboard");

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
    fn the_keyboard_moves_between_panels_and_the_tree() {
        let _serial = crate::test_serial();
        let root = fixture("panel-keys", &[("a.txt", "")]);
        let mut app = app(&root, Some("a.txt"));
        ctrl(&mut app, '\\');
        let right = app.active;
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert_eq!((app.active, app.focus), (0, Focus::Editor));
        with_mods(&mut app, KeyCode::Left, CTRL_ALT);
        assert_eq!(app.focus, Focus::Tree, "left of the panels is the tree");
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!((app.active, app.focus), (0, Focus::Editor));
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!(app.active, right);
        with_mods(&mut app, KeyCode::Right, CTRL_ALT);
        assert_eq!(app.active, right, "nothing further right");

        // Too small to split again.
        ctrl(&mut app, '\\');
        assert_eq!(app.panels.len(), 2);
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
}
