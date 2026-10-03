//! Math in reader mode, drawn as images. RaTeX (KaTeX's parser and layout
//! rules, ported to Rust) lays the LaTeX out in KaTeX's fonts, and it's
//! drawn here into an image a whole number of cells in size, which the
//! renderer shows with the Kitty graphics protocol or Sixel.
//!
//! Display math is a little larger than the text around it, as KaTeX makes
//! it. Inline math is the same size, but shrunk to fit one row if it's
//! taller, with its baseline on the text's where there's room.
//!
//! Where the terminal can't show images, math stays text.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use opentui::Image;
use ratex_font::FontId;
use ratex_types::{Color, DisplayItem, DisplayList, MathStyle, PathCommand};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::tty;

/// A cell's size in pixels when the terminal doesn't say.
const DEFAULT_CELL: (u32, u32) = (8, 16);
/// The text's size, as a share of a cell's height.
const TEXT_EM: f32 = 0.8;
/// How much larger than the text math is.
const MATH_SCALE: f32 = 1.1;
/// Where the text's baseline is, as a share of a cell's height down it.
const BASELINE: f32 = 0.78;
/// Room either side of a formula, in pixels.
const PAD: f32 = 1.0;
/// The most rows display math takes: taller, it's shrunk to fit. (A
/// `\rule` can be any height, and the image is drawn whole.)
const MAX_ROWS: f32 = 100.0;

/// Whether the terminal shows images, so math is drawn as them.
static IMAGES: AtomicBool = AtomicBool::new(false);

/// Says whether the terminal shows images, as the renderer has found out.
pub fn set_images(images: bool) {
    IMAGES.store(images, Ordering::Relaxed);
}

/// The size of a cell in pixels to draw math for, or `None` if math is
/// text.
pub fn cell() -> Option<(u32, u32)> {
    if !IMAGES.load(Ordering::Relaxed) {
        return None;
    }
    let (width, height) = tty::cell_pixels().unwrap_or(DEFAULT_CELL);
    Some((width.max(1), height.max(1)))
}

/// A formula laid out for cells of some size.
pub struct Formula {
    /// The LaTeX, as written.
    pub tex: String,
    pub display: bool,
    /// The cells it takes, and their size in pixels.
    pub cols: u32,
    pub rows: u32,
    pub cell: (u32, u32),
    list: DisplayList,
    /// Pixels per em.
    em: f32,
    /// Where its box's top left corner goes, in pixels from the image's.
    origin: (f32, f32),
    /// Its image, drawn in a color.
    image: RefCell<Option<([u8; 3], Rc<Image>)>>,
}

impl std::fmt::Debug for Formula {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Formula")
            .field("tex", &self.tex)
            .field("display", &self.display)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish()
    }
}

impl Formula {
    /// Lays out `tex`, display math if `display`, for cells `cell` pixels
    /// in size, at most `max_cols` wide. Fails if it doesn't parse.
    pub fn new(
        tex: &str,
        display: bool,
        cell: (u32, u32),
        max_cols: usize,
    ) -> Result<Formula, String> {
        let ast = ratex_parser::parse(tex).map_err(|err| err.to_string())?;
        if ast.is_empty() {
            return Err("empty".to_string());
        }
        let options = ratex_layout::LayoutOptions {
            style: match display {
                true => MathStyle::Display,
                false => MathStyle::Text,
            },
            ..Default::default()
        };
        let list = ratex_layout::to_display_list(&ratex_layout::layout(&ast, &options));
        let (width, height, depth) = (list.width as f32, list.height as f32, list.depth as f32);
        if !(width.is_finite() && height.is_finite() && depth.is_finite()) {
            return Err("unbounded".to_string());
        }
        let (cell_w, cell_h) = (cell.0 as f32, cell.1 as f32);
        let mut em = cell_h * TEXT_EM * MATH_SCALE;
        // Inline math fits a row, display math [`MAX_ROWS`], and all of it
        // fits the width it has.
        let rows = if display { MAX_ROWS } else { 1.0 };
        if height + depth > 0.0 {
            em = em.min(rows * cell_h / (height + depth));
        }
        let room = max_cols.max(1) as f32 * cell_w - 2.0 * PAD;
        if width > 0.0 {
            em = em.min(room / width);
        }
        let cols = (((width * em + 2.0 * PAD) / cell_w).ceil() as u32).max(1);
        let rows = match display {
            true => ((((height + depth) * em) / cell_h).ceil() as u32).max(1),
            false => 1,
        };
        let x = (cols as f32 * cell_w - width * em) / 2.0;
        let y = match display {
            true => (rows as f32 * cell_h - (height + depth) * em) / 2.0,
            // The baseline on the text's, or as near as fits.
            false => {
                let baseline = (cell_h * BASELINE).min(cell_h - depth * em);
                baseline.max(height * em) - height * em
            }
        };
        Ok(Formula {
            tex: tex.to_string(),
            display,
            cols,
            rows,
            list,
            cell,
            em,
            origin: (x, y),
            image: RefCell::new(None),
        })
    }

