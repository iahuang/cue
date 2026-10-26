//! The file browser: a panel that lists a folder, as the Finder's list
//! view does, for going through files anywhere, in the workspace or not.
//!
//! Each row is an entry, with its size, when it was modified, its
//! permissions, and its owner, as many as there's room for; a click on a
//! column's heading sorts by it, and again, the other way. Folders come
//! first, and names sort as people count, `file2` before `file10`. Files
//! starting with `.` are hidden until asked for.
//!
//! A click selects; Shift+click selects up to there, and Ctrl+click adds
//! or takes away one entry, as Ctrl+click is in the editor (right click
//! has the menu). A double click goes into a folder or opens a file in the
//! panel, in place of the browser, which going back returns to. Dragging
//! what's selected moves it into a folder, or opens it in another panel
//! (see the app).
//!
//! Folders are listed on another thread, so a slow network filesystem or a
//! folder of a hundred thousand files never holds up the screen: names
//! come first, in batches, and then what each one is. Refreshing lists
//! again behind what's shown, which it takes the place of once done.
//!
//! Paths stay as they were gone through, symlinks and all, as a shell's
//! `cd` keeps them: `~/ws` stays `~/ws`, though the folder may really be
//! `/nfs/user/a8f3e.mount/ws`, and `..` goes back up the way it came. A
//! folder opened from a file, whose path is resolved, is shown by the way
//! it was last gone through, or failing that, through the symlinks in the
//! home folder (see [`logical`]).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentui::{Attributes, Buffer};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::file_dialog::human_size;
use crate::icons;
use crate::input::{Mouse, MouseButton, MouseKind, MULTI_CLICK};
use crate::keymap::Command;
use crate::layout::Rect;
use crate::status::Status;
use crate::theme;
use crate::wake;

/// Entries go over from the listing thread this many at a time, at most.
const BATCH: usize = 1024;
/// Or after this long, whichever comes first.
const BATCH_TIME: Duration = Duration::from_millis(40);
/// While a folder fills in, it's sorted again at most this often.
const SORT_EVERY: Duration = Duration::from_millis(150);
/// Letters typed this close together find a name together.
const TYPE_AHEAD: Duration = Duration::from_secs(1);
/// The columns between those right of the name.
const GAP: u32 = 2;
/// The name keeps at least this many columns before others are shown.
const MIN_NAME: u32 = 24;

/// What the entries are sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    /// The extension.
    Kind,
}

/// How the entries are sorted. Folders come first either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Sort {
        Sort {
            key: SortKey::Name,
            descending: false,
        }
    }
}

/// What an entry is, as `lstat` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Link,
    /// A pipe, socket, or device.
    Other,
}

/// Where a symlink points.
#[derive(Debug, Clone)]
struct Link {
    target: PathBuf,
    /// It points to a folder, which it's gone into as one.
    to_dir: bool,
    /// What it points to isn't there, or can't be reached.
    broken: bool,
}

/// What `lstat` says of an entry, but for a symlink's size, which is its
/// target's.
#[derive(Debug, Clone)]
struct Meta {
    size: u64,
    mode: u32,
    owner: Arc<str>,
    /// When it was modified, as shown, and in seconds since the epoch.
    time: String,
    modified: i64,
}

#[derive(Debug, Clone)]
struct Item {
    name: OsString,
    /// The name as shown: control characters, as a newline in a name, are
    /// shown as `?`.
    shown: String,
    /// The name in lowercase, to sort by.
    key: String,
    kind: Kind,
    link: Option<Link>,
    /// `None` until read, or if it couldn't be.
    meta: Option<Meta>,
    /// Its metadata was read, or couldn't be.
    read: bool,
}

impl Item {
    fn new(name: OsString, kind: Kind) -> Item {
        let lossy = name.to_string_lossy();
        let shown: String = lossy
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        Item {
            key: shown.to_lowercase(),
            shown,
            name,
            kind,
            link: None,
            meta: None,
            read: false,
        }
    }

    fn is_dir(&self) -> bool {
        self.kind == Kind::Dir || self.link.as_ref().is_some_and(|link| link.to_dir)
    }

    fn is_hidden(&self) -> bool {
        self.name.as_bytes().first() == Some(&b'.')
    }

    /// Its extension, lowercase, to sort by kind.
    fn extension(&self) -> &str {
        match self.key.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => extension,
            _ => "",
        }
    }
}

/// What the listing thread sends.
enum Batch {
    /// Where the folder really is, symlinks resolved.
    Real(PathBuf),
    Items(Vec<Item>),
    /// What the entries at these positions, in the order they came, are.
    Meta(Vec<(usize, Option<Meta>, Option<Link>)>),
    /// Listing is done; it stopped short if this says why.
    Done(Option<String>),
    /// The folder couldn't be read, for this reason.
    Failed(String),
}

/// A folder being listed.
struct Loader {
    batches: Receiver<Batch>,
    /// Refreshing, the new listing, to take the place of what's shown once
    /// it's done; `None` while what's shown fills in.
    next: Option<Listing>,
}

#[derive(Default)]
struct Listing {
    items: Vec<Item>,
    real: Option<PathBuf>,
    error: Option<String>,
    warning: Option<String>,
}

/// A press on a row, waiting to see whether it drags.
#[derive(Debug, Clone, Copy)]
struct Press {
    item: usize,
    x: u32,
    y: u32,
    /// Released without dragging, it selects only this one: it was pressed
    /// among others selected, which a drag takes along.
    collapse: bool,
}

/// A column right of the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Size,
    Modified,
    Mode,
    Owner,
}

impl Column {
    fn width(self) -> u32 {
        match self {
            Column::Size => 8,
            Column::Modified => 12,
            Column::Mode => 10,
            Column::Owner => 8,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Column::Size => "Size",
            Column::Modified => "Modified",
            Column::Mode => "Mode",
            Column::Owner => "Owner",
        }
    }

    fn sort(self) -> Option<SortKey> {
        match self {
            Column::Size => Some(SortKey::Size),
            Column::Modified => Some(SortKey::Modified),
            Column::Mode | Column::Owner => None,
        }
    }
}

/// What the app should do after the browser handled input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    None,
    /// Open these files in the panel, the last one shown.
    Open(Vec<PathBuf>),
    /// Go to this folder, and select the entry named, if it's there.
    Go(PathBuf, Option<OsString>),
    /// Quick Look at this, or put it away if it's what's looked at.
    Look(PathBuf),
    /// Show the menu of what's selected, at this cell, or with `None`,
    /// beside the cursor's row.
    Menu(Option<(u32, u32)>),
    /// What's selected is being dragged (see [`Browser::selection`]).
    Drag,
}

/// Something selected, or the folder listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    pub is_dir: bool,
}

pub struct Browser {
    /// The folder listed, as gone through.
    dir: PathBuf,
    real: Option<PathBuf>,
    items: Vec<Item>,
    /// The entries shown, in order, by index into `items`.
    order: Vec<usize>,
    /// Where each item is in `order`, or `usize::MAX` if hidden.
    rank: Vec<usize>,
    /// The entry the keyboard is on, and the one Shift selects from.
    cursor: Option<usize>,
    anchor: Option<usize>,
    selected: HashSet<usize>,
    /// The first row on screen.
    scroll: usize,
    /// The panel's body: the headings, then the rows.
    area: Rect,
    sort: Sort,
    show_hidden: bool,
    loader: Option<Loader>,
    /// Why the folder can't be read.
    error: Option<String>,
    /// Why it was listed only partly.
    warning: Option<String>,
    /// Entries came or changed since it was sorted, which was then.
    unsorted: bool,
    sorted_at: Instant,
    /// The entry to select once it's listed.
    wanted: Option<OsString>,
    /// What's been typed to find a name, and when last.
    typed: String,
    typed_at: Option<Instant>,
    last_click: Option<(usize, Instant)>,
    press: Option<Press>,
    /// The folder a drag would go into, if it's one listed or this one.
    drop: Option<PathBuf>,
    /// Asked to refresh while listing: it lists again once that's done,
    /// rather than start over each time, as files keep changing.
    again: bool,
    /// Nothing's been selected since the folder was gone to: the first
    /// entry stays selected as more come.
    untouched: bool,
}

