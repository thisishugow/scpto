//! Core of the app state machine: construction, dispatch of every `Action`
//! and `BgEvent` to the feature modules, the two panes and which one has
//! focus, the terminal size and the layout derived from it, the input mode
//! reported to `ui/input.rs`, status messages, id allocation, and quit.
//!
//! Per-pane list behavior (cursor, selection, sort, filter, hidden files,
//! loading) is implemented by `ui::pane::Pane`; core only decides which pane
//! an action applies to, passes the page height, and turns pane navigation
//! into `Effect::ListDir` and `Effect::RememberDirectory`. Core does not
//! implement features: transfers, dialogs, connections, sync, edit, and
//! preview handle their own actions and events in their modules, and core
//! routes to them by input mode and event kind as described in "Routing"
//! below.
//!
//! # Behavior
//!
//! - Focus: Tab goes Left -> Right -> queue panel (only while it is visible
//!   as a panel, not collapsed to its summary row) -> Left. In the queue, Tab and Esc leave it; clicking a pane focuses
//!   that pane. Hiding the queue panel while it has focus returns focus to
//!   the pane focused before.
//! - Filter: `/` starts typing a filter in the focused pane
//!   (`InputMode::Filter`); Enter keeps the filter, Esc clears it, and any
//!   action other than text, cursor moves, Enter, and Esc stops typing
//!   (keeping the filter) and is handled as in browse mode. Esc in browse
//!   mode clears a kept filter.
//! - Navigation: Enter on an enterable row lists it; on the `..` row it is
//!   Parent; on a file it is the preview feature's. Parent lists the parent
//!   and reselects the directory left. A successful (non-refresh) listing emits `RememberDirectory`.
//!   A failed listing is reported in the status bar; when it was a pane's
//!   first listing (for example a remembered directory that no longer
//!   exists) and the endpoint's home is known, the pane lists its home
//!   instead.
//! - Mouse: click focuses and moves the cursor; with the toggle modifier it
//!   toggles that row (and moves the cursor there, so a later range starts
//!   from it); with the range modifier it selects from the cursor to the
//!   row. Double click opens. The wheel scrolls the pane under the pointer.
//!   A click on a pane's endpoint button (`ClickTarget::PaneTitle`) focuses
//!   it and opens the host picker for it (`connection_open_picker`), like
//!   `o`. Right clicks never reach the app.
//! - Sorting: `SortBy(key)` sorts the focused pane by `key` in
//!   its natural order (`state::Sort::by`) unless it already sorts by it;
//!   `ReverseSort` flips the order; a click on a column header
//!   (`ClickTarget::ColumnHeader`, also a double click, so quick clicks
//!   keep flipping) focuses the pane and reverses the order when `key` is
//!   the current key, else sorts by it. The cursor stays on the same entry.
//!   Every change emits `Effect::RememberSort` for the pane's endpoint (and
//!   updates `App::persisted`); a pane shown on an endpoint with a
//!   remembered sort (`App::new`, `switch_endpoint`) takes that sort, else
//!   it keeps its own.
//! - Menu answers: `Answer::Menu(action)` is handled with `handle_action`,
//!   exactly as if the item's key had been pressed.
//! - `B` bookmarks the focused pane's directory (`SaveBookmark`) unless it
//!   is already bookmarked; `R` reveals the cursor entry; `t` opens a shell
//!   in the focused pane's directory; `q` quits.
//! - Status messages stay until replaced.
//! - Layout (`CoreState::layout`): below 60 columns one pane is
//!   drawn at a time (the focused one) under a tab row; Tab or a click on
//!   the other tab (`ClickTarget::PaneArea`) switches it. The queue panel
//!   takes at most a third of the rows and is one summary row below 24
//!   rows. The last row is the button bar and the row above it
//!   the status bar. Below 40x10 every action except `Quit` and `Resize` is ignored
//!   and the state is kept for when the terminal grows. A resize that hides
//!   the source pane of a drag ends the drag.
//!
//! Startup: the left pane lists its directory at once (a local path is
//! always absolute, a remote one waits for `BgEvent::Connected`). When the
//! right pane is not given it starts as `Local` in the same directory as a
//! local left pane, and the host picker opens on it (wiring point 1). A pane
//! given without a path opens the
//! endpoint's remembered last directory when one exists, else its home
//! (from `BgEvent::Connected`; a local pane without a path connects `Local`
//! to learn it). An endpoint is connected once, however many panes show it.
//! `ResolveHost` is emitted for every discovered alias.
//!
//! Endpoints no pane shows any more are not disconnected here: whether one
//! can be closed also depends on the transfer queue, so `Effect::Disconnect`
//! is the connection feature's decision.
//!
//! # Public API
//!
//! ```text
//! impl App {
//!     /// Builds the app and the effects of the first frame (connects,
//!     /// listings, `ResolveHost` for the picker's aliases). `left` and
//!     /// `right` are an endpoint and an optional absolute start directory.
//!     pub fn new(
//!         settings: Settings,
//!         hosts: HostList,
//!         groups: Groups,
//!         state: PersistedState,
//!         background: Background,
//!         left: (Endpoint, Option<PathBuf>),
//!         right: Option<(Endpoint, Option<PathBuf>)>,
//!     ) -> (App, Vec<Effect>);
//!
//!     /// Handles one user action. Pure; returns the effects to execute.
//!     pub fn handle_action(&mut self, action: Action) -> Vec<Effect>;
//!     /// Handles one background result. Pure; returns follow-up effects.
//!     pub fn handle_event(&mut self, event: BgEvent) -> Vec<Effect>;
//!     /// How `ui/input.rs` must interpret the next key.
//!     pub fn input_mode(&self) -> InputMode;
//!
//!     /// For features (host picker, bookmark jump, sudo): shows `endpoint`
//!     /// in a pane, at `dir` when given, else at the remembered last
//!     /// directory or home; connects when needed. On the pane's current
//!     /// endpoint it only lists `dir` (when given).
//!     pub fn switch_endpoint(&mut self, side: PaneSide, endpoint: Endpoint, dir: Option<PathBuf>) -> Vec<Effect>;
//!     /// For features (after file operations): re-lists a pane's
//!     /// directory keeping cursor, selection, and filter.
//!     pub fn refresh(&mut self, side: PaneSide) -> Vec<Effect>;
//! }
//!
//! impl CoreState {
//!     pub fn pane(&self, side: PaneSide) -> &Pane;
//!     pub fn pane_mut(&mut self, side: PaneSide) -> &mut Pane;
//!     pub fn focused(&self) -> PaneSide;
//!     /// Whether the queue panel has focus (then `focused` is the pane
//!     /// focus returns to).
//!     pub fn queue_focused(&self) -> bool;
//!     /// Screen areas for the current size and panel visibility; render
//!     /// draws into exactly these so key paging and the screen agree.
//!     pub fn layout(&self) -> AppLayout;
//!     /// Whether the terminal is too small to draw anything but a message.
//!     pub fn too_small(&self) -> bool;
//!     /// Whether a pane's file list is on screen.
//!     pub fn pane_drawn(&self, side: PaneSide) -> bool;
//!     /// Focuses a pane (in single-pane mode, the pane drawn).
//!     pub fn focus(&mut self, side: PaneSide);
//!     /// List rows of a pane: its layout height minus
//!     /// `widgets::pane::CHROME_ROWS`.
//!     pub fn page_height(&self, side: PaneSide) -> usize;
//!     /// Allocators shared by every feature; ids are never reused.
//!     pub fn next_request(&mut self) -> RequestId;
//!     pub fn next_job(&mut self) -> JobId;
//!     pub fn next_timer(&mut self) -> TimerId;
//!     /// The status bar message.
//!     pub fn set_status(&mut self, message: StatusMessage);
//!     pub fn status(&self) -> Option<&StatusMessage>;
//! }
//!
//! pub struct AppLayout {
//!     pub left: Rect,
//!     pub right: Rect,
//!     /// Single-pane mode: the tab row.
//!     pub tabs: Option<Rect>,
//!     /// Where the quick view is drawn.
//!     pub quick_view: Rect,
//!     /// The queue panel when visible.
//!     pub queue: Option<Rect>,
//!     /// The queue panel is a one-row summary.
//!     pub queue_collapsed: bool,
//!     /// The row above the button bar.
//!     pub status_bar: Rect,
//!     /// The last row.
//!     pub button_bar: Rect,
//! }
//!
//! pub struct StatusMessage {
//!     pub text: String,
//!     /// Drawn in the error style (failures and configuration warnings).
//!     pub error: bool,
//! }
//!
//! impl StatusMessage {
//!     pub fn info(text: impl Into<String>) -> StatusMessage;
//!     pub fn error(text: impl Into<String>) -> StatusMessage;
//! }
//! ```
//!
//! Until the first `Action::Resize` the size is 0x0, so every page height is
//! 0 (which panes treat as 1); the runtime sends the initial size first.
//!
//! # Routing
//!
//! Each feature module documents its handlers in its "Public API" section.
//!
//! Feature action handlers share one shape: `fn <feature>_action(&mut self,
//! action: Action) -> Result<Vec<Effect>, Action>`; `Ok` claims the action,
//! `Err` hands it back unchanged for the next handler. Background events go
//! to one method per event kind (no decline; core routes by kind), except
//! `TimerFired`, which each timer-owning feature claims by id
//! (`Option<Vec<Effect>>`). Features never call each other: they read and
//! write each other's documented `pub` state fields, push dialogs onto
//! `DialogState::stack`, and leave answers in `DialogState::answer`, which
//! core routes.
//!
//! `input_mode`, first match wins:
//!
//! 1. `dialog_input_mode()` (`TextField` / `Dialog` for the top dialog,
//!    else `Picker` while a picker is open);
//! 2. `preview_input_mode()` (`Viewer`: help screen or full preview);
//! 3. `sync_input_mode()` (`SyncView`);
//! 4. `Queue` while the queue panel has focus;
//! 5. `Filter` while typing a filter;
//! 6. `Browse`.
//!
//! `handle_action` (after `Resize`, which stays core's):
//!
//! | Mode | Chain (first `Ok` wins; what nobody claims is dropped) |
//! |---|---|
//! | `TextField`, `Dialog`, `Picker` | `dialog_action` |
//! | `Viewer` | `preview_action` |
//! | `SyncView` | `sync_action` → `preview_action` (claims `Help`) |
//! | `Queue` | `transfer_action` → `preview_action` (claims `Help`) → core `queue_action` |
//! | `Filter` | core `filter_action`; `Paste` inserts its characters into the filter (`Pane::edit_filter`); an action that ends typing continues with the `Browse` chain |
//! | `Browse` | `transfer_action` → `dialog_action` → `connection_action` → `preview_action` → `edit_action` → `sync_action` → core `browse_action` |
//!
//! Inside core's own arms: `Quit` returns `transfer_quit_requested()` (2);
//! `Open` on an entry that is not enterable returns
//! `preview_open(side)`; every `Effect::Connect` core emits is followed by
//! `connection_connecting(&endpoint)`. After every action handled in
//! `Browse` or `Filter` mode (any chain member), core appends
//! `preview_cursor_moved()`. After every action, core takes
//! `self.dialog.answer` and routes it:
//!
//! | Answer | Handler |
//! |---|---|
//! | `Dialog` with `Conflict`, `TarConflict`, `FinderDrop` | `transfer_dialog_answer` |
//! | `Dialog` with `SudoPassword`, `Vpn` | `connection_dialog_answer` |
//! | `Dialog` with `ConfirmSync` | `sync_dialog_answer` |
//! | `Dialog` with `EditConflict`, `EditUploadFailed` | `edit_dialog_answer` |
//! | `Dialog` with `QuitWhileRunning` | core: `DialogButton::Yes` → `[Effect::Quit]` |
//! | `Picked { kind: PickerKind::Hosts { side }, target }` | `connection_picked(side, target)` |
//!
//! (The dialog feature handles `MakeDir`, `Rename`, `Chmod`, `Delete`,
//! `TrashFailed`, `Notice`, and bookmark picks itself and never leaves them
//! in `answer`.)
//!
//! `handle_event`:
//!
//! | Event | Handlers, in order |
//! |---|---|
//! | `Listed` | core `listed`, then `preview_cursor_moved` |
//! | `Connected` | core `connected`, then `connection_connected` |
//! | `ConnectFailed` | core (status, `connecting`), then `connection_failed` |
//! | `InteractiveAuthRequired` | `connection_interactive_auth` |
//! | `SudoPasswordRequired` | `connection_sudo_password_required` |
//! | `Disconnected` | core (status message), then `connection_disconnected` |
//! | `Reconnecting` | `connection_reconnecting` |
//! | `Reconnected` | core `refresh_endpoint`, then `connection_reconnected` |
//! | `HostResolved` | `connection_host_resolved` |
//! | `VpnFinished` | `connection_vpn_finished` |
//! | `FsOpFinished` | `dialog_fs_op_finished` |
//! | `DroppedPathsChecked` | `transfer_dropped_paths_checked` |
//! | `Transfer(event)` | `transfer_event`, then `sync_jobs_settled`, then `connection_release_unused` (3) |
//! | `Compared` | `sync_compared` |
//! | `SyncFinished` | `sync_finished`, then `connection_release_unused` |
//! | `PreviewLoaded` | `preview_loaded` |
//! | `ImageResized` | `preview_image_resized` |
//! | `EditFinished` | `edit_finished`, then `connection_release_unused` |
//! | `ShellExited` | core: an `Err` is an error status; `Ok` refreshes the focused pane (the shell may have changed files) |
//! | `Revealed` | core: an `Err` is an error status |
//! | `StateWriteFailed` | core (error status) |
//! | `TimerFired { id, at }` | `transfer_timer` (then `preview_cursor_moved`), else `connection_timer`, else `preview_timer` |
//!
//! Wiring points:
//!
//! 1. `App::new` ends with `connection_open_picker(PaneSide::Right)` when
//!    `right` is `None` (the host picker opens on the right pane at
//!    startup).
//! 2. `Quit` passes the transfer feature's check first:
//!    `transfer_quit_requested()` answers `[Effect::Quit]` or opens the
//!    confirmation.
//! 3. `Effect::Disconnect` is the connection feature's decision:
//!    `connection_release_unused()` runs after `switch_endpoint` and after
//!    the events marked above. For every endpoint it disconnects, core
//!    forgets that endpoint's entry in `homes` and `connecting`, so showing
//!    it again connects it again.
//! 4. Aliases that `Groups::is_excluded` excludes get no `ResolveHost` in
//!    `App::new` (and the picker leaves them out).
//! 5. Core's private `last_directory` / `remember_directory` helpers are
//!    replaced by `PersistedState::last_directory` /
//!    `PersistedState::remember_directory`, and `add_bookmark` uses
//!    `PersistedState::add_bookmark`.
//! 6. `Effect::Connect(Endpoint::Local)` is answered by the session
//!    executor (`Sessions::connect`) with `Connected { Local, home }`; core
//!    keeps emitting it for a local pane without a start directory.
//! 7. The runtime sends `Action::Resize` with the terminal size before the
//!    first frame is drawn.
//!
//! Startup also shows the warning of `state::load_state` (a corrupt or
//! unreadable `state.json`) with `CoreState::set_status` right after
//! `App::new`, unless the ssh configuration warning is already shown.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Display;
use std::path::PathBuf;

