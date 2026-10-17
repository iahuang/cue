//! Timings of opening, scrolling, typing in, and finding in a big file, as
//! the app does them: each step handles an event and draws a frame. Run
//! with a release build:
//!
//! ```sh
//! CUE_BENCH_FILE=/path/to/big.log cargo test --release -p cue bench_big_file -- --ignored --nocapture
//! ```

use super::*;
use crate::input::{KeyCode, Mods};
use opentui::{OwnedBuffer, WidthMethod};
use std::time::Duration;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 50;

struct Timings {
    name: &'static str,
    times: Vec<Duration>,
}

impl Timings {
    fn new(name: &'static str) -> Timings {
        Timings {
            name,
            times: Vec::new(),
        }
    }

    fn report(&mut self) {
        self.times.sort();
        let n = self.times.len();
        let total: Duration = self.times.iter().sum();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        eprintln!(
            "{:<28} n={n:<4} mean {:>9.2} ms  p50 {:>9.2} ms  max {:>9.2} ms",
            self.name,
            ms(total / n.max(1) as u32),
            ms(self.times[n / 2]),
            ms(self.times[n - 1]),
        );
    }
}

fn max_rss_mb() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    // Bytes on macOS.
    usage.ru_maxrss as f64 / (1 << 20) as f64
}

fn frame(app: &App) {
    let frame = OwnedBuffer::new(WIDTH, HEIGHT, false, WidthMethod::Unicode, "bench").unwrap();
    app.draw(&frame);
}

