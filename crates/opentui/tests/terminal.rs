//! Exercises `EmbeddedTerminal` against the real native library.

use std::sync::{Mutex, MutexGuard};

use opentui::{
    Attributes, CursorStyle, EmbeddedTerminal, KeyEvent, KeyMods, MouseAction, MouseButton,
    MouseEvent, OwnedBuffer, Rgba, WidthMethod,
};

/// The native core is single-threaded (see `Error::WrongThread`).
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn buffer(width: u32, height: u32) -> OwnedBuffer {
    let buffer = OwnedBuffer::new(width, height, false, WidthMethod::Unicode, "test").unwrap();
    buffer.clear(Rgba::BLACK);
    buffer
}

#[test]
fn draws_output_where_asked_within_the_clip() {
    let _serial = serial();
    let mut term = EmbeddedTerminal::new(6, 2, 1000).unwrap();
    term.write(b"ab\r\n\x1b[1mcd").unwrap();
    let frame = buffer(10, 3);
    frame.draw_text("..........", 0, 0, Rgba::WHITE, None, Attributes::NONE);
    frame.with_clip(1, 0, 3, 3, || term.draw(&frame, 1, 1));
    let text = frame.to_text(true);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows[0], "..........", "above the terminal is untouched");
    assert_eq!(rows[1].trim_end(), " ab", "{text}");
    assert_eq!(rows[2].trim_end(), " cd", "clipped past column 4: {text}");

    // Drawing again redraws everything, not just what changed.
    let frame = buffer(6, 2);
    term.draw(&frame, 0, 0);
    assert!(frame.to_text(true).starts_with("ab"));
}

#[test]
fn host_palette_keeps_the_host_terminals_colors() {
    let _serial = serial();
    let mut term = EmbeddedTerminal::new(4, 1, 1000).unwrap();
    term.set_host_palette(true);
    term.write(b"a\x1b[32mb").unwrap();
    let frame = buffer(4, 1);
    term.draw(&frame, 0, 0);
    assert!(frame.fg_at(0, 0).unwrap().is_terminal_default());
    assert!(frame.bg_at(0, 0).unwrap().is_terminal_default());
    assert_eq!(frame.fg_at(1, 0).unwrap().palette_index(), Some(2));
}

#[test]
fn encodes_input_for_the_programs_modes() {
    let _serial = serial();
    let mut term = EmbeddedTerminal::new(10, 3, 1000).unwrap();
    let ctrl_c = KeyEvent::press("KeyC", KeyMods::CTRL, "c", Some('c'));
    assert_eq!(term.encode_key(&ctrl_c), b"\x03");
    let alt_b = KeyEvent::press("KeyB", KeyMods::ALT, "b", Some('b'));
    assert_eq!(term.encode_key(&alt_b), b"\x1bb");
    let up = KeyEvent::press("ArrowUp", KeyMods::NONE, "", None);
    assert_eq!(term.encode_key(&up), b"\x1b[A");
    let text = KeyEvent::press("", KeyMods::NONE, "é", None);
    assert_eq!(term.encode_key(&text), "é".as_bytes());

    // Application cursor keys, then the kitty keyboard protocol.
    term.write(b"\x1b[?1h").unwrap();
    assert_eq!(term.encode_key(&up), b"\x1bOA");
    term.write(b"\x1b[>1u").unwrap();
    let esc = KeyEvent::press("Escape", KeyMods::NONE, "", None);
    assert_eq!(term.encode_key(&esc), b"\x1b[27u");

    let click = MouseEvent {
        action: MouseAction::Press,
        button: Some(MouseButton::Left),
        mods: KeyMods::NONE,
        x: 2,
        y: 1,
        any_button_pressed: true,
    };
    assert!(
        term.encode_mouse(&click).is_empty(),
        "no mouse reports asked for"
    );
    term.write(b"\x1b[?1000h\x1b[?1006h").unwrap();
    assert_eq!(term.encode_mouse(&click), b"\x1b[<0;3;2M");

    assert_eq!(term.encode_paste("a\nb"), b"a\rb");
    term.write(b"\x1b[?2004h").unwrap();
    assert_eq!(term.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");

    assert!(term.encode_focus(true).is_empty());
    term.write(b"\x1b[?1004h").unwrap();
    assert_eq!(term.encode_focus(false), b"\x1b[O");
}

#[test]
fn hands_over_clipboard_writes() {
    let _serial = serial();
    let mut term = EmbeddedTerminal::new(10, 3, 1000).unwrap();
    assert_eq!(term.take_clipboard(), None);
    // Longer than the first read's buffer.
    let text = "clip ".repeat(100);
    let encoded = base64(text.as_bytes());
    term.write(format!("\x1b]52;c;{encoded}\x1b\\").as_bytes())
        .unwrap();
    assert_eq!(term.take_clipboard().as_deref(), Some(text.as_str()));
    assert_eq!(term.take_clipboard(), None);
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn answers_queries_and_reports_state() {
    let _serial = serial();
    let mut term = EmbeddedTerminal::new(10, 3, 1000).unwrap();
    term.write(b"ab\x1b[6n").unwrap();
    assert_eq!(term.drain_responses(), b"\x1b[1;3R");
    assert!(term.drain_responses().is_empty());

    term.write(b"\x1b]2;make test\x07\x1b[5 q").unwrap();
    assert_eq!(term.title(), "make test");
    term.draw(&buffer(10, 3), 0, 0);
    let cursor = term.cursor();
    assert_eq!(cursor.position, Some((2, 0)));
    assert!(cursor.visible);
    assert_eq!(cursor.style, CursorStyle::Bar);

    assert!(!term.is_alternate_screen());
    term.write(b"\x1b[?1049h").unwrap();
    assert!(term.is_alternate_screen());
    term.write(b"\x1b[?1049l").unwrap();

    term.write(b"\r\nline two").unwrap();
    term.set_selection((0, 1), (7, 1)).unwrap();
    assert_eq!(term.selected_text(), "line two");
    term.clear_selection();
    assert_eq!(term.selected_text(), "");

    assert!(term.resize(0, 3).is_err());
    term.resize(20, 4).unwrap();
}