impl Browser {
    /// A browser of `dir`, with `select` selected once it's listed.
    pub fn new(dir: PathBuf, select: Option<OsString>, sort: Sort, show_hidden: bool) -> Browser {
        let mut browser = Browser {
            dir: PathBuf::new(),
            real: None,
            items: Vec::new(),
            order: Vec::new(),
            rank: Vec::new(),
            cursor: None,
            anchor: None,
            selected: HashSet::new(),
            scroll: 0,
            area: Rect::default(),
            sort,
            show_hidden,
            loader: None,
            error: None,
            warning: None,
            unsorted: false,
            sorted_at: Instant::now(),
            wanted: None,
            typed: String::new(),
            typed_at: None,
            last_click: None,
            press: None,
            drop: None,
            again: false,
            untouched: true,
        };
        browser.go(dir, select);
        browser
    }

    /// The folder listed, as gone through.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the folder really is, once known.
    pub fn real_dir(&self) -> Option<&Path> {
        self.real.as_deref()
    }

    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn shows_hidden(&self) -> bool {
        self.show_hidden
    }

    #[cfg(test)]
    pub fn is_loading(&self) -> bool {
        self.loader.is_some()
    }

    /// Lists `dir` in place of what's listed, selecting `select` once it's
    /// there.
    pub fn go(&mut self, dir: PathBuf, select: Option<OsString>) {
        self.dir = normalize(&dir);
        self.real = None;
        self.items.clear();
        self.order.clear();
        self.rank.clear();
        self.cursor = None;
        self.anchor = None;
        self.selected.clear();
        self.scroll = 0;
        self.error = None;
        self.warning = None;
        self.wanted = select;
        self.press = None;
        self.last_click = None;
        self.typed.clear();
        self.again = false;
        self.untouched = true;
        self.loader = Some(Loader {
            batches: spawn_listing(self.dir.clone()),
            next: None,
        });
    }

    /// Lists the folder again, behind what's shown, which keeps its
    /// selection where it's still there. Asked while it lists, even for
    /// the first time, it lists again once done: starting over could keep
    /// a slow folder that keeps changing from ever being listed.
    pub fn refresh(&mut self) {
        if self.loader.is_some() {
            self.again = true;
            return;
        }
        self.loader = Some(Loader {
            batches: spawn_listing(self.dir.clone()),
            next: Some(Listing::default()),
        });
    }

    /// Selects the entry named `name` alone, once it's listed if it isn't
    /// yet.
    pub fn select_name(&mut self, name: &OsStr) {
        self.untouched = false;
        // Entries come before they're sorted, and shown.
        match self.items.iter().position(|item| item.name == name) {
            Some(index) if self.position_of(index).is_some() => self.select_only(index),
            _ => self.wanted = Some(name.to_os_string()),
        }
    }

