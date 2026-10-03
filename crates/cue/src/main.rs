//! cue: a terminal text editor on OpenTUI's native core.

mod alert;
mod app;
mod attach;
mod changes;
mod client;
mod config;
mod context_menu;
mod document;
mod editor;
mod file_dialog;
mod file_index;
mod find;
mod git;
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
mod math;
mod panel;
mod picker;
mod pty;
mod reader;
mod recovery;
mod resume;
mod search;
mod search_modal;
mod session;
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
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opentui::{Output, Renderer, Rgba};

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use app::{App, AppAction};
use attach::{Control, Request};
use input::{Event, Parser};
use keymap::{Command, Keymap};
use session::Session;
use workspace::Workspace;

/// How long input must be idle before a lone ESC counts as the Escape key.
const ESC_TIMEOUT: Duration = Duration::from_millis(30);
/// Upper bound on a wait, so terminal resizes are picked up promptly.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// While output streams into a terminal, the screen is drawn at most this
/// often, so drawing doesn't slow reading it.
const FRAME: Duration = Duration::from_millis(8);
/// Frames sent that the terminal hasn't been seen to get, at most. Over a
/// slow link, ssh takes frames as fast as they're drawn and they queue in
/// the network; holding off instead lets input pile up, so the next frame
/// skips straight to where it leads.
const FRAMES_IN_FLIGHT: usize = 2;
/// How long to wait for a terminal that hasn't answered yet, which may be
/// one that never does.
const FIRST_ANSWER_TIMEOUT: Duration = Duration::from_secs(1);
/// How long to wait for a terminal that answers, before drawing anyway.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    // The renderer restores the terminal when dropped during unwinding; hold
    // the panic message until then so it isn't drawn into the alternate screen.
    static PANIC: Mutex<Option<String>> = Mutex::new(None);
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::capture();
        *PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{info}\n{backtrace}"));
    }));
    // Before the binary can be replaced, as when cue is updated.
    attach::build();

    let start = match parse_args() {
        Ok(Mode::Serve(start)) => start,
        Ok(Mode::Client(args)) => return client::run(args),
        Ok(Mode::List) => return client::list(),
        Ok(Mode::End(id)) => return client::end(id),
        Err(code) => return code,
    };
    let result = std::panic::catch_unwind(|| serve(start));
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

const USAGE: &str = "usage: cue [FOLDER]... [FILE[:LINE[:COLUMN]]]
       cue --resume [--all] | --fresh [FOLDER]...
       cue --list | --end [SESSION]";

