//! An image file on screen in a panel, or a PDF's pages.
//!
//! The image starts fit to the panel, never enlarged past its own size,
//! and centered. The wheel zooms in and out around the mouse, down to
//! fitting again, and dragging moves a zoomed image around. OpenTUI's
//! renderer sends images with the Kitty graphics protocol where the
//! terminal has it, Sixel where it has that, or else as half-block
//! characters.
//!
//! A PDF's pages go one under another, a row apart, as large as fits each
//! of them in the panel. The wheel and the up and down arrows scroll
//! through them, Shift with the wheel or the wheel sideways scrolls
//! across, and dragging moves them. With Ctrl, the wheel zooms around the
//! mouse. Page Up and Page Down, or the left and right arrows, go to the
//! top of the page before and after, and Home and End to the start and
//! end, staying zoomed. Pages are rendered as large as they're drawn, so
//! that they're sharp at any zoom: whole, unless that's much more than
//! fits on screen, and then the part of them on screen and around it,
//! with the whole page as it fits besides. That happens on another
//! thread, which renders the pages just off screen too; meanwhile, what
//! was rendered last of each page is drawn, stretched to where it is now.
//!
//! When the file changes, as a PDF does when LaTeX builds it again, it's
//! read again, staying where it was scrolled and zoomed to. Until the
//! pages are rendered again, they're drawn as they were. A file that
//! doesn't read, as one half written may not, leaves what's shown as it
//! is until it changes again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use opentui::{Buffer, Image};

use crate::config;
use crate::input::{Mouse, MouseButton, MouseKind};
use crate::keymap::Command;
use crate::layout::Rect;
use crate::pdf::{Job, Pdf, Renderer};
use crate::status::Status;
use crate::tty;

/// File extensions opened as images rather than as text.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "pdf"];
/// A cell's size in pixels when the terminal doesn't say.
const DEFAULT_CELL: (u32, u32) = (8, 16);
/// How much a step of the wheel zooms.
const ZOOM_STEP: f64 = 1.1;
/// How large an image is drawn: fit, but not enlarged, and enlarged as far
/// as each of its pixels 16 screen pixels across.
const IMAGE_SCALES: Scales = Scales {
    fit: 1.0,
    max: 16.0,
};
/// How large a PDF's pages are drawn at most, in screen pixels per point.
const MAX_PAGE_SCALE: f64 = 32.0;
/// Rows between a PDF's pages.
const GAP: u32 = 1;
/// How many times the panel's cells a page may take and still be rendered
/// whole.
const WHOLE_PAGE: u64 = 2;

/// Whether `path` is opened as an image, going by its extension: a
/// picture, or a PDF.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

pub struct ImageView {
    path: PathBuf,
    shown: Shown,
    /// The file's size in bytes.
    bytes: u64,
    /// The file as last read, to tell whether it changed since.
    stamp: Option<Stamp>,
    /// An image's zoom: `None` while it's fit to the panel.
    zoom: Option<Zoom>,
    /// Where a drag that moves an image started.
    drag: Option<Drag>,
}

/// What an image view shows.
enum Shown {
    Image(Image),
    Pdf(Box<Pages>),
}

/// A PDF, its pages one under another, and how they're scrolled and
/// zoomed.
struct Pages {
    /// Each page's size in points.
    sizes: Vec<(f64, f64)>,
    /// Screen pixels per point, or `None` while the pages fit the panel.
    scale: Option<f64>,
    /// What's at the panel's top-left corner: across, in points from the
    /// widest page's left edge, and down, in pages from the first's top,
    /// each taking in the gap under it (see [`Layout::down`]). In points
    /// and pages so that it stays as the panel resizes, and kept in bounds
    /// when drawn, not here.
    at: (f64, f64),
    /// Where a drag that moves the pages started.
    drag: Option<Drag>,
    renderer: Renderer<Render>,
    /// What's rendered of the pages on screen and around them.
    rendered: RefCell<HashMap<usize, Renders>>,
    /// What was rendered of the pages before the file changed, drawn for
    /// each until any of it is rendered again.
    stale: RefCell<HashMap<usize, Renders>>,
    /// What was asked for last that isn't rendered yet.
    asked: RefCell<Vec<Render>>,
    /// The pages that can't be rendered, and why.
    failed: RefCell<HashMap<usize, String>>,
}

/// What's rendered of a page: the whole of it, and a part of it, each as
/// rendered last, at whatever size.
#[derive(Default)]
struct Renders {
    whole: Option<(Render, Rc<Image>)>,
    part: Option<(Render, Rc<Image>)>,
}

impl Renders {
    fn get(&self, layer: Layer) -> Option<&(Render, Rc<Image>)> {
        match layer {
            Layer::Whole => self.whole.as_ref(),
            Layer::Part => self.part.as_ref(),
        }
    }

    /// What's rendered, whole and in part.
    fn renders(&self) -> (Option<Render>, Option<Render>) {
        let render = |slot: &Option<(Render, Rc<Image>)>| slot.as_ref().map(|(render, _)| *render);
        (render(&self.whole), render(&self.part))
    }
}

/// What of a page is rendered, and how large.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Render {
    page: usize,
    /// The whole page's size, in cells.
    size: (u32, u32),
    /// The part of it rendered, in cells from its top-left corner.
    part: Rect,
    cell: (u32, u32),
}

/// Where a PDF's pages go at a scale: one under another, `GAP` rows
/// apart, each centered across the widest.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    /// Screen pixels per point.
    scale: f64,
    /// Each page's size in cells, and its top, in rows under the first's.
    pages: Vec<(u32, u32, i64)>,
    /// The widest page's width, and all of their height, in cells.
    cols: u32,
    rows: i64,
}

/// Where something's drawn: at screen cell (x, y), which may be outside
/// the area it's clipped to, and cols by rows cells in size.
type Cells = (i64, i64, u32, u32);

/// How large something may be drawn, in screen pixels per its own.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Scales {
    /// The most fitting the panel enlarges it.
    fit: f64,
    /// The most zooming in does.
    max: f64,
}

/// An image zoomed in past fitting its panel.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Zoom {
    /// Screen pixels per image pixel.
    scale: f64,
    /// Where the image's top-left corner is, in screen pixels from the
    /// panel's. Kept in bounds when drawn, not here, so that the image
    /// stays put as the panel resizes.
    origin: (f64, f64),
}

/// A drag moving the image or pages: the screen cell it started at, and
/// their top-left corner then, in screen pixels from the panel's.
#[derive(Debug, Clone, Copy)]
struct Drag {
    x: u32,
    y: u32,
    origin: (f64, f64),
}

