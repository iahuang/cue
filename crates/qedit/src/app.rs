//! The whole screen: the file tree on the left and the editor on the right.
//!
//! The app owns every open file. Each keeps its own text, undo history,
//! cursor, and scroll position while another is shown, so switching files
//! never loses work. Keys go through the keymap to whichever side has focus.
//!
//! As in VS Code, a file opened with a single click or Space is a preview:
//! the next preview replaces it, so stepping through files doesn't keep them
//! all open. Editing it, or opening it with Enter or a double click, keeps it.
//!
//! The picker (Ctrl+P for files, Ctrl+K for commands) and workspace search
//! (Ctrl+Shift+F) open over everything and have the keyboard until they
//! close. The workspace's files are listed in the background from the
//! start, so the picker can show them right away.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use opentui::{Attributes, Buffer, Rgba};

use crate::document;
use crate::editor::{Action, Editor};
use crate::file_index::FileIndex;
use crate::input::{Key, KeyCode, Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::{Command, Context, Keymap};
use crate::picker::{Choice, Mode, Picker, PickerAction};
use crate::search_modal::{Memory, SearchAction, SearchModal};
use crate::tree::{FileTree, TreeAction};
use crate::workspace::Workspace;

const DIVIDER: Rgba = Rgba::rgb(69, 71, 90);

const DEFAULT_TREE_WIDTH: u32 = 30;
const MIN_TREE_WIDTH: u32 = 12;
/// The tree hides rather than leave the editor narrower than this.
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
    Editor,
}

/// Where a mouse press landed; drags and the release go there too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseTarget {
    Tree,
    Divider,
    Editor,
}

pub struct App {
    workspace: Workspace,
    keymap: Keymap,
    tree: FileTree,
    /// Open files, in the order they were opened. Never empty.
    editors: Vec<Editor>,
    /// The index of the editor on screen.
    current: usize,
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
    /// The editor showing a preview, which the next preview replaces.
    preview: Option<usize>,
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
}

/// A popup's query line, which typing and pasting edit.
trait QueryInput {
    fn insert(&mut self, text: &str);
    fn delete_backward(&mut self);
    fn delete_word_backward(&mut self);
}

impl QueryInput for Picker {
    fn insert(&mut self, text: &str) {
        Picker::insert(self, text);
    }
    fn delete_backward(&mut self) {
        Picker::delete_backward(self);
    }
    fn delete_word_backward(&mut self) {
        Picker::delete_word_backward(self);
    }
}

impl QueryInput for SearchModal {
    fn insert(&mut self, text: &str) {
        SearchModal::insert(self, text);
    }
    fn delete_backward(&mut self) {
        SearchModal::delete_backward(self);
    }
    fn delete_word_backward(&mut self) {
        SearchModal::delete_word_backward(self);
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
        tree.set_height(height);
        if let Some(file) = &file {
            tree.reveal(file);
            tree.set_active(Some(file), false);
        }
        let focus = if file.is_some() {
            Focus::Editor
        } else {
            Focus::Tree
        };
        let editor = Editor::open(file.clone(), width, height).map_err(|reason| match &file {
            Some(file) => format!("{}: {reason}", file.display()),
            None => reason,
        })?;
        let mut app = App {
            files: FileIndex::new(&workspace),
            workspace,
            keymap: Keymap::default(),
            tree,
            editors: vec![editor],
            current: 0,
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
        };
        app.note_recent();
        app.layout();
        Ok(app)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.layout();
    }

    pub fn handle_key(&mut self, key: Key) -> AppAction {
        let action = self.dispatch_key(key);
        self.keep_if_edited();
        action
    }

