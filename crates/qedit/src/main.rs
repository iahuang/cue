//! qedit: a terminal text editor on OpenTUI's native core.

mod document;
mod editor;
mod history;
mod input;
mod terminal;
mod words;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opentui::{EditBuffer, Output, Renderer, WidthMethod};

use editor::{Action, Editor, File};
use input::{Event, Parser};

/// How long input must be idle before a lone ESC counts as the Escape key.
const ESC_TIMEOUT: Duration = Duration::from_millis(30);
/// Upper bound on a wait, so terminal resizes are picked up promptly.
const IDLE_POLL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    // The renderer restores the terminal when dropped during unwinding; hold
    // the panic message until then so it isn't drawn into the alternate screen.
    static PANIC: Mutex<Option<String>> = Mutex::new(None);
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::capture();
        *PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{info}\n{backtrace}"));
    }));

    let path = match parse_args() {
        Ok(path) => path,
        Err(code) => return code,
    };
    let result = std::panic::catch_unwind(|| run(path));
    if let Some(message) = PANIC.lock().unwrap_or_else(|e| e.into_inner()).take() {
        eprintln!("qedit crashed: {message}");
        return ExitCode::FAILURE;
    }
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(err)) => {
            eprintln!("qedit: {err}");
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}

const USAGE: &str = "usage: qedit [FILE]";

/// The optional file argument, or the exit code for `--help` / bad usage.
fn parse_args() -> Result<Option<PathBuf>, ExitCode> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if args.next().is_some() {
        eprintln!("{USAGE}");
        return Err(ExitCode::FAILURE);
    }
    match first {
        Some(arg) if arg == "-h" || arg == "--help" => {
            println!("{USAGE}\n\nOpens FILE (or a new, unnamed buffer). ^S save, ^Z/^Y undo/redo, ^A select all, ^C/^X/^V copy/cut/paste,\n^W toggle wrap, ^Q quit. Shift+movement or the mouse selects.");
            Err(ExitCode::SUCCESS)
        }
        Some(arg) => Ok(Some(PathBuf::from(arg))),
        None => Ok(None),
    }
}

fn run(path: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    // Load before taking over the terminal so errors print normally.
    let loaded = match &path {
        Some(path) => Some(document::load(path).map_err(|e| format!("{}: {e}", path.display()))?),
        None => None,
    };

    let (mut width, mut height) = terminal::size();
    let mut renderer = Renderer::new(width, height, Output::Stdout)?;
    renderer.setup_terminal(true);
    // Clicks, drags, and the wheel; plain motion isn't needed.
    renderer.enable_mouse(false);

    let buffer = EditBuffer::new(WidthMethod::Unicode)?;
    buffer.set_tab_width(4);
    let mut file = File {
        path,
        line_ending: Default::default(),
    };
    let mut notice = None;
    if let Some(loaded) = loaded {
        buffer.set_text(&loaded.text);
        buffer.set_cursor(0, 0);
        file.line_ending = loaded.line_ending;
        if loaded.mixed_endings {
            notice = Some(format!(
                "Mixed line endings; saving will use {}.",
                loaded.line_ending.label()
            ));
        }
    }
    let mut editor = Editor::new(&buffer, file, width, height)?;
    if let Some(notice) = notice {
        editor.show_message(notice, false);
    }
    let mut parser = Parser::new();

    loop {
        {
            let frame = renderer.next_buffer()?;
            let (x, y) = editor.draw(&frame);
            renderer.set_cursor_position(x as i32 + 1, y as i32 + 1, true);
        }
        renderer.render(false);

        // Wait for input; after a partial sequence, only briefly.
        let mut events = loop {
            let timeout = if parser.has_pending() {
                ESC_TIMEOUT
            } else {
                IDLE_POLL
            };
            let bytes = terminal::read_input(timeout)?;
            let events = if bytes.is_empty() {
                parser.flush()
            } else {
                parser.feed(&bytes)
            };
            if !events.is_empty() || terminal::size() != (width, height) {
                break events;
            }
        };

        for event in events.drain(..) {
            match event {
                Event::Key(key) => match editor.handle_key(key) {
                    Action::Quit => return Ok(()),
                    Action::Copy(text) => {
                        renderer.copy_to_clipboard(&text);
                    }
                    Action::Continue => {}
                },
                Event::Mouse(mouse) => editor.handle_mouse(mouse, Instant::now()),
                Event::Paste(text) => editor.paste(&text),
                Event::Reply(bytes) => renderer.process_capability_response(&bytes),
            }
        }

        let size = terminal::size();
        if size != (width, height) {
            (width, height) = size;
            renderer.resize(width, height);
            editor.resize(width, height);
        }
    }
}

/// Serializes tests that use the native core, which is single-threaded,
/// across every module in this test binary.
#[cfg(test)]
fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