/// A file as it was on disk: if it has the same time and size, it's
/// surely not changed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Option<Stamp> {
        Some(Stamp {
            modified: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

/// Where the image is drawn: at cell (`x`, `y`) of the screen, which may
/// be outside the area it's clipped to, `cols` by `rows` cells in size.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    x: i64,
    y: i64,
    cols: u32,
    rows: u32,
    /// Screen pixels per image pixel.
    scale: f64,
    /// The image's top-left corner, in screen pixels from the area's, as
    /// kept in bounds but before it's put on a cell.
    origin: (f64, f64),
}

impl ImageView {
    /// Reads and decodes the image or PDF at `path`, or says why it
    /// can't.
    pub fn open(path: &Path) -> Result<ImageView, String> {
        let (data, stamp) = read(path).map_err(|err| err.to_string())?;
        let bytes = data.len() as u64;
        Ok(ImageView {
            path: path.to_path_buf(),
            shown: decode(path, data)?,
            bytes,
            stamp,
            zoom: None,
            drag: None,
        })
    }

    /// Reads the file again if it changed since it was last read, staying
    /// as scrolled and zoomed. Returns whether it did. If it doesn't read,
    /// or it's a PDF that looks half written, what's shown stays until the
    /// file changes again.
    pub fn reload(&mut self) -> bool {
        let Ok(meta) = fs::metadata(&self.path) else {
            return false;
        };
        if Stamp::of(&meta).is_some_and(|stamp| Some(stamp) == self.stamp) {
            return false;
        }
        let Ok((data, stamp)) = read(&self.path) else {
            return false;
        };
        self.stamp = stamp;
        if is_pdf(&self.path) && !looks_whole_pdf(&data) {
            return false;
        }
        let bytes = data.len() as u64;
        let Ok(shown) = decode(&self.path, data) else {
            return false;
        };
        self.bytes = bytes;
        let old = std::mem::replace(&mut self.shown, shown);
        match (old, &mut self.shown) {
            (Shown::Pdf(old), Shown::Pdf(new)) => new.take_over(*old),
            // Zoomed into the same picture, or one the same size.
            (Shown::Image(old), Shown::Image(new)) if size(&old) == size(new) => {}
            _ => {
                self.zoom = None;
                self.drag = None;
            }
        }
        true
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Follows the file to `path`, where it was moved.
    pub fn rename(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// Draws the image or pages in `area`, clipped to it.
    pub fn draw(&self, frame: &Buffer, area: Rect) {
        let cell = cell_pixels();
        let image = match &self.shown {
            Shown::Image(image) => image,
            Shown::Pdf(pages) => return pages.draw(frame, cell, area),
        };
        let Some(placed) = place(size(image), IMAGE_SCALES, self.zoom, cell, area) else {
            return;
        };
        let (cols, rows) = (placed.cols, placed.rows);
        let (x, y) = (placed.x as i32, placed.y as i32);
        frame.draw_image(image, x, y, cols, rows, cols * cell.0, rows * cell.1);
    }

    /// The wheel zooms, around the mouse, and the left button drags the
    /// image around while it's zoomed. A PDF's pages scroll instead (see
    /// [`Pages::handle_mouse`]). `mouse` is in screen cells, and the image
    /// is in `area`.
    pub fn handle_mouse(&mut self, mouse: Mouse, area: Rect) {
        let cell = cell_pixels();
        let image = match &mut self.shown {
            Shown::Image(image) => size(image),
            Shown::Pdf(pages) => return pages.handle_mouse(mouse, cell, area),
        };
        let place = |zoom| place(image, IMAGE_SCALES, zoom, cell, area);
        match mouse.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let factor = match mouse.kind {
                    MouseKind::ScrollUp => ZOOM_STEP,
                    _ => 1.0 / ZOOM_STEP,
                };
                self.zoom = zoom_at(
                    image,
                    IMAGE_SCALES,
                    self.zoom,
                    factor,
                    (mouse.x, mouse.y),
                    cell,
                    area,
                );
                // A drag carries on from where the image is now.
                let placed = self.zoom.and(place(self.zoom));
                self.drag = self.drag.zip(placed).map(|(_, placed)| Drag {
                    x: mouse.x,
                    y: mouse.y,
                    origin: placed.origin,
                });
            }
            MouseKind::Press(MouseButton::Left) => {
                let placed = self.zoom.and(place(self.zoom));
                self.drag = placed.map(|placed| Drag {
                    x: mouse.x,
                    y: mouse.y,
                    origin: placed.origin,
                });
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let (Some(drag), Some(zoom)) = (self.drag, &mut self.zoom) {
                    zoom.origin = dragged(drag, mouse, cell);
                }
            }
            MouseKind::Release(_) => self.drag = None,
            _ => {}
        }
    }

    /// Runs an editor command on a PDF in `area`: the ones that move the
    /// cursor scroll or go through its pages (see [`Pages::run`]).
    pub fn run(&mut self, command: Command, area: Rect) {
        if let Shown::Pdf(pages) = &mut self.shown {
            pages.run(command, cell_pixels(), area);
        }
    }

    /// Its size, format, and file size, and how far it's zoomed in `area`,
    /// unless it's at its own size; for a PDF, the page most on screen and
    /// how far it's zoomed past fitting, or why the page can't be shown.
    pub fn status(&self, area: Rect) -> Status {
        let cell = cell_pixels();
        let image = match &self.shown {
            Shown::Image(image) => image,
            Shown::Pdf(pages) => return pages.status(self.bytes, cell, area),
        };
        let mut info = format!("{} × {}", image.width(), image.height());
        if let Some(format) = image.format() {
            info += &format!("  {format}");
        }
        info += &format!("  {}", file_size(self.bytes));
        if let Some(placed) = place(size(image), IMAGE_SCALES, self.zoom, cell, area) {
            let percent = (placed.scale * 100.0).round();
            if percent != 100.0 {
                info += &format!("  {percent}%");
            }
        }
        Status::Info(info)
    }
}

impl Pages {
    fn new(pdf: Pdf) -> Pages {
        Pages {
            sizes: (0..pdf.pages()).map(|page| pdf.page_size(page)).collect(),
            renderer: Renderer::new(Arc::new(pdf)),
            scale: None,
            at: (0.0, 0.0),
            drag: None,
            rendered: RefCell::default(),
            stale: RefCell::default(),
            asked: RefCell::default(),
            failed: RefCell::default(),
        }
    }

    /// Carries on from `old`, the same file before it changed: scrolled
    /// and zoomed as it was, and drawing what was rendered of it until
    /// its pages are rendered again.
    fn take_over(&mut self, old: Pages) {
        self.scale = old.scale;
        self.at = old.at;
        self.drag = old.drag;
        // What hadn't been rendered again since an earlier change, too.
        let mut stale = old.stale.into_inner();
        stale.extend(old.rendered.into_inner());
        stale.retain(|&page, _| page < self.sizes.len());
        self.stale = RefCell::new(stale);
    }

    /// The scale that fits each page in `area`, with cells `cell` pixels
    /// in size.
    fn fit_scale(&self, cell: (u32, u32), area: Rect) -> f64 {
        let width = area.width as f64 * cell.0 as f64;
        let height = area.height as f64 * cell.1 as f64;
        self.sizes
            .iter()
            .map(|&(w, h)| (width / w.max(1.0)).min(height / h.max(1.0)))
            .fold(f64::INFINITY, f64::min)
    }

    /// Where the pages go at the scale they're drawn at in `area`, unless
    /// there's no room.
    fn layout(&self, cell: (u32, u32), area: Rect) -> Option<Layout> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let fit = self.fit_scale(cell, area);
        let scale = self.scale.filter(|&scale| scale > fit).unwrap_or(fit);
        Some(Layout::new(&self.sizes, scale, cell))
    }

    /// Where the pages' top-left corner goes, laid out as `layout` in
    /// `area`, in screen pixels from the area's: where `at` says, as far as
    /// it can go without leaving room at an edge that could be page. Along
    /// a side that fits, they're centered.
    fn origin(&self, layout: &Layout, cell: (u32, u32), area: Rect) -> (f64, f64) {
        let (cell_w, cell_h) = (cell.0 as f64, cell.1 as f64);
        (
            keep_in(
                -self.at.0 * layout.scale,
                layout.cols as f64 * cell_w,
                area.width as f64 * cell_w,
            ),
            keep_in(
                -layout.down(self.at.1) * cell_h,
                layout.rows as f64 * cell_h,
                area.height as f64 * cell_h,
            ),
        )
    }

    /// Moves the pages, laid out as `layout`, to have their top-left
    /// corner `origin` screen pixels from the area's.
    fn move_to(&mut self, layout: &Layout, origin: (f64, f64), cell: (u32, u32)) {
        self.at = (
            -origin.0 / layout.scale,
            layout.at(-origin.1 / cell.1 as f64),
        );
    }

    /// Scrolls `by` screen pixels, across and down.
    fn scroll(&mut self, by: (f64, f64), cell: (u32, u32), area: Rect) {
        let Some(layout) = self.layout(cell, area) else {
            return;
        };
        let origin = self.origin(&layout, cell, area);
        self.move_to(&layout, (origin.0 - by.0, origin.1 - by.1), cell);
    }

    /// The pages on screen in `area`, laid out as `layout`, top first, and
    /// where they're drawn.
    fn on_screen(&self, layout: &Layout, cell: (u32, u32), area: Rect) -> Vec<(usize, Cells)> {
        let origin = self.origin(layout, cell, area);
        let x = area.x as i64 + (origin.0 / cell.0 as f64).round() as i64;
        let y = area.y as i64 + (origin.1 / cell.1 as f64).round() as i64;
        let (top, bottom) = (area.y as i64, area.y as i64 + area.height as i64);
        let first = layout
            .pages
            .partition_point(|&(_, rows, at)| y + at + rows as i64 <= top);
        layout.pages[first..]
            .iter()
            .enumerate()
            .take_while(|(_, &(_, _, at))| y + at < bottom)
            .map(|(index, &(cols, rows, at))| {
                let left = x + (layout.cols - cols) as i64 / 2;
                (first + index, (left, y + at, cols, rows))
            })
            .collect()
    }

    /// The page most on screen in `area`, the first of those that are
    /// equally.
    fn current(&self, cell: (u32, u32), area: Rect) -> usize {
        let Some(layout) = self.layout(cell, area) else {
            return (self.at.1.max(0.0) as usize).min(self.sizes.len() - 1);
        };
        let shown = self.on_screen(&layout, cell, area);
        let mut current = (0, 0);
        for (page, placed) in shown {
            let rows = visible(placed, area).map_or(0, |part| part.height);
            if rows > current.1 {
                current = (page, rows);
            }
        }
        current.0
    }

    /// The wheel scrolls, down and up, or across with Shift or sideways,
    /// and with Ctrl, zooms around the mouse. The left button drags the
    /// pages around. `mouse` is in screen cells, and the pages are in
    /// `area`.
    fn handle_mouse(&mut self, mouse: Mouse, cell: (u32, u32), area: Rect) {
        let lines = config::get().scroll_lines as f64;
        let (across, down) = (lines * cell.0 as f64, lines * cell.1 as f64);
        match mouse.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown if mouse.mods.ctrl => {
                let factor = match mouse.kind {
                    MouseKind::ScrollUp => ZOOM_STEP,
                    _ => 1.0 / ZOOM_STEP,
                };
                self.zoom_by(factor, (mouse.x, mouse.y), cell, area);
            }
            MouseKind::ScrollUp if mouse.mods.shift => self.scroll((-across, 0.0), cell, area),
            MouseKind::ScrollDown if mouse.mods.shift => self.scroll((across, 0.0), cell, area),
            MouseKind::ScrollUp => self.scroll((0.0, -down), cell, area),
            MouseKind::ScrollDown => self.scroll((0.0, down), cell, area),
            MouseKind::ScrollLeft => self.scroll((-across, 0.0), cell, area),
            MouseKind::ScrollRight => self.scroll((across, 0.0), cell, area),
            MouseKind::Press(MouseButton::Left) => {
                self.drag = self.layout(cell, area).map(|layout| Drag {
                    x: mouse.x,
                    y: mouse.y,
                    origin: self.origin(&layout, cell, area),
                });
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let Some((drag, layout)) = self.drag.zip(self.layout(cell, area)) {
                    self.move_to(&layout, dragged(drag, mouse, cell), cell);
                }
            }
            MouseKind::Release(_) => self.drag = None,
            _ => {}
        }
    }

    /// Zooms by `factor`, keeping what's under screen cell `at` there, but
    /// not past fitting `area` or `MAX_PAGE_SCALE`.
    fn zoom_by(&mut self, factor: f64, at: (u32, u32), cell: (u32, u32), area: Rect) {
        let Some(layout) = self.layout(cell, area) else {
            return;
        };
        let fit = self.fit_scale(cell, area);
        let scale = (layout.scale * factor).clamp(fit, MAX_PAGE_SCALE.max(fit));
        let (cell_w, cell_h) = (cell.0 as f64, cell.1 as f64);
        let mouse = (
            (at.0 as f64 - area.x as f64 + 0.5) * cell_w,
            (at.1 as f64 - area.y as f64 + 0.5) * cell_h,
        );
        // What's under the mouse: across in points, and down in pages.
        let origin = self.origin(&layout, cell, area);
        let across = (mouse.0 - origin.0) / layout.scale;
        let down = layout.at((mouse.1 - origin.1) / cell_h);
        let layout = Layout::new(&self.sizes, scale, cell);
        self.scale = (scale > fit).then_some(scale);
        let origin = (
            mouse.0 - across * scale,
            mouse.1 - layout.down(down) * cell_h,
        );
        self.move_to(&layout, origin, cell);
        // A drag carries on from where the pages are now.
        let origin = self.origin(&layout, cell, area);
        self.drag = self.drag.map(|_| Drag {
            x: at.0,
            y: at.1,
            origin,
        });
    }

    /// Runs an editor command in `area`: up and down scroll, Page Up and
    /// Page Down or left and right go to the top of the page before or
    /// after the one most on screen, or of that one if its top is above
    /// the panel's, and the start and end of the document or line go to
    /// the start and end. Staying zoomed, and as far across.
    fn run(&mut self, command: Command, cell: (u32, u32), area: Rect) {
        let Some(layout) = self.layout(cell, area) else {
            return;
        };
        let lines = config::get().scroll_lines as f64 * cell.1 as f64;
        let current = self.current(cell, area);
        // The rows above the panel's top.
        let above = -self.origin(&layout, cell, area).1 / cell.1 as f64;
        let at = match command {
            Command::CursorDown => return self.scroll((0.0, lines), cell, area),
            Command::CursorUp => return self.scroll((0.0, -lines), cell, area),
            Command::CursorPageDown | Command::CursorRight => current as f64 + 1.0,
            Command::CursorPageUp | Command::CursorLeft => {
                match layout.down(current as f64) < above - 0.5 {
                    true => current as f64,
                    false => current.saturating_sub(1) as f64,
                }
            }
            Command::DocumentStart | Command::LineStart => 0.0,
            // Past the last page, which keeps it at its end.
            Command::DocumentEnd | Command::LineEnd => self.sizes.len() as f64,
            _ => return,
        };
        self.at = (-self.origin(&layout, cell, area).0 / layout.scale, at);
    }

    /// The page most on screen, the file's size, and how far the pages
    /// are zoomed past fitting, or why the page can't be shown.
    fn status(&self, bytes: u64, cell: (u32, u32), area: Rect) -> Status {
        self.take_finished();
        let page = self.current(cell, area);
        if let Some(reason) = self.failed.borrow().get(&page) {
            return Status::Message {
                text: reason.clone(),
                error: true,
            };
        }
        let total = self.sizes.len();
        let mut info = format!("Page {} of {total}  PDF", page + 1);
        info += &format!("  {}", file_size(bytes));
        let fit = self.fit_scale(cell, area);
        if let Some(scale) = self.scale.filter(|&scale| scale > fit) {
            info += &format!("  {}%", (scale / fit * 100.0).round());
        }
        Status::Info(info)
    }

    /// Takes what the renderer finished.
    fn take_finished(&self) {
        for (job, rendered) in self.renderer.finished() {
            let render = job.tag;
            self.asked.borrow_mut().retain(|asked| *asked != render);
            self.stale.borrow_mut().remove(&render.page);
            let image = rendered.and_then(|rgba| {
                Image::from_rgba(job.size.0, job.size.1, &rgba).map_err(|err| err.to_string())
            });
            match image {
                Ok(image) => {
                    let mut rendered = self.rendered.borrow_mut();
                    let renders = rendered.entry(render.page).or_default();
                    let slot = match render.is_whole() {
                        true => &mut renders.whole,
                        false => &mut renders.part,
                    };
                    *slot = Some((render, Rc::new(image)));
                }
                Err(reason) => {
                    self.failed.borrow_mut().insert(render.page, reason);
                }
            }
        }
    }

    /// Draws the pages on screen in `area`, as rendered, and asks for what
    /// isn't to be: of them, and of the pages just above and below them.
    /// Forgets what's rendered of the pages further away.
    fn draw(&self, frame: &Buffer, cell: (u32, u32), area: Rect) {
        self.take_finished();
        let Some(layout) = self.layout(cell, area) else {
            return;
        };
        let fit = Layout::new(&self.sizes, self.fit_scale(cell, area), cell);
        let shown = self.on_screen(&layout, cell, area);
        let (Some(&(first, _)), Some(&(last, _))) = (shown.first(), shown.last()) else {
            return;
        };
        let near =
            |page: &usize, _: &mut Renders| first.saturating_sub(2) <= *page && *page <= last + 2;
        let mut rendered = self.rendered.borrow_mut();
        rendered.retain(near);
        let mut stale = self.stale.borrow_mut();
        stale.retain(near);
        let failed = self.failed.borrow();
        let mut wanted = Vec::new();
        for &(page, placed) in &shown {
            let size = (placed.2, placed.3);
            let Some(visible) = visible(placed, area) else {
                continue;
            };
            let want = Render {
                page,
                size,
                part: visible,
                cell,
            };
            let renders = rendered.get(&page);
            let (whole, part) = renders.map_or((None, None), Renders::renders);
            // Until any of it's rendered again, what was before it changed.
            let drawn = renders.or(stale.get(&page));
            if let Some(drawn) = drawn {
                let (whole, part) = drawn.renders();
                for layer in layers(want, whole, part) {
                    if let Some((render, image)) = drawn.get(layer) {
                        let (x, y, cols, rows) = drawn_now(*render, placed);
                        frame.draw_image(image, x, y, cols, rows, cols * cell.0, rows * cell.1);
                    }
                }
            }
            if !failed.contains_key(&page) {
                let fit_size = (fit.pages[page].0, fit.pages[page].1);
                wanted.extend(wants(want, fit_size, whole, part, area));
            }
        }
        // The pages just off screen, whole, so they're there to scroll to.
        for page in [first.checked_sub(1), Some(last + 1)].into_iter().flatten() {
            let Some(&(cols, rows, _)) = layout.pages.get(page) else {
                continue;
            };
            if failed.contains_key(&page) {
                continue;
            }
            // Any whole page does for one too large to render whole.
            let whole = rendered
                .get(&page)
                .and_then(|renders| renders.whole.as_ref())
                .map(|(render, _)| render.size);
            let size = match fits_whole((cols, rows), area) {
                true => (cols, rows),
                false if whole.is_none() => (fit.pages[page].0, fit.pages[page].1),
                false => continue,
            };
            if whole != Some(size) {
                wanted.push(Render::whole(page, size, cell));
            }
        }
        if *self.asked.borrow() != wanted {
            self.renderer
                .ask(wanted.iter().map(|&render| self.job(render)).collect());
            *self.asked.borrow_mut() = wanted;
        }
    }

    /// The job that renders `render`, a screen pixel for each pixel.
    fn job(&self, render: Render) -> Job<Render> {
        let (cell_w, cell_h) = (render.cell.0 as f64, render.cell.1 as f64);
        let (width, height) = self.sizes[render.page];
        let (cols, rows) = render.size;
        let part = render.part;
        Job {
            tag: render,
            page: render.page,
            // Across the cells it's drawn on, which round its shape a
            // little.
            scale: (cols as f64 * cell_w / width, rows as f64 * cell_h / height),
            from: (part.x as f64 * cell_w, part.y as f64 * cell_h),
            size: (part.width * render.cell.0, part.height * render.cell.1),
        }
    }
}

