//! Editor state and key bindings on top of OpenTUI's `EditBuffer` and
//! `EditorView`, which own the text, cursor, and scrolling.

use std::path::PathBuf;

use opentui::{Attributes, Buffer, EditBuffer, EditorView, Rgba, WrapMode};

use crate::document::{self, LineEnding};
use crate::input::{Key, KeyCode, Mods};

const STATUS_BG: Rgba = Rgba::rgb(49, 50, 68);
const STATUS_FG: Rgba = Rgba::rgb(205, 214, 244);
const STATUS_DIM: Rgba = Rgba::rgb(147, 153, 178);
const STATUS_ERROR_BG: Rgba = Rgba::rgb(180, 60, 80);

pub enum Action {
    Continue,
    Quit,
}

/// The file being edited.
pub struct File {
    /// `None` until the buffer is first saved.
    pub path: Option<PathBuf>,
    pub line_ending: LineEnding,
}

struct Message {
    text: String,
    error: bool,
}

/// The "Save as" line editor shown in the status bar.
struct Prompt {
    input: String,
}

pub struct Editor<'eb> {
    buffer: &'eb EditBuffer,
    view: EditorView<'eb>,
    file: File,
    modified: bool,
    wrap: WrapMode,
    width: u32,
    height: u32,
    message: Option<Message>,
    prompt: Option<Prompt>,
    /// Ctrl+Q was pressed with unsaved changes; a second press quits.
    quit_armed: bool,
}

