//! Tabs: each is a whole layout of panels, and one is on screen at a time.
//!
//! Tabs are a level above panels, as tmux's windows are above its panes:
//! switching tabs switches the whole layout right of the file tree. A tab's
//! panels keep what they show while it's off screen, and terminals in them
//! keep running.
//!
//! Zoomed, a tab's active panel fills it, framed, and the others are
//! hidden until it's zoomed out, as tmux's zoomed panes and tiling window
//! managers' monocle layouts are. Moving to another panel shows that one.
//!
//! With more than one tab, a bar across the top of the panels lists them,
//! numbered, by the name they were given or else by what their active
//! panel shows, then a `+` for a new one. A dot after a tab's name says a
//! terminal in it is waiting for the user.

use opentui::{Attributes, Buffer};

use crate::layout::{self, Handle, Layout, PanelId, Rect};
use crate::panel::Panel;
use crate::theme::{self, Hue};

/// Names a tab for as long as it's open.
pub type TabId = u32;

/// Names in the bar are cut to this many characters, or fewer if the tabs
/// don't fit.
const MAX_NAME: usize = 24;
/// What the bar calls a tab whose active panel is empty.
const EMPTY: &str = "Empty";
/// The button after the tabs that opens a new one.
const NEW: &str = " + ";
/// After the name of a tab with a terminal waiting for the user.
const WAITING: &str = "●";

pub struct Tab {
    pub id: TabId,
    pub layout: Layout,
    /// The panels in the layout, in no particular order. Never empty.
    pub panels: Vec<Panel>,
    /// The panel keys go to while the tab is on screen, unless the file
    /// tree has focus.
    pub active: PanelId,
    /// The panel active before, which may have closed since.
    pub previous: Option<PanelId>,
    /// The name it was given, if it was renamed.
    pub name: Option<String>,
    /// The active panel fills the tab, and the others are hidden.
    pub zoomed: bool,
}

impl Tab {
    /// A tab with one empty panel, numbered `panel`.
    pub fn new(id: TabId, panel: PanelId) -> Tab {
        Tab {
            id,
            layout: Layout::Panel(panel),
            panels: vec![Panel::new(panel)],
            active: panel,
            previous: None,
            name: None,
            zoomed: false,
        }
    }

    pub fn panel_mut(&mut self, id: PanelId) -> Option<&mut Panel> {
        self.panels.iter_mut().find(|panel| panel.id == id)
    }

    pub fn active_panel(&self) -> &Panel {
        self.panels
            .iter()
            .find(|panel| panel.id == self.active)
            .expect("the active panel is open")
    }

    pub fn active_panel_mut(&mut self) -> &mut Panel {
        let active = self.active;
        self.panel_mut(active).expect("the active panel is open")
    }

    /// What the bar calls it: the name it was given, or what its active
    /// panel shows.
    pub fn title(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.active_panel().title())
            .unwrap_or_else(|| EMPTY.to_string())
    }

    /// Whether a terminal in one of its panels is waiting for the user.
    pub fn waiting(&self) -> bool {
        self.panels.iter().any(|panel| {
            panel
                .terminal()
                .is_some_and(|terminal| terminal.borrow().notification().is_some())
        })
    }

    /// Lays its panels out to fill `area`. Zoomed, the active panel fills
    /// it instead, and the others keep their places for when it's zoomed
    /// out. A lone panel isn't zoomed.
    pub fn set_area(&mut self, area: Rect) {
        if self.panels.len() == 1 {
            self.zoomed = false;
        }
        for (id, rect) in self.layout.panels(area) {
            if let Some(panel) = self.panel_mut(id) {
                panel.set_area(rect);
            }
        }
        // As a panel closes, the active one may be gone until another is.
        let (active, zoomed) = (self.active, self.zoomed);
        if let Some(panel) = self.panel_mut(active).filter(|_| zoomed) {
            panel.set_area(zoomed_area(area));
        }
    }

    /// The panels on screen when the tab fills `area`, and where: zoomed,
    /// only the active one.
    pub fn panel_areas(&self, area: Rect) -> Vec<(PanelId, Rect)> {
        match self.zoomed {
            true => vec![(self.active, zoomed_area(area))],
            false => self.layout.panels(area),
        }
    }

    /// The handles between panels on screen when the tab fills `area`:
    /// none while zoomed.
    pub fn handles(&self, area: Rect) -> Vec<Handle> {
        match self.zoomed {
            true => Vec::new(),
            false => self.layout.handles(area),
        }
    }
}

/// Where a zoomed panel goes in `area`: inside a frame a cell wide, if
/// that leaves room for a panel.
pub fn zoomed_area(area: Rect) -> Rect {
    if area.width < layout::MIN_WIDTH + 2 || area.height < layout::MIN_HEIGHT + 2 {
        return area;
    }
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width - 2,
        height: area.height - 2,
    }
}

/// Draws the frame around a zoomed panel in `area`, unless there's no
/// room for one (see [`zoomed_area`]).
pub fn draw_zoom_frame(frame: &Buffer, area: Rect) {
    if zoomed_area(area) == area {
        return;
    }
    let color = theme::colors().accent;
    let draw = |text: &str, x, y| frame.draw_text(text, x, y, color, None, Attributes::NONE);
    let rule = "─".repeat(area.width as usize - 2);
    let bottom = area.y + area.height - 1;
    draw(&format!("╭{rule}╮"), area.x, area.y);
    draw(&format!("╰{rule}╯"), area.x, bottom);
    for y in area.y + 1..bottom {
        draw("│", area.x, y);
        draw("│", area.x + area.width - 1, y);
    }
}

