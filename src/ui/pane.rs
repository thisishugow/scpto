//! Per-pane list state (pure, no I/O): which endpoint and directory a pane
//! shows, its entries, the visible (hidden-filtered, filtered, sorted) list,
//! cursor and scroll offset, the multi-selection, sort, filter, and
//! directory loading. App-level concerns (focus, which pane an action
//! targets, effects, dialogs, connection status) live in `ui/app/core.rs`
//! and the feature modules.
//!
//! # Invariants
//!
//! Fields are public so render and widgets can read them without copying,
//! but only the methods below change them, which keeps these invariants:
//!
//! - `visible` holds indices into `entries`: entries whose name passes
//!   `show_hidden` (a leading `.` hides) and the fuzzy `filter`, sorted by
//!   `sort`. Directories and symlinks to directories come first, in both
//!   orders. Ties break by name compared case-insensitively, then bytewise.
//! - Rows (plan 8.9): a listed pane whose `cwd` is not `/` shows a `..` row
//!   first, then one row per `visible` entry. Every index called a row
//!   (cursor, scroll, `row_entry`, clicks, drags) counts the `..` row. It
//!   has no entry: it is never selected, marked, transferred, or counted
//!   (select all and invert skip it, `targets` is empty while the cursor is
//!   on it), and `Open` on it goes to the parent.
//! - `cursor < row_count()` unless there are no rows (then 0);
//!   `scroll <= cursor` and the cursor row stays within the last page height
//!   passed in. A page height of 0 (a terminal too small to show rows) is
//!   treated as 1.
//! - `selection` holds names (not indices, which shift under sort and
//!   filter) of entries in `entries`; a reload drops names that vanished.
//!   Hidden or filtered-out entries stay selected but operations only use
//!   selected entries that are visible.
//! - The filter is a case-insensitive subsequence match on the name, both
//!   sides NFC-normalized first (macOS lists decomposed names); it is
//!   edited only at its end, so `TextEdit::{Left, Right, Home, End,
//!   DeleteForward}` do nothing.
//!
//! # Loading
//!
//! Navigation never changes `cwd` directly: it records a [`PendingList`]
//! and the caller emits `Effect::ListDir` for it. The old listing stays on
//! screen until `BgEvent::Listed` for the same `RequestId` arrives; success
//! replaces `cwd`, `entries`, clears the filter and the selection, and puts
//! the cursor on `reselect` when present (else the first entry, or the `..`
//! row of an empty directory); failure
//! keeps the old listing and is reported by the app. Results with any other
//! id are stale and ignored. A refresh of the same directory keeps cursor
//! (by name), selection, and filter.
//!
//! Names are matched for `reselect` and for keeping the cursor exactly
//! first, then NFC-insensitively, so a name typed by the user (NFC) finds
//! the entry a macOS volume lists in decomposed form.
//!
//! # Public API (Wave 2, ui/pane)
//!
//! ```text
//! impl Pane {
//!     /// A pane that has not listed anything yet (`cwd` None, empty).
//!     pub fn new(endpoint: Endpoint, show_hidden: bool) -> Pane;
//!     /// Switches endpoint (host picker, sudo): clears everything except
//!     /// sort and show_hidden.
//!     pub fn set_endpoint(&mut self, endpoint: Endpoint);
//!
//!     // Loading.
//!     /// Starts listing `path` (absolute) with `request`; `reselect` names
//!     /// the entry to put the cursor on afterwards. Returns the location to
//!     /// put in `Effect::ListDir`.
//!     pub fn begin_list(&mut self, request: RequestId, path: PathBuf, reselect: Option<OsString>) -> Location;
//!     /// Starts re-listing `cwd` keeping cursor, selection, and filter;
//!     /// `None` before the first listing.
//!     pub fn begin_refresh(&mut self, request: RequestId) -> Option<Location>;
//!     /// Applies a listing result. `Ok(false)` when the result is stale;
//!     /// `Err` hands a current failure back to the caller.
//!     pub fn finish_list(&mut self, request: RequestId, result: Result<Vec<Entry>, FsError>) -> Result<bool, FsError>;
//!     /// The current directory as a location, once listed.
//!     pub fn location(&self) -> Option<Location>;
//!     /// Path of the parent of `cwd`, and the name to reselect there;
//!     /// `None` at `/`.
//!     pub fn parent(&self) -> Option<(PathBuf, OsString)>;
//!     /// Absolute path of a row's entry; `None` for the `..` row.
//!     pub fn path_of(&self, index: usize) -> Option<PathBuf>;
//!
//!     // Reading.
//!     /// Whether the first row is `..` (listed, `cwd` not `/`).
//!     pub fn has_parent_row(&self) -> bool;
//!     pub fn is_parent_row(&self, index: usize) -> bool;
//!     /// Rows including the `..` row.
//!     pub fn row_count(&self) -> usize;
//!     /// The entry of a row; `None` for the `..` row and past the end.
//!     pub fn row_entry(&self, index: usize) -> Option<&Entry>;
//!     pub fn cursor_entry(&self) -> Option<&Entry>;
//!     pub fn is_selected(&self, entry: &Entry) -> bool;
//!     /// Operation targets: the visible selected entries in visible order,
//!     /// else the cursor entry; absolute paths. Empty when nothing applies.
//!     pub fn targets(&self) -> Vec<PathBuf>;
//!     /// Drag sources for a press on visible row `index`: every target when
//!     /// that row is selected, else that row alone (plan 2.4).
//!     pub fn drag_sources(&self, index: usize) -> Vec<PathBuf>;
//!
//!     // Cursor and scrolling. `page` is the number of list rows on screen.
//!     pub fn move_cursor(&mut self, movement: CursorMove, page: usize);
//!     pub fn set_cursor(&mut self, index: usize, page: usize);
//!     /// Mouse wheel: moves `scroll` by `lines` (negative is up), clamped,
//!     /// dragging the cursor along only when it would leave the page.
//!     pub fn scroll_by(&mut self, lines: i32, page: usize);
//!
//!     // Selection.
//!     /// Space / Insert: toggles the cursor row and moves down one row.
//!     pub fn toggle_and_advance(&mut self, page: usize);
//!     /// Ctrl/Cmd+click.
//!     pub fn toggle(&mut self, index: usize);
//!     /// Shift+click: selects every row between the cursor and `index`
//!     /// inclusive (adds to the selection), then moves the cursor there.
//!     pub fn select_range(&mut self, index: usize, page: usize);
//!     /// Selects every visible row.
//!     pub fn select_all(&mut self);
//!     /// Inverts the selection of every visible row.
//!     pub fn invert_selection(&mut self);
//!     pub fn clear_selection(&mut self);
//!
//!     // View.
//!     /// Sort menu and column headers: re-sorts, keeping the cursor on the
//!     /// same entry.
//!     pub fn set_sort(&mut self, sort: Sort, page: usize);
//!     /// `.`: keeps the cursor on the same entry when it stays visible.
//!     pub fn toggle_hidden(&mut self, page: usize);
//!     pub fn edit_filter(&mut self, edit: TextEdit, page: usize);
//!     pub fn clear_filter(&mut self, page: usize);
//! }
//! ```

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

