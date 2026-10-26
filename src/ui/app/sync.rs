//! Directory comparison in the app: the comparison view, entry selection,
//! and starting a one-way sync.
//!
//! # Behavior
//!
//! - `=` (browse mode): when both panes are listed, emits `Effect::Compare`
//!   of their locations ([`SyncState::pending`] holds its request id) and
//!   shows `comparing...` in the status bar; a second `=` while pending
//!   cancels the first (`Effect::CancelRequest`) and compares again.
//! - `Compared` for the pending request opens [`SyncView`]: every entry
//!   except `DiffStatus::Same` files is visible (directories present on both
//!   sides stay visible so their contents read as a tree), none selected,
//!   cursor on the first row. A failure is an error status. Answers for
//!   other request ids are dropped.
//! - Comparison view (`InputMode::SyncView`): `MoveCursor`, `Scroll {
//!   region: ScrollRegion::SyncView }`, `Click` on `ClickTarget::SyncRow`
//!   move; `ToggleSelect` toggles the cursor entry and moves down;
//!   `Cancel` closes the view; `Help` is handed back (core passes it to the
//!   preview feature's help viewer).
//! - `StartSync(direction)` with at least one selected entry pushes a
//!   `DialogPurpose::ConfirmSync { direction }` dialog: "Copy N entries
//!   left -> right?" (or right -> left), Ok / Cancel, and when
//!   `sync::extraneous` is not empty for the selection, the checkbox
//!   "Delete M entries that exist only on the destination" (unchecked, so
//!   deleting is off by default). Its Ok answer
//!   (`App::sync_dialog_answer`) emits `Effect::Sync` with a new job id,
//!   the comparison, the selected relative paths, and `delete_extraneous`
//!   from the checkbox; pushes a `QueueJob { sync: true, mode: None, .. }`
//!   onto `TransferState::jobs`; records a [`RunningSync`]; and closes the
//!   view.
//! - `SyncFinished` sets the queue job's `outcome` (`Finished` with a
//!   default summary, or `Failed`), removes the [`RunningSync`], and
//!   refreshes the destination pane with `App::refresh`. A sync that runs as
//!   an engine job also finishes through `TransferEvent`s of the same job
//!   id, which the transfer feature handles; it removes the `RunningSync`
//!   here when its `QueueJob` has an outcome (checked by
//!   `App::sync_finished` and by `App::sync_jobs_settled`).
//!
//! # Public API (Wave 3, ui/app/sync)
//!
//! ```text
//! impl App {
//!     /// Claims `Compare` in browse mode and every action in
//!     /// `InputMode::SyncView` except `Help`.
//!     pub fn sync_action(&mut self, action: Action) -> Result<Vec<Effect>, Action>;
//!     /// `BgEvent::Compared`.
//!     pub fn sync_compared(&mut self, request: RequestId, result: Result<Box<Comparison>, Error>) -> Vec<Effect>;
//!     /// `BgEvent::SyncFinished`.
//!     pub fn sync_finished(&mut self, job: JobId, result: Result<(), Error>) -> Vec<Effect>;
//!     /// After any `BgEvent::Transfer`: forgets running syncs whose engine
//!     /// job finished.
//!     pub fn sync_jobs_settled(&mut self);
//!     /// The answer of `DialogPurpose::ConfirmSync`.
//!     pub fn sync_dialog_answer(&mut self, answer: DialogAnswer) -> Vec<Effect>;
//!     /// `Some(InputMode::SyncView)` while the view is open.
//!     pub fn sync_input_mode(&self) -> Option<InputMode>;
//!
//!     /// View accessor for render.
//!     pub fn sync_view(&self) -> Option<SyncListView<'_>>;
//! }
//! ```

use crate::sync::Comparison;
use crate::transfer::model::JobId;
use crate::ui::action::RequestId;
use crate::vfs::Endpoint;

/// State owned by this feature; part of [`super::App`].
#[derive(Debug, Default)]
pub struct SyncState {
    /// The comparison view while open.
    pub view: Option<SyncView>,
    /// The `Effect::Compare` in flight.
    pub pending: Option<RequestId>,
    /// Syncs started and not finished; their endpoints stay connected.
    pub running: Vec<RunningSync>,
}

/// The open comparison view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncView {
    /// The comparison shown.
    pub comparison: Box<Comparison>,
    /// Indices into `comparison.entries` shown, in order.
    pub visible: Vec<usize>,
    /// One flag per `comparison.entries` element.
    pub selected: Vec<bool>,
    /// Cursor position in `visible`.
    pub cursor: usize,
    /// First position of `visible` on screen.
    pub first_visible: usize,
}

/// A sync started with `Effect::Sync` that has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningSync {
    /// Its job id.
    pub job: JobId,
    /// The left directory's endpoint.
    pub left: Endpoint,
    /// The right directory's endpoint.
    pub right: Endpoint,
}
