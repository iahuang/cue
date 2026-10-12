//! cue: a terminal text editor on OpenTUI's native core.

mod alert;
mod app;
mod attach;
mod autoindent;
mod bridge;
mod changes;
mod client;
mod commit_diff;
mod commit_view;
mod config;
mod context_menu;
mod crash;
mod definition;
mod diff;
mod document;
mod editor;
mod file_dialog;
mod file_index;
mod file_ops;
mod find;
mod git;
mod history;
mod host;
mod icons;
mod image;
mod indent;
mod input;
mod input_box;
mod keymap;
mod language;
mod layout;
mod line_edit;
mod location;
mod log;
mod math;
mod panel;
mod pdf;
mod picker;
mod pty;
mod quick_look;
mod reader;
mod recovery;
mod relocate;
mod remote;
mod replica;
mod resume;
mod scroller;
mod search;
mod search_modal;
mod session;
mod status;
mod switcher;
mod symbols;
mod syntax;
mod tab;
mod term_hub;
mod terminal;
mod theme;
mod toast;
mod tree;
mod tty;
mod wake;
mod watch;
mod wire;
mod words;
mod workspace;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentui::{Output, Renderer, Rgba};

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use app::{App, AppAction};
use attach::{Control, Request};
use host::Link;
use input::{Event, Parser};
use keymap::{Command, Keymap};
use session::SessionLink;
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
/// The kitty keyboard protocol's flags while the app waits for a key to
/// go up: every key as an escape code (8), modifiers included, with the
/// text it types (16), and whether it went up (2), besides the renderer's.
const KEY_RELEASE_FLAGS: u8 = 0b11010;