    /// Its image, with `fg` for the color of text, which is what color it
    /// is but where it says otherwise.
    pub fn image(&self, fg: [u8; 3]) -> Option<Rc<Image>> {
        if let Some((color, image)) = &*self.image.borrow() {
            if *color == fg {
                return Some(image.clone());
            }
        }
        let (width, height, pixels) = self.rgba(fg);
        let image = Rc::new(Image::from_rgba(width, height, &pixels).ok()?);
        *self.image.borrow_mut() = Some((fg, image.clone()));
        Some(image)
    }

    /// Draws it: its width and height in pixels, and their RGBA, with
    /// straight alpha.
    pub fn rgba(&self, fg: [u8; 3]) -> (u32, u32, Vec<u8>) {
        let width = self.cols * self.cell.0;
        let height = self.rows * self.cell.1;
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return (width, height, vec![0; (width * height * 4) as usize]);
        };
        let canvas = Canvas {
            em: self.em,
            origin: self.origin,
            fg,
        };
        for item in &self.list.items {
            canvas.draw(&mut pixmap, item);
        }
        let mut pixels = pixmap.take();
        for pixel in pixels.chunks_exact_mut(4) {
            let alpha = pixel[3] as u32;
            if alpha > 0 && alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        (width, height, pixels)
    }
}

/// Formulas laid out for one cell size, kept from one layout of the text
/// to the next, with their images.
pub struct Typesetter {
    cell: (u32, u32),
    kept: HashMap<(String, bool, usize), Option<Rc<Formula>>>,
    /// Those asked for in the layout being made.
    used: HashMap<(String, bool, usize), Option<Rc<Formula>>>,
}

impl Typesetter {
    pub fn new(cell: (u32, u32)) -> Typesetter {
        Typesetter {
            cell,
            kept: HashMap::new(),
            used: HashMap::new(),
        }
    }

    pub fn cell(&self) -> (u32, u32) {
        self.cell
    }

    /// `tex` laid out, as [`Formula::new`] does, or `None` if it doesn't
    /// parse.
    pub fn formula(&mut self, tex: &str, display: bool, max_cols: usize) -> Option<Rc<Formula>> {
        let key = (tex.to_string(), display, max_cols);
        if let Some(formula) = self.used.get(&key) {
            return formula.clone();
        }
        let formula = match self.kept.remove(&key) {
            Some(formula) => formula,
            None => Formula::new(tex, display, self.cell, max_cols)
                .ok()
                .map(Rc::new),
        };
        self.used.insert(key, formula.clone());
        formula
    }

    /// After a layout: forgets the formulas it didn't use.
    pub fn finish(&mut self) {
        self.kept = std::mem::take(&mut self.used);
    }
}