/// Usage and every command with its shortcut.
fn help() -> String {
    let keymap = Keymap::new(&config::load().0.keys);
    let mut help = format!(
        "{USAGE}\n\nOpens the specified folders, or the current folder if none are specified.\nOpens FILE, or a new unnamed buffer if no file is specified.\nAppend :LINE or :LINE:COLUMN to a file path, for example src/main.rs:12:5.\nHold Shift while moving the cursor, or drag with the mouse, to select text.\nSettings are stored in {}. Use Open Settings in the command palette to create the file.\n\nSessions preserve tabs, files, unsaved changes, and terminals.\nUse Keep Session to create a session. Quitting with unsaved changes or running\nprograms also creates one. When you quit, programs in session terminals continue\nrunning in the background. Use End Session to end the session.\nWithout FILE, cue resumes the folder's session if exactly one exists.\n\n  -r, --resume   choose a session for the folder\n  -a, --all      choose from every session (implies --resume)\n      --fresh    start a new cue instance\n  -l, --list     list all sessions\n      --end      end SESSION, or the folder's session\n\n",
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

/// What this process is to do.
enum Mode {
    /// Be the `cue` the shell ran (see [`client`]).
    Client(client::Args),
    /// Do the work, in the terminal the client handed over (see
    /// [`attach`]).
    Serve(Start),
    List,
    End(Option<String>),
}

/// What a server starts with.
enum Start {
    /// The folders and file named.
    Paths(Vec<PathBuf>),
    /// The dormant session in this folder.
    Restore(PathBuf),
    /// The session in this folder, from a process this one replaced, and
    /// its running terminals (see [`attach::exec`]).
    Adopt(PathBuf),
}

/// What the command line asks for, or the exit code for `--help`,
/// `--version`, or bad usage.
fn parse_args() -> Result<Mode, ExitCode> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "--serve") {
        args.next();
        let folder = |args: &mut dyn Iterator<Item = std::ffi::OsString>| {
            args.next().map(PathBuf::from).ok_or(ExitCode::FAILURE)
        };
        return Ok(Mode::Serve(
            match args.peek().and_then(|arg| arg.to_str()) {
                Some("--restore") => {
                    args.next();
                    Start::Restore(folder(&mut args)?)
                }
                Some("--adopt") => {
                    args.next();
                    Start::Adopt(folder(&mut args)?)
                }
                _ => Start::Paths(args.map(PathBuf::from).collect()),
            },
        ));
    }
    let mut client = client::Args::default();
    let mut options = true;
    while let Some(arg) = args.next() {
        match arg.to_str().filter(|_| options) {
            Some("-h" | "--help") => {
                print!("{}", help());
                return Err(ExitCode::SUCCESS);
            }
            Some("-V" | "--version") => {
                println!("cue {}", env!("CARGO_PKG_VERSION"));
                return Err(ExitCode::SUCCESS);
            }
            Some("-r" | "--resume") => client.resume = true,
            // Implies --resume.
            Some("-a" | "--all") => {
                client.resume = true;
                client.all = true;
            }
            Some("--fresh") => client.fresh = true,
            Some("-l" | "--list") => return Ok(Mode::List),
            Some("--end") => {
                let id = args.next().map(|id| id.to_string_lossy().into_owned());
                return Ok(Mode::End(id));
            }
            Some("--") => options = false,
            Some(option) if option.starts_with('-') && option.len() > 1 => {
                eprintln!("cue: unknown option {option}\n{USAGE}");
                return Err(ExitCode::FAILURE);
            }
            _ => client.paths.push(PathBuf::from(arg)),
        }
    }
    // For debugging: no server, and no sessions to go back to.
    if env_flag("CUE_NO_SERVER") && !client.resume {
        return Ok(Mode::Serve(Start::Paths(client.paths)));
    }
    Ok(Mode::Client(client))
}

/// Set on SIGHUP: the terminal is gone.
static HUNG_UP: AtomicBool = AtomicBool::new(false);
/// Set on SIGTERM: cue is to exit, keeping what a session would.
static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    match signal {
        libc::SIGHUP => HUNG_UP.store(true, Ordering::Relaxed),
        _ => TERMINATED.store(true, Ordering::Relaxed),
    }
}