    /// Takes what the listing thread sent. Returns whether anything shown
    /// changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(loader) = &mut self.loader {
            let batch = match loader.batches.try_recv() {
                Ok(batch) => batch,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => Batch::Done(None),
            };
            // Refreshing, the new listing fills in behind what's shown.
            let next = loader.next.as_mut();
            changed |= next.is_none();
            match batch {
                Batch::Real(real) => match next {
                    Some(next) => next.real = Some(real),
                    None => self.real = Some(real),
                },
                Batch::Items(items) => match next {
                    Some(next) => next.items.extend(items),
                    None => {
                        self.items.extend(items);
                        self.unsorted = true;
                    }
                },
                Batch::Meta(metas) => {
                    let live = next.is_none();
                    let items = match next {
                        Some(next) => &mut next.items,
                        None => &mut self.items,
                    };
                    for (index, meta, link) in metas {
                        if let Some(item) = items.get_mut(index) {
                            item.meta = meta;
                            item.read = true;
                            item.link = link;
                        }
                    }
                    self.unsorted |= live;
                }
                Batch::Done(warning) => {
                    match self.loader.take().and_then(|loader| loader.next) {
                        Some(next) => self.take_listing(Listing { warning, ..next }),
                        None => self.warning = warning,
                    }
                    self.unsorted = true;
                    changed = true;
                }
                Batch::Failed(error) => {
                    match self.loader.take().and_then(|loader| loader.next) {
                        Some(next) => self.take_listing(Listing {
                            error: Some(error),
                            ..next
                        }),
                        None => self.error = Some(error),
                    }
                    changed = true;
                }
            }
        }
        if self.again && self.loader.is_none() {
            self.again = false;
            self.refresh();
        }
        let due = self.loader.is_none() || self.sorted_at.elapsed() >= SORT_EVERY;
        if self.unsorted && due {
            self.resort();
            changed = true;
        }
        changed
    }

    /// Shows `listing`, a refresh of the folder, keeping the selection,
    /// cursor, and scroll on the entries still there.
    fn take_listing(&mut self, listing: Listing) {
        let index: HashMap<&OsStr, usize> = listing
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| (item.name.as_os_str(), i))
            .collect();
        let find = |old: usize| index.get(self.items[old].name.as_os_str()).copied();
        let selected: HashSet<usize> = self.selected.iter().filter_map(|&i| find(i)).collect();
        let cursor = self.cursor.and_then(find);
        let anchor = self.anchor.and_then(find);
        // Gone, the cursor goes to the entry that took its place.
        let position = self.cursor.map(|cursor| self.rank[cursor]);
        drop(index);
        self.items = listing.items;
        self.real = listing.real.or(self.real.take());
        self.error = listing.error;
        self.warning = listing.warning;
        let lost = selected.is_empty() && !self.selected.is_empty();
        self.selected = selected;
        self.cursor = cursor;
        self.anchor = anchor;
        self.resort();
        if cursor.is_none() {
            let took_place = position.and_then(|p| self.order.get(p).or(self.order.last()));
            if let Some(&item) = took_place {
                match lost || self.selected.is_empty() {
                    true => self.select_only(item),
                    false => self.cursor = Some(item),
                }
            }
        }
    }

    /// Sorts the entries shown again, keeping the cursor's row in view if
    /// it was.
    fn resort(&mut self) {
        let was_shown = self
            .cursor
            .is_some_and(|cursor| self.position_of(cursor).is_some_and(|p| self.on_screen(p)));
        let sort = self.sort;
        let items = &self.items;
        let mut order: Vec<usize> = (0..items.len())
            .filter(|&i| self.show_hidden || !items[i].is_hidden())
            .collect();
        order.sort_by(|&a, &b| compare(&items[a], &items[b], sort));
        let mut rank = vec![usize::MAX; items.len()];
        for (position, &item) in order.iter().enumerate() {
            rank[item] = position;
        }
        self.order = order;
        self.rank = rank;
        self.selected.retain(|&i| self.rank[i] != usize::MAX);
        self.unsorted = false;
        self.sorted_at = Instant::now();
        if let Some(wanted) = &self.wanted {
            let found = self
                .order
                .iter()
                .copied()
                .find(|&i| self.items[i].name == *wanted);
            if let Some(item) = found {
                self.wanted = None;
                self.untouched = false;
                self.select_only(item);
                self.center(self.rank[item]);
                return;
            }
        }
        if self.untouched && self.wanted.is_none() {
            if let Some(&first) = self.order.first() {
                self.select_only(first);
            }
        }
        if self.cursor.is_some_and(|c| self.rank[c] == usize::MAX) {
            self.cursor = None;
        }
        // Listed for the first time, the first entry is selected, as in
        // the tree.
        if self.cursor.is_none() && self.selected.is_empty() {
            if let Some(&first) = self.order.first() {
                self.select_only(first);
            }
        }
        if was_shown {
            self.scroll_into_view();
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Where `item` is among the rows, if it's shown.
    fn position_of(&self, item: usize) -> Option<usize> {
        self.rank.get(item).copied().filter(|&p| p != usize::MAX)
    }

    fn rows_shown(&self) -> usize {
        self.area.height.saturating_sub(1).max(1) as usize
    }

    fn on_screen(&self, position: usize) -> bool {
        (self.scroll..self.scroll + self.rows_shown()).contains(&position)
    }

    fn max_scroll(&self) -> usize {
        self.order.len().saturating_sub(self.rows_shown())
    }

    fn scroll_into_view(&mut self) {
        let Some(position) = self.cursor.and_then(|c| self.position_of(c)) else {
            return;
        };
        self.scroll =
            crate::tree::scroll_to_show(position, self.scroll, self.rows_shown(), self.order.len());
    }

    /// Scrolls `position` to the middle of the screen, if it's off it.
    fn center(&mut self, position: usize) {
        if !self.on_screen(position) {
            self.scroll = position.saturating_sub(self.rows_shown() / 2);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The panel's body, where it draws.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        self.scroll = self.scroll.min(self.max_scroll());
        self.scroll_into_view();
    }

    fn path_of(&self, item: usize) -> PathBuf {
        self.dir.join(&self.items[item].name)
    }

    fn target_of(&self, item: usize) -> Target {
        Target {
            path: self.path_of(item),
            is_dir: self.items[item].is_dir(),
        }
    }

    /// The name of the entry the keyboard is on.
    pub fn cursor_name(&self) -> Option<OsString> {
        self.cursor.map(|c| self.items[c].name.clone())
    }

    /// The entry the keyboard is on.
    pub fn cursor(&self) -> Option<Target> {
        self.cursor.map(|c| self.target_of(c))
    }

    /// What's selected, in the order shown.
    pub fn selection(&self) -> Vec<Target> {
        let mut selected: Vec<usize> = self.selected.iter().copied().collect();
        selected.sort_by_key(|&i| self.rank[i]);
        selected.into_iter().map(|i| self.target_of(i)).collect()
    }

    /// The folder listed, as a target.
    pub fn folder(&self) -> Target {
        Target {
            path: self.dir.clone(),
            is_dir: true,
        }
    }

    /// Whether Esc has something to do: more than one entry is selected.
    pub fn takes_escape(&self) -> bool {
        self.selected.len() > 1
    }

    fn select_only(&mut self, item: usize) {
        self.selected.clear();
        self.selected.insert(item);
        self.cursor = Some(item);
        self.anchor = Some(item);
        self.scroll_into_view();
    }

    /// Selects from the anchor to `item`, which the cursor goes to.
    fn select_to(&mut self, item: usize) {
        let anchor = self.anchor.filter(|&a| self.position_of(a).is_some());
        let Some(anchor) = anchor.or(self.cursor) else {
            return self.select_only(item);
        };
        let (a, b) = (self.rank[anchor], self.rank[item]);
        let range = a.min(b)..=a.max(b);
        self.selected = self.order[range].iter().copied().collect();
        self.cursor = Some(item);
        self.anchor = Some(anchor);
        self.scroll_into_view();
    }

    fn toggle(&mut self, item: usize) {
        if !self.selected.remove(&item) {
            self.selected.insert(item);
        }
        self.cursor = Some(item);
        self.anchor = Some(item);
    }

    /// Moves the cursor to row `position` (clamped), selecting it alone, or
    /// with `extend`, from the anchor to it.
    fn move_to(&mut self, position: usize, extend: bool) {
        let Some(&item) = self
            .order
            .get(position.min(self.order.len().saturating_sub(1)))
        else {
            return;
        };
        match extend {
            true => self.select_to(item),
            false => self.select_only(item),
        }
    }

    /// Runs `command`, which moves through the entries, opens them, or
    /// changes what's shown. With `extend`, moving selects as it goes.
    pub fn run(&mut self, command: Command, extend: bool) -> BrowserAction {
        self.untouched &= matches!(command, Command::TreeRefresh | Command::BrowserToggleHidden);
        let position = self.cursor.and_then(|c| self.position_of(c));
        let page = self.rows_shown().saturating_sub(1).max(1);
        match command {
            Command::TreeUp => match position {
                Some(p) => self.move_to(p.saturating_sub(1), extend),
                None => self.move_to(0, false),
            },
            Command::TreeDown => match position {
                Some(p) => self.move_to(p + 1, extend),
                None => self.move_to(0, false),
            },
            Command::TreePageUp => self.move_to(position.unwrap_or(0).saturating_sub(page), extend),
            Command::TreePageDown => self.move_to(position.unwrap_or(0) + page, extend),
            Command::TreeFirst => self.move_to(0, extend),
            Command::TreeLast => self.move_to(usize::MAX, extend),
            Command::TreeOpen => return self.open(),
            Command::TreeExpand => {
                if let Some(cursor) = self.cursor.filter(|&c| self.items[c].is_dir()) {
                    return BrowserAction::Go(self.path_of(cursor), None);
                }
            }
            Command::TreeCollapse | Command::BrowserParent => return self.parent(),
            Command::TreeRefresh => self.refresh(),
            Command::SelectAll => {
                self.selected = self.order.iter().copied().collect();
            }
            Command::ClearSelection => {
                if let Some(cursor) = self.cursor {
                    self.select_only(cursor);
                }
            }
            Command::BrowserToggleSelected => {
                if let Some(cursor) = self.cursor {
                    self.toggle(cursor);
                }
            }
            Command::BrowserToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.resort();
                self.scroll_into_view();
            }
            Command::BrowserSortByName => self.sort_by(SortKey::Name),
            Command::BrowserSortBySize => self.sort_by(SortKey::Size),
            Command::BrowserSortByModified => self.sort_by(SortKey::Modified),
            Command::BrowserSortByKind => self.sort_by(SortKey::Kind),
            Command::TreeContextMenu => return BrowserAction::Menu(None),
            _ => {}
        }
        BrowserAction::None
    }

    /// Sorts by `key`, or if it already is, the other way.
    pub fn sort_by(&mut self, key: SortKey) {
        self.sort = match self.sort.key == key {
            true => Sort {
                key,
                descending: !self.sort.descending,
            },
            // Newest and largest first, as people mostly want them.
            false => Sort {
                key,
                descending: matches!(key, SortKey::Size | SortKey::Modified),
            },
        };
        self.resort();
        if let Some(position) = self.cursor.and_then(|c| self.position_of(c)) {
            self.center(position);
        }
    }

    /// Enter: goes into the folder the cursor's on, or opens the files
    /// selected, or the cursor's.
    fn open(&mut self) -> BrowserAction {
        let Some(cursor) = self.cursor else {
            return BrowserAction::None;
        };
        if self.items[cursor].is_dir() {
            return BrowserAction::Go(self.path_of(cursor), None);
        }
        let mut files: Vec<PathBuf> = self
            .selection()
            .into_iter()
            .filter(|target| !target.is_dir)
            .map(|target| target.path)
            .collect();
        if !self.selected.contains(&cursor) {
            files = vec![self.path_of(cursor)];
        }
        BrowserAction::Open(files)
    }

    /// Goes up to the folder this one is in, with this one selected there.
    fn parent(&self) -> BrowserAction {
        match (self.dir.parent(), self.dir.file_name()) {
            (Some(parent), name) => {
                BrowserAction::Go(parent.to_path_buf(), name.map(OsStr::to_os_string))
            }
            (None, _) => BrowserAction::None,
        }
    }

    /// A letter typed, unbound: goes to the first name starting with what's
    /// been typed lately, as in the Finder. Returns whether it was taken.
    pub fn type_char(&mut self, c: char, now: Instant) -> bool {
        if c.is_control() {
            return false;
        }
        let fresh = self
            .typed_at
            .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD);
        if fresh {
            self.typed.clear();
        }
        self.untouched = false;
        self.typed.extend(c.to_lowercase());
        self.typed_at = Some(now);
        let typed = &self.typed;
        let found = self
            .order
            .iter()
            .copied()
            .find(|&i| self.items[i].key.starts_with(typed.as_str()));
        if let Some(item) = found {
            self.select_only(item);
            self.center(self.rank[item]);
        }
        true
    }

    // --- mouse ------------------------------------------------------------------

    /// The item on screen row `y`, if any.
    fn item_at(&self, y: u32) -> Option<usize> {
        let first = self.area.y + 1;
        if y < first || y >= self.area.bottom() {
            return None;
        }
        self.order.get(self.scroll + (y - first) as usize).copied()
    }

    /// The folder on screen at (`x`, `y`), to drop on, and its row.
    pub fn folder_at(&self, x: u32, y: u32) -> Option<(PathBuf, Rect)> {
        if !self.area.contains(x, y) {
            return None;
        }
        let item = self.item_at(y).filter(|&i| self.items[i].is_dir())?;
        let rect = Rect {
            y,
            height: 1,
            ..self.area
        };
        Some((self.path_of(item), rect))
    }

    /// Whether a drag from here at (`x`, `y`) is over one of the entries it
    /// takes.
    pub fn is_dragged_at(&self, y: u32) -> bool {
        self.item_at(y).is_some_and(|i| self.selected.contains(&i))
    }

    /// Whether row `y` has something a click acts on.
    pub fn hover_row(&self, y: u32) -> bool {
        y == self.area.y || self.item_at(y).is_some()
    }

    /// Where the cursor's row is, for a menu beside it.
    pub fn cursor_position(&self) -> Option<(u32, u32)> {
        let position = self.cursor.and_then(|c| self.position_of(c))?;
        self.on_screen(position).then(|| {
            let y = self.area.y + 1 + (position - self.scroll) as u32;
            (self.area.x + 3 + icons::width(), y)
        })
    }

    pub fn handle_mouse(&mut self, mouse: Mouse, now: Instant) -> BrowserAction {
        if let MouseKind::Press(_) = mouse.kind {
            self.untouched = false;
        }
        let wheel = crate::config::get().scroll_lines as usize;
        match mouse.kind {
            MouseKind::ScrollUp => self.scroll = self.scroll.saturating_sub(wheel),
            MouseKind::ScrollDown => self.scroll = (self.scroll + wheel).min(self.max_scroll()),
            MouseKind::Press(MouseButton::Left) if mouse.y == self.area.y => {
                if let Some(key) = self.heading_at(mouse.x) {
                    self.sort_by(key);
                }
            }
            MouseKind::Press(MouseButton::Right) => {
                match self.item_at(mouse.y) {
                    Some(item) if !self.selected.contains(&item) => self.select_only(item),
                    Some(item) => self.cursor = Some(item),
                    None => self.selected.clear(),
                }
                return BrowserAction::Menu(Some((mouse.x, mouse.y)));
            }
            MouseKind::Press(MouseButton::Left) => {
                let Some(item) = self.item_at(mouse.y) else {
                    self.selected.clear();
                    self.last_click = None;
                    return BrowserAction::None;
                };
                let double = self
                    .last_click
                    .is_some_and(|(last, at)| last == item && now.duration_since(at) < MULTI_CLICK);
                // A third click starts over.
                self.last_click = (!double).then_some((item, now));
                let mods = mouse.mods;
                if mods.alt {
                    self.select_only(item);
                    return BrowserAction::Look(self.path_of(item));
                }
                if mods.ctrl {
                    self.toggle(item);
                    return BrowserAction::None;
                }
                if mods.shift {
                    self.select_to(item);
                    return BrowserAction::None;
                }
                if double {
                    self.select_only(item);
                    return self.open();
                }
                let among = self.selected.contains(&item) && self.selected.len() > 1;
                match among {
                    true => {
                        self.cursor = Some(item);
                        self.anchor = Some(item);
                    }
                    false => self.select_only(item),
                }
                self.press = Some(Press {
                    item,
                    x: mouse.x,
                    y: mouse.y,
                    collapse: among,
                });
            }
            MouseKind::Drag(MouseButton::Left) => {
                if let Some(press) = self.press {
                    if (mouse.x, mouse.y) != (press.x, press.y) {
                        self.press = None;
                        self.last_click = None;
                        return BrowserAction::Drag;
                    }
                }
            }
            MouseKind::Release(MouseButton::Left) => {
                if let Some(press) = self.press.take() {
                    if press.collapse {
                        self.select_only(press.item);
                    }
                }
            }
            _ => {}
        }
        BrowserAction::None
    }

    /// What the heading at column `x` sorts by, if anything.
    fn heading_at(&self, x: u32) -> Option<SortKey> {
        let (name_end, columns) = layout(self.area.width);
        let x = x.checked_sub(self.area.x)?;
        if x < name_end {
            return Some(SortKey::Name);
        }
        columns
            .into_iter()
            .find(|&(column, start)| (start..start + column.width()).contains(&x))
            .and_then(|(column, _)| column.sort())
    }

    // --- drawing ----------------------------------------------------------------

    /// What the status bar says: how many entries there are, and are
    /// selected, and how big those are.
    pub fn status(&self) -> Status {
        let mut parts = vec![count(self.order.len(), "item")];
        let hidden = self.items.len() - self.order.len();
        if hidden > 0 {
            parts.push(format!("{hidden} hidden"));
        }
        if self.selected.len() > 1 {
            let mut part = format!("{} selected", self.selected.len());
            let size: u64 = self
                .selected
                .iter()
                .filter(|&&i| !self.items[i].is_dir())
                .filter_map(|&i| self.items[i].meta.as_ref())
                .map(|meta| meta.size)
                .sum();
            if size > 0 {
                part.push_str(&format!(" ({})", human_size(size)));
            }
            parts.push(part);
        } else if let Some(item) = self.cursor.map(|c| &self.items[c]) {
            if let Some(link) = &item.link {
                parts.push(format!("{} → {}", item.shown, link.target.display()));
            }
        }
        if self.loader.is_some() {
            parts.push("listing…".to_string());
        }
        if let Some(warning) = &self.warning {
            return Status::Message {
                text: format!("{} · {warning}", parts.join(" · ")),
                error: true,
            };
        }
        Status::Info(parts.join(" · "))
    }

    /// Marks `folder` as where a drag would go, or with `None`, nowhere.
    pub fn set_drop(&mut self, folder: Option<PathBuf>) {
        self.drop = folder;
    }

    /// Where a drag would go, as marked.
    pub fn drop(&self) -> Option<&Path> {
        self.drop.as_deref()
    }

    /// Draws the headings and rows in its area. The `active` panel's
    /// selection is brighter.
    pub fn draw(&self, frame: &Buffer, active: bool) {
        let drop = self.drop.as_deref();
        let colors = theme::colors();
        let area = self.area;
        if area.height == 0 {
            return;
        }
        let (name_end, columns) = layout(area.width);
        // The headings.
        frame.fill_rect(area.x, area.y, area.width, 1, colors.bg);
        let arrow = |key| match (self.sort.key == key, self.sort.descending) {
            (true, false) => " ▲",
            (true, true) => " ▼",
            (false, _) => "",
        };
        let name_heading = match self.sort.key {
            SortKey::Kind => format!("Name{}", arrow(SortKey::Kind).replace(' ', " kind ")),
            _ => format!("Name{}", arrow(SortKey::Name)),
        };
        let heading_x = area.x + 1 + icons::width();
        frame.draw_text(
            &name_heading,
            heading_x,
            area.y,
            colors.muted,
            None,
            Attributes::BOLD,
        );
        for &(column, start) in &columns {
            let title = match column.sort() {
                Some(key) => format!("{}{}", column.title(), arrow(key)),
                None => column.title().to_string(),
            };
            let x = match column {
                // Sizes are right-aligned, and so their heading.
                Column::Size => area.x + start + column.width() - title.chars().count() as u32,
                _ => area.x + start,
            };
            frame.draw_text(&title, x, area.y, colors.muted, None, Attributes::BOLD);
        }
        let body = Rect {
            y: area.y + 1,
            height: area.height - 1,
            ..area
        };
        if self.order.is_empty() {
            let (note, fg) = match (&self.error, self.loader.is_some()) {
                (Some(error), _) => (format!("Can't read this folder: {error}"), colors.error),
                (None, true) => ("Listing…".to_string(), colors.muted),
                (None, false) if !self.items.is_empty() => {
                    (count(self.items.len(), "hidden item"), colors.muted)
                }
                (None, false) => ("Empty folder".to_string(), colors.muted),
            };
            let note = crate::tree::truncate(&note, body.width.saturating_sub(2) as usize);
            let x = body.x + body.width.saturating_sub(note.width() as u32) / 2;
            let y = body.y + body.height.saturating_sub(1) / 2;
            frame.draw_text(&note, x, y, fg, None, Attributes::NONE);
            return;
        }
        let rows = self
            .order
            .iter()
            .skip(self.scroll)
            .take(body.height as usize);
        for (y, &item) in (body.y..).zip(rows) {
            self.draw_row(frame, item, y, (name_end, &columns), active, drop);
        }
    }

    fn draw_row(
        &self,
        frame: &Buffer,
        index: usize,
        y: u32,
        (name_end, columns): (u32, &[(Column, u32)]),
        active: bool,
        drop: Option<&Path>,
    ) {
        let colors = theme::colors();
        let area = self.area;
        let item = &self.items[index];
        let selected = self.selected.contains(&index);
        let bg = match selected {
            true if active => Some(colors.selected),
            true => Some(colors.selected_unfocused),
            false if active && self.cursor == Some(index) => Some(colors.line_highlight),
            false => None,
        };
        if let Some(bg) = bg {
            frame.fill_rect(area.x, y, area.width, 1, bg);
        }
        if drop.is_some_and(|drop| item.is_dir() && drop == self.path_of(index)) {
            frame.fill_rect(area.x, y, area.width, 1, colors.drop_tint);
        }
        let mut x = area.x + 1;
        let dim = item.is_hidden() || item.kind == Kind::Other;
        if icons::enabled() {
            let icon = match item.is_dir() {
                true => icons::folder(false),
                false => icons::file(&item.shown),
            };
            x = icon.draw(frame, x, y, dim.then_some(colors.muted));
        }
        let broken = item.link.as_ref().is_some_and(|link| link.broken);
        let fg = match () {
            _ if broken => colors.error,
            _ if dim => colors.muted,
            _ => colors.text,
        };
        let mut attributes = Attributes::NONE;
        if item.kind == Kind::Link {
            attributes |= Attributes::ITALIC;
        }
        let mut name = item.shown.clone();
        if item.is_dir() && !icons::enabled() {
            name.push('/');
        }
        let end = area.x + name_end;
        let room = end.saturating_sub(x + 1);
        let name = truncate_width(&name, room as usize);
        frame.draw_text(&name, x, y, fg, None, attributes);
        let mut after = x + name.width() as u32;
        if let Some(link) = &item.link {
            let arrow = format!(" → {}", link.target.display());
            let room = end.saturating_sub(after + 1) as usize;
            if room > 3 {
                let arrow = truncate_width(&arrow, room);
                let fg = if broken { colors.error } else { colors.muted };
                frame.draw_text(&arrow, after, y, fg, None, Attributes::NONE);
                after += arrow.width() as u32;
            }
        }
        let _ = after;
        for &(column, start) in columns {
            let x = area.x + start;
            let (text, fg) = self.cell(item, column);
            let text = truncate_width(&text, column.width() as usize);
            let x = match column {
                Column::Size => x + column.width() - text.width() as u32,
                _ => x,
            };
            frame.draw_text(&text, x, y, fg, None, Attributes::NONE);
        }
    }

    /// What `item` shows in `column`, and in what color.
    fn cell(&self, item: &Item, column: Column) -> (String, opentui::Rgba) {
        let colors = theme::colors();
        let Some(meta) = &item.meta else {
            let text = match item.read {
                true => "?",
                // Still being read.
                false => "",
            };
            return (text.to_string(), colors.faint);
        };
        match column {
            Column::Size if item.is_dir() => ("—".to_string(), colors.faint),
            Column::Size => (human_size(meta.size), colors.muted),
            Column::Modified => (meta.time.clone(), colors.muted),
            Column::Mode => (mode_string(item.kind, meta.mode), colors.faint),
            Column::Owner => (meta.owner.to_string(), colors.faint),
        }
    }
}