impl<'eb> Editor<'eb> {
    /// An editor filling a `width` x `height` screen, with the last row used
    /// for the status bar.
    pub fn new(
        buffer: &'eb EditBuffer,
        file: File,
        width: u32,
        height: u32,
    ) -> opentui::Result<Editor<'eb>> {
        let (view_w, view_h) = text_area(width, height);
        let view = buffer.view(view_w, view_h)?;
        let wrap = WrapMode::None;
        view.set_wrap_mode(wrap);
        Ok(Editor {
            buffer,
            view,
            file,
            modified: false,
            wrap,
            width,
            height,
            message: None,
            prompt: None,
            quit_armed: false,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let (view_w, view_h) = text_area(width, height);
        self.view.set_viewport_size(view_w, view_h);
    }

    /// Shows `text` in the status bar until the next key press.
    pub fn show_message(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: text.into(),
            error,
        });
    }

    pub fn handle_key(&mut self, key: Key) -> Action {
        self.message = None;
        if self.prompt.is_some() {
            self.handle_prompt_key(key);
            return Action::Continue;
        }

        let Key { code, mods } = key;
        let quit_armed = std::mem::take(&mut self.quit_armed);
        let eb = self.buffer;
        match code {
            KeyCode::Char('q' | 'c') if mods.ctrl => {
                if !self.modified || quit_armed {
                    return Action::Quit;
                }
                self.quit_armed = true;
                self.show_message("Unsaved changes. ^Q again to quit, ^S to save.", true);
            }
            KeyCode::Char('s') if mods.ctrl => self.save(),
            KeyCode::Char('w') if mods.ctrl => self.toggle_wrap(),
            KeyCode::Char(c) if mods == Mods::NONE || mods == SHIFT => {
                let mut utf8 = [0u8; 4];
                self.edit(|eb| eb.insert_text(c.encode_utf8(&mut utf8)));
            }
            KeyCode::Enter => self.edit(EditBuffer::new_line),
            KeyCode::Tab if !mods.shift => self.edit(|eb| eb.insert_text("\t")),
            KeyCode::Backspace => self.edit(EditBuffer::delete_char_backward),
            KeyCode::Delete => self.edit(EditBuffer::delete_char),
            KeyCode::Left => eb.move_cursor_left(),
            KeyCode::Right => eb.move_cursor_right(),
            KeyCode::Up => self.view.move_up_visual(),
            KeyCode::Down => self.view.move_down_visual(),
            KeyCode::Home => self.view.move_to_visual_line_start(),
            KeyCode::End => self.view.move_to_visual_line_end(),
            KeyCode::PageUp => (0..self.page()).for_each(|_| self.view.move_up_visual()),
            KeyCode::PageDown => (0..self.page()).for_each(|_| self.view.move_down_visual()),
            _ => {}
        }
        Action::Continue
    }

    pub fn paste(&mut self, text: &str) {
        // Terminals send newlines in pastes as CR.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        match &mut self.prompt {
            // Only the first line makes sense in a file name.
            Some(prompt) => prompt.input.push_str(text.lines().next().unwrap_or("")),
            None => self.edit(|eb| eb.insert_text(&text)),
        }
    }

    /// Draws the frame and returns the terminal cursor position (0-based
    /// column, row).
    pub fn draw(&self, frame: &Buffer) -> (u32, u32) {
        frame.clear(Rgba::terminal_default([0, 0, 0]));
        frame.draw_editor_view(&self.view, 0, 0);
        match self.draw_status(frame) {
            Some(prompt_cursor) => prompt_cursor,
            None => {
                let cursor = self.view.visual_cursor();
                (cursor.col, cursor.row)
            }
        }
    }

    /// Returns the cursor position when the prompt has focus.
    fn draw_status(&self, frame: &Buffer) -> Option<(u32, u32)> {
        if self.height < 2 {
            return None;
        }
        let y = self.height - 1;

        if let Some(prompt) = &self.prompt {
            frame.fill_rect(0, y, self.width, 1, STATUS_BG);
            let label = " Save as: ";
            frame.draw_text(label, 0, y, STATUS_DIM, None, Attributes::NONE);
            let x = label.len() as u32;
            // Keep the end of a long path visible.
            let room = self.width.saturating_sub(x + 1) as usize;
            let chars: Vec<char> = prompt.input.chars().collect();
            let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
            frame.draw_text(&shown, x, y, STATUS_FG, None, Attributes::NONE);
            return Some((x + shown.chars().count() as u32, y));
        }

        if let Some(message) = &self.message {
            let bg = if message.error {
                STATUS_ERROR_BG
            } else {
                STATUS_BG
            };
            frame.fill_rect(0, y, self.width, 1, bg);
            frame.draw_text(
                &format!(" {}", message.text),
                0,
                y,
                STATUS_FG,
                None,
                Attributes::BOLD,
            );
            return None;
        }

        frame.fill_rect(0, y, self.width, 1, STATUS_BG);
        let cursor = self.buffer.cursor();
        let name = match &self.file.path {
            Some(path) => path.display().to_string(),
            None => "[new file]".to_string(),
        };
        let dirty = if self.modified { " [+]" } else { "" };
        let wrap = match self.wrap {
            WrapMode::None => "nowrap",
            _ => "wrap",
        };
        let info = format!(
            "{dirty}  Ln {}, Col {}  {}  {wrap}",
            cursor.row + 1,
            cursor.col + 1,
            self.file.line_ending.label(),
        );
        // Shorten the path from the left so the rest of the status stays visible.
        let room = (self.width as usize).saturating_sub(info.chars().count() + 1);
        let left = format!(" {}{info}", truncate_left(&name, room));
        frame.draw_text(&left, 0, y, STATUS_FG, None, Attributes::BOLD);
        let hints = "^S save  ^W wrap  ^Q quit ";
        let hints_x = self.width.saturating_sub(hints.len() as u32);
        if hints_x as usize > left.chars().count() {
            frame.draw_text(hints, hints_x, y, STATUS_DIM, None, Attributes::NONE);
        }
        None
    }

    fn edit(&mut self, f: impl FnOnce(&EditBuffer)) {
        f(self.buffer);
        self.modified = true;
    }

    fn save(&mut self) {
        let Some(path) = self.file.path.clone() else {
            self.prompt = Some(Prompt {
                input: String::new(),
            });
            return;
        };
        let text = self.buffer.text();
        match document::save(&path, &text, self.file.line_ending) {
            Ok(()) => {
                self.modified = false;
                let lines = self.buffer.line_count();
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                self.show_message(format!("Wrote {name} ({lines} lines)"), false);
            }
            // Reason first: the status bar clips long paths on the right.
            Err(err) => self.show_message(format!("Can't save: {err} ({})", path.display()), true),
        }
    }

    fn handle_prompt_key(&mut self, Key { code, mods }: Key) {
        let prompt = self.prompt.as_mut().expect("prompt is open");
        match code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Char('c' | 'q') if mods.ctrl => self.prompt = None,
            KeyCode::Enter => {
                let input = prompt.input.trim().to_string();
                self.prompt = None;
                if !input.is_empty() {
                    self.file.path = Some(PathBuf::from(input));
                    self.save();
                }
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Char(c) if !mods.ctrl && !mods.alt => prompt.input.push(c),
            _ => {}
        }
    }

    fn toggle_wrap(&mut self) {
        self.wrap = match self.wrap {
            WrapMode::None => WrapMode::Word,
            _ => WrapMode::None,
        };
        self.view.set_wrap_mode(self.wrap);
    }

    fn page(&self) -> u32 {
        text_area(self.width, self.height)
            .1
            .saturating_sub(1)
            .max(1)
    }
}