use ratatui::layout::Rect;

use super::dialog::{Answer, DialogPurpose, PickerKind};
use super::App;
use crate::error::FsError;
use crate::groups::Groups;
use crate::hosts::HostList;
use crate::preview::Background;
use crate::settings::Settings;
use crate::state::{PersistedState, Sort, SortKey};
use crate::transfer::model::JobId;
use crate::ui::action::{
    Action, BgEvent, ClickModifiers, ClickTarget, DialogButton, Effect, InputMode, PaneSide,
    RequestId, ScrollRegion, TextEdit, TimerId,
};
use crate::ui::pane::Pane;
use crate::ui::widgets::pane::CHROME_ROWS;
use crate::vfs::{Endpoint, Entry, Location};

/// Terminals narrower than this show one pane at a time.
pub const SINGLE_PANE_BELOW_WIDTH: u16 = 60;
/// The smallest terminal scpto draws its interface in; below it only a
/// "Terminal too small" message is drawn.
pub const MIN_WIDTH: u16 = 40;
/// See [`MIN_WIDTH`].
pub const MIN_HEIGHT: u16 = 10;
/// Terminals shorter than this collapse the queue panel to one summary row.
const QUEUE_COLLAPSE_BELOW_HEIGHT: u16 = 24;
/// The quick view is split off as the other pane only when both panes keep
/// at least this many columns; otherwise it covers the whole pane area.
const QUICK_VIEW_MIN_PANE_WIDTH: u16 = 40;

/// Screen areas of one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLayout {
    /// The left pane. In single-pane mode both panes have the same area and
    /// only the focused one is drawn.
    pub left: Rect,
    /// The right pane.
    pub right: Rect,
    /// Single-pane mode: the one-row tab header above the pane, showing both
    /// endpoints.
    pub tabs: Option<Rect>,
    /// Where the quick view is drawn while it shows an entry: the
    /// non-focused pane when both panes keep enough columns, else the area
    /// of every pane (in single-pane mode, the pane under the tabs).
    pub quick_view: Rect,
    /// The queue panel when visible.
    pub queue: Option<Rect>,
    /// The queue panel is a one-row summary (short terminals).
    pub queue_collapsed: bool,
    /// The status bar (the row above the button bar).
    pub status_bar: Rect,
    /// The button bar (the last row).
    pub button_bar: Rect,
}

/// A status bar message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    /// The text.
    pub text: String,
    /// Drawn in the error style (failures and configuration warnings).
    pub error: bool,
}

impl StatusMessage {
    /// An informational message.
    pub fn info(text: impl Into<String>) -> StatusMessage {
        StatusMessage {
            text: text.into(),
            error: false,
        }
    }

    /// An error message.
    pub fn error(text: impl Into<String>) -> StatusMessage {
        StatusMessage {
            text: text.into(),
            error: true,
        }
    }
}

/// State owned by this feature; part of [`super::App`].
#[derive(Debug)]
pub struct CoreState {
    left: Pane,
    right: Pane,
    focused: PaneSide,
    queue_focused: bool,
    queue_visible: bool,
    /// Typing a filter in the focused pane (`InputMode::Filter`).
    filtering: bool,
    width: u16,
    height: u16,
    /// The last reported size is below [`MIN_WIDTH`] x [`MIN_HEIGHT`].
    too_small: bool,
    status: Option<StatusMessage>,
    next_request: u64,
    next_job: u64,
    next_timer: u64,
    /// Directory each pane lists once its endpoint connects; `None` while
    /// waiting means the endpoint's home.
    start_left: Option<PathBuf>,
    start_right: Option<PathBuf>,
    /// Home directories of endpoints that answered `BgEvent::Connected`.
    homes: HashMap<Endpoint, PathBuf>,
    /// Endpoints with an `Effect::Connect` awaiting its answer.
    connecting: HashSet<Endpoint>,
}

impl CoreState {
    fn new(left: Endpoint, right: Endpoint, show_hidden: bool) -> CoreState {
        CoreState {
            left: Pane::new(left, show_hidden),
            right: Pane::new(right, show_hidden),
            focused: PaneSide::Left,
            queue_focused: false,
            queue_visible: false,
            filtering: false,
            width: 0,
            height: 0,
            too_small: false,
            status: None,
            next_request: 0,
            next_job: 0,
            next_timer: 0,
            start_left: None,
            start_right: None,
            homes: HashMap::new(),
            connecting: HashSet::new(),
        }
    }

    /// A pane.
    pub fn pane(&self, side: PaneSide) -> &Pane {
        match side {
            PaneSide::Left => &self.left,
            PaneSide::Right => &self.right,
        }
    }

    /// A pane, mutably.
    pub fn pane_mut(&mut self, side: PaneSide) -> &mut Pane {
        match side {
            PaneSide::Left => &mut self.left,
            PaneSide::Right => &mut self.right,
        }
    }

    /// The focused pane (the one focus returns to while the queue panel has
    /// focus).
    pub fn focused(&self) -> PaneSide {
        self.focused
    }

    /// Whether the queue panel has focus.
    pub fn queue_focused(&self) -> bool {
        self.queue_focused
    }

    /// Screen areas for the current size and panel visibility; render draws
    /// into exactly these so key paging and the screen agree. The
    /// button bar is the last row and the status bar the row above it; the
    /// queue panel, when visible, takes the
    /// bottom third of the rest, or one summary row on terminals shorter
    /// than 24 rows; the panes share the remaining rows, side by side, or,
    /// below 60 columns, one at a time under a tab row.
    pub fn layout(&self) -> AppLayout {
        let bar_height = self.height.min(1);
        let status_height = (self.height - bar_height).min(1);
        let body = self.height - bar_height - status_height;
        let queue_collapsed = self.queue_collapsed();
        let queue_height = match (self.queue_visible, queue_collapsed) {
            (false, _) => 0,
            (true, true) => body.min(1),
            (true, false) => body / 3,
        };
        let panes_height = body - queue_height;
        let (left, right, tabs) = if self.single_pane() {
            let tabs_height = panes_height.min(1);
            let pane = Rect::new(0, tabs_height, self.width, panes_height - tabs_height);
            (pane, pane, Some(Rect::new(0, 0, self.width, tabs_height)))
        } else {
            let left_width = self.width / 2;
            (
                Rect::new(0, 0, left_width, panes_height),
                Rect::new(left_width, 0, self.width - left_width, panes_height),
                None,
            )
        };
        let quick_view = if left.width >= QUICK_VIEW_MIN_PANE_WIDTH
            && right.width >= QUICK_VIEW_MIN_PANE_WIDTH
            && tabs.is_none()
        {
            match self.focused.other() {
                PaneSide::Left => left,
                PaneSide::Right => right,
            }
        } else {
            left.union(right)
        };
        AppLayout {
            left,
            right,
            tabs,
            quick_view,
            queue: self
                .queue_visible
                .then(|| Rect::new(0, panes_height, self.width, queue_height)),
            queue_collapsed,
            status_bar: Rect::new(0, body, self.width, status_height),
            button_bar: Rect::new(0, body + status_height, self.width, bar_height),
        }
    }