thread_local! {
    /// How long what waited for idle time took after each step that left
    /// any, and drawing again after it.
    static IDLE: std::cell::RefCell<Vec<Duration>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Handles one event and draws the frame after it, as the main loop does,
/// then does what waits for idle time, as the main loop does before the
/// next event if it doesn't come at once.
fn step(app: &mut App, timings: &mut Timings, event: impl FnOnce(&mut App)) {
    let start = Instant::now();
    event(app);
    app.poll();
    frame(app);
    timings.times.push(start.elapsed());
    let start = Instant::now();
    if app.idle() {
        frame(app);
        IDLE.with(|idle| idle.borrow_mut().push(start.elapsed()));
    }
}

fn report_idle() {
    let times = IDLE.with(|idle| std::mem::take(&mut *idle.borrow_mut()));
    if !times.is_empty() {
        Timings {
            name: "idle work after steps",
            times,
        }
        .report();
    }
}

fn press(app: &mut App, code: KeyCode, mods: Mods) {
    app.handle_key(Key::new(code, mods));
}

fn wheel(app: &mut App, kind: MouseKind) {
    let mouse = Mouse {
        kind,
        x: WIDTH / 2,
        y: HEIGHT / 2,
        mods: Mods::NONE,
    };
    app.handle_mouse(mouse, Instant::now());
}

#[test]
#[ignore]
fn bench_big_file() {
    let Some(path) = std::env::var_os("CUE_BENCH_FILE").map(PathBuf::from) else {
        eprintln!("set CUE_BENCH_FILE");
        return;
    };
    let query = std::env::var("CUE_BENCH_QUERY").unwrap_or_else(|_| "lambda".into());
    let root = path.parent().unwrap().to_path_buf();
    eprintln!(
        "{} ({} bytes)",
        path.display(),
        fs::metadata(&path).unwrap().len()
    );

    if std::env::var("CUE_BENCH_ONLY").as_deref() == Ok("open") {
        let mut open = Timings::new("open and draw");
        for _ in 0..8 {
            let start = Instant::now();
            let workspace = Workspace::new([root.clone()]).unwrap();
            let app = App::new(workspace, Some(path.clone()), WIDTH, HEIGHT).unwrap();
            frame(&app);
            open.times.push(start.elapsed());
        }
        open.report();
        return;
    }
    let mut open = Timings::new("open");
    let start = Instant::now();
    let workspace = Workspace::new([root]).unwrap();
    let mut app = App::new(workspace, Some(path), WIDTH, HEIGHT).unwrap();
    open.times.push(start.elapsed());
    open.report();
    let mut first = Timings::new("first frame");
    step(&mut app, &mut first, |_| {});
    first.report();
    let lines = app.editor().unwrap().document().buffer.line_count();
    eprintln!("lines: {lines}, max rss {:.0} MB", max_rss_mb());

    // For profiling: repeats one step for a while.
    if let Ok(only) = std::env::var("CUE_BENCH_ONLY") {
        app.editor_mut().unwrap().jump_to((lines / 2, 0));
        if only == "find" {
            press(&mut app, KeyCode::Char('f'), Mods::CTRL);
        }
        let mut timings = Timings::new("repeated");
        for i in 0..200 {
            step(&mut app, &mut timings, |app| match only.as_str() {
                "wheel" => wheel(app, MouseKind::ScrollDown),
                "type" => press(app, KeyCode::Char('x'), Mods::NONE),
                "enter" => press(app, KeyCode::Enter, Mods::NONE),
                // Typing a word, then undoing it.
                "undo" => {
                    let code = if i % 2 == 0 {
                        KeyCode::Char('x')
                    } else {
                        KeyCode::Char('z')
                    };
                    let mods = if i % 2 == 0 { Mods::NONE } else { Mods::CTRL };
                    for _ in 0..if i % 2 == 0 { 8 } else { 1 } {
                        press(app, code, mods);
                    }
                }
                "find" => {
                    let code = if i % 2 == 0 {
                        KeyCode::Char('a')
                    } else {
                        KeyCode::Backspace
                    };
                    press(app, code, Mods::NONE)
                }
                _ => panic!("CUE_BENCH_ONLY is open, wheel, type, enter, undo, or find"),
            });
        }
        timings.report();
        report_idle();
        return;
    }

    let mut timings = Timings::new("wheel down");
    for _ in 0..100 {
        step(&mut app, &mut timings, |app| {
            wheel(app, MouseKind::ScrollDown)
        });
    }
    timings.report();
    let mut timings = Timings::new("page down");
    for _ in 0..50 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::PageDown, Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("cursor down");
    for _ in 0..50 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Down, Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("ctrl+end / ctrl+home");
    for i in 0..10 {
        let code = if i % 2 == 0 {
            KeyCode::End
        } else {
            KeyCode::Home
        };
        step(&mut app, &mut timings, |app| press(app, code, Mods::CTRL));
    }
    timings.report();

    // Type in the middle of the file.
    let mut timings = Timings::new("jump to middle");
    step(&mut app, &mut timings, |app| {
        app.editor_mut().unwrap().jump_to((lines / 2, 0))
    });
    timings.report();
    let mut timings = Timings::new("type a character");
    for c in "hello world, typing a few words here".chars() {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Char(c), Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("enter");
    for _ in 0..10 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Enter, Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("backspace");
    for _ in 0..20 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Backspace, Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("undo");
    for _ in 0..5 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Char('z'), Mods::CTRL)
        });
    }
    timings.report();
    let mut timings = Timings::new("wheel after edits");
    for _ in 0..30 {
        step(&mut app, &mut timings, |app| {
            wheel(app, MouseKind::ScrollDown)
        });
    }
    timings.report();
    eprintln!("max rss {:.0} MB", max_rss_mb());

    // Find, typing the query a character at a time.
    let mut timings = Timings::new("open find");
    step(&mut app, &mut timings, |app| {
        press(app, KeyCode::Char('f'), Mods::CTRL)
    });
    timings.report();
    let mut timings = Timings::new("type in find");
    for c in query.chars() {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Char(c), Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("next match");
    for _ in 0..20 {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Enter, Mods::NONE)
        });
    }
    timings.report();
    let mut timings = Timings::new("wheel with find open");
    for _ in 0..30 {
        step(&mut app, &mut timings, |app| {
            wheel(app, MouseKind::ScrollDown)
        });
    }
    timings.report();
    let mut timings = Timings::new("close find");
    step(&mut app, &mut timings, |app| {
        press(app, KeyCode::Esc, Mods::NONE)
    });
    timings.report();
    let mut timings = Timings::new("type after find");
    for c in "more".chars() {
        step(&mut app, &mut timings, |app| {
            press(app, KeyCode::Char(c), Mods::NONE)
        });
    }
    timings.report();
    report_idle();
    eprintln!("max rss {:.0} MB", max_rss_mb());
}
