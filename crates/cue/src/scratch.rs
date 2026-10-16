//! The scratchpad: a panel of its own, floating over the others, that
//! Ctrl+; shows and puts away, as tiling window managers' scratchpads and
//! Neovim's floating terminals are.
//!
//! It's no tab's: the same one floats over whichever tab is on screen, and
//! keeps what it shows, and its history, while put away. It shows what any
//! panel can, but can't be split, zoomed, or arranged, though layout mode
//! cuts, copies, and pastes to and from it as it does panels. While it's
//! out, it has the keyboard, and what's opened, from the sidebar or
//! anywhere else, opens in it. A click on the panels around it puts it
//! away.
//!
//! It's centered over the panels, framed, with its panel's header in the
//! frame's top edge, and dragging the frame resizes it, evenly on both
//! sides; so do the arrows in layout mode. Its size is kept as a share of
//! the panels' area, so it scales with the screen.

use opentui::Buffer;

use crate::keymap::Keymap;
use crate::layout::{self, Direction, PanelId, Rect};
use crate::panel::Panel;
use crate::tab;
use crate::theme;
use crate::workspace::Workspace;

/// The scratchpad's panel id, which no tab's panel has.
pub const ID: PanelId = PanelId::MAX;
/// Its share of the panels' area, across and down, until resized.
const DEFAULT_SIZE: (f32, f32) = (0.7, 0.7);

/// Which of its frame's edges a drag on it moves: the sides, the top and
/// bottom, or at a corner, both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub sides: bool,
    pub ends: bool,
}

pub struct Scratch {
    pub panel: Panel,
    /// It's out, over the panels.
    pub open: bool,
    /// Its share of the panels' area, across and down.
    size: (f32, f32),
}

impl Scratch {
    pub fn new() -> Scratch {
        Scratch {
            panel: Panel::new(ID),
            open: false,
            size: DEFAULT_SIZE,
        }
    }

    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    pub fn set_size(&mut self, (width, height): (f32, f32)) {
        let share = |n: f32| match n.is_finite() {
            true => n.clamp(0.0, 1.0),
            false => 0.0,
        };
        self.size = (share(width), share(height));
    }

    /// Goes back to the size it starts at.
    pub fn reset_size(&mut self) {
        self.size = DEFAULT_SIZE;
    }