    /// Single-pane mode: the terminal is narrower than
    /// [`SINGLE_PANE_BELOW_WIDTH`], so only the focused pane is drawn.
    fn single_pane(&self) -> bool {
        self.width < SINGLE_PANE_BELOW_WIDTH
    }

    /// The last reported terminal size is below [`MIN_WIDTH`] x
    /// [`MIN_HEIGHT`]: only a "too small" message is drawn and input other
    /// than `Quit` and `Resize` is ignored.
    pub fn too_small(&self) -> bool {
        self.too_small
    }

    /// Whether a pane's file list is on screen in the current layout (both
    /// are side by side; in single-pane mode only the focused one). The
    /// quick view covering a pane does not count.
    pub fn pane_drawn(&self, side: PaneSide) -> bool {
        !self.too_small && (!self.single_pane() || side == self.focused)
    }

    fn queue_collapsed(&self) -> bool {
        self.height < QUEUE_COLLAPSE_BELOW_HEIGHT
    }

    /// List rows of a pane: its layout height minus the pane chrome.
    pub fn page_height(&self, side: PaneSide) -> usize {
        let layout = self.layout();
        let area = match side {
            PaneSide::Left => layout.left,
            PaneSide::Right => layout.right,
        };
        usize::from(area.height.saturating_sub(CHROME_ROWS))
    }

    /// A new request id; ids are never reused.
    pub fn next_request(&mut self) -> RequestId {
        self.next_request += 1;
        RequestId(self.next_request)
    }

    /// A new job id; ids are never reused.
    pub fn next_job(&mut self) -> JobId {
        self.next_job += 1;
        JobId(self.next_job)
    }

    /// A new timer id; ids are never reused.
    pub fn next_timer(&mut self) -> TimerId {
        self.next_timer += 1;
        TimerId(self.next_timer)
    }

    /// Replaces the status bar message.
    pub fn set_status(&mut self, message: StatusMessage) {
        self.status = Some(message);
    }

    /// The status bar message.
    pub fn status(&self) -> Option<&StatusMessage> {
        self.status.as_ref()
    }

    fn start_dir_mut(&mut self, side: PaneSide) -> &mut Option<PathBuf> {
        match side {
            PaneSide::Left => &mut self.start_left,
            PaneSide::Right => &mut self.start_right,
        }
    }

    /// Focuses a pane, leaving the queue panel. In single-pane mode this is
    /// also the pane drawn (for example after a drag hovered over its tab).
    pub fn focus(&mut self, side: PaneSide) {
        self.focused = side;
        self.queue_focused = false;
    }

    /// Tab: Left -> Right -> queue panel (only while it is drawn as a
    /// panel, not as the collapsed summary row) -> Left.
    fn switch_focus(&mut self) {
        let queue_focusable = self.queue_visible && !self.queue_collapsed();
        match (self.focused, self.queue_focused) {
            (_, true) => self.focus(PaneSide::Left),
            (PaneSide::Left, false) => self.focused = PaneSide::Right,
            (PaneSide::Right, false) if queue_focusable => self.queue_focused = true,
            (PaneSide::Right, false) => self.focused = PaneSide::Left,
        }
    }

    /// Takes a new terminal size: re-derives the too-small state, leaves a
    /// queue panel that collapsed to its summary row, and keeps every
    /// cursor on screen.
    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.too_small = width < MIN_WIDTH || height < MIN_HEIGHT;
        if self.queue_collapsed() {
            self.queue_focused = false;
        }
        self.reclamp_panes();
    }

    fn toggle_queue(&mut self) {
        self.queue_visible = !self.queue_visible;
        if !self.queue_visible {
            self.queue_focused = false;
        }
    }

    /// Re-applies the cursor and scroll invariants after a size change.
    fn reclamp_panes(&mut self) {
        for side in [PaneSide::Left, PaneSide::Right] {
            let page = self.page_height(side);
            let pane = self.pane_mut(side);
            pane.set_cursor(pane.cursor, page);
        }
    }
}

impl App {
    /// Builds the app and the effects of the first frame (connects,
    /// listings, `ResolveHost` for the picker's aliases). `left` and `right`
    /// are an endpoint and an optional absolute start directory.
    pub fn new(
        settings: Settings,
        hosts: HostList,
        groups: Groups,
        state: PersistedState,
        background: Background,
        left: (Endpoint, Option<PathBuf>),
        right: Option<(Endpoint, Option<PathBuf>)>,
    ) -> (App, Vec<Effect>) {
        let (left_endpoint, left_dir) = left;
        let open_picker = right.is_none();
        let (right_endpoint, right_dir) = right.unwrap_or_else(|| {
            let dir = left_dir.clone().filter(|_| left_endpoint.is_local());
            (Endpoint::Local, dir)
        });
        let mut core = CoreState::new(left_endpoint, right_endpoint, settings.show_hidden);
        for side in [PaneSide::Left, PaneSide::Right] {
            let pane = core.pane_mut(side);
            if let Some(sort) = state.last_sort(&pane.endpoint) {
                pane.set_sort(sort, 1);
            }
        }
        let mut app = App {
            core,
            connection: Default::default(),
            transfer: Default::default(),
            dialog: Default::default(),
            sync: Default::default(),
            edit: Default::default(),
            preview: Default::default(),
            settings,
            hosts,
            groups,
            persisted: state,
            background,
        };
        if let Some(first) = app.hosts.warnings.first() {
            let more = app.hosts.warnings.len() - 1;
            let text = if more == 0 {
                format!("ssh config: {first}")
            } else {
                format!("ssh config: {first} (and {more} more)")
            };
            app.core.set_status(StatusMessage::error(text));
        }
        let mut effects = app.start_pane(PaneSide::Left, left_dir);
        effects.extend(app.start_pane(PaneSide::Right, right_dir));
        let resolve: Vec<Effect> = app
            .hosts
            .aliases
            .iter()
            .filter(|alias| !app.groups.is_excluded(alias))
            .cloned()
            .map(Effect::ResolveHost)
            .collect();
        effects.extend(resolve);
        if open_picker {
            effects.extend(app.connection_open_picker(PaneSide::Right));
        }
        (app, effects)
    }

    /// How `ui/input.rs` must interpret the next key.
    pub fn input_mode(&self) -> InputMode {
        if let Some(mode) = self
            .dialog_input_mode()
            .or_else(|| self.preview_input_mode())
            .or_else(|| self.sync_input_mode())
        {
            mode
        } else if self.core.queue_focused {
            InputMode::Queue
        } else if self.core.filtering {
            InputMode::Filter
        } else {
            InputMode::Browse
        }
    }

    /// Handles one user action. Pure; returns the effects to execute.
    pub fn handle_action(&mut self, action: Action) -> Vec<Effect> {
        if let Action::Resize { width, height } = action {
            // A drag whose source pane disappears with the new layout ends.
            let source_was_drawn = self
                .transfer
                .drag
                .as_ref()
                .is_some_and(|drag| self.core.pane_drawn(drag.side));
            self.core.resize(width, height);
            if let Some(drag) = &self.transfer.drag {
                if source_was_drawn && !self.core.pane_drawn(drag.side) {
                    self.transfer.drag = None;
                }
            }
            return Vec::new();
        }
        // A too-small terminal draws only a message, so nothing
        // but quitting may act on the hidden state.
        if self.core.too_small && !matches!(action, Action::Quit) {
            return Vec::new();
        }
        let mut effects = match self.input_mode() {
            InputMode::Browse => {
                let mut effects = self.browse_chain(action);
                effects.extend(self.preview_cursor_moved());
                effects
            }
            InputMode::Filter => {
                let mut effects = self.filter_action(action);
                effects.extend(self.preview_cursor_moved());
                effects
            }
            InputMode::Queue => self
                .transfer_action(action)
                .or_else(|action| self.preview_action(action))
                .unwrap_or_else(|action| self.queue_action(action)),
            InputMode::TextField | InputMode::Dialog | InputMode::Picker => {
                self.dialog_action(action).unwrap_or_default()
            }
            InputMode::Viewer => self.preview_action(action).unwrap_or_default(),
            InputMode::SyncView => self
                .sync_action(action)
                .or_else(|action| self.preview_action(action))
                .unwrap_or_default(),
        };
        effects.extend(self.route_answer());
        self.forget_disconnected(&effects);
        effects
    }

    /// Handles one background result. Pure; returns follow-up effects.
    pub fn handle_event(&mut self, event: BgEvent) -> Vec<Effect> {
        let effects = self.route_event(event);
        self.forget_disconnected(&effects);
        effects
    }

    /// For features (host picker, bookmark jump, sudo): shows `endpoint` in
    /// a pane, at `dir` when given, else at the remembered last directory or
    /// home; connects when needed. On the pane's current endpoint it only
    /// lists `dir` (when given).
    pub fn switch_endpoint(
        &mut self,
        side: PaneSide,
        endpoint: Endpoint,
        dir: Option<PathBuf>,
    ) -> Vec<Effect> {
        if self.core.pane(side).endpoint == endpoint {
            return dir
                .map(|path| self.list(side, path, None))
                .into_iter()
                .collect();
        }
        if self.core.focused == side {
            self.core.filtering = false;
        }
        let remembered = self.persisted.last_sort(&endpoint);
        let page = self.core.page_height(side);
        let pane = self.core.pane_mut(side);
        pane.set_endpoint(endpoint);
        if let Some(sort) = remembered {
            pane.set_sort(sort, page);
        }
        let mut effects = self.start_pane(side, dir);
        effects.extend(self.connection_release_unused());
        effects
    }

    /// For features (after file operations): re-lists a pane's directory
    /// keeping cursor, selection, and filter.
    pub fn refresh(&mut self, side: PaneSide) -> Vec<Effect> {
        let request = self.core.next_request();
        self.core
            .pane_mut(side)
            .begin_refresh(request)
            .map(|location| Effect::ListDir { request, location })
            .into_iter()
            .collect()
    }

    // Routing.

