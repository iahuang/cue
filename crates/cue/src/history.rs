//! Groups native undo snapshots into user-visible undo steps.
//!
//! The native edit buffer snapshots before every edit, so typing a word would
//! take one undo per character. Each edit reports how many snapshots it
//! recorded; this module decides which consecutive edits form one group, so
//! undo steps through words and deletions the way editors usually do.
//!
//! It also tracks the save point, so "modified" means "differs from the saved
//! state in history", not "was edited since opening".

/// What an edit was, for deciding whether it continues the previous group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typing one character.
    Type(char),
    /// Backspace or Delete of one character.
    Delete,
    /// Anything else (newline, paste, cut, ...): always its own group.
    Other,
}

#[derive(Debug, Default)]
pub struct History {
    /// Snapshot counts of each group, oldest first.
    undo: Vec<u32>,
    redo: Vec<u32>,
    /// The kind of the last edit, while the next edit may still join its group.
    open: Option<EditKind>,
    /// Undo depth of the saved state; `None` when it is no longer reachable.
    saved: Option<usize>,
    /// The last group took the file's text from disk, and nothing has
    /// been recorded, undone, or redone since: the next reload joins it.
    reloaded: bool,
}

impl History {
    pub fn new() -> History {
        History {
            saved: Some(0),
            ..History::default()
        }
    }

    /// Records an edit that took `steps` native snapshots.
    pub fn record(&mut self, kind: EditKind, steps: u32) {
        if steps == 0 {
            return;
        }
        self.reloaded = false;
        if !self.redo.is_empty() {
            // The native buffer drops its redo history on a new edit too.
            if self.saved.is_some_and(|saved| saved > self.undo.len()) {
                self.saved = None;
            }
            self.redo.clear();
        }
        match (self.open, self.undo.last_mut()) {
            (Some(open), Some(last)) if continues(open, kind) => {
                *last += steps;
                if self.saved == Some(self.undo.len()) {
                    // Extending the saved group changes the saved state.
                    self.saved = None;
                }
            }
            _ => self.undo.push(steps),
        }
        self.open = Some(kind);
    }

    /// Ends the current group, e.g. after the cursor moves.
    pub fn break_group(&mut self) {
        self.open = None;
    }

    /// The number of snapshots to undo for the next group, moving it to redo.
    pub fn undo(&mut self) -> Option<u32> {
        self.open = None;
        self.reloaded = false;
        let steps = self.undo.pop()?;
        self.redo.push(steps);
        Some(steps)
    }

    pub fn redo(&mut self) -> Option<u32> {
        self.open = None;
        self.reloaded = false;
        let steps = self.redo.pop()?;
        self.undo.push(steps);
        Some(steps)
    }

    /// The disk contents are unknown, so no undo state is known saved.
    pub fn mark_unsaved(&mut self) {
        self.saved = None;
        self.open = None;
    }

    pub fn mark_saved(&mut self) {
        self.saved = Some(self.undo.len());
        self.open = None;
    }

    /// Records taking the file's text from disk, which took `steps`
    /// snapshots, as saved. With `join`, reloads in a row, with no edit,
    /// undo, or redo between, are one group, so a file that keeps changing
    /// doesn't bury the edits before it.
    pub fn record_reload(&mut self, steps: u32, join: bool) {
        let joins = join && self.reloaded;
        match self.undo.last_mut() {
            Some(last) if joins => *last += steps,
            _ => {
                self.break_group();
                self.record(EditKind::Other, steps);
            }
        }
        self.mark_saved();
        self.reloaded = join && (steps > 0 || joins);
    }

    pub fn is_modified(&self) -> bool {
        self.saved != Some(self.undo.len())
    }
}

fn continues(open: EditKind, next: EditKind) -> bool {
    match (open, next) {
        // A word and the whitespace after it form one group; the next word
        // starts a new one.
        (EditKind::Type(prev), EditKind::Type(next)) => {
            !prev.is_whitespace() || next.is_whitespace()
        }
        (EditKind::Delete, EditKind::Delete) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(h: &mut History, s: &str) {
        for c in s.chars() {
            h.record(EditKind::Type(c), 1);
        }
    }

    #[test]
    fn typing_groups_by_word() {
        let mut h = History::new();
        type_str(&mut h, "hello world");
        assert_eq!(h.undo(), Some(5), "world");
        assert_eq!(h.undo(), Some(6), "hello + space");
        assert_eq!(h.undo(), None);
        assert_eq!(h.redo(), Some(6));
        assert_eq!(h.redo(), Some(5));
        assert_eq!(h.redo(), None);
    }

    #[test]
    fn breaks_and_kinds_split_groups() {
        let mut h = History::new();
        type_str(&mut h, "ab");
        h.break_group();
        type_str(&mut h, "cd");
        h.record(EditKind::Delete, 1);
        h.record(EditKind::Delete, 2);
        h.record(EditKind::Other, 1);
        h.record(EditKind::Other, 1);
        let groups: Vec<_> = std::iter::from_fn(|| h.undo()).collect();
        assert_eq!(groups, [1, 1, 3, 2, 2]);
    }

    #[test]
    fn no_op_edits_record_nothing() {
        let mut h = History::new();
        h.record(EditKind::Delete, 0);
        assert!(!h.is_modified());
        assert_eq!(h.undo(), None);
    }

    #[test]
    fn modified_tracks_the_save_point() {
        let mut h = History::new();
        type_str(&mut h, "ab");
        assert!(h.is_modified());
        h.mark_saved();
        assert!(!h.is_modified());

        // Typing after a save starts a new group, so undo returns to it.
        type_str(&mut h, "c");
        assert!(h.is_modified());
        h.undo();
        assert!(!h.is_modified());
        h.undo();
        assert!(h.is_modified(), "before the save point");
        h.redo();
        assert!(!h.is_modified());
    }

    #[test]
    fn save_point_lost_when_its_redo_branch_is_dropped() {
        let mut h = History::new();
        type_str(&mut h, "a b");
        h.mark_saved();
        h.undo();
        h.undo();
        type_str(&mut h, "x");
        assert!(h.is_modified());
        h.undo();
        assert!(h.is_modified(), "the saved state is unreachable now");
    }

    #[test]
    fn reloads_in_a_row_are_one_group() {
        let mut h = History::new();
        type_str(&mut h, "a");
        h.mark_saved();
        h.record_reload(2, true);
        h.break_group();
        h.record_reload(2, true);
        h.record_reload(0, true);
        h.record_reload(1, true);
        assert!(!h.is_modified());
        assert_eq!(h.undo(), Some(5), "all three reloads");
        assert!(h.is_modified());
        h.redo();
        assert!(!h.is_modified());

        // An edit, undo, or redo between starts a new one.
        h.record_reload(1, true);
        assert_eq!(h.undo(), Some(1));
        h.redo();
        h.record_reload(1, true);
        type_str(&mut h, "b");
        h.undo();
        h.record_reload(1, true);
        let groups: Vec<_> = std::iter::from_fn(|| h.undo()).collect();
        assert_eq!(groups, vec![1, 1, 1, 5, 1]);

        // A revert, not joined, isn't joined by the next reload either.
        let mut h = History::new();
        h.record_reload(1, true);
        h.record_reload(1, false);
        h.record_reload(1, true);
        let groups: Vec<_> = std::iter::from_fn(|| h.undo()).collect();
        assert_eq!(groups, vec![1, 1, 1]);
    }
}
