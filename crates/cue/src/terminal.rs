//! A terminal: a shell running in a pty, and the emulator that keeps its
//! screen, shown in a panel.
//!
//! Like open files, terminals belong to the app and outlive the panels that
//! show them, but a terminal is in at most one panel at a time: its pty has
//! one size. Keys cue doesn't keep for itself go to the shell (see
//! [`crate::keymap::Keymap::lookup_terminal`]), encoded as the program in
//! the foreground asked for.
//!
//! The mouse goes to the program if it asked for it (as vim and htop do),
//! unless Shift is held. Otherwise dragging selects text, and the wheel
//! scrolls back through the output, or in a full-screen program that
//! didn't ask for the mouse, sends arrow keys, as terminals do.
//!
//! When the shell exits, the terminal keeps its last screen until Enter
//! starts a new shell.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use opentui::{
    Buffer, EmbeddedTerminal, KeyEvent, KeyMods, MouseAction, MouseButton as TermButton, MouseEvent,
};

use crate::input::{Key, KeyCode, Mods, Mouse, MouseButton, MouseKind};
use crate::layout::Rect;
use crate::pty::Pty;
use crate::status::{Prompt, PromptKey, Status};

/// Bytes of history kept above the screen.
const SCROLLBACK: u32 = 10 * 1024 * 1024;
/// At most this much output is read per poll, so a flood of it can't keep
/// the screen from being drawn, or keys from being read.
const READ_BUDGET: usize = 256 * 1024;
/// Rows the wheel scrolls.
const WHEEL_ROWS: i32 = 3;

pub struct Terminal {
    /// Numbers terminals, from 1, in the order they were started.
    id: u32,
    /// The name it was given, if it was renamed.
    name: Option<String>,
    /// The prompt for a new name, while it's open.
    prompt: Option<Prompt>,
    vt: EmbeddedTerminal,
    pty: Pty,
    cwd: PathBuf,
    /// The screen's size and place; the pty's size too.
    area: Rect,
    /// The shell ended, and everything it started closed the pty.
    closed: bool,
    exit: Option<ExitStatus>,
    /// The program is getting the mouse, from a press to its release.
    forwarding_mouse: bool,
    /// Where a drag to select text started.
    selecting_from: Option<(u16, u16)>,
}

impl Terminal {
    /// Starts a shell in `cwd`, on a screen the size of `area`.
    pub fn new(id: u32, cwd: &Path, area: Rect) -> io::Result<Terminal> {
        let (cols, rows) = size(area);
        let mut vt = EmbeddedTerminal::new(cols, rows, SCROLLBACK)
            .map_err(|e| io::Error::other(e.to_string()))?;
        vt.set_host_palette(true);
        let pty = Pty::shell(cwd, cols, rows)?;
        Ok(Terminal {
            id,
            name: None,
            prompt: None,
            vt,
            pty,
            cwd: cwd.to_path_buf(),
            area,
            closed: false,
            exit: None,
            forwarding_mouse: false,
            selecting_from: None,
        })
    }

    /// The folder it started in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Starts a new shell, after the last one exited, on a clear screen
    /// with none of the modes the last one left on.
    pub fn restart(&mut self) -> io::Result<()> {
        let (cols, rows) = size(self.area);
        self.pty = Pty::shell(&self.cwd, cols, rows)?;
        self.closed = false;
        self.exit = None;
        let _ = self.vt.write(b"\x1bc");
        Ok(())
    }

    /// What to poll for this terminal, if anything: its pty, and whether
    /// input is waiting to be taken.
    pub fn watch(&self) -> Option<(i32, bool)> {
        (!self.closed).then(|| (self.pty.fd(), self.pty.has_unsent()))
    }

