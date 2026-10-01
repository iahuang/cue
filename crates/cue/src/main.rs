//! cue: a terminal text editor on OpenTUI's native core.

mod alert;
mod app;
mod config;
mod context_menu;
mod document;
mod editor;
mod file_dialog;
mod file_index;
mod find;
mod history;
mod icons;
mod image;
mod indent;
mod input;
mod keymap;
mod language;
mod layout;
mod line_edit;
mod location;
mod panel;
mod picker;
mod pty;
mod recovery;
mod search;
mod search_modal;
mod status;
mod symbols;
mod syntax;
mod tab;
mod terminal;
mod theme;
mod tree;
mod tty;
mod watch;
mod words;
mod workspace;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opentui::{Output, Renderer, Rgba};

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

    let paths = match parse_args() {
        Ok(paths) => paths,
        Err(code) => return code,
    };
    let result = std::panic::catch_unwind(|| run(paths));
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

const USAGE: &str = "usage: cue [FOLDER]... [FILE[:LINE[:COLUMN]]]";

/// Usage and every command with its shortcut.
fn help() -> String {
    let keymap = Keymap::new(&config::load().0.keys);
    let mut help = format!(
        "{USAGE}\n\nOpens each FOLDER, or the current folder, with FILE (or a new, unnamed buffer) open,\nat LINE and COLUMN if given, as compilers print them: src/main.rs:12:5.\nShift+movement or the mouse selects.\nSettings are in {}; Open Settings in the command palette makes it.\n\n",
        config::path().map_or("~/.config/cue/config.toml".into(), |path| path.display().to_string())
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

/// The file and folder arguments, or the exit code for `--help`,
/// `--version`, or bad usage.
fn parse_args() -> Result<Vec<PathBuf>, ExitCode> {
    let mut paths = Vec::new();
    for arg in std::env::args_os().skip(1) {
        if arg == "-h" || arg == "--help" {
            print!("{}", help());
            return Err(ExitCode::SUCCESS);
        }
        if arg == "-V" || arg == "--version" {
            println!("cue {}", env!("CARGO_PKG_VERSION"));
            return Err(ExitCode::SUCCESS);
        }
        paths.push(PathBuf::from(arg));
    }
    Ok(paths)
}

/// Set on SIGHUP, as when an ssh connection drops, or SIGTERM: cue exits,
/// copying what's unsaved for next time (see [`recovery`]).
static HUNG_UP: AtomicBool = AtomicBool::new(false);

extern "C" fn hang_up(_: libc::c_int) {
    HUNG_UP.store(true, Ordering::Relaxed);
}

fn run(paths: Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let (config, config_warnings) = config::load();
    config::set(config);
    // Load before taking over the terminal so errors print normally.
    let (folders, files): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.into_iter().partition(|path| path.is_dir());
    let mut files = files.into_iter();
    let (file, position) = match files.next() {
        Some(file) => {
            let (file, position) = file_and_position(file);
            (Some(file), position)
        }
        None => (None, None),
    };
    if let Some(extra) = files.next() {
        return Err(format!("{}: only one file opens at a time", extra.display()).into());
    }
    let workspace = match folders.is_empty() {
        true => Workspace::new([std::env::current_dir()?])?,
        false => Workspace::new(folders)?,
    };
    let (mut width, mut height) = tty::size();
    let mut app = App::new(workspace, file, width, height)?;
    if let Some(position) = position {
        app.go_to(position);
    }
    app.warn_about_config(&config_warnings);

    let mut renderer = Renderer::new(width, height, Output::Stdout)?;
    renderer.setup_terminal(true);
    // Zooming into a large image sends it whole: tens of megabytes as
    // base64 through the terminal otherwise.
    renderer.use_kitty_image_files();
    // Clicks, drags, and the wheel; plain motion isn't needed.
    renderer.enable_mouse(false);
    let mut parser = Parser::new();
    // What was typed while waiting for the terminal's colors.
    let mut typed = ask_colors_first(&mut renderer, &mut parser, &mut app)?;
    // Dropped before the renderer, putting the terminal's background back.
    let mut host_colors = HostColors { background: None };
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        unsafe {
            libc::signal(
                signal,
                hang_up as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }

    // When stdin last had input, to tell a lone ESC from the start of a
    // sequence split across reads.
    let mut last_input = Instant::now();
    loop {
        app.update_theme();
        host_colors.apply(&mut renderer, &mut app);
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
        let mut events = if !typed.is_empty() {
            std::mem::take(&mut typed)
        } else {
            loop {
                let mut timeout = IDLE_POLL;
                if parser.has_pending() {
                    timeout = timeout.min(ESC_TIMEOUT.saturating_sub(last_input.elapsed()));
                }
                if changed {
                    timeout = timeout.min(FRAME.saturating_sub(drawn.elapsed()));
                }
                let bytes = tty::read_input(timeout, &app.watched())?;
                // Dropping the app copies what's unsaved.
                if HUNG_UP.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let events = if !bytes.is_empty() {
                    last_input = Instant::now();
                    parser.feed(&bytes)
                } else if parser.has_pending() && last_input.elapsed() >= ESC_TIMEOUT {
                    parser.flush()
                } else {
                    Vec::new()
                };
                changed |= app.poll();
                if let Some(text) = app.take_copied() {
                    renderer.copy_to_clipboard(&text);
                }
                changed |= renderer.poll_kitty_image_transport();
                let resized = tty::size() != (width, height);
                if !events.is_empty() || resized || (changed && drawn.elapsed() >= FRAME) {
                    break events;
                }
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
                    take_reply(&mut renderer, &mut app, &bytes);
                    AppAction::Continue
                }
            };
            match action {
                AppAction::Quit => {
                    app.discard_recovery();
                    return Ok(());
                }
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

/// How long to wait at startup for the terminal to say what its colors
/// are, before drawing without them.
const COLORS_TIMEOUT: Duration = Duration::from_millis(250);

/// Asks the terminal what its colors are, the theme's made of or picked
/// by, and waits for the answer, so the first frame has them. Returns what
/// was typed meanwhile.
fn ask_colors_first(
    renderer: &mut Renderer,
    parser: &mut Parser,
    app: &mut App,
) -> std::io::Result<Vec<Event>> {
    ask_colors();
    let asked = Instant::now();
    let mut typed = Vec::new();
    while let Some(left) = COLORS_TIMEOUT.checked_sub(asked.elapsed()) {
        let bytes = tty::read_input(left, &[])?;
        let mut answered = false;
        for event in parser.feed(&bytes) {
            match event {
                Event::Reply(bytes) => {
                    answered |= bytes == theme::ASKED_LAST;
                    take_reply(renderer, app, &bytes);
                }
                event => typed.push(event),
            }
        }
        if answered {
            break;
        }
    }
    Ok(typed)
}

fn ask_colors() {
    write_to_terminal(&theme::color_queries());
}

fn write_to_terminal(text: &str) {
    let mut stdout = std::io::stdout().lock();
    // The terminal is gone if this fails, which reading input finds out.
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// The terminal's own colors, as the theme and the settings have them: the
/// cursor's (OSC 12), and the background's (OSC 11), which is put back when
/// cue exits.
struct HostColors {
    /// The background cue gave the terminal, if it did.
    background: Option<[u8; 3]>,
}

impl HostColors {
    fn apply(&mut self, renderer: &mut Renderer, app: &mut App) {
        let colors = theme::colors();
        let config = config::get();
        renderer.set_cursor_color(match config.cursor_color {
            true => colors.cursor,
            false => Rgba::terminal_default([255; 3]),
        });
        let bg = colors.bg;
        let background = (config.terminal_background && !bg.is_terminal_default())
            .then(|| [bg.r(), bg.g(), bg.b()]);
        if background == self.background {
            return;
        }
        write_to_terminal(&theme::set_background(background));
        self.background = background;
        app.set_terminal_background(background.is_some());
        if background.is_none() {
            // Its own may have changed meanwhile, as from dark to light.
            ask_colors();
        }
    }
}

impl Drop for HostColors {
    fn drop(&mut self) {
        if self.background.is_some() {
            write_to_terminal(&theme::set_background(None));
        }
    }
}

/// Takes a terminal's reply to a query: about its colors, for the theme,
/// or to the renderer's.
fn take_reply(renderer: &mut Renderer, app: &mut App, bytes: &[u8]) {
    if theme::appearance_change(bytes).is_some() {
        // Switched between dark and light: its colors are new.
        ask_colors();
    }
    if app.take_terminal_reply(bytes) {
        return;
    }
    if !renderer.process_kitty_image_reply(bytes) {
        renderer.process_capability_response(bytes);
    }
}

/// A file argument, and the position printed after its name, as in
/// `src/main.rs:12:5`, unless a file is named that whole.
fn file_and_position(arg: PathBuf) -> (PathBuf, Option<location::Position>) {
    if arg.exists() {
        return (arg, None);
    }
    let Some(text) = arg.to_str() else {
        return (arg, None);
    };
    match location::split_position(text) {
        (path, Some(position)) if !path.is_empty() => (PathBuf::from(path), Some(position)),
        _ => (arg, None),
    }
}

/// Serializes tests that use the native core, which is single-threaded,
/// across every module in this test binary.
#[cfg(test)]
fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