use crate::error::FsError;
use crate::state::{Sort, SortKey, SortOrder};
use crate::ui::action::{CursorMove, RequestId, TextEdit};
use crate::vfs::{Endpoint, Entry, Location};

/// A directory listing in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingList {
    /// The `Effect::ListDir` correlation id.
    pub request: RequestId,
    /// The directory being listed.
    pub path: PathBuf,
    /// Whether this re-lists `cwd` (keeps cursor, selection, filter).
    pub refresh: bool,
    /// Entry to put the cursor on once listed (for example the directory
    /// just left by going to the parent, or a new folder).
    pub reselect: Option<OsString>,
}

/// One pane's state. See the module documentation for invariants and the
/// method list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The endpoint shown.
    pub endpoint: Endpoint,
    /// The listed directory; `None` until the first listing succeeded (a
    /// remote pane waits for its connection and home directory).
    pub cwd: Option<PathBuf>,
    /// Entries of `cwd` as listed (unsorted, `.` and `..` excluded).
    pub entries: Vec<Entry>,
    /// Indices into `entries` of the entries shown, in display order (the
    /// rows after the `..` row, if any).
    pub visible: Vec<usize>,
    /// Cursor row.
    pub cursor: usize,
    /// The first row on screen.
    pub scroll: usize,
    /// Names of selected entries.
    pub selection: HashSet<OsString>,
    /// Fuzzy filter text; empty means no filter.
    pub filter: String,
    /// Sort setting.
    pub sort: Sort,
    /// Whether dotfiles are shown.
    pub show_hidden: bool,
    /// The listing in flight, if any.
    pub pending: Option<PendingList>,
    /// `fold(name)` of each entry, parallel to `entries`; computed once per
    /// listing so filtering and sorting large directories do not allocate
    /// per comparison.
    folded: Vec<String>,
    /// The last page height passed in (at least 1), used where a method has
    /// no page parameter (a finished listing).
    page: usize,
}

impl Pane {
    /// A pane that has not listed anything yet (`cwd` None, empty).
    pub fn new(endpoint: Endpoint, show_hidden: bool) -> Pane {
        Pane {
            endpoint,
            cwd: None,
            entries: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            scroll: 0,
            selection: HashSet::new(),
            filter: String::new(),
            sort: Sort::default(),
            show_hidden,
            pending: None,
            folded: Vec::new(),
            page: 1,
        }
    }

    /// Switches endpoint (host picker, sudo): clears everything except sort
    /// and show_hidden.
    pub fn set_endpoint(&mut self, endpoint: Endpoint) {
        *self = Pane {
            sort: self.sort,
            page: self.page,
            ..Pane::new(endpoint, self.show_hidden)
        };
    }

    // Loading.

    /// Starts listing `path` (absolute) with `request`; `reselect` names the
    /// entry to put the cursor on afterwards. Returns the location to put in
    /// `Effect::ListDir`.
    pub fn begin_list(
        &mut self,
        request: RequestId,
        path: PathBuf,
        reselect: Option<OsString>,
    ) -> Location {
        self.pending = Some(PendingList {
            request,
            path: path.clone(),
            refresh: false,
            reselect,
        });
        Location {
            endpoint: self.endpoint.clone(),
            path,
        }
    }

    /// Starts re-listing `cwd` keeping cursor, selection, and filter; `None`
    /// before the first listing.
    pub fn begin_refresh(&mut self, request: RequestId) -> Option<Location> {
        let path = self.cwd.clone()?;
        self.pending = Some(PendingList {
            request,
            path: path.clone(),
            refresh: true,
            reselect: None,
        });
        Some(Location {
            endpoint: self.endpoint.clone(),
            path,
        })
    }

    /// Applies a listing result. `Ok(false)` when the result is stale;
    /// `Err` hands a current failure back to the caller.
    pub fn finish_list(
        &mut self,
        request: RequestId,
        result: Result<Vec<Entry>, FsError>,
    ) -> Result<bool, FsError> {
        let pending = match self.pending.take() {
            Some(pending) if pending.request == request => pending,
            other => {
                self.pending = other;
                return Ok(false);
            }
        };
        let entries = result?;
        // The refresh keeps the entry under the cursor as it is now, not as
        // it was when the refresh started: the user may have moved since.
        let reselect = if pending.refresh {
            self.cursor_entry().map(|entry| entry.name.clone())
        } else {
            pending.reselect
        };
        let old_cursor = self.cursor;
        self.folded = entries.iter().map(|entry| fold(&entry.name)).collect();
        self.entries = entries;
        if pending.refresh {
            let present: HashSet<&OsStr> =
                self.entries.iter().map(|e| e.name.as_os_str()).collect();
            self.selection
                .retain(|name| present.contains(name.as_os_str()));
        } else {
            self.cwd = Some(pending.path);
            self.selection.clear();
            self.filter.clear();
            self.scroll = 0;
        }
        self.rebuild();
        let found = reselect.as_deref().and_then(|name| self.find_row(name));
        self.cursor = match found {
            Some(index) => index,
            None if pending.refresh => old_cursor,
            None if self.visible.is_empty() => 0,
            None => self.first_entry_row(),
        };
        self.clamp(self.page);
        Ok(true)
    }

    /// The current directory as a location, once listed.
    pub fn location(&self) -> Option<Location> {
        self.cwd.as_ref().map(|path| Location {
            endpoint: self.endpoint.clone(),
            path: path.clone(),
        })
    }

    /// Path of the parent of `cwd`, and the name to reselect there; `None`
    /// at `/` and before the first listing.
    pub fn parent(&self) -> Option<(PathBuf, OsString)> {
        let cwd = self.cwd.as_ref()?;
        let name = cwd.file_name()?.to_os_string();
        Some((cwd.parent()?.to_path_buf(), name))
    }

    /// Absolute path of a row's entry; `None` for the `..` row.
    pub fn path_of(&self, index: usize) -> Option<PathBuf> {
        let entry = self.row_entry(index)?;
        Some(self.cwd.as_ref()?.join(&entry.name))
    }

    // Reading.

    /// Whether the first row is `..`: the pane is listed and `cwd` is not
    /// `/`.
    pub fn has_parent_row(&self) -> bool {
        self.cwd.as_ref().is_some_and(|cwd| cwd.parent().is_some())
    }

