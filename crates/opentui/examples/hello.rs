//! Draws a few frames to the terminal, then restores it.
//!
//! cargo run -p opentui --example hello

use std::thread::sleep;
use std::time::Duration;

use opentui::{Attributes, Output, Renderer, Rgba, TextBuffer, WidthMethod, WrapMode};

fn main() -> opentui::Result<()> {
    let (width, height) = (48, 12);
    let mut renderer = Renderer::new(width, height, Output::Stdout)?;
    renderer.setup_terminal(true);

    let text = TextBuffer::new(WidthMethod::Unicode)?;
    text.set_text("OpenTUI's Zig core, driven from Rust. This paragraph is wrapped by a native text buffer view.");
    text.set_default_fg(Some(Rgba::rgb(200, 200, 200)));
    let view = text.view()?;
    view.set_wrap_mode(WrapMode::Word);
    view.set_wrap_width(Some(width - 4));

    let panel = Rgba::rgb(30, 30, 46);
    for tick in 0..30u32 {
        {
            let frame = renderer.next_buffer()?;
            // SGR 49: the terminal's own background, not a hardcoded black.
            frame.clear(Rgba::terminal_default([0, 0, 0]));
            frame.fill_rect(1, 1, width - 2, height - 2, panel);
            frame.draw_text(
                "hello from rust",
                2,
                2,
                Rgba::rgb(137, 180, 250),
                None,
                Attributes::BOLD,
            );
            frame.draw_text_buffer_view(&view, 2, 4);
            let bar = "█".repeat((tick as usize * (width as usize - 4)) / 29);
            frame.draw_text(
                &bar,
                2,
                height - 3,
                Rgba::indexed(114),
                None,
                Attributes::NONE,
            );
        }
        renderer.render(false);
        // Nothing else reads stdin here, so hand the query replies to the renderer.
        renderer.poll_terminal_responses().ok();
        sleep(Duration::from_millis(50));
    }
    sleep(Duration::from_millis(500));
    Ok(())
}
