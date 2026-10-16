//! Layout mode: arranging panels from the keyboard with plain keys, so
//! that swapping, resizing, and the like need no shortcuts of their own.
//!
//! Ctrl+J goes in, and Esc, Enter, or Ctrl+J again comes out. Until then,
//! keys are the mode's, terminals' included: arrows (or h, j, k, l) move
//! the keyboard to the panel that way, with Shift they trade places with
//! what's that way (see [`Layout::trade`]), and with Alt they move its
//! edge that way. The status bar lists the rest.
//!
//! Tabs go as panels do, one size up: `[` and `]` switch to the tab that
//! way, with Shift (`{` and `}`) they trade places with it, `W` closes
//! the tab as `w` does the panel, and `1` to `9` go to that tab.
//!
//! Cut takes the active panel out of the layout, to paste over another,
//! in whichever tab; until then it's gone, so cutting is closing too, but
//! what it showed stays open. Copy keeps what the panel shows, and its
//! history, to paste over others; a terminal shows in one panel at a time,
//! so pasting one moves it. Pasting over a panel is as opening
//! something over it: its files stay open behind, and terminals keep
//! running, but its history is the pasted one's. Cut or copied, the
//! clipboard lasts after the mode is off. Undo
//! takes back resizing, swapping, evening out, flipping, zooming, and
//! moving tabs, one change at a time, back to splitting, closing, or
//! pasting.
//!
//! With the scratchpad out (see [`crate::scratch`]), the arrows resize it
//! instead, and `=` puts it back to the size it starts at. Cut, copy, and
//! paste work on it as on a panel; cutting it leaves it empty, and puts it
//! away, to paste over the tab's panels.

use opentui::{Attributes, Buffer};

use crate::input::{Key, KeyCode};
use crate::layout::{Axis, Direction, Layout};
use crate::panel::{Panel, Visit};
use crate::tab::TabId;
use crate::theme;

/// What a key does in layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Leave,
    Focus(Direction),
    Swap(Direction),
    Resize(Direction),
    Equalize,
    Flip,
    Split(Axis),
    Zoom,
    Cut,
    Copy,
    Paste,
    Close,
    NewTab,
    PreviousTab,
    NextTab,
    /// Trades the tab on screen with the one before it, or after.
    MoveTab(Direction),
    CloseTab,
    /// Goes to the tab at this index.
    GoToTab(usize),
    Undo,
}

/// How many cells a resize moves an edge.
pub const RESIZE_CELLS: u32 = 2;

/// What the status bar lists, by key.
const HINTS: &[(&str, &str)] = &[
    ("←↓↑→", "focus"),
    ("⇧", "swap"),
    ("⌥", "resize"),
    ("=", "even out"),
    ("r", "flip"),
    ("\\ |", "split"),
    ("z", "zoom"),
    ("x c v", "cut copy paste"),
    ("w", "close"),
    ("u", "undo"),
    ("esc", "done"),
    // Tabs last, the first to go when the bar's too narrow.
    ("t", "new tab"),
    ("[ ] { }", "tabs, move"),
    ("W", "close tab"),
];

/// What the status bar lists with the scratchpad out, which only resizes.
const SCRATCH_HINTS: &[(&str, &str)] = &[
    ("←↓↑→", "resize"),
    ("=", "reset size"),
    ("x c v", "cut copy paste"),
    ("t", "new tab"),
    ("[ ] { }", "tabs, move"),
    ("W", "close tab"),
    ("esc", "done"),
];