    /// Whether row `index` is the `..` row.
    pub fn is_parent_row(&self, index: usize) -> bool {
        index == 0 && self.has_parent_row()
    }

    /// Number of rows, including the `..` row.
    pub fn row_count(&self) -> usize {
        self.first_entry_row() + self.visible.len()
    }

    /// The entry of row `index`; `None` for the `..` row and past the end.
    pub fn row_entry(&self, index: usize) -> Option<&Entry> {
        let position = index.checked_sub(self.first_entry_row())?;
        self.visible
            .get(position)
            .map(|&entry| &self.entries[entry])
    }

    /// The entry under the cursor.
    pub fn cursor_entry(&self) -> Option<&Entry> {
        self.row_entry(self.cursor)
    }

    /// Whether `entry` is selected.
    pub fn is_selected(&self, entry: &Entry) -> bool {
        self.selection.contains(&entry.name)
    }

    /// Operation targets: the visible selected entries in visible order,
    /// else the cursor entry; absolute paths. Empty when nothing applies.
    pub fn targets(&self) -> Vec<PathBuf> {
        let Some(cwd) = &self.cwd else {
            return Vec::new();
        };
        let selected: Vec<PathBuf> = self
            .visible
            .iter()
            .map(|&entry| &self.entries[entry])
            .filter(|entry| self.is_selected(entry))
            .map(|entry| cwd.join(&entry.name))
            .collect();
        if !selected.is_empty() {
            return selected;
        }
        self.path_of(self.cursor).into_iter().collect()
    }

    /// Drag sources for a press on row `index`: every target when that row
    /// is selected, else that row alone (plan 2.4); nothing for the `..`
    /// row.
    pub fn drag_sources(&self, index: usize) -> Vec<PathBuf> {
        match self.row_entry(index) {
            Some(entry) if self.is_selected(entry) => self.targets(),
            Some(_) => self.path_of(index).into_iter().collect(),
            None => Vec::new(),
        }
    }

    // Cursor and scrolling.

    /// Moves the cursor; `page` is the number of list rows on screen.
    pub fn move_cursor(&mut self, movement: CursorMove, page: usize) {
        let page = page.max(1);
        let last = self.row_count().saturating_sub(1);
        self.cursor = match movement {
            CursorMove::Up => self.cursor.saturating_sub(1),
            CursorMove::Down => (self.cursor + 1).min(last),
            CursorMove::PageUp => self.cursor.saturating_sub(page),
            CursorMove::PageDown => (self.cursor + page).min(last),
            CursorMove::Home => 0,
            CursorMove::End => last,
        };
        self.clamp(page);
    }

    /// Puts the cursor on row `index` (clamped to the last row).
    pub fn set_cursor(&mut self, index: usize, page: usize) {
        self.cursor = index;
        self.clamp(page);
    }

    /// Mouse wheel: moves `scroll` by `lines` (negative is up), clamped,
    /// dragging the cursor along only when it would leave the page.
    pub fn scroll_by(&mut self, lines: i32, page: usize) {
        let page = page.max(1);
        let max_scroll = self.row_count().saturating_sub(page);
        let distance = usize::try_from(lines.unsigned_abs()).unwrap_or(usize::MAX);
        self.scroll = if lines < 0 {
            self.scroll.saturating_sub(distance)
        } else {
            self.scroll.saturating_add(distance)
        }
        .min(max_scroll);
        self.cursor = self.cursor.clamp(self.scroll, self.scroll + page - 1);
        self.clamp(page);
    }

    // Selection.

    /// Space / Insert: toggles the cursor row and moves down one row.
    pub fn toggle_and_advance(&mut self, page: usize) {
        self.toggle(self.cursor);
        self.move_cursor(CursorMove::Down, page);
    }

    /// Ctrl/Cmd+click: toggles the selection of row `index` (nothing for
    /// the `..` row).
    pub fn toggle(&mut self, index: usize) {
        let Some(name) = self.row_entry(index).map(|entry| entry.name.clone()) else {
            return;
        };
        if !self.selection.remove(&name) {
            self.selection.insert(name);
        }
    }

    /// Shift+click: selects every row between the cursor and `index`
    /// inclusive (adds to the selection), then moves the cursor there.
    pub fn select_range(&mut self, index: usize, page: usize) {
        if self.visible.is_empty() {
            return;
        }
        let index = index.min(self.row_count() - 1);
        let (low, high) = if index < self.cursor {
            (index, self.cursor)
        } else {
            (self.cursor, index)
        };
        // Rows to entry positions; the `..` row has none.
        let offset = self.first_entry_row();
        let low = low.saturating_sub(offset);
        if let Some(high) = high.checked_sub(offset) {
            for &entry in &self.visible[low..=high] {
                self.selection.insert(self.entries[entry].name.clone());
            }
        }
        self.set_cursor(index, page);
    }

    /// Selects every visible entry (never the `..` row).
    pub fn select_all(&mut self) {
        for &entry in &self.visible {
            self.selection.insert(self.entries[entry].name.clone());
        }
    }

    /// Inverts the selection of every visible entry (never the `..` row).
    pub fn invert_selection(&mut self) {
        for &entry in &self.visible {
            let name = &self.entries[entry].name;
            if !self.selection.remove(name) {
                self.selection.insert(name.clone());
            }
        }
    }

    /// Clears the whole selection, including hidden and filtered-out
    /// entries.
    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    // View.

    /// Sort menu and column headers: re-sorts, keeping the cursor on the
    /// same entry.
    pub fn set_sort(&mut self, sort: Sort, page: usize) {
        self.sort = sort;
        self.rebuild_keeping_cursor(page);
    }

    /// `.`: keeps the cursor on the same entry when it stays visible.
    pub fn toggle_hidden(&mut self, page: usize) {
        self.show_hidden = !self.show_hidden;
        self.rebuild_keeping_cursor(page);
    }

    /// Edits the filter at its end; caret movement and forward deletion do
    /// nothing.
    pub fn edit_filter(&mut self, edit: TextEdit, page: usize) {
        match edit {
            TextEdit::Insert(c) => self.filter.push(c),
            TextEdit::Backspace => {
                self.filter.pop();
            }
            TextEdit::DeleteWord => {
                let kept = self
                    .filter
                    .trim_end()
                    .trim_end_matches(|c: char| !c.is_whitespace());
                self.filter.truncate(kept.len());
            }
            TextEdit::Clear => self.filter.clear(),
            TextEdit::DeleteForward
            | TextEdit::Left
            | TextEdit::Right
            | TextEdit::Home
            | TextEdit::End => return,
        }
        self.rebuild_keeping_cursor(page);
    }