    /// Reads the shell's output into the screen and answers its queries.
    /// Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        self.pty.flush();
        let mut buf = [0u8; 64 * 1024];
        let mut read = 0;
        while !self.closed && read < READ_BUDGET {
            match self.pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    read += n;
                    // Anything it can't make sense of is dropped, as
                    // terminals do.
                    let _ = self.vt.write(&buf[..n]);
                    changed = true;
                }
                Err(_) => self.closed = true,
            }
        }
        let responses = self.vt.drain_responses();
        if !responses.is_empty() {
            self.pty.write(&responses);
        }
        if self.exit.is_none() {
            // A shell can exit leaving jobs that hold the pty open.
            if let Some(status) = self.pty.exit_status() {
                self.exit = Some(status);
                changed = true;
            }
        }
        changed
    }

    /// The text the program put on the clipboard (OSC 52) since the last
    /// call, if any. Programs do this over ssh, where they can't reach the
    /// system clipboard themselves.
    pub fn take_copied(&mut self) -> Option<String> {
        self.vt.take_clipboard()
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// What to call it: the name it was given, or "Terminal 2".
    pub fn name(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => format!("Terminal {}", self.id),
        }
    }

    /// Whether it was given a name.
    pub fn is_renamed(&self) -> bool {
        self.name.is_some()
    }

    /// Asks for a new name in the status bar, starting from the one it
    /// was given, if any.
    pub fn show_rename(&mut self) {
        let name = self.name.as_deref().unwrap_or_default();
        self.prompt = Some(Prompt::new("Rename terminal", name));
    }

    /// The rename prompt has the keyboard.
    pub fn prompt_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
    }

    /// A key for the rename prompt, while it's open. An empty name goes
    /// back to the numbered one.
    pub fn handle_prompt_key(&mut self, key: Key) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        match prompt.handle_key(key) {
            PromptKey::Continue => {}
            PromptKey::Cancel => self.prompt = None,
            PromptKey::Submit(name) => {
                self.prompt = None;
                self.name = (!name.is_empty()).then_some(name);
            }
        }
    }

    /// How the shell ended, once it has.
    pub fn exit(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// Whether a program the shell started is running in it.
    pub fn is_busy(&self) -> bool {
        self.exit.is_none() && self.pty.is_busy()
    }

    /// The program the shell is running, or the shell, such as `vim`.
    pub fn program(&self) -> Option<String> {
        if self.exit.is_some() {
            return None;
        }
        self.pty.foreground_name()
    }

    /// The title the program set, if any.
    pub fn title(&self) -> String {
        self.vt.title()
    }

    pub fn set_area(&mut self, area: Rect) {
        let old = size(self.area);
        self.area = area;
        let (cols, rows) = size(area);
        if (cols, rows) != old && self.vt.resize(cols, rows).is_ok() {
            self.pty.resize(cols, rows);
            self.selecting_from = None;
        }
    }

    /// Sends `key` to the program, and brings the view back to the live
    /// screen.
    pub fn send_key(&mut self, key: Key) {
        let Some(event) = key_event(key) else {
            return;
        };
        let bytes = self.vt.encode_key(&event.as_event());
        self.send(&bytes);
    }

    /// Pastes `text` into the program, or into the rename prompt while
    /// it's open.
    pub fn paste(&mut self, text: &str) {
        if let Some(prompt) = &mut self.prompt {
            prompt.paste(text);
            return;
        }
        let bytes = self.vt.encode_paste(text);
        self.send(&bytes);
    }

    /// Tells the program it got or lost the keyboard, if it asked to know.
    pub fn focus(&mut self, focused: bool) {
        let bytes = self.vt.encode_focus(focused);
        if !bytes.is_empty() && !self.closed {
            self.pty.write(&bytes);
        }
    }

    /// Clears the screen and the history above it, as Cmd+K does in macOS
    /// terminals. A shell at its prompt redraws it, keeping what was typed.
    /// A full-screen program keeps its screen.
    pub fn clear(&mut self) {
        self.vt.clear_selection();
        self.vt.scroll_to_bottom();
        if self.vt.is_alternate_screen() {
            return;
        }
        // Home, erase the screen, then the history, so nothing erased is
        // kept in it.
        let _ = self.vt.write(b"\x1b[H\x1b[2J\x1b[3J");
        if self.exit.is_none() && !self.pty.is_busy() {
            // Ctrl+L: the shell clears the screen and redraws its prompt.
            self.pty.write(b"\x0c");
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.closed {
            return;
        }
        self.vt.clear_selection();
        self.vt.scroll_to_bottom();
        self.pty.write(bytes);
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        Some(self.vt.selected_text()).filter(|text| !text.is_empty())
    }

    /// A mouse event at screen cell (`mouse.x`, `mouse.y`), in the
    /// terminal or dragged from it.
    pub fn handle_mouse(&mut self, mouse: Mouse) {
        let (cols, rows) = size(self.area);
        let x = mouse.x.saturating_sub(self.area.x).min(cols as u32 - 1);
        let y = mouse.y.saturating_sub(self.area.y).min(rows as u32 - 1);
        let mods = key_mods(mouse.mods);
        let event = |action, button, any_button_pressed| MouseEvent {
            action,
            button,
            mods,
            x,
            y,
            any_button_pressed,
        };
        // Shift keeps the mouse for selecting, as in other terminals.
        let offer = !mouse.mods.shift && !self.closed;
        match mouse.kind {
            MouseKind::Press(button) => {
                self.forwarding_mouse = false;
                if offer {
                    let event = event(MouseAction::Press, term_button(button), true);
                    let bytes = self.vt.encode_mouse(&event);
                    if !bytes.is_empty() {
                        self.forwarding_mouse = true;
                        self.pty.write(&bytes);
                        return;
                    }
                }
                self.vt.clear_selection();
                self.selecting_from = (button == MouseButton::Left).then_some((x as u16, y as u16));
            }
            MouseKind::Drag(button) if self.forwarding_mouse => {
                let event = event(MouseAction::Motion, term_button(button), true);
                let bytes = self.vt.encode_mouse(&event);
                self.pty.write(&bytes);
            }
            MouseKind::Drag(_) => {
                if let Some(from) = self.selecting_from {
                    let _ = self.vt.set_selection(from, (x as u16, y as u16));
                }
            }
            MouseKind::Release(button) if self.forwarding_mouse => {
                self.forwarding_mouse = false;
                let event = event(MouseAction::Release, term_button(button), false);
                let bytes = self.vt.encode_mouse(&event);
                self.pty.write(&bytes);
            }
            MouseKind::Release(_) => self.selecting_from = None,
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let up = mouse.kind == MouseKind::ScrollUp;
                if offer {
                    let button = if up {
                        TermButton::Four
                    } else {
                        TermButton::Five
                    };
                    let bytes =
                        self.vt
                            .encode_mouse(&event(MouseAction::Press, Some(button), false));
                    if !bytes.is_empty() {
                        self.pty.write(&bytes);
                        return;
                    }
                }
                if self.vt.is_alternate_screen() {
                    // Full-screen programs have no scrollback; less and man
                    // scroll with the arrow keys.
                    let code = if up { KeyCode::Up } else { KeyCode::Down };
                    for _ in 0..WHEEL_ROWS {
                        self.send_key(Key::new(code, Mods::NONE));
                    }
                } else {
                    self.vt.scroll(if up { -WHEEL_ROWS } else { WHEEL_ROWS });
                }
            }
            MouseKind::ScrollLeft | MouseKind::ScrollRight | MouseKind::Move => {}
        }
    }

    /// Draws the screen in its area, and returns where the terminal cursor
    /// goes if `focused`.
    pub fn draw(&self, frame: &Buffer, focused: bool) -> Option<(u32, u32)> {
        let area = self.area;
        self.vt.draw(frame, area.x as i32, area.y as i32);
        let cursor = self.vt.cursor();
        let (x, y) = cursor.position?;
        (focused && cursor.visible && self.exit.is_none())
            .then_some((area.x + x as u32, area.y + y as u32))
    }

    /// What the status bar shows while this terminal is in the active panel.
    pub fn status(&self) -> Status {
        if let Some(prompt) = &self.prompt {
            return prompt.status();
        }
        match self.exit {
            Some(status) => Status::Message {
                text: format!("{}. Enter starts a new shell.", describe_exit(status)),
                error: false,
            },
            None => match self.program() {
                Some(program) => Status::Terminal(format!("{}  {program}", self.name())),
                None => Status::Terminal(self.name()),
            },
        }
    }
}