fn main() -> ExitCode {
    // Before anything else: it may start this process over, from a copy of
    // its binary.
    relocate::init();
    // Before the binary can be replaced, as when cue is updated.
    attach::build();
    crash::install();

    let mode = parse_args();
    // Only a client needs the copy it runs from, if it does: to start a
    // server.
    if !matches!(mode, Ok(Mode::Client(_) | Mode::Restarted(..))) {
        relocate::release();
    }
    // Whether the app's shown in this process, the client's.
    let here = match &mode {
        Ok(Mode::Client(args)) => args.remote.is_some() || env_flag(HOST_PROCESS_VARIABLE),
        Ok(Mode::Restarted(..)) => true,
        _ => false,
    };
    // The session's only for restarting, to forget once it's back.
    let mut ephemeral = false;
    // The bridge to the host on another machine, if it's there.
    let mut bridge = None;
    let start = match mode {
        Ok(Mode::Serve(start)) => start,
        Ok(Mode::Restarted(dir, only_for_restart, None)) => {
            ephemeral = only_for_restart;
            // Its host may have had nothing left to wait for.
            match session::is_live(&dir) {
                true => Start::Attach(dir),
                false => Start::Restore(dir),
            }
        }
        Ok(Mode::Restarted(dir, only_for_restart, Some(host))) => {
            ephemeral = only_for_restart;
            match reach(&host).and_then(|mut reached| {
                let live = reached.sessions()?.iter().any(|l| l.dir == dir && l.live);
                bridge = Some(reached);
                Ok(live)
            }) {
                Ok(true) => Start::Attach(dir),
                Ok(false) => Start::Restore(dir),
                Err(err) => {
                    relocate::release();
                    eprintln!("cue: can't reach {host}: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
        Ok(Mode::Client(args)) if args.remote.is_some() => {
            let destination = args.remote.clone().unwrap();
            let mut reached = match reach(&destination.host) {
                Ok(reached) => reached,
                Err(err) => {
                    relocate::release();
                    eprintln!("cue: can't reach {}: {err}", destination.host);
                    return ExitCode::FAILURE;
                }
            };
            let opening = remote::target(&mut reached, &destination, &args);
            bridge = Some(reached);
            match opening {
                Ok(remote::Opening::Session(listing)) if listing.live => Start::Attach(listing.dir),
                Ok(remote::Opening::Session(listing)) => Start::Restore(listing.dir),
                Ok(remote::Opening::Paths(paths)) => Start::Paths(paths),
                Err(code) => {
                    relocate::release();
                    return code;
                }
            }
        }
        Ok(Mode::Client(args)) if here => match client::target(&args) {
            Ok(Some(listing)) if listing.live => Start::Attach(listing.dir),
            Ok(Some(listing)) => Start::Restore(listing.dir),
            Ok(None) => Start::Paths(args.paths),
            Err(code) => {
                relocate::release();
                return code;
            }
        },
        Ok(Mode::Client(args)) => {
            let code = client::run(args);
            relocate::release();
            return code;
        }
        Ok(Mode::Host(adopt)) => return host_process(adopt),
        Ok(Mode::Bridge) => return bridge::run(),
        Ok(Mode::List) => return client::list(),
        Ok(Mode::End(id)) => return client::end(id),
        Err(code) => return code,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match here {
        true => show(start, ephemeral, bridge),
        false => serve(start),
    }));
    relocate::release();
    if let Some(panic) = crash::take_panic() {
        eprintln!("cue crashed: {}", panic.text);
        // Said, unless stderr is the background's log.
        if unsafe { libc::isatty(libc::STDERR_FILENO) } == 1 {
            if let Some(report) = &panic.report {
                crash::mark_seen(report);
            }
        }
        crash::close_log();
        return ExitCode::FAILURE;
    }
    crash::close_log();
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
       cue [USER@]HOST:[PATH]
       cue --list | --end [SESSION]";

/// Usage and every command with its shortcut.
fn help() -> String {
    let keymap = Keymap::new(&config::load().0.keys);
    let mut help = format!(
        "{USAGE}\n\nOpens the specified folders, or the current folder if none are specified.\nOpens FILE, or a new unnamed buffer if no file is specified.\nAppend :LINE or :LINE:COLUMN to a file path, for example src/main.rs:12:5.\nHold Shift while moving the cursor, or drag with the mouse, to select text.\nSettings are stored in {}. Use Open Settings in the command palette to create the file.\n\nSessions preserve tabs, files, unsaved changes, and terminals.\nUse Keep Session to create a session. Quitting with unsaved changes or running\nprograms also creates one. When you quit, programs in session terminals continue\nrunning in the background. Use End Session to end the session.\nWithout FILE, cue resumes the folder's session if exactly one exists.\n\nHOST:PATH opens PATH on another machine, over ssh, as scp names it: PATH is\nfrom the home folder there. Without PATH, choose one of its sessions or folders.\n\n  -r, --resume   choose a session for the folder\n  -a, --all      choose from all sessions (implies --resume)\n      --fresh    start a new cue instance\n  -l, --list     list all sessions\n      --end      end SESSION (an ID or name), or the folder's session\n\n",
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
    /// Show the session in this folder, as a client that restarted (see
    /// [`restart_client`]), forgetting it once it's back if it was only
    /// for that; on another machine, if one's named.
    Restarted(PathBuf, bool, Option<String>),
    /// Be a host (see [`host::serve_listener`]), on the socket the app
    /// that started this process handed it, or with a folder, the host
    /// process this one replaced (see [`host::Handover`]).
    Host(Option<PathBuf>),
    /// Be the bridge a client on another machine reaches over ssh (see
    /// [`bridge`]).
    Bridge,
    List,
    End(Option<String>),
}

/// Has the app use a host process (see [`host::Link::spawn`]), shown by
/// the `cue` the shell ran (see [`show`]): experimental, until it's how
/// cue always runs.
const HOST_PROCESS_VARIABLE: &str = "CUE_HOST_PROCESS";

/// What a server starts with.
enum Start {
    /// The folders and file named.
    Paths(Vec<PathBuf>),
    /// The dormant session in this folder.
    Restore(PathBuf),
    /// The session in this folder, from a process this one replaced, and
    /// its running terminals (see [`attach::exec`]).
    Adopt(PathBuf),
    /// The session in this folder, from the host process running it, its
    /// terminals included (see [`show`]).
    Attach(PathBuf),
}

/// What the command line asks for, or the exit code for `--help`,
/// `--version`, or bad usage.
fn parse_args() -> Result<Mode, ExitCode> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "--host") {
        args.next();
        let adopt = match args.next() {
            Some(arg) if arg == "--adopt" => {
                Some(args.next().map(PathBuf::from).ok_or(ExitCode::FAILURE)?)
            }
            _ => None,
        };
        return Ok(Mode::Host(adopt));
    }
    if args.peek().is_some_and(|arg| arg == "--bridge") {
        return Ok(Mode::Bridge);
    }
    if args.peek().is_some_and(|arg| arg == RESTARTED) {
        args.next();
        let dir = args.next().map(PathBuf::from).ok_or(ExitCode::FAILURE)?;
        let (mut ephemeral, mut remote) = (false, None);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some(EPHEMERAL) => ephemeral = true,
                Some(REMOTE) => remote = args.next().and_then(|host| host.into_string().ok()),
                _ => {}
            }
        }
        return Ok(Mode::Restarted(dir, ephemeral, remote));
    }
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
            _ => match remote::destination(&arg) {
                Some(destination) if client.remote.is_none() => client.remote = Some(destination),
                Some(_) => {
                    eprintln!("cue: only one host can be named\n{USAGE}");
                    return Err(ExitCode::FAILURE);
                }
                None => client.paths.push(PathBuf::from(arg)),
            },
        }
    }
    if client.remote.is_some() && !client.paths.is_empty() {
        eprintln!("cue: a host's path is named after it, as HOST:PATH\n{USAGE}");
        return Err(ExitCode::FAILURE);
    }
    // For debugging: no server, and no sessions to go back to.
    if env_flag("CUE_NO_SERVER") && !client.resume && client.remote.is_none() {
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
        Start::Paths(paths) => {
            let cwd = std::env::current_dir()?;
            open(Link::start(), paths, &cwd, width, height)?
        }
        Start::Restore(dir) => resume(Link::start(), &dir, width, height, Vec::new())?,
        Start::Adopt(dir) => {
            let (ephemeral, adopted) =
                attach::read_manifest(&dir).ok_or("No session state found to take over.")?;
            let mut app = resume(Link::start(), &dir, width, height, adopted)?;
            if ephemeral {
                app.forget_session();
            }
            app
        }
        Start::Attach(_) => return Err("Only a host process's client attaches.".into()),
    };
    app.warn_about_config(&config_warnings);
    if let Some(note) = crashes() {
        app.show_message_now(note, true);
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
                let exe = relocate::origin().to_path_buf();
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
            // Only from a host process, as with `CUE_NO_SERVER` too.
            Outcome::Moved => {
                app.save_session(true);
                leave(app);
                return Ok(());
            }
            Outcome::Ended => {
                leave(app);
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
        attach::let_go_of_stdio(crash::open_log());
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

/// What an app starts with, asked of its host at once, so that the
/// answers come together.
struct Starting {
    host: Link,
    about: host::Call,
    sessions: host::Call,
}

impl Starting {
    fn new(host: Link) -> Starting {
        Starting {
            about: host.call(host::Request::About),
            sessions: host.call(host::Request::Sessions),
            host,
        }
    }

    /// What the host's machine is like, once it's said, put in use: its
    /// settings, but `[ui]` and `[keys]`, and its home folder, where it's
    /// another machine. A host in this process shares this one's. With this
    /// app's own settings, as they were.
    fn about(&self) -> (host::About, config::Config) {
        let about = match self.about.wait() {
            Some(host::Reply::About(about)) => about,
            _ => host::About::default(),
        };
        let own = (*config::get()).clone();
        if !self.host.is_local() {
            let theirs = config::host_settings(about.settings.as_deref());
            config::set(config::merge(own.clone(), &theirs));
        }
        if self.host.remote().is_some() {
            client::set_home(about.home.clone());
        }
        (about, own)
    }

    /// The app, made, given what it's to know of the host.
    fn finish(self, mut app: App, (about, own): (host::About, config::Config)) -> App {
        app.use_about(about, own);
        let sessions = match self.sessions.wait() {
            Some(host::Reply::Sessions(sessions)) => sessions,
            _ => Vec::new(),
        };
        if let Some(note) = background_sessions(&app, sessions) {
            app.show_message_now(note, false);
        }
        app
    }
}

/// What the host found at each of `paths`, waiting for it.
fn look(host: &Link, paths: Vec<PathBuf>) -> Vec<Option<host::Look>> {
    let count = paths.len();
    match host.call(host::Request::Look { paths }).wait() {
        Some(host::Reply::Looked(looked)) if looked.len() == count => looked,
        _ => vec![None; count],
    }
}

/// An app showing `paths`, relative to `cwd`, on `host`: the folders
/// named, or `cwd`, with the file named open, at the line and column
/// given. The host says what each is.
fn open(
    host: Link,
    paths: Vec<PathBuf>,
    cwd: &Path,
    width: u32,
    height: u32,
) -> Result<App, Box<dyn std::error::Error>> {
    let starting = Starting::new(host.clone());
    // Each path, and what it'd name with the position printed after it
    // taken off, as in `src/main.rs:12:5`.
    let named: Vec<(PathBuf, Option<(PathBuf, location::Position)>)> = paths
        .iter()
        .map(|path| {
            let split = path
                .to_str()
                .and_then(|text| match location::split_position(text) {
                    (file, Some(position)) if !file.is_empty() => Some((cwd.join(file), position)),
                    _ => None,
                });
            (cwd.join(path), split)
        })
        .collect();
    let mut asked = vec![cwd.to_path_buf()];
    for (whole, split) in &named {
        asked.push(whole.clone());
        asked.extend(split.as_ref().map(|(file, _)| file.clone()));
    }
    let mut looked = look(&host, asked).into_iter();
    let here = looked.next().flatten();
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for (whole, split) in named {
        let found = looked.next().flatten();
        if split.is_some() {
            looked.next();
        }
        match (found, split) {
            (Some(look), _) if look.is_dir => folders.push(look.real),
            (None, Some((file, position))) => files.push((file, Some(position))),
            _ => files.push((whole, None)),
        }
    }
    let mut files = files.into_iter();
    let (file, position) = files.next().unzip();
    if let Some((extra, _)) = files.next() {
        return Err(format!("{}: only one file can be specified", extra.display()).into());
    }
    if folders.is_empty() {
        let here = here.filter(|look| look.is_dir);
        let here = here.ok_or_else(|| format!("{} isn't a folder", cwd.display()))?;
        folders.push(here.real);
    }
    let about = starting.about();
    let workspace = Workspace::found(folders);
    let app = App::with_host(host, workspace, file, width, height)?;
    let mut app = starting.finish(app, about);
    if let Some(position) = position.flatten() {
        app.go_to(position);
    }
    Ok(app)
}

/// An app going back to the session in `dir`, on `host`, with the
/// terminals `adopted` from a process this one replaced.
fn resume(
    host: Link,
    dir: &Path,
    width: u32,
    height: u32,
    adopted: Vec<app::Adopted>,
) -> Result<App, Box<dyn std::error::Error>> {
    let starting = Starting::new(host.clone());
    let read = host.call(host::Request::Session(session::SessionCall::Read(
        dir.to_path_buf(),
    )));
    // First: a host process taken over from a client says so once that
    // one's saved it, and left.
    let session =
        SessionLink::open(&host, dir).map_err(|err| format!("can't open the session: {err}"))?;
    let state = match read.wait() {
        Some(host::Reply::Session(session::SessionAnswer::State(Some(state)))) => *state,
        _ => return Err("the session can't be read".into()),
    };
    let looked = look(&host, state.roots.clone());
    let roots: Vec<PathBuf> = state
        .roots
        .iter()
        .zip(looked)
        .filter(|(_, look)| look.as_ref().is_some_and(|look| look.is_dir))
        .map(|(root, _)| root.clone())
        .collect();
    if roots.is_empty() {
        return Err("The session's folders no longer exist.".into());
    }
    let about = starting.about();
    let mut app = App::with_host(host, Workspace::found(roots), None, width, height)?;
    // Before what's running elsewhere is said, which this isn't.
    app.restore_session(session, state, adopted)?;
    Ok(starting.finish(app, about))
}

/// A note of the sessions running in the background but `app`'s own,
/// among `sessions`, the host's, so that none is forgotten, if there are
/// any.
fn background_sessions(app: &App, sessions: Vec<session::Listing>) -> Option<String> {
    let own = app.session().map(SessionLink::id);
    let running: Vec<String> = sessions
        .into_iter()
        .filter(|listing| listing.live && !listing.state.attached)
        .filter(|listing| Some(listing.id.as_str()) != own)
        .map(|listing| {
            let folder = listing.state.roots.first().map(|root| client::tilde(root));
            let folder = listing.state.name.clone().or(folder).unwrap_or_default();
            match listing.state.programs().as_slice() {
                [] => folder,
                programs => format!("{folder} ({})", programs.join(", ")),
            }
        })
        .collect();
    let list = match app.host().remote() {
        Some(remote) => format!("`cue {remote}:`"),
        None => "`cue --list`".to_string(),
    };
    match running.len() {
        0 => None,
        1 => Some(format!(
            "One session is running in the background: {}. Run {list} for details.",
            running[0]
        )),
        n => Some(format!(
            "{n} sessions are running in the background: {}. Run {list} for details.",
            running.join(", ")
        )),
    }
}

/// Where the crash reports not said yet are, if there are any.
fn crashes() -> Option<String> {
    let dir = crash::default_dir()?;
    match crash::collect(&dir).as_slice() {
        [] => None,
        [report] => Some(format!(
            "cue crashed. The report is in {}.",
            client::tilde_here(report)
        )),
        reports => Some(format!(
            "cue crashed {} times. The reports are in {}.",
            reports.len(),
            client::tilde_here(&dir)
        )),
    }
}

/// Runs a host process, until its client's gone; with `adopt`, in place
/// of one of another build, for the session in that folder (see
/// [`host::Handover`]).
fn host_process(adopt: Option<PathBuf>) -> ExitCode {
    let Some(listener) = host::listener_from_env() else {
        eprintln!("cue: --host is for cue to start");
        return ExitCode::FAILURE;
    };
    let (config, _) = config::load();
    config::set(config);
    // Where a panic's said: there's no terminal.
    attach::let_go_of_stdio(crash::open_log());
    let handover = match adopt.as_deref().map(host::Handover::from_env) {
        Some(Ok(handover)) => Some(handover),
        Some(Err(err)) => {
            eprintln!("cue: can't take over the session: {err}");
            crash::close_log();
            return ExitCode::FAILURE;
        }
        None => None,
    };
    host::serve_listener(listener, true, handover);
    crash::close_log();
    ExitCode::SUCCESS
}

/// How long leaving waits for the host to hear what's left to say.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shows the app in this process's terminal, as the `cue` the shell ran,
/// with its host a process of its own (`CUE_HOST_PROCESS`, for now): a
/// new one, or with [`Start::Attach`], the one running the session,
/// which another client showing it leaves first. Quitting hangs up on its
/// terminals; detaching, or the terminal going, leaves them running in the
/// host, for `cue` to come back to; it exits either way, unless it
/// restarts (see [`restart_client`]). The session's forgotten once it's
/// back if it's `ephemeral`.
fn show(
    start: Start,
    ephemeral: bool,
    bridge: Option<remote::Bridge>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (config, config_warnings) = config::load();
    config::set(config);
    let (width, height) = tty::size();
    let spawn = || Link::spawn().map_err(|err| format!("can't start a host process: {err}"));
    let connect = |bridge: remote::Bridge, target| {
        let host = bridge.name().to_string();
        bridge
            .connect(target)
            .map_err(|err| format!("can't reach cue on {host}: {err}"))
    };
    let mut app = match (start, bridge) {
        (Start::Paths(paths), None) => {
            let cwd = std::env::current_dir()?;
            open(spawn()?, paths, &cwd, width, height)?
        }
        (Start::Paths(paths), Some(bridge)) => {
            let home = bridge.home.clone();
            open(
                connect(bridge, bridge::Target::New)?,
                paths,
                &home,
                width,
                height,
            )?
        }
        (Start::Restore(dir), None) => resume(spawn()?, &dir, width, height, Vec::new())?,
        (Start::Restore(dir), Some(bridge)) => {
            let host = connect(bridge, bridge::Target::New)?;
            resume(host, &dir, width, height, Vec::new())?
        }
        (Start::Attach(dir), None) => {
            let host = Link::connect(&session::socket_path(&dir))
                .map_err(|err| format!("can't reach the session: {err}"))?;
            resume(host, &dir, width, height, Vec::new())?
        }
        (Start::Attach(dir), Some(bridge)) => {
            let host = connect(bridge, bridge::Target::Attach(dir.clone()))?;
            resume(host, &dir, width, height, Vec::new())?
        }
        (Start::Adopt(_), _) => return Err("Only a server takes over.".into()),
    };
    if ephemeral {
        app.forget_session();
    }
    // The host's started, from the copy this runs from, if it does.
    relocate::release();
    app.warn_about_config(&config_warnings);
    if let Some(note) = crashes() {
        app.show_message_now(note, true);
    }
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        unsafe {
            libc::signal(
                signal,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }
    loop {
        let said = match attached(&mut app, None, &mut None, None)? {
            Outcome::Quit => {
                app.discard_recovery();
                close(app);
                return Ok(());
            }
            Outcome::Restart => {
                let Some((dir, ephemeral)) = app.keep_to_restart(true) else {
                    continue;
                };
                app.set_attached(false);
                let remote = app.host().remote().map(str::to_string);
                let resume = match ephemeral {
                    true => String::new(),
                    false => format!(" Run `{}` to resume.", resume_command(&app)),
                };
                leave(app);
                let err = restart_client(&dir, ephemeral, remote.as_deref());
                return Err(format!("can't restart: {err}.{resume}").into());
            }
            Outcome::Detach => {
                app.set_attached(false);
                detached(&app)
            }
            Outcome::HangUp | Outcome::Terminate => {
                if !app.hang_up() {
                    close(app);
                    return Ok(());
                }
                // Nowhere to say it.
                String::new()
            }
            Outcome::Moved => {
                app.set_attached(false);
                "the session moved to another terminal.".to_string()
            }
            // Not served any more.
            Outcome::Ended => "the session was ended.".to_string(),
            Outcome::Attach(..) | Outcome::End(_) => unreachable!("it takes no clients"),
        };
        leave(app);
        if !said.is_empty() {
            eprintln!("cue: {said}");
        }
        return Ok(());
    }
}

/// Says a client restarted, with the session's folder: `cue --restarted
/// DIR [--ephemeral] [--remote HOST]`.
const RESTARTED: &str = "--restarted";
const EPHEMERAL: &str = "--ephemeral";
const REMOTE: &str = "--remote";

/// Reaches `host` over ssh (see [`remote`]), its paths shown from its
/// home folder from then on.
fn reach(host: &str) -> std::io::Result<remote::Bridge> {
    let bridge = remote::Bridge::open(host)?;
    client::set_home(Some(bridge.home.clone()));
    Ok(bridge)
}

/// Starts the cue that's on disk now, as a client, in this one's place,
/// to come back to the session in `dir`, which this one left, on `remote`
/// if it's on another machine; and to forget it once it's back, if it's
/// `ephemeral`. Its host takes up that build too, if it's another (see
/// [`host::serve_listener`]). Returns only if it can't.
fn restart_client(dir: &Path, ephemeral: bool, remote: Option<&str>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(relocate::origin());
    command.arg(RESTARTED).arg(dir);
    if ephemeral {
        command.arg(EPHEMERAL);
    }
    if let Some(remote) = remote {
        command.arg(REMOTE).arg(remote);
    }
    command.exec()
}

/// What to run to come back to `app`'s session.
fn resume_command(app: &App) -> String {
    match app.host().remote() {
        Some(remote) => format!("cue {remote}:"),
        None => "cue --resume".to_string(),
    }
}

/// Lets `app` go, hanging up on its terminals, once the host hears.
fn close(app: App) {
    let closed = app.host().closed();
    drop(app);
    closed.wait(CLOSE_TIMEOUT);
}

/// Lets `app` go, leaving its terminals running in the host, for a
/// client to come back to.
fn leave(mut app: App) {
    app.leave();
    close(app);
}

/// What the client that detached says about how to come back.
fn detached(app: &App) -> String {
    let running = app.running_programs();
    let with = match running.as_slice() {
        [] => String::new(),
        [program] => format!(". Running: {program}"),
        programs => format!(". Running: {}", programs.join(", ")),
    };
    let session = app.session();
    let id = session.map(|session| session.name().unwrap_or(session.id()));
    let id = id.unwrap_or_default();
    format!(
        "Session {id} detached{with}. Run `{}` to resume.",
        resume_command(app)
    )
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
/// it isn't yet, unless its host is a process: then that does.
fn listen(app: &App, listener: &mut Option<Listener>) -> std::io::Result<()> {
    let (None, Some(session), true) = (&listener, app.session(), app.host().is_local()) else {
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
    crash::close_log();
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
        crash::close_log();
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
    let mut app = resume(Link::start(), &dir, width, height, adopted)?;
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
    /// Another client is taking over, from the host process.
    Moved,
    /// The session was ended, from the host process.
    Ended,
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
        // Nothing's typed while detached, to act on.
        let _ = app.take_deferred();
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
    // Report plain motion as well, for hover feedback.
    renderer.enable_mouse(true);
    // Experimental, for slow links such as ssh: fewer bytes per frame.
    renderer.set_compact_output(env_flag("CUE_COMPACT_OUTPUT"));
    let mut parser = Parser::new();
    // What was typed while waiting for the terminal's colors.
    let Ok(mut typed) = ask_colors_first(&mut renderer, &mut parser, app) else {
        return Ok(Outcome::HangUp);
    };
    // Dropped before the renderer, putting the terminal's background back.
    let mut host_colors = HostColors { background: None };
    // Likewise, putting the terminal's title back.
    let mut host_title = HostTitle { title: None };

    // When stdin last had input, to tell a lone ESC from the start of a
    // sequence split across reads.
    let mut last_input = Instant::now();
    let mut pacing = Pacing::new(env_flag("CUE_PACING"));
    // Whether something happened since the last frame.
    let mut dirty = true;
    let mut drawn = Instant::now();
    // Whether the terminal was asked to report keys going up, which this
    // one may do.
    let mut asked_key_releases = false;
    app.set_key_releases(true);
    loop {
        // Kept as a session meanwhile, it takes clients.
        let _ = listen(app, listener);
        app.update_theme();
        host_colors.apply(&mut renderer, app);
        host_title.apply(app);
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
                if let Some(wake) = app.wakes_in() {
                    timeout = timeout.min(wake);
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
                match app.take_deferred() {
                    Some(AppAction::Quit) => return Ok(Outcome::Quit),
                    Some(AppAction::Detach) => return Ok(Outcome::Detach),
                    Some(AppAction::Restart) => return Ok(Outcome::Restart),
                    Some(AppAction::Moved) => return Ok(Outcome::Moved),
                    Some(AppAction::Ended) => return Ok(Outcome::Ended),
                    Some(AppAction::Copy(text)) => {
                        renderer.copy_to_clipboard(&text);
                    }
                    Some(AppAction::Continue) | None => {}
                }
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
                Event::Release(mods) => app.handle_release(mods),
                Event::Reply(bytes) => {
                    take_reply(&mut renderer, app, &bytes);
                    AppAction::Continue
                }
            };
            match action {
                AppAction::Quit => return Ok(Outcome::Quit),
                AppAction::Detach => return Ok(Outcome::Detach),
                AppAction::Restart => return Ok(Outcome::Restart),
                AppAction::Moved => return Ok(Outcome::Moved),
                AppAction::Ended => return Ok(Outcome::Ended),
                AppAction::Copy(text) => {
                    renderer.copy_to_clipboard(&text);
                }
                AppAction::Continue => {}
            }
        }

        if app.wants_key_releases() != asked_key_releases {
            asked_key_releases = !asked_key_releases;
            let mut flags = renderer.kitty_keyboard_flags();
            if asked_key_releases {
                flags |= KEY_RELEASE_FLAGS;
            }
            if !renderer.change_kitty_keyboard_flags(flags) {
                app.set_key_releases(false);
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

/// The terminal's title (OSC 0), as [`App::window_title`] has it. The
/// terminal's own is saved on its title stack first, and put back when cue
/// exits.
struct HostTitle {
    /// The title cue gave the terminal, if it did.
    title: Option<String>,
}

impl HostTitle {
    fn apply(&mut self, app: &App) {
        let title = app.window_title();
        if self.title.as_ref() == Some(&title) {
            return;
        }
        // Push the terminal's own (XTWINOPS 22) the first time.
        let push = match self.title {
            None => "\x1b[22;0t",
            Some(_) => "",
        };
        write_to_terminal(&format!("{push}\x1b]0;{title}\x1b\\"));
        self.title = Some(title);
    }
}

impl Drop for HostTitle {
    fn drop(&mut self) {
        if self.title.is_some() {
            // Cleared first for terminals without a title stack, where
            // popping (XTWINOPS 23) does nothing.
            write_to_terminal("\x1b]0;\x1b\\\x1b[23;0t");
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

/// Serializes tests that use the native core, which is single-threaded,
/// across every module in this test binary.
#[cfg(test)]
fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a lock this process let go of is free within a second, as
/// `held` says. A `flock` lock lasts while any copy of its file is open,
/// and a child another test forks meanwhile has a copy until it execs.
#[cfg(test)]
fn let_go(held: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    while held() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}
