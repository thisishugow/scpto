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
//! - `cursor < visible.len()` unless `visible` is empty (then 0);
//!   `scroll <= cursor` and the cursor row stays within the last page height
//!   passed in.
//! - `selection` holds names (not indices, which shift under sort and
//!   filter) of entries in `entries`; a reload drops names that vanished.
//!   Hidden or filtered-out entries stay selected but operations only use
//!   selected entries that are visible.
//! - The filter is a case-insensitive subsequence match on the name; it is
//!   edited only at its end, so `TextEdit::{Left, Right, Home, End,
//!   DeleteForward}` do nothing.
//!
//! # Loading
//!
//! Navigation never changes `cwd` directly: it records a [`PendingList`]
//! and the caller emits `Effect::ListDir` for it. The old listing stays on
//! screen until `BgEvent::Listed` for the same `RequestId` arrives; success
//! replaces `cwd`, `entries`, clears the filter, and puts the cursor on
//! `reselect` when present (else the first row); failure keeps the old
//! listing and is reported by the app. Results with any other id are stale
//! and ignored. A refresh of the same directory keeps cursor (by name),
//! selection, and filter.
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
//!     /// Absolute path of a visible row.
//!     pub fn path_of(&self, index: usize) -> Option<PathBuf>;
//!
//!     // Reading.
//!     pub fn visible_len(&self) -> usize;
//!     pub fn visible_entry(&self, index: usize) -> Option<&Entry>;
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
//!     /// `s`: Name ascending -> Size descending -> Modified descending ->
//!     /// Name ascending. Keeps the cursor on the same entry.
//!     pub fn cycle_sort(&mut self, page: usize);
//!     /// `.`: keeps the cursor on the same entry when it stays visible.
//!     pub fn toggle_hidden(&mut self, page: usize);
//!     pub fn edit_filter(&mut self, edit: TextEdit, page: usize);
//!     pub fn clear_filter(&mut self, page: usize);
//! }
//! ```

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;

use crate::ui::action::RequestId;
use crate::vfs::{Endpoint, Entry};

/// What a pane sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortKey {
    /// File name.
    Name,
    /// Size in bytes (directories sort as 0 among themselves by name).
    Size,
    /// Modification time.
    Modified,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortOrder {
    /// Smallest, oldest, or A first.
    Ascending,
    /// Largest, newest, or Z first.
    Descending,
}

/// A pane's sort setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort {
    /// What to sort by.
    pub key: SortKey,
    /// Which way.
    pub order: SortOrder,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            key: SortKey::Name,
            order: SortOrder::Ascending,
        }
    }
}

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
    /// Indices into `entries` of the rows shown, in display order.
    pub visible: Vec<usize>,
    /// Cursor position in `visible`.
    pub cursor: usize,
    /// Index in `visible` of the first row on screen.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
