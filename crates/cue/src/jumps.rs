//! Where the keyboard was, across every panel, to go back and forward
//! through with Ctrl+- and Ctrl+=.
//!
//! Each panel's history keeps what that panel showed (see
//! [`crate::panel`]). This list keeps the places in all of them in the
//! order they were left: what the active panel's history notes, and the
//! panel the keyboard was in when it moves to another. Going back goes to
//! the panel again, in whichever tab, as well as to the place in it.

use crate::layout::PanelId;
use crate::panel::Visit;

/// How far back the list goes.
const JUMPS: usize = 100;

/// A place in a panel.
#[derive(Debug, Clone)]
pub struct Jump {
    pub panel: PanelId,
    pub visit: Visit,
}

impl Jump {
    /// Whether it's the same place as `other`, in the same panel.
    fn is(&self, other: &Jump) -> bool {
        self.panel == other.panel && self.visit.is(&other.visit)
    }
}

/// The places left, most recent last, and those gone back from, most
/// recent last.
#[derive(Debug, Default)]
pub struct Jumps {
    back: Vec<Jump>,
    forward: Vec<Jump>,
    /// The panel the keyboard was in when last caught up with (see
    /// [`Jumps::follow`]).
    at: Option<PanelId>,
}

impl Jumps {
    /// Catches up with the keyboard being in panel `active`, as it is
    /// now at `here`. If it was in another panel before, that panel's
    /// place is left: its first place `left` since (see
    /// [`crate::panel::Panel::take_left`]), or else what it shows, from
    /// `before`. Otherwise, what the panel itself `left` is.
    pub fn follow(
        &mut self,
        active: PanelId,
        here: Option<&Visit>,
        left: Vec<Jump>,
        before: Option<Visit>,
    ) {
        let moved = self.at.is_some_and(|at| at != active);
        let from: Vec<Jump> = match (moved, self.at) {
            (true, Some(at)) => left
                .into_iter()
                .find(|jump| jump.panel == at)
                .or_else(|| before.map(|visit| Jump { panel: at, visit }))
                .into_iter()
                .collect(),
            _ => left
                .into_iter()
                .filter(|jump| jump.panel == active)
                .collect(),
        };
        self.at = Some(active);
        let here = here.map(|visit| Jump {
            panel: active,
            visit: visit.clone(),
        });
        for jump in from {
            if !here.as_ref().is_some_and(|here| here.is(&jump)) {
                self.note(jump);
            }
        }
    }

    /// The panel the keyboard was in when last caught up with.
    pub fn at(&self) -> Option<PanelId> {
        self.at
    }

    /// Takes the keyboard being in panel `active` as it is, without
    /// leaving anywhere, as after going back.
    pub fn settle(&mut self, active: PanelId) {
        self.at = Some(active);
    }

    /// Notes `jump` as the place to go back to. Going somewhere new,
    /// there's no going forward.
    fn note(&mut self, jump: Jump) {
        self.forward.clear();
        self.back.retain(|old| !old.is(&jump));
        self.back.push(jump);
        if self.back.len() > JUMPS {
            self.back.remove(0);
        }
    }

    /// Goes back (or forward, if not `back`): takes the last place there
    /// that `usable` accepts and isn't `here`, and puts `here` on the
    /// other side, for going the other way. Places passed over, which are
    /// gone, are dropped.
    pub fn step(
        &mut self,
        back: bool,
        here: Option<Jump>,
        usable: impl Fn(&Jump) -> bool,
    ) -> Option<Jump> {
        let (from, to) = match back {
            true => (&mut self.back, &mut self.forward),
            false => (&mut self.forward, &mut self.back),
        };
        let jump = loop {
            let jump = from.pop()?;
            let is_here = here.as_ref().is_some_and(|here| here.is(&jump));
            if !is_here && usable(&jump) {
                break jump;
            }
        };
        to.extend(here);
        Some(jump)
    }

