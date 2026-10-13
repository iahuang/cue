//! PDF files, read and rendered by hayro, a page at a time, for an image
//! view to show its pages with. hayro is all Rust, so PDFs look the same wherever cue
//! runs. A malformed file can make it panic, which is contained to that
//! file: it can't be opened, or a page can't be shown.
//!
//! Pages render on a thread of their own, which wakes the main loop when
//! one's done, as a page full of images can take a good part of a second.
//! While they're zoomed, what's asked for waits until the zooming stops,
//! as each step of it changes their size.

use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::{DecryptionError, LoadPdfError, Pdf as Parsed};
use hayro::kurbo::Affine;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{Pixmap, RasterizerSettings, RenderContext, Resources, TargetInit};
use hayro::{RenderCache, RenderSettings};

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use crate::crash;
use crate::wake;

pub struct Pdf {
    parsed: Parsed,
}

impl Pdf {
    /// Reads the PDF in `data`, or says why it can't.
    pub fn open(data: Vec<u8>) -> Result<Pdf, String> {
        let parsed = crash::contain(|| Parsed::new(data))
            .map_err(|panic| format!("Can't read this PDF: {panic}"))?
            .map_err(|err| reason(err).to_string())?;
        if parsed.pages().is_empty() {
            return Err("This PDF has no pages".into());
        }
        Ok(Pdf { parsed })
    }

    /// How many pages it has, at least one.
    pub fn pages(&self) -> usize {
        self.parsed.pages().len()
    }

    /// Page `page`'s size in points, turned as the page says it's shown.
    pub fn page_size(&self, page: usize) -> (f64, f64) {
        let (width, height) = self.parsed.pages()[page].render_dimensions();
        (width as f64, height as f64)
    }

    /// Renders part of page `page`, on white: `size` pixels of it, with
    /// the page `scale` pixels per point across and down, starting `from`
    /// pixels right of and below its top-left corner. Rows of RGBA bytes,
    /// top first.
    pub fn render(
        &self,
        page: usize,
        scale: (f64, f64),
        from: (f64, f64),
        size: (u32, u32),
    ) -> Result<Vec<u8>, String> {
        let page = &self.parsed.pages()[page];
        let (width, height) = (clamp_u16(size.0), clamp_u16(size.1));
        let transform = Affine::translate((-from.0, -from.1))
            * Affine::scale_non_uniform(scale.0, scale.1)
            * page.initial_transform(true).to_kurbo();
        crash::contain(|| {
            let mut context = RenderContext::new(width, height);
            hayro::render_into(
                page,
                &RenderCache::new(),
                &InterpreterSettings::default(),
                &RenderSettings::default(),
                &mut context,
                transform,
            );
            context.flush();
            let mut pixmap = Pixmap::new(width, height);
            let settings = RasterizerSettings {
                target_init: TargetInit::Clear(WHITE),
                ..Default::default()
            };
            context.render_with(&mut pixmap, &mut Resources::default(), settings);
            // Premultiplied, but on white, every pixel is opaque.
            pixmap.data_as_u8_slice().to_vec()
        })
        .map_err(|panic| format!("Can't render this page: {panic}"))
    }
}

/// What to render: [`Pdf::render`]'s arguments, and `tag`, which comes
/// back with what's rendered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Job<T> {
    pub tag: T,
    pub page: usize,
    pub scale: (f64, f64),
    pub from: (f64, f64),
    pub size: (u32, u32),
}

/// A job done: RGBA rows, as [`Pdf::render`] gives them, or why not.
pub type Rendered<T> = (Job<T>, Result<Vec<u8>, String>);

/// Renders a PDF's pages on a thread of its own, which ends when this is
/// dropped and what it's rendering is done. Jobs are asked for in lists,
/// rendered in order, and a list asked for replaces what's left of the
/// last: it's rendered after the job rendering now, once it's been
/// `settle` without a newer one.
pub struct Renderer<T> {
    jobs: Sender<(Vec<Job<T>>, Duration)>,
    done: Receiver<Rendered<T>>,
}