/// Where the name's columns end, and the columns right of it with where
/// each starts, from the left of a browser `width` wide: as many as fit
/// beside a name [`MIN_NAME`] wide, in the order they matter.
fn layout(width: u32) -> (u32, Vec<(Column, u32)>) {
    let mut shown = Vec::new();
    let mut used = 1;
    for column in [Column::Size, Column::Modified, Column::Mode, Column::Owner] {
        let more = column.width() + GAP;
        if width < 1 + MIN_NAME + used + more {
            break;
        }
        shown.push(column);
        used += more;
    }
    // In the order they go across.
    let across = [Column::Size, Column::Modified, Column::Mode, Column::Owner];
    let mut x = width.saturating_sub(used) + GAP;
    let name_end = width.saturating_sub(used);
    let columns = across
        .into_iter()
        .filter(|column| shown.contains(column))
        .map(|column| {
            let start = x;
            x += column.width() + GAP;
            (column, start)
        })
        .collect();
    (name_end, columns)
}

/// How `a` and `b` go in order: folders first, then as `sort` says, with
/// names as the tie-break.
fn compare(a: &Item, b: &Item, sort: Sort) -> Ordering {
    let folders = b.is_dir().cmp(&a.is_dir());
    if folders != Ordering::Equal {
        return folders;
    }
    let by_name = || natural(&a.key, &b.key).then_with(|| a.name.cmp(&b.name));
    let flip = |order: Ordering| match sort.descending {
        true => order.reverse(),
        false => order,
    };
    // Names break ties the usual way round, whichever way it's sorted.
    match sort.key {
        SortKey::Name => flip(by_name()),
        SortKey::Size => {
            let size = |item: &Item| item.meta.as_ref().map(|meta| meta.size);
            flip(size(a).cmp(&size(b))).then_with(by_name)
        }
        SortKey::Modified => {
            let time = |item: &Item| item.meta.as_ref().map(|meta| meta.modified);
            flip(time(a).cmp(&time(b))).then_with(by_name)
        }
        SortKey::Kind => flip(a.extension().cmp(b.extension())).then_with(by_name),
    }
}

