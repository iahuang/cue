//! An image file on screen in a panel.
//!
//! The image starts fit to the panel, never enlarged past its own size,
//! and centered. The wheel zooms in and out around the mouse, down to
//! fitting again, and dragging moves a zoomed image around. OpenTUI's
//! renderer sends it with the Kitty graphics protocol where the terminal
//! has it, Sixel where it has that, or else as half-block characters.

use std::fs;
use std::path::{Path, PathBuf};

use opentui::{Buffer, Image};

use crate::input::{Mouse, MouseButton, MouseKind};
use crate::layout::Rect;
use crate::status::Status;
use crate::tty;

/// File extensions opened as images rather than as text.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
/// A cell's size in pixels when the terminal doesn't say.
const DEFAULT_CELL: (u32, u32) = (8, 16);
/// How much a step of the wheel zooms.
const ZOOM_STEP: f64 = 1.1;
/// How far an image can be enlarged: each of its pixels this many screen
/// pixels across.
const MAX_SCALE: f64 = 16.0;

/// Whether `path` is opened as an image, going by its extension.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

pub struct ImageView {
    path: PathBuf,
    image: Image,
    /// The file's size in bytes.
    bytes: u64,
    /// `None` while it's fit to the panel.
    zoom: Option<Zoom>,
    /// Where a drag that moves the image started.
    drag: Option<Drag>,
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

/// A drag moving the image: the screen cell it started at, and the
/// image's origin then.
#[derive(Debug, Clone, Copy)]
struct Drag {
    x: u32,
    y: u32,
    origin: (f64, f64),
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
    /// Reads and decodes the image at `path`, or says why it can't.
    pub fn open(path: &Path) -> Result<ImageView, String> {
        let data = fs::read(path).map_err(|err| err.to_string())?;
        let image = Image::decode(&data).map_err(|err| err.to_string())?;
        Ok(ImageView {
            path: path.to_path_buf(),
            image,
            bytes: data.len() as u64,
            zoom: None,
            drag: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Follows the file to `path`, where it was moved.
    pub fn rename(&mut self, path: PathBuf) {
        self.path = path;
    }

    /// Draws the image in `area`, clipped to it.
    pub fn draw(&self, frame: &Buffer, area: Rect) {
        let cell = cell_pixels();
        let Some(placed) = self.place(cell, area) else {
            return;
        };
        let (x, y) = (placed.x as i32, placed.y as i32);
        let (cols, rows) = (placed.cols, placed.rows);
        frame.draw_image(&self.image, x, y, cols, rows, cols * cell.0, rows * cell.1);
    }

    /// Where the image goes in `area`, with cells `cell` pixels in size.
    fn place(&self, cell: (u32, u32), area: Rect) -> Option<Placement> {
        let image = (self.image.width(), self.image.height());
        place(image, self.zoom, cell, area)
    }

    /// The wheel zooms, around the mouse, and the left button drags the
    /// image around while it's zoomed. `mouse` is in screen cells, and the
    /// image is in `area`.
    pub fn handle_mouse(&mut self, mouse: Mouse, area: Rect) {
        let cell = cell_pixels();
        match mouse.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let factor = match mouse.kind {
                    MouseKind::ScrollUp => ZOOM_STEP,
                    _ => 1.0 / ZOOM_STEP,
                };
                self.zoom_by(factor, (mouse.x, mouse.y), cell, area);
            }
            MouseKind::Press(MouseButton::Left) => {
                self.drag = self
                    .zoom
                    .and(self.place(cell, area))
                    .map(|placed| Drag {
                        x: mouse.x,
                        y: mouse.y,
                        origin: placed.origin,
                    });
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let (Some(drag), Some(zoom)) = (self.drag, &mut self.zoom) {
                    let dx = (mouse.x as f64 - drag.x as f64) * cell.0 as f64;
                    let dy = (mouse.y as f64 - drag.y as f64) * cell.1 as f64;
                    zoom.origin = (drag.origin.0 + dx, drag.origin.1 + dy);
                }
            }
            MouseKind::Release(_) => self.drag = None,
            _ => {}
        }
    }

    /// Zooms by `factor`, around screen cell `at` (see [`zoom_at`]).
    fn zoom_by(&mut self, factor: f64, at: (u32, u32), cell: (u32, u32), area: Rect) {
        let image = (self.image.width(), self.image.height());
        self.zoom = zoom_at(image, self.zoom, factor, at, cell, area);
        // A drag carries on from where the image is now.
        let placed = self.zoom.and(self.place(cell, area));
        self.drag = self.drag.zip(placed).map(|(drag, placed)| Drag {
            origin: placed.origin,
            ..drag
        });
    }

    /// Its size, format, and file size, and how far it's zoomed in `area`,
    /// unless it's at its own size.
    pub fn status(&self, area: Rect) -> Status {
        let (width, height) = (self.image.width(), self.image.height());
        let mut info = format!("{width} × {height}");
        if let Some(format) = self.image.format() {
            info += &format!("  {format}");
        }
        info += &format!("  {}", file_size(self.bytes));
        if let Some(placed) = self.place(cell_pixels(), area) {
            let percent = (placed.scale * 100.0).round();
            if percent != 100.0 {
                info += &format!("  {percent}%");
            }
        }
        Status::Info(info)
    }
}

/// The terminal's cells' size in pixels, or a guess.
fn cell_pixels() -> (u32, u32) {
    let (width, height) = tty::cell_pixels().unwrap_or(DEFAULT_CELL);
    (width.max(1), height.max(1))
}

/// `zoom` of an `image` pixels in size in `area`, with cells `cell` pixels
/// in size, zoomed by `factor`, keeping what's under screen cell `at`
/// there, but not past fitting `area` or [`MAX_SCALE`]. `None`, zoomed
/// out to fitting.
fn zoom_at(
    image: (u32, u32),
    zoom: Option<Zoom>,
    factor: f64,
    at: (u32, u32),
    cell: (u32, u32),
    area: Rect,
) -> Option<Zoom> {
    let placed = place(image, zoom, cell, area)?;
    let fit = fit_scale(image, cell, area);
    let scale = (placed.scale * factor).clamp(fit, MAX_SCALE.max(fit));
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
/// `cell` pixels in size, but doesn't enlarge it.
fn fit_scale(image: (u32, u32), cell: (u32, u32), area: Rect) -> f64 {
    let width = area.width as f64 * cell.0 as f64 / image.0.max(1) as f64;
    let height = area.height as f64 * cell.1 as f64 / image.1.max(1) as f64;
    width.min(height).min(1.0)
}

/// Where an `image` pixels in size goes in `area`, with cells `cell`
/// pixels in size: fit and centered, or with `zoom`, as large as it says,
/// and where it says as far as it can without leaving room at an edge
/// that could be image. Along a side that fits, it's centered. `None` if
/// there's no room or no image.
fn place(image: (u32, u32), zoom: Option<Zoom>, cell: (u32, u32), area: Rect) -> Option<Placement> {
    if area.width == 0 || area.height == 0 || image.0 == 0 || image.1 == 0 {
        return None;
    }
    let fit = fit_scale(image, cell, area);
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
        let placed = place(image, zoom, (10, 20), area)?;
        let (x, y) = (placed.x - area.x as i64, placed.y - area.y as i64);
        Some((placed.cols, placed.rows, x, y))
    }

    #[test]
    fn images_fit_keep_their_shape_and_are_never_enlarged() {
        // 800x400 into 50x20 cells of 10x20: 500 pixels wide at most.
        assert_eq!(placed((800, 400), None, area(50, 20)), Some((50, 13, 0, 4)));
        // Tall: the height limits it.
        assert_eq!(placed((400, 800), None, area(50, 20)), Some((20, 20, 15, 0)));
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
        let zoom = zoom_at(image, None, 1.6, at, cell, area).unwrap();
        assert!((zoom.scale - 1.0).abs() < 1e-9, "{zoom:?}");
        // The mouse was 105 pixels into the image, its 168th pixel across,
        // which stays 105 pixels in at full size.
        assert!((zoom.origin.0 - (105.0 - 168.0)).abs() < 1e-9, "{zoom:?}");
        // 80x20 cells now, 6.3 of them left of the area. It fits
        // vertically, so it's centered that way.
        assert_eq!(placed(image, Some(zoom), area), Some((80, 20, -6, 0)));

        // Zooming out past fitting fits it again.
        assert_eq!(zoom_at(image, Some(zoom), 0.4, at, cell, area), None);
        // And there's a limit going in.
        let mut zoom = Some(zoom);
        for _ in 0..100 {
            zoom = zoom_at(image, zoom, 2.0, at, cell, area);
        }
        assert_eq!(zoom.unwrap().scale, MAX_SCALE);
    }

    #[test]
    fn zoomed_images_stay_in_bounds() {
        let (image, area) = ((800, 400), area(50, 20));
        let zoom = |origin| Some(Zoom { scale: 1.0, origin });
        // 80x20 cells in 50x20: it moves left and right only, as far as
        // its edges.
        assert_eq!(placed(image, zoom((0.0, 0.0)), area), Some((80, 20, 0, 0)));
        assert_eq!(placed(image, zoom((-1000.0, 90.0)), area), Some((80, 20, -30, 0)));
        assert_eq!(placed(image, zoom((70.0, -90.0)), area), Some((80, 20, 0, 0)));
        // Zoomed less than fitting, as the panel grew, it fits.
        let small = Some(Zoom {
            scale: 0.1,
            origin: (-100.0, 0.0),
        });
        assert_eq!(placed(image, small, area), placed(image, None, area));
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