impl Layout {
    /// Lays out pages `sizes` points in size at `scale` screen pixels per
    /// point, with cells `cell` pixels in size.
    fn new(sizes: &[(f64, f64)], scale: f64, cell: (u32, u32)) -> Layout {
        let mut pages = Vec::with_capacity(sizes.len());
        let mut top = 0;
        for &(width, height) in sizes {
            let cols = ((width * scale / cell.0 as f64).round() as u32).max(1);
            let rows = ((height * scale / cell.1 as f64).round() as u32).max(1);
            pages.push((cols, rows, top));
            top += (rows + GAP) as i64;
        }
        Layout {
            scale,
            cols: pages.iter().map(|&(cols, _, _)| cols).max().unwrap_or(1),
            rows: top - GAP as i64,
            pages,
        }
    }

    /// The rows from the first page's top to `at` pages down: to the top
    /// of the page it's in, and as far through that page and the gap
    /// under it as it is through the page.
    fn down(&self, at: f64) -> f64 {
        let page = (at.max(0.0) as usize).min(self.pages.len() - 1);
        let (_, rows, top) = self.pages[page];
        top as f64 + (at - page as f64) * (rows + GAP) as f64
    }

    /// How many pages down from the first's top it is to `rows` under it:
    /// the other way from [`Layout::down`].
    fn at(&self, rows: f64) -> f64 {
        let page = self
            .pages
            .partition_point(|&(_, _, top)| top as f64 <= rows)
            .saturating_sub(1);
        let (_, height, top) = self.pages[page];
        page as f64 + (rows - top as f64) / (height + GAP) as f64
    }
}