/// Does the work: shows the app in the terminal it was handed, then when
/// that's let go (see [`Outcome`]), goes on in the background, if it's a
/// session, until a client attaches.
fn serve(start: Start) -> Result<(), Box<dyn std::error::Error>> {
    let mut control = Control::from_env();
    let (config, config_warnings) = config::load();
    config::set(config);
    // Load before taking over the terminal so errors print normally.
    let (width, height) = tty::size();
    let mut app = match start {
        Start::Paths(paths) => open(paths, width, height)?,
        Start::Restore(dir) => resume(&dir, width, height, Vec::new())?,
        Start::Adopt(dir) => {
            let (ephemeral, adopted) =
                attach::read_manifest(&dir).ok_or("No session state found to take over.")?;
            let mut app = resume(&dir, width, height, adopted)?;
            if ephemeral {
                app.forget_session();
            }
            app
        }
    };
    app.warn_about_config(&config_warnings);
    if let Some(note) = background_sessions(&app) {
        app.show_message_now(note, false);
    }
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        unsafe {
            libc::signal(
                signal,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }

    let mut listener: Option<Listener> = None;
    // What the attached client says the terminal is, if not this process.
    let mut environment: Option<Vec<(String, String)>> = None;
    loop {
        let outcome = attached(
            &mut app,
            control.as_mut(),
            &mut listener,
            environment.clone(),
        )?;
        match outcome {
            Outcome::Quit => {
                app.discard_recovery();
                if let Some(control) = &mut control {
                    control.send("exit 0");
                }
                return Ok(());
            }
            Outcome::Restart => {
                let exe = std::env::current_exe()?;
                app = restart(app, &exe, control.as_ref(), None, &mut listener)?;
                continue;
            }
            Outcome::Detach => {
                let Some(mut client) = control.take() else {
                    // Run without a client, there's nothing to detach from:
                    // it's left to come back to, as quitting does.
                    return Ok(());
                };
                if let Err(err) = listening(&app, &mut listener) {
                    app.show_message_now(format!("Can't detach: {err}"), true);
                    control = Some(client);
                    continue;
                }
                client.send(&format!("detached {}", detached(&app)));
            }
            Outcome::HangUp => {
                control = None;
                if !app.hang_up() || listening(&app, &mut listener).is_err() {
                    app.save_session(true);
                    return Ok(());
                }
            }
            Outcome::Terminate => {
                app.hang_up();
                return Ok(());
            }
            Outcome::End(requester) => {
                app.end_session_now();
                if let Some(control) = &mut control {
                    control.send("ended");
                }
                drop(app);
                drop(requester);
                return Ok(());
            }
            Outcome::Attach(client, request) => {
                // Taken over by another terminal.
                tty::set_nonblocking(false);
                if let Some(mut old) = control.take() {
                    old.send("moved");
                }
                app = take(
                    app,
                    client,
                    request,
                    &mut control,
                    &mut environment,
                    &mut listener,
                )?;
                continue;
            }
        }

        // Detached: in the background, until a client attaches.
        attach::let_go_of_stdio();
        app.set_attached(false);
        environment = None;
        match headless(&mut app, listener.as_ref().map(|l| &l.listener)) {
            Wake::Attach(client, request) => {
                app = take(
                    app,
                    client,
                    request,
                    &mut control,
                    &mut environment,
                    &mut listener,
                )?;
            }
            Wake::End(requester) => {
                app.end_session_now();
                drop(app);
                drop(requester);
                return Ok(());
            }
            Wake::Terminate => {
                app.save_session(true);
                return Ok(());
            }
        }
    }
}

/// An app showing `paths`: the folders named, or the current one, with
/// the file named open, at the line and column given.
fn open(paths: Vec<PathBuf>, width: u32, height: u32) -> Result<App, Box<dyn std::error::Error>> {
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
        return Err(format!("{}: only one file can be specified", extra.display()).into());
    }
    let workspace = match folders.is_empty() {
        true => Workspace::new([std::env::current_dir()?])?,
        false => Workspace::new(folders)?,
    };
    let mut app = App::new(workspace, file, width, height)?;
    if let Some(position) = position {
        app.go_to(position);
    }
    Ok(app)
}

/// An app going back to the session in `dir`, with the terminals
/// `adopted` from a process this one replaced.
fn resume(
    dir: &Path,
    width: u32,
    height: u32,
    adopted: Vec<app::Adopted>,
) -> Result<App, Box<dyn std::error::Error>> {
    let session = Session::open(dir).map_err(|err| format!("can't open the session: {err}"))?;
    let state = session::read(dir).ok_or("the session can't be read")?;
    let roots: Vec<PathBuf> = state
        .roots
        .iter()
        .filter(|root| root.is_dir())
        .cloned()
        .collect();
    if roots.is_empty() {
        return Err("The session's folders no longer exist.".into());
    }
    let mut app = App::new(Workspace::new(roots)?, None, width, height)?;
    app.restore_session(session, state, adopted)?;
    Ok(app)
}

/// A note of the sessions running in the background but `app`'s own, so
/// that none is forgotten, if there are any.
fn background_sessions(app: &App) -> Option<String> {
    let own = app.session().map(Session::id);
    let running: Vec<String> = session::list(&session::default_dir()?)
        .into_iter()
        .filter(|listing| listing.live && !listing.state.attached)
        .filter(|listing| Some(listing.id.as_str()) != own)
        .map(|listing| {
            let folder = listing.state.roots.first().map(|root| client::tilde(root));
            let folder = folder.unwrap_or_default();
            match listing.state.programs().as_slice() {
                [] => folder,
                programs => format!("{folder} ({})", programs.join(", ")),
            }
        })
        .collect();
    match running.len() {
        0 => None,
        1 => Some(format!(
            "One session is running in the background: {}. Run `cue --list` for details.",
            running[0]
        )),
        n => Some(format!(
            "{n} sessions are running in the background: {}. Run `cue --list` for details.",
            running.join(", ")
        )),
    }
}