    /// The `Browse` chain: every feature in turn, then core.
    fn browse_chain(&mut self, action: Action) -> Vec<Effect> {
        self.transfer_action(action)
            .or_else(|action| self.dialog_action(action))
            .or_else(|action| self.connection_action(action))
            .or_else(|action| self.preview_action(action))
            .or_else(|action| self.edit_action(action))
            .or_else(|action| self.sync_action(action))
            .unwrap_or_else(|action| self.browse_action(action))
    }

    /// Routes the answer a dialog or picker left in `DialogState::answer`
    /// to the feature that owns it.
    fn route_answer(&mut self) -> Vec<Effect> {
        let Some(answer) = self.dialog.answer.take() else {
            return Vec::new();
        };
        match answer {
            Answer::Dialog(answer) => match answer.purpose {
                DialogPurpose::Conflict { .. }
                | DialogPurpose::TarConflict { .. }
                | DialogPurpose::FinderDrop { .. } => self.transfer_dialog_answer(answer),
                DialogPurpose::SudoPassword { .. } | DialogPurpose::Vpn { .. } => {
                    self.connection_dialog_answer(answer)
                }
                DialogPurpose::ConfirmSync { .. } => self.sync_dialog_answer(answer),
                DialogPurpose::EditConflict { .. } | DialogPurpose::EditUploadFailed { .. } => {
                    self.edit_dialog_answer(answer)
                }
                DialogPurpose::QuitWhileRunning => match answer.button {
                    DialogButton::Yes => vec![Effect::Quit],
                    _ => Vec::new(),
                },
                // The dialog feature answers its own purposes and never
                // leaves them here.
                DialogPurpose::MakeDir { .. }
                | DialogPurpose::Rename { .. }
                | DialogPurpose::Chmod { .. }
                | DialogPurpose::Delete { .. }
                | DialogPurpose::TrashFailed { .. }
                | DialogPurpose::Notice => Vec::new(),
            },
            Answer::Picked {
                kind: PickerKind::Hosts { side },
                target,
            } => self.connection_picked(side, target),
            // Bookmark picks are the dialog feature's own.
            Answer::Picked {
                kind: PickerKind::Bookmarks { .. },
                ..
            } => Vec::new(),
            Answer::Menu(action) => self.handle_action(action),
        }
    }

    /// Wiring point 3: an endpoint the connection feature disconnects is
    /// forgotten, so showing it again connects it again.
    fn forget_disconnected(&mut self, effects: &[Effect]) {
        for effect in effects {
            if let Effect::Disconnect(endpoint) = effect {
                self.core.homes.remove(endpoint);
                self.core.connecting.remove(endpoint);
            }
        }
    }

    fn route_event(&mut self, event: BgEvent) -> Vec<Effect> {
        match event {
            BgEvent::Listed {
                request, result, ..
            } => {
                let mut effects = self.listed(request, result);
                effects.extend(self.preview_cursor_moved());
                effects
            }
            BgEvent::Connected { endpoint, home } => {
                let mut effects = self.connected(endpoint.clone(), home);
                effects.extend(self.connection_connected(&endpoint));
                effects
            }
            BgEvent::ConnectFailed { endpoint, error } => {
                self.core.connecting.remove(&endpoint);
                self.error_status(format!("{endpoint}: {error}"));
                self.connection_failed(&endpoint, &error)
            }
            BgEvent::InteractiveAuthRequired { endpoint, error } => {
                self.connection_interactive_auth(endpoint, error)
            }
            BgEvent::SudoPasswordRequired(endpoint) => {
                self.connection_sudo_password_required(endpoint)
            }
            BgEvent::Disconnected { endpoint, error } => {
                self.error_status(format!("Lost connection to {endpoint}: {error}"));
                self.connection_disconnected(&endpoint, &error)
            }
            BgEvent::Reconnecting {
                endpoint,
                attempt,
                delay,
                error,
            } => self.connection_reconnecting(endpoint, attempt, delay, error),
            BgEvent::Reconnected(endpoint) => {
                let mut effects = self.refresh_endpoint(&endpoint);
                effects.extend(self.connection_reconnected(&endpoint));
                effects
            }
            BgEvent::HostResolved { alias, result } => self.connection_host_resolved(alias, result),
            BgEvent::VpnFinished { endpoint, result } => {
                self.connection_vpn_finished(endpoint, result)
            }
            BgEvent::FsOpFinished {
                request,
                endpoint,
                op,
                failures,
            } => self.dialog_fs_op_finished(request, endpoint, op, failures),
            BgEvent::DroppedPathsChecked {
                text,
                paths,
                all_exist,
            } => self.transfer_dropped_paths_checked(text, paths, all_exist),
            BgEvent::TransferCommandFailed(error) => {
                self.error_status(error);
                Vec::new()
            }
            BgEvent::TransferSubmitted { job, result } => {
                self.transfer_submitted(job, result);
                self.connection_release_unused()
            }
            BgEvent::Transfer(event) => {
                let mut effects = self.transfer_event(event);
                self.sync_jobs_settled();
                effects.extend(self.connection_release_unused());
                effects
            }
            BgEvent::Compared { request, result } => self.sync_compared(request, result),
            BgEvent::SyncFinished { job, result } => {
                let mut effects = self.sync_finished(job, result);
                effects.extend(self.connection_release_unused());
                effects
            }
            BgEvent::PreviewLoaded {
                request,
                location,
                result,
            } => self.preview_loaded(request, location, result),
            BgEvent::ImageResized(resized) => {
                self.preview_image_resized(resized);
                Vec::new()
            }
            BgEvent::EditFinished { location, result } => {
                let mut effects = self.edit_finished(location, result);
                effects.extend(self.connection_release_unused());
                effects
            }
            BgEvent::ShellExited { result } => match result {
                Err(error) => {
                    self.error_status(error);
                    Vec::new()
                }
                // The shell may have changed files.
                Ok(()) => self.refresh(self.core.focused),
            },
            BgEvent::Revealed { result, .. } => {
                if let Err(error) = result {
                    self.error_status(error);
                }
                Vec::new()
            }
            BgEvent::StateWriteFailed(error) => {
                self.error_status(error);
                Vec::new()
            }
            BgEvent::TimerFired { id, at } => match self.transfer_timer(id) {
                // The drag switched panes: the quick view follows the cursor
                // of the newly focused pane.
                Some(mut effects) => {
                    effects.extend(self.preview_cursor_moved());
                    effects
                }
                None => self
                    .connection_timer(id, at)
                    .or_else(|| self.preview_timer(id, at))
                    .unwrap_or_default(),
            },
        }
    }

    // Actions.

    fn browse_action(&mut self, action: Action) -> Vec<Effect> {
        let side = self.core.focused;
        let page = self.core.page_height(side);
        match action {
            Action::SwitchFocus => self.core.switch_focus(),
            Action::MoveCursor(movement) => self.core.pane_mut(side).move_cursor(movement, page),
            Action::Open => return self.open(side),
            Action::Parent => return self.parent(side),
            Action::Refresh => return self.refresh(side),
            Action::ToggleSelect => self.core.pane_mut(side).toggle_and_advance(page),
            Action::SelectAll => self.core.pane_mut(side).select_all(),
            Action::InvertSelection => self.core.pane_mut(side).invert_selection(),
            Action::StartFilter => self.core.filtering = true,
            Action::ToggleHidden => self.core.pane_mut(side).toggle_hidden(page),
            Action::SortBy(key) => {
                if self.core.pane(side).sort.key != key {
                    return self.set_sort(side, Sort::by(key));
                }
            }
            Action::ReverseSort => {
                let sort = self.core.pane(side).sort.reversed();
                return self.set_sort(side, sort);
            }
            Action::ToggleQueue => self.core.toggle_queue(),
            Action::Cancel => self.core.pane_mut(side).clear_filter(page),
            Action::AddBookmark => return self.add_bookmark(side),
            Action::Reveal => return self.reveal(side),
            Action::OpenShell => {
                return self
                    .core
                    .pane(side)
                    .location()
                    .map(Effect::OpenShell)
                    .into_iter()
                    .collect();
            }
            Action::Quit => return self.transfer_quit_requested(),
            Action::Click {
                target, modifiers, ..
            } => return self.click(target, modifiers),
            Action::DoubleClick { target, .. } => return self.double_click(target),
            Action::Scroll { region, lines } => self.scroll(region, lines),
            // Owned by the transfer, dialog, preview, edit, connection, and
            // sync features.
            Action::Copy
            | Action::Move
            | Action::MakeDir
            | Action::Delete
            | Action::Rename
            | Action::Edit
            | Action::Preview
            | Action::ToggleQuickView
            | Action::ToggleMarkdown
            | Action::Chmod
            | Action::SortMenu
            | Action::ContextMenu(_)
            | Action::Help
            | Action::OpenEndpointPicker
            | Action::ReconnectWithSudo
            | Action::ShowBookmarks
            | Action::Compare
            | Action::StartSync(_)
            | Action::Queue { .. }
            | Action::Paste(_)
            | Action::DragStart { .. }
            | Action::DragMove { .. }
            | Action::Drop { .. }
            | Action::DragCancel => {}
            // Meaningful only in dialogs, pickers, and filters.
            Action::Text(_)
            | Action::Confirm
            | Action::DialogFocus(_)
            | Action::ToggleOption
            | Action::DialogHotkey(_)
            | Action::PickerSelectTab(_)
            | Action::PickerRemove => {}
            Action::Resize { .. } => {}
        }
        Vec::new()
    }

    fn filter_action(&mut self, action: Action) -> Vec<Effect> {
        let side = self.core.focused;
        let page = self.core.page_height(side);
        match action {
            Action::Text(edit) => self.core.pane_mut(side).edit_filter(edit, page),
            Action::Paste(text) => {
                let pane = self.core.pane_mut(side);
                for c in text.chars().filter(|c| !c.is_control()) {
                    pane.edit_filter(TextEdit::Insert(c), page);
                }
            }
            Action::MoveCursor(movement) => self.core.pane_mut(side).move_cursor(movement, page),
            Action::Confirm => self.core.filtering = false,
            Action::Cancel => {
                self.core.filtering = false;
                self.core.pane_mut(side).clear_filter(page);
            }
            other => {
                self.core.filtering = false;
                return self.browse_chain(other);
            }
        }
        Vec::new()
    }