/// Every panel in `tabs`, on screen or not.
pub fn all_panels(tabs: &[Tab]) -> impl Iterator<Item = &Panel> {
    tabs.iter().flat_map(|tab| &tab.panels)
}

pub fn all_panels_mut(tabs: &mut [Tab]) -> impl Iterator<Item = &mut Panel> {
    tabs.iter_mut().flat_map(|tab| &mut tab.panels)
}

/// Something in the tab bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarItem {
    /// The tab at this index.
    Tab(usize),
    /// The button that opens a new tab.
    New,
}

/// The tab bar's items, where they are when it fills `area` (a row), and
/// their text. Names are cut shorter, all alike, until the tabs and the
/// button fit; what still doesn't is left off at the right.
pub fn bar(tabs: &[Tab], area: Rect) -> Vec<(BarItem, Rect, String)> {
    let titles: Vec<String> = tabs.iter().map(Tab::title).collect();
    let label = |index: usize, max: usize| {
        let title = cut(&titles[index], max);
        match tabs[index].waiting() {
            true => format!(" {} {title} {WAITING} ", index + 1),
            false => format!(" {} {title} ", index + 1),
        }
    };
    let width = |max| {
        (0..titles.len())
            .map(|index| label(index, max).chars().count())
            .sum::<usize>()
    };
    let room = (area.width as usize).saturating_sub(NEW.chars().count());
    let mut max = MAX_NAME;
    while max > 1 && width(max) > room {
        max -= 1;
    }
    let items = (0..titles.len())
        .map(|index| (BarItem::Tab(index), label(index, max)))
        .chain([(BarItem::New, NEW.to_string())]);
    let right = area.x + area.width;
    let mut x = area.x;
    let mut placed = Vec::new();
    for (item, text) in items {
        if x >= right {
            break;
        }
        let width = (text.chars().count() as u32).min(right - x);
        let rect = Rect {
            x,
            y: area.y,
            width,
            height: 1,
        };
        placed.push((item, rect, text));
        x += width;
    }
    placed
}

/// Draws the tab bar across `area`, with the tab at `current` on screen.
pub fn draw_bar(frame: &Buffer, tabs: &[Tab], current: usize, area: Rect) {
    let colors = theme::colors();
    for (item, rect, text) in bar(tabs, area) {
        let (fg, bg, attributes) = match item {
            BarItem::Tab(index) if index == current => {
                (colors.text, Some(colors.surface), Attributes::BOLD)
            }
            _ => (colors.muted, None, Attributes::NONE),
        };
        frame.with_clip(rect.x, rect.y, rect.width, rect.height, || {
            frame.draw_text(&text, rect.x, rect.y, fg, bg, attributes);
            if let Some(at) = text.strip_suffix(&format!("{WAITING} ")) {
                let x = rect.x + at.chars().count() as u32;
                let dot = colors.hue(Hue::Yellow);
                frame.draw_text(WAITING, x, rect.y, dot, bg, Attributes::NONE);
            }
        });
    }
}

/// The first `max` characters of `s`, marked with a trailing ellipsis if cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(names: &[&str]) -> Vec<Tab> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| Tab {
                name: Some(name.to_string()),
                ..Tab::new(i as TabId, i as PanelId)
            })
            .collect()
    }

    fn row(width: u32) -> Rect {
        Rect {
            x: 10,
            y: 0,
            width,
            height: 1,
        }
    }

    #[test]
    fn tabs_are_numbered_and_named_then_the_new_button() {
        let tabs = tabs(&["main.rs", "zsh"]);
        let items = bar(&tabs, row(80));
        let texts: Vec<&str> = items.iter().map(|(_, _, text)| text.as_str()).collect();
        assert_eq!(texts, [" 1 main.rs ", " 2 zsh ", " + "]);
        let rects: Vec<(u32, u32)> = items.iter().map(|(_, r, _)| (r.x, r.width)).collect();
        assert_eq!(rects, [(10, 11), (21, 7), (28, 3)]);
        assert_eq!(items[2].0, BarItem::New);
    }

    #[test]
    fn names_shrink_to_fit_and_the_rest_is_cut_off() {
        let tabs = tabs(&["a-long-file-name.rs", "another-long-one.rs"]);
        let items = bar(&tabs, row(30));
        let texts: Vec<&str> = items.iter().map(|(_, _, text)| text.as_str()).collect();
        assert_eq!(texts, [" 1 a-long-f… ", " 2 another-… ", " + "]);
        let total: u32 = items.iter().map(|(_, r, _)| r.width).sum();
        assert!(total <= 30);

        // Too narrow for even one character each: the right is left off.
        let tabs = self::tabs(&["a", "b", "c", "d"]);
        let items = bar(&tabs, row(12));
        let end = items.last().map(|(_, r, _)| r.x + r.width);
        assert_eq!(end, Some(22));
        assert!(!items.iter().any(|(item, _, _)| *item == BarItem::New));
    }

    #[test]
    fn an_unnamed_tab_is_called_after_its_active_panel() {
        let tab = Tab::new(0, 0);
        assert_eq!(tab.title(), "Empty");
    }
}
