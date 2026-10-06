//! Quick Look, as in the Finder: Space in the sidebar, or Alt+click on an
//! entry, floats a preview of it over the panels, and Space again, or Esc,
//! puts it away. It follows the selection as it moves, so arrowing or
//! clicking through the tree looks through the files without opening any.
//!
//! Text is shown as the editor shows it, syntax highlighted, and Markdown
//! as reader mode does; images as an image panel does; and a folder as a
//! list of what's in it. The wheel scrolls it, or zooms an image. It's
//! only for looking: a file shown here isn't opened, and keeps no cursor.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use opentui::{Attributes, Buffer};

use crate::document::Document;
use crate::editor::Editor;
use crate::file_dialog::human_size;
use crate::icons;
use crate::image::{self, ImageView};
use crate::input::{Mouse, MouseKind};
use crate::keymap::Keymap;
use crate::layout::Rect;
use crate::picker::{self, Area};
use crate::status::Status;
use crate::theme::{self, Theme};
use crate::tree;

/// The widest it gets, in columns.
const MAX_WIDTH: u32 = 120;
/// Narrower than this, it takes what there is.
const MIN_WIDTH: u32 = 30;
/// Files larger than this aren't read.
const MAX_BYTES: u64 = 16 << 20;
/// Rows the mouse wheel scrolls a folder.
const WHEEL_ROWS: usize = 3;

/// What's shown inside the box.
enum Body {
    Text(Box<Editor>),
    Image(ImageView),
    /// A folder's entries, folders first: their names and whether each is
    /// a folder, and the first row on screen.
    Folder(Vec<(String, bool)>, usize),
    /// Why there's nothing to show.
    Note(String),
}

pub struct QuickLook {
    path: PathBuf,
    /// Under the title: where it is and how big.
    about: String,
    body: Body,
    area: Area,
}

impl QuickLook {
    /// A look at `path`, as shown by `shown`, floating in `bounds`.
    pub fn new(path: &Path, shown: String, theme: &Rc<Theme>, bounds: Rect) -> QuickLook {
        let (body, about) = load(path, theme);
        let about = match about {
            Some(about) if !shown.is_empty() => format!("{shown}  ·  {about}"),
            Some(about) => about,
            None => shown,
        };
        let mut look = QuickLook {
            path: path.to_path_buf(),
            about,
            body,
            area: place(bounds),
        };
        look.set_bounds(bounds);
        look
    }