/// Compares `a` and `b` as people count: runs of digits by their value,
/// so `file2` comes before `file10`.
fn natural(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let digits = |s: &[u8]| s.iter().take_while(|c| c.is_ascii_digit()).count();
                let (da, db) = (digits(a), digits(b));
                let trim = |s: &[u8]| {
                    let zeros = s.iter().take_while(|&&c| c == b'0').count();
                    s[zeros..].to_vec()
                };
                let (na, nb) = (trim(&a[..da]), trim(&b[..db]));
                let order = na
                    .len()
                    .cmp(&nb.len())
                    .then_with(|| na.cmp(&nb))
                    // `1` before `01`.
                    .then_with(|| da.cmp(&db));
                if order != Ordering::Equal {
                    return order;
                }
                (a, b) = (&a[da..], &b[db..]);
            }
            (Some(x), Some(y)) if x != y => return x.cmp(y),
            _ => (a, b) = (&a[1..], &b[1..]),
        }
    }
}

/// `ls -l`'s way of saying what `kind` of entry it is and who may do what
/// with it: `drwxr-xr-x`.
fn mode_string(kind: Kind, mode: u32) -> String {
    let mut s = String::with_capacity(10);
    s.push(match kind {
        Kind::Dir => 'd',
        Kind::Link => 'l',
        Kind::File => '-',
        Kind::Other => match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFIFO as u32 => 'p',
            m if m == libc::S_IFSOCK as u32 => 's',
            m if m == libc::S_IFCHR as u32 => 'c',
            m if m == libc::S_IFBLK as u32 => 'b',
            _ => '?',
        },
    });
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, c) in bits {
        s.push(if mode & bit != 0 { c } else { '-' });
    }
    // Set-user-ID, set-group-ID, and sticky, where `x` goes.
    let special = [(0o4000, 3, 's'), (0o2000, 6, 's'), (0o1000, 9, 't')];
    let mut chars: Vec<char> = s.chars().collect();
    for (bit, at, c) in special {
        if mode & bit != 0 {
            chars[at] = match chars[at] {
                'x' => c,
                _ => c.to_ascii_uppercase(),
            };
        }
    }
    chars.into_iter().collect()
}