/// What the client that detached says about how to come back.
fn detached(app: &App) -> String {
    let running = app.running_programs();
    let with = match running.as_slice() {
        [] => String::new(),
        [program] => format!(". Running: {program}"),
        programs => format!(". Running: {}", programs.join(", ")),
    };
    let id = app.session().map(Session::id).unwrap_or_default();
    format!("Session {id} detached{with}. Run `cue --resume` to resume.")
}

/// A session's socket, listened on.
struct Listener {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listens for clients on the session's socket, if `app` is a session and
/// it isn't yet.
fn listen(app: &App, listener: &mut Option<Listener>) -> std::io::Result<()> {
    let (None, Some(session)) = (&listener, app.session()) else {
        return Ok(());
    };
    let path = session.socket();
    *listener = Some(Listener {
        listener: attach::listen(&path)?,
        path,
    });
    Ok(())
}

/// Listens on the session's socket, failing if `app` isn't a session.
fn listening(app: &App, listener: &mut Option<Listener>) -> std::io::Result<()> {
    listen(app, listener)?;
    match listener {
        Some(_) => Ok(()),
        None => Err(std::io::Error::other("Not a session.")),
    }
}

/// Shows `app` in the terminal of `request`, a client's, talking to it
/// over `client` from then on. A client of another build of cue has it
/// take over from this one (see [`restart`]).
fn take(
    app: App,
    client: Control,
    request: attach::Attach,
    control: &mut Option<Control>,
    environment: &mut Option<Vec<(String, String)>>,
    listener: &mut Option<Listener>,
) -> Result<App, Box<dyn std::error::Error>> {
    if request.build != attach::build() && request.exe.is_file() {
        *control = Some(client);
        return restart(
            app,
            &request.exe,
            control.as_ref(),
            Some(request.stdio),
            listener,
        );
    }
    attach::take_stdio(request.stdio);
    *control = Some(client);
    *environment = Some(request.environment);
    let mut app = app;
    app.set_attached(true);
    Ok(app)
}

/// Starts the cue at `exe` in this process's place, to go on from where
/// `app` is (see [`App::hand_over`]): in the terminal `stdio`, if given,
/// or this process's, with `control` to its client. Returns only if it
/// can't, going on in this process instead.
fn restart(
    mut app: App,
    exe: &Path,
    control: Option<&Control>,
    stdio: Option<[std::os::fd::OwnedFd; 3]>,
    listener: &mut Option<Listener>,
) -> Result<App, Box<dyn std::error::Error>> {
    if let Some(stdio) = stdio {
        attach::take_stdio(stdio);
    }
    let Some((dir, ephemeral, adopted)) = app.hand_over() else {
        return Ok(app);
    };
    let (width, height) = tty::size();
    let error = match attach::write_manifest(&dir, ephemeral, &adopted) {
        Ok(()) => {
            drop(app);
            // Taken up again by the new process.
            *listener = None;
            let error = attach::exec(exe, &dir, control);
            attach::read_manifest(&dir);
            error
        }
        Err(error) => {
            drop(app);
            error
        }
    };
    let mut app = resume(&dir, width, height, adopted)?;
    if ephemeral {
        app.forget_session();
    }
    app.show_message_now(format!("Can't restart cue: {error}"), true);
    Ok(app)
}

/// Why a terminal stopped showing the app.
enum Outcome {
    Quit,
    /// To go on in the background (see [`AppAction::Detach`]).
    Detach,
    /// The terminal is gone.
    HangUp,
    /// SIGTERM.
    Terminate,
    Restart,
    /// Another client asked to show the app.
    Attach(Control, attach::Attach),
    /// A client asked to end the session.
    End(Control),
}

/// What woke a detached app.
enum Wake {
    Attach(Control, attach::Attach),
    End(Control),
    Terminate,
}

/// How long a detached app waits between looking at its terminals.
const HEADLESS_POLL: Duration = Duration::from_secs(1);

/// Runs `app` in the background, its terminals' programs running on,
/// until a client attaches or asks to end it, or SIGTERM.
fn headless(app: &mut App, listener: Option<&std::os::unix::net::UnixListener>) -> Wake {
    loop {
        let mut watched = app.watched();
        if let Some(listener) = listener {
            watched.push((listener.as_raw_fd(), false));
        }
        tty::wait(&watched, HEADLESS_POLL);
        if TERMINATED.load(Ordering::Relaxed) {
            return Wake::Terminate;
        }
        HUNG_UP.store(false, Ordering::Relaxed);
        app.poll();
        let _ = app.take_copied();
        let Some(listener) = listener else {
            continue;
        };
        match attach::accept(listener) {
            Some((client, Request::Attach(request))) => match refuse(client, &request) {
                Some(client) => return Wake::Attach(client, request),
                None => {}
            },
            Some((client, Request::End)) => return Wake::End(client),
            None => {}
        }
    }
}

/// `client`, unless `request` comes from one of this process's own
/// terminals, which would show it in itself: then it's told so.
fn refuse(mut client: Control, request: &attach::Attach) -> Option<Control> {
    if request.is_inside(std::process::id()) {
        client.send("error can't attach a session to its own terminal");
        return None;
    }
    Some(client)
}

/// Shows `app` in the terminal on stdin and stdout until it's let go:
/// quit, detached, gone, and so on (see [`Outcome`]). `control` talks to
/// the client, if there is one; `environment` is what it says of the
/// terminal. Clients attaching meanwhile, once the app is a session, take
/// it from this terminal.
fn attached(
    app: &mut App,
    mut control: Option<&mut Control>,
    listener: &mut Option<Listener>,
    environment: Option<Vec<(String, String)>>,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    let (mut width, mut height) = tty::size();
    app.resize(width, height);
    let mut renderer = match environment {
        Some(environment) => Renderer::with_environment(width, height, environment)?,
        None => Renderer::new(width, height, Output::Stdout)?,
    };
    renderer.setup_terminal(true);
    renderer.set_cursor_style(opentui::CursorShape::Line, false);
    // Zooming into a large image sends it whole: tens of megabytes as
    // base64 through the terminal otherwise.
    renderer.use_kitty_image_files();
    // Until the terminal answers, unless OPENTUI_IMAGE_PROTOCOL says.
    math::set_images(renderer.draws_images());
    // Clicks, drags, and the wheel; plain motion isn't needed.
    renderer.enable_mouse(false);
    // Experimental, for slow links such as ssh: fewer bytes per frame.
    renderer.set_compact_output(env_flag("CUE_COMPACT_OUTPUT"));
    let mut parser = Parser::new();
    // What was typed while waiting for the terminal's colors.
    let Ok(mut typed) = ask_colors_first(&mut renderer, &mut parser, app) else {
        return Ok(Outcome::HangUp);
    };
    // Dropped before the renderer, putting the terminal's background back.
    let mut host_colors = HostColors { background: None };

    // When stdin last had input, to tell a lone ESC from the start of a
    // sequence split across reads.
    let mut last_input = Instant::now();
    let mut pacing = Pacing::new(env_flag("CUE_PACING"));
    // Whether something happened since the last frame.
    let mut dirty = true;
    let mut drawn = Instant::now();
    loop {
        // Kept as a session meanwhile, it takes clients.
        let _ = listen(app, listener);
        app.update_theme();
        host_colors.apply(&mut renderer, app);
        if dirty && pacing.ready() {
            {
                let frame = renderer.next_buffer()?;
                match app.draw(&frame) {
                    Some((x, y)) => renderer.set_cursor_position(x as i32 + 1, y as i32 + 1, true),
                    None => renderer.set_cursor_position(1, 1, false),
                }
            }
            renderer.render(false);
            pacing.sent();
            dirty = false;
            drawn = Instant::now();
        }

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
                if dirty {
                    timeout = timeout.min(pacing.time_left());
                }
                let mut watched = app.watched();
                watched.extend(control.as_ref().map(|control| (control.fd(), false)));
                watched.extend(listener.as_ref().map(|l| (l.listener.as_raw_fd(), false)));
                let Ok(bytes) = tty::read_input(timeout, &watched) else {
                    return Ok(Outcome::HangUp);
                };
                if TERMINATED.load(Ordering::Relaxed) {
                    return Ok(Outcome::Terminate);
                }
                if HUNG_UP.swap(false, Ordering::Relaxed) {
                    return Ok(Outcome::HangUp);
                }
                // The client goes with the terminal. A resize is noticed
                // below.
                if let Some(control) = control.as_deref_mut() {
                    if control.receive().is_err() {
                        return Ok(Outcome::HangUp);
                    }
                }
                if let Some(listener) = listener.as_ref() {
                    match attach::accept(&listener.listener) {
                        Some((client, Request::Attach(request))) => {
                            if let Some(client) = refuse(client, &request) {
                                // Putting this terminal back as it was is
                                // only worth trying: it may be stuck, as
                                // over an ssh connection that froze.
                                tty::set_nonblocking(true);
                                return Ok(Outcome::Attach(client, request));
                            }
                        }
                        Some((client, Request::End)) => return Ok(Outcome::End(client)),
                        None => {}
                    }
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
                if !events.is_empty()
                    || resized
                    || (changed && drawn.elapsed() >= FRAME)
                    || (dirty && pacing.ready())
                {
                    break events;
                }
            }
        };
        dirty |= changed;

        for event in events.drain(..) {
            match &event {
                Event::Reply(bytes) if is_device_attributes(bytes) => pacing.answered(),
                _ => dirty = true,
            }
            let action = match event {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse, Instant::now()),
                Event::Paste(text) => {
                    app.paste(&text);
                    AppAction::Continue
                }
                Event::Reply(bytes) => {
                    take_reply(&mut renderer, app, &bytes);
                    AppAction::Continue
                }
            };
            match action {
                AppAction::Quit => return Ok(Outcome::Quit),
                AppAction::Detach => return Ok(Outcome::Detach),
                AppAction::Restart => return Ok(Outcome::Restart),
                AppAction::Copy(text) => {
                    renderer.copy_to_clipboard(&text);
                }
                AppAction::Continue => {}
            }
        }

