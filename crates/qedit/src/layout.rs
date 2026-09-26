//! How the space right of the file tree is divided into panels.
//!
//! The layout is a tree: each split divides its area in two, side by side or
//! stacked, at a ratio, and each leaf is a panel. Panels side by side have a
//! one-column divider between them. Stacked panels need none: the lower
//! one's header divides them. Dragging either resizes the split.
//!
//! A panel dragged by its header moves: dropped near an edge of another
//! panel, it splits that panel's room on that side; dropped in the middle,
//! the two swap places.

/// Names a panel for as long as it's open.
pub type PanelId = u32;

/// Panels are split no smaller than this, and resizing keeps them at least
/// this big where there's room.
pub const MIN_WIDTH: u32 = 20;
/// The header and two rows of text.
pub const MIN_HEIGHT: u32 = 3;
/// How near an edge of a panel, as a share of its size, a panel dropped on
/// it goes beside it rather than in its place.
const EDGE: f32 = 1.0 / 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Side by side.
    Horizontal,
    /// Stacked.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, x: u32, y: u32) -> bool {
        (self.x..self.x + self.width).contains(&x) && (self.y..self.y + self.height).contains(&y)
    }

    fn right(&self) -> u32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }
}

/// Where a panel dragged by its header would go if dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drop {
    /// The panel it's dropped on.
    pub target: PanelId,
    /// The side of the target it goes, or `None` to swap with it.
    pub side: Option<Direction>,
    /// The room it would take, as the layout is now, to preview.
    pub rect: Rect,
}