fn count(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// `s` cut to `max` columns, ending in an ellipsis if cut.
fn truncate_width(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut cut = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w + 1 > max {
            break;
        }
        width += w;
        cut.push(c);
    }
    cut.push('…');
    cut
}

/// `path` without `.` and `..`, which go nowhere and up, the way they were
/// typed: symlinks are kept, as a shell's `cd` keeps them.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            component => normal.push(component),
        }
    }
    normal
}

/// Why `err` happened, briefly, without the OS's error number.
fn describe(err: &io::Error) -> String {
    let text = err.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

// --- listing ------------------------------------------------------------------

/// Lists `dir` on another thread: what it sends, until it's dropped.
fn spawn_listing(dir: PathBuf) -> Receiver<Batch> {
    let (sender, batches) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("cue-browser".into())
        .spawn(move || list(&dir, &sender));
    if let Err(err) = spawned {
        let (sender, batches) = mpsc::channel();
        let _ = sender.send(Batch::Failed(describe(&err)));
        return batches;
    }
    batches
}

/// Sends `dir`'s entries, then what each is, in batches, waking the main
/// loop for each. Stops once nothing hears.
fn list(dir: &Path, sender: &Sender<Batch>) {
    let send = |batch| {
        let heard = sender.send(batch).is_ok();
        wake::wake();
        heard
    };
    if let Ok(real) = fs::canonicalize(dir) {
        if real != dir {
            learn(&real, dir);
        }
        if !send(Batch::Real(real)) {
            return;
        }
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            send(Batch::Failed(describe(&err)));
            return;
        }
    };
    let mut names = Vec::new();
    let mut batch = Vec::new();
    let mut since = Instant::now();
    let mut warning = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                warning = Some(format!("Stopped listing: {}", describe(&err)));
                break;
            }
        };
        // From the directory entry where the filesystem says, so this
        // doesn't wait on each file.
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_symlink() => Kind::Link,
            Ok(kind) if kind.is_dir() => Kind::Dir,
            Ok(kind) if kind.is_file() => Kind::File,
            Ok(_) => Kind::Other,
            Err(_) => Kind::File,
        };
        let name = entry.file_name();
        names.push(name.clone());
        batch.push(Item::new(name, kind));
        if batch.len() >= BATCH || since.elapsed() >= BATCH_TIME {
            if !send(Batch::Items(std::mem::take(&mut batch))) {
                return;
            }
            since = Instant::now();
        }
    }
    if !batch.is_empty() && !send(Batch::Items(batch)) {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut metas = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let (meta, link) = read_meta(&dir.join(name), now);
        metas.push((index, meta, link));
        if metas.len() >= BATCH || since.elapsed() >= BATCH_TIME {
            if !send(Batch::Meta(std::mem::take(&mut metas))) {
                return;
            }
            since = Instant::now();
        }
    }
    if !metas.is_empty() && !send(Batch::Meta(metas)) {
        return;
    }
    send(Batch::Done(warning));
}

/// What's at `path`, and if it's a symlink, where it points, with `now`
/// for how long ago it was modified.
fn read_meta(path: &Path, now: i64) -> (Option<Meta>, Option<Link>) {
    let Ok(lstat) = fs::symlink_metadata(path) else {
        return (None, None);
    };
    let mut size = lstat.len();
    let link = lstat.file_type().is_symlink().then(|| {
        let target = fs::read_link(path).unwrap_or_default();
        match fs::metadata(path) {
            Ok(meta) => {
                size = meta.len();
                Link {
                    target,
                    to_dir: meta.is_dir(),
                    broken: false,
                }
            }
            Err(_) => Link {
                target,
                to_dir: false,
                broken: true,
            },
        }
    });
    let modified = lstat.mtime();
    let meta = Meta {
        size,
        mode: lstat.mode(),
        owner: owner(lstat.uid()),
        time: modified_time(modified, now),
        modified,
    };
    (Some(meta), link)
}

/// When `secs` since the epoch was, as `ls -l` says: the day and time
/// within half a year of `now`, else the day and year.
fn modified_time(secs: i64, now: i64) -> String {
    const HALF_YEAR: i64 = 182 * 24 * 60 * 60;
    let recent = (now - HALF_YEAR..=now + 60 * 60).contains(&secs);
    let format = match recent {
        true => c"%b %e %H:%M",
        false => c"%b %e  %Y",
    };
    // Named, `time_t` is deprecated on musl, whose is changing size.
    let time = secs as _;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    let mut out = [0u8; 64];
    let len = unsafe { libc::strftime(out.as_mut_ptr().cast(), out.len(), format.as_ptr(), &tm) };
    String::from_utf8_lossy(&out[..len]).into_owned()
}

/// The name of the user `uid`, or the number if there's none. Looked up
/// once each: with accounts in a directory service, as at many workplaces,
/// a lookup can take a while.
fn owner(uid: u32) -> Arc<str> {
    static NAMES: OnceLock<Mutex<HashMap<u32, Arc<str>>>> = OnceLock::new();
    let names = NAMES.get_or_init(Mutex::default);
    if let Some(name) = names.lock().unwrap_or_else(|e| e.into_inner()).get(&uid) {
        return name.clone();
    }
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let found =
        unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    let name: Arc<str> = match found == 0 && !result.is_null() && !pwd.pw_name.is_null() {
        true => unsafe { CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into(),
        false => uid.to_string().into(),
    };
    names
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(uid, name.clone());
    name
}

// --- paths --------------------------------------------------------------------

/// Folders gone through by symlinks: where each really is, and the path
/// through the link.
fn aliases() -> &'static Mutex<Vec<(PathBuf, PathBuf)>> {
    static ALIASES: OnceLock<Mutex<Vec<(PathBuf, PathBuf)>>> = OnceLock::new();
    ALIASES.get_or_init(|| Mutex::new(home_aliases()))
}