const SHIFT: Mods = Mods {
    shift: true,
    alt: false,
    ctrl: false,
};

/// The last `max` characters of `s`, marked with a leading ellipsis if cut.
fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("…{tail}")
}

/// The text area: everything but the status bar row.
fn text_area(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.saturating_sub(1).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentui::{OwnedBuffer, WidthMethod};
    use std::fs;
    use std::sync::{Mutex, MutexGuard};

    /// The native core is single-threaded and the harness runs tests in parallel.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn press(editor: &mut Editor, keys: &str) {
        for c in keys.chars() {
            editor.handle_key(Key::new(KeyCode::Char(c), Mods::NONE));
        }
    }

    fn ctrl(editor: &mut Editor, c: char) -> Action {
        editor.handle_key(Key::new(KeyCode::Char(c), Mods::CTRL))
    }

    fn status(editor: &Editor) -> String {
        let screen = OwnedBuffer::new(60, 4, false, WidthMethod::Unicode, "test").unwrap();
        editor.draw(&screen);
        screen
            .to_text(true)
            .lines()
            .nth(3)
            .unwrap()
            .trim_end()
            .to_string()
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qedit-editor-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn save_writes_the_file_and_clears_modified() {
        let _serial = serial();
        let path = temp_path("save.txt");
        let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
        eb.set_text("one\r\n".replace("\r\n", "\n").as_str());
        let file = File {
            path: Some(path.clone()),
            line_ending: LineEnding::CrLf,
        };
        let mut editor = Editor::new(&eb, file, 60, 4).unwrap();
        assert!(!status(&editor).contains("[+]"));

        eb.set_cursor(1, 0);
        press(&mut editor, "two");
        assert!(
            status(&editor).contains("save.txt [+]"),
            "{}",
            status(&editor)
        );

        ctrl(&mut editor, 's');
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\r\ntwo");
        assert!(
            status(&editor).starts_with(" Wrote "),
            "{}",
            status(&editor)
        );
        press(&mut editor, "");
        editor.handle_key(Key::new(KeyCode::Left, Mods::NONE));
        assert!(!status(&editor).contains("[+]"), "{}", status(&editor));
    }

    #[test]
    fn save_as_prompt_names_a_new_file() {
        let _serial = serial();
        let path = temp_path("prompted.txt");
        let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
        let file = File {
            path: None,
            line_ending: LineEnding::Lf,
        };
        let mut editor = Editor::new(&eb, file, 60, 4).unwrap();
        press(&mut editor, "hi");
        ctrl(&mut editor, 's');
        assert!(status(&editor).starts_with(" Save as:"));

        // Typing goes to the prompt, not the buffer; Esc cancels.
        press(&mut editor, "zzz");
        editor.handle_key(Key::new(KeyCode::Esc, Mods::NONE));
        assert_eq!(eb.text(), "hi");

        ctrl(&mut editor, 's');
        editor.paste(path.to_str().unwrap());
        editor.handle_key(Key::new(KeyCode::Enter, Mods::NONE));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        assert!(status(&editor).starts_with(" Wrote "));
    }

    #[test]
    fn quit_asks_again_only_with_unsaved_changes() {
        let _serial = serial();
        let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
        let file = || File {
            path: None,
            line_ending: LineEnding::Lf,
        };
        let mut clean = Editor::new(&eb, file(), 60, 4).unwrap();
        assert!(matches!(ctrl(&mut clean, 'q'), Action::Quit));
        drop(clean);

        let mut dirty = Editor::new(&eb, file(), 60, 4).unwrap();
        press(&mut dirty, "x");
        assert!(matches!(ctrl(&mut dirty, 'q'), Action::Continue));
        assert!(status(&dirty).contains("Unsaved changes"));
        // Any other key disarms the confirmation.
        press(&mut dirty, "y");
        assert!(matches!(ctrl(&mut dirty, 'q'), Action::Continue));
        assert!(matches!(ctrl(&mut dirty, 'q'), Action::Quit));
    }
}