impl<T: Send + 'static> Renderer<T> {
    pub fn new(pdf: Arc<Pdf>) -> Renderer<T> {
        let (jobs, asked) = mpsc::channel::<(Vec<Job<T>>, Duration)>();
        let (finished, done) = mpsc::channel();
        std::thread::spawn(move || {
            let mut jobs = VecDeque::new();
            loop {
                let mut newer = asked.try_iter().last();
                if newer.is_none() && jobs.is_empty() {
                    match asked.recv() {
                        Ok(list) => newer = Some(list),
                        Err(_) => return,
                    }
                }
                // Each list waits out its settle for a newer one.
                while let Some((list, settle)) = newer.take() {
                    if !settle.is_zero() {
                        match asked.recv_timeout(settle) {
                            Ok(list) => {
                                newer = Some(list);
                                continue;
                            }
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    jobs = list.into();
                }
                let Some(job) = jobs.pop_front() else {
                    continue;
                };
                let rendered = pdf.render(job.page, job.scale, job.from, job.size);
                if finished.send((job, rendered)).is_err() {
                    return;
                }
                wake::wake();
            }
        });
        Renderer { jobs, done }
    }

    /// Asks for `jobs` to be rendered, in order, after what's rendering
    /// now and instead of what was asked for before, once `settle` passes
    /// without asking again.
    pub fn ask(&self, jobs: Vec<Job<T>>, settle: Duration) {
        let _ = self.jobs.send((jobs, settle));
    }

    /// What's been rendered since the last call, oldest first, as RGBA
    /// rows or why it couldn't be.
    pub fn finished(&self) -> impl Iterator<Item = Rendered<T>> + '_ {
        self.done.try_iter()
    }
}

fn clamp_u16(n: u32) -> u16 {
    n.clamp(1, u16::MAX as u32) as u16
}

/// Why a PDF couldn't be read.
fn reason(err: LoadPdfError) -> &'static str {
    match err {
        LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
            "Password-protected PDFs aren't supported"
        }
        LoadPdfError::Decryption(DecryptionError::UnsupportedAlgorithm) => {
            "This PDF's encryption isn't supported"
        }
        LoadPdfError::Decryption(_) => "This PDF's encryption is malformed",
        LoadPdfError::Invalid => "Not a PDF, or a malformed one",
    }
}

/// A PDF of `pages`, each `width` x `height` points and drawn by its
/// content stream.
#[cfg(test)]
pub fn test_pdf(pages: &[((u32, u32), &str)]) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        String::new(),
    ];
    let mut kids = Vec::new();
    for &((width, height), content) in pages {
        let page = objects.len() + 1;
        kids.push(format!("{page} 0 R"));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents {} 0 R >>",
            page + 1
        ));
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        pages.len()
    );
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf += &format!("{} 0 obj\n{object}\nendobj\n", index + 1);
    }
    let xref = pdf.len();
    pdf += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        pdf += &format!("{offset:010} 00000 n \n");
    }
    pdf += &format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    );
    pdf.into_bytes()
}

