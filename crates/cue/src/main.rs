//! cue: a terminal text editor on OpenTUI's native core.

mod app;
mod context_menu;
mod document;
mod editor;
mod file_dialog;
mod file_index;
mod find;
mod history;
mod input;
mod keymap;
mod language;
mod layout;
mod line_edit;
mod panel;
mod picker;
mod pty;
mod search;
mod search_modal;
mod status;
mod syntax;
mod tab;
mod terminal;
mod theme;
mod tree;
mod tty;
mod words;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opentui::{Output, Renderer};

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use app::{App, AppAction};
use input::{Event, Parser};
use keymap::{Command, Keymap};
use workspace::Workspace;

/// How long input must be idle before a lone ESC counts as the Escape key.
const ESC_TIMEOUT: Duration = Duration::from_millis(30);
/// Upper bound on a wait, so terminal resizes are picked up promptly.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// While output streams into a terminal, the screen is drawn at most this
/// often, so drawing doesn't slow reading it.
const FRAME: Duration = Duration::from_millis(8);

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
        eprintln!("cue crashed: {message}");
        return ExitCode::FAILURE;
    }
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(err)) => {
            eprintln!("cue: {err}");
            ExitCode::FAILURE
        }
        Err(_) => ExitCode::FAILURE,
    }
}

const USAGE: &str = "usage: cue [FILE | FOLDER]";

/// Usage and every command with its shortcut.
fn help() -> String {
    let keymap = Keymap::default();
    let mut help = format!(
        "{USAGE}\n\nOpens FOLDER, or the current folder with FILE (or a new, unnamed buffer) open.\nShift+movement or the mouse selects.\n\n"
    );
    let key = |command| {
        keymap
            .shortcut(command)
            .map_or(String::new(), |key| key.to_string())
    };
    let width = Command::ALL
        .iter()
        .map(|&c| key(c).len())
        .max()
        .unwrap_or(0)
        + 2;
    for &command in Command::ALL {
        let key = key(command);
        help += &format!("  {key:<width$}{:<30}{}\n", command.id(), command.title());
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
    let (mut width, mut height) = tty::size();
    let mut app = App::new(workspace, file, width, height)?;

    let mut renderer = Renderer::new(width, height, Output::Stdout)?;
    renderer.setup_terminal(true);
    // Clicks, drags, and the wheel; plain motion isn't needed.
    renderer.enable_mouse(false);
    let mut parser = Parser::new();

    // When stdin last had input, to tell a lone ESC from the start of a
    // sequence split across reads.
    let mut last_input = Instant::now();
    loop {
        {
            let frame = renderer.next_buffer()?;
            match app.draw(&frame) {
                Some((x, y)) => renderer.set_cursor_position(x as i32 + 1, y as i32 + 1, true),
                None => renderer.set_cursor_position(1, 1, false),
            }
        }
        renderer.render(false);
        let drawn = Instant::now();

        // Wait for input, or output in a terminal; after a partial
        // sequence, only briefly.
        let mut changed = false;
        let mut events = loop {
            let mut timeout = IDLE_POLL;
            if parser.has_pending() {
                timeout = timeout.min(ESC_TIMEOUT.saturating_sub(last_input.elapsed()));
            }
            if changed {
                timeout = timeout.min(FRAME.saturating_sub(drawn.elapsed()));
            }
            let bytes = tty::read_input(timeout, &app.watched())?;
            let events = if !bytes.is_empty() {
                last_input = Instant::now();
                parser.feed(&bytes)
            } else if parser.has_pending() && last_input.elapsed() >= ESC_TIMEOUT {
                parser.flush()
            } else {
                Vec::new()
            };
            changed |= app.poll();
            let resized = tty::size() != (width, height);
            if !events.is_empty() || resized || (changed && drawn.elapsed() >= FRAME) {
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

        let size = tty::size();
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
