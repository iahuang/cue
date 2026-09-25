//! Pins down the native undo and selection behavior the wrappers rely on.

use std::sync::{Mutex, MutexGuard};

use opentui::{EditBuffer, Rgba, SelectionBehavior, SelectionColors, WidthMethod, WrapMode};

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const COLORS: SelectionColors = SelectionColors {
    bg: Rgba::rgb(60, 60, 120),
    fg: None,
};

/// Runs `edit` on a fresh buffer holding `text` with the cursor at
/// (`row`, `col`), then checks that undoing exactly the reported number of
/// snapshots restores the text and exhausts the history.
fn assert_steps_exact(text: &str, (row, col): (u32, u32), edit: impl FnOnce(&EditBuffer) -> u32) {
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text(text);
    eb.set_cursor(row, col);
    assert!(!eb.can_undo(), "set_text clears history");
    let steps = edit(&eb);
    for i in 0..steps {
        assert!(eb.undo(), "undo {i} of {steps} failed");
    }
    assert_eq!(eb.text(), text, "text restored after {steps} undos");
    assert!(!eb.can_undo(), "more snapshots than the {steps} reported");
}

#[test]
fn edits_report_exact_snapshot_counts() {
    let _serial = serial();
    assert_steps_exact("abc", (0, 1), |eb| eb.insert_text("xyz"));
    assert_steps_exact("abc", (0, 1), |eb| eb.insert_text(""));
    assert_steps_exact("abc", (0, 1), |eb| eb.new_line());
    assert_steps_exact("abc", (0, 2), |eb| eb.delete_char_backward());
    assert_steps_exact("abc", (0, 0), |eb| eb.delete_char_backward());
    assert_steps_exact("a\nb", (1, 0), |eb| eb.delete_char_backward());
    assert_steps_exact("abc", (0, 1), |eb| eb.delete_char());
    assert_steps_exact("abc", (0, 3), |eb| eb.delete_char());
    assert_steps_exact("a\nb", (0, 1), |eb| eb.delete_char());
    assert_steps_exact("héllo wörld", (0, 1), |eb| {
        let view = eb.view(40, 5).unwrap();
        view.set_selection(1, 8, COLORS);
        view.delete_selected_text()
    });
    assert_steps_exact("abc", (0, 1), |eb| {
        let view = eb.view(40, 5).unwrap();
        view.delete_selected_text()
    });
}

#[test]
fn undo_restores_cursor_and_redo_is_dropped_by_new_edits() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.insert_text("one");
    eb.insert_text(" two");
    assert!(eb.undo());
    assert_eq!(eb.text(), "one");
    assert_eq!(eb.cursor().col, 3);
    assert!(eb.can_redo());
    assert!(eb.redo());
    assert_eq!(eb.text(), "one two");
    assert!(eb.undo());

    eb.insert_text("!");
    assert!(!eb.can_redo(), "a new edit discards the redo history");
    assert!(!eb.redo());
    assert!(eb.undo());
    assert!(eb.undo());
    assert_eq!(eb.text(), "");
    assert!(!eb.undo());
}

#[test]
fn selection_offsets_match_cursor_offsets() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text("héllo\nwörld ✓ end");
    let view = eb.view(40, 5).unwrap();

    eb.set_cursor(1, 3);
    let end = eb.cursor().offset;
    view.set_selection(2, end, COLORS);
    assert_eq!(view.selection(), Some((2, end)));
    assert_eq!(view.selected_text(), "llo\nwör");

    // Reversed order is normalized.
    view.set_selection(end, 2, COLORS);
    assert_eq!(view.selected_text(), "llo\nwör");

    view.clear_selection();
    assert_eq!(view.selection(), None);
    assert_eq!(view.selected_text(), "");
}

#[test]
fn mouse_style_local_selection() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    eb.set_text("hello world\nsecond line");
    let view = eb.view(40, 5).unwrap();
    view.set_wrap_mode(WrapMode::None);

    // Press: moves the cursor, selects nothing.
    view.set_local_selection((3, 0), (3, 0), SelectionBehavior::Cell, true, COLORS);
    assert_eq!((eb.cursor().row, eb.cursor().col), (0, 3));
    assert!(
        matches!(view.selection(), None | Some((3, 3))),
        "{:?}",
        view.selection()
    );

    // Drag onto the next line. The cell under the pointer is included in the
    // selection, but the cursor stays on that cell rather than after it.
    view.update_local_selection((3, 0), (6, 1), SelectionBehavior::Cell, true, COLORS);
    assert_eq!(view.selected_text(), "lo world\nsecond ");
    assert_eq!((eb.cursor().row, eb.cursor().col), (1, 6));

    // Double click snaps to the word, triple click to the line.
    view.set_local_selection((8, 0), (8, 0), SelectionBehavior::Word, true, COLORS);
    assert_eq!(view.selected_text(), "world");
    view.set_local_selection((2, 1), (2, 1), SelectionBehavior::Line, true, COLORS);
    assert!(
        view.selected_text().starts_with("second line"),
        "{:?}",
        view.selected_text()
    );
}

#[test]
fn scrolling_the_viewport_can_move_the_cursor() {
    let _serial = serial();
    let eb = EditBuffer::new(WidthMethod::Unicode).unwrap();
    let text: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
    eb.set_text(&text.join("\n"));
    eb.set_cursor(0, 0);
    let view = eb.view(20, 10).unwrap();
    assert_eq!(view.total_virtual_line_count(), 100);

    view.scroll_to(0, 50, true);
    let vc = view.visual_cursor();
    assert_eq!(view.viewport().y, 50, "scroll kept after layout");
    assert!(
        vc.row < 10 && eb.cursor().row >= 50,
        "cursor moved into view: {vc:?}"
    );
}