impl Render {
    /// Of the whole of page `page`, `size` cells large.
    fn whole(page: usize, size: (u32, u32), cell: (u32, u32)) -> Render {
        let part = Rect {
            x: 0,
            y: 0,
            width: size.0,
            height: size.1,
        };
        Render {
            page,
            size,
            part,
            cell,
        }
    }

    fn is_whole(&self) -> bool {
        *self == Render::whole(self.page, self.size, self.cell)
    }

    /// Whether it has all of `want` of the page, at its size.
    fn covers(&self, want: Render) -> bool {
        let (part, other) = (self.part, want.part);
        (self.page, self.size, self.cell) == (want.page, want.size, want.cell)
            && part.x <= other.x
            && part.y <= other.y
            && other.x + other.width <= part.x + part.width
            && other.y + other.height <= part.y + part.height
    }
}

/// What [`layers`] draws: what's rendered of a page in [`Renders`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum Layer {
    Whole,
    Part,
}

/// What's drawn of a page, bottom first, while `want` of it is wanted, of
/// `whole` and `part` as rendered last: either if it has all of `want`, or
/// else both, stretched to where they are now.
fn layers(want: Render, whole: Option<Render>, part: Option<Render>) -> Vec<Layer> {
    let rendered = [(Layer::Whole, whole), (Layer::Part, part)];
    let rendered = rendered
        .into_iter()
        .filter_map(|(layer, render)| Some((layer, render?)));
    if let Some((layer, _)) = rendered.clone().find(|(_, render)| render.covers(want)) {
        return vec![layer];
    }
    rendered.map(|(layer, _)| layer).collect()
}