        let size = tty::size();
        if size != (width, height) {
            dirty = true;
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

/// Whether the environment variable `name` is set, to anything but 0.
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty() && value != "0")
}

/// Keeps at most [`FRAMES_IN_FLIGHT`] frames between cue and the screen,
/// when on (experimental: `CUE_PACING`).
/// Each frame is followed by a request for the terminal's device
/// attributes, which it answers once it has read everything before.
struct Pacing {
    /// When each frame not yet answered for was sent, oldest first.
    sent: std::collections::VecDeque<Instant>,
    /// Whether the terminal has answered at all.
    answers: bool,
    /// False once a terminal that never answered has been waited on.
    on: bool,
}

impl Pacing {
    fn new(on: bool) -> Self {
        Pacing {
            sent: std::collections::VecDeque::new(),
            answers: false,
            on,
        }
    }

    fn sent(&mut self) {
        if self.on {
            write_to_terminal("\x1b[c");
            self.sent.push_back(Instant::now());
        }
    }

    fn answered(&mut self) {
        self.answers = true;
        self.sent.pop_front();
    }

    /// Whether another frame can be sent now.
    fn ready(&mut self) -> bool {
        if self.on && self.time_left().is_zero() {
            // Not answering, or too slow to wait on.
            self.on = self.answers;
            self.sent.clear();
        }
        self.sent.len() < FRAMES_IN_FLIGHT
    }

    /// How long until waiting for an answer gives up.
    fn time_left(&self) -> Duration {
        let timeout = match self.answers {
            true => ANSWER_TIMEOUT,
            false => FIRST_ANSWER_TIMEOUT,
        };
        match self.sent.front() {
            Some(oldest) => timeout.saturating_sub(oldest.elapsed()),
            None => Duration::MAX,
        }
    }
}

/// Whether `reply` answers a request for device attributes (DA1).
fn is_device_attributes(reply: &[u8]) -> bool {
    reply.starts_with(b"\x1b[?") && reply.ends_with(b"c")
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
        math::set_images(renderer.draws_images());
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