    /// What's being looked at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Floats it in `bounds`, as the screen resizes.
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.area = place(bounds);
        let body = self.body_area();
        if let Body::Text(editor) = &mut self.body {
            editor.set_area(body.x, body.y, body.width, body.height);
        }
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        self.area.contains(x, y)
    }

    /// Where it is on screen.
    #[cfg(test)]
    pub fn area(&self) -> Area {
        self.area
    }

    /// The box's inside, below the line about the file.
    fn body_area(&self) -> Rect {
        let Area {
            x,
            y,
            width,
            height,
        } = self.area;
        Rect {
            x: x + 1,
            y: y + 3,
            width: width.saturating_sub(2).max(1),
            height: height.saturating_sub(4).max(1),
        }
    }

    /// The wheel scrolls it, or zooms an image; dragging moves a zoomed
    /// image. Clicks do nothing else.
    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) {
        let area = self.body_area();
        let wheel = matches!(
            mouse.kind,
            MouseKind::ScrollUp
                | MouseKind::ScrollDown
                | MouseKind::ScrollLeft
                | MouseKind::ScrollRight
        );
        match &mut self.body {
            Body::Image(image) => image.handle_mouse(mouse, area),
            Body::Text(editor) if wheel => {
                let local = Mouse {
                    x: mouse.x.saturating_sub(area.x),
                    y: mouse.y.saturating_sub(area.y),
                    ..mouse
                };
                editor.handle_mouse(local, now);
            }
            Body::Folder(entries, scroll) => {
                let max = entries.len().saturating_sub(area.height as usize);
                match mouse.kind {
                    MouseKind::ScrollUp => *scroll = scroll.saturating_sub(WHEEL_ROWS),
                    MouseKind::ScrollDown => *scroll = (*scroll + WHEEL_ROWS).min(max),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn draw(&self, frame: &Buffer, keymap: &Keymap) {
        let area = self.area;
        if area.width < 8 || area.height < 5 {
            return;
        }
        let colors = theme::colors();
        frame.with_clip(area.x, area.y, area.width, area.height, || {
            let name = self.path.file_name().map_or_else(
                || self.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            let room = area.width.saturating_sub(6) as usize;
            picker::draw_frame(frame, area, &tree::truncate(&name, room));
            let about = match &self.body {
                Body::Image(image) => match image.status(self.body_area()) {
                    Status::Info(info) => format!("{}  ·  {info}", self.about),
                    _ => self.about.clone(),
                },
                _ => self.about.clone(),
            };
            let room = area.width.saturating_sub(4) as usize;
            frame.draw_text(
                &tree::truncate(&about, room),
                area.x + 2,
                area.y + 1,
                colors.muted,
                None,
                Attributes::NONE,
            );
            if let Some(key) = keymap.shortcut(crate::keymap::Command::TreeQuickLook) {
                picker::draw_status(frame, area, &format!(" {key} to close "));
            }
        });
        let body = self.body_area();
        frame.with_clip(body.x, body.y, body.width, body.height, || {
            match &self.body {
                Body::Text(editor) => {
                    editor.draw(frame, keymap);
                }
                Body::Image(image) => image.draw(frame, body),
                Body::Folder(entries, scroll) => draw_folder(frame, body, entries, *scroll),
                Body::Note(note) => {
                    let width = note.chars().count() as u32;
                    let x = body.x + body.width.saturating_sub(width) / 2;
                    let y = body.y + body.height.saturating_sub(1) / 2;
                    frame.draw_text(note, x, y, colors.muted, None, Attributes::NONE);
                }
            }
        });
    }
}

/// Where it floats in `bounds`: in the middle, with a margin around it
/// where there's room for one.
fn place(bounds: Rect) -> Area {
    let width = bounds
        .width
        .saturating_sub(8)
        .min(MAX_WIDTH)
        .max(bounds.width.min(MIN_WIDTH));
    let height = bounds.height.saturating_sub(4).max(bounds.height.min(12));
    Area {
        x: bounds.x + (bounds.width - width) / 2,
        y: bounds.y + (bounds.height - height) / 2,
        width,
        height,
    }
}

/// Reads what's at `path`, and says how big it is, if that's known.
fn load(path: &Path, theme: &Rc<Theme>) -> (Body, Option<String>) {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) => return (Body::Note(err.to_string()), None),
    };
    if meta.is_dir() {
        return match list(path) {
            Ok(entries) => {
                let about = match entries.len() {
                    1 => "1 item".to_string(),
                    n => format!("{n} items"),
                };
                (Body::Folder(entries, 0), Some(about))
            }
            Err(err) => (Body::Note(err.to_string()), None),
        };
    }
    let size = human_size(meta.len());
    if image::is_image(path) {
        // The image says its own size, with its dimensions.
        return match ImageView::open(path) {
            Ok(image) => (Body::Image(image), None),
            Err(reason) => (Body::Note(reason), Some(size)),
        };
    }
    if meta.len() > MAX_BYTES {
        return (Body::Note("Too large to preview".into()), Some(size));
    }
    let doc = match Document::open(Some(path.to_path_buf()), theme.clone()) {
        Ok((doc, _)) => doc,
        Err(_) => return (Body::Note("No preview".into()), Some(size)),
    };
    // Counted as `wc -l` would, not counting after the last line break.
    let lines = match doc.text().lines().count() {
        1 => "1 line".to_string(),
        n => format!("{n} lines"),
    };
    let mut editor = match Editor::show(doc, 1, 1) {
        Ok(editor) => editor,
        Err(err) => return (Body::Note(err.to_string()), Some(size)),
    };
    if editor.is_markdown() {
        editor.read_from(0);
    }
    (
        Body::Text(Box::new(editor)),
        Some(format!("{lines}  ·  {size}")),
    )
}

/// What's in the folder at `path` that the tree would show, folders first,
/// each by name.
fn list(path: &Path) -> std::io::Result<Vec<(String, bool)>> {
    let mut entries: Vec<(String, bool)> = fs::read_dir(path)?
        .filter_map(Result::ok)
        .filter(|entry| !tree::is_hidden(&entry.file_name()))
        .map(|entry| {
            let is_dir = entry.path().is_dir();
            (entry.file_name().to_string_lossy().into_owned(), is_dir)
        })
        .collect();
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    Ok(entries)
}

fn draw_folder(frame: &Buffer, area: Rect, entries: &[(String, bool)], scroll: usize) {
    let colors = theme::colors();
    if entries.is_empty() {
        let note = "Empty folder";
        let x = area.x + area.width.saturating_sub(note.len() as u32) / 2;
        let y = area.y + area.height.saturating_sub(1) / 2;
        frame.draw_text(note, x, y, colors.muted, None, Attributes::NONE);
        return;
    }
    let rows = entries.iter().skip(scroll).take(area.height as usize);
    for (y, (name, is_dir)) in (area.y..).zip(rows) {
        let mut x = area.x + 1;
        if icons::enabled() {
            let icon = match is_dir {
                true => icons::folder(false),
                false => icons::file(name),
            };
            x = icon.draw(frame, x, y, None);
        }
        let room = (area.x + area.width).saturating_sub(x + 1) as usize;
        let shown = match is_dir {
            true => format!("{name}/"),
            false => name.clone(),
        };
        frame.draw_text(
            &tree::truncate(&shown, room),
            x,
            y,
            colors.text,
            None,
            Attributes::NONE,
        );
    }
}
