//! The app state machine's interface: [`Action`] in, [`Effect`] out,
//! [`BgEvent`] back.
//!
//! `ui/input.rs` turns terminal events into `Action`s, interpreting keys
//! according to the [`InputMode`] the app reports. The app handles each
//! `Action` and each `BgEvent` purely and returns `Effect`s; effect executors
//! perform the I/O and report results as `BgEvent`s. Requests whose answers
//! may arrive late carry a [`RequestId`] chosen by the app so stale results
//! can be dropped.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::edit::EditOutcome;
use crate::error::{ConnectError, Error, FsError};
use crate::hosts::ResolvedHost;
use crate::preview::{ImageResized, PreviewData};
use crate::state::{Sort, SortKey};
use crate::sync::{Comparison, SyncDirection};
use crate::transfer::model::{JobId, QueueTarget, TransferCommand, TransferEvent};
use crate::vfs::{Endpoint, Entry, Location};

/// One of the two panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneSide {
    /// The left pane (defaults to the local machine).
    Left,
    /// The right pane (defaults to the selected remote host).
    Right,
}

impl PaneSide {
    /// The opposite pane.
    pub fn other(self) -> PaneSide {
        match self {
            PaneSide::Left => PaneSide::Right,
            PaneSide::Right => PaneSide::Left,
        }
    }
}

/// Correlates an effect with its background result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// Identifies a timer scheduled with [`Effect::ScheduleTimer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(pub u64);

/// A password held in zeroized memory. `Debug` never prints the content.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a password.
    pub fn new(value: Zeroizing<String>) -> Secret {
        Secret(value)
    }

    /// The password text. Callers must not copy it into non-zeroized memory.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// How the app wants keys interpreted; reported to `ui/input.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Pane browsing: the full key table of plan 2.4.
    Browse,
    /// Typing a pane filter: characters edit the filter, arrows move the
    /// cursor, Enter keeps the filter, Esc clears it.
    Filter,
    /// A dialog with a text field (rename, mkdir, chmod, masked password):
    /// characters edit the field, Tab moves focus, Enter confirms, Esc
    /// cancels. Bookmarks have no name, so `B` never opens one.
    TextField,
    /// A dialog with buttons only, or a popup menu (sort menu, context
    /// menu): Tab/arrows move focus, Space toggles the focused option,
    /// Enter confirms, Esc cancels, letters are hotkeys.
    Dialog,
    /// A filterable list (host picker, bookmarks): characters filter,
    /// arrows move, Enter picks, Esc closes.
    Picker,
    /// Full-screen preview or help: arrows and page keys scroll, `m` toggles
    /// Markdown rendering, Esc or `q` closes.
    Viewer,
    /// The queue panel has focus: arrows move, keys control the selected
    /// job or item, Tab or Esc leaves.
    Queue,
    /// The directory comparison view: arrows move, Space toggles an entry,
    /// a direction key starts the sync, Esc closes.
    SyncView,
}

/// Cursor movement within a list or viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMove {
    /// One row up.
    Up,
    /// One row down.
    Down,
    /// One page up.
    PageUp,
    /// One page down.
    PageDown,
    /// First row.
    Home,
    /// Last row.
    End,
}

/// An edit of the focused text field or filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEdit {
    /// Insert a character at the caret.
    Insert(char),
    /// Delete the character before the caret.
    Backspace,
    /// Delete the character after the caret.
    DeleteForward,
    /// Delete the word before the caret (Ctrl+W).
    DeleteWord,
    /// Clear the field (Ctrl+U).
    Clear,
    /// Move the caret left.
    Left,
    /// Move the caret right.
    Right,
    /// Move the caret to the start.
    Home,
    /// Move the caret to the end.
    End,
}

/// Direction of focus movement inside a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusStep {
    /// Next field or button.
    Next,
    /// Previous field or button.
    Previous,
}