/// Where a split's two sides meet, which dragging moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    /// The way to the split from the top of the layout: `false` for the
    /// first side, `true` for the second.
    pub path: Vec<bool>,
    pub axis: Axis,
    /// The cells to grab: the divider column between panels side by side,
    /// or the lower panel's header.
    pub rect: Rect,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Panel(PanelId),
    Split {
        axis: Axis,
        /// The first side's share of the room.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    /// Splits panel `target` in two, with `new` right of it or below it.
    /// Returns false if there's no such panel.
    pub fn split(&mut self, target: PanelId, new: PanelId, axis: Axis) -> bool {
        let side = match axis {
            Axis::Horizontal => Direction::Right,
            Axis::Vertical => Direction::Down,
        };
        self.insert(target, new, side)
    }

    /// Splits panel `target` in two, with `new` on its `side`. Returns false
    /// if there's no such panel.
    fn insert(&mut self, target: PanelId, new: PanelId, side: Direction) -> bool {
        match self {
            Layout::Panel(id) if *id == target => {
                let axis = match side {
                    Direction::Left | Direction::Right => Axis::Horizontal,
                    Direction::Up | Direction::Down => Axis::Vertical,
                };
                let (first, second) = match side {
                    Direction::Left | Direction::Up => (new, target),
                    Direction::Right | Direction::Down => (target, new),
                };
                *self = Layout::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Layout::Panel(first)),
                    second: Box::new(Layout::Panel(second)),
                };
                true
            }
            Layout::Panel(_) => false,
            Layout::Split { first, second, .. } => {
                first.insert(target, new, side) || second.insert(target, new, side)
            }
        }
    }

    /// Moves panel `id` where `drop` says. Returns false if either panel is
    /// missing or they're the same.
    pub fn drop_panel(&mut self, id: PanelId, drop: Drop) -> bool {
        let ids = self.panels(Rect::default());
        let has = |panel| ids.iter().any(|&(p, _)| p == panel);
        if id == drop.target || !has(id) || !has(drop.target) {
            return false;
        }
        match drop.side {
            Some(side) => {
                self.remove(id);
                self.insert(drop.target, id, side)
            }
            None => {
                self.swap(id, drop.target);
                true
            }
        }
    }

    /// Swaps panels `a` and `b`, wherever they are.
    fn swap(&mut self, a: PanelId, b: PanelId) {
        match self {
            Layout::Panel(id) if *id == a => *id = b,
            Layout::Panel(id) if *id == b => *id = a,
            Layout::Panel(_) => {}
            Layout::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    /// Where panel `dragged`, dragged by its header to (`x`, `y`), would go,
    /// when the layout fills `area`: beside the panel there, on the side
    /// whose edge is near, or in its place in the middle. Too little room
    /// to split the panel there leaves only swapping. `None` over the
    /// dragged panel itself, or no panel.
    pub fn drop_at(&self, area: Rect, dragged: PanelId, x: u32, y: u32) -> Option<Drop> {
        let (target, rect) = self
            .panels(area)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))?;
        if target == dragged {
            return None;
        }
        // How far across the panel, from 0 to 1, in the middle of the cell.
        let across =
            |at: u32, start: u32, size: u32| (at - start) as f32 / size as f32 + 0.5 / size as f32;
        let (fx, fy) = (
            across(x, rect.x, rect.width),
            across(y, rect.y, rect.height),
        );
        let (near, side) = [
            (fx, Direction::Left),
            (1.0 - fx, Direction::Right),
            (fy, Direction::Up),
            (1.0 - fy, Direction::Down),
        ]
        .into_iter()
        .min_by(|a, b| a.0.total_cmp(&b.0))?;
        let mut side = (near < EDGE).then_some(side);
        if let Some(to) = side {
            // The target has the dragged panel's room too once it's gone.
            let mut without = self.clone();
            without.remove(dragged);
            let room = without
                .panels(area)
                .into_iter()
                .find(|&(id, _)| id == target)
                .map(|(_, rect)| rect)?;
            let fits = match to {
                Direction::Left | Direction::Right => room.width > 2 * MIN_WIDTH,
                Direction::Up | Direction::Down => room.height >= 2 * MIN_HEIGHT,
            };
            side = side.filter(|_| fits);
        }
        let half = |axis, ratio| divide(rect, axis, ratio);
        let rect = match side {
            None => rect,
            Some(Direction::Left) => half(Axis::Horizontal, 0.5).0,
            Some(Direction::Right) => half(Axis::Horizontal, 0.5).1,
            Some(Direction::Up) => half(Axis::Vertical, 0.5).0,
            Some(Direction::Down) => half(Axis::Vertical, 0.5).1,
        };
        Some(Drop { target, side, rect })
    }

    /// Removes panel `id`, giving its room to the other side of its split.
    /// Returns the panel there nearest to where it was, or `None` if there's
    /// no such panel or it's the only one.
    pub fn remove(&mut self, id: PanelId) -> Option<PanelId> {
        let Layout::Split { first, second, .. } = self else {
            return None;
        };
        let other = match (&**first, &**second) {
            (Layout::Panel(p), other) if *p == id => Some((other.clone(), false)),
            (other, Layout::Panel(p)) if *p == id => Some((other.clone(), true)),
            _ => None,
        };
        match other {
            Some((other, removed_second)) => {
                // Next to where the removed panel was: the first panel of
                // the side after it, or the last of the side before it.
                let next = if removed_second {
                    other.last()
                } else {
                    other.first()
                };
                *self = other;
                Some(next)
            }
            None => first.remove(id).or_else(|| second.remove(id)),
        }
    }

    fn first(&self) -> PanelId {
        match self {
            Layout::Panel(id) => *id,
            Layout::Split { first, .. } => first.first(),
        }
    }

    fn last(&self) -> PanelId {
        match self {
            Layout::Panel(id) => *id,
            Layout::Split { second, .. } => second.last(),
        }
    }

    /// Every panel and its area, when the layout fills `area`.
    pub fn panels(&self, area: Rect) -> Vec<(PanelId, Rect)> {
        let mut panels = Vec::new();
        self.walk(area, &mut Vec::new(), &mut |node, area, _| {
            if let Layout::Panel(id) = node {
                panels.push((*id, area));
            }
        });
        panels
    }

    /// Where each split's sides meet, when the layout fills `area`.
    pub fn handles(&self, area: Rect) -> Vec<Handle> {
        let mut handles = Vec::new();
        self.walk(area, &mut Vec::new(), &mut |node, area, path| {
            if let Layout::Split { axis, ratio, .. } = node {
                let (first, second) = divide(area, *axis, *ratio);
                let rect = match axis {
                    Axis::Horizontal => Rect {
                        x: first.right(),
                        width: 1,
                        ..area
                    },
                    Axis::Vertical => Rect {
                        y: second.y,
                        height: 1,
                        ..area
                    },
                };
                handles.push(Handle {
                    path: path.to_vec(),
                    axis: *axis,
                    rect,
                });
            }
        });
        handles
    }

    /// Visits every node, with its area and path, parents first.
    fn walk(
        &self,
        area: Rect,
        path: &mut Vec<bool>,
        visit: &mut impl FnMut(&Layout, Rect, &[bool]),
    ) {
        visit(self, area, path);
        if let Layout::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            let (a, b) = divide(area, *axis, *ratio);
            path.push(false);
            first.walk(a, path, visit);
            path.pop();
            path.push(true);
            second.walk(b, path, visit);
            path.pop();
        }
    }

    /// Moves the split at `path`, filling `area`, so its sides meet at
    /// (`x`, `y`): the divider to column `x`, or the lower side's header to
    /// row `y`. Each side keeps its minimum size where there's room.
    pub fn drag(&mut self, path: &[bool], area: Rect, x: u32, y: u32) {
        let Layout::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };
        if let Some((&side, rest)) = path.split_first() {
            let (a, b) = divide(area, *axis, *ratio);
            let (node, area) = if side { (second, b) } else { (first, a) };
            return node.drag(rest, area, x, y);
        }
        let (room, wanted, min) = match axis {
            Axis::Horizontal => (
                area.width.saturating_sub(1),
                x.saturating_sub(area.x),
                MIN_WIDTH,
            ),
            Axis::Vertical => (area.height, y.saturating_sub(area.y), MIN_HEIGHT),
        };
        if room < 2 {
            return;
        }
        let min = min.min(room / 2).max(1);
        let size = wanted.clamp(min, room - min);
        *ratio = size as f32 / room as f32;
    }

    /// The panel next to panel `from` in `direction`, when the layout fills
    /// `area`: of those that border it there, the one alongside it most.
    pub fn neighbor(&self, area: Rect, from: PanelId, direction: Direction) -> Option<PanelId> {
        let panels = self.panels(area);
        let (_, from) = *panels.iter().find(|(id, _)| *id == from)?;
        let overlap = |a: (u32, u32), b: (u32, u32)| a.1.min(b.1).saturating_sub(a.0.max(b.0));
        panels
            .iter()
            .filter_map(|&(id, rect)| {
                let (gap, alongside) = match direction {
                    Direction::Left if rect.right() <= from.x => (
                        from.x - rect.right(),
                        overlap((rect.y, rect.bottom()), (from.y, from.bottom())),
                    ),
                    Direction::Right if rect.x >= from.right() => (
                        rect.x - from.right(),
                        overlap((rect.y, rect.bottom()), (from.y, from.bottom())),
                    ),
                    Direction::Up if rect.bottom() <= from.y => (
                        from.y - rect.bottom(),
                        overlap((rect.x, rect.right()), (from.x, from.right())),
                    ),
                    Direction::Down if rect.y >= from.bottom() => (
                        rect.y - from.bottom(),
                        overlap((rect.x, rect.right()), (from.x, from.right())),
                    ),
                    _ => return None,
                };
                (alongside > 0).then_some((id, gap, alongside))
            })
            .min_by_key(|&(_, gap, alongside)| (gap, std::cmp::Reverse(alongside)))
            .map(|(id, _, _)| id)
    }
}