/// What `key` does, if anything. Keys with Ctrl or Super held are cue's
/// shortcuts, as outside the mode.
pub fn action(key: Key) -> Option<Action> {
    let mods = key.mods;
    if mods.ctrl || mods.sup {
        return None;
    }
    let direction = match key.code {
        KeyCode::Left => Some(Direction::Left),
        KeyCode::Down => Some(Direction::Down),
        KeyCode::Up => Some(Direction::Up),
        KeyCode::Right => Some(Direction::Right),
        // Terminals on macOS send Option+Left and Right as Alt+B and F, as
        // the editor's word movement takes them.
        KeyCode::Char('b') if mods.alt => Some(Direction::Left),
        KeyCode::Char('f') if mods.alt => Some(Direction::Right),
        KeyCode::Char(c) => match c.to_ascii_lowercase() {
            'h' => Some(Direction::Left),
            'j' => Some(Direction::Down),
            'k' => Some(Direction::Up),
            'l' => Some(Direction::Right),
            _ => None,
        },
        _ => None,
    };
    if let Some(direction) = direction {
        let shifted = mods.shift || matches!(key.code, KeyCode::Char(c) if c.is_uppercase());
        return Some(match (mods.alt, shifted) {
            (true, _) => Action::Resize(direction),
            (false, true) => Action::Swap(direction),
            (false, false) => Action::Focus(direction),
        });
    }
    if mods.alt {
        return None;
    }
    Some(match key.code {
        KeyCode::Esc | KeyCode::Enter => Action::Leave,
        KeyCode::Char('=') => Action::Equalize,
        KeyCode::Char('r') => Action::Flip,
        // Some terminals report Shift+\ as `\` with Shift, not `|`.
        KeyCode::Char('\\') if mods.shift => Action::Split(Axis::Vertical),
        KeyCode::Char('\\') => Action::Split(Axis::Horizontal),
        // As Ctrl+Shift+\ splits down; `-` looks like the line it draws.
        KeyCode::Char('|' | '-') => Action::Split(Axis::Vertical),
        KeyCode::Char('z') => Action::Zoom,
        KeyCode::Char('x') => Action::Cut,
        KeyCode::Char('c') => Action::Copy,
        KeyCode::Char('v') => Action::Paste,
        // Some terminals report Shift+W as `w` with Shift, and Shift+[ as
        // `[` with Shift, not `W` and `{`.
        KeyCode::Char('w') if mods.shift => Action::CloseTab,
        KeyCode::Char('W') => Action::CloseTab,
        KeyCode::Char('w') => Action::Close,
        KeyCode::Char('t') => Action::NewTab,
        KeyCode::Char('[') if mods.shift => Action::MoveTab(Direction::Left),
        KeyCode::Char(']') if mods.shift => Action::MoveTab(Direction::Right),
        KeyCode::Char('{') => Action::MoveTab(Direction::Left),
        KeyCode::Char('}') => Action::MoveTab(Direction::Right),
        KeyCode::Char('[') => Action::PreviousTab,
        KeyCode::Char(']') => Action::NextTab,
        KeyCode::Char(c @ '1'..='9') => Action::GoToTab(c as usize - '1' as usize),
        KeyCode::Char('u') => Action::Undo,
        _ => return None,
    })
}

/// A panel cut or copied, to paste over another.
// There's one at most.
#[allow(clippy::large_enum_variant)]
pub enum PanelClip {
    /// A panel cut, out of every tab until it's pasted. What it shows
    /// stays open meanwhile, and its terminal keeps running.
    Cut(Panel),
    /// What a panel showed when copied, and its history.
    Copy {
        visit: Visit,
        back: Vec<Visit>,
        forward: Vec<Visit>,
        title: String,
    },
}

impl PanelClip {
    /// What the status bar calls it in layout mode.
    pub fn label(&self) -> String {
        match self {
            PanelClip::Cut(panel) => {
                let title = panel.title().unwrap_or_else(|| "Empty".into());
                format!("Cut {title}")
            }
            PanelClip::Copy { title, .. } => format!("Copied {title}"),
        }
    }

    /// The panel cut, if one was.
    pub fn cut(&self) -> Option<&Panel> {
        match self {
            PanelClip::Cut(panel) => Some(panel),
            PanelClip::Copy { .. } => None,
        }
    }
}

