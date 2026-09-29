//! Exercises the wrappers against the real native library with memory output.

use std::sync::{Mutex, MutexGuard};

use opentui::{
    Attributes, Error, Image, Output, OwnedBuffer, RenderStatus, Renderer, Rgba, TextBuffer,
    WidthMethod, WrapMode,
};

/// The native core is single-threaded (see `Error::WrongThread`) and the test
/// harness runs tests on parallel threads.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn draws_text_into_the_frame() {
    let _serial = serial();
    let mut renderer = Renderer::new(12, 3, Output::Memory).unwrap();
    {
        let frame = renderer.next_buffer().unwrap();
        assert_eq!((frame.width(), frame.height()), (12, 3));
        frame.clear(Rgba::BLACK);
        frame.draw_text("hello", 1, 1, Rgba::WHITE, None, Attributes::BOLD);
        frame.draw_char(
            '✓',
            7,
            1,
            Rgba::rgb(0, 255, 0),
            Rgba::BLACK,
            Attributes::NONE,
        );
        assert_eq!(
            frame.to_text(true).lines().nth(1).unwrap().trim_end(),
            " hello ✓"
        );
    }
    assert_eq!(renderer.render(true), RenderStatus::Rendered);
}

/// A 4x2 opaque red PNG.
const RED_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 127, 168, 125, 99, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 25,
    51, 160, 11, 0, 0, 15, 33, 15, 241, 4, 55, 198, 159, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];

#[test]
fn decodes_and_draws_images() {
    let _serial = serial();
    let image = Image::decode(RED_PNG).unwrap();
    assert_eq!((image.width(), image.height()), (4, 2));
    assert_eq!(image.format(), Some("PNG"));
    assert!(matches!(Image::decode(b"not an image"), Err(Error::Image(_))));

    let mut renderer = Renderer::new(12, 4, Output::Memory).unwrap();
    {
        let frame = renderer.next_buffer().unwrap();
        frame.clear(Rgba::BLACK);
        assert!(frame.draw_image(&image, 1, 1, 4, 2, 0, 0));
        // Entirely outside the clip: nothing to draw.
        let drawn = frame.with_clip(8, 0, 4, 4, || frame.draw_image(&image, 1, 1, 4, 2, 0, 0));
        assert!(!drawn);
    }
    // Dropping the image while the frame still has it is fine.
    drop(image);
    assert_eq!(renderer.render(true), RenderStatus::Rendered);
}

#[test]
fn zero_sized_renderer_is_an_error() {
    let _serial = serial();
    assert_eq!(
        Renderer::new(0, 5, Output::Memory).err(),
        Some(Error::CreateFailed("renderer"))
    );
}

#[test]
fn text_buffer_round_trips_and_wraps() {
    let _serial = serial();
    let text = TextBuffer::new(WidthMethod::Unicode).unwrap();
    text.set_text("abcdefghij\nxyz");
    assert_eq!(text.text(), "abcdefghij\nxyz");
    assert_eq!(text.line_count(), 2);

    let view = text.view().unwrap();
    view.set_wrap_mode(WrapMode::Char);
    view.set_wrap_width(Some(4));
    // "abcd" "efgh" "ij" "xyz"
    assert_eq!(view.virtual_line_count(), 4);

    let buffer = OwnedBuffer::new(4, 4, false, WidthMethod::Unicode, "wrap-test").unwrap();
    buffer.clear(Rgba::BLACK);
    buffer.draw_text_buffer_view(&view, 0, 0);
    assert_eq!(
        buffer
            .to_text(true)
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>(),
        ["abcd", "efgh", "ij", "xyz"]
    );

    // Edits are visible to an existing view.
    text.append("!!");
    assert_eq!(text.text(), "abcdefghij\nxyz!!");
    // ... "xyz!" "!"
    assert_eq!(view.virtual_line_count(), 5);
}

#[test]
fn objects_are_confined_to_one_thread() {
    let _serial = serial();
    let _text = TextBuffer::new(WidthMethod::Unicode).unwrap();
    let other = std::thread::spawn(|| TextBuffer::new(WidthMethod::Unicode).err())
        .join()
        .unwrap();
    assert_eq!(other, Some(Error::WrongThread));
}

#[test]
fn with_clip_keeps_drawing_inside_the_rectangle() {
    let _serial = serial();
    let buffer = OwnedBuffer::new(8, 2, false, WidthMethod::Unicode, "clip").unwrap();
    buffer.clear(Rgba::BLACK);
    buffer.with_clip(2, 0, 3, 1, || {
        buffer.draw_text("abcdefgh", 0, 0, Rgba::WHITE, None, Attributes::NONE);
        buffer.draw_text("abcdefgh", 0, 1, Rgba::WHITE, None, Attributes::NONE);
    });
    buffer.draw_text("after", 0, 1, Rgba::WHITE, None, Attributes::NONE);
    assert_eq!(buffer.to_text(true), "  cde   \nafter   \n");
}