/// The two sides of a split of `area`. Side by side, a column between them
/// is left for the divider.
fn divide(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let room = area.width.saturating_sub(1);
            let first = ((room as f32 * ratio).round() as u32).min(room);
            (
                Rect {
                    width: first,
                    ..area
                },
                Rect {
                    x: area.x + first + 1,
                    width: room - first,
                    ..area
                },
            )
        }
        Axis::Vertical => {
            let room = area.height;
            let first = ((room as f32 * ratio).round() as u32).min(room);
            (
                Rect {
                    height: first,
                    ..area
                },
                Rect {
                    y: area.y + first,
                    height: room - first,
                    ..area
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 10,
        y: 0,
        width: 81,
        height: 30,
    };

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// 1 | 2 side by side, with 3 below 2.
    fn three() -> Layout {
        let mut layout = Layout::Panel(1);
        assert!(layout.split(1, 2, Axis::Horizontal));
        assert!(layout.split(2, 3, Axis::Vertical));
        assert!(!layout.split(9, 4, Axis::Vertical));
        layout
    }

    #[test]
    fn splits_divide_the_area() {
        assert_eq!(Layout::Panel(1).panels(AREA), [(1, AREA)]);
        assert_eq!(
            three().panels(AREA),
            [
                (1, rect(10, 0, 40, 30)),
                (2, rect(51, 0, 40, 15)),
                (3, rect(51, 15, 40, 15)),
            ]
        );
        assert_eq!(
            three().handles(AREA),
            [
                Handle {
                    path: vec![],
                    axis: Axis::Horizontal,
                    rect: rect(50, 0, 1, 30),
                },
                Handle {
                    path: vec![true],
                    axis: Axis::Vertical,
                    rect: rect(51, 15, 40, 1),
                },
            ]
        );
    }

    #[test]
    fn removing_a_panel_gives_its_room_to_its_neighbor() {
        let mut layout = three();
        assert_eq!(layout.remove(9), None);
        assert_eq!(layout.remove(2), Some(3));
        assert_eq!(
            layout.panels(AREA),
            [(1, rect(10, 0, 40, 30)), (3, rect(51, 0, 40, 30))]
        );
        assert_eq!(layout.remove(3), Some(1));
        assert_eq!(layout, Layout::Panel(1));
        assert_eq!(layout.remove(1), None, "the last panel stays");

        // Removing the first side of a split goes to the second's first
        // panel.
        let mut layout = three();
        assert_eq!(layout.remove(1), Some(2));
    }

    #[test]
    fn dragging_moves_a_split_within_limits() {
        let mut layout = three();
        layout.drag(&[], AREA, 30, 0);
        assert_eq!(layout.panels(AREA)[0].1, rect(10, 0, 20, 30));
        layout.drag(&[], AREA, 0, 0);
        assert_eq!(layout.panels(AREA)[0].1.width, MIN_WIDTH, "clamped");
        layout.drag(&[], AREA, 200, 0);
        assert_eq!(layout.panels(AREA)[1].1.width, MIN_WIDTH, "clamped");

        // The header of the lower panel goes where it's dragged.
        layout.drag(&[true], AREA, 70, 6);
        let panels = layout.panels(AREA);
        assert_eq!(panels[1].1.height, 6);
        assert_eq!(panels[2].1, rect(71, 6, 20, 24));
        layout.drag(&[true], AREA, 70, 29);
        assert_eq!(layout.panels(AREA)[2].1.height, MIN_HEIGHT);
    }

    #[test]
    fn dropping_a_panel_near_an_edge_puts_it_beside() {
        use Direction::*;
        let layout = three();
        // Near 3's left edge, and near its bottom.
        let drop = layout.drop_at(AREA, 1, 55, 22).unwrap();
        assert_eq!((drop.target, drop.side), (3, Some(Left)));
        assert_eq!(drop.rect, rect(51, 15, 20, 15));
        let drop = layout.drop_at(AREA, 1, 70, 28).unwrap();
        assert_eq!((drop.target, drop.side), (3, Some(Down)));
        assert_eq!(drop.rect, rect(51, 23, 40, 7));

        let mut moved = layout.clone();
        assert!(moved.drop_panel(1, drop));
        assert_eq!(
            moved.panels(AREA),
            [
                (2, rect(10, 0, 81, 15)),
                (3, rect(10, 15, 81, 8)),
                (1, rect(10, 23, 81, 7)),
            ]
        );

        // 2 over 1, both left of 3.
        let mut moved = layout.clone();
        let drop = layout.drop_at(AREA, 2, 30, 1).unwrap();
        assert_eq!((drop.target, drop.side), (1, Some(Up)));
        assert!(moved.drop_panel(2, drop));
        assert_eq!(
            moved.panels(AREA),
            [
                (2, rect(10, 0, 40, 15)),
                (1, rect(10, 15, 40, 15)),
                (3, rect(51, 0, 40, 30)),
            ]
        );
    }

    #[test]
    fn dropping_a_panel_in_the_middle_swaps() {
        let layout = three();
        let drop = layout.drop_at(AREA, 3, 30, 15).unwrap();
        assert_eq!(
            drop,
            Drop {
                target: 1,
                side: None,
                rect: rect(10, 0, 40, 30)
            }
        );
        let mut swapped = layout.clone();
        assert!(swapped.drop_panel(3, drop));
        assert_eq!(
            swapped.panels(AREA),
            [
                (3, rect(10, 0, 40, 30)),
                (2, rect(51, 0, 40, 15)),
                (1, rect(51, 15, 40, 15)),
            ]
        );

        // Not on itself, the divider, or a panel that's gone.
        assert_eq!(layout.drop_at(AREA, 1, 30, 15), None);
        assert_eq!(layout.drop_at(AREA, 1, 50, 15), None);
        let mut same = layout.clone();
        assert!(!same.drop_panel(9, drop));
        assert!(!same.drop_panel(1, drop));
        assert_eq!(same, layout);
    }

    #[test]
    fn a_panel_too_small_to_split_can_only_be_swapped() {
        let narrow = Rect { width: 60, ..AREA };
        let layout = three();
        // 3 is 29 wide, even without 2: too narrow to split side by side.
        let drop = layout.drop_at(narrow, 2, 45, 22).unwrap();
        assert_eq!((drop.target, drop.side), (3, None));
        // Without 2, 3 is 30 high, and splits.
        let drop = layout.drop_at(narrow, 2, 45, 29).unwrap();
        assert_eq!((drop.target, drop.side), (3, Some(Direction::Down)));
    }

    #[test]
    fn neighbors_are_found_by_direction() {
        let layout = three();
        use Direction::*;
        assert_eq!(layout.neighbor(AREA, 1, Right), Some(2));
        assert_eq!(layout.neighbor(AREA, 1, Left), None);
        assert_eq!(layout.neighbor(AREA, 3, Left), Some(1));
        assert_eq!(layout.neighbor(AREA, 3, Up), Some(2));
        assert_eq!(layout.neighbor(AREA, 2, Down), Some(3));
        assert_eq!(layout.neighbor(AREA, 2, Up), None);
    }
}