    fn dispatch_key(&mut self, key: Key) -> AppAction {
        self.editor_mut().clear_message();
        if self.editor().prompt_open() {
            let action = self.editor_mut().handle_prompt_key(key);
            return self.editor_action(action);
        }
        let context = match self.focus {
            _ if self.search.is_some() => Context::Search,
            _ if self.picker.is_some() => Context::Picker,
            Focus::Tree => Context::Tree,
            Focus::Editor => Context::Editor,
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
                    self.editor_mut().type_key(key);
                }
                AppAction::Continue
            }
        }
    }

    /// The query line of the open popup, if any.
    fn query_input(&mut self) -> Option<&mut dyn QueryInput> {
        if let Some(search) = &mut self.search {
            return Some(search);
        }
        self.picker
            .as_mut()
            .map(|picker| picker as &mut dyn QueryInput)
    }

    /// A key for a popup's query: typing, or the editor's keys for deleting
    /// and pasting.
    fn edit_query(&mut self, key: Key) {
        let binding = self.keymap.lookup(key, Context::Editor);
        let clipboard = self.clipboard.clone();
        let Some(input) = self.query_input() else {
            return;
        };
        match binding {
            Some((Command::DeleteBackward, _)) => input.delete_backward(),
            Some((Command::DeleteWordBackward, _)) => input.delete_word_backward(),
            Some((Command::Paste, _)) => {
                if let Some(text) = &clipboard {
                    input.insert(text.lines().next().unwrap_or(""));
                }
            }
            Some(_) => {}
            None => {
                if let KeyCode::Char(c) = key.code {
                    if key.mods.is_plain() {
                        input.insert(c.encode_utf8(&mut [0; 4]));
                    }
                }
            }
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
            command if matches!(command.context(), Context::Picker | Context::Search) => {
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
                let action = self
                    .editors
                    .get_mut(self.current)
                    .expect("an editor is open")
                    .run(command, select, &mut self.clipboard);
                // Saving an unnamed file from the tree asks for a name in
                // the editor's status bar.
                if self.editor().prompt_open() {
                    self.focus = Focus::Editor;
                }
                self.keep_if_edited();
                return self.editor_action(action);
            }
        }
        AppAction::Continue
    }

    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> AppAction {
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
        let tree_width = self.visible_tree_width();
        let at = if tree_width > 0 && mouse.x < tree_width {
            MouseTarget::Tree
        } else if tree_width > 0 && mouse.x == tree_width {
            MouseTarget::Divider
        } else {
            MouseTarget::Editor
        };
        let target = match mouse.kind {
            MouseKind::Press(_) => {
                self.mouse_target = Some(at);
                at
            }
            MouseKind::Drag(_) | MouseKind::Release(_) => match self.mouse_target {
                Some(target) => target,
                None => return AppAction::Continue,
            },
            _ => at,
        };
        if let MouseKind::Release(_) = mouse.kind {
            self.mouse_target = None;
        }
        if let MouseKind::Press(_) = mouse.kind {
            // Like a key press, a click dismisses messages, the quit
            // confirmation, and the "Save as" prompt.
            self.quit_armed = false;
            let editor = self.editor_mut();
            editor.clear_message();
            editor.cancel_prompt();
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
            MouseTarget::Editor => {
                if let MouseKind::Press(MouseButton::Left) = mouse.kind {
                    self.focus = Focus::Editor;
                }
                let x = self.editor_x();
                let local = Mouse {
                    x: mouse.x.saturating_sub(x),
                    ..mouse
                };
                self.editor_mut().handle_mouse(local, now);
            }
        }
        AppAction::Continue
    }

    /// Text pasted through the terminal.
    pub fn paste(&mut self, text: &str) {
        if let Some(input) = self.query_input() {
            // Terminals send newlines in pastes as CR.
            let line = text.split(['\r', '\n']).next().unwrap_or("");
            input.insert(line);
        } else if self.focus == Focus::Editor {
            self.editor_mut().paste(text);
            self.keep_if_edited();
        }
    }

    /// Draws the frame and returns where the terminal cursor goes (0-based
    /// column, row), or `None` to hide it.
    pub fn draw(&self, frame: &Buffer) -> Option<(u32, u32)> {
        frame.clear(Rgba::terminal_default([0, 0, 0]));
        let tree_width = self.visible_tree_width();
        if tree_width > 0 {
            self.tree
                .draw(frame, 0, tree_width, self.focus == Focus::Tree);
            for y in 0..self.height {
                frame.draw_text("│", tree_width, y, DIVIDER, None, Attributes::NONE);
            }
        }
        let cursor = self.editor().draw(frame, &self.keymap, &self.workspace);
        if let Some(search) = &self.search {
            return search.draw(frame);
        }
        if let Some(picker) = &self.picker {
            return picker.draw(frame);
        }
        (self.focus == Focus::Editor).then_some(cursor)
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
                let shown = self.editor().path();
                let recent = self
                    .recent
                    .iter()
                    .filter(|path| Some(path.as_path()) != shown && path.is_file())
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
            .selected_text()
            .filter(|text| !text.contains('\n'));
        // Open files are searched as they are, saved or not.
        let unsaved: HashMap<PathBuf, String> = self
            .editors
            .iter()
            .filter(|editor| editor.is_modified())
            .filter_map(|editor| Some((editor.path()?.to_path_buf(), editor.text())))
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
                    self.editor_mut().select_in_line(line, range);
                    self.focus = Focus::Editor;
                }
            }
        }
        AppAction::Continue
    }

    // --- files ------------------------------------------------------------------

    /// Shows the file at `path`, opening it if it isn't open yet. A new
    /// editor takes the place of an unnamed buffer that was never typed in,
    /// or as a `preview`, of the previous preview. Opening a preview without
    /// `preview` keeps it. Returns false, with a message, if the file can't
    /// be opened.
    fn open(&mut self, path: &Path, preview: bool) -> bool {
        let path = document::resolve(path);
        self.editor_mut().clear_message();
        if let Some(index) = self.find_editor(&path) {
            self.current = index;
            if !preview && self.preview == Some(index) {
                self.preview = None;
            }
        } else {
            let editor = match Editor::open(Some(path.clone()), self.width, self.height) {
                Ok(editor) => editor,
                Err(reason) => {
                    // Reason first: the status bar clips long paths on the right.
                    let message = format!(
                        "Can't open: {reason} ({})",
                        self.workspace.display_path(&path)
                    );
                    self.editor_mut().show_message(message, true);
                    return false;
                }
            };
            let replace = if self.editor().is_blank() {
                Some(self.current)
            } else if preview {
                self.preview
                    .filter(|&index| !self.editors[index].is_modified())
            } else {
                None
            };
            match replace {
                Some(index) => {
                    self.editors[index] = editor;
                    self.current = index;
                }
                None => {
                    self.editors.push(editor);
                    self.current = self.editors.len() - 1;
                }
            }
            if preview {
                self.preview = Some(self.current);
            }
        }
        self.show_active_in_tree();
        self.layout();
        true
    }

    /// An edited preview is kept open.
    fn keep_if_edited(&mut self) {
        if self.preview == Some(self.current) && self.editor().is_modified() {
            self.preview = None;
            self.show_active_in_tree();
        }
    }

    fn show_active_in_tree(&mut self) {
        let path = self.editor().path().map(Path::to_path_buf);
        let preview = self.preview == Some(self.current);
        self.tree.set_active(path.as_deref(), preview);
        self.note_recent();
    }

    /// Moves the file on screen to the front of the recent files.
    fn note_recent(&mut self) {
        let Some(path) = self.editor().path().map(Path::to_path_buf) else {
            return;
        };
        self.recent.retain(|recent| *recent != path);
        self.recent.insert(0, path);
        self.recent.truncate(RECENT_FILES);
    }

    /// The open editor for the file at `path`, however it was named.
    fn find_editor(&self, path: &Path) -> Option<usize> {
        self.editors.iter().position(|editor| {
            editor
                .path()
                .is_some_and(|open| document::same_file(open, path))
        })
    }

    /// Saves the current file to `input` from the "Save as" prompt, relative
    /// to the workspace's first folder, unless another editor has that file.
    fn save_as(&mut self, input: &Path) -> AppAction {
        let base = match self.workspace.roots().first() {
            Some(root) => root.clone(),
            None => std::env::current_dir().unwrap_or_default(),
        };
        let path = document::resolve(&base.join(input));
        if self
            .find_editor(&path)
            .is_some_and(|index| index != self.current)
        {
            let message = format!(
                "Can't save: {} is already open.",
                self.workspace.display_path(&path)
            );
            self.editor_mut().show_message(message, true);
            return AppAction::Continue;
        }
        let action = self.editor_mut().save_as(path);
        self.editor_action(action)
    }

    fn quit(&mut self) -> AppAction {
        let unsaved: Vec<String> = self
            .editors
            .iter()
            .filter(|editor| editor.is_modified())
            .map(|editor| match editor.path() {
                Some(path) => self.workspace.display_path(path),
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
        self.editor_mut().show_message(message, true);
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

    /// The editor's left column: right of the tree and its divider.
    fn editor_x(&self) -> u32 {
        match self.visible_tree_width() {
            0 => 0,
            width => width + 1,
        }
    }

    fn layout(&mut self) {
        if self.visible_tree_width() == 0 && self.focus == Focus::Tree {
            self.focus = Focus::Editor;
        }
        self.tree.set_height(self.height);
        if let Some(picker) = &mut self.picker {
            picker.set_size(self.width, self.height);
        }
        if let Some(search) = &mut self.search {
            search.set_size(self.width, self.height);
        }
        let x = self.editor_x();
        let width = self.width.saturating_sub(x).max(1);
        let height = self.height;
        self.editor_mut().set_area(x, width, height);
    }

    fn editor(&self) -> &Editor {
        &self.editors[self.current]
    }

    fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editors[self.current]
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
        assert!(app.editor().is_blank(), "typing in the tree doesn't edit");

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.editor().path(), Some(root.join("b.txt").as_path()));
        assert_eq!(app.editors.len(), 1, "the blank buffer was replaced");
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
        assert_eq!(app.editor().path(), Some(root.join("b.txt").as_path()));
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.editors.len(), 2);
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
        assert!(app.editor().is_modified());
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
            app.editor().path(),
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
        // The editor draws right of the divider.
        let line = screen(&app).lines().next().unwrap().to_string();
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
        assert!(app.editor().prompt_open());
        let path = root.join("new.txt");
        type_text(&mut app, path.to_str().unwrap());
        key(&mut app, KeyCode::Enter);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        assert!(screen(&app).contains("new.txt"));
        app.tree.run(Command::TreeLast);
        ctrl(&mut app, 'e');
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.editors.len(), 1, "reopening switches to the open file");
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
        assert_eq!(app.editors.len(), 1);
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
        assert_eq!(app.editor().path(), Some(a.as_path()));
        assert!(
            screen(&app).contains("▾ dir"),
            "revealed without `..` folders"
        );

        for other in ["linked/a.txt", "link.txt", "dir/a.txt"] {
            assert!(app.open(&root.join(other), false));
            assert_eq!(app.editors.len(), 1, "{other}");
        }
    }

    #[test]
    fn save_as_refuses_a_file_open_in_another_editor() {
        let _serial = crate::test_serial();
        let root = fixture("save-as-open", &[("a.txt", "original")]);
        let mut app = app(&root, Some("a.txt"));
        app.editors.push(Editor::open(None, 80, 10).unwrap());
        app.current = 1;
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
        assert!(app.editor().is_blank());
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

    fn open_paths(app: &App) -> Vec<String> {
        app.editors
            .iter()
            .map(|editor| {
                let path = editor.path().unwrap();
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
        assert_eq!(app.preview, Some(0));
        assert!(app.tree.active_is_preview());

        // Enter keeps the preview open.
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.preview, None);
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
        assert_eq!(app.preview, None);
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
        assert_eq!(app.preview, None);
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
        assert_eq!(app.preview, Some(0), "a single click previews");
        assert_eq!(app.focus, Focus::Tree);
        click(&mut app, 1, t0 + Duration::from_millis(100));
        assert_eq!(app.preview, None, "a double click keeps it");
        assert_eq!(app.focus, Focus::Editor, "and moves focus to it");
        // Slow clicks are two single clicks.
        click(&mut app, 2, t0 + Duration::from_secs(2));
        click(&mut app, 2, t0 + Duration::from_secs(3));
        assert_eq!(app.preview, Some(1));
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
            app.editor().path(),
            Some(root.join("a.txt").as_path()),
            "typing doesn't edit"
        );
        assert!(!app.editor().is_modified());

        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert!(screen(&app).contains("magic"));

        // The previous file is listed first, so Ctrl+P, Enter switches back.
        go_to_file(&mut app);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.editor().path(), Some(root.join("a.txt").as_path()));
        assert_eq!(app.editors.len(), 2);
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
        assert_eq!(app.editor().path(), Some(root.join("a.txt").as_path()));
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
        assert!(!app.editor().is_modified(), "typing doesn't edit");

        key(&mut app, KeyCode::Enter);
        assert!(app.search.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.editor().path(), Some(root.join("src/b.rs").as_path()));
        assert_eq!(app.editor().selected_text().as_deref(), Some("needle"));

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
        app.editor_mut().select_in_line(0, 0..3);
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
        assert!(app.editor().is_modified(), "the edit is still there");
    }
}