    /// Removes the filter, keeping the cursor on the same entry.
    pub fn clear_filter(&mut self, page: usize) {
        if self.filter.is_empty() {
            return;
        }
        self.filter.clear();
        self.rebuild_keeping_cursor(page);
    }

    // Internals.

    /// Recomputes `visible` from `entries`, `show_hidden`, `filter`, and
    /// `sort`. Does not touch the cursor.
    fn rebuild(&mut self) {
        let filter = fold(OsStr::new(&self.filter));
        let entries = &self.entries;
        let folded = &self.folded;
        let show_hidden = self.show_hidden;
        self.visible = (0..entries.len())
            .filter(|&i| show_hidden || !entries[i].name.as_bytes().starts_with(b"."))
            .filter(|&i| is_subsequence(&filter, &folded[i]))
            .collect();
        let rank = |i: usize| !entries[i].is_enterable();
        let name = |i: usize| (folded[i].as_str(), entries[i].name.as_bytes());
        let extension = |i: usize| extension(&folded[i]);
        let size = |i: usize| {
            if entries[i].is_enterable() {
                None
            } else {
                entries[i].size
            }
        };
        let mtime = |i: usize| entries[i].mtime;
        let visible = &mut self.visible;
        match (self.sort.key, self.sort.order) {
            (SortKey::Name, SortOrder::Ascending) => {
                visible.sort_by_cached_key(|&i| (rank(i), name(i)));
            }
            (SortKey::Name, SortOrder::Descending) => {
                visible.sort_by_cached_key(|&i| (rank(i), Reverse(name(i))));
            }
            (SortKey::Extension, SortOrder::Ascending) => {
                visible.sort_by_cached_key(|&i| (rank(i), extension(i), name(i)));
            }
            (SortKey::Extension, SortOrder::Descending) => {
                visible.sort_by_cached_key(|&i| (rank(i), Reverse((extension(i), name(i)))));
            }
            (SortKey::Size, SortOrder::Ascending) => {
                visible.sort_by_cached_key(|&i| (rank(i), size(i), name(i)));
            }
            (SortKey::Size, SortOrder::Descending) => {
                visible.sort_by_cached_key(|&i| (rank(i), Reverse(size(i)), name(i)));
            }
            (SortKey::Modified, SortOrder::Ascending) => {
                visible.sort_by_cached_key(|&i| (rank(i), mtime(i), name(i)));
            }
            (SortKey::Modified, SortOrder::Descending) => {
                visible.sort_by_cached_key(|&i| (rank(i), Reverse(mtime(i)), name(i)));
            }
        }
    }

    /// Rebuilds `visible` and keeps the cursor on the same entry when it is
    /// still visible (on the `..` row when it was there), else at the same
    /// row (clamped).
    fn rebuild_keeping_cursor(&mut self, page: usize) {
        let offset = self.first_entry_row();
        let current = self
            .cursor
            .checked_sub(offset)
            .and_then(|position| self.visible.get(position).copied());
        self.rebuild();
        if let Some(position) =
            current.and_then(|entry| self.visible.iter().position(|&v| v == entry))
        {
            self.cursor = offset + position;
        }
        self.clamp(page);
    }

    /// The row of the first entry: 1 below a `..` row, else 0.
    fn first_entry_row(&self) -> usize {
        usize::from(self.has_parent_row())
    }

    /// Row of the entry named `name`: an exact match first, else one whose
    /// NFC form equals the NFC form of `name`.
    fn find_row(&self, name: &OsStr) -> Option<usize> {
        let exact = self
            .visible
            .iter()
            .position(|&entry| self.entries[entry].name == name);
        let position = exact.or_else(|| {
            let wanted = nfc(name);
            self.visible
                .iter()
                .position(|&entry| nfc(&self.entries[entry].name) == wanted)
        })?;
        Some(self.first_entry_row() + position)
    }

    /// Restores the cursor and scroll invariants for `page` rows and
    /// remembers `page`.
    fn clamp(&mut self, page: usize) {
        let page = page.max(1);
        self.page = page;
        let rows = self.row_count();
        if rows == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(rows - 1);
        self.scroll = self.scroll.min(rows.saturating_sub(page));
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + page {
            self.scroll = self.cursor + 1 - page;
        }
    }
}

/// A name in NFC form (lossy for names that are not UTF-8).
fn nfc(name: &OsStr) -> String {
    name.to_string_lossy().nfc().collect()
}

/// The form names and filter text are compared in: NFC, then lowercase.
fn fold(name: &OsStr) -> String {
    nfc(name).to_lowercase()
}

/// The extension of a folded name: the text after the last `.`, empty for
/// names without one and for dotfiles without a second dot.
fn extension(folded: &str) -> &str {
    match folded.rfind('.') {
        Some(0) | None => "",
        Some(dot) => &folded[dot + 1..],
    }
}