/// What a change in layout mode changed from, to undo it.
#[derive(Debug, Clone, PartialEq)]
pub enum Before {
    /// A tab's layout, and whether it was zoomed.
    Layout(TabId, Layout, bool),
    /// The tabs' order.
    Tabs(Vec<TabId>),
}

/// Changes that join the one just before, if it was the same kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Joining {
    Resize,
    MoveTab,
}

/// Layout mode, while it's on.
#[derive(Debug, Default)]
pub struct Arranging {
    /// What was before each change that can be undone, most recent last.
    undo: Vec<Before>,
    /// The last change, if the next one of its kind joins it.
    joining: Option<Joining>,
}

impl Arranging {
    /// Notes tab `tab`'s layout before a change to it, to undo. Resizes in
    /// a row are one change.
    pub fn before(&mut self, tab: TabId, layout: &Layout, zoomed: bool, resize: bool) {
        let joining = resize.then_some(Joining::Resize);
        self.note(joining, || Before::Layout(tab, layout.clone(), zoomed));
    }

    /// Notes the tabs' `order` before moving one, to undo. Moves in a row
    /// are one change.
    pub fn before_moving_tab(&mut self, order: Vec<TabId>) {
        self.note(Some(Joining::MoveTab), || Before::Tabs(order));
    }

    fn note(&mut self, joining: Option<Joining>, before: impl FnOnce() -> Before) {
        if joining.is_none() || joining != self.joining {
            self.undo.push(before());
        }
        self.joining = joining;
    }

    /// Forgets what can be undone, as after a change to which panels
    /// there are, which undoing doesn't take back.
    pub fn forget(&mut self) {
        self.undo.clear();
        self.joining = None;
    }

    /// What was before the last change to undo.
    pub fn undo(&mut self) -> Option<Before> {
        self.joining = None;
        self.undo.pop()
    }
}