/// Every button any dialog can show. Mouse clicks on a button arrive as
/// [`ClickTarget::DialogButton`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogButton {
    /// Accept.
    Ok,
    /// Dismiss without acting.
    Cancel,
    /// Affirm a question.
    Yes,
    /// Deny a question.
    No,
    /// Conflict: overwrite.
    Overwrite,
    /// Conflict: skip.
    Skip,
    /// Conflict: rename to `name (1).ext`.
    Rename,
    /// Conflict: overwrite if the source is newer.
    OverwriteIfNewer,
    /// Conflict: resume the partial file.
    Resume,
    /// Conflict checkbox: apply to all in this job.
    ApplyToAll,
    /// Tar conflict: overwrite every existing file.
    OverwriteAll,
    /// VPN hint: run the configured command.
    Run,
    /// Trash failed: delete permanently instead.
    DeletePermanently,
    /// Finder drop: upload the dropped paths.
    Upload,
    /// Retry a failed operation.
    Retry,
}

/// Where a context menu opens (plan 8.9: Option/Alt+click on a pane row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuAnchor {
    /// The pane clicked in.
    pub side: PaneSide,
    /// Visible row index clicked.
    pub index: usize,
    /// Pointer column; the menu opens at the pointer.
    pub column: u16,
    /// Pointer row.
    pub row: u16,
}

/// Keyboard modifiers of a mouse click, as reported by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClickModifiers {
    /// Ctrl or Cmd held: toggle the clicked row's selection.
    pub toggle: bool,
    /// Shift held: select the range from the cursor to the clicked row.
    pub range: bool,
}

/// What a mouse click hit, resolved from the last rendered layout by
/// [`HitMap::resolve`](crate::ui::hit::HitMap::resolve).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickTarget {
    /// A file row; `index` is the row's position in the pane's visible
    /// (filtered and sorted) list.
    PaneRow {
        /// The pane.
        side: PaneSide,
        /// Visible row index.
        index: usize,
    },
    /// Any other part of a pane (border, path line, blank space below the
    /// rows).
    PaneArea(PaneSide),
    /// The endpoint button in a pane's title: opens the host picker for
    /// that pane, like `o`.
    PaneTitle(PaneSide),
    /// A column header of a pane: sorts by that column, or reverses the
    /// order when it is already the sort column.
    ColumnHeader {
        /// The pane.
        side: PaneSide,
        /// The column's sort key.
        key: SortKey,
    },
    /// A job or item row in the queue panel.
    QueueRow(QueueTarget),
    /// A button in the open dialog.
    DialogButton(DialogButton),
    /// A row of the host picker or bookmark list (visible index).
    PickerRow(usize),
    /// A row of the comparison view (visible index).
    SyncRow(usize),
    /// An item of the open popup menu (index in the menu).
    MenuRow(usize),
}

/// A scrollable area under the mouse wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRegion {
    /// A pane's file list.
    Pane(PaneSide),
    /// The queue panel.
    Queue,
    /// The full-screen preview or the quick view.
    Preview,
    /// The help screen.
    Help,
    /// The host picker or bookmark list.
    Picker,
    /// The comparison view.
    SyncView,
}

/// Where a drag would drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// A pane outside any folder row: its current directory.
    Pane(PaneSide),
    /// A folder row (visible index) in a pane.
    Folder {
        /// The pane.
        side: PaneSide,
        /// Visible row index of the folder.
        index: usize,
    },
    /// Nowhere valid; releasing here cancels the drag.
    Invalid,
}

/// Queue panel controls, from keys (applied to the queue cursor) or from
/// clicking a row's button (applied to that row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueControl {
    /// Pause if running or queued, resume if paused.
    TogglePause,
    /// Cancel, keeping any partial file.
    Cancel,
    /// Re-queue a failed or canceled entry.
    Retry,
    /// Delete the partial file of a canceled or failed entry.
    DiscardPartial,
    /// Re-queue every failed item.
    RetryAllFailed,
    /// Remove finished jobs.
    ClearFinished,
}