/// Where and how a formula's display list is drawn.
struct Canvas {
    em: f32,
    origin: (f32, f32),
    fg: [u8; 3],
}

impl Canvas {
    /// A point `x`, `y` ems into the formula's box, in pixels.
    fn at(&self, x: f64, y: f64) -> (f32, f32) {
        (
            self.origin.0 + x as f32 * self.em,
            self.origin.1 + y as f32 * self.em,
        )
    }

    /// Paint in `color`, or in the text's where it's the default (black).
    fn paint(&self, color: &Color) -> Paint<'static> {
        let (r, g, b) = match *color == Color::BLACK {
            true => (self.fg[0], self.fg[1], self.fg[2]),
            false => (
                (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
            ),
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, (color.a.clamp(0.0, 1.0) * 255.0).round() as u8);
        paint.anti_alias = true;
        paint
    }

    fn draw(&self, pixmap: &mut Pixmap, item: &DisplayItem) {
        match item {
            DisplayItem::GlyphPath {
                x,
                y,
                scale,
                font,
                char_code,
                color,
            } => {
                let (px, py) = self.at(*x, *y);
                let font = FontId::parse(font).unwrap_or(FontId::MainRegular);
                let mut path = PathBuilder::new();
                if glyph(&mut path, font, *char_code, px, py, self.em * *scale as f32) {
                    if let Some(path) = path.finish() {
                        let paint = self.paint(color);
                        pixmap.fill_path(
                            &path,
                            &paint,
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    }
                }
            }
            DisplayItem::Line {
                x,
                y,
                width,
                thickness,
                color,
                dashed,
            } => {
                let (px, py) = self.at(*x, *y);
                let width = *width as f32 * self.em;
                let thickness = (*thickness as f32 * self.em).max(1.0);
                let dash = match dashed {
                    true => (4.0 * thickness).max(2.0),
                    false => width,
                };
                let mut start = px;
                while start < px + width {
                    let length = dash.min(px + width - start);
                    self.rect(
                        pixmap,
                        start,
                        py - thickness / 2.0,
                        length,
                        thickness,
                        color,
                    );
                    start += 2.0 * dash;
                }
            }
            DisplayItem::Rect {
                x,
                y,
                width,
                height,
                color,
            } => {
                let (px, py) = self.at(*x, *y);
                let (width, height) = (*width as f32 * self.em, *height as f32 * self.em);
                self.rect(pixmap, px, py, width, height, color);
            }
            DisplayItem::Path {
                x,
                y,
                commands,
                fill,
                color,
            } => {
                let origin = self.at(*x, *y);
                let paint = self.paint(color);
                if *fill {
                    // A subpath at a time: stretchy arrows are pieces whose
                    // windings may be opposite, and would cancel out where
                    // they overlap. Even-odd, as RaTeX fills them, for tall
                    // delimiters' stems.
                    let starts = commands
                        .iter()
                        .enumerate()
                        .filter(|(_, command)| matches!(command, PathCommand::MoveTo { .. }))
                        .map(|(i, _)| i)
                        .chain([commands.len()])
                        .collect::<Vec<_>>();
                    for part in starts.windows(2) {
                        if let Some(path) = self.path(origin, &commands[part[0]..part[1]]) {
                            pixmap.fill_path(
                                &path,
                                &paint,
                                FillRule::EvenOdd,
                                Transform::identity(),
                                None,
                            );
                        }
                    }
                } else if let Some(path) = self.path(origin, commands) {
                    let stroke = Stroke {
                        width: 1.5,
                        ..Default::default()
                    };
                    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
        }
    }

    fn rect(&self, pixmap: &mut Pixmap, x: f32, y: f32, width: f32, height: f32, color: &Color) {
        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, width, height) {
            // As a path, which is anti-aliased: thin rules stay visible.
            let path = PathBuilder::from_rect(rect);
            let paint = self.paint(color);
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    fn path(&self, origin: (f32, f32), commands: &[PathCommand]) -> Option<tiny_skia::Path> {
        let em = self.em;
        let at = |x: f64, y: f64| (origin.0 + x as f32 * em, origin.1 + y as f32 * em);
        let mut path = PathBuilder::new();
        for command in commands {
            match *command {
                PathCommand::MoveTo { x, y } => {
                    let (x, y) = at(x, y);
                    path.move_to(x, y);
                }
                PathCommand::LineTo { x, y } => {
                    let (x, y) = at(x, y);
                    path.line_to(x, y);
                }
                PathCommand::QuadTo { x1, y1, x, y } => {
                    let ((x1, y1), (x, y)) = (at(x1, y1), at(x, y));
                    path.quad_to(x1, y1, x, y);
                }
                PathCommand::CubicTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x,
                    y,
                } => {
                    let ((x1, y1), (x2, y2), (x, y)) = (at(x1, y1), at(x2, y2), at(x, y));
                    path.cubic_to(x1, y1, x2, y2, x, y);
                }
                PathCommand::Close => path.close(),
            }
        }
        path.finish()
    }
}

/// Adds the outline of character `code` in `font`, at `size` pixels per em
/// with its origin at (`x`, `y`), to `path`. Returns false if the font
/// (or else KaTeX's main font) hasn't got it.
fn glyph(path: &mut PathBuilder, font: FontId, code: u32, x: f32, y: f32, size: f32) -> bool {
    let c = ratex_font::katex_ttf_glyph_char(font, code);
    let found = face(font)
        .and_then(|face| Some((face, face.glyph_index(c)?)))
        .or_else(|| {
            let face = face(FontId::MainRegular)?;
            Some((face, face.glyph_index(c)?))
        });
    let Some((face, id)) = found else {
        return false;
    };
    let scale = size / face.units_per_em() as f32;
    let mut outline = Outline { path, x, y, scale };
    face.outline_glyph(id, &mut outline).is_some()
}

/// A glyph's outline, flipped (fonts' y is up) and scaled into a path.
struct Outline<'a> {
    path: &'a mut PathBuilder,
    x: f32,
    y: f32,
    scale: f32,
}

impl Outline<'_> {
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        (self.x + x * self.scale, self.y - y * self.scale)
    }
}