    fn queue_action(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::SwitchFocus => self.core.switch_focus(),
            Action::Cancel => self.core.queue_focused = false,
            Action::ToggleQueue
            | Action::Quit
            | Action::Scroll { .. }
            | Action::Click {
                target:
                    ClickTarget::PaneRow { .. }
                    | ClickTarget::PaneArea(_)
                    | ClickTarget::PaneTitle(_)
                    | ClickTarget::ColumnHeader { .. },
                ..
            }
            | Action::DoubleClick {
                target:
                    ClickTarget::PaneRow { .. }
                    | ClickTarget::PaneArea(_)
                    | ClickTarget::PaneTitle(_)
                    | ClickTarget::ColumnHeader { .. },
                ..
            } => return self.browse_action(action),
            // Option/Alt+click on a pane row leaves the queue like a click.
            Action::ContextMenu(Some(_)) => return self.browse_chain(action),
            // Queue navigation and controls are the transfer feature's.
            _ => {}
        }
        Vec::new()
    }

    fn click(&mut self, target: ClickTarget, modifiers: ClickModifiers) -> Vec<Effect> {
        match target {
            ClickTarget::PaneRow { side, index } => {
                self.core.focus(side);
                let page = self.core.page_height(side);
                let pane = self.core.pane_mut(side);
                if modifiers.range {
                    pane.select_range(index, page);
                } else {
                    if modifiers.toggle {
                        pane.toggle(index);
                    }
                    pane.set_cursor(index, page);
                }
            }
            ClickTarget::PaneArea(side) => self.core.focus(side),
            ClickTarget::PaneTitle(side) => {
                self.core.focus(side);
                return self.connection_open_picker(side);
            }
            ClickTarget::ColumnHeader { side, key } => return self.header_click(side, key),
            // Rows and buttons of the queue, dialogs, menus, pickers, and
            // the comparison view belong to their features.
            ClickTarget::QueueRow(_)
            | ClickTarget::DialogButton(_)
            | ClickTarget::MenuRow(_)
            | ClickTarget::PickerRow(_)
            | ClickTarget::PickerTab(_)
            | ClickTarget::SyncRow(_) => {}
        }
        Vec::new()
    }

    /// A click on a column header: reverses the order of the current key,
    /// else sorts by the clicked one.
    fn header_click(&mut self, side: PaneSide, key: SortKey) -> Vec<Effect> {
        self.core.focus(side);
        let current = self.core.pane(side).sort;
        let sort = if current.key == key {
            current.reversed()
        } else {
            Sort::by(key)
        };
        self.set_sort(side, sort)
    }

    /// Re-sorts a pane (keeping the cursor entry) and remembers the sort
    /// for its endpoint.
    fn set_sort(&mut self, side: PaneSide, sort: Sort) -> Vec<Effect> {
        let page = self.core.page_height(side);
        let pane = self.core.pane_mut(side);
        pane.set_sort(sort, page);
        let endpoint = pane.endpoint.clone();
        self.persisted.remember_sort(endpoint.clone(), sort);
        vec![Effect::RememberSort { endpoint, sort }]
    }

    fn double_click(&mut self, target: ClickTarget) -> Vec<Effect> {
        match target {
            ClickTarget::PaneRow { side, index } => {
                self.core.focus(side);
                let page = self.core.page_height(side);
                self.core.pane_mut(side).set_cursor(index, page);
                self.open(side)
            }
            ClickTarget::PaneArea(side) => {
                self.core.focus(side);
                Vec::new()
            }
            // Quick clicks on a header keep flipping the order.
            ClickTarget::ColumnHeader { side, key } => self.header_click(side, key),
            // The first click already opened the picker.
            ClickTarget::PaneTitle(_)
            | ClickTarget::QueueRow(_)
            | ClickTarget::DialogButton(_)
            | ClickTarget::MenuRow(_)
            | ClickTarget::PickerRow(_)
            | ClickTarget::PickerTab(_)
            | ClickTarget::SyncRow(_) => Vec::new(),
        }
    }

    fn scroll(&mut self, region: ScrollRegion, lines: i32) {
        match region {
            ScrollRegion::Pane(side) => {
                let page = self.core.page_height(side);
                self.core.pane_mut(side).scroll_by(lines, page);
            }
            ScrollRegion::Queue
            | ScrollRegion::Preview
            | ScrollRegion::Help
            | ScrollRegion::Picker
            | ScrollRegion::SyncView => {}
        }
    }

    /// Enters the cursor row when it is enterable (the `..` row goes to the
    /// parent), else previews it.
    fn open(&mut self, side: PaneSide) -> Vec<Effect> {
        let pane = self.core.pane(side);
        if pane.is_parent_row(pane.cursor) {
            return self.parent(side);
        }
        match pane.cursor_entry() {
            None => return Vec::new(),
            Some(entry) if !entry.is_enterable() => return self.preview_open(side),
            Some(_) => {}
        }
        match pane.path_of(pane.cursor) {
            Some(path) => vec![self.list(side, path, None)],
            None => Vec::new(),
        }
    }

    fn parent(&mut self, side: PaneSide) -> Vec<Effect> {
        match self.core.pane(side).parent() {
            Some((path, name)) => vec![self.list(side, path, Some(name))],
            None => Vec::new(),
        }
    }

    fn add_bookmark(&mut self, side: PaneSide) -> Vec<Effect> {
        let Some(location) = self.core.pane(side).location() else {
            return Vec::new();
        };
        let label = describe(&location);
        if !self.persisted.add_bookmark(location.clone()) {
            self.core
                .set_status(StatusMessage::info(format!("Already bookmarked: {label}")));
            return Vec::new();
        }
        self.core
            .set_status(StatusMessage::info(format!("Bookmarked {label}")));
        vec![Effect::SaveBookmark(location)]
    }

    fn reveal(&mut self, side: PaneSide) -> Vec<Effect> {
        let pane = self.core.pane(side);
        pane.path_of(pane.cursor)
            .map(|path| {
                Effect::Reveal(Location {
                    endpoint: pane.endpoint.clone(),
                    path,
                })
            })
            .into_iter()
            .collect()
    }

    // Loading.

    fn list(&mut self, side: PaneSide, path: PathBuf, reselect: Option<OsString>) -> Effect {
        let request = self.core.next_request();
        let location = self.core.pane_mut(side).begin_list(request, path, reselect);
        Effect::ListDir { request, location }
    }

    /// Lists a pane that was just given its endpoint, or records where to
    /// start and connects the endpoint when its home is needed or it is a
    /// remote that is not connected yet.
    fn start_pane(&mut self, side: PaneSide, dir: Option<PathBuf>) -> Vec<Effect> {
        let endpoint = self.core.pane(side).endpoint.clone();
        let start = dir.or_else(|| self.persisted.last_directory(&endpoint).map(PathBuf::from));
        let home = self.core.homes.get(&endpoint).cloned();
        let ready = endpoint.is_local() || home.is_some();
        match start.clone().or(home) {
            Some(path) if ready => vec![self.list(side, path, None)],
            _ => {
                *self.core.start_dir_mut(side) = start;
                if self.core.homes.contains_key(&endpoint)
                    || !self.core.connecting.insert(endpoint.clone())
                {
                    Vec::new()
                } else {
                    self.connection_connecting(&endpoint);
                    vec![Effect::Connect(endpoint)]
                }
            }
        }
    }

    fn listed(&mut self, request: RequestId, result: Result<Vec<Entry>, FsError>) -> Vec<Effect> {
        let Some(side) = [PaneSide::Left, PaneSide::Right].into_iter().find(|&side| {
            self.core
                .pane(side)
                .pending
                .as_ref()
                .is_some_and(|pending| pending.request == request)
        }) else {
            return Vec::new();
        };
        let pane = self.core.pane(side);
        let refresh = pane.pending.as_ref().is_some_and(|pending| pending.refresh);
        let first_listing = pane.cwd.is_none();
        let failed_path = pane.pending.as_ref().map(|pending| pending.path.clone());
        match self.core.pane_mut(side).finish_list(request, result) {
            Ok(false) => Vec::new(),
            Ok(true) if refresh => Vec::new(),
            Ok(true) => {
                let Some(location) = self.core.pane(side).location() else {
                    return Vec::new();
                };
                self.persisted.remember_directory(location.clone());
                vec![Effect::RememberDirectory(location)]
            }
            Err(error) => {
                let endpoint = self.core.pane(side).endpoint.clone();
                self.error_status(match &endpoint {
                    Endpoint::Local => error.to_string(),
                    Endpoint::Remote { .. } => format!("{endpoint}: {error}"),
                });
                match self.core.homes.get(&endpoint).cloned() {
                    Some(home) if first_listing && failed_path.as_ref() != Some(&home) => {
                        vec![self.list(side, home, None)]
                    }
                    _ => Vec::new(),
                }
            }
        }
    }

    fn connected(&mut self, endpoint: Endpoint, home: PathBuf) -> Vec<Effect> {
        self.core.connecting.remove(&endpoint);
        self.core.homes.insert(endpoint.clone(), home.clone());
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            let pane = self.core.pane(side);
            let waiting = pane.endpoint == endpoint && pane.cwd.is_none() && pane.pending.is_none();
            if waiting {
                let path = self
                    .core
                    .start_dir_mut(side)
                    .take()
                    .unwrap_or_else(|| home.clone());
                effects.push(self.list(side, path, None));
            }
        }
        effects
    }

    fn refresh_endpoint(&mut self, endpoint: &Endpoint) -> Vec<Effect> {
        let mut effects = Vec::new();
        for side in [PaneSide::Left, PaneSide::Right] {
            if self.core.pane(side).endpoint == *endpoint {
                effects.extend(self.refresh(side));
            }
        }
        effects
    }

    fn error_status(&mut self, text: impl Display) {
        self.core.set_status(StatusMessage::error(text.to_string()));
    }
}