/// A user intent. Variants whose meaning depends on time carry the
/// `Instant` of the input event so the state machine stays testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // Navigation (plan 2.4, 8.9: letter keys only, no function keys).
    /// Tab: switch the focused pane (and the queue panel when visible).
    SwitchFocus,
    /// Move the cursor or scroll a viewer.
    MoveCursor(CursorMove),
    /// Enter / Right / l: enter a directory (the `..` row: the parent),
    /// preview a file.
    Open,
    /// Backspace / Left / h: go to the parent directory.
    Parent,
    /// Space / Insert: toggle selection (or a sync entry) and move down.
    ToggleSelect,
    /// Ctrl+A: select all.
    SelectAll,
    /// `*`: invert the selection.
    InvertSelection,

    // File operations.
    /// c: copy to the other pane's current directory.
    Copy,
    /// m: move to the other pane's current directory.
    Move,
    /// n: create a directory.
    MakeDir,
    /// d / Delete: delete after confirmation.
    Delete,
    /// r: rename.
    Rename,
    /// e: edit.
    Edit,
    /// v: full-screen preview.
    Preview,
    /// i: toggle quick view in the non-focused pane.
    ToggleQuickView,
    /// m in a viewer: toggle rendered and raw Markdown.
    ToggleMarkdown,
    /// p: chmod.
    Chmod,
    /// R: reveal in Finder (download first for remote files).
    Reveal,

    // View.
    /// `/`: start filtering the focused pane.
    StartFilter,
    /// `.`: toggle hidden files.
    ToggleHidden,
    /// s: open the sort menu of the focused pane.
    SortMenu,
    /// Sort the focused pane by a key in its natural order
    /// (`state::Sort::by`); nothing changes when it already sorts by it.
    SortBy(SortKey),
    /// Reverse the focused pane's sort order.
    ReverseSort,
    /// x (on the cursor row of the focused pane, `None`) or Option/Alt+click
    /// on a pane row: open the context menu listing that entry's actions.
    ContextMenu(Option<MenuAnchor>),
    /// Q: toggle the queue panel.
    ToggleQueue,
    /// `?`: help.
    Help,

    // Endpoints and tools.
    /// o: switch the focused pane's endpoint (host picker incl. Local).
    OpenEndpointPicker,
    /// S: reconnect the focused pane with sudo.
    ReconnectWithSudo,
    /// b: bookmark list.
    ShowBookmarks,
    /// B: bookmark the focused pane's directory immediately (bookmarks have
    /// no name).
    AddBookmark,
    /// `=`: compare the two panes' directories.
    Compare,
    /// Start a one-way sync of the selected comparison entries.
    StartSync(SyncDirection),
    /// t: open a shell in the focused pane's directory.
    OpenShell,
    /// q: quit (asks when transfers are in progress).
    Quit,

    // Queue panel.
    /// A queue control; `target` is `None` for the queue cursor. `Cancel` on
    /// a job started by [`Effect::Sync`] routes to [`Effect::CancelSync`];
    /// every other control goes to the engine as [`Effect::Transfer`].
    Queue {
        /// What to do.
        control: QueueControl,
        /// The clicked row, or `None` for the selected one.
        target: Option<QueueTarget>,
    },

    // Dialogs, filter, pickers.
    /// Edit the active text field or filter.
    Text(TextEdit),
    /// Enter in a dialog, picker, or filter.
    Confirm,
    /// Esc: close the dialog or viewer, clear the filter, cancel.
    Cancel,
    /// Move focus between dialog fields and buttons.
    DialogFocus(FocusStep),
    /// Space on a dialog checkbox.
    ToggleOption,
    /// A letter hotkey in a dialog; the open dialog maps it to a button.
    DialogHotkey(char),
    /// Remove the selected picker entry (bookmark list).
    PickerRemove,

    // Mouse.
    /// A single click.
    Click {
        /// What was hit.
        target: ClickTarget,
        /// Modifier keys.
        modifiers: ClickModifiers,
        /// When the button was pressed.
        at: Instant,
    },
    /// A second click on the same row within the double-click interval.
    DoubleClick {
        /// What was hit.
        target: ClickTarget,
        /// When the second press happened.
        at: Instant,
    },
    /// Mouse wheel; negative `lines` scrolls up.
    Scroll {
        /// Area under the pointer.
        region: ScrollRegion,
        /// Lines to scroll.
        lines: i32,
    },
    /// A press on a file row moved by at least one cell.
    DragStart {
        /// The pane pressed in.
        side: PaneSide,
        /// Visible index of the pressed row.
        index: usize,
        /// When the drag threshold was crossed.
        at: Instant,
    },
    /// The pointer moved while dragging.
    DragMove {
        /// What is under the pointer.
        target: DropTarget,
        /// Pointer column, for the floating label.
        column: u16,
        /// Pointer row, for the floating label.
        row: u16,
    },
    /// The button was released while dragging.
    Drop {
        /// What was under the pointer.
        target: DropTarget,
        /// When the button was released.
        at: Instant,
    },
    /// Esc during a drag.
    DragCancel,

    // Terminal.
    /// Bracketed paste content (a Finder drop or typed text).
    Paste(String),
    /// The terminal was resized.
    Resize {
        /// Columns.
        width: u16,
        /// Rows.
        height: u16,
    },
}