impl ttf_parser::OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.path.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.path.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ((x1, y1), (x, y)) = (self.at(x1, y1), self.at(x, y));
        self.path.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let ((x1, y1), (x2, y2), (x, y)) = (self.at(x1, y1), self.at(x2, y2), self.at(x, y));
        self.path.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.path.close();
    }
}

macro_rules! fonts {
    ($($id:ident => $file:literal),* $(,)?) => {
        &[$((FontId::$id, include_bytes!(concat!("../../../vendor/katex-fonts/", $file)))),*]
    };
}

/// KaTeX's fonts, by RaTeX's names for them.
const FONTS: &[(FontId, &[u8])] = fonts! {
    AmsRegular => "KaTeX_AMS-Regular.ttf",
    CaligraphicRegular => "KaTeX_Caligraphic-Regular.ttf",
    FrakturRegular => "KaTeX_Fraktur-Regular.ttf",
    FrakturBold => "KaTeX_Fraktur-Bold.ttf",
    MainBold => "KaTeX_Main-Bold.ttf",
    MainBoldItalic => "KaTeX_Main-BoldItalic.ttf",
    MainItalic => "KaTeX_Main-Italic.ttf",
    MainRegular => "KaTeX_Main-Regular.ttf",
    MathBoldItalic => "KaTeX_Math-BoldItalic.ttf",
    MathItalic => "KaTeX_Math-Italic.ttf",
    SansSerifBold => "KaTeX_SansSerif-Bold.ttf",
    SansSerifItalic => "KaTeX_SansSerif-Italic.ttf",
    SansSerifRegular => "KaTeX_SansSerif-Regular.ttf",
    ScriptRegular => "KaTeX_Script-Regular.ttf",
    Size1Regular => "KaTeX_Size1-Regular.ttf",
    Size2Regular => "KaTeX_Size2-Regular.ttf",
    Size3Regular => "KaTeX_Size3-Regular.ttf",
    Size4Regular => "KaTeX_Size4-Regular.ttf",
    TypewriterRegular => "KaTeX_Typewriter-Regular.ttf",
};

