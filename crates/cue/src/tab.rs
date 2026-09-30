//! Tabs: each is a whole layout of panels, and one is on screen at a time.
//!
//! Tabs are a level above panels, as tmux's windows are above its panes:
//! switching tabs switches the whole layout right of the file tree. A tab's
//! panels keep what they show while it's off screen, and terminals in them
//! keep running.
//!
//! With more than one tab, a bar across the top of the panels lists them,
//! numbered, by the name they were given or else by what their active
//! panel shows, then a `+` for a new one.

use opentui::{Attributes, Buffer};

use crate::editor::{STATUS_BG, STATUS_DIM, STATUS_FG};
use crate::layout::{Layout, PanelId, Rect};
use crate::panel::Panel;

/// Names a tab for as long as it's open.
pub type TabId = u32;

/// Names in the bar are cut to this many characters, or fewer if the tabs
/// don't fit.
const MAX_NAME: usize = 24;
/// What the bar calls a tab whose active panel is empty.
const EMPTY: &str = "Empty";
/// The button after the tabs that opens a new one.
const NEW: &str = " + ";

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

    /// Lays its panels out to fill `area`.
    pub fn set_area(&mut self, area: Rect) {
        for (id, rect) in self.layout.panels(area) {
            if let Some(panel) = self.panel_mut(id) {
                panel.set_area(rect);
            }
        }
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
    let label = |index: usize, max: usize| format!(" {} {} ", index + 1, cut(&titles[index], max));
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
    for (item, rect, text) in bar(tabs, area) {
        let (fg, bg, attributes) = match item {
            BarItem::Tab(index) if index == current => {
                (STATUS_FG, Some(STATUS_BG), Attributes::BOLD)
            }
            _ => (STATUS_DIM, None, Attributes::NONE),
        };
        frame.with_clip(rect.x, rect.y, rect.width, rect.height, || {
            frame.draw_text(&text, rect.x, rect.y, fg, bg, attributes)
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