/// Notes that the folder really at `real` was gone through as `logical`.
fn learn(real: &Path, logical: &Path) {
    let mut aliases = aliases().lock().unwrap_or_else(|e| e.into_inner());
    aliases.retain(|(other, _)| other != real);
    aliases.push((real.to_path_buf(), logical.to_path_buf()));
}

/// The home folder, and the symlinks to folders in it, as where each
/// really is and the path through the home folder: at work, `~/ws` may be
/// a link to a mount on a network filesystem. Where they point is read,
/// not followed: following one could wait on a slow mount. The rest is
/// learned as the links are gone through.
fn home_aliases() -> Vec<(PathBuf, PathBuf)> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let mut aliases = Vec::new();
    // Only the home folder itself, which cue's working in already, is
    // resolved.
    if let Ok(real) = fs::canonicalize(&home) {
        if real != home {
            aliases.push((real, home.clone()));
        }
    }
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            let link = entry.path();
            if let Ok(target) = fs::read_link(&link) {
                let target = normalize(&home.join(target));
                aliases.push((target, link));
            }
        }
    }
    // The way into the folder cue was started in, if a shell said.
    if let Some(pwd) = std::env::var_os("PWD").map(PathBuf::from) {
        if let Ok(real) = fs::canonicalize(&pwd) {
            if real != pwd && pwd.is_absolute() {
                aliases.push((real, pwd));
            }
        }
    }
    aliases
}