/// Content filling a 100 x 100 point page with red.
#[cfg(test)]
pub const RED_100: &str = "1 0 0 rg 0 0 100 100 re f";

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * width + x) * 4) as usize;
        rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn pages_have_their_sizes() {
        let pdf = Pdf::open(test_pdf(&[((200, 100), ""), ((300, 400), "")])).unwrap();
        assert_eq!(pdf.pages(), 2);
        assert_eq!(pdf.page_size(0), (200.0, 100.0));
        assert_eq!(pdf.page_size(1), (300.0, 400.0));
    }

    #[test]
    fn parts_of_pages_render_where_asked() {
        // Red, with a blue square in the top-left quarter: PDF's origin
        // is at the bottom left.
        let content = format!("{RED_100} 0 0 1 rg 0 50 50 50 re f");
        let pdf = Pdf::open(test_pdf(&[((100, 100), &content)])).unwrap();

        // The whole page at 2 pixels a point.
        let rgba = pdf.render(0, (2.0, 2.0), (0.0, 0.0), (200, 200)).unwrap();
        assert_eq!(rgba.len(), 200 * 200 * 4);
        assert_eq!(pixel(&rgba, 200, 10, 10), [0, 0, 255, 255]);
        assert_eq!(pixel(&rgba, 200, 190, 190), [255, 0, 0, 255]);

        // Its bottom-right quarter: all red.
        let rgba = pdf
            .render(0, (2.0, 2.0), (100.0, 100.0), (100, 100))
            .unwrap();
        assert_eq!(pixel(&rgba, 100, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&rgba, 100, 99, 99), [255, 0, 0, 255]);

        // Past the page is white.
        let rgba = pdf.render(0, (1.0, 1.0), (50.0, 50.0), (100, 100)).unwrap();
        assert_eq!(pixel(&rgba, 100, 75, 75), [255, 255, 255, 255]);
    }

    #[test]
    fn pages_render_on_their_own_thread() {
        let _serial = crate::test_serial();
        let pdf = Pdf::open(test_pdf(&[((100, 100), RED_100), ((100, 100), "")])).unwrap();
        let renderer = Renderer::new(Arc::new(pdf));
        let job = |tag, page| Job {
            tag,
            page,
            scale: (1.0, 1.0),
            from: (0.0, 0.0),
            size: (10, 10),
        };
        wake::take();
        renderer.ask(vec![job("red", 0), job("blank", 1)], Duration::ZERO);
        renderer.ask(vec![job("first", 0), job("second", 1)], Duration::ZERO);
        let mut done = Vec::new();
        let start = std::time::Instant::now();
        while done
            .last()
            .is_none_or(|(job, _): &(Job<&str>, _)| job.tag != "second")
        {
            assert!(start.elapsed().as_secs() < 10, "nothing rendered");
            std::thread::sleep(std::time::Duration::from_millis(5));
            done.extend(renderer.finished());
        }
        assert!(wake::take());
        // The first list may have been cut short by the second, asked for
        // while it rendered, but what's rendered is in order.
        let tags: Vec<_> = done.iter().map(|(job, _)| job.tag).collect();
        assert!(tags.ends_with(&["first", "second"]), "{tags:?}");
        assert!(
            !tags.contains(&"blank") || tags.contains(&"red"),
            "{tags:?}"
        );
        for (job, rendered) in done {
            let color = match job.page {
                0 => [255, 0, 0, 255],
                _ => [255, 255, 255, 255],
            };
            assert_eq!(pixel(&rendered.unwrap(), 10, 5, 5), color, "{}", job.tag);
        }
    }

    #[test]
    fn what_waits_to_settle_is_replaced_by_what_is_asked_for_meanwhile() {
        let _serial = crate::test_serial();
        let pdf = Pdf::open(test_pdf(&[((100, 100), RED_100)])).unwrap();
        let renderer = Renderer::new(Arc::new(pdf));
        let job = |tag| Job {
            tag,
            page: 0,
            scale: (1.0, 1.0),
            from: (0.0, 0.0),
            size: (10, 10),
        };
        let settle = Duration::from_millis(200);
        renderer.ask(vec![job("zooming")], settle);
        renderer.ask(vec![job("zoomed")], settle);
        let start = std::time::Instant::now();
        let mut done = Vec::new();
        while done.is_empty() {
            assert!(start.elapsed().as_secs() < 10, "nothing rendered");
            std::thread::sleep(std::time::Duration::from_millis(5));
            done.extend(renderer.finished().map(|(job, _)| job.tag));
        }
        assert!(start.elapsed() >= settle);
        assert_eq!(done, ["zoomed"]);
    }

    #[test]
    fn what_isnt_a_pdf_says_so() {
        assert_eq!(
            Pdf::open(b"hello".to_vec()).err().as_deref(),
            Some("Not a PDF, or a malformed one")
        );
        assert!(Pdf::open(test_pdf(&[])).is_err());
    }
}