/// How a delete removes files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMethod {
    /// Move to the system trash (local endpoint).
    Trash,
    /// Remove permanently, recursing into directories.
    Permanent,
}

/// A single file operation requested by the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsOp {
    /// Create a directory.
    MakeDir {
        /// The directory to create.
        path: PathBuf,
    },
    /// Rename within one directory or endpoint.
    Rename {
        /// Current path.
        from: PathBuf,
        /// New path.
        to: PathBuf,
    },
    /// Delete paths.
    Delete {
        /// Paths to delete.
        paths: Vec<PathBuf>,
        /// Trash or permanent.
        method: DeleteMethod,
    },
    /// Change permission bits.
    Chmod {
        /// Paths to change.
        paths: Vec<PathBuf>,
        /// New permission bits.
        mode: u32,
    },
}

/// A side effect requested by the app. Effects that suspend the TUI
/// (interactive auth, shell, VPN command, editor) are run one at a time by
/// the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    // Sessions.
    /// Connect an endpoint in the background (BatchMode). Answers with
    /// `Connected`, `Reconnecting` (retryable failure, including an
    /// unreachable first connect), `ConnectFailed`, `InteractiveAuthRequired`,
    /// or `SudoPasswordRequired`.
    Connect(Endpoint),
    /// Suspend the TUI and run the master connection in the foreground so
    /// OpenSSH can prompt; then continue connecting.
    InteractiveAuth(Endpoint),
    /// Continue a sudo start with the password the user typed.
    SudoPassword {
        /// The sudo endpoint.
        endpoint: Endpoint,
        /// The password; zeroized when dropped.
        password: Secret,
    },
    /// Retry a connection now (after a VPN command or on user request).
    Reconnect(Endpoint),
    /// Close an endpoint no pane uses any more.
    Disconnect(Endpoint),
    /// Resolve an alias with `ssh -G`.
    ResolveHost(String),
    /// Suspend the TUI and run a VPN command via `/bin/sh -c`.
    RunVpn {
        /// The endpoint that could not be reached.
        endpoint: Endpoint,
        /// The command from the matching `vpn` rule.
        command: String,
    },

    // File system.
    /// List a directory (browsing channel).
    ListDir {
        /// Correlation id.
        request: RequestId,
        /// The directory.
        location: Location,
    },
    /// Perform a file operation.
    FsOp {
        /// Correlation id.
        request: RequestId,
        /// Where to perform it.
        endpoint: Endpoint,
        /// What to do.
        op: FsOp,
    },
    /// Check whether pasted absolute paths all exist locally (Finder drop).
    CheckDroppedPaths {
        /// The pasted text as received.
        text: String,
        /// The tokens split with `shell-words`.
        paths: Vec<PathBuf>,
    },

    // Transfers and sync.
    /// Control the transfer engine.
    Transfer(TransferCommand),
    /// Compare two directory trees.
    Compare {
        /// Correlation id.
        request: RequestId,
        /// The left directory.
        left: Location,
        /// The right directory.
        right: Location,
    },
    /// Copy the given relative paths one way, via rsync or as an engine job
    /// with id `job`. An rsync sync reports no per-file progress; the queue
    /// shows it as indeterminate until `BgEvent::SyncFinished`. Selected
    /// paths that exist only on the destination side are deleted there when
    /// `delete_extraneous` is set, else left alone (see `sync`).
    Sync {
        /// Job id used for the engine path and for `SyncFinished`.
        job: JobId,
        /// Copy direction.
        direction: SyncDirection,
        /// The comparison the paths were selected from: its `left` and
        /// `right` are the two directories, and its entries tell the engine
        /// path which selected paths are files or symlinks on the source
        /// (`sync::engine_job`) and which exist only on the destination
        /// (`sync::extraneous`).
        comparison: Box<Comparison>,
        /// The selected paths, relative to both directories.
        paths: Vec<PathBuf>,
        /// Delete selected destination-only paths (default off; the
        /// checkbox of the sync confirmation).
        delete_extraneous: bool,
    },
    /// Cancel a sync started by [`Effect::Sync`]. The sync executor knows
    /// which backend runs it: it kills rsync (answered by
    /// `BgEvent::SyncFinished`) or sends `TransferCommand::Cancel` for the
    /// engine job of the same id.
    CancelSync(JobId),

    // Preview, edit, external programs.
    /// Load a preview (browsing channel). Superseded loads are canceled with
    /// [`Effect::CancelRequest`].
    LoadPreview {
        /// Correlation id.
        request: RequestId,
        /// The file.
        location: Location,
    },
    /// Edit a file with the user's editor.
    Edit(Location),
    /// Upload or discard an edited copy after `EditOutcome::RemoteChanged`.
    ResolveEditConflict {
        /// The remote file.
        location: Location,
        /// The local temp file with the edits.
        temp: PathBuf,
        /// `true` to overwrite the remote file, `false` to discard the edits.
        overwrite: bool,
    },
    /// Suspend the TUI and open a shell in a directory.
    OpenShell(Location),
    /// Reveal a file in Finder (or open its directory on Linux).
    Reveal(Location),

    // State, timers, control.
    /// Remember the last directory of an endpoint.
    RememberDirectory(Location),
    /// Remember the last pane sort of an endpoint.
    RememberSort {
        /// The endpoint.
        endpoint: Endpoint,
        /// Its sort.
        sort: Sort,
    },
    /// Add a bookmark.
    SaveBookmark(Location),
    /// Remove a bookmark.
    RemoveBookmark(Location),
    /// Abandon an in-flight request; its result, if any, is dropped.
    CancelRequest(RequestId),
    /// Deliver `BgEvent::TimerFired` after `after`. Scheduling an id again
    /// replaces the earlier timer.
    ScheduleTimer {
        /// Timer id.
        id: TimerId,
        /// Delay.
        after: Duration,
    },
    /// Exit the application.
    Quit,
}