    /// Drops what panel `panel` showed at `visit` from the places to go
    /// back to, as when it was closed for good.
    pub fn drop_visit(&mut self, panel: PanelId, visit: &Visit) {
        self.back
            .retain(|old| !(old.panel == panel && old.visit.shows_same(visit)));
    }

    /// The places to go back to, most recent last, and forward to.
    pub fn lists(&self) -> (&[Jump], &[Jump]) {
        (&self.back, &self.forward)
    }

    /// Takes lists kept from before, as [`Jumps::lists`] gave them, with
    /// nowhere left yet.
    pub fn set(&mut self, back: Vec<Jump>, forward: Vec<Jump>) {
        *self = Jumps {
            back,
            forward,
            at: None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn image(name: &str) -> Visit {
        Visit::Image(PathBuf::from(name))
    }

    fn jump(panel: PanelId, name: &str) -> Jump {
        Jump {
            panel,
            visit: image(name),
        }
    }

    fn names(list: &[Jump]) -> Vec<(PanelId, String)> {
        list.iter()
            .map(|jump| match &jump.visit {
                Visit::Image(path) => (jump.panel, path.display().to_string()),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn moving_to_another_panel_leaves_where_the_keyboard_was() {
        let mut jumps = Jumps::default();
        jumps.follow(1, Some(&image("a")), vec![], Some(image("a")));
        assert!(jumps.lists().0.is_empty(), "nowhere to have come from");
        // Panel 2 replacing what it showed as the keyboard got there
        // doesn't count: the keyboard was never there.
        jumps.follow(2, Some(&image("c")), vec![jump(2, "b")], Some(image("a")));
        assert_eq!(names(jumps.lists().0), [(1, "a".into())]);
        jumps.follow(2, Some(&image("d")), vec![jump(2, "c")], None);
        assert_eq!(names(jumps.lists().0), [(1, "a".into()), (2, "c".into())]);
    }

    #[test]
    fn a_panel_left_first_leaves_what_it_showed_then() {
        let mut jumps = Jumps::default();
        jumps.settle(1);
        jumps.follow(2, Some(&image("c")), vec![jump(1, "a")], Some(image("b")));
        assert_eq!(names(jumps.lists().0), [(1, "a".into())]);
    }

    #[test]
    fn going_back_and_forward_swaps_sides() {
        let mut jumps = Jumps::default();
        jumps.settle(1);
        jumps.follow(1, Some(&image("c")), vec![jump(1, "a"), jump(1, "b")], None);
        let usable = |_: &Jump| true;
        let back = jumps.step(true, Some(jump(1, "c")), usable).unwrap();
        assert_eq!(names(&[back]), [(1, "b".into())]);
        assert_eq!(names(jumps.lists().1), [(1, "c".into())]);
        let forward = jumps.step(false, Some(jump(1, "b")), usable).unwrap();
        assert_eq!(names(&[forward]), [(1, "c".into())]);
        assert_eq!(names(jumps.lists().0), [(1, "a".into()), (1, "b".into())]);
    }

    #[test]
    fn places_gone_are_skipped_and_dropped() {
        let mut jumps = Jumps::default();
        jumps.settle(1);
        jumps.follow(1, None, vec![jump(1, "a"), jump(1, "gone")], None);
        let usable = |jump: &Jump| !matches!(&jump.visit, Visit::Image(p) if p.ends_with("gone"));
        let back = jumps.step(true, None, usable).unwrap();
        assert_eq!(names(&[back]), [(1, "a".into())]);
        assert!(jumps.lists().0.is_empty());
    }

    #[test]
    fn the_same_place_is_kept_once_per_panel() {
        let mut jumps = Jumps::default();
        jumps.settle(1);
        jumps.follow(2, None, vec![], Some(image("a")));
        jumps.follow(1, None, vec![], Some(image("b")));
        jumps.follow(2, None, vec![], Some(image("a")));
        assert_eq!(names(jumps.lists().0), [(2, "b".into()), (1, "a".into())]);
    }
}