/// "The shell exited", with its code or signal if it failed.
pub fn describe_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => "The shell exited".to_string(),
        (Some(code), _) => format!("The shell exited with code {code}"),
        (None, Some(signal)) => format!("The shell was ended by signal {signal}"),
        (None, None) => "The shell exited".to_string(),
    }
}

fn size(area: Rect) -> (u16, u16) {
    let clamp = |n: u32| n.clamp(1, u16::MAX as u32) as u16;
    (clamp(area.width), clamp(area.height))
}

fn key_mods(mods: Mods) -> KeyMods {
    let mut out = KeyMods::NONE;
    for (held, bit) in [
        (mods.shift, KeyMods::SHIFT),
        (mods.ctrl, KeyMods::CTRL),
        (mods.alt, KeyMods::ALT),
        (mods.sup, KeyMods::SUPER),
    ] {
        if held {
            out |= bit;
        }
    }
    out
}

/// The button as programs are told of it; cue keeps the back and forward
/// buttons.
fn term_button(button: MouseButton) -> Option<TermButton> {
    match button {
        MouseButton::Left => Some(TermButton::Left),
        MouseButton::Middle => Some(TermButton::Middle),
        MouseButton::Right => Some(TermButton::Right),
        MouseButton::Back | MouseButton::Forward => None,
    }
}