/// A background result delivered to the app.
#[derive(Debug)]
pub enum BgEvent {
    // Sessions.
    /// An endpoint connected.
    Connected {
        /// The endpoint.
        endpoint: Endpoint,
        /// Its home directory.
        home: PathBuf,
    },
    /// Connecting failed for good (no automatic retry). Sent only for
    /// failures whose `ConnectError::should_auto_reconnect()` is false, and
    /// for sudo failures; retryable failures arrive as `Reconnecting`.
    ConnectFailed {
        /// The endpoint.
        endpoint: Endpoint,
        /// Why; `Error::Connect` or `Error::Sudo`.
        error: Error,
    },
    /// A background connect hit an auth or host key failure; the app should
    /// request [`Effect::InteractiveAuth`] when it is safe to suspend.
    InteractiveAuthRequired {
        /// The endpoint.
        endpoint: Endpoint,
        /// The BatchMode failure.
        error: ConnectError,
    },
    /// sudo needs a password; the app shows a masked input.
    SudoPasswordRequired(Endpoint),
    /// An established connection dropped.
    Disconnected {
        /// The endpoint.
        endpoint: Endpoint,
        /// Why.
        error: ConnectError,
    },
    /// The last attempt failed with a retryable error and the session will
    /// retry after `delay`. A first connect that finds the host unreachable
    /// is reported as `Reconnecting { attempt: 1, error: Unreachable { .. } }`
    /// so the VPN hint fires (`ConnectError::vpn_hint_eligible`).
    Reconnecting {
        /// The endpoint.
        endpoint: Endpoint,
        /// One-based number of the retry that follows `delay`.
        attempt: u32,
        /// Wait before this attempt.
        delay: Duration,
        /// Why the last attempt failed.
        error: ConnectError,
    },
    /// A dropped connection is back.
    Reconnected(Endpoint),
    /// `ssh -G` answered for an alias.
    HostResolved {
        /// The alias.
        alias: String,
        /// The resolution or its failure.
        result: Result<ResolvedHost, Error>,
    },
    /// The VPN command finished.
    VpnFinished {
        /// The endpoint to reconnect.
        endpoint: Endpoint,
        /// Whether the command ran successfully.
        result: Result<(), Error>,
    },