/// A location as shown in messages: `/path` locally, `host:/path` remotely.
fn describe(location: &Location) -> String {
    let path = location.path.display();
    match &location.endpoint {
        Endpoint::Local => path.to_string(),
        Endpoint::Remote { host, sudo: false } => format!("{host}:{path}"),
        Endpoint::Remote { host, sudo: true } => format!("{host}:{path} [sudo]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ConfigError, ConnectError, FsError};
    use crate::transfer::model::TransferCommand;
    use crate::ui::action::CursorMove;
    use crate::vfs::EntryKind;
    use std::time::Instant;

    fn web() -> Endpoint {
        Endpoint::Remote {
            host: "web".into(),
            sudo: false,
        }
    }

    fn file(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::File,
            size: Some(1),
            mtime: Some(1),
            mode: Some(0o644),
        }
    }

    fn dir(name: &str) -> Entry {
        Entry {
            name: name.into(),
            kind: EntryKind::Dir,
            size: Some(1),
            mtime: Some(1),
            mode: Some(0o755),
        }
    }

    fn local(path: &str) -> Location {
        Location {
            endpoint: Endpoint::Local,
            path: path.into(),
        }
    }

    fn build(
        state: PersistedState,
        left: (Endpoint, Option<PathBuf>),
        right: Option<(Endpoint, Option<PathBuf>)>,
    ) -> (App, Vec<Effect>) {
        App::new(
            Settings::default(),
            HostList::default(),
            Groups::default(),
            state,
            Background::Dark,
            left,
            right,
        )
    }

    /// The `(request, location)` of a `ListDir` effect.
    fn list_dir(effect: &Effect) -> (RequestId, Location) {
        match effect {
            Effect::ListDir { request, location } => (*request, location.clone()),
            other => panic!("expected ListDir, got {other:?}"),
        }
    }

    fn deliver(app: &mut App, effect: &Effect, entries: Vec<Entry>) -> Vec<Effect> {
        let (request, location) = list_dir(effect);
        app.handle_event(BgEvent::Listed {
            request,
            location,
            result: Ok(entries),
        })
    }

    /// Two local panes at `/l` and `/r`, both listed, on an 80x24 screen.
    fn two_local() -> App {
        let (mut app, effects) = build(
            PersistedState::default(),
            (Endpoint::Local, Some("/l".into())),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        assert_eq!(effects.len(), 2);
        deliver(
            &mut app,
            &effects[0],
            vec![dir("sub"), file("a"), file("b")],
        );
        deliver(&mut app, &effects[1], vec![file("x")]);
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        app
    }

    fn cursor_name(app: &App, side: PaneSide) -> String {
        let entry = app.core.pane(side).cursor_entry().expect("cursor");
        entry.name.to_string_lossy().into_owned()
    }

    #[test]
    fn refresh_reloads_only_the_focused_pane_and_is_ignored_in_queue() {
        let mut app = two_local();
        app.core.pane_mut(PaneSide::Left).toggle(2);
        app.core.pane_mut(PaneSide::Left).set_cursor(3, 20);
        let untouched = app.core.pane(PaneSide::Right).clone();
        let effects = app.handle_action(Action::Refresh);
        assert_eq!(effects.len(), 1);
        assert_eq!(list_dir(&effects[0]).1, local("/l"));
        deliver(&mut app, &effects[0], vec![file("0"), file("a"), file("b")]);
        assert_eq!(cursor_name(&app, PaneSide::Left), "b");
        assert!(app
            .core
            .pane(PaneSide::Left)
            .selection
            .contains(std::ffi::OsStr::new("a")));
        assert_eq!(app.core.pane(PaneSide::Right), &untouched);
        app.handle_action(Action::ToggleQueue);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.input_mode(), InputMode::Queue);
        assert!(app.handle_action(Action::Refresh).is_empty());
    }

    #[test]
    fn startup_lists_local_panes_and_resolves_aliases() {
        let hosts = HostList {
            aliases: vec!["db".into(), "web".into()],
            warnings: Vec::new(),
        };
        let (app, effects) = App::new(
            Settings {
                show_hidden: true,
                ..Settings::default()
            },
            hosts,
            Groups::default(),
            PersistedState::default(),
            Background::Light,
            (Endpoint::Local, Some("/work".into())),
            None,
        );
        assert_eq!(effects.len(), 4);
        assert_eq!(list_dir(&effects[0]).1, local("/work"));
        // The right pane defaults to Local in the same directory.
        assert_eq!(list_dir(&effects[1]).1, local("/work"));
        assert_ne!(list_dir(&effects[0]).0, list_dir(&effects[1]).0);
        assert_eq!(effects[2], Effect::ResolveHost("db".into()));
        assert_eq!(effects[3], Effect::ResolveHost("web".into()));
        assert!(app.core.pane(PaneSide::Left).show_hidden);
        // Without a right endpoint the host picker opens (wiring point 1).
        assert_eq!(app.input_mode(), InputMode::Picker);
        assert_eq!(app.core.focused(), PaneSide::Left);
        assert_eq!(app.background, Background::Light);
    }

    #[test]
    fn remote_panes_connect_once_and_list_after_connected() {
        let (mut app, effects) = build(
            PersistedState::default(),
            (web(), Some("/srv".into())),
            Some((web(), None)),
        );
        assert_eq!(effects, [Effect::Connect(web())]);

        let effects = app.handle_event(BgEvent::Connected {
            endpoint: web(),
            home: "/home/me".into(),
        });
        assert_eq!(effects.len(), 2);
        assert_eq!(list_dir(&effects[0]).1.path, PathBuf::from("/srv"));
        assert_eq!(list_dir(&effects[1]).1.path, PathBuf::from("/home/me"));
        assert_eq!(list_dir(&effects[1]).1.endpoint, web());

        let remembered = deliver(&mut app, &effects[0], vec![file("a")]);
        assert_eq!(
            remembered,
            [Effect::RememberDirectory(Location {
                endpoint: web(),
                path: "/srv".into()
            })]
        );
        assert_eq!(app.persisted.last_directories.len(), 1);
    }

    #[test]
    fn remembered_directory_is_used_and_falls_back_to_home_when_gone() {
        let state = PersistedState {
            last_directories: vec![Location {
                endpoint: web(),
                path: "/gone".into(),
            }],
            ..PersistedState::default()
        };
        let (mut app, _) = build(
            state,
            (Endpoint::Local, Some("/l".into())),
            Some((web(), None)),
        );
        let effects = app.handle_event(BgEvent::Connected {
            endpoint: web(),
            home: "/home/me".into(),
        });
        let (request, location) = list_dir(&effects[0]);
        assert_eq!(location.path, PathBuf::from("/gone"));

        let effects = app.handle_event(BgEvent::Listed {
            request,
            location,
            result: Err(FsError::NotFound {
                path: "/gone".into(),
            }),
        });
        let status = app.core.status().expect("status");
        assert!(status.error);
        assert_eq!(status.text, "web: /gone does not exist");
        assert_eq!(list_dir(&effects[0]).1.path, PathBuf::from("/home/me"));

        // A failing home does not loop.
        let (request, location) = list_dir(&effects[0]);
        let effects = app.handle_event(BgEvent::Listed {
            request,
            location,
            result: Err(FsError::PermissionDenied {
                path: "/home/me".into(),
            }),
        });
        assert!(effects.is_empty());
    }

    #[test]
    fn local_pane_without_a_directory_connects_local_for_its_home() {
        let (mut app, effects) = build(PersistedState::default(), (web(), None), None);
        assert_eq!(
            effects,
            [Effect::Connect(web()), Effect::Connect(Endpoint::Local)]
        );
        let effects = app.handle_event(BgEvent::Connected {
            endpoint: Endpoint::Local,
            home: "/Users/me".into(),
        });
        assert_eq!(list_dir(&effects[0]).1, local("/Users/me"));
    }

    #[test]
    fn config_warnings_become_an_error_status() {
        let warning = |line| ConfigError::Parse {
            path: "/c".into(),
            line,
            message: "bad".into(),
        };
        let hosts = HostList {
            aliases: Vec::new(),
            warnings: vec![warning(1), warning(2)],
        };
        let (app, _) = App::new(
            Settings::default(),
            hosts,
            Groups::default(),
            PersistedState::default(),
            Background::Dark,
            (Endpoint::Local, Some("/".into())),
            None,
        );
        let status = app.core.status().expect("status");
        assert!(status.error);
        assert!(status.text.starts_with("ssh config: "));
        assert!(status.text.ends_with("(and 1 more)"));
    }

    #[test]
    fn open_enters_directories_and_parent_reselects() {
        let mut app = two_local();
        assert_eq!(cursor_name(&app, PaneSide::Left), "sub");
        let effects = app.handle_action(Action::Open);
        assert_eq!(list_dir(&effects[0]).1, local("/l/sub"));
        let remembered = deliver(&mut app, &effects[0], vec![file("inner")]);
        assert_eq!(remembered, [Effect::RememberDirectory(local("/l/sub"))]);

        // Open on a file previews it.
        assert!(matches!(
            app.handle_action(Action::Open).as_slice(),
            [Effect::LoadPreview { location, .. }] if *location == local("/l/sub/inner")
        ));
        app.handle_action(Action::Cancel);

        let effects = app.handle_action(Action::Parent);
        assert_eq!(list_dir(&effects[0]).1, local("/l"));
        deliver(
            &mut app,
            &effects[0],
            vec![file("a"), dir("sub"), file("b")],
        );
        assert_eq!(cursor_name(&app, PaneSide::Left), "sub");
        assert_eq!(app.persisted.last_directories, [local("/l")]);
    }

    #[test]
    fn stale_listings_are_ignored() {
        let mut app = two_local();
        let first = app.handle_action(Action::Open);
        let second = app.handle_action(Action::Open);
        assert!(deliver(&mut app, &first[0], vec![file("old")]).is_empty());
        assert_eq!(app.core.pane(PaneSide::Left).cwd, Some(PathBuf::from("/l")));
        deliver(&mut app, &second[0], vec![file("new")]);
        assert_eq!(cursor_name(&app, PaneSide::Left), "new");
    }

    #[test]
    fn keys_drive_the_focused_pane() {
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::End));
        assert_eq!(cursor_name(&app, PaneSide::Left), "b");
        // Home is the `..` row: Space there selects nothing.
        app.handle_action(Action::MoveCursor(CursorMove::Home));
        assert!(app.core.pane(PaneSide::Left).cursor_entry().is_none());
        app.handle_action(Action::ToggleSelect);
        assert!(app.core.pane(PaneSide::Left).selection.is_empty());
        assert_eq!(cursor_name(&app, PaneSide::Left), "sub");
        app.handle_action(Action::ToggleSelect);
        assert_eq!(cursor_name(&app, PaneSide::Left), "a");
        app.handle_action(Action::InvertSelection);
        assert_eq!(app.core.pane(PaneSide::Left).selection.len(), 2);
        app.handle_action(Action::SelectAll);
        assert_eq!(app.core.pane(PaneSide::Left).selection.len(), 3);
        app.handle_action(Action::ReverseSort);
        assert_ne!(app.core.pane(PaneSide::Left).sort, Default::default());
        app.handle_action(Action::ToggleHidden);
        assert!(app.core.pane(PaneSide::Left).show_hidden);
        // The other pane is untouched.
        assert!(app.core.pane(PaneSide::Right).selection.is_empty());
        assert!(!app.core.pane(PaneSide::Right).show_hidden);
    }

    #[test]
    fn tab_cycles_panes_and_the_visible_queue() {
        let mut app = two_local();
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.core.focused(), PaneSide::Right);
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.core.focused(), PaneSide::Left);

        app.handle_action(Action::ToggleQueue);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.input_mode(), InputMode::Queue);
        assert!(app.core.queue_focused());
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.focused(), PaneSide::Left);

        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::Cancel);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.focused(), PaneSide::Right);

        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::ToggleQueue);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.layout().queue, None);
    }

    #[test]
    fn filter_mode_types_keeps_and_clears() {
        let mut app = two_local();
        app.handle_action(Action::StartFilter);
        assert_eq!(app.input_mode(), InputMode::Filter);
        app.handle_action(Action::Text(TextEdit::Insert('b')));
        assert_eq!(app.core.pane(PaneSide::Left).visible.len(), 2);
        app.handle_action(Action::Confirm);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.pane(PaneSide::Left).filter, "b");

        // Esc in browse mode clears a kept filter.
        app.handle_action(Action::Cancel);
        assert_eq!(app.core.pane(PaneSide::Left).filter, "");

        app.handle_action(Action::StartFilter);
        app.handle_action(Action::Text(TextEdit::Insert('a')));
        app.handle_action(Action::Cancel);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.pane(PaneSide::Left).filter, "");

        // Another action stops typing, keeps the filter, and is handled.
        app.handle_action(Action::StartFilter);
        app.handle_action(Action::Text(TextEdit::Insert('a')));
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.input_mode(), InputMode::Browse);
        assert_eq!(app.core.focused(), PaneSide::Right);
        assert_eq!(app.core.pane(PaneSide::Left).filter, "a");
    }

    #[test]
    fn mouse_clicks_focus_select_and_open() {
        let mut app = two_local();
        let at = Instant::now();
        let click = |side, index, modifiers| Action::Click {
            target: ClickTarget::PaneRow { side, index },
            modifiers,
            at,
        };
        app.handle_action(click(PaneSide::Right, 0, ClickModifiers::default()));
        assert_eq!(app.core.focused(), PaneSide::Right);

        let toggle = ClickModifiers {
            toggle: true,
            range: false,
        };
        let range = ClickModifiers {
            toggle: false,
            range: true,
        };
        app.handle_action(click(PaneSide::Left, 2, toggle));
        assert_eq!(app.core.focused(), PaneSide::Left);
        assert_eq!(cursor_name(&app, PaneSide::Left), "a");
        app.handle_action(click(PaneSide::Left, 3, range));
        assert_eq!(app.core.pane(PaneSide::Left).targets().len(), 2);
        // Toggling the `..` row selects nothing.
        app.handle_action(click(PaneSide::Left, 0, toggle));
        assert_eq!(app.core.pane(PaneSide::Left).selection.len(), 2);

        app.handle_action(Action::Click {
            target: ClickTarget::PaneArea(PaneSide::Right),
            modifiers: ClickModifiers::default(),
            at,
        });
        assert_eq!(app.core.focused(), PaneSide::Right);

        let effects = app.handle_action(Action::DoubleClick {
            target: ClickTarget::PaneRow {
                side: PaneSide::Left,
                index: 1,
            },
            at,
        });
        assert_eq!(app.core.focused(), PaneSide::Left);
        assert_eq!(list_dir(&effects[0]).1, local("/l/sub"));

        // A double click on `..` goes to the parent, reselecting `l`.
        let effects = app.handle_action(Action::DoubleClick {
            target: ClickTarget::PaneRow {
                side: PaneSide::Right,
                index: 0,
            },
            at,
        });
        assert_eq!(list_dir(&effects[0]).1, local("/"));
        assert_eq!(
            app.core
                .pane(PaneSide::Right)
                .pending
                .as_ref()
                .expect("listing")
                .reselect,
            Some("r".into())
        );
    }

    #[test]
    fn open_on_the_parent_row_goes_up() {
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::Home));
        let effects = app.handle_action(Action::Open);
        assert_eq!(list_dir(&effects[0]).1, local("/"));
        // Nothing to copy, delete, or drag on `..`.
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::Home));
        assert!(app.handle_action(Action::Copy).is_empty());
        assert!(app.handle_action(Action::Delete).is_empty());
        assert!(app.dialog.stack.is_empty());
        app.handle_action(Action::DragStart {
            side: PaneSide::Left,
            index: 0,
            at: Instant::now(),
        });
        assert!(app.transfer.drag.is_none());
    }

    #[test]
    fn sorting_by_menu_header_and_reverse_is_remembered_per_endpoint() {
        let mut app = two_local();
        let at = Instant::now();
        let header = |key| Action::Click {
            target: ClickTarget::ColumnHeader {
                side: PaneSide::Right,
                key,
            },
            modifiers: ClickModifiers::default(),
            at,
        };
        // A header click focuses its pane and sorts by the column.
        let effects = app.handle_action(header(SortKey::Size));
        assert_eq!(app.core.focused(), PaneSide::Right);
        let size = Sort::by(SortKey::Size);
        assert_eq!(app.core.pane(PaneSide::Right).sort, size);
        assert_eq!(
            effects,
            [Effect::RememberSort {
                endpoint: Endpoint::Local,
                sort: size
            }]
        );
        assert_eq!(app.persisted.last_sort(&Endpoint::Local), Some(size));
        // Clicking the active header again reverses it.
        app.handle_action(header(SortKey::Size));
        assert_eq!(app.core.pane(PaneSide::Right).sort, size.reversed());

        // The menu's `SortBy` keeps an active key, `ReverseSort` flips it.
        app.handle_action(Action::SortBy(SortKey::Size));
        assert_eq!(app.core.pane(PaneSide::Right).sort, size.reversed());
        app.handle_action(Action::ReverseSort);
        assert_eq!(app.core.pane(PaneSide::Right).sort, size);
        let effects = app.handle_action(Action::SortBy(SortKey::Extension));
        let extension = Sort::by(SortKey::Extension);
        assert_eq!(app.core.pane(PaneSide::Right).sort, extension);
        assert_eq!(effects.len(), 1);
        // The left pane keeps its own sort.
        assert_eq!(app.core.pane(PaneSide::Left).sort, Sort::default());

        // A pane switched to an endpoint with a remembered sort takes it.
        let mut state = PersistedState::default();
        state.remember_sort(web(), Sort::by(SortKey::Modified));
        let (mut app, _) = build(
            state,
            (Endpoint::Local, Some("/l".into())),
            Some((web(), Some("/srv".into()))),
        );
        assert_eq!(
            app.core.pane(PaneSide::Right).sort,
            Sort::by(SortKey::Modified)
        );
        assert_eq!(app.core.pane(PaneSide::Left).sort, Sort::default());
        app.switch_endpoint(PaneSide::Left, web(), Some("/tmp".into()));
        assert_eq!(
            app.core.pane(PaneSide::Left).sort,
            Sort::by(SortKey::Modified)
        );
    }

    #[test]
    fn menus_run_the_chosen_action_like_its_key() {
        let mut app = two_local();
        app.handle_action(Action::SortMenu);
        assert_eq!(app.input_mode(), InputMode::Dialog);
        app.handle_action(Action::DialogHotkey('s'));
        assert!(app.dialog.menu.is_none());
        assert_eq!(app.core.pane(PaneSide::Left).sort, Sort::by(SortKey::Size));

        // The context menu of the cursor row runs its items through core.
        app.handle_action(Action::ContextMenu(None));
        let effects = app.handle_action(Action::DialogHotkey('B'));
        assert_eq!(effects, [Effect::SaveBookmark(local("/l"))]);
        assert_eq!(app.input_mode(), InputMode::Browse);
    }

    #[test]
    fn control_r_runs_the_context_menu_shortcut_but_not_other_dialogs() {
        use crate::ui::hit::HitMap;
        use crate::ui::input::InputState;
        use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let mut app = two_local();
        let event = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let press = |app: &mut App| {
            let action = InputState::new()
                .handle(&event, app.input_mode(), &HitMap::default(), Instant::now())
                .expect("control-r action in menu mode");
            app.handle_action(action)
        };
        app.handle_action(Action::ContextMenu(None));
        let effects = press(&mut app);
        assert!(app.dialog.menu.is_none());
        assert_eq!(effects.len(), 1);
        assert_eq!(list_dir(&effects[0]).1, local("/l"));
        deliver(&mut app, &effects[0], vec![dir("sub"), file("a")]);
        app.handle_action(Action::SortMenu);
        assert!(press(&mut app).is_empty());
        assert!(app.dialog.menu.is_some());
        app.handle_action(Action::Cancel);
        app.handle_action(Action::Delete);
        assert!(press(&mut app).is_empty());
        assert!(!app.dialog.stack.is_empty());
        assert!(app.core.pane(PaneSide::Left).pending.is_none());
    }

    #[test]
    fn an_alt_click_on_a_pane_row_leaves_the_queue_for_the_context_menu() {
        let mut app = two_local();
        app.handle_action(Action::ToggleQueue);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        assert_eq!(app.input_mode(), InputMode::Queue);
        app.handle_action(Action::ContextMenu(Some(crate::ui::action::MenuAnchor {
            side: PaneSide::Left,
            index: 2,
            column: 5,
            row: 5,
        })));
        assert!(!app.core.queue_focused());
        assert_eq!(cursor_name(&app, PaneSide::Left), "a");
        assert!(app.dialog.menu.is_some());
    }

    #[test]
    fn a_click_on_the_title_button_opens_the_host_picker_for_that_pane() {
        let mut app = two_local();
        app.handle_action(Action::Click {
            target: ClickTarget::PaneTitle(PaneSide::Right),
            modifiers: ClickModifiers::default(),
            at: Instant::now(),
        });
        assert_eq!(app.core.focused(), PaneSide::Right);
        assert_eq!(app.input_mode(), InputMode::Picker);
        let picker = app.dialog.picker.as_ref().expect("picker");
        assert_eq!(
            picker.kind,
            PickerKind::Hosts {
                side: PaneSide::Right
            }
        );
    }

    #[test]
    fn wheel_scrolls_the_pane_under_the_pointer() {
        let (mut app, effects) = build(
            PersistedState::default(),
            (Endpoint::Local, Some("/l".into())),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        app.handle_action(Action::Resize {
            width: 80,
            height: 24,
        });
        let many: Vec<Entry> = (0..100).map(|i| file(&format!("f{i:03}"))).collect();
        deliver(&mut app, &effects[1], many);
        app.handle_action(Action::Scroll {
            region: ScrollRegion::Pane(PaneSide::Right),
            lines: 30,
        });
        let pane = app.core.pane(PaneSide::Right);
        assert_eq!((pane.scroll, pane.cursor), (30, 30));
        assert_eq!(app.core.focused(), PaneSide::Left);
    }

    #[test]
    fn layout_and_page_height_follow_size_and_queue() {
        let mut app = two_local();
        let layout = app.core.layout();
        assert_eq!(layout.left, Rect::new(0, 0, 40, 22));
        assert_eq!(layout.right, Rect::new(40, 0, 40, 22));
        assert_eq!(layout.status_bar, Rect::new(0, 22, 80, 1));
        assert_eq!(layout.button_bar, Rect::new(0, 23, 80, 1));
        assert_eq!(layout.queue, None);
        assert_eq!(app.core.page_height(PaneSide::Left), 18);

        app.handle_action(Action::ToggleQueue);
        let layout = app.core.layout();
        assert_eq!(layout.queue, Some(Rect::new(0, 15, 80, 7)));
        assert_eq!(layout.left.height, 15);
        assert_eq!(app.core.page_height(PaneSide::Right), 11);

        assert!(!layout.queue_collapsed);
        // Wide enough for the quick view to take the other pane only.
        assert_eq!(layout.quick_view, layout.right);

        // Below 24 rows the queue is one summary row.
        app.handle_action(Action::Resize {
            width: 81,
            height: 12,
        });
        let layout = app.core.layout();
        assert_eq!(layout.queue, Some(Rect::new(0, 9, 81, 1)));
        assert!(layout.queue_collapsed);
        assert_eq!(layout.right, Rect::new(40, 0, 41, 9));
        assert_eq!(layout.tabs, None);
        assert_eq!(app.core.page_height(PaneSide::Left), 5);

        app.handle_action(Action::Resize {
            width: 0,
            height: 0,
        });
        assert_eq!(app.core.layout().status_bar.height, 0);
        assert_eq!(app.core.layout().button_bar.height, 0);
        app.handle_action(Action::Resize {
            width: 80,
            height: 1,
        });
        assert_eq!(app.core.layout().button_bar, Rect::new(0, 0, 80, 1));
        assert_eq!(app.core.layout().status_bar.height, 0);
    }

    #[test]
    fn narrow_terminals_show_one_pane_under_tabs() {
        let mut app = two_local();
        app.handle_action(Action::Resize {
            width: 70,
            height: 30,
        });
        // Two panes of 35 columns: too narrow to split off the quick view.
        let layout = app.core.layout();
        assert_eq!(layout.tabs, None);
        assert_eq!(layout.quick_view, Rect::new(0, 0, 70, 28));

        app.handle_action(Action::Resize {
            width: 50,
            height: 20,
        });
        let layout = app.core.layout();
        assert_eq!(layout.tabs, Some(Rect::new(0, 0, 50, 1)));
        assert_eq!(layout.left, Rect::new(0, 1, 50, 17));
        assert_eq!(layout.right, layout.left);
        assert_eq!(layout.quick_view, layout.left);
        assert_eq!(app.core.page_height(PaneSide::Right), 13);
        assert!(app.core.pane_drawn(PaneSide::Left));
        assert!(!app.core.pane_drawn(PaneSide::Right));
        // Tab switches the pane shown.
        app.handle_action(Action::SwitchFocus);
        assert!(app.core.pane_drawn(PaneSide::Right));
        assert!(!app.core.pane_drawn(PaneSide::Left));
    }

    #[test]
    fn a_collapsed_queue_takes_no_focus() {
        let mut app = two_local();
        app.handle_action(Action::ToggleQueue);
        app.handle_action(Action::SwitchFocus);
        app.handle_action(Action::SwitchFocus);
        assert!(app.core.queue_focused());
        // Shrinking collapses the panel and gives focus back to the pane.
        app.handle_action(Action::Resize {
            width: 80,
            height: 20,
        });
        assert!(!app.core.queue_focused());
        assert_eq!(app.core.focused(), PaneSide::Right);
        app.handle_action(Action::SwitchFocus);
        assert!(!app.core.queue_focused());
        assert_eq!(app.core.focused(), PaneSide::Left);
    }

    #[test]
    fn a_too_small_terminal_ignores_input_but_quit() {
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::Down));
        app.handle_action(Action::Resize {
            width: 30,
            height: 8,
        });
        assert!(app.core.too_small());
        assert!(app
            .handle_action(Action::MoveCursor(CursorMove::Down))
            .is_empty());
        assert!(app.handle_action(Action::Open).is_empty());
        assert!(app.handle_action(Action::Copy).is_empty());
        assert!(app.handle_action(Action::SwitchFocus).is_empty());
        assert_eq!(app.core.focused(), PaneSide::Left);
        assert_eq!(cursor_name(&app, PaneSide::Left), "a");
        // Growing again restores everything as it was.
        app.handle_action(Action::Resize {
            width: 160,
            height: 45,
        });
        assert!(!app.core.too_small());
        assert_eq!(cursor_name(&app, PaneSide::Left), "a");
        app.handle_action(Action::Resize {
            width: 40,
            height: 9,
        });
        assert_eq!(app.handle_action(Action::Quit), [Effect::Quit]);
    }

    #[test]
    fn resizing_away_a_drag_source_ends_the_drag() {
        let mut app = two_local();
        app.handle_action(Action::DragStart {
            side: PaneSide::Left,
            index: 1,
            at: Instant::now(),
        });
        assert!(app.transfer.drag.is_some());
        // Single-pane mode still draws the focused source pane.
        app.handle_action(Action::Resize {
            width: 50,
            height: 20,
        });
        assert!(app.transfer.drag.is_some());
        app.handle_action(Action::Resize {
            width: 30,
            height: 8,
        });
        assert!(app.transfer.drag.is_none());
    }

    #[test]
    fn resize_keeps_the_cursor_on_screen() {
        let (mut app, effects) = build(
            PersistedState::default(),
            (Endpoint::Local, Some("/l".into())),
            Some((Endpoint::Local, Some("/r".into()))),
        );
        let many: Vec<Entry> = (0..100).map(|i| file(&format!("f{i:03}"))).collect();
        deliver(&mut app, &effects[0], many);
        app.handle_action(Action::Resize {
            width: 80,
            height: 50,
        });
        // The cursor starts on the first entry (row 1) and moves a page
        // of 44 rows.
        app.handle_action(Action::MoveCursor(CursorMove::PageDown));
        assert_eq!(app.core.pane(PaneSide::Left).cursor, 45);
        app.handle_action(Action::Resize {
            width: 80,
            height: 10,
        });
        let pane = app.core.pane(PaneSide::Left);
        assert_eq!(pane.cursor, 45);
        assert!(pane.cursor < pane.scroll + app.core.page_height(PaneSide::Left));
    }

    #[test]
    fn bookmarks_are_added_once() {
        let mut app = two_local();
        assert_eq!(
            app.handle_action(Action::AddBookmark),
            [Effect::SaveBookmark(local("/l"))]
        );
        assert_eq!(app.core.status().expect("status").text, "Bookmarked /l");
        assert!(app.handle_action(Action::AddBookmark).is_empty());
        assert_eq!(
            app.core.status().expect("status"),
            &StatusMessage::info("Already bookmarked: /l")
        );
        assert_eq!(app.persisted.bookmarks, [local("/l")]);
    }

    #[test]
    fn reveal_shell_and_quit() {
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::Down));
        assert_eq!(
            app.handle_action(Action::Reveal),
            [Effect::Reveal(local("/l/a"))]
        );
        assert_eq!(
            app.handle_action(Action::OpenShell),
            [Effect::OpenShell(local("/l"))]
        );
        assert_eq!(app.handle_action(Action::Quit), [Effect::Quit]);
    }

    #[test]
    fn switch_endpoint_connects_and_reuses_connections() {
        let mut app = two_local();
        let effects = app.switch_endpoint(PaneSide::Right, web(), None);
        assert_eq!(effects, [Effect::Connect(web())]);
        assert_eq!(app.core.pane(PaneSide::Right).cwd, None);
        let effects = app.handle_event(BgEvent::Connected {
            endpoint: web(),
            home: "/home/me".into(),
        });
        deliver(&mut app, &effects[0], vec![file("r")]);

        // The connected endpoint is reused without a new Connect, at the
        // remembered directory.
        let effects = app.switch_endpoint(PaneSide::Left, web(), None);
        assert_eq!(
            list_dir(&effects[0]).1,
            Location {
                endpoint: web(),
                path: "/home/me".into()
            }
        );

        // Bookmark jump on the same endpoint only lists.
        let effects = app.switch_endpoint(PaneSide::Left, web(), Some("/etc".into()));
        assert_eq!(list_dir(&effects[0]).1.path, PathBuf::from("/etc"));
        assert!(app.switch_endpoint(PaneSide::Left, web(), None).is_empty());

        // Back to local: its remembered directory, one per endpoint, so the
        // one listed last (the right pane's).
        let effects = app.switch_endpoint(PaneSide::Left, Endpoint::Local, None);
        assert_eq!(list_dir(&effects[0]).1, local("/r"));
    }

    #[test]
    fn a_failed_connect_can_be_retried_by_switching_again() {
        let mut app = two_local();
        assert_eq!(
            app.switch_endpoint(PaneSide::Right, web(), None),
            [Effect::Connect(web())]
        );
        app.handle_event(BgEvent::ConnectFailed {
            endpoint: web(),
            error: ConnectError::AuthFailed {
                detail: "denied".into(),
            }
            .into(),
        });
        assert!(app.core.status().expect("status").error);
        app.switch_endpoint(PaneSide::Right, Endpoint::Local, Some("/r".into()));
        assert_eq!(
            app.switch_endpoint(PaneSide::Right, web(), None),
            [Effect::Connect(web())]
        );
    }

    #[test]
    fn reconnected_refreshes_panes_on_that_endpoint() {
        let mut app = two_local();
        app.handle_action(Action::MoveCursor(CursorMove::Down));
        app.handle_action(Action::ToggleSelect);
        let effects = app.handle_event(BgEvent::Reconnected(Endpoint::Local));
        assert_eq!(effects.len(), 3);
        assert_eq!(
            effects[2],
            Effect::Transfer(TransferCommand::EndpointReconnected(Endpoint::Local))
        );
        let remembered = deliver(
            &mut app,
            &effects[0],
            vec![dir("sub"), file("a"), file("b")],
        );
        assert!(remembered.is_empty());
        assert_eq!(app.core.pane(PaneSide::Left).selection.len(), 1);
        assert_eq!(cursor_name(&app, PaneSide::Left), "b");
        assert!(app.handle_event(BgEvent::Reconnected(web())).is_empty());
    }

    #[test]
    fn listing_failures_and_disconnects_are_reported() {
        let mut app = two_local();
        let effects = app.handle_action(Action::Open);
        let (request, location) = list_dir(&effects[0]);
        let effects = app.handle_event(BgEvent::Listed {
            request,
            location,
            result: Err(FsError::PermissionDenied {
                path: "/l/sub".into(),
            }),
        });
        assert!(effects.is_empty());
        assert_eq!(
            app.core.status().expect("status"),
            &StatusMessage::error("Permission denied: /l/sub")
        );
        assert_eq!(app.core.pane(PaneSide::Left).cwd, Some(PathBuf::from("/l")));

        app.handle_event(BgEvent::Disconnected {
            endpoint: web(),
            error: ConnectError::AuthFailed { detail: "x".into() },
        });
        assert!(app
            .core
            .status()
            .expect("status")
            .text
            .starts_with("Lost connection to web"));
    }

    #[test]
    fn ids_are_never_reused() {
        let mut app = two_local();
        let a = app.core.next_request();
        let b = app.core.next_request();
        assert!(b > a);
        assert_ne!(app.core.next_job(), app.core.next_job());
        assert_ne!(app.core.next_timer(), app.core.next_timer());
    }
}
