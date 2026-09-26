//! qedit: a terminal text editor on OpenTUI's native core.

mod app;
mod document;
mod editor;
mod file_index;
mod history;
mod input;
mod keymap;
mod picker;
mod terminal;
mod tree;
mod words;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opentui::{Output, Renderer};

use app::{App, AppAction};
use input::{Event, Parser};
use keymap::{Command, Keymap};
use workspace::Workspace;

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

const USAGE: &str = "usage: qedit [FILE | FOLDER]";

/// Usage and every command with its shortcut.
fn help() -> String {
    let keymap = Keymap::default();
    let mut help = format!(
        "{USAGE}\n\nOpens FOLDER, or the current folder with FILE (or a new, unnamed buffer) open.\nShift+movement or the mouse selects.\n\n"
    );
    for &command in Command::ALL {
        let key = keymap
            .shortcut(command)
            .map_or(String::new(), |key| key.to_string());
        help += &format!("  {key:<16}{:<30}{}\n", command.id(), command.title());
    }
    help
}

/// The optional file or folder argument, or the exit code for `--help` / bad usage.
fn parse_args() -> Result<Option<PathBuf>, ExitCode> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    if args.next().is_some() {
        eprintln!("{USAGE}");
        return Err(ExitCode::FAILURE);
    }
    match first {
        Some(arg) if arg == "-h" || arg == "--help" => {
            print!("{}", help());
            Err(ExitCode::SUCCESS)
        }
        Some(arg) => Ok(Some(PathBuf::from(arg))),
        None => Ok(None),
    }
}

fn run(path: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    // Load before taking over the terminal so errors print normally.
    let cwd = std::env::current_dir()?;
    let (root, file) = match path {
        Some(path) if path.is_dir() => (path, None),
        Some(path) => (cwd, Some(path)),
        None => (cwd, None),
    };
    let workspace = Workspace::new([root])?;
    let (mut width, mut height) = terminal::size();
    let mut app = App::new(workspace, file, width, height)?;

    let mut renderer = Renderer::new(width, height, Output::Stdout)?;
    renderer.setup_terminal(true);
    // Clicks, drags, and the wheel; plain motion isn't needed.
    renderer.enable_mouse(false);
    let mut parser = Parser::new();

    loop {
        {
            let frame = renderer.next_buffer()?;
            match app.draw(&frame) {
                Some((x, y)) => renderer.set_cursor_position(x as i32 + 1, y as i32 + 1, true),
                None => renderer.set_cursor_position(1, 1, false),
            }
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
            if !events.is_empty() || terminal::size() != (width, height) || app.poll() {
                break events;
            }
        };

        for event in events.drain(..) {
            let action = match event {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse, Instant::now()),
                Event::Paste(text) => {
                    app.paste(&text);
                    AppAction::Continue
                }
                Event::Reply(bytes) => {
                    renderer.process_capability_response(&bytes);
                    AppAction::Continue
                }
            };
            match action {
                AppAction::Quit => return Ok(()),
                AppAction::Copy(text) => {
                    renderer.copy_to_clipboard(&text);
                }
                AppAction::Continue => {}
            }
        }

        let size = terminal::size();
        if size != (width, height) {
            (width, height) = size;
            renderer.resize(width, height);
            app.resize(width, height);
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