/// What to render of a page in `area` so that `want` of it is drawn as
/// large as it's drawn, if what's rendered of it, `whole` and `part`,
/// won't do: the whole page if it's not much larger than `area`, and
/// otherwise around `want` (as far as half `area` past it each way), and
/// the whole page at `fit_size`, if none of it is rendered whole.
fn wants(
    want: Render,
    fit_size: (u32, u32),
    whole: Option<Render>,
    part: Option<Render>,
    area: Rect,
) -> Vec<Render> {
    let covered = |want| [whole, part].into_iter().flatten().any(|r| r.covers(want));
    let (cols, rows) = want.size;
    if fits_whole(want.size, area) {
        return match covered(want) {
            true => vec![],
            false => vec![Render::whole(want.page, want.size, want.cell)],
        };
    }
    let mut wants = Vec::new();
    if !covered(want) {
        let around = |at: u32, size: u32, room: u32, most: u32| {
            let start = at.saturating_sub(room / 2);
            (start, (at + size + room / 2).min(most) - start)
        };
        let (x, width) = around(want.part.x, want.part.width, area.width, cols);
        let (y, height) = around(want.part.y, want.part.height, area.height, rows);
        let part = Rect {
            x,
            y,
            width,
            height,
        };
        wants.push(Render { part, ..want });
    }
    if whole.is_none() {
        wants.push(Render::whole(want.page, fit_size, want.cell));
    }
    wants
}

/// Whether a page `size` cells large is rendered whole in `area`.
fn fits_whole(size: (u32, u32), area: Rect) -> bool {
    size.0 as u64 * size.1 as u64 <= WHOLE_PAGE * area.width as u64 * area.height as u64
}