    // File system.
    /// A directory listing finished.
    Listed {
        /// Correlation id.
        request: RequestId,
        /// The directory.
        location: Location,
        /// The entries or the failure.
        result: Result<Vec<Entry>, FsError>,
    },
    /// A file operation finished.
    FsOpFinished {
        /// Correlation id.
        request: RequestId,
        /// Where it ran.
        endpoint: Endpoint,
        /// What was requested.
        op: FsOp,
        /// Per-path failures; empty means full success.
        failures: Vec<FsError>,
    },
    /// The Finder-drop existence check finished.
    DroppedPathsChecked {
        /// The pasted text as received.
        text: String,
        /// The tokens checked.
        paths: Vec<PathBuf>,
        /// Whether every token is an existing absolute local path.
        all_exist: bool,
    },

    // Transfers and sync.
    /// An engine event.
    Transfer(TransferEvent),
    /// A comparison finished.
    Compared {
        /// Correlation id.
        request: RequestId,
        /// The comparison or its failure.
        result: Result<Box<Comparison>, Error>,
    },
    /// An rsync-based sync finished, or an engine-based sync failed before
    /// its job was submitted (deleting extraneous destination paths).
    /// Engine-based syncs otherwise report through `Transfer` events of the
    /// same job id.
    SyncFinished {
        /// The sync's job id.
        job: JobId,
        /// Success or failure.
        result: Result<(), Error>,
    },

    // Preview, edit, external programs.
    /// A preview load finished.
    PreviewLoaded {
        /// Correlation id.
        request: RequestId,
        /// The file.
        location: Location,
        /// The content or its failure.
        result: Result<PreviewData, Error>,
    },
    /// A background image resize finished.
    ImageResized(ImageResized),
    /// An edit session ended.
    EditFinished {
        /// The edited file.
        location: Location,
        /// The outcome or its failure.
        result: Result<EditOutcome, Error>,
    },
    /// The `t` shell exited (its exit status is the user's business).
    ShellExited {
        /// Failure to start the shell, if any.
        result: Result<(), Error>,
    },
    /// A reveal finished.
    Revealed {
        /// The revealed file.
        location: Location,
        /// Success or failure.
        result: Result<(), Error>,
    },

    // State and timers.
    /// Persisting state or bookmarks failed.
    StateWriteFailed(Error),
    /// A scheduled timer fired.
    TimerFired {
        /// Timer id.
        id: TimerId,
        /// When it fired.
        at: Instant,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new(Zeroizing::new("hunter2".to_string()));
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        let effect = Effect::SudoPassword {
            endpoint: Endpoint::Remote {
                host: "h".into(),
                sudo: true,
            },
            password: secret.clone(),
        };
        assert!(!format!("{effect:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn pane_side_other_flips() {
        assert_eq!(PaneSide::Left.other(), PaneSide::Right);
        assert_eq!(PaneSide::Right.other(), PaneSide::Left);
    }
}