fn face(font: FontId) -> Option<&'static ttf_parser::Face<'static>> {
    static FACES: OnceLock<HashMap<FontId, ttf_parser::Face<'static>>> = OnceLock::new();
    FACES
        .get_or_init(|| {
            FONTS
                .iter()
                .filter_map(|(id, data)| Some((*id, ttf_parser::Face::parse(data, 0).ok()?)))
                .collect()
        })
        .get(&font)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (u32, u32) = (10, 20);

    #[test]
    fn every_font_parses() {
        for (id, _) in FONTS {
            assert!(face(*id).is_some(), "{id:?}");
        }
    }

    #[test]
    fn display_math_takes_whole_cells() {
        let formula = Formula::new(r"\frac{a}{b} + \sqrt{x^2}", true, CELL, 80).unwrap();
        assert!(formula.rows >= 2, "{}", formula.rows);
        let (width, height, pixels) = formula.rgba([255, 255, 255]);
        assert_eq!(
            (width, height),
            (formula.cols * CELL.0, formula.rows * CELL.1)
        );
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        // Something was drawn, in the text's color.
        let opaque: Vec<&[u8]> = pixels.chunks(4).filter(|p| p[3] == 255).collect();
        assert!(!opaque.is_empty());
        assert!(opaque.iter().all(|p| p[..3] == [255, 255, 255]));
    }

    #[test]
    fn inline_math_fits_a_row() {
        for tex in [
            "x",
            r"x_i^2",
            r"\frac{QK^\top}{\sqrt{d_k}}",
            r"\sum_{i=1}^n",
        ] {
            let formula = Formula::new(tex, false, CELL, 80).unwrap();
            assert_eq!(formula.rows, 1, "{tex}");
        }
    }

    #[test]
    fn math_fits_the_width_it_has() {
        let tex = r"a + b + c + d + e + f + g + h + i + j + k + l + m + n + o + p";
        assert!(Formula::new(tex, true, CELL, 200).unwrap().cols > 10);
        assert!(Formula::new(tex, true, CELL, 10).unwrap().cols <= 10);
    }

    #[test]
    fn math_has_bounds() {
        for tex in [r"\rule{1em}{100000em}", r"\raisebox{100000em}{x}"] {
            let formula = Formula::new(tex, true, CELL, 80).unwrap();
            assert!(formula.rows <= MAX_ROWS as u32, "{tex}: {}", formula.rows);
        }
    }

    #[test]
    fn colors_it_names_stay() {
        let formula = Formula::new(r"\color{red}{x}", true, CELL, 80).unwrap();
        let (_, _, pixels) = formula.rgba([255, 255, 255]);
        assert!(pixels.chunks(4).any(|p| p == [255, 0, 0, 255]));
    }

    #[test]
    fn bad_latex_fails() {
        assert!(Formula::new(r"\frac{a}{", true, CELL, 80).is_err());
        assert!(Formula::new("", false, CELL, 80).is_err());
    }

    #[test]
    fn typesetter_keeps_what_the_last_layout_used() {
        let mut typesetter = Typesetter::new(CELL);
        let a = typesetter.formula("x", false, 80).unwrap();
        typesetter.formula("y", false, 80).unwrap();
        typesetter.finish();
        assert!(Rc::ptr_eq(&a, &typesetter.formula("x", false, 80).unwrap()));
        typesetter.finish();
        // "y" wasn't used last time, so it's laid out again.
        assert_eq!(typesetter.kept.len(), 1);
    }
}