/// The part of something `placed` that's in `area`, in cells from its
/// top-left corner, if any is.
fn visible(placed: Cells, area: Rect) -> Option<Rect> {
    let (x, y, cols, rows) = placed;
    let left = x.max(area.x as i64);
    let top = y.max(area.y as i64);
    let right = (x + cols as i64).min(area.x as i64 + area.width as i64);
    let bottom = (y + rows as i64).min(area.y as i64 + area.height as i64);
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: (left - x) as u32,
        y: (top - y) as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// Where what `rendered` rendered of a page goes while the page is drawn
/// at `placed`: its part of the page, stretched to the page's size now.
/// As (x, y, cols, rows).
fn drawn_now(rendered: Render, placed: Cells) -> (i32, i32, u32, u32) {
    let (x, y, cols, rows) = placed;
    let (width, height) = rendered.size;
    let part = rendered.part;
    let map = |at: u32, size: u32, to: i64, to_size: u32| {
        to + (at as f64 / size as f64 * to_size as f64).round() as i64
    };
    let left = map(part.x, width, x, cols);
    let right = map(part.x + part.width, width, x, cols);
    let top = map(part.y, height, y, rows);
    let bottom = map(part.y + part.height, height, y, rows);
    let width = (right - left).max(1) as u32;
    let height = (bottom - top).max(1) as u32;
    (left as i32, top as i32, width, height)
}

/// Where a drag puts what it moves, with the mouse at `mouse`: its
/// top-left corner, in screen pixels from the area's.
fn dragged(drag: Drag, mouse: Mouse, cell: (u32, u32)) -> (f64, f64) {
    let dx = (mouse.x as f64 - drag.x as f64) * cell.0 as f64;
    let dy = (mouse.y as f64 - drag.y as f64) * cell.1 as f64;
    (drag.origin.0 + dx, drag.origin.1 + dy)
}

/// Reads the file at `path`, and how it was on disk. Taken before it's
/// read, so a change while it's read is seen as one after.
fn read(path: &Path) -> io::Result<(Vec<u8>, Option<Stamp>)> {
    let mut file = fs::File::open(path)?;
    let stamp = file.metadata().ok().and_then(|meta| Stamp::of(&meta));
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok((data, stamp))
}

/// Decodes the image or PDF in `data`, read from `path`.
fn decode(path: &Path, data: Vec<u8>) -> Result<Shown, String> {
    match is_pdf(path) {
        true => Ok(Shown::Pdf(Box::new(Pages::new(Pdf::open(data)?)))),
        false => Image::decode(&data)
            .map(Shown::Image)
            .map_err(|err| err.to_string()),
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Whether `data` ends as a PDF does, with `%%EOF` near its end: one still
/// being written doesn't yet. A malformed one may read anyway, with pages
/// missing, so this is asked only of a file that read before.
fn looks_whole_pdf(data: &[u8]) -> bool {
    let tail = &data[data.len().saturating_sub(1024)..];
    tail.windows(5).any(|window| window == b"%%EOF")
}

/// An image's size in its own pixels.
fn size(image: &Image) -> (u32, u32) {
    (image.width(), image.height())
}

/// The terminal's cells' size in pixels, or a guess.
fn cell_pixels() -> (u32, u32) {
    let (width, height) = tty::cell_pixels().unwrap_or(DEFAULT_CELL);
    (width.max(1), height.max(1))
}

/// `zoom` of an `image` pixels in size in `area`, with cells `cell` pixels
/// in size, zoomed by `factor`, keeping what's under screen cell `at`
/// there, but not past fitting `area` or `scales.max`. `None`, zoomed out
/// to fitting.
fn zoom_at(
    image: (u32, u32),
    scales: Scales,
    zoom: Option<Zoom>,
    factor: f64,
    at: (u32, u32),
    cell: (u32, u32),
    area: Rect,
) -> Option<Zoom> {
    let placed = place(image, scales, zoom, cell, area)?;
    let fit = fit_scale(image, scales, cell, area);
    let scale = (placed.scale * factor).clamp(fit, scales.max.max(fit));
    if scale <= fit {
        return None;
    }
    // As drawn, on whole cells, so the point under the mouse is the one
    // seen there.
    let (cell_w, cell_h) = (cell.0 as f64, cell.1 as f64);
    let drawn = (
        (placed.x - area.x as i64) as f64 * cell_w,
        (placed.y - area.y as i64) as f64 * cell_h,
    );
    let drawn_scale = (
        placed.cols as f64 * cell_w / image.0 as f64,
        placed.rows as f64 * cell_h / image.1 as f64,
    );
    let at = (
        (at.0 as f64 - area.x as f64 + 0.5) * cell_w,
        (at.1 as f64 - area.y as f64 + 0.5) * cell_h,
    );
    Some(Zoom {
        scale,
        origin: (
            at.0 - (at.0 - drawn.0) * scale / drawn_scale.0,
            at.1 - (at.1 - drawn.1) * scale / drawn_scale.1,
        ),
    })
}

/// The scale that fits an `image` pixels in size in `area`, with cells
/// `cell` pixels in size, enlarging it no more than `scales.fit`.
fn fit_scale(image: (u32, u32), scales: Scales, cell: (u32, u32), area: Rect) -> f64 {
    let width = area.width as f64 * cell.0 as f64 / image.0.max(1) as f64;
    let height = area.height as f64 * cell.1 as f64 / image.1.max(1) as f64;
    width.min(height).min(scales.fit)
}

/// Where an `image` pixels in size goes in `area`, with cells `cell`
/// pixels in size: fit and centered, or with `zoom`, as large as it says,
/// and where it says as far as it can without leaving room at an edge
/// that could be image. Along a side that fits, it's centered. `None` if
/// there's no room or no image.
fn place(
    image: (u32, u32),
    scales: Scales,
    zoom: Option<Zoom>,
    cell: (u32, u32),
    area: Rect,
) -> Option<Placement> {
    if area.width == 0 || area.height == 0 || image.0 == 0 || image.1 == 0 {
        return None;
    }
    let fit = fit_scale(image, scales, cell, area);
    // The panel may have grown past the zoom since.
    let zoom = zoom.filter(|zoom| zoom.scale > fit);
    let (scale, origin) = zoom.map_or((fit, (0.0, 0.0)), |zoom| (zoom.scale, zoom.origin));
    let (cell_w, cell_h) = (cell.0 as f64, cell.1 as f64);
    let cols = ((image.0 as f64 * scale / cell_w).round() as u32).max(1);
    let rows = ((image.1 as f64 * scale / cell_h).round() as u32).max(1);
    let (cols, rows) = match zoom {
        Some(_) => (cols, rows),
        None => (cols.min(area.width), rows.min(area.height)),
    };
    let origin = (
        keep_in(origin.0, cols as f64 * cell_w, area.width as f64 * cell_w),
        keep_in(origin.1, rows as f64 * cell_h, area.height as f64 * cell_h),
    );
    Some(Placement {
        x: area.x as i64 + (origin.0 / cell_w).round() as i64,
        y: area.y as i64 + (origin.1 / cell_h).round() as i64,
        cols,
        rows,
        scale,
        origin,
    })
}

/// Where something `size` long starts along a side `room` long, from
/// `start`: centered if it fits, and otherwise leaving no room at either
/// end.
fn keep_in(start: f64, size: f64, room: f64) -> f64 {
    if size <= room {
        (room - size) / 2.0
    } else {
        start.clamp(room - size, 0.0)
    }
}

/// `bytes` as people write file sizes: "812 B", "4.2 KB", "1.3 MB".
fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1000.0;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// A 4x2 opaque red PNG, 75 bytes.
#[cfg(test)]
pub const TEST_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 4, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 127, 168, 125, 99, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 25,
    51, 160, 11, 0, 0, 15, 33, 15, 241, 4, 55, 198, 159, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u32, height: u32) -> Rect {
        Rect {
            x: 30,
            y: 1,
            width,
            height,
        }
    }

    /// The cells the image takes, and its top-left cell in the area.
    fn placed(image: (u32, u32), zoom: Option<Zoom>, area: Rect) -> Option<(u32, u32, i64, i64)> {
        let placed = place(image, IMAGE_SCALES, zoom, (10, 20), area)?;
        let (x, y) = (placed.x - area.x as i64, placed.y - area.y as i64);
        Some((placed.cols, placed.rows, x, y))
    }

    #[test]
    fn images_fit_keep_their_shape_and_are_never_enlarged() {
        // 800x400 into 50x20 cells of 10x20: 500 pixels wide at most.
        assert_eq!(placed((800, 400), None, area(50, 20)), Some((50, 13, 0, 4)));
        // Tall: the height limits it.
        assert_eq!(
            placed((400, 800), None, area(50, 20)),
            Some((20, 20, 15, 0))
        );
        // Small images keep their size.
        assert_eq!(placed((100, 40), None, area(50, 20)), Some((10, 2, 20, 9)));
        // At least a cell, and nothing in no room.
        assert_eq!(placed((1, 1), None, area(50, 20)), Some((1, 1, 25, 10)));
        assert_eq!(placed((100, 100), None, area(0, 20)), None);
    }

    #[test]
    fn zooming_keeps_the_point_under_the_mouse_in_place() {
        let (image, cell, area) = ((800, 400), (10, 20), area(50, 20));
        // Fit, it's at 62.5%: 50x13 cells at (0, 4).
        let at = (area.x + 10, area.y + 7);
        let zoom = zoom_at(image, IMAGE_SCALES, None, 1.6, at, cell, area).unwrap();
        assert!((zoom.scale - 1.0).abs() < 1e-9, "{zoom:?}");
        // The mouse was 105 pixels into the image, its 168th pixel across,
        // which stays 105 pixels in at full size.
        assert!((zoom.origin.0 - (105.0 - 168.0)).abs() < 1e-9, "{zoom:?}");
        // 80x20 cells now, 6.3 of them left of the area. It fits
        // vertically, so it's centered that way.
        assert_eq!(placed(image, Some(zoom), area), Some((80, 20, -6, 0)));

        // Zooming out past fitting fits it again.
        assert_eq!(
            zoom_at(image, IMAGE_SCALES, Some(zoom), 0.4, at, cell, area),
            None
        );
        // And there's a limit going in.
        let mut zoom = Some(zoom);
        for _ in 0..100 {
            zoom = zoom_at(image, IMAGE_SCALES, zoom, 2.0, at, cell, area);
        }
        assert_eq!(zoom.unwrap().scale, IMAGE_SCALES.max);
    }

    #[test]
    fn zoomed_images_stay_in_bounds() {
        let (image, area) = ((800, 400), area(50, 20));
        let zoom = |origin| Some(Zoom { scale: 1.0, origin });
        // 80x20 cells in 50x20: it moves left and right only, as far as
        // its edges.
        assert_eq!(placed(image, zoom((0.0, 0.0)), area), Some((80, 20, 0, 0)));
        assert_eq!(
            placed(image, zoom((-1000.0, 90.0)), area),
            Some((80, 20, -30, 0))
        );
        assert_eq!(
            placed(image, zoom((70.0, -90.0)), area),
            Some((80, 20, 0, 0))
        );
        // Zoomed less than fitting, as the panel grew, it fits.
        let small = Some(Zoom {
            scale: 0.1,
            origin: (-100.0, 0.0),
        });
        assert_eq!(placed(image, small, area), placed(image, None, area));
    }

    const CELL: (u32, u32) = (10, 20);

    /// A PDF of `count` pages 100 x 100 points each.
    fn pages(count: usize) -> Pages {
        let content = vec![((100, 100), crate::pdf::RED_100); count];
        Pages::new(Pdf::open(crate::pdf::test_pdf(&content)).unwrap())
    }

    /// The pages on screen in `area`, and where.
    fn shown(pages: &Pages, area: Rect) -> Vec<(usize, Cells)> {
        let layout = pages.layout(CELL, area).unwrap();
        pages.on_screen(&layout, CELL, area)
    }

    fn info(status: Status) -> String {
        match status {
            Status::Info(info) => info,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pages_go_a_row_apart_and_centered() {
        // At a point a pixel: 10 x 5 cells, and 20 x 3, rounding 2.5 up.
        let layout = Layout::new(&[(100.0, 100.0), (200.0, 50.0)], 1.0, CELL);
        assert_eq!(layout.pages, [(10, 5, 0), (20, 3, 6)]);
        assert_eq!((layout.cols, layout.rows), (20, 9));
        // Half a page down is half through it and the gap under it.
        assert_eq!(layout.down(0.5), 3.0);
        assert_eq!(layout.at(3.0), 0.5);
        assert_eq!(layout.down(1.0), 6.0);
        assert_eq!(layout.at(6.0), 1.0);
        // Before the first page and past the last carry on from them.
        assert_eq!(layout.at(-3.0), -0.5);
        assert_eq!(layout.down(3.0), 6.0 + 2.0 * 4.0);
    }

    #[test]
    fn pages_fill_the_panel_and_scroll_within_it() {
        let mut pages = pages(3);
        let area = area(50, 20);
        // 100 points square into 50 x 20 cells of 10 x 20: the height
        // limits each page, to 4 pixels a point, centered across.
        assert_eq!(pages.fit_scale(CELL, area), 4.0);
        assert_eq!(shown(&pages, area), [(0, (35, 1, 40, 20))]);

        // Ten rows down, the next page shows under the gap.
        pages.scroll((0.0, 200.0), CELL, area);
        assert_eq!(
            shown(&pages, area),
            [(0, (35, -9, 40, 20)), (1, (35, 12, 40, 20))]
        );
        // The page most on screen is the one shown in the status bar.
        assert_eq!(pages.current(CELL, area), 0);
        pages.scroll((0.0, 20.0), CELL, area);
        assert_eq!(pages.current(CELL, area), 1);
        let status = info(pages.status(1234, CELL, area));
        assert_eq!(status, "Page 2 of 3  PDF  1.2 KB");

        // As far as the last page's bottom, and back as far as the first's
        // top, and not across while it fits.
        pages.scroll((500.0, 10_000.0), CELL, area);
        assert_eq!(shown(&pages, area), [(2, (35, 1, 40, 20))]);
        pages.scroll((0.0, -20.0), CELL, area);
        // The gap above the last page.
        assert_eq!(shown(&pages, area), [(2, (35, 2, 40, 20))]);
        pages.scroll((0.0, -10_000.0), CELL, area);
        assert_eq!(shown(&pages, area), [(0, (35, 1, 40, 20))]);

        // The panel resizing keeps what's at its top there.
        pages.scroll((0.0, 200.0), CELL, area);
        let wide = Rect {
            width: 100,
            height: 40,
            ..area
        };
        assert_eq!(shown(&pages, wide)[0], (0, (40, -19, 80, 40)));
    }

    #[test]
    fn zooming_pages_keeps_the_point_under_the_mouse_in_place() {
        let mut pages = pages(3);
        let area = area(50, 20);
        // Under the mouse is 51.25 points across the first page, and half
        // way down it.
        let at = (area.x + 25, area.y + 10);
        pages.zoom_by(2.0, at, CELL, area);
        assert_eq!(pages.scale, Some(8.0));
        // 80 x 40 cells now, 15.5 left of the panel and 10 above it.
        assert_eq!(shown(&pages, area), [(0, (14, -9, 80, 40))]);
        let status = info(pages.status(10, CELL, area));
        assert_eq!(status, "Page 1 of 3  PDF  10 B  200%");

        // Zooming out past fitting fits, where it was.
        pages.zoom_by(0.1, at, CELL, area);
        assert_eq!(pages.scale, None);
        assert_eq!(shown(&pages, area), [(0, (35, 1, 40, 20))]);
        // And there's a limit going in.
        for _ in 0..100 {
            pages.zoom_by(2.0, at, CELL, area);
        }
        assert_eq!(pages.scale, Some(MAX_PAGE_SCALE));

        // With Ctrl, the wheel zooms; without, it scrolls.
        let mut pages = self::pages(3);
        let wheel = |kind, ctrl| Mouse {
            kind,
            x: at.0,
            y: at.1,
            mods: crate::input::Mods {
                ctrl,
                ..crate::input::Mods::NONE
            },
        };
        pages.handle_mouse(wheel(MouseKind::ScrollUp, true), CELL, area);
        assert!((pages.scale.unwrap() - 4.0 * ZOOM_STEP).abs() < 1e-9);
        pages.handle_mouse(wheel(MouseKind::ScrollDown, true), CELL, area);
        assert_eq!(pages.scale, None);
        pages.handle_mouse(wheel(MouseKind::ScrollDown, false), CELL, area);
        assert!(shown(&pages, area)[0].1 .1 < 1);
    }

    #[test]
    fn page_keys_go_to_the_tops_of_pages() {
        let mut pages = pages(3);
        let area = area(50, 20);
        let top = |pages: &Pages| shown(pages, area)[0];
        pages.run(Command::CursorPageUp, CELL, area);
        assert_eq!(top(&pages), (0, (35, 1, 40, 20)));
        pages.run(Command::CursorPageDown, CELL, area);
        assert_eq!(top(&pages), (1, (35, 1, 40, 20)));
        pages.run(Command::CursorRight, CELL, area);
        pages.run(Command::CursorPageDown, CELL, area);
        assert_eq!(top(&pages), (2, (35, 1, 40, 20)));
        pages.run(Command::CursorPageUp, CELL, area);
        assert_eq!(top(&pages), (1, (35, 1, 40, 20)));

        // Partway down a page, back goes to its top first.
        pages.scroll((0.0, 100.0), CELL, area);
        pages.run(Command::CursorLeft, CELL, area);
        assert_eq!(top(&pages), (1, (35, 1, 40, 20)));
        pages.run(Command::DocumentStart, CELL, area);
        assert_eq!(top(&pages), (0, (35, 1, 40, 20)));
        pages.run(Command::LineEnd, CELL, area);
        assert_eq!(top(&pages), (2, (35, 1, 40, 20)));
        assert_eq!(pages.current(CELL, area), 2);

        // Zoomed, they stay zoomed, and as far across.
        pages.run(Command::DocumentStart, CELL, area);
        pages.zoom_by(2.0, (area.x, area.y), CELL, area);
        pages.scroll((150.0, 0.0), CELL, area);
        let across = top(&pages).1 .0;
        pages.run(Command::CursorPageDown, CELL, area);
        assert_eq!(top(&pages), (1, (across, 1, 80, 40)));
        // Other commands do nothing.
        pages.run(Command::DeleteBackward, CELL, area);
        assert_eq!(top(&pages), (1, (across, 1, 80, 40)));
    }

    fn render(page: usize, size: (u32, u32), part: (u32, u32, u32, u32)) -> Render {
        let (x, y, width, height) = part;
        Render {
            page,
            size,
            part: Rect {
                x,
                y,
                width,
                height,
            },
            cell: CELL,
        }
    }

    #[test]
    fn what_was_rendered_stands_in_where_it_is_now() {
        let fit = render(0, (10, 10), (0, 0, 10, 10));
        assert_eq!(drawn_now(fit, (0, 0, 10, 10)), (0, 0, 10, 10));
        // Zoomed in twice around its middle: the whole page, twice as
        // large, partly off screen.
        assert_eq!(drawn_now(fit, (-5, -5, 20, 20)), (-5, -5, 20, 20));
        // And back: the part that was on screen shrinks into the middle.
        let zoomed = render(0, (20, 20), (5, 5, 10, 10));
        assert_eq!(drawn_now(zoomed, (0, 0, 10, 10)), (3, 3, 5, 5));
        // Scrolled, it moves along.
        assert_eq!(drawn_now(zoomed, (0, 7, 20, 20)), (5, 12, 10, 10));
    }

    #[test]
    fn the_whole_page_stands_in_under_what_was_rendered() {
        let fit = render(0, (10, 10), (0, 0, 10, 10));
        // Fit, the whole page is what's rendered.
        assert_eq!(layers(fit, Some(fit), None), [Layer::Whole]);
        // Zoomed four times, with the middle rendered at twice: the whole
        // page under that.
        let want = render(0, (40, 40), (15, 15, 10, 10));
        let less = render(0, (20, 20), (5, 5, 10, 10));
        assert_eq!(
            layers(want, Some(fit), Some(less)),
            [Layer::Whole, Layer::Part]
        );
        // Until what's wanted is rendered, or more around it.
        let around = render(0, (40, 40), (10, 10, 20, 20));
        assert_eq!(layers(want, Some(fit), Some(around)), [Layer::Part]);
        assert_eq!(layers(want, None, None), []);
    }

    #[test]
    fn pages_are_rendered_whole_unless_much_larger_than_the_panel() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let fit = render(0, (10, 10), (0, 0, 10, 10));
        assert_eq!(wants(fit, (10, 10), None, None, area), [fit]);
        assert_eq!(wants(fit, (10, 10), Some(fit), None, area), []);
        // A little larger, the whole of it is rendered, at its size.
        let some = render(0, (14, 14), (2, 2, 10, 10));
        let whole = render(0, (14, 14), (0, 0, 14, 14));
        assert_eq!(wants(some, (10, 10), Some(fit), None, area), [whole]);
        // Much larger, what's on screen and around it is, and the whole
        // page as it fits if none of it is rendered whole.
        let want = render(0, (40, 40), (15, 15, 10, 10));
        let around = render(0, (40, 40), (10, 10, 20, 20));
        assert_eq!(wants(want, (10, 10), None, None, area), [around, fit]);
        assert_eq!(wants(want, (10, 10), Some(whole), None, area), [around]);
        assert_eq!(wants(want, (10, 10), Some(fit), Some(around), area), []);
        // Around it only as far as the page goes.
        let corner = render(0, (40, 40), (0, 30, 10, 10));
        let around = render(0, (40, 40), (0, 25, 15, 15));
        assert_eq!(wants(corner, (10, 10), Some(fit), None, area), [around]);
    }

    #[test]
    fn a_pdf_shows_its_page_and_size() {
        let dir = std::env::temp_dir().join(format!("cue-image-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.pdf");
        let content = vec![((100, 100), crate::pdf::RED_100); 3];
        fs::write(&path, crate::pdf::test_pdf(&content)).unwrap();
        let mut view = ImageView::open(&path).unwrap();
        let status = info(view.status(area(50, 20)));
        assert!(status.starts_with("Page 1 of 3  PDF  "), "{status}");
        view.run(Command::DocumentEnd, area(50, 20));
        let status = info(view.status(area(50, 20)));
        assert!(status.starts_with("Page 3 of 3  PDF  "), "{status}");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_changed_file_is_read_again_where_it_was() {
        let dir = std::env::temp_dir().join(format!("cue-image-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reload.pdf");
        let pdf = |count| crate::pdf::test_pdf(&vec![((100, 100), crate::pdf::RED_100); count]);
        fs::write(&path, pdf(3)).unwrap();
        let mut view = ImageView::open(&path).unwrap();
        let area = area(50, 20);
        let status = |view: &ImageView| info(view.status(area));
        // Unchanged, it's not read again.
        assert!(!view.reload());
        view.run(Command::CursorPageDown, area);
        let wheel = Mouse {
            kind: MouseKind::ScrollUp,
            x: area.x + 25,
            y: area.y + 10,
            mods: crate::input::Mods {
                ctrl: true,
                ..crate::input::Mods::NONE
            },
        };
        view.handle_mouse(wheel, area);
        assert!(
            status(&view).starts_with("Page 2 of 3  PDF"),
            "{}",
            status(&view)
        );

        // Half written, it stays as it was.
        let longer = pdf(4);
        fs::write(&path, &longer[..longer.len() / 2]).unwrap();
        assert!(!view.reload());
        assert!(
            status(&view).starts_with("Page 2 of 3  PDF"),
            "{}",
            status(&view)
        );
        // Written, it's read again, as far down and zoomed.
        fs::write(&path, &longer).unwrap();
        assert!(view.reload());
        let now = status(&view);
        assert!(
            now.starts_with("Page 2 of 4  PDF") && now.ends_with("110%"),
            "{now}"
        );
        // One that doesn't read is left as it was too.
        fs::write(&path, "%PDF-1.4\n%%EOF\n").unwrap();
        assert!(!view.reload());
        assert_eq!(status(&view), now);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn what_was_rendered_is_drawn_until_rendered_again() {
        let _serial = crate::test_serial();
        let mut old = pages(3);
        let image = Rc::new(Image::from_rgba(1, 1, &[255; 4]).unwrap());
        for page in [0, 2] {
            let renders = Renders {
                whole: Some((Render::whole(page, (10, 10), CELL), image.clone())),
                part: None,
            };
            old.rendered.get_mut().insert(page, renders);
        }
        old.scale = Some(8.0);
        old.at = (1.0, 0.5);
        let mut new = pages(2);
        new.take_over(old);
        assert_eq!((new.scale, new.at), (Some(8.0), (1.0, 0.5)));
        // Not the pages it no longer has.
        let stale: Vec<_> = new.stale.borrow().keys().copied().collect();
        assert_eq!(stale, [0]);

        // Rendered again, the page is drawn as it is now.
        let render = Render::whole(0, (10, 10), CELL);
        new.renderer.ask(vec![new.job(render)]);
        let start = std::time::Instant::now();
        while !new.stale.borrow().is_empty() {
            assert!(start.elapsed().as_secs() < 10, "nothing rendered");
            std::thread::sleep(std::time::Duration::from_millis(5));
            new.take_finished();
        }
        assert!(new.rendered.borrow().contains_key(&0));
    }

    #[test]
    fn pdfs_are_images() {
        assert!(is_image(Path::new("paper.PDF")));
        let dir = std::env::temp_dir().join(format!("cue-image-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.pdf");
        fs::write(&path, "%PDF-1.4 not really").unwrap();
        assert!(ImageView::open(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_sizes_read_naturally() {
        assert_eq!(file_size(812), "812 B");
        assert_eq!(file_size(4_200), "4.2 KB");
        assert_eq!(file_size(1_300_000), "1.3 MB");
    }

    #[test]
    fn images_are_known_by_extension() {
        assert!(is_image(Path::new("a/b.PNG")));
        assert!(is_image(Path::new("photo.jpeg")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("png")));
    }
}