    /// Where it goes over `area`, the panels', with its frame.
    pub fn outer(&self, area: Rect) -> Rect {
        let cells = |share: f32, whole: u32, min: u32| {
            let cells = (share * whole as f32).round() as u32;
            cells.max(min).min(whole)
        };
        let width = cells(self.size.0, area.width, layout::MIN_WIDTH + 2);
        let height = cells(self.size.1, area.height, layout::MIN_HEIGHT + 2);
        Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        }
    }

    /// Lays its panel out over `area`, the panels': inside its frame, but
    /// for the header, which is the frame's top edge. Without room for a
    /// frame, it's as a panel is.
    pub fn set_area(&mut self, area: Rect) {
        let outer = self.outer(area);
        let framed = tab::zoomed_area(outer) != outer;
        self.panel.set_framed(framed);
        self.panel.set_area(match framed {
            true => Rect {
                x: outer.x + 1,
                y: outer.y,
                width: outer.width - 2,
                height: outer.height - 1,
            },
            false => outer,
        });
    }

    /// Which of the frame's edges are at screen cell (`x`, `y`), over
    /// `area`, if any. Between the top corners is the header's.
    pub fn edges_at(&self, area: Rect, x: u32, y: u32) -> Option<Edges> {
        let outer = self.outer(area);
        if !outer.contains(x, y) || tab::zoomed_area(outer) == outer {
            return None;
        }
        let edges = Edges {
            sides: x == outer.x || x == outer.x + outer.width - 1,
            ends: y == outer.y || y == outer.y + outer.height - 1,
        };
        if y == outer.y && !edges.sides {
            return None;
        }
        (edges.sides || edges.ends).then_some(edges)
    }

    /// Resizes it, as dragging `edges` of its frame to screen cell (`x`,
    /// `y`), over `area`, does: the opposite edges move as far the other
    /// way, so it stays centered.
    pub fn drag(&mut self, area: Rect, edges: Edges, x: u32, y: u32) {
        let outer = self.outer(area);
        let span = |at: u32, start: u32, length: u32| {
            // Twice the distance from the center, in half cells, rounded up
            // to a whole cell.
            let center = 2 * start + length - 1;
            (2 * at).abs_diff(center) + 1
        };
        if edges.sides && area.width > 0 {
            let width = span(x, outer.x, outer.width);
            self.size.0 = (width as f32 / area.width as f32).min(1.0);
        }
        if edges.ends && area.height > 0 {
            let height = span(y, outer.y, outer.height);
            self.size.1 = (height as f32 / area.height as f32).min(1.0);
        }
    }

    /// Resizes it by `cells` a side, over `area`: wider going right,
    /// narrower going left, taller going up, and shorter going down.
    pub fn nudge(&mut self, area: Rect, direction: Direction, cells: u32) {
        let outer = self.outer(area);
        let resize = |length: u32, grow: bool, whole: u32| {
            let length = match grow {
                true => length + 2 * cells,
                false => length.saturating_sub(2 * cells),
            };
            (length as f32 / whole.max(1) as f32).min(1.0)
        };
        match direction {
            Direction::Right | Direction::Left => {
                let grow = direction == Direction::Right;
                self.size.0 = resize(outer.width, grow, area.width);
                // Not narrower than it can be, so going back is as far.
                self.size.0 = self.size.0.max(self.min_share(area).0);
            }
            Direction::Up | Direction::Down => {
                let grow = direction == Direction::Up;
                self.size.1 = resize(outer.height, grow, area.height);
                self.size.1 = self.size.1.max(self.min_share(area).1);
            }
        }
    }

    /// The smallest it can be over `area`, as a share of it.
    fn min_share(&self, area: Rect) -> (f32, f32) {
        let share = |min: u32, whole: u32| (min as f32 / whole.max(1) as f32).min(1.0);
        (
            share(layout::MIN_WIDTH + 2, area.width),
            share(layout::MIN_HEIGHT + 2, area.height),
        )
    }

    /// Draws it over `area`, the panels', covering what's under it, and
    /// returns where the cursor goes in it, if anywhere. It's set apart from
    /// them by its background, the one code blocks in reader mode have:
    /// what's drawn on the theme's background, or the terminal's, is on that
    /// instead, and what has a background of its own, such as the
    /// selection, keeps it. A terminal in it has the last word on its own
    /// cells, as a program in it may set any background.
    pub fn draw(
        &self,
        frame: &Buffer,
        area: Rect,
        keymap: &Keymap,
        workspace: &Workspace,
        preview: bool,
    ) -> Option<(u32, u32)> {
        let outer = self.outer(area);
        let colors = theme::colors();
        frame.fill_rect(outer.x, outer.y, outer.width, outer.height, colors.bg);
        tab::draw_zoom_frame(frame, outer);
        let cursor = self.panel.draw(frame, keymap, workspace, true, preview);
        let (bg, set_apart) = (colors.bg, colors.surface_inactive);
        let plain = |color: opentui::Rgba| {
            color.is_terminal_default()
                || (color.r(), color.g(), color.b()) == (bg.r(), bg.g(), bg.b())
        };
        // A terminal draws its own: the same background, where its program
        // doesn't choose another (see `App::set_terminal_backgrounds`).
        let terminal = self.panel.terminal().map(|_| self.panel.body());
        let mapped = |x, y| !terminal.is_some_and(|body| body.contains(x, y));
        frame.map_background(
            outer.x,
            outer.y,
            outer.width,
            outer.height,
            |x, y, color| match mapped(x, y) && plain(color) {
                true => set_apart,
                false => color,
            },
        );
        cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 10,
        y: 1,
        width: 100,
        height: 40,
    };

    #[test]
    fn it_centers_over_the_panels_at_its_share() {
        let scratch = Scratch::new();
        assert_eq!(
            scratch.outer(AREA),
            Rect {
                x: 25,
                y: 7,
                width: 70,
                height: 28
            }
        );
        // Never smaller than a panel can be, framed, nor larger than there's
        // room for.
        let mut small = Scratch::new();
        small.set_size((0.0, 0.0));
        let outer = small.outer(AREA);
        assert_eq!((outer.width, outer.height), (22, 5));
        let tiny = Rect {
            width: 10,
            height: 3,
            ..AREA
        };
        assert_eq!(Scratch::new().outer(tiny), tiny);
    }

    #[test]
    fn dragging_the_frame_resizes_it_evenly() {
        let mut scratch = Scratch::new();
        let outer = scratch.outer(AREA);
        let right = outer.x + outer.width - 1;
        let edges = scratch.edges_at(AREA, right, outer.y + 3).unwrap();
        assert_eq!(
            edges,
            Edges {
                sides: true,
                ends: false
            }
        );
        assert_eq!(scratch.edges_at(AREA, right - 1, outer.y + 3), None);
        scratch.drag(AREA, edges, right + 5, 0);
        let wider = scratch.outer(AREA);
        assert_eq!((wider.width, wider.height), (80, 28));
        assert_eq!(wider.x, outer.x - 5, "both sides moved");
        assert_eq!(wider.x + wider.width - 1, right + 5);

        let corner = scratch.edges_at(AREA, wider.x, wider.y).unwrap();
        assert!(corner.sides && corner.ends);
        scratch.drag(AREA, corner, wider.x + 10, wider.y + 2);
        let smaller = scratch.outer(AREA);
        assert_eq!((smaller.width, smaller.height), (60, 24));
    }

    #[test]
    fn nudging_grows_and_shrinks_it() {
        let mut scratch = Scratch::new();
        scratch.nudge(AREA, Direction::Right, 2);
        scratch.nudge(AREA, Direction::Up, 2);
        let outer = scratch.outer(AREA);
        assert_eq!((outer.width, outer.height), (74, 32));
        for _ in 0..40 {
            scratch.nudge(AREA, Direction::Left, 2);
            scratch.nudge(AREA, Direction::Down, 2);
        }
        let outer = scratch.outer(AREA);
        assert_eq!((outer.width, outer.height), (22, 5));
        scratch.nudge(AREA, Direction::Right, 2);
        assert_eq!(scratch.outer(AREA).width, 26);
        scratch.reset_size();
        assert_eq!(scratch.outer(AREA).width, 70);
    }
}