/// Whether every character of `needle` occurs in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut rest = haystack.chars();
    needle.chars().all(|wanted| rest.any(|c| c == wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::EntryKind;
    use std::time::Instant;

    const PAGE: usize = 3;

    fn file(name: &str, size: u64, mtime: i64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(size),
            mtime: Some(mtime),
            mode: Some(0o644),
        }
    }

    fn dir(name: &str, mtime: i64) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Dir,
            size: Some(4096),
            mtime: Some(mtime),
            mode: Some(0o755),
        }
    }

    fn dir_link(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Symlink {
                target: "/srv".into(),
                target_is_dir: true,
            },
            size: Some(4),
            mtime: Some(0),
            mode: Some(0o777),
        }
    }

    fn sample() -> Vec<Entry> {
        vec![
            file("b.txt", 300, 30),
            file("A.txt", 100, 10),
            dir("zdir", 5),
            file(".hidden", 50, 50),
            file("c.log", 200, 20),
            dir("adir", 1),
        ]
    }

    /// A local pane listed at `/home/u` with `entries`, told the page
    /// height first like the app does on its first resize.
    fn listed(entries: Vec<Entry>) -> Pane {
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.set_cursor(0, PAGE);
        pane.begin_list(RequestId(1), "/home/u".into(), None);
        assert_eq!(pane.finish_list(RequestId(1), Ok(entries)), Ok(true));
        assert_invariants(&pane, PAGE);
        pane
    }

    /// Names of the entry rows in order (the `..` row has no entry).
    fn names(pane: &Pane) -> Vec<String> {
        (0..pane.row_count())
            .filter_map(|i| pane.row_entry(i))
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect()
    }

    fn cursor_name(pane: &Pane) -> String {
        pane.cursor_entry()
            .expect("cursor")
            .name
            .to_string_lossy()
            .into_owned()
    }

    fn assert_invariants(pane: &Pane, page: usize) {
        let page = page.max(1);
        assert!(pane.visible.iter().all(|&i| i < pane.entries.len()));
        assert_eq!(
            pane.row_count(),
            pane.visible.len() + usize::from(pane.has_parent_row())
        );
        if pane.row_count() == 0 {
            assert_eq!((pane.cursor, pane.scroll), (0, 0));
        } else {
            assert!(pane.cursor < pane.row_count());
            assert!(pane.scroll <= pane.cursor);
            assert!(pane.cursor < pane.scroll + page);
        }
        assert_eq!(pane.folded.len(), pane.entries.len());
    }

    #[test]
    fn default_sort_is_name_ascending() {
        assert_eq!(
            Sort::default(),
            Sort {
                key: SortKey::Name,
                order: SortOrder::Ascending
            }
        );
    }

    #[test]
    fn panes_below_the_root_start_with_a_parent_row() {
        let pane = listed(sample());
        assert!(pane.has_parent_row());
        assert!(pane.is_parent_row(0));
        assert!(!pane.is_parent_row(1));
        assert_eq!(pane.row_count(), 6);
        assert_eq!(pane.row_entry(0), None);
        assert_eq!(pane.path_of(0), None);
        assert_eq!(pane.row_entry(1).expect("row").name, "adir");
        assert_eq!(pane.row_entry(6), None);
        // A new listing puts the cursor on the first entry, not on `..`.
        assert_eq!(pane.cursor, 1);

        // An empty directory has only the `..` row, with the cursor on it.
        let empty = listed(Vec::new());
        assert_eq!((empty.row_count(), empty.cursor), (1, 0));
        assert!(empty.targets().is_empty());

        // The root has no parent row; neither has an unlisted pane.
        let mut root = Pane::new(Endpoint::Local, false);
        assert!(!root.has_parent_row());
        root.begin_list(RequestId(1), "/".into(), None);
        assert_eq!(root.finish_list(RequestId(1), Ok(sample())), Ok(true));
        assert!(!root.has_parent_row());
        assert_eq!((root.row_count(), root.cursor), (5, 0));
    }

    #[test]
    fn the_parent_row_is_never_selected_targeted_or_dragged() {
        let mut pane = listed(sample());
        pane.set_cursor(0, PAGE);
        assert!(pane.cursor_entry().is_none());
        assert!(pane.targets().is_empty());
        assert!(pane.drag_sources(0).is_empty());
        pane.toggle(0);
        assert!(pane.selection.is_empty());
        // Space on `..` selects nothing and moves down.
        pane.toggle_and_advance(PAGE);
        assert!(pane.selection.is_empty());
        assert_eq!(pane.cursor, 1);
        // Select all and invert only cover entries.
        pane.select_all();
        assert_eq!(pane.selection.len(), 5);
        pane.invert_selection();
        assert!(pane.selection.is_empty());
        // A range starting on `..` covers the entries up to the row.
        pane.set_cursor(0, PAGE);
        pane.select_range(2, PAGE);
        let mut selected: Vec<_> = pane.selection.iter().cloned().collect();
        selected.sort();
        assert_eq!(selected, ["adir", "zdir"]);
        // A range ending on `..` from the first entry selects that entry.
        pane.clear_selection();
        pane.set_cursor(1, PAGE);
        pane.select_range(0, PAGE);
        assert_eq!(pane.selection.len(), 1);
        assert_eq!(pane.cursor, 0);
        // While the cursor is on `..`, a selection is still the target.
        assert_eq!(pane.targets(), [PathBuf::from("/home/u/adir")]);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn the_parent_row_stays_on_top_through_sort_and_filter() {
        let mut pane = listed(sample());
        pane.set_cursor(0, PAGE);
        pane.set_sort(Sort::by(SortKey::Size), PAGE);
        assert_eq!(pane.cursor, 0);
        assert!(pane.is_parent_row(0));
        pane.edit_filter(TextEdit::Insert('z'), PAGE);
        assert_eq!(names(&pane), ["zdir"]);
        assert_eq!(pane.row_count(), 2);
        pane.edit_filter(TextEdit::Insert('q'), PAGE);
        assert_eq!(pane.row_count(), 1);
        assert_eq!(pane.cursor, 0);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn new_pane_is_empty_until_listed() {
        let pane = Pane::new(Endpoint::Local, true);
        assert_eq!(pane.cwd, None);
        assert_eq!(pane.location(), None);
        assert_eq!(pane.parent(), None);
        assert!(pane.targets().is_empty());
        assert!(pane.show_hidden);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn listing_sorts_dirs_first_case_insensitively_and_hides_dotfiles() {
        let pane = listed(sample());
        assert_eq!(names(&pane), ["adir", "zdir", "A.txt", "b.txt", "c.log"]);
        assert_eq!(pane.cwd, Some(PathBuf::from("/home/u")));
        assert_eq!(pane.cursor, 1);
    }

    #[test]
    fn symlinks_to_directories_sort_with_directories() {
        let pane = listed(vec![file("a", 1, 1), dir_link("link"), dir("z", 1)]);
        assert_eq!(names(&pane), ["link", "z", "a"]);
    }

    #[test]
    fn name_ties_break_bytewise() {
        let pane = listed(vec![file("b", 1, 1), file("B", 1, 1), file("a", 1, 1)]);
        assert_eq!(names(&pane), ["a", "B", "b"]);
    }

    #[test]
    fn sorting_keeps_dirs_first_and_the_cursor_entry() {
        let mut pane = listed(sample());
        pane.set_cursor(4, PAGE);
        assert_eq!(cursor_name(&pane), "b.txt");

        pane.set_sort(Sort::by(SortKey::Size), PAGE);
        assert_eq!(
            pane.sort,
            Sort {
                key: SortKey::Size,
                order: SortOrder::Descending
            }
        );
        // Directories keep name order among themselves (size counts as 0).
        assert_eq!(names(&pane), ["adir", "zdir", "b.txt", "c.log", "A.txt"]);
        assert_eq!(cursor_name(&pane), "b.txt");
        assert_invariants(&pane, PAGE);

        pane.set_sort(Sort::by(SortKey::Modified), PAGE);
        assert_eq!(
            pane.sort,
            Sort {
                key: SortKey::Modified,
                order: SortOrder::Descending
            }
        );
        assert_eq!(names(&pane), ["zdir", "adir", "b.txt", "c.log", "A.txt"]);
        assert_eq!(cursor_name(&pane), "b.txt");

        pane.set_sort(Sort::by(SortKey::Name), PAGE);
        assert_eq!(pane.sort, Sort::default());
        assert_eq!(names(&pane), ["adir", "zdir", "A.txt", "b.txt", "c.log"]);
        assert_eq!(cursor_name(&pane), "b.txt");
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn every_sort_order_is_supported() {
        let mut pane = listed(sample());
        let cases = [
            (
                SortKey::Name,
                SortOrder::Descending,
                ["zdir", "adir", "c.log", "b.txt", "A.txt"],
            ),
            (
                SortKey::Size,
                SortOrder::Ascending,
                ["adir", "zdir", "A.txt", "c.log", "b.txt"],
            ),
            (
                SortKey::Modified,
                SortOrder::Ascending,
                ["adir", "zdir", "A.txt", "c.log", "b.txt"],
            ),
            (
                SortKey::Extension,
                SortOrder::Ascending,
                ["adir", "zdir", "c.log", "A.txt", "b.txt"],
            ),
            (
                SortKey::Extension,
                SortOrder::Descending,
                ["zdir", "adir", "b.txt", "A.txt", "c.log"],
            ),
        ];
        for (key, order, expected) in cases {
            pane.sort = Sort { key, order };
            pane.rebuild_keeping_cursor(PAGE);
            assert_eq!(names(&pane), expected, "{key:?} {order:?}");
        }
    }

    #[test]
    fn extensions_are_the_text_after_the_last_dot() {
        assert_eq!(extension("archive.tar.gz"), "gz");
        assert_eq!(extension("readme"), "");
        assert_eq!(extension(".bashrc"), "");
        assert_eq!(extension(".config.toml"), "toml");
        assert_eq!(extension("trailing."), "");
        let pane = {
            let mut pane = listed(vec![
                file("b.rs", 1, 1),
                file("Makefile", 1, 1),
                file("a.toml", 1, 1),
                file("c.RS", 1, 1),
            ]);
            pane.set_sort(Sort::by(SortKey::Extension), PAGE);
            pane
        };
        // No extension first, then extensions compared case-insensitively.
        assert_eq!(names(&pane), ["Makefile", "b.rs", "c.RS", "a.toml"]);
    }

    #[test]
    fn unknown_sizes_and_times_sort_as_smallest() {
        let mut unknown = file("x", 0, 0);
        unknown.size = None;
        unknown.mtime = None;
        let mut pane = listed(vec![file("y", 5, 5), unknown]);
        pane.sort = Sort {
            key: SortKey::Size,
            order: SortOrder::Ascending,
        };
        pane.rebuild_keeping_cursor(PAGE);
        assert_eq!(names(&pane), ["x", "y"]);
        pane.sort = Sort {
            key: SortKey::Modified,
            order: SortOrder::Descending,
        };
        pane.rebuild_keeping_cursor(PAGE);
        assert_eq!(names(&pane), ["y", "x"]);
    }

    #[test]
    fn sorting_100k_entries_is_fast_and_correct() {
        let entries: Vec<Entry> = (0..100_000u64)
            .map(|i| {
                // Spread names, sizes, and times so no key is presorted.
                let n = i.wrapping_mul(2_654_435_761) % 1_000_003;
                if i % 10 == 0 {
                    dir(&format!("Dir{n:07}"), n as i64)
                } else {
                    file(
                        &format!("file{n:07}.dat"),
                        n * 7 % 100_000,
                        (n * 13 % 1_000_000) as i64,
                    )
                }
            })
            .collect();
        let mut pane = listed(entries);
        assert_eq!(pane.visible.len(), 100_000);

        let started = Instant::now();
        for key in [SortKey::Size, SortKey::Modified, SortKey::Name] {
            pane.set_sort(Sort::by(key), PAGE);
        }
        let elapsed = started.elapsed();

        assert_eq!(pane.sort, Sort::default());
        let ordered = |pane: &Pane| {
            pane.visible.windows(2).all(|pair| {
                let (a, b) = (&pane.entries[pair[0]], &pane.entries[pair[1]]);
                let key = |e: &Entry| (!e.is_enterable(), e.name.to_ascii_lowercase());
                key(a) <= key(b)
            })
        };
        assert!(ordered(&pane));
        // Three full re-sorts of 100k rows; generous for unoptimized builds,
        // and far below what a sort that allocates per comparison takes.
        assert!(elapsed.as_secs_f64() < 3.0, "sorting took {elapsed:?}");

        pane.sort = Sort {
            key: SortKey::Size,
            order: SortOrder::Descending,
        };
        pane.rebuild_keeping_cursor(PAGE);
        assert!(pane.visible.windows(2).all(|pair| {
            let (a, b) = (&pane.entries[pair[0]], &pane.entries[pair[1]]);
            match (a.is_enterable(), b.is_enterable()) {
                (true, false) => true,
                (false, true) => false,
                (true, true) => a.name.to_ascii_lowercase() <= b.name.to_ascii_lowercase(),
                (false, false) => a.size >= b.size,
            }
        }));
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn hidden_toggle_keeps_the_cursor_entry() {
        let mut pane = listed(sample());
        pane.set_cursor(3, PAGE);
        assert_eq!(cursor_name(&pane), "A.txt");
        pane.toggle_hidden(PAGE);
        assert_eq!(
            names(&pane),
            ["adir", "zdir", ".hidden", "A.txt", "b.txt", "c.log"]
        );
        assert_eq!(cursor_name(&pane), "A.txt");

        pane.set_cursor(3, PAGE);
        assert_eq!(cursor_name(&pane), ".hidden");
        pane.toggle_hidden(PAGE);
        // The entry disappeared; the cursor stays at its position.
        assert_eq!(pane.cursor, 3);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn filter_is_a_case_insensitive_subsequence_edited_at_the_end() {
        let mut pane = listed(sample());
        for c in "TXT".chars() {
            pane.edit_filter(TextEdit::Insert(c), PAGE);
        }
        assert_eq!(names(&pane), ["A.txt", "b.txt"]);
        for edit in [
            TextEdit::Left,
            TextEdit::Home,
            TextEdit::End,
            TextEdit::Right,
            TextEdit::DeleteForward,
        ] {
            pane.edit_filter(edit, PAGE);
            assert_eq!(pane.filter, "TXT");
        }
        pane.edit_filter(TextEdit::Backspace, PAGE);
        assert_eq!(pane.filter, "TX");
        assert_eq!(names(&pane), ["A.txt", "b.txt"]);

        pane.edit_filter(TextEdit::Clear, PAGE);
        for c in "dr".chars() {
            pane.edit_filter(TextEdit::Insert(c), PAGE);
        }
        assert_eq!(names(&pane), ["adir", "zdir"]);

        pane.edit_filter(TextEdit::Insert('x'), PAGE);
        assert!(pane.visible.is_empty());
        assert_invariants(&pane, PAGE);
        pane.clear_filter(PAGE);
        assert_eq!(pane.visible.len(), 5);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn filter_delete_word_removes_the_last_word() {
        let mut pane = listed(sample());
        for c in "ab cd  ".chars() {
            pane.edit_filter(TextEdit::Insert(c), PAGE);
        }
        pane.edit_filter(TextEdit::DeleteWord, PAGE);
        assert_eq!(pane.filter, "ab ");
        pane.edit_filter(TextEdit::DeleteWord, PAGE);
        assert_eq!(pane.filter, "");
    }

    #[test]
    fn filter_matches_decomposed_names_with_composed_input() {
        // "caf\u{e9}" typed (NFC) against "cafe\u{301}" as listed by macOS (NFD).
        let mut pane = listed(vec![file("Cafe\u{301}.txt", 1, 1), file("cafe.txt", 1, 1)]);
        for c in "caf\u{e9}".chars() {
            pane.edit_filter(TextEdit::Insert(c), PAGE);
        }
        assert_eq!(names(&pane), ["Cafe\u{301}.txt"]);
    }

    #[test]
    fn cursor_moves_and_scroll_follows_the_page() {
        // Ten entries below the `..` row: rows 0..=10.
        let entries: Vec<Entry> = (0..10).map(|i| file(&format!("f{i}"), 1, 1)).collect();
        let mut pane = listed(entries);
        pane.move_cursor(CursorMove::Down, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (2, 0));
        pane.move_cursor(CursorMove::PageDown, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (5, 3));
        pane.move_cursor(CursorMove::End, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (10, 8));
        pane.move_cursor(CursorMove::Down, PAGE);
        assert_eq!(pane.cursor, 10);
        pane.move_cursor(CursorMove::PageUp, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (7, 7));
        pane.move_cursor(CursorMove::Up, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (6, 6));
        pane.move_cursor(CursorMove::Home, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (0, 0));
        pane.move_cursor(CursorMove::Up, PAGE);
        assert_eq!(pane.cursor, 0);
        pane.set_cursor(99, PAGE);
        assert_eq!((pane.cursor, pane.scroll), (10, 8));
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn a_zero_page_height_does_not_underflow() {
        let entries: Vec<Entry> = (0..5).map(|i| file(&format!("f{i}"), 1, 1)).collect();
        let mut pane = listed(entries);
        for movement in [
            CursorMove::PageDown,
            CursorMove::Down,
            CursorMove::End,
            CursorMove::PageUp,
        ] {
            pane.move_cursor(movement, 0);
            assert_invariants(&pane, 0);
        }
        pane.scroll_by(-10, 0);
        assert_invariants(&pane, 0);
        pane.scroll_by(10, 0);
        assert_invariants(&pane, 0);
        pane.toggle_and_advance(0);
        pane.select_range(0, 0);
        pane.set_sort(Sort::by(SortKey::Size), 0);
        assert_invariants(&pane, 0);

        let mut empty = listed(Vec::new());
        for movement in [CursorMove::Down, CursorMove::End, CursorMove::PageUp] {
            empty.move_cursor(movement, 0);
        }
        empty.scroll_by(3, 0);
        empty.toggle_and_advance(0);
        empty.select_range(4, 0);
        assert_invariants(&empty, 0);
        assert!(empty.selection.is_empty());
    }

    #[test]
    fn wheel_scrolls_and_drags_the_cursor_only_when_it_leaves_the_page() {
        let entries: Vec<Entry> = (0..10).map(|i| file(&format!("f{i}"), 1, 1)).collect();
        let mut pane = listed(entries);
        pane.set_cursor(1, PAGE);
        pane.scroll_by(1, PAGE);
        assert_eq!((pane.scroll, pane.cursor), (1, 1));
        pane.scroll_by(3, PAGE);
        assert_eq!((pane.scroll, pane.cursor), (4, 4));
        pane.scroll_by(100, PAGE);
        assert_eq!((pane.scroll, pane.cursor), (8, 8));
        pane.scroll_by(-1, PAGE);
        assert_eq!((pane.scroll, pane.cursor), (7, 8));
        pane.scroll_by(-100, PAGE);
        assert_eq!((pane.scroll, pane.cursor), (0, 2));
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn selection_toggle_range_all_and_invert() {
        let mut pane = listed(sample());
        pane.toggle_and_advance(PAGE);
        assert_eq!(pane.cursor, 2);
        assert!(pane.is_selected(&dir("adir", 0)));

        pane.toggle(1);
        assert!(pane.selection.is_empty());

        pane.set_cursor(2, PAGE);
        pane.select_range(4, PAGE);
        assert_eq!(pane.cursor, 4);
        let mut selected: Vec<_> = pane.selection.iter().cloned().collect();
        selected.sort();
        assert_eq!(selected, ["A.txt", "b.txt", "zdir"]);

        // Ranges also extend upwards and add to the selection.
        pane.select_range(0, PAGE);
        assert_eq!(pane.selection.len(), 4);
        assert_eq!(pane.cursor, 0);

        pane.invert_selection();
        let inverted: Vec<_> = pane.selection.iter().cloned().collect();
        assert_eq!(inverted, ["c.log"]);

        pane.select_all();
        assert_eq!(pane.selection.len(), 5);
        assert!(!pane.selection.contains(OsStr::new(".hidden")));
        pane.clear_selection();
        assert!(pane.selection.is_empty());
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn targets_use_visible_selection_else_the_cursor() {
        let mut pane = listed(sample());
        pane.set_cursor(3, PAGE);
        assert_eq!(pane.targets(), [PathBuf::from("/home/u/A.txt")]);

        pane.toggle(5);
        pane.toggle(1);
        assert_eq!(
            pane.targets(),
            [
                PathBuf::from("/home/u/adir"),
                PathBuf::from("/home/u/c.log")
            ]
        );

        // A selection that is entirely filtered out falls back to the cursor.
        for c in "txt".chars() {
            pane.edit_filter(TextEdit::Insert(c), PAGE);
        }
        assert!(pane.is_selected(&dir("adir", 0)));
        assert_eq!(pane.targets(), [PathBuf::from("/home/u/A.txt")]);
    }

    #[test]
    fn drag_sources_follow_plan_2_4() {
        let mut pane = listed(sample());
        pane.toggle(3);
        pane.toggle(4);
        assert_eq!(
            pane.drag_sources(4),
            [
                PathBuf::from("/home/u/A.txt"),
                PathBuf::from("/home/u/b.txt")
            ]
        );
        assert_eq!(pane.drag_sources(5), [PathBuf::from("/home/u/c.log")]);
        assert!(pane.drag_sources(0).is_empty());
        assert!(pane.drag_sources(99).is_empty());
    }

    #[test]
    fn navigation_reselects_the_directory_just_left() {
        let mut pane = listed(sample());
        let location = pane.begin_list(RequestId(2), "/home/u/zdir".into(), None);
        assert_eq!(
            location,
            Location {
                endpoint: Endpoint::Local,
                path: "/home/u/zdir".into()
            }
        );
        // The old listing stays until the result arrives.
        assert_eq!(pane.visible.len(), 5);
        assert_eq!(
            pane.finish_list(RequestId(2), Ok(vec![file("inner", 1, 1)])),
            Ok(true)
        );
        assert_eq!(pane.cwd, Some(PathBuf::from("/home/u/zdir")));

        let (parent, name) = pane.parent().expect("parent");
        assert_eq!(
            (parent.clone(), name.clone()),
            (PathBuf::from("/home/u"), OsString::from("zdir"))
        );
        pane.begin_list(RequestId(3), parent, Some(name));
        assert_eq!(pane.finish_list(RequestId(3), Ok(sample())), Ok(true));
        assert_eq!(cursor_name(&pane), "zdir");
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn reselect_matches_decomposed_names() {
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.begin_list(RequestId(1), "/v".into(), Some("caf\u{e9}".into()));
        let entries = vec![file("a", 1, 1), file("b", 1, 1), dir("cafe\u{301}", 1)];
        assert_eq!(pane.finish_list(RequestId(1), Ok(entries)), Ok(true));
        assert_eq!(cursor_name(&pane), "cafe\u{301}");
    }

    #[test]
    fn reselect_prefers_an_exact_match() {
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.begin_list(RequestId(1), "/v".into(), Some("cafe\u{301}".into()));
        let entries = vec![file("caf\u{e9}", 1, 1), file("cafe\u{301}", 1, 1)];
        assert_eq!(pane.finish_list(RequestId(1), Ok(entries)), Ok(true));
        assert_eq!(cursor_name(&pane), "cafe\u{301}");
    }

    #[test]
    fn reselect_far_down_scrolls_it_into_the_last_page() {
        let entries: Vec<Entry> = (0..10).map(|i| file(&format!("f{i}"), 1, 1)).collect();
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.set_cursor(0, 4);
        pane.begin_list(RequestId(1), "/v".into(), Some("f8".into()));
        assert_eq!(pane.finish_list(RequestId(1), Ok(entries)), Ok(true));
        // `f8` is row 9, below the `..` row.
        assert_eq!((pane.cursor, pane.scroll), (9, 6));
        assert_invariants(&pane, 4);
    }

    #[test]
    fn new_directory_clears_filter_and_selection() {
        let mut pane = listed(sample());
        pane.toggle(1);
        pane.edit_filter(TextEdit::Insert('t'), PAGE);
        pane.begin_list(RequestId(2), "/other".into(), None);
        assert_eq!(pane.finish_list(RequestId(2), Ok(sample())), Ok(true));
        assert!(pane.selection.is_empty());
        assert!(pane.filter.is_empty());
        assert_eq!(pane.cursor, 1);
    }

    #[test]
    fn refresh_keeps_cursor_selection_and_filter_and_drops_vanished_names() {
        let mut pane = listed(sample());
        pane.toggle(4);
        pane.toggle(5);
        pane.edit_filter(TextEdit::Insert('.'), PAGE);
        assert_eq!(names(&pane), ["A.txt", "b.txt", "c.log"]);
        pane.set_cursor(2, PAGE);

        let location = pane.begin_refresh(RequestId(2)).expect("listed");
        assert_eq!(location.path, PathBuf::from("/home/u"));
        // The user moves while the refresh is in flight.
        pane.set_cursor(3, PAGE);
        let mut fresh = sample();
        fresh.retain(|e| e.name != "b.txt");
        fresh.push(file("0new.txt", 1, 1));
        assert_eq!(pane.finish_list(RequestId(2), Ok(fresh)), Ok(true));

        assert_eq!(pane.filter, ".");
        assert_eq!(names(&pane), ["0new.txt", "A.txt", "c.log"]);
        assert_eq!(cursor_name(&pane), "c.log");
        let selected: Vec<_> = pane.selection.iter().cloned().collect();
        assert_eq!(selected, ["c.log"]);
        assert_invariants(&pane, PAGE);
    }

    #[test]
    fn refresh_keeps_the_position_when_the_cursor_entry_vanished() {
        let mut pane = listed(sample());
        pane.set_cursor(5, PAGE);
        pane.begin_refresh(RequestId(2)).expect("listed");
        let mut fresh = sample();
        fresh.retain(|e| e.name != "c.log");
        assert_eq!(pane.finish_list(RequestId(2), Ok(fresh)), Ok(true));
        assert_eq!(cursor_name(&pane), "b.txt");
    }

    #[test]
    fn refresh_needs_a_listing() {
        let mut pane = Pane::new(Endpoint::Local, false);
        assert_eq!(pane.begin_refresh(RequestId(1)), None);
        assert_eq!(pane.pending, None);
    }

    #[test]
    fn stale_and_failed_results() {
        let mut pane = listed(sample());
        pane.begin_list(RequestId(2), "/a".into(), None);
        pane.begin_list(RequestId(3), "/b".into(), None);
        assert_eq!(pane.finish_list(RequestId(2), Ok(Vec::new())), Ok(false));
        assert_eq!(pane.visible.len(), 5);

        let error = FsError::PermissionDenied { path: "/b".into() };
        assert_eq!(
            pane.finish_list(RequestId(3), Err(error.clone())),
            Err(error)
        );
        assert_eq!(pane.cwd, Some(PathBuf::from("/home/u")));
        assert_eq!(pane.visible.len(), 5);
        assert_eq!(pane.pending, None);
        // A late duplicate of a finished request is stale.
        assert_eq!(pane.finish_list(RequestId(3), Ok(Vec::new())), Ok(false));
    }

    #[test]
    fn set_endpoint_keeps_only_sort_and_hidden() {
        let mut pane = listed(sample());
        pane.set_sort(Sort::by(SortKey::Size), PAGE);
        pane.toggle_hidden(PAGE);
        pane.toggle(1);
        pane.begin_list(RequestId(5), "/x".into(), None);
        let remote = Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        };
        pane.set_endpoint(remote.clone());
        let mut expected = Pane::new(remote, true);
        expected.sort = pane.sort;
        expected.page = PAGE;
        assert_eq!(pane, expected);
        assert_eq!(pane.finish_list(RequestId(5), Ok(sample())), Ok(false));
    }

    #[test]
    fn parent_of_root_is_none() {
        let mut pane = Pane::new(Endpoint::Local, false);
        pane.begin_list(RequestId(1), "/".into(), None);
        assert_eq!(pane.finish_list(RequestId(1), Ok(sample())), Ok(true));
        assert_eq!(pane.parent(), None);
        assert_eq!(pane.path_of(0), Some(PathBuf::from("/adir")));
    }
}