/// `path`, which has its symlinks resolved, as gone through them: by the
/// deepest folder it's in that was gone through by a link (see
/// [`aliases`]). Otherwise as it is.
pub fn logical(path: &Path) -> PathBuf {
    let aliases = aliases().lock().unwrap_or_else(|e| e.into_inner());
    let best = aliases
        .iter()
        .filter(|(real, _)| path.starts_with(real) && real.parent().is_some())
        .max_by_key(|(real, _)| real.components().count());
    match best {
        Some((real, logical)) => match path.strip_prefix(real) {
            Ok(rest) if rest.as_os_str().is_empty() => logical.clone(),
            Ok(rest) => logical.join(rest),
            Err(_) => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// `path` as shown: in the home folder, from `~`.
pub fn display(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// The folders of `path` as shown (see [`display`]), each with its name
/// and path: `~`, `ws`, `cue` for `~/ws/cue`.
pub fn crumbs(path: &Path) -> Vec<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut crumbs = Vec::new();
    let mut at = PathBuf::new();
    let rest = match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => {
            at = home.clone().unwrap_or_default();
            crumbs.push(("~".to_string(), at.clone()));
            rest.to_path_buf()
        }
        None => path.to_path_buf(),
    };
    for component in rest.components() {
        at.push(component);
        let name = match component {
            Component::RootDir => "/".to_string(),
            component => component.as_os_str().to_string_lossy().into_owned(),
        };
        crumbs.push((name, at.clone()));
    }
    crumbs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn fixture(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("cue-browser-{}", std::process::id()))
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in files {
            let path = dir.join(file);
            match file.strip_suffix('/') {
                Some(_) => fs::create_dir_all(&path).unwrap(),
                None => {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, file.as_bytes()).unwrap();
                }
            }
        }
        dir.canonicalize().unwrap()
    }

    /// A browser of `dir`, listed in full.
    fn listed(dir: &Path) -> Browser {
        let mut browser = Browser::new(dir.to_path_buf(), None, Sort::default(), false);
        browser.set_area(Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 10,
        });
        settle(&mut browser);
        browser
    }

    fn settle(browser: &mut Browser) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while browser.is_loading() {
            assert!(Instant::now() < deadline, "listing took too long");
            browser.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        browser.poll();
    }

    fn names(browser: &Browser) -> Vec<String> {
        browser
            .order
            .iter()
            .map(|&i| browser.items[i].shown.clone())
            .collect()
    }

    fn selected(browser: &Browser) -> Vec<String> {
        browser
            .selection()
            .iter()
            .map(|target| {
                target
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn names_sort_as_people_count() {
        let mut names = vec!["file10", "file2", "file1", "file01", "a", "b10c", "b9c"];
        names.sort_by(|a, b| natural(a, b));
        assert_eq!(
            names,
            ["a", "b9c", "b10c", "file1", "file01", "file2", "file10"]
        );
    }

    #[test]
    fn lists_folders_first_and_hides_dotfiles_until_asked() {
        let root = fixture(
            "list",
            &["b.txt", "a/", "C.txt", ".hidden", "file10", "file9"],
        );
        let mut browser = listed(&root);
        assert_eq!(names(&browser), ["a", "b.txt", "C.txt", "file9", "file10"]);
        assert_eq!(browser.status(), Status::Info("5 items · 1 hidden".into()));
        browser.run(Command::BrowserToggleHidden, false);
        assert_eq!(names(&browser)[1], ".hidden");
    }

    #[test]
    fn sorts_by_size_and_back() {
        let root = fixture("size", &["small", "big"]);
        fs::write(root.join("big"), vec![b'x'; 5000]).unwrap();
        let mut browser = listed(&root);
        browser.sort_by(SortKey::Size);
        assert_eq!(names(&browser), ["big", "small"], "largest first");
        browser.sort_by(SortKey::Size);
        assert_eq!(names(&browser), ["small", "big"]);
    }

    #[test]
    fn selects_ranges_and_toggles_entries() {
        let root = fixture("select", &["a", "b", "c", "d"]);
        let mut browser = listed(&root);
        browser.run(Command::TreeDown, false);
        assert_eq!(selected(&browser), ["b"]);
        browser.run(Command::TreeDown, true);
        browser.run(Command::TreeDown, true);
        assert_eq!(selected(&browser), ["b", "c", "d"]);
        browser.run(Command::TreeUp, true);
        assert_eq!(selected(&browser), ["b", "c"]);
        browser.run(Command::BrowserToggleSelected, false);
        assert_eq!(selected(&browser), ["b"]);
        browser.run(Command::SelectAll, false);
        assert_eq!(selected(&browser), ["a", "b", "c", "d"]);
        browser.run(Command::ClearSelection, false);
        assert_eq!(selected(&browser), ["c"]);
    }

    #[test]
    fn clicks_select_and_modifiers_extend() {
        let root = fixture("clicks", &["a", "b", "c", "d"]);
        let mut browser = listed(&root);
        let now = Instant::now();
        let click = |browser: &mut Browser, y, mods, now| {
            let mut action = BrowserAction::None;
            for kind in [
                MouseKind::Press(MouseButton::Left),
                MouseKind::Release(MouseButton::Left),
            ] {
                let mouse = Mouse {
                    kind,
                    x: 5,
                    y,
                    mods,
                };
                let done = browser.handle_mouse(mouse, now);
                if done != BrowserAction::None {
                    action = done;
                }
            }
            action
        };
        // Row 1 is the headings; entries start below.
        click(&mut browser, 3, crate::input::Mods::NONE, now);
        assert_eq!(selected(&browser), ["b"]);
        click(&mut browser, 5, crate::input::Mods::SHIFT, now);
        assert_eq!(selected(&browser), ["b", "c", "d"]);
        click(&mut browser, 4, crate::input::Mods::CTRL, now);
        assert_eq!(selected(&browser), ["b", "d"]);
        // Pressing among those selected keeps them, to drag; released, it
        // selects the one.
        click(&mut browser, 5, crate::input::Mods::NONE, now);
        assert_eq!(selected(&browser), ["d"]);
        // A double click opens.
        let later = now + Duration::from_secs(1);
        click(&mut browser, 2, crate::input::Mods::NONE, later);
        let action = click(&mut browser, 2, crate::input::Mods::NONE, later);
        assert_eq!(action, BrowserAction::Open(vec![root.join("a")]));
        // Clicking a heading sorts.
        click(&mut browser, 1, crate::input::Mods::NONE, later);
        assert!(browser.sort().descending);
    }

    #[test]
    fn dragging_a_selected_row_starts_a_drag() {
        let root = fixture("drag", &["a", "b"]);
        let mut browser = listed(&root);
        let now = Instant::now();
        let mouse = |kind, x, y| Mouse {
            kind,
            x,
            y,
            mods: crate::input::Mods::NONE,
        };
        browser.handle_mouse(mouse(MouseKind::Press(MouseButton::Left), 5, 3), now);
        let action = browser.handle_mouse(mouse(MouseKind::Drag(MouseButton::Left), 6, 3), now);
        assert_eq!(action, BrowserAction::Drag);
        assert_eq!(selected(&browser), ["b"]);
    }

    #[test]
    fn typing_goes_to_a_name() {
        let root = fixture("type", &["apple", "banana", "blueberry", "cherry"]);
        let mut browser = listed(&root);
        let now = Instant::now();
        browser.type_char('b', now);
        browser.type_char('l', now);
        assert_eq!(selected(&browser), ["blueberry"]);
        browser.type_char('c', now + Duration::from_secs(2));
        assert_eq!(selected(&browser), ["cherry"]);
    }

    #[test]
    fn going_up_selects_the_folder_come_from() {
        let root = fixture("up", &["a/", "b/inner"]);
        let mut browser = listed(&root.join("b"));
        let BrowserAction::Go(dir, name) = browser.run(Command::BrowserParent, false) else {
            panic!("goes up");
        };
        assert_eq!(dir, root);
        browser.go(dir, name);
        settle(&mut browser);
        assert_eq!(selected(&browser), ["b"]);
    }

    #[test]
    fn symlinks_to_folders_are_folders_and_broken_ones_say_so() {
        let root = fixture("links", &["real/inside"]);
        symlink(root.join("real"), root.join("link")).unwrap();
        symlink(root.join("nowhere"), root.join("broken")).unwrap();
        let mut browser = listed(&root);
        assert_eq!(names(&browser), ["link", "real", "broken"]);
        let link = browser.order[0];
        browser.select_only(link);
        // Gone into by the link's path, not where it leads.
        assert_eq!(
            browser.run(Command::TreeOpen, false),
            BrowserAction::Go(root.join("link"), None)
        );
        let broken = &browser.items[browser.order[2]];
        assert!(broken.link.as_ref().unwrap().broken);
    }

    #[test]
    fn a_folder_that_cant_be_read_says_why() {
        let root = fixture("locked", &["locked/secret"]);
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let browser = listed(&locked);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        // Root can read anything.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(browser.error.as_deref(), Some("Permission denied"));
        }
    }

    #[test]
    fn refreshing_keeps_the_selection_on_what_is_still_there() {
        let root = fixture("refresh", &["a", "b", "c"]);
        let mut browser = listed(&root);
        browser.run(Command::TreeDown, false);
        browser.run(Command::TreeDown, true);
        fs::write(root.join("aa"), "").unwrap();
        fs::remove_file(root.join("c")).unwrap();
        browser.refresh();
        // What's shown stays while the new listing comes.
        assert_eq!(names(&browser), ["a", "b", "c"]);
        settle(&mut browser);
        assert_eq!(names(&browser), ["a", "aa", "b"]);
        assert_eq!(selected(&browser), ["b"]);
    }

    #[test]
    fn many_entries_list_in_batches() {
        let root = fixture("many", &[]);
        for i in 0..5000 {
            fs::write(root.join(format!("f{i}")), "").unwrap();
        }
        let browser = listed(&root);
        assert_eq!(browser.order.len(), 5000);
        assert_eq!(names(&browser)[..3], ["f0", "f1", "f2"]);
        assert_eq!(names(&browser)[4999], "f4999");
    }

    #[test]
    fn entries_listed_but_not_yet_sorted_can_be_asked_for() {
        let root = fixture("unsorted", &[]);
        for i in 0..3000 {
            fs::write(root.join(format!("f{i}")), "").unwrap();
        }
        let mut browser = Browser::new(root.clone(), None, Sort::default(), false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while browser.items.len() <= browser.rank.len() {
            assert!(Instant::now() < deadline, "listing took too long");
            browser.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        let name = browser.items.last().unwrap().name.clone();
        browser.select_name(&name);
        settle(&mut browser);
        assert_eq!(browser.cursor_name(), Some(name));
    }

    #[test]
    fn refreshing_while_first_listing_lists_again_after() {
        let root = fixture("refresh-first", &[]);
        for i in 0..3000 {
            fs::write(root.join(format!("f{i}")), "").unwrap();
        }
        let mut browser = Browser::new(root.clone(), None, Sort::default(), false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while browser.items.is_empty() {
            assert!(Instant::now() < deadline, "listing took too long");
            browser.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        fs::write(root.join("new"), "").unwrap();
        browser.refresh();
        // What's come so far stays, rather than starting over.
        assert!(!browser.items.is_empty());
        // Once listed, it lists again, and finds what came since.
        settle(&mut browser);
        assert_eq!(browser.order.len(), 3001);
    }

    #[test]
    fn modes_read_as_ls_writes_them() {
        assert_eq!(mode_string(Kind::Dir, 0o755), "drwxr-xr-x");
        assert_eq!(mode_string(Kind::File, 0o4755), "-rwsr-xr-x");
        assert_eq!(mode_string(Kind::Dir, 0o1777), "drwxrwxrwt");
        assert_eq!(mode_string(Kind::File, 0o2644), "-rw-r-Sr--");
    }

    #[test]
    fn resolved_paths_are_shown_by_the_way_they_were_gone_through() {
        let root = fixture("logical", &["mount/ws/project/"]);
        symlink(root.join("mount/ws"), root.join("ws")).unwrap();
        // Going through the link teaches the way.
        listed(&root.join("ws"));
        let resolved = root.join("mount/ws/project");
        assert_eq!(logical(&resolved), root.join("ws/project"));
        assert_eq!(logical(&root.join("mount/ws")), root.join("ws"));
        assert_eq!(logical(&root.join("mount")), root.join("mount"));
    }

    #[test]
    fn columns_fit_the_width() {
        assert!(layout(30).1.is_empty());
        let (name_end, columns) = layout(80);
        let shown: Vec<Column> = columns.iter().map(|&(column, _)| column).collect();
        assert_eq!(
            shown,
            [Column::Size, Column::Modified, Column::Mode, Column::Owner]
        );
        let (column, start) = *columns.last().unwrap();
        assert_eq!(start + column.width(), 79);
        assert!(name_end >= MIN_NAME);
        let narrow: Vec<Column> = layout(50).1.iter().map(|&(c, _)| c).collect();
        assert_eq!(narrow, [Column::Size, Column::Modified]);
    }

    #[test]
    fn crumbs_name_each_folder() {
        let crumbs = crumbs(Path::new("/var/log"));
        let names: Vec<&str> = crumbs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["/", "var", "log"]);
        assert_eq!(crumbs[1].1, Path::new("/var"));
    }
}