/// Draws layout mode's keys across the status bar at row `y`, `width`
/// wide, and what's cut or copied, as `clip` says (see
/// [`PanelClip::label`]), or with the `scratchpad` out, its keys.
pub fn draw_hints(frame: &Buffer, y: u32, width: u32, clip: Option<&str>, scratchpad: bool) {
    let colors = theme::colors();
    frame.fill_rect(0, y, width, 1, colors.surface);
    let label = " LAYOUT ";
    frame.draw_text(
        label,
        0,
        y,
        colors.on_accent,
        Some(colors.accent),
        Attributes::BOLD,
    );
    let mut x = label.chars().count() as u32 + 1;
    let fits = |text: &str, x: &mut u32| {
        let end = *x + text.chars().count() as u32;
        let fit = end <= width;
        if fit {
            *x = end;
        }
        fit
    };
    if let Some(clip) = clip {
        let at = x;
        if fits(clip, &mut x) {
            frame.draw_text(clip, at, y, colors.accent, None, Attributes::NONE);
            x += 2;
        }
    }
    let hints = match scratchpad {
        true => SCRATCH_HINTS,
        false => HINTS,
    };
    for (key, does) in hints {
        let (at, key) = (x, format!("{key} "));
        if !fits(&format!("{key}{does}"), &mut x) {
            break;
        }
        frame.draw_text(&key, at, y, colors.text, None, Attributes::NONE);
        let does_at = at + key.chars().count() as u32;
        frame.draw_text(does, does_at, y, colors.muted, None, Attributes::NONE);
        x += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn key(code: KeyCode, mods: Mods) -> Option<Action> {
        action(Key::new(code, mods))
    }

    #[test]
    fn arrows_and_hjkl_focus_swap_and_resize() {
        let alt = Mods {
            alt: true,
            ..Mods::NONE
        };
        use Direction::*;
        assert_eq!(key(KeyCode::Left, Mods::NONE), Some(Action::Focus(Left)));
        assert_eq!(
            key(KeyCode::Char('j'), Mods::NONE),
            Some(Action::Focus(Down))
        );
        assert_eq!(key(KeyCode::Up, Mods::SHIFT), Some(Action::Swap(Up)));
        // Shift+L may come as `L`, with or without Shift.
        assert_eq!(
            key(KeyCode::Char('L'), Mods::NONE),
            Some(Action::Swap(Right))
        );
        assert_eq!(
            key(KeyCode::Char('l'), Mods::SHIFT),
            Some(Action::Swap(Right))
        );
        assert_eq!(key(KeyCode::Right, alt), Some(Action::Resize(Right)));
        assert_eq!(key(KeyCode::Char('b'), alt), Some(Action::Resize(Left)));
        assert_eq!(key(KeyCode::Char('f'), alt), Some(Action::Resize(Right)));
        assert_eq!(key(KeyCode::Char('b'), Mods::NONE), None);
    }

    #[test]
    fn shortcuts_are_cues() {
        assert_eq!(key(KeyCode::Char('j'), Mods::CTRL), None);
        assert_eq!(key(KeyCode::Char('v'), Mods::SUPER), None);
        assert_eq!(key(KeyCode::Char('v'), Mods::NONE), Some(Action::Paste));
        assert_eq!(key(KeyCode::Char('q'), Mods::NONE), None);
        let down = Some(Action::Split(Axis::Vertical));
        assert_eq!(key(KeyCode::Char('|'), Mods::SHIFT), down);
        assert_eq!(key(KeyCode::Char('-'), Mods::NONE), down);
        assert_eq!(key(KeyCode::Char('\\'), Mods::SHIFT), down);
    }

    #[test]
    fn resizes_in_a_row_undo_together() {
        let mut arranging = Arranging::default();
        let (a, b) = (Layout::Panel(1), Layout::Panel(2));
        arranging.before(0, &a, false, true);
        arranging.before(0, &b, false, true);
        arranging.before(0, &b, true, false);
        assert_eq!(arranging.undo(), Some(Before::Layout(0, b, true)));
        assert_eq!(arranging.undo(), Some(Before::Layout(0, a, false)));
        assert_eq!(arranging.undo(), None);
    }

    #[test]
    fn tab_moves_in_a_row_undo_together() {
        let mut arranging = Arranging::default();
        arranging.before_moving_tab(vec![0, 1, 2]);
        arranging.before_moving_tab(vec![1, 0, 2]);
        arranging.before(1, &Layout::Panel(1), false, true);
        arranging.before_moving_tab(vec![1, 2, 0]);
        assert_eq!(arranging.undo(), Some(Before::Tabs(vec![1, 2, 0])));
        assert!(matches!(arranging.undo(), Some(Before::Layout(..))));
        assert_eq!(arranging.undo(), Some(Before::Tabs(vec![0, 1, 2])));
    }

    #[test]
    fn shifted_brackets_and_w_are_for_tabs() {
        let left = Some(Action::MoveTab(Direction::Left));
        assert_eq!(key(KeyCode::Char('{'), Mods::SHIFT), left);
        assert_eq!(key(KeyCode::Char('['), Mods::SHIFT), left);
        let right = Some(Action::MoveTab(Direction::Right));
        assert_eq!(key(KeyCode::Char('}'), Mods::NONE), right);
        assert_eq!(key(KeyCode::Char(']'), Mods::NONE), Some(Action::NextTab));
        assert_eq!(key(KeyCode::Char('W'), Mods::SHIFT), Some(Action::CloseTab));
        assert_eq!(key(KeyCode::Char('w'), Mods::SHIFT), Some(Action::CloseTab));
        assert_eq!(key(KeyCode::Char('w'), Mods::NONE), Some(Action::Close));
        assert_eq!(
            key(KeyCode::Char('3'), Mods::NONE),
            Some(Action::GoToTab(2))
        );
        assert_eq!(key(KeyCode::Char('0'), Mods::NONE), None);
    }
}
