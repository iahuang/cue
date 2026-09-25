//! Exercises the edit buffer and editor view wrappers against the native library.

use std::sync::{Mutex, MutexGuard};

use opentui::{EditBuffer, OwnedBuffer, Rgba, WidthMethod, WrapMode};

/// The native core is single-threaded and the harness runs tests in parallel.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn typing_and_deleting() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.insert_text("helo");
    eb.move_cursor_left();
    eb.insert_text("l");
    assert_eq!(
        eb.cursor().col,
        4,
        "cursor stays after the insertion, before 'o'"
    );
    eb.move_cursor_right();
    eb.new_line();
    eb.insert_text("wörld!");
    eb.delete_char_backward();
    assert_eq!(eb.text(), "hello\nwörld");
    assert_eq!(eb.line_count(), 2);
    let c = eb.cursor();
    assert_eq!((c.row, c.col), (1, 5));

    // Backspace at the start of a line joins it with the previous one.
    eb.set_cursor(1, 0);
    eb.delete_char_backward();
    assert_eq!(eb.text(), "hellowörld");
    assert_eq!((eb.cursor().row, eb.cursor().col), (0, 5));

    eb.delete_char();
    assert_eq!(eb.text(), "helloörld");
}

#[test]
fn logical_navigation_clamps_to_line_ends() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text("a long line\nab\nlonger line here");
    eb.set_cursor(0, 8);
    eb.move_cursor_down();
    assert_eq!((eb.cursor().row, eb.cursor().col), (1, 2));
    eb.move_cursor_right();
    assert_eq!(
        (eb.cursor().row, eb.cursor().col),
        (2, 0),
        "right at EOL wraps to next line"
    );
    eb.move_cursor_left();
    assert_eq!((eb.cursor().row, eb.cursor().col), (1, 2));
}

#[test]
fn view_scrolls_to_keep_the_cursor_visible() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    let text: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
    eb.set_text(&text.join("\n"));
    let view = eb.view(20, 5).unwrap();
    view.set_wrap_mode(WrapMode::None);

    for _ in 0..30 {
        view.move_down_visual();
    }
    assert_eq!(eb.cursor().row, 30);
    let vc = view.visual_cursor();
    assert!(
        vc.row < 5,
        "cursor row {} outside a 5-line viewport",
        vc.row
    );
    assert_eq!(vc.logical_row, 30);

    let screen = OwnedBuffer::new(20, 5, false, WidthMethod::Unicode, "view").unwrap();
    screen.clear(Rgba::BLACK);
    screen.draw_editor_view(&view, 0, 0);
    let rows: Vec<String> = screen
        .to_text(true)
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    assert_eq!(rows[vc.row as usize], "line 30");
}

#[test]
fn visual_line_start_and_end_follow_wrapping() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text("aaaa bbbb cccc");
    let view = eb.view(5, 5).unwrap();
    view.set_wrap_mode(WrapMode::Char);
    // Wrapped as "aaaa " "bbbb " "cccc".
    eb.set_cursor(0, 7);
    view.move_to_visual_line_start();
    assert_eq!(eb.cursor().col, 5);
    view.move_to_visual_line_end();
    let vc = view.visual_cursor();
    assert_eq!(vc.row, 1);
    assert!(
        eb.cursor().col >= 9 && eb.cursor().col <= 10,
        "col {}",
        eb.cursor().col
    );
}

#[test]
fn offsets_ranges_and_deletes() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text("héllo 漢字\nsecond\tx\nend");
    // Offsets count display columns (漢 and 字 are two wide) plus one per newline.
    assert_eq!(eb.position_to_offset(1, 0), 11);
    // A tab counts as its display width: 2 by default.
    assert_eq!(eb.position_to_offset(2, 0), 11 + 6 + 2 + 1 + 1);
    let pos = eb.offset_to_position(8).unwrap();
    assert_eq!((pos.row, pos.col), (0, 8));
    assert_eq!(eb.offset_to_position(10_000), None);

    assert_eq!(eb.text_range(6, 10), "漢字");
    assert_eq!(eb.text_range(10, 17), "\nsecond");
    assert_eq!(
        eb.text_range(17, 10),
        "\nsecond",
        "reversed bounds are normalized"
    );
    assert_eq!(eb.text_range(3, 3), "");

    eb.set_cursor_by_offset(13);
    assert_eq!((eb.cursor().row, eb.cursor().col), (1, 2));

    assert_eq!(eb.delete_range((0, 5), (1, 6)), 1);
    assert_eq!(eb.text(), "héllo\tx\nend");
    assert_eq!((eb.cursor().row, eb.cursor().col), (0, 5));
    assert_eq!(eb.delete_range((0, 1), (0, 1)), 0);
    assert!(eb.undo());
    assert_eq!(eb.text(), "héllo 漢字\nsecond\tx\nend");
}

#[test]
fn shared_view_keeps_its_buffer_alive() {
    let _serial = serial();
    let view = {
        let eb = std::rc::Rc::new(EditBuffer::new(WidthMethod::Unicode).unwrap());
        eb.set_text("kept");
        eb.shared_view(10, 2).unwrap()
    };
    let screen = OwnedBuffer::new(10, 2, false, WidthMethod::Unicode, "test").unwrap();
    screen.clear(Rgba::BLACK);
    screen.draw_editor_view(&view, 0, 0);
    assert!(screen.to_text(true).starts_with("kept"));
}