/// A key as the emulator takes it: the physical key, its modifiers, and
/// the text it types.
#[derive(Debug, PartialEq, Eq)]
struct KeyInput {
    code: String,
    mods: KeyMods,
    text: String,
    unshifted: Option<char>,
}

impl KeyInput {
    fn as_event(&self) -> KeyEvent<'_> {
        KeyEvent::press(&self.code, self.mods, &self.text, self.unshifted)
    }
}

fn key_event(key: Key) -> Option<KeyInput> {
    let mods = key_mods(key.mods);
    let named = |code: &str| KeyInput {
        code: code.to_string(),
        mods,
        text: String::new(),
        unshifted: None,
    };
    Some(match key.code {
        KeyCode::Char(c) => {
            let base = c.to_ascii_lowercase();
            // With Shift reported separately (Ctrl+Shift+C), the text is
            // the shifted letter, as the keyboard would type it.
            let typed = if key.mods.shift {
                c.to_ascii_uppercase()
            } else {
                c
            };
            KeyInput {
                code: char_code(base).unwrap_or_default(),
                mods,
                text: typed.to_string(),
                unshifted: char_code(base).is_some().then_some(base),
            }
        }
        KeyCode::Enter => named("Enter"),
        KeyCode::Tab => named("Tab"),
        KeyCode::Backspace => named("Backspace"),
        KeyCode::Delete => named("Delete"),
        KeyCode::Insert => named("Insert"),
        KeyCode::Esc => named("Escape"),
        KeyCode::Left => named("ArrowLeft"),
        KeyCode::Right => named("ArrowRight"),
        KeyCode::Up => named("ArrowUp"),
        KeyCode::Down => named("ArrowDown"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::F(n) => named(&format!("F{n}")),
    })
}

/// The W3C code of the US-layout key that types `c` unshifted.
fn char_code(c: char) -> Option<String> {
    let code = match c {
        'a'..='z' => return Some(format!("Key{}", c.to_ascii_uppercase())),
        '0'..='9' => return Some(format!("Digit{c}")),
        ' ' => "Space",
        '-' => "Minus",
        '=' => "Equal",
        '[' => "BracketLeft",
        ']' => "BracketRight",
        '\\' => "Backslash",
        ';' => "Semicolon",
        '\'' => "Quote",
        ',' => "Comma",
        '.' => "Period",
        '/' => "Slash",
        '`' => "Backquote",
        _ => return None,
    };
    Some(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn encode(term: &Terminal, key: Key) -> Vec<u8> {
        term.vt.encode_key(&key_event(key).unwrap().as_event())
    }

    fn key(code: KeyCode, mods: Mods) -> Key {
        Key::new(code, mods)
    }

    #[test]
    fn keys_encode_as_a_terminal_sends_them() {
        let _serial = crate::test_serial();
        let root = std::env::temp_dir();
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 5,
        };
        let term = Terminal::new(1, &root, area).unwrap();
        let ctrl_shift = Mods {
            shift: true,
            ..Mods::CTRL
        };
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        for (key, bytes) in [
            (key(KeyCode::Char('a'), Mods::NONE), &b"a"[..]),
            (key(KeyCode::Char('A'), Mods::NONE), b"A"),
            (key(KeyCode::Char('é'), Mods::NONE), "é".as_bytes()),
            (key(KeyCode::Char('c'), Mods::CTRL), b"\x03"),
            (key(KeyCode::Char('\\'), Mods::CTRL), b"\x1c"),
            (key(KeyCode::Char(']'), Mods::CTRL), b"\x1d"),
            (key(KeyCode::Char(' '), Mods::CTRL), b"\x00"),
            (key(KeyCode::Char('b'), alt), b"\x1bb"),
            (key(KeyCode::Char('x'), ctrl_shift), b"\x1b[120;6u"),
            (key(KeyCode::Enter, Mods::NONE), b"\r"),
            (key(KeyCode::Backspace, Mods::NONE), b"\x7f"),
            (key(KeyCode::Backspace, alt), b"\x1b\x7f"),
            (key(KeyCode::Tab, Mods::SHIFT), b"\x1b[Z"),
            (key(KeyCode::Esc, Mods::NONE), b"\x1b"),
            (key(KeyCode::Up, Mods::NONE), b"\x1b[A"),
            (key(KeyCode::Left, alt), b"\x1b[1;3D"),
            (key(KeyCode::PageDown, Mods::NONE), b"\x1b[6~"),
            (key(KeyCode::F(1), Mods::NONE), b"\x1bOP"),
            (key(KeyCode::F(5), Mods::NONE), b"\x1b[15~"),
        ] {
            assert_eq!(encode(&term, key), bytes, "{key:?}");
        }
    }

    /// What's on screen, as text.
    fn screen(term: &Terminal) -> String {
        let frame = opentui::OwnedBuffer::new(
            term.area.width,
            term.area.height,
            false,
            opentui::WidthMethod::Unicode,
            "test",
        )
        .unwrap();
        term.draw(&frame, true);
        frame.to_text(true)
    }

    /// Polls until a line on screen is `line`, or panics after a few
    /// seconds.
    fn wait_for(term: &mut Terminal, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            term.poll();
            let screen = screen(term);
            if screen.lines().any(|shown| shown.trim_end() == line) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no {line:?} on screen:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn runs_a_shell_that_takes_input_and_exits() {
        let _serial = crate::test_serial();
        // Short enough not to wrap.
        let root = Path::new("/tmp").canonicalize().unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 6,
        };
        let mut term = Terminal::new(1, &root, area).unwrap();
        // Whatever the login shell prints, it's at a prompt eventually.
        for c in "echo $((6*7)) $TERM $(stty size); pwd".chars() {
            term.send_key(key(KeyCode::Char(c), Mods::NONE));
        }
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "42 xterm-256color 6 40");
        wait_for(&mut term, &root.display().to_string());

        // The pty follows the panel's size.
        term.set_area(Rect {
            width: 30,
            height: 8,
            ..area
        });
        // A shell with bracketed paste on waits for Enter.
        term.paste("stty size");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "8 30");
        assert!(term.exit().is_none());

        term.paste("exit 3");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        let deadline = Instant::now() + Duration::from_secs(10);
        while term.exit().is_none() {
            assert!(Instant::now() < deadline, "the shell didn't exit");
            term.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(term.exit().unwrap().code(), Some(3));
        assert_eq!(
            term.status(),
            Status::Message {
                text: "The shell exited with code 3. Enter starts a new shell.".into(),
                error: false
            }
        );

        term.restart().unwrap();
        term.paste("echo again");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "again");
    }

    #[test]
    fn clearing_drops_the_screen_and_history_but_keeps_the_prompt() {
        let _serial = crate::test_serial();
        let root = Path::new("/tmp").canonicalize().unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 6,
        };
        let mut term = Terminal::new(1, &root, area).unwrap();
        // More than a screenful, so some goes into the history.
        term.paste("seq 1 20; echo do''ne");
        term.send_key(key(KeyCode::Enter, Mods::NONE));
        wait_for(&mut term, "done");
        // Typed at the prompt, and still there after.
        term.paste("echo kept");

        term.clear();
        wait_for_line_ending(&mut term, "echo kept");
        let shown = screen(&term);
        assert!(!shown.contains("done"), "{shown}");
        term.vt.scroll(-100);
        let history = screen(&term);
        assert!(!history.contains("20"), "{history}");
        let first = history.lines().next().unwrap_or_default();
        assert!(first.trim_end().ends_with("echo kept"), "{history}");
    }

    /// Polls until the first line on screen ends with `end`.
    fn wait_for_line_ending(term: &mut Terminal, end: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            term.poll();
            let screen = screen(term);
            if screen
                .lines()
                .next()
                .is_some_and(|first| first.trim_end().ends_with(end))
            {
                return;
            }
            assert!(Instant::now() < deadline, "no {end:?} first:\n{screen}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
